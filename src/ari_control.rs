use base64::Engine;
use crate::ari_external_media::{Encapsulation, ExternalMediaRequest, Transport};
use crate::config::Config;
use anyhow::{bail, Context, Result};
use reqwest::StatusCode;
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, Instant};
use tracing::{debug, info, warn};
use url::Url;

#[derive(Debug, Clone)]
pub struct MediaSetup {
    pub bridge_id: String,
    pub media_channel_id: String,
    pub media_websocket_url: String,
}

#[derive(Debug, Clone)]
pub struct ActiveCall {
    pub bridge_id: String,
    pub outbound_channel_id: String,
    pub media_channel_id: String,
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
        assert!(!cfg.ari_base_url.is_empty(), "ARI base URL must not be empty");
        assert!(!cfg.ari_username.is_empty(), "ARI username must not be empty");

        // Url::join replaces the last path segment unless the base ends with '/'.
        // Ensure trailing slash so "/ari" + "bridges" = "/ari/bridges", not "/bridges".
        let mut base = cfg.ari_base_url.clone();
        if !base.ends_with('/') {
            base.push('/');
        }
        let base_url =
            Url::parse(&base).with_context(|| format!("invalid ARI URL {}", cfg.ari_base_url))?;

        info!(base_url = %base_url, "event=ari_controller_created");

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
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    > {
        let mut ws_url = self.path_url("/events")?;
        let scheme = match ws_url.scheme() {
            "http" => "ws",
            "https" => "wss",
            s => bail!("unsupported ARI URL scheme for events websocket: {s}"),
        };
        ws_url
            .set_scheme(scheme)
            .map_err(|_| anyhow::anyhow!("failed to convert ARI URL scheme to websocket"))?;

        ws_url
            .query_pairs_mut()
            .clear()
            .append_pair("app", app);

        // Build request with explicit Basic Auth header
        let auth = format!("{}:{}", self.username, self.password);
        let auth_base64 = base64::engine::general_purpose::STANDARD.encode(auth);
        let auth_header = format!("Basic {auth_base64}");

        let request = tokio_tungstenite::tungstenite::handshake::client::Request::builder()
            .uri(ws_url.as_str())
            .header("Authorization", auth_header)
            .header("Host", ws_url.host_str().unwrap_or("localhost"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Key", tokio_tungstenite::tungstenite::handshake::client::generate_key())
            .header("Sec-WebSocket-Version", "13")
            .body(())?;

        // Asterisk may still be starting when the bridge is spawned.
        // Use enough retries with exponential backoff to survive the
        // typical 5-15 second Asterisk startup window. 20 retries with
        // 500 ms base and 2x backoff capped at 5 s gives ~60 s total.
        let max_retries: u32 = 20;
        let backoff_base_ms: u64 = 500;
        let backoff_cap = Duration::from_secs(5);
        let mut attempt: u32 = 0;

        info!(
            url = %ws_url,
            app,
            max_retries,
            backoff_base_ms,
            "event=ari_events_connecting, reason=registering stasis app with asterisk"
        );

        loop {
            match tokio_tungstenite::connect_async(request.clone()).await {
                Ok((stream, _resp)) => {
                    debug!(app, url = %ws_url, attempt, "event=ari_events_connected");
                    return Ok(stream);
                }
                Err(err) => {
                    attempt += 1;
                    if attempt >= max_retries {
                        return Err(anyhow::anyhow!(err))
                            .with_context(|| format!(
                                "failed to connect ARI events websocket to {ws_url} after {max_retries} attempts"
                            ));
                    }
                    let multiplier = 1u32.checked_shl(attempt.min(10)).unwrap_or(u32::MAX);
                    let delay = Duration::from_millis(backoff_base_ms)
                        .saturating_mul(multiplier)
                        .min(backoff_cap);
                    warn!(
                        error = %err,
                        attempt,
                        max_retries,
                        delay_ms = delay.as_millis(),
                        "event=ari_events_connect_retry",
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Set up the media channel and obtain its WebSocket URL, but do not
    /// dial the outbound call yet.  This lets the caller establish the media
    /// WebSocket connection before placing the PSTN call.
    pub async fn setup_media(
        &self,
        media_req: &ExternalMediaRequest,
    ) -> Result<MediaSetup> {
        assert!(!media_req.app.is_empty(), "stasis app name must not be empty");
        self.validate_media_mode(media_req)?;

        let bridge_id = unique_id("slmodem-bridge");
        let media_channel_id = unique_id("slmodem-media");

        info!(
            bridge_id = %bridge_id,
            media_channel_id = %media_channel_id,
            "event=setup_media_start"
        );

        self.create_bridge(&bridge_id).await?;
        if let Err(err) = self
            .create_external_media(&media_channel_id, media_req)
            .await
        {
            self.best_effort_destroy_bridge(&bridge_id).await;
            return Err(err);
        }

        let connection_id = match self
            .wait_for_channel_variable(
                &media_channel_id,
                "MEDIA_WEBSOCKET_CONNECTION_ID",
                Duration::from_secs(10),
            )
            .await
        {
            Ok(id) => id,
            Err(err) => {
                self.best_effort_hangup_channel(&media_channel_id).await;
                self.best_effort_destroy_bridge(&bridge_id).await;
                return Err(
                    err.context("missing MEDIA_WEBSOCKET_CONNECTION_ID for media websocket connection"),
                );
            }
        };
        let media_websocket_url = self.media_websocket_url(&connection_id)?;

        Ok(MediaSetup {
            bridge_id,
            media_channel_id,
            media_websocket_url,
        })
    }

    /// Dial the outbound call and bridge it with the already-created media
    /// channel.  Call this after connecting to the media WebSocket.
    pub async fn dial_and_bridge(
        &self,
        dial: &str,
        app: &str,
        setup: &MediaSetup,
    ) -> Result<ActiveCall> {
        assert!(!dial.is_empty(), "dial string must not be empty");
        let outbound_channel_id = unique_id("slmodem-out");
        let endpoint = self.endpoint_from_dial(dial);

        info!(
            dial,
            endpoint = %endpoint,
            outbound_channel_id = %outbound_channel_id,
            bridge_id = %setup.bridge_id,
            "event=dial_and_bridge_start"
        );

        if let Err(err) = self
            .create_and_dial_outbound(dial, &outbound_channel_id, app)
            .await
        {
            self.best_effort_hangup_channel(&setup.media_channel_id).await;
            self.best_effort_destroy_bridge(&setup.bridge_id).await;
            return Err(err);
        }

        if let Err(err) = self
            .add_channels_to_bridge(
                &setup.bridge_id,
                &[&outbound_channel_id, &setup.media_channel_id],
            )
            .await
        {
            self.best_effort_hangup_channel(&outbound_channel_id).await;
            self.best_effort_hangup_channel(&setup.media_channel_id).await;
            self.best_effort_destroy_bridge(&setup.bridge_id).await;
            return Err(err);
        }

        if let Err(err) = self
            .wait_channel_up(&outbound_channel_id, self.originate_timeout)
            .await
        {
            self.best_effort_hangup_channel(&outbound_channel_id).await;
            self.best_effort_hangup_channel(&setup.media_channel_id).await;
            self.best_effort_destroy_bridge(&setup.bridge_id).await;
            return Err(err);
        }

        Ok(ActiveCall {
            bridge_id: setup.bridge_id.clone(),
            outbound_channel_id,
            media_channel_id: setup.media_channel_id.clone(),
        })
    }

    /// Clean up a media setup that never progressed to a full call.
    /// Hangs up the media channel and destroys the bridge.
    pub async fn cleanup_media(&self, setup: &MediaSetup) {
        info!(
            media_channel_id = %setup.media_channel_id,
            bridge_id = %setup.bridge_id,
            "event=cleanup_media_start"
        );
        self.best_effort_hangup_channel(&setup.media_channel_id)
            .await;
        self.best_effort_destroy_bridge(&setup.bridge_id).await;
        info!("event=cleanup_media_complete");
    }

    /// Tear down all resources for a completed call: outbound channel,
    /// media channel, and Asterisk bridge.
    pub async fn teardown(&self, call: &ActiveCall) {
        info!(
            outbound_channel_id = %call.outbound_channel_id,
            media_channel_id = %call.media_channel_id,
            bridge_id = %call.bridge_id,
            "event=teardown_ari_resources"
        );
        self.best_effort_hangup_channel(&call.outbound_channel_id)
            .await;
        self.best_effort_hangup_channel(&call.media_channel_id)
            .await;
        self.best_effort_destroy_bridge(&call.bridge_id).await;
        info!("event=teardown_ari_complete");
    }

    /// Only websocket/none/server mode is implemented. Other combinations
    /// (UDP+RTP, AudioSocket) would require different relay logic in bridge.rs.
    /// Fail fast here rather than producing a confusing Asterisk 4xx later.
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

    /// Build the media WebSocket URL from the connection ID.
    /// The media WebSocket is served at /media/{id} on the HTTP server root,
    /// NOT under the /ari REST prefix. We derive the host:port from the ARI
    /// base URL but strip the path.
    fn media_websocket_url(&self, connection_id: &str) -> Result<String> {
        let scheme = match self.base_url.scheme() {
            "http" => "ws",
            "https" => "wss",
            s => bail!("unsupported ARI URL scheme for media websocket: {s}"),
        };
        let host = self.base_url.host_str()
            .ok_or_else(|| anyhow::anyhow!("ARI base URL has no host"))?;
        let port = self.base_url.port().unwrap_or(8088);
        Ok(format!("{scheme}://{host}:{port}/media/{connection_id}"))
    }

    /// Connect to the media WebSocket with authentication.
    /// Uses an explicit Authorization header because tokio-tungstenite's
    /// connect_async ignores credentials embedded in the URL.
    pub async fn connect_media_websocket(
        &self,
        url: &str,
        connect_timeout: Duration,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    > {
        assert!(!url.is_empty(), "media websocket URL must not be empty");
        info!(url, timeout_ms = connect_timeout.as_millis(), "event=media_ws_connecting");

        let ws_url = Url::parse(url)
            .with_context(|| format!("invalid media websocket URL: {url}"))?;

        // Build request with explicit Basic Auth header and the required
        // "media" subprotocol. Asterisk's chan_websocket registers a
        // WebSocket handler with subprotocol "media" — without the
        // Sec-WebSocket-Protocol header, the upgrade is rejected.
        let auth = format!("{}:{}", self.username, self.password);
        let auth_base64 = base64::engine::general_purpose::STANDARD.encode(auth);
        let auth_header = format!("Basic {auth_base64}");

        let request = tokio_tungstenite::tungstenite::handshake::client::Request::builder()
            .uri(ws_url.as_str())
            .header("Authorization", auth_header)
            .header("Host", ws_url.host_str().unwrap_or("localhost"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Protocol", "media")
            .header("Sec-WebSocket-Key", tokio_tungstenite::tungstenite::handshake::client::generate_key())
            .header("Sec-WebSocket-Version", "13")
            .body(())?;

        let (stream, _resp) = tokio::time::timeout(
            connect_timeout,
            tokio_tungstenite::connect_async(request),
        )
        .await
        .with_context(|| {
            format!("timed out connecting to media websocket {url}")
        })?
        .with_context(|| {
            format!("failed connecting to media websocket {url}")
        })?;

        info!(url, "event=media_ws_connected");
        Ok(stream)
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
        debug!(channel_id, timeout_ms = timeout.as_millis(), "event=wait_channel_up_start");
        let start = Instant::now();
        while start.elapsed() <= timeout {
            match self.get_channel_state(channel_id).await? {
                Some(state) if state.eq_ignore_ascii_case("up") => {
                    info!(channel_id, elapsed_ms = start.elapsed().as_millis(), "event=channel_up");
                    return Ok(());
                }
                Some(state) => {
                    debug!(channel_id, state = %state, "event=channel_not_yet_up");
                    sleep(Duration::from_millis(150)).await;
                }
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
        debug!(channel_id, variable, timeout_ms = timeout.as_millis(), "event=wait_for_variable_start");
        let start = Instant::now();
        while start.elapsed() <= timeout {
            if let Some(value) = self.get_channel_variable(channel_id, variable).await? {
                info!(channel_id, variable, value = %value, "event=variable_resolved");
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
    fn builds_media_websocket_url_without_ari_prefix() {
        // Media WebSocket is served at /media/{id} on the HTTP root,
        // NOT under the /ari REST prefix.
        let ctl = AriController::from_config(&cfg()).unwrap();
        let url = ctl.media_websocket_url("media-conn-1").unwrap();
        assert_eq!(url, "ws://127.0.0.1:8088/media/media-conn-1");
    }
}
