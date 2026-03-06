use crate::codec;
use crate::config::Config;
use crate::dial_string::DialString;
use crate::rtp;
use crate::sdp;
use crate::session::{Session, SessionState};
use crate::sip::{CallState, SipClient};
use crate::slmodem::socket_from_raw_fd;
use anyhow::{Context, Result};
use std::net::SocketAddr;
use tracing::{debug, error, info, warn};

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

/// 160 samples * 2 bytes (S16_LE) at 8000 Hz = one 20 ms frame of silence.
/// slmodemd's DSP expects a continuous clock of audio frames; starving it
/// causes internal buffer underruns that corrupt modem training sequences.
const SILENCE_FRAME_BYTES: usize = 320;

/// Interval between silence frames sent to slmodemd while idle. 20 ms matches
/// the codec frame duration so the DSP sees a steady sample clock.
const SILENCE_INTERVAL_MS: u64 = 20;

/// How often to log relay throughput stats. 2 seconds is frequent enough
/// to diagnose audio flow problems without flooding logs.
const RELAY_STATS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Safety limit on drain iterations during SIP setup. At 20 ms silence
/// intervals, 500 iterations = 10 seconds — well beyond any SIP setup
/// sequence. If we're still draining after this, something is stuck.
const DRAIN_ITERATIONS_LIMIT: u32 = 500;

/// SSRC for our RTP stream. Fixed value is fine — we're the only sender
/// on this RTP session, no SSRC collision possible.
const RTP_SSRC: u32 = 0x534C4D44; // "SLMD" in ASCII

#[allow(clippy::too_many_lines)]
pub async fn run(cfg: Config) -> Result<()> {
    assert!(
        cfg.socket_fd >= 0,
        "socket fd must be non-negative, got {}",
        cfg.socket_fd
    );
    assert!(
        !cfg.telnyx_sip_user.is_empty(),
        "TELNYX_SIP_USER must not be empty"
    );

    info!(
        fd = cfg.socket_fd,
        sip_domain = %cfg.telnyx_sip_domain,
        "event=bridge_run_start"
    );

    let sl_stream = socket_from_raw_fd(cfg.socket_fd)?;

    // Resolve SIP server address
    let sip_server: SocketAddr = format!("{}:5060", cfg.telnyx_sip_domain)
        .parse()
        .or_else(|_| {
            // Domain name — resolve via DNS
            use std::net::ToSocketAddrs;
            format!("{}:5060", cfg.telnyx_sip_domain)
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "DNS resolution failed"))
        })
        .with_context(|| format!("failed to resolve SIP server: {}", cfg.telnyx_sip_domain))?;

    info!(sip_server = %sip_server, "event=sip_server_resolved");

    // Create SIP client and register with the trunk
    let sip_bind_addr: SocketAddr = format!("{}:0", cfg.sip_local_addr)
        .parse()
        .context("invalid SIP_LOCAL_ADDR")?;

    let sip = SipClient::new(
        sip_bind_addr,
        sip_server,
        cfg.telnyx_sip_domain.clone(),
        cfg.telnyx_sip_user.clone(),
        cfg.telnyx_sip_pass.clone(),
        cfg.caller_id.clone(),
        cfg.caller_name.clone(),
    )
    .await?;

    // REGISTER with Telnyx (handles 401 digest challenge)
    info!("event=registering_sip");
    sip.register().await?;
    info!("event=sip_registered");

    // Signal to slmodemd that the audio path is ready.
    // 'R' = Ready. slmodemd blocks on socket_start() until it reads this byte.
    let mut sl_stream = sl_stream;
    use tokio::io::AsyncWriteExt;
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
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(SILENCE_INTERVAL_MS));
        let mut line_buf = Vec::with_capacity(LINE_BUF_LIMIT);

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
                info!("event=slmodemd_socket_closed, reason=normal shutdown");
                return Ok(());
            }
        };

        calls_count += 1;
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

        // Bind RTP socket
        let rtp_bind: SocketAddr = format!("{}:{}", cfg.sip_local_addr, cfg.sip_local_rtp_port)
            .parse()
            .context("invalid RTP bind address")?;
        let rtp_socket = tokio::net::UdpSocket::bind(rtp_bind)
            .await
            .with_context(|| format!("failed to bind RTP socket on {rtp_bind}"))?;
        let rtp_local = rtp_socket.local_addr()?;
        info!(rtp_local = %rtp_local, "event=rtp_socket_bound");

        // Build SDP offer with our RTP address
        let session_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let sdp_offer = sdp::build_offer(sip.local_ip(), rtp_local.port(), session_id);

        // SIP INVITE with concurrent slmodemd drain
        let setup_result = {
            let invite_fut = sip.invite(&dial_string, &sdp_offer, cfg.invite_timeout);
            drain_during_sip_setup(&mut read_half, &mut write_half, &silence, invite_fut).await
        };

        let mut call = match setup_result {
            Ok(call) => {
                session.transition(SessionState::ConnectingMedia)?;
                call
            }
            Err(err) => {
                warn!(error = %err, dial = %dial_string, "event=sip_setup_failed, returning to wait state");
                sl_stream = read_half
                    .reunite(write_half)
                    .map_err(|_| anyhow::anyhow!("failed to reunite socket halves"))?;
                continue;
            }
        };

        let remote_rtp = match call.remote_rtp {
            Some(addr) => addr,
            None => {
                warn!(dial = %dial_string, "event=no_remote_rtp, reason=no SDP in INVITE response");
                sip.bye(&call).await.ok();
                sl_stream = read_half
                    .reunite(write_half)
                    .map_err(|_| anyhow::anyhow!("failed to reunite socket halves"))?;
                continue;
            }
        };

        // Send ACK for 200 OK (immediate answer) or poll for it during
        // the media relay (early media / ringing).
        if call.state == CallState::Answered {
            sip.ack(&call).await?;
        }

        // Full bidirectional relay with slin↔ulaw codec conversion over RTP.
        session.transition(SessionState::MediaActive)?;
        info!(
            dial = %session.dial(),
            state = ?session.state(),
            remote_rtp = %remote_rtp,
            "event=bridge_start"
        );
        let started = tokio::time::Instant::now();

        let sl_stream_reunited = read_half
            .reunite(write_half)
            .map_err(|_| anyhow::anyhow!("failed to reunite socket halves for relay"))?;

        let (rtp_sender, rtp_receiver) = rtp::create_rtp_pair(rtp_socket, RTP_SSRC);

        // Run media relay, concurrently polling for 200 OK if in early media.
        // The block ensures the answer_poll future (which borrows &mut call)
        // is dropped before we use call for teardown.
        let relay_result = {
            let answer_poll_fut = async {
                if call.state == CallState::EarlyMedia {
                    let mut poll_interval =
                        tokio::time::interval(std::time::Duration::from_millis(50));
                    loop {
                        poll_interval.tick().await;
                        match sip.poll_answer(&mut call).await {
                            Ok(true) => break,
                            Ok(false) => {}
                            Err(e) => {
                                warn!(error = %e, "event=answer_poll_error");
                                break;
                            }
                        }
                    }
                }
            };

            let relay_fut =
                relay_media(sl_stream_reunited, rtp_sender, rtp_receiver, remote_rtp);

            tokio::pin!(relay_fut);
            tokio::pin!(answer_poll_fut);
            let mut answer_done = false;
            loop {
                if answer_done {
                    break relay_fut.await;
                }
                tokio::select! {
                    result = &mut relay_fut => break result,
                    _ = &mut answer_poll_fut => {
                        answer_done = true;
                    }
                }
            }
        };

        match relay_result {
            Ok((sl_reunited, bytes_to_rtp, bytes_to_sl)) => {
                let elapsed_ms = started.elapsed().as_millis();
                info!(
                    dial = %dial_string,
                    duration_ms = elapsed_ms,
                    tx_bytes = bytes_to_rtp,
                    rx_bytes = bytes_to_sl,
                    "event=bridge_stop",
                );
                sl_stream = sl_reunited;
            }
            Err(e) => {
                error!(error = %e, dial = %dial_string, "event=media_relay_error");
                sip.bye(&call).await.ok();
                return Err(e);
            }
        }

        info!(dial = %dial_string, "event=teardown_start");
        sip.bye(&call).await.ok();
        info!(dial = %dial_string, "event=teardown_complete");
        session.transition(SessionState::Terminating)?;
        session.transition(SessionState::Terminated)?;
    }
}

/// Drain slmodemd audio and feed silence while an async SIP setup runs.
///
/// slmodemd starts its DSP immediately after sending DIAL:. It writes modem
/// tones to the socket and expects continuous received audio frames (20 ms
/// cadence). This function keeps the DSP alive during SIP signaling by:
/// 1. Reading and discarding audio from slmodemd (prevents kernel buffer
///    overflow and stale-audio burst when relay starts)
/// 2. Writing silence frames every 20 ms (prevents DSP underrun and timing
///    drift that would corrupt modem training)
async fn drain_during_sip_setup<F, T>(
    read_half: &mut tokio::net::unix::OwnedReadHalf,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    silence: &[u8],
    setup_fut: F,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    assert!(
        silence.len() == SILENCE_FRAME_BYTES,
        "silence must be {SILENCE_FRAME_BYTES} bytes"
    );

    tokio::pin!(setup_fut);

    let mut drain_buf = [0u8; 2048];
    let mut silence_interval =
        tokio::time::interval(std::time::Duration::from_millis(SILENCE_INTERVAL_MS));
    let mut drained_bytes: u64 = 0;
    let mut iterations: u32 = 0;

    loop {
        assert!(
            iterations < DRAIN_ITERATIONS_LIMIT,
            "drain loop exceeded {DRAIN_ITERATIONS_LIMIT} iterations — SIP setup stuck"
        );
        iterations += 1;

        tokio::select! {
            result = &mut setup_fut => {
                if drained_bytes > 0 {
                    info!(
                        drained_bytes,
                        iterations,
                        "event=drain_during_setup_complete"
                    );
                }
                return result;
            }
            res = read_half.read(&mut drain_buf) => {
                match res {
                    Ok(0) => {
                        return Err(anyhow::anyhow!(
                            "slmodemd closed socket during SIP setup"
                        ));
                    }
                    Ok(n) => {
                        drained_bytes += n as u64;
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!(
                            "slmodemd read error during SIP setup: {e}"
                        ));
                    }
                }
            }
            _ = silence_interval.tick() => {
                if let Err(e) = write_half.write_all(silence).await {
                    return Err(anyhow::anyhow!(
                        "failed to write silence during SIP setup: {e}"
                    ));
                }
            }
        }
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    assert!(
        silence.len() == SILENCE_FRAME_BYTES,
        "silence buffer must be {SILENCE_FRAME_BYTES} bytes, got {}",
        silence.len()
    );

    let mut buf = [0u8; 1024];
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(
        DIAL_WAIT_HEARTBEAT_SECS,
    ));
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
                if let Err(e) = write_half.write_all(silence).await {
                    warn!(error = %e, "event=silence_write_failed, reason=socket closing");
                    return None;
                }
            }
            _ = heartbeat.tick() => {
                info!("event=dial_wait_heartbeat, reason=still waiting for DIAL command from slmodemd");
            }
        }
    }
}

/// Bidirectional media relay between slmodemd unix socket and remote RTP
/// endpoint, with slin↔ulaw codec conversion.
///
/// slmodemd speaks signed 16-bit linear PCM (S16_LE, 8 kHz). The SIP/RTP
/// endpoint uses ulaw (G.711 µ-law, payload type 0). Converting here gives
/// the cleanest possible audio path — no transcoding middleman, direct UDP
/// delivery with minimal latency.
///
/// Returns the reunited unix stream and byte counts for each direction.
async fn relay_media(
    sl_stream: tokio::net::UnixStream,
    mut rtp_sender: rtp::RtpSender,
    rtp_receiver: rtp::RtpReceiver,
    remote_rtp: SocketAddr,
) -> Result<(tokio::net::UnixStream, u64, u64)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut sl_reader, mut sl_writer) = sl_stream.into_split();

    info!(remote_rtp = %remote_rtp, "event=relay_media_start");

    // sl→rtp: read slin from slmodemd, convert to ulaw, send as RTP.
    let sl_to_rtp = async {
        let mut total: u64 = 0;
        let mut interval_bytes: u64 = 0;
        let mut frames: u64 = 0;
        let mut last_report = tokio::time::Instant::now();
        let mut buf = [0u8; 2048];
        let mut residual: Option<u8> = None;

        loop {
            let n = sl_reader
                .read(&mut buf)
                .await
                .context("failed reading from slmodemd socket")?;
            if n == 0 {
                debug!("event=sl_to_rtp_eof, reason=slmodemd closed audio stream");
                break;
            }

            let slin = codec::align_slin_buffer(&mut residual, &buf[..n]);
            if slin.is_empty() {
                continue;
            }

            let ulaw = codec::encode_ulaw(&slin);
            total += ulaw.len() as u64;
            interval_bytes += ulaw.len() as u64;
            frames += 1;

            rtp_sender.send_packet(&ulaw, remote_rtp)
                .await
                .context("failed sending RTP packet")?;

            if last_report.elapsed() >= RELAY_STATS_INTERVAL {
                let elapsed_ms = last_report.elapsed().as_millis();
                info!(
                    direction = "sl→rtp",
                    interval_bytes,
                    interval_frames = frames,
                    interval_ms = elapsed_ms,
                    total_bytes = total,
                    "event=relay_throughput"
                );
                interval_bytes = 0;
                frames = 0;
                last_report = tokio::time::Instant::now();
            }
        }
        Result::<u64>::Ok(total)
    };

    // rtp→sl: receive RTP from remote, convert ulaw to slin, write to slmodemd.
    let rtp_to_sl = async {
        let mut total: u64 = 0;
        let mut interval_bytes: u64 = 0;
        let mut frames: u64 = 0;
        let mut min_frame: usize = usize::MAX;
        let mut max_frame: usize = 0;
        let mut last_report = tokio::time::Instant::now();

        loop {
            match rtp_receiver.recv_packet().await {
                Ok(Some((payload, _addr))) => {
                    let len = payload.len();
                    total += len as u64;
                    interval_bytes += len as u64;
                    frames += 1;
                    min_frame = min_frame.min(len);
                    max_frame = max_frame.max(len);

                    let slin = codec::decode_ulaw(&payload);
                    sl_writer
                        .write_all(&slin)
                        .await
                        .context("failed writing RTP audio to slmodemd socket")?;

                    if last_report.elapsed() >= RELAY_STATS_INTERVAL {
                        let elapsed_ms = last_report.elapsed().as_millis();
                        info!(
                            direction = "rtp→sl",
                            interval_bytes,
                            interval_frames = frames,
                            interval_ms = elapsed_ms,
                            total_bytes = total,
                            min_frame_bytes = min_frame,
                            max_frame_bytes = max_frame,
                            "event=relay_throughput"
                        );
                        interval_bytes = 0;
                        frames = 0;
                        min_frame = usize::MAX;
                        max_frame = 0;
                        last_report = tokio::time::Instant::now();
                    }
                }
                Ok(None) => {
                    // Invalid RTP packet (wrong version, too short) — skip
                    debug!("event=rtp_invalid_packet_skipped");
                }
                Err(e) => {
                    // UDP recv error — likely socket closed
                    debug!(error = %e, "event=rtp_recv_error");
                    break;
                }
            }
        }
        Result::<u64>::Ok(total)
    };

    let (bytes_to_rtp, bytes_to_sl) =
        tokio::try_join!(sl_to_rtp, rtp_to_sl).context("media relay failed")?;

    info!(
        tx_bytes = bytes_to_rtp,
        rx_bytes = bytes_to_sl,
        "event=relay_media_stop"
    );

    let sl_reunited = sl_reader
        .reunite(sl_writer)
        .map_err(|_| anyhow::anyhow!("failed to reunite sl_stream parts after relay"))?;

    Ok((sl_reunited, bytes_to_rtp, bytes_to_sl))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_frame_size_matches_codec_frame() {
        // 160 samples * 2 bytes/sample (S16_LE) = 320 bytes per 20ms frame at 8000 Hz.
        assert_eq!(SILENCE_FRAME_BYTES, 320);
    }

    #[test]
    fn calls_limit_is_reasonable() {
        assert!(CALLS_LIMIT >= 100, "calls limit too low for realistic use");
        assert!(CALLS_LIMIT <= 10_000, "calls limit unreasonably high");
    }

    #[test]
    fn drain_iterations_limit_covers_setup_window() {
        // At 20ms per iteration, the limit should cover at least 5 seconds
        // of SIP setup time.
        let coverage_ms = DRAIN_ITERATIONS_LIMIT as u64 * SILENCE_INTERVAL_MS;
        assert!(
            coverage_ms >= 5000,
            "drain limit only covers {coverage_ms}ms, need at least 5000ms"
        );
    }
}
