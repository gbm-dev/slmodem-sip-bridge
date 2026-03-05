use crate::ari_control::AriController;
use crate::ari_external_media::ExternalMediaRequest;
use crate::config::Config;
use crate::session::{Session, SessionState};
use crate::slmodem::socket_from_raw_fd;
use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, instrument, warn, debug};

#[instrument(skip_all, fields(dial = %cfg.dial_string, media = %cfg.media_addr, fd = cfg.socket_fd))]
pub async fn run(cfg: Config, media_request: ExternalMediaRequest) -> Result<()> {
    let sl_stream = socket_from_raw_fd(cfg.socket_fd)?;
    let ari = AriController::from_config(&cfg)?;

    // --- CRITICAL SYNC POINT ---
    // Register the Stasis app by opening the ARI events WebSocket once.
    // Signal readiness to slmodemd once this is up.
    let ari_events = ari.connect_events(&media_request.app).await?;
    let _ari_drain = tokio::spawn(drain_ari_events(ari_events));

    // Signal to slmodemd that the audio path is ready.
    // We use a mutable local for the stream to allow looping.
    let mut sl_stream = sl_stream;
    sl_stream.writable().await?;
    sl_stream.write_all(b"\n").await?;
    info!("event=sent_ready_to_slmodemd");

    // Give Asterisk a moment to register the Stasis app.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let silence = [0u8; 384]; // 192 samples * 2 bytes (S16_LE) for 9600Hz

    loop {
        info!("event=waiting_for_dial_string");
        
        let (read_half, mut write_half) = sl_stream.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(20));
        let mut dial_string = String::new();

        // Loop generating silence until we get a dial string from slmodemd
        loop {
            let mut line = String::new();
            tokio::select! {
                res = reader.read_line(&mut line) => {
                    match res {
                        Ok(0) => {
                            warn!("slmodemd closed the control socket");
                            return Ok(());
                        }
                        Ok(_) => {
                            let s = line.trim().to_string();
                            if !s.is_empty() {
                                dial_string = s;
                                break;
                            }
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                _ = interval.tick() => {
                    // Keep slmodemd DSP alive by providing a clock
                    if let Err(e) = write_half.write_all(&silence).await {
                        warn!(error = %e, "failed to write silence, socket might be closing");
                        return Ok(());
                    }
                }
            }
        }

        info!(dial = %dial_string, "event=received_dial_string");
        let mut session = Session::new(dial_string.clone());
        session.transition(SessionState::Originating)?;

        // Create the media channel and obtain its WebSocket URL.
        let media_setup = ari.setup_media(&media_request).await?;
        session.transition(SessionState::ConnectingMedia)?;

        // Connect the media WebSocket (with auth).
        let media_ws = ari
            .connect_media_websocket(&media_setup.media_websocket_url, cfg.connect_timeout)
            .await?;

        // Now dial the outbound call and bridge the channels.
        let _call = match ari
            .dial_and_bridge(&dial_string, &media_request.app, &media_setup)
            .await
        {
            Ok(call) => call,
            Err(err) => {
                warn!(error = %err, "dial failed, returning to wait state");
                // Reunite socket for next loop
                sl_stream = reader.into_inner().reunite(write_half)
                    .map_err(|_| anyhow::anyhow!("failed to reunite socket halves after dial failure"))?;
                continue;
            }
        };

        // Relay media between slmodemd and Asterisk.
        session.transition(SessionState::MediaActive)?;
        info!(dial = %session.dial(), state = ?session.state(), "event=bridge_start");
        let started = tokio::time::Instant::now();

        // Reconstruct sl_stream from halves for relay_media
        let sl_stream_reunited = reader.into_inner().reunite(write_half)
            .map_err(|_| anyhow::anyhow!("failed to reunite socket halves for relay"))?;

        match relay_media(sl_stream_reunited, media_ws).await {
            Ok((sl_reunited, bytes_to_ws, bytes_to_sl)) => {
                let elapsed = started.elapsed().as_millis();
                info!(
                    dial = %dial_string,
                    duration_ms = elapsed,
                    tx_bytes = bytes_to_ws,
                    rx_bytes = bytes_to_sl,
                    "event=bridge_stop",
                );
                sl_stream = sl_reunited;
            }
            Err(e) => {
                warn!(error = %e, "media relay ended with error");
                // If it failed, we can't easily recover the stream if it was consumed.
                // But relay_media returns it now.
                return Err(e);
            }
        }
        
        session.transition(SessionState::Terminating)?;
    }
}

async fn relay_media(
    sl_stream: tokio::net::UnixStream,
    media_ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<(tokio::net::UnixStream, u64, u64)> {
    let (mut ws_writer, mut ws_reader) = media_ws.split();
    let (mut sl_reader, mut sl_writer) = tokio::io::split(sl_stream);

    let sl_to_ws = async {
        let mut total = 0u64;
        let mut buf = [0u8; 2048];
        loop {
            let n = sl_reader
                .read(&mut buf)
                .await
                .context("failed reading from slmodemd socket")?;
            if n == 0 {
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
        let mut total = 0u64;
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
                Message::Close(_) => break,
                Message::Text(_) | Message::Ping(_) | Message::Pong(_) => {}
                _ => {}
            }
        }
        // Do NOT shutdown the socket if we want to reuse it for the next call!
        // Instead, just return.
        Result::<u64>::Ok(total)
    };

    let (bytes_to_ws, bytes_to_sl) = tokio::try_join!(sl_to_ws, ws_to_sl).context("media relay failed")?;
    
    // Recover the stream parts
    let sl_reader = sl_reader.reunite(sl_writer)
        .map_err(|_| anyhow::anyhow!("failed to reunite sl_stream parts after relay"))?;
    
    Ok((sl_reader, bytes_to_ws, bytes_to_sl))
}

async fn drain_ari_events(
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let (_writer, mut reader) = stream.split();
    while let Some(msg) = reader.next().await {
        match msg {
            Ok(Message::Close(_)) => break,
            Err(_) => break,
            _ => {}
        }
    }
}
