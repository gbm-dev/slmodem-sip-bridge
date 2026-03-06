mod ari_control;
mod ari_external_media;
mod bridge;
mod config;
mod dial_string;
mod session;
mod slmodem;

use anyhow::Result;
use ari_external_media::ExternalMediaRequest;
use clap::Parser;
use config::{Cli, Config};
use dial_string::DialString;

#[tokio::main]
async fn main() -> Result<()> {
    // Pre-tracing guard: if we crash before tracing initializes (e.g. bad args
    // from slmodemd, missing env vars), this ensures we leave a breadcrumb on
    // stderr that operators can find in `docker logs`.
    eprintln!("slmodem-asterisk-bridge: process started, pid={}", std::process::id());

    init_logging();

    let cli = Cli::parse();
    let mut cfg: Config = cli.into();

    // Validate the dial string is well-formed before proceeding. Malformed
    // input from slmodemd should fail fast with a clear message, not propagate
    // as a confusing ARI error later.
    let dial = DialString::parse(&cfg.dial_string)?;
    cfg.dial_string = dial.as_str().to_string();

    let external_media = ExternalMediaRequest::from_env_defaults();
    let external_media_pairs = external_media.query_pairs();

    if cfg.caller_id.is_empty() {
        tracing::warn!("event=no_caller_id, reason=TELNYX_OUTBOUND_CID not set — outbound calls will likely get 403 from provider");
    }

    tracing::info!(
        dial = %cfg.dial_string,
        media = %cfg.media_addr,
        fd = cfg.socket_fd,
        caller_id = %cfg.caller_id,
        ari_base_url = %cfg.ari_base_url,
        ari_endpoint = ExternalMediaRequest::endpoint_path(),
        ari_transport = external_media.transport.as_str(),
        ari_encapsulation = external_media.encapsulation.as_str(),
        ari_query_pairs = ?external_media_pairs,
        "event=startup"
    );

    let result = bridge::run(cfg, external_media).await;

    match &result {
        Ok(()) => tracing::info!("event=shutdown_clean"),
        Err(err) => tracing::error!(error = %err, "event=shutdown_error"),
    }

    result
}

fn init_logging() {
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "slmodem_asterisk_bridge=debug,info".to_string());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
