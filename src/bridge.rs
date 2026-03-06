use crate::ari_control::AriController;
use crate::ari_external_media::ExternalMediaRequest;
use crate::config::Config;
use crate::dial_string::DialString;
use crate::session::{Session, SessionState};
use crate::slmodem::socket_from_raw_fd;
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, instrument, warn};

/// Maximum bytes a single line from slmodemd can be before we discard it.
/// Dial strings are short (<128 chars); anything longer is garbled data.
const LINE_BUF_LIMIT: usize = 256;

/// Upper bound on consecutive calls in a single bridge lifetime. Prevents an
/// infinite loop if slmodemd keeps sending DIAL: commands without ever closing
/// the socket. 1000 calls is far beyond any realistic session.
const CALLS_LIMIT: u32 = 1000;

/// Maximum time to spend waiting for a DIAL: command before logging a
/// liveness heartbeat. Does not abort — just ensures we leave evidence in
/// logs that the bridge is alive and waiting. 60s is long enough to avoid
/// log spam, short enough to confirm the process isn't stuck.
const DIAL_WAIT_HEARTBEAT_SECS: u64 = 60;

/// 192 samples * 2 bytes (S16_LE) at 9600 Hz = one 20 ms frame of silence.
/// slmodemd's DSP expects a continuous clock of audio frames; starving it
/// causes internal buffer underruns that corrupt modem training sequences.
const SILENCE_FRAME_BYTES: usize = 384;

/// Interval between silence frames sent to slmodemd while idle. 20 ms matches
/// the codec frame duration so the DSP sees a steady sample clock.
const SILENCE_INTERVAL_MS: u64 = 20;

#[instrument(skip_all, fields(dial = %cfg.dial_string, media = %cfg.media_addr, fd = cfg.socket_fd))]
pub async fn run(cfg: Config, media_request: ExternalMediaRequest) -> Result<()> {
    assert!(cfg.socket_fd >= 0, "socket fd must be non-negative, got {}", cfg.socket_fd);
    assert!(!cfg.ari_base_url.is_empty(), "ARI base URL must not be empty");

    info!(
        fd = cfg.socket_fd,
        ari_base_url = %cfg.ari_base_url,
        "event=bridge_run_start"
    );

    let sl_stream = socket_from_raw_fd(cfg.socket_fd)?;
    let ari = AriController::from_config(&cfg)?;

    // --- CRITICAL SYNC POINT ---
    // Register the Stasis app by opening the ARI events WebSocket once.
    // Signal readiness to slmodemd only after this succeeds.
    info!("event=connecting_ari_events, reason=must register stasis app before signaling readiness");
    let ari_events = ari.connect_events(&media_request.app).await?;
    let ari_drain = tokio::spawn(drain_ari_events(ari_events));
    info!("event=ari_events_stream_established");

    // Signal to slmodemd that the audio path is ready.
    // 'R' = Ready. slmodemd blocks on socket_start() until it reads this byte.
    // Without this signal, slmodemd's modem emulation is frozen — AT commands
    // sent to /dev/ttySL0 will time out.
    let mut sl_stream = sl_stream;
    sl_stream.writable().await?;
    sl_stream.write_all(b"R").await?;
    info!("event=sent_ready_to_slmodemd, reason=unblocks slmodemd modem emulation");

    let silence = [0u8; SILENCE_FRAME_BYTES];
    let mut calls_count: u32 = 0;

    loop {
        assert!(
            calls_count < CALLS_LIMIT,
            "exceeded call limit of {CALLS_LIMIT} — aborting to prevent runaway loop"
        );

        info!(calls_count, "event=waiting_for_dial_string");

        let (mut read_half, mut write_half) = sl_stream.into_split();
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(SILENCE_INTERVAL_MS));
        let mut line_buf = Vec::with_capacity(LINE_BUF_LIMIT);

        // Feed silence to slmodemd while waiting for a DIAL: command.
        // We MUST drain read_half continuously to prevent slmodemd from
        // blocking on write and deadlocking.
        let dial_string = wait_for_dial_string(
            &mut read_half,
            &mut write_half,
            &mut interval,
            &mut line_buf,
            &silence,
        )
        .await;

        let dial_string = match dial_string {
            Some(ds) => ds,
            None => {
                // slmodemd closed the socket — clean shutdown.
                info!("event=slmodemd_socket_closed, reason=normal shutdown, aborting ARI drain");
                ari_drain.abort();
                return Ok(());
            }
        };

        calls_count += 1;
        // slmodemd sends the raw ATDT string including the T/P prefix
        // (e.g. "T17186945647"). Strip it so the SIP endpoint gets a
        // clean phone number, not "PJSIP/T17186945647@telnyx-out".
        let dial_string = match DialString::parse(&dial_string) {
            Ok(parsed) => parsed.as_str().to_string(),
            Err(err) => {
                warn!(error = %err, raw = %dial_string, "event=invalid_dial_string");
                sl_stream = read_half
                    .reunite(write_half)
                    .map_err(|_| anyhow::anyhow!("failed to reunite socket halves"))?;
                continue;
            }
        };
        info!(dial = %dial_string, calls_count, "event=received_dial_string");
        let mut session = Session::new(dial_string.clone());
        session.transition(SessionState::Originating)?;

        // Create the media channel and obtain its WebSocket URL.
        let media_setup = match ari.setup_media(&media_request).await {
            Ok(setup) => {
                info!(
                    bridge_id = %setup.bridge_id,
                    media_channel_id = %setup.media_channel_id,
                    "event=media_setup_complete"
                );
                setup
            }
            Err(err) => {
                warn!(error = %err, dial = %dial_string, "event=media_setup_failed, returning to wait state");
                sl_stream = read_half
                    .reunite(write_half)
                    .map_err(|_| anyhow::anyhow!("failed to reunite socket halves"))?;
                continue;
            }
        };
        session.transition(SessionState::ConnectingMedia)?;

        // Connect the media WebSocket (with auth).
        let media_ws = match ari
            .connect_media_websocket(&media_setup.media_websocket_url, cfg.connect_timeout)
            .await
        {
            Ok(ws) => {
                info!(
                    url = %media_setup.media_websocket_url,
                    "event=media_websocket_connected"
                );
                ws
            }
            Err(err) => {
                warn!(error = %err, "event=media_websocket_connect_failed");
                ari.cleanup_media(&media_setup).await;
                sl_stream = read_half
                    .reunite(write_half)
                    .map_err(|_| anyhow::anyhow!("failed to reunite socket halves"))?;
                continue;
            }
        };

        // Dial the outbound call and bridge the channels.
        let call = match ari
            .dial_and_bridge(&dial_string, &media_request.app, &media_setup)
            .await
        {
            Ok(call) => {
                info!(
                    outbound_channel_id = %call.outbound_channel_id,
                    bridge_id = %call.bridge_id,
                    "event=dial_and_bridge_complete"
                );
                call
            }
            Err(err) => {
                warn!(error = %err, dial = %dial_string, "event=dial_failed, returning to wait state");
                ari.cleanup_media(&media_setup).await;
                sl_stream = read_half
                    .reunite(write_half)
                    .map_err(|_| anyhow::anyhow!("failed to reunite socket halves"))?;
                continue;
            }
        };

        // Relay media between slmodemd and Asterisk.
        session.transition(SessionState::MediaActive)?;
        info!(dial = %session.dial(), state = ?session.state(), "event=bridge_start");
        let started = tokio::time::Instant::now();

        let sl_stream_reunited = read_half
            .reunite(write_half)
            .map_err(|_| anyhow::anyhow!("failed to reunite socket halves for relay"))?;

        match relay_media(sl_stream_reunited, media_ws).await {
            Ok((sl_reunited, bytes_to_ws, bytes_to_sl)) => {
                let elapsed_ms = started.elapsed().as_millis();
                info!(
                    dial = %dial_string,
                    duration_ms = elapsed_ms,
                    tx_bytes = bytes_to_ws,
                    rx_bytes = bytes_to_sl,
                    "event=bridge_stop",
                );
                sl_stream = sl_reunited;
            }
            Err(e) => {
                error!(error = %e, dial = %dial_string, "event=media_relay_error");
                ari.teardown(&call).await;
                return Err(e);
            }
        }

        // Clean up Asterisk resources before next call.
        info!(dial = %dial_string, "event=teardown_start");
        ari.teardown(&call).await;
        info!(dial = %dial_string, "event=teardown_complete");
        session.transition(SessionState::Terminating)?;
    }
}

/// Read from slmodemd until we receive a "DIAL:<number>\n" line.
/// Returns `None` if slmodemd closes the socket.
async fn wait_for_dial_string(
    read_half: &mut tokio::net::unix::OwnedReadHalf,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    interval: &mut tokio::time::Interval,
    line_buf: &mut Vec<u8>,
    silence: &[u8],
) -> Option<String> {
    assert!(
        silence.len() == SILENCE_FRAME_BYTES,
        "silence buffer must be {SILENCE_FRAME_BYTES} bytes, got {}",
        silence.len()
    );

    let mut buf = [0u8; 1024];
    let mut heartbeat = tokio::time::interval(
        std::time::Duration::from_secs(DIAL_WAIT_HEARTBEAT_SECS),
    );
    // Skip the first immediate tick so the first heartbeat fires after the full interval.
    heartbeat.tick().await;

    loop {
        tokio::select! {
            res = read_half.read(&mut buf) => {
                let n = match res {
                    Ok(0) => {
                        info!("event=slmodemd_socket_eof, reason=slmodemd closed the control socket");
                        return None;
                    }
                    Ok(n) => n,
                    Err(e) => {
                        warn!(error = %e, "event=slmodemd_read_error");
                        return None;
                    }
                };

                for &b in &buf[..n] {
                    if b == b'\n' {
                        let line = String::from_utf8_lossy(line_buf);
                        if let Some(ds) = line.strip_prefix("DIAL:") {
                            let ds = ds.trim().to_string();
                            if !ds.is_empty() {
                                return Some(ds);
                            }
                            debug!("event=empty_dial_string, reason=DIAL: prefix present but number is empty");
                        } else if !line.is_empty() {
                            debug!(line = %line, "event=unknown_line_from_slmodemd");
                        }
                        line_buf.clear();
                    } else {
                        line_buf.push(b);
                        if line_buf.len() > LINE_BUF_LIMIT {
                            warn!(
                                len = line_buf.len(),
                                limit = LINE_BUF_LIMIT,
                                "event=line_buffer_overflow, reason=discarding garbled data from slmodemd"
                            );
                            line_buf.clear();
                        }
                    }
                }
            }
            _ = interval.tick() => {
                // Keep slmodemd DSP alive by providing a steady sample clock.
                if let Err(e) = write_half.write_all(silence).await {
                    warn!(error = %e, "event=silence_write_failed, reason=socket closing");
                    return None;
                }
            }
            _ = heartbeat.tick() => {
                // Liveness heartbeat: confirms the bridge is alive and waiting.
                // Without this, a long idle period produces zero log output,
                // making it impossible to distinguish "waiting" from "stuck".
                info!("event=dial_wait_heartbeat, reason=still waiting for DIAL command from slmodemd");
            }
        }
    }
}

/// Bidirectional media relay between slmodemd unix socket and Asterisk media WebSocket.
/// Returns the reunited unix stream and byte counts for each direction.
async fn relay_media(
    sl_stream: tokio::net::UnixStream,
    media_ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<(tokio::net::UnixStream, u64, u64)> {
    let (mut ws_writer, mut ws_reader) = media_ws.split();
    let (mut sl_reader, mut sl_writer) = sl_stream.into_split();

    info!("event=relay_media_start");

    let sl_to_ws = async {
        let mut total: u64 = 0;
        let mut buf = [0u8; 2048];
        loop {
            let n = sl_reader
                .read(&mut buf)
                .await
                .context("failed reading from slmodemd socket")?;
            if n == 0 {
                debug!("event=sl_to_ws_eof, reason=slmodemd closed audio stream");
                let _ = ws_writer.send(Message::Close(None)).await;
                break;
            }
            total += n as u64;
            ws_writer
                .send(Message::Binary(buf[..n].to_vec().into()))
                .await
                .context("failed writing modem audio to media websocket")?;
        }
        Result::<u64>::Ok(total)
    };

    let ws_to_sl = async {
        let mut total: u64 = 0;
        while let Some(frame) = ws_reader.next().await {
            let frame = frame.context("failed reading from media websocket")?;
            match frame {
                Message::Binary(data) => {
                    total += data.len() as u64;
                    sl_writer
                        .write_all(&data)
                        .await
                        .context("failed writing media audio to slmodemd socket")?;
                }
                Message::Close(reason) => {
                    debug!(?reason, "event=ws_to_sl_close, reason=asterisk closed media websocket");
                    break;
                }
                Message::Text(text) => {
                    // Asterisk should not send text frames on the media WS.
                    // Log it so we can diagnose unexpected protocol behavior.
                    warn!(text = %text, "event=unexpected_text_frame_on_media_ws");
                }
                Message::Ping(_) | Message::Pong(_) => {}
                _ => {
                    debug!("event=unknown_ws_frame_type");
                }
            }
        }
        Result::<u64>::Ok(total)
    };

    let (bytes_to_ws, bytes_to_sl) =
        tokio::try_join!(sl_to_ws, ws_to_sl).context("media relay failed")?;

    info!(
        tx_bytes = bytes_to_ws,
        rx_bytes = bytes_to_sl,
        "event=relay_media_stop"
    );

    let sl_reunited = sl_reader
        .reunite(sl_writer)
        .map_err(|_| anyhow::anyhow!("failed to reunite sl_stream parts after relay"))?;

    Ok((sl_reunited, bytes_to_ws, bytes_to_sl))
}

/// Drain the ARI events WebSocket to keep the Stasis app registered.
/// This stream MUST stay open for the bridge's lifetime — dropping it
/// unregisters the app and Asterisk will reject subsequent ARI calls.
async fn drain_ari_events(
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    info!("event=ari_event_drain_start");
    let (_writer, mut reader) = stream.split();

    /// Upper bound on ARI events to drain before logging a summary. Prevents
    /// unbounded silent consumption. 100k events at ~1/sec = ~28 hours; well
    /// beyond any single bridge session.
    const EVENTS_DRAIN_LOG_INTERVAL: u64 = 1000;
    const EVENTS_DRAIN_LIMIT: u64 = 100_000;

    let mut events_count: u64 = 0;

    while let Some(msg) = reader.next().await {
        events_count += 1;

        if events_count % EVENTS_DRAIN_LOG_INTERVAL == 0 {
            debug!(events_count, "event=ari_drain_heartbeat");
        }

        if events_count >= EVENTS_DRAIN_LIMIT {
            warn!(
                events_count,
                limit = EVENTS_DRAIN_LIMIT,
                "event=ari_drain_limit_reached, reason=exceeded event drain limit, stopping"
            );
            break;
        }

        match msg {
            Ok(Message::Close(reason)) => {
                info!(?reason, "event=ari_events_closed, reason=asterisk closed the events websocket");
                break;
            }
            Ok(Message::Text(text)) => {
                // ARI events are JSON text frames. Log at debug level so
                // operators can enable verbose tracing without code changes.
                debug!(event_text = %text, "event=ari_event_received");
            }
            Err(err) => {
                error!(error = %err, "event=ari_events_error, reason=events websocket read failed");
                break;
            }
            _ => {}
        }
    }

    info!(events_count, "event=ari_event_drain_stop");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_frame_size_matches_codec_frame() {
        // 192 samples * 2 bytes/sample (S16_LE) = 384 bytes per 20ms frame at 9600 Hz.
        assert_eq!(SILENCE_FRAME_BYTES, 384);
    }

    #[test]
    fn calls_limit_is_reasonable() {
        // Must be large enough for any realistic session but bounded.
        assert!(CALLS_LIMIT >= 100, "calls limit too low for realistic use");
        assert!(CALLS_LIMIT <= 10_000, "calls limit unreasonably high");
    }
}
