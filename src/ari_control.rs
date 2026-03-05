use crate::ari_external_media::{Encapsulation, ExternalMediaRequest, Transport};
use crate::config::Config;
use anyhow::{bail, Context, Result};
use reqwest::StatusCode;
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, Instant};
use tokio_tungstenite::tungstenite;
use tracing::{debug, warn};
use url::Url;

#[derive(Debug, Clone)]
pub struct ActiveCall {
    pub bridge_id: String,
    pub outbound_channel_id: String,
    pub media_channel_id: String,
    pub media_websocket_url: String,
}

#[derive(Debug, Clone)]
pub struct AriController {
    client: reqwest::Client,
    base_url: Url,
    username: String,
    password: String,
    dial_endpoint_template: String,
    originate_timeout: Duration,
}

impl AriController {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        // Url::join replaces the last path segment unless the base ends with '/'.
        // Ensure trailing slash so "/ari" + "bridges" = "/ari/bridges", not "/bridges".
        let mut base = cfg.ari_base_url.clone();
        if !base.ends_with('/') {
            base.push('/');
        }
        let base_url =
            Url::parse(&base).with_context(|| format!("invalid ARI URL {}", cfg.ari_base_url))?;
        Ok(Self {
            client: reqwest::Client::new(),
            base_url,
            username: cfg.ari_username.clone(),
            password: cfg.ari_password.clone(),
            dial_endpoint_template: cfg.ari_dial_endpoint_template.clone(),
            originate_timeout: cfg.originate_timeout,
        })
    }

    /// Open the ARI events WebSocket, which registers the Stasis app with
    /// Asterisk. The returned stream must be kept alive for the duration of
    /// the call — dropping it unregisters the app.
    pub async fn connect_events(
        &self,
        app: &str,
    ) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>
    {
        let mut ws_url = self.base_url.clone();
        let scheme = match ws_url.scheme() {
            "http" => "ws",
            "https" => "wss",
            s => bail!("unsupported ARI URL scheme for events websocket: {s}"),
        };
        ws_url
            .set_scheme(scheme)
            .map_err(|_| anyhow::anyhow!("failed to convert ARI URL scheme to websocket"))?;
        ws_url.set_path("/ari/events");
        ws_url
            .query_pairs_mut()
            .clear()
            .append_pair("app", app)
            .append_pair("subscribeAll", "true");

        let auth = format!("{}:{}", self.username, self.password);
        let auth_header =
            format!("Basic {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &auth));

        let request = tungstenite::http::Request::builder()
            .uri(ws_url.as_str())
            .header("Authorization", &auth_header)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tungstenite::handshake::client::generate_key(),
            )
            .body(())
            .context("failed to build ARI events websocket request")?;

        let (stream, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .context("failed to connect ARI events websocket")?;

        debug!(app, url = %ws_url, "event=ari_events_connected");
        Ok(stream)
    }

    pub async fn start_call(
        &self,
        dial: &str,
        media_req: &ExternalMediaRequest,
    ) -> Result<ActiveCall> {
        self.validate_media_mode(media_req)?;

        let bridge_id = unique_id("slmodem-bridge");
        let media_channel_id = unique_id("slmodem-media");
        let outbound_channel_id = unique_id("slmodem-out");

        self.create_bridge(&bridge_id).await?;
        if let Err(err) = self
            .create_external_media(&media_channel_id, media_req)
            .await
        {
            self.best_effort_destroy_bridge(&bridge_id).await;
            return Err(err);
        }

        if let Err(err) = self
            .create_and_dial_outbound(dial, &outbound_channel_id, &media_req.app)
            .await
        {
            self.best_effort_hangup_channel(&media_channel_id).await;
            self.best_effort_destroy_bridge(&bridge_id).await;
            return Err(err);
        }

        if let Err(err) = self
            .add_channels_to_bridge(&bridge_id, &[&outbound_channel_id, &media_channel_id])
            .await
        {
            self.best_effort_hangup_channel(&outbound_channel_id).await;
            self.best_effort_hangup_channel(&media_channel_id).await;
            self.best_effort_destroy_bridge(&bridge_id).await;
            return Err(err);
        }

        if let Err(err) = self
            .wait_channel_up(&outbound_channel_id, self.originate_timeout)
            .await
        {
            self.best_effort_hangup_channel(&outbound_channel_id).await;
            self.best_effort_hangup_channel(&media_channel_id).await;
            self.best_effort_destroy_bridge(&bridge_id).await;
            return Err(err);
        }

        let connection_id = self
            .wait_for_channel_variable(
                &media_channel_id,
                "MEDIA_WEBSOCKET_CONNECTION_ID",
                Duration::from_secs(10),
            )
            .await
            .context("missing MEDIA_WEBSOCKET_CONNECTION_ID for media websocket connection")?;
        let media_websocket_url = self.media_websocket_url(&connection_id)?;

        Ok(ActiveCall {
            bridge_id,
            outbound_channel_id,
            media_channel_id,
            media_websocket_url,
        })
    }

    pub async fn teardown(&self, call: &ActiveCall) {
        self.best_effort_hangup_channel(&call.outbound_channel_id)
            .await;
        self.best_effort_hangup_channel(&call.media_channel_id)
            .await;
        self.best_effort_destroy_bridge(&call.bridge_id).await;
    }

    fn validate_media_mode(&self, media_req: &ExternalMediaRequest) -> Result<()> {
        let is_supported = media_req.transport == Transport::WebSocket
            && media_req.encapsulation == Encapsulation::None
            && media_req.connection_type.eq_ignore_ascii_case("server");

        if !is_supported {
            bail!(
                "unsupported media mode: expected websocket/none/server, got {}/{}/{}",
                media_req.transport.as_str(),
                media_req.encapsulation.as_str(),
                media_req.connection_type
            );
        }

        Ok(())
    }

    fn endpoint_from_dial(&self, dial: &str) -> String {
        self.dial_endpoint_template.replace("{dial}", dial)
    }

    fn media_websocket_url(&self, connection_id: &str) -> Result<String> {
        let mut ws_url = self.base_url.clone();
        let scheme = match ws_url.scheme() {
            "http" => "ws",
            "https" => "wss",
            s => bail!("unsupported ARI URL scheme for media websocket: {s}"),
        };
        ws_url
            .set_scheme(scheme)
            .map_err(|_| anyhow::anyhow!("failed to convert ARI URL scheme to websocket"))?;
        ws_url.set_path(&format!("/media/{connection_id}"));
        ws_url.set_query(None);
        Ok(ws_url.to_string())
    }

    async fn create_bridge(&self, bridge_id: &str) -> Result<()> {
        let url = self.path_url("/bridges")?;
        self.client
            .post(url)
            .basic_auth(&self.username, Some(&self.password))
            .query(&[("type", "mixing,proxy_media"), ("bridgeId", bridge_id)])
            .send()
            .await
            .context("failed to create ARI bridge request")?
            .error_for_status()
            .context("ARI bridge creation failed")?;
        Ok(())
    }

    async fn create_external_media(
        &self,
        channel_id: &str,
        media_req: &ExternalMediaRequest,
    ) -> Result<()> {
        let url = self.path_url("/channels/externalMedia")?;
        let mut query = media_req.query_pairs();
        query.push(("channelId".to_string(), channel_id.to_string()));

        self.client
            .post(url)
            .basic_auth(&self.username, Some(&self.password))
            .query(&query)
            .send()
            .await
            .context("failed to create external media request")?
            .error_for_status()
            .context("ARI externalMedia creation failed")?;
        Ok(())
    }

    async fn create_and_dial_outbound(
        &self,
        dial: &str,
        channel_id: &str,
        app: &str,
    ) -> Result<()> {
        let endpoint = self.endpoint_from_dial(dial);
        let create_url = self.path_url("/channels/create")?;

        self.client
            .post(create_url)
            .basic_auth(&self.username, Some(&self.password))
            .query(&[
                ("endpoint", endpoint.as_str()),
                ("app", app),
                ("channelId", channel_id),
                ("appArgs", dial),
            ])
            .send()
            .await
            .context("failed to create outbound channel request")?
            .error_for_status()
            .context("ARI outbound channel creation failed")?;

        let dial_url = self.path_url(&format!("/channels/{channel_id}/dial"))?;
        self.client
            .post(dial_url)
            .basic_auth(&self.username, Some(&self.password))
            .query(&[("timeout", self.originate_timeout.as_secs())])
            .send()
            .await
            .context("failed to dial outbound channel request")?
            .error_for_status()
            .context("ARI outbound dial failed")?;

        Ok(())
    }

    async fn add_channels_to_bridge(&self, bridge_id: &str, channel_ids: &[&str]) -> Result<()> {
        let url = self.path_url(&format!("/bridges/{bridge_id}/addChannel"))?;
        let joined = channel_ids.join(",");

        self.client
            .post(url)
            .basic_auth(&self.username, Some(&self.password))
            .query(&[("channel", joined.as_str())])
            .send()
            .await
            .context("failed to add channels to bridge request")?
            .error_for_status()
            .context("ARI addChannel failed")?;
        Ok(())
    }

    async fn wait_channel_up(&self, channel_id: &str, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        while start.elapsed() <= timeout {
            match self.get_channel_state(channel_id).await? {
                Some(state) if state.eq_ignore_ascii_case("up") => return Ok(()),
                Some(_) => sleep(Duration::from_millis(150)).await,
                None => bail!("outbound channel {channel_id} disappeared before answer"),
            }
        }

        bail!("timed out waiting for outbound channel {channel_id} to reach Up state")
    }

    async fn wait_for_channel_variable(
        &self,
        channel_id: &str,
        variable: &str,
        timeout: Duration,
    ) -> Result<String> {
        let start = Instant::now();
        while start.elapsed() <= timeout {
            if let Some(value) = self.get_channel_variable(channel_id, variable).await? {
                return Ok(value);
            }
            sleep(Duration::from_millis(100)).await;
        }

        bail!("timed out waiting for variable {variable} on channel {channel_id}")
    }

    async fn get_channel_state(&self, channel_id: &str) -> Result<Option<String>> {
        let url = self.path_url(&format!("/channels/{channel_id}"))?;
        let resp = self
            .client
            .get(url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .context("failed to query outbound channel state")?;

        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let chan: AriChannel = resp
            .error_for_status()
            .context("ARI channel state query failed")?
            .json()
            .await
            .context("failed to decode ARI channel state response")?;
        Ok(Some(chan.state))
    }

    async fn get_channel_variable(
        &self,
        channel_id: &str,
        variable: &str,
    ) -> Result<Option<String>> {
        let url = self.path_url(&format!("/channels/{channel_id}/variable"))?;
        let resp = self
            .client
            .get(url)
            .basic_auth(&self.username, Some(&self.password))
            .query(&[("variable", variable)])
            .send()
            .await
            .with_context(|| format!("failed to query ARI variable {variable}"))?;

        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let var: AriVariable = resp
            .error_for_status()
            .with_context(|| format!("ARI variable query failed for {variable}"))?
            .json()
            .await
            .with_context(|| format!("failed to decode ARI variable response for {variable}"))?;

        Ok(var.value.filter(|v| !v.is_empty()))
    }

    async fn best_effort_hangup_channel(&self, channel_id: &str) {
        let url = match self.path_url(&format!("/channels/{channel_id}")) {
            Ok(url) => url,
            Err(err) => {
                warn!(channel_id, error = %err, "event=cleanup_hangup_url_error");
                return;
            }
        };

        match self
            .client
            .delete(url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
        {
            Ok(resp) if resp.status() == StatusCode::NOT_FOUND => {}
            Ok(resp) if !resp.status().is_success() => {
                warn!(
                    channel_id,
                    status = %resp.status(),
                    "event=cleanup_hangup_failed"
                );
            }
            Ok(_) => {}
            Err(err) => {
                warn!(channel_id, error = %err, "event=cleanup_hangup_failed");
            }
        }
    }

    async fn best_effort_destroy_bridge(&self, bridge_id: &str) {
        let url = match self.path_url(&format!("/bridges/{bridge_id}")) {
            Ok(url) => url,
            Err(err) => {
                warn!(bridge_id, error = %err, "event=cleanup_bridge_url_error");
                return;
            }
        };

        match self
            .client
            .delete(url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
        {
            Ok(resp) if resp.status() == StatusCode::NOT_FOUND => {}
            Ok(resp) if !resp.status().is_success() => {
                warn!(
                    bridge_id,
                    status = %resp.status(),
                    "event=cleanup_bridge_failed"
                );
            }
            Ok(_) => {}
            Err(err) => {
                warn!(bridge_id, error = %err, "event=cleanup_bridge_failed");
            }
        }
    }

    fn path_url(&self, path: &str) -> Result<Url> {
        self.base_url
            .join(path.trim_start_matches('/'))
            .with_context(|| format!("failed to build ARI URL for path {path}"))
    }
}

#[derive(Debug, Deserialize)]
struct AriChannel {
    state: String,
}

#[derive(Debug, Deserialize)]
struct AriVariable {
    value: Option<String>,
}

fn unique_id(prefix: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_micros();
    format!("{prefix}-{now}-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::AriController;
    use crate::config::Config;
    use std::time::Duration;

    fn cfg() -> Config {
        Config {
            dial_string: "15551234567".to_string(),
            socket_fd: 3,
            media_addr: "127.0.0.1:21000".to_string(),
            connect_timeout: Duration::from_secs(3),
            ari_base_url: "http://127.0.0.1:8088/ari".to_string(),
            ari_username: "u".to_string(),
            ari_password: "p".to_string(),
            ari_dial_endpoint_template: "PJSIP/{dial}@telnyx-out".to_string(),
            originate_timeout: Duration::from_secs(60),
        }
    }

    #[test]
    fn endpoint_template_replaces_dial_placeholder() {
        let ctl = AriController::from_config(&cfg()).unwrap();
        assert_eq!(
            ctl.endpoint_from_dial("18005551212"),
            "PJSIP/18005551212@telnyx-out"
        );
    }

    #[test]
    fn path_url_preserves_ari_prefix() {
        let ctl = AriController::from_config(&cfg()).unwrap();
        let url = ctl.path_url("/bridges").unwrap();
        assert_eq!(url.path(), "/ari/bridges");
    }

    #[test]
    fn builds_media_websocket_url_from_ari_base() {
        let ctl = AriController::from_config(&cfg()).unwrap();
        let url = ctl.media_websocket_url("media-conn-1").unwrap();
        assert_eq!(url, "ws://127.0.0.1:8088/media/media-conn-1");
    }
}
