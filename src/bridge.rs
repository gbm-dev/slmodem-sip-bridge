use crate::ari_control::AriController;
use crate::ari_external_media::ExternalMediaRequest;
use crate::config::Config;
use crate::session::{Session, SessionState};
use crate::slmodem::socket_from_raw_fd;
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, instrument};

#[instrument(skip_all, fields(dial = %cfg.dial_string, media = %cfg.media_addr, fd = cfg.socket_fd))]
pub async fn run(cfg: Config, media_request: ExternalMediaRequest) -> Result<()> {
    let mut session = Session::new(cfg.dial_string.clone());
    session.transition(SessionState::Originating)?;

    let sl_stream = socket_from_raw_fd(cfg.socket_fd)?;
    let ari = AriController::from_config(&cfg)?;

    // --- CRITICAL SYNC POINT ---
    // Register the Stasis app by opening the ARI events WebSocket.
    // Signal readiness to slmodemd once this is up.
    let ari_events = ari.connect_events(&media_request.app).await?;
    let _ari_drain = tokio::spawn(drain_ari_events(ari_events));

    // Signal to slmodemd that the audio path is ready.
    // We write a single newline to the Unix socket. slmodemd will be
    // blocking on a read until this arrives.
    sl_stream.writable().await?;
    sl_stream.try_write(b"\n")?;
    info!("event=sent_ready_to_slmodemd");

    // Give Asterisk a moment to register the Stasis app before we start
    // creating channels for it.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Read the dial string from the socket.
    // If started by ATZ, this will be empty. We will then wait for the real
    // dial string from the ATDT command.
    let mut dial_string;
    let mut line = String::new();
    let mut reader = tokio::io::BufReader::new(sl_stream);
    
    // Read first dial string (from startup)
    reader.read_line(&mut line).await?;
    dial_string = line.trim().to_string();
    
    while dial_string.is_empty() {
        info!("event=waiting_for_dial_string");
        line.clear();
        reader.read_line(&mut line).await?;
        dial_string = line.trim().to_string();
    }
    info!(dial = %dial_string, "event=received_dial_string");
    session = Session::new(dial_string.clone());
    let sl_stream = reader.into_inner();

    // Create the media channel and obtain its WebSocket URL first.
    let media_setup = ari.setup_media(&media_request).await?;
    session.transition(SessionState::ConnectingMedia)?;

    // Connect the media WebSocket (with auth) before dialing so the
    // media path is ready when audio starts flowing.
    let media_ws = ari
        .connect_media_websocket(&media_setup.media_websocket_url, cfg.connect_timeout)
        .await?;

    // Now dial the outbound call and bridge the channels.
    let call = match ari
        .dial_and_bridge(&dial_string, &media_request.app, &media_setup)
        .await
    {
        Ok(call) => call,
        Err(err) => {
            // Media setup resources are cleaned up inside dial_and_bridge on error.
            return Err(err);
        }
    };

    let result = async {
        session.transition(SessionState::MediaActive)?;
        info!(dial = %session.dial(), state = ?session.state(), "event=bridge_start");
        let started = tokio::time::Instant::now();

        let (to_media, to_sl) =
            relay_media(sl_stream, media_ws).await.with_context(|| {
                format!("failed to relay media via {}", call.media_websocket_url)
            })?;

        let elapsed = started.elapsed().as_millis();
        session.transition(SessionState::Terminating)?;
        info!(
            bytes_to_media = to_media,
            bytes_to_slmodem = to_sl,
            elapsed_ms = elapsed,
            state = ?session.state(),
            "event=bridge_stop"
        );
        session.transition(SessionState::Terminated)?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    ari.teardown(&call).await;
    result?;

    Ok(())
}

async fn relay_media(
    sl_stream: tokio::net::UnixStream,
    media_ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<(u64, u64)> {
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
        sl_writer
            .shutdown()
            .await
            .context("failed shutting down slmodemd socket write side")?;
        Result::<u64>::Ok(total)
    };

    tokio::try_join!(sl_to_ws, ws_to_sl).context("media relay failed")
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
