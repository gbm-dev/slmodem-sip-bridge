use crate::ari_control::AriController;
use crate::ari_external_media::ExternalMediaRequest;
use crate::config::Config;
use crate::session::{Session, SessionState};
use crate::slmodem::socket_from_raw_fd;
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, instrument};

#[instrument(skip_all, fields(dial = %cfg.dial_string, media = %cfg.media_addr, fd = cfg.socket_fd))]
pub async fn run(cfg: Config, media_request: ExternalMediaRequest) -> Result<()> {
    let mut session = Session::new(cfg.dial_string.clone());
    session.transition(SessionState::Originating)?;

    let sl_stream = socket_from_raw_fd(cfg.socket_fd)?;
    let ari = AriController::from_config(&cfg)?;

    // Register the Stasis app by opening the ARI events WebSocket.
    // This must stay open for the lifetime of the call.
    let _ari_events = ari.connect_events(&media_request.app).await?;

    let call = ari.start_call(&cfg.dial_string, &media_request).await?;
    session.transition(SessionState::ConnectingMedia)?;

    let result = async {
        session.transition(SessionState::MediaActive)?;
        info!(dial = %session.dial(), state = ?session.state(), "event=bridge_start");
        let started = tokio::time::Instant::now();

        let (to_media, to_sl) =
            relay_media_websocket(sl_stream, &call.media_websocket_url, cfg.connect_timeout)
                .await
                .with_context(|| {
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

async fn relay_media_websocket(
    sl_stream: tokio::net::UnixStream,
    media_websocket_url: &str,
    connect_timeout: std::time::Duration,
) -> Result<(u64, u64)> {
    let (media_ws, _) = timeout(connect_timeout, connect_async(media_websocket_url))
        .await
        .with_context(|| {
            format!(
                "timed out connecting to media websocket {}",
                media_websocket_url
            )
        })?
        .with_context(|| {
            format!(
                "failed connecting to media websocket {}",
                media_websocket_url
            )
        })?;

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
