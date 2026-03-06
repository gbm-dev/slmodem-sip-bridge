mod bridge;
mod codec;
mod config;
mod dial_string;
mod rtp;
mod sdp;
mod session;
mod sip;
mod slmodem;

use anyhow::Result;
use clap::Parser;
use config::{Cli, Config};
use dial_string::DialString;

#[tokio::main]
async fn main() -> Result<()> {
    // Pre-tracing guard: if we crash before tracing initializes (e.g. bad args
    // from slmodemd, missing env vars), this ensures we leave a breadcrumb on
    // stderr that operators can find in `docker logs`.
    eprintln!("slmodem-sip-bridge: process started, pid={}", std::process::id());

    init_logging();

    let cli = Cli::parse();
    let mut cfg: Config = cli.into();

    // Validate the dial string is well-formed before proceeding. Malformed
    // input from slmodemd should fail fast with a clear message, not propagate
    // as a confusing SIP error later.
    let dial = DialString::parse(&cfg.dial_string)?;
    cfg.dial_string = dial.as_str().to_string();

    if cfg.caller_id.is_empty() {
        tracing::warn!("event=no_caller_id, reason=TELNYX_OUTBOUND_CID not set — outbound calls will likely get 403 from provider");
    }

    tracing::info!(
        dial = %cfg.dial_string,
        fd = cfg.socket_fd,
        caller_id = %cfg.caller_id,
        sip_domain = %cfg.telnyx_sip_domain,
        rtp_port = cfg.sip_local_rtp_port,
        "event=startup"
    );

    let result = bridge::run(cfg).await;

    match &result {
        Ok(()) => tracing::info!("event=shutdown_clean"),
        Err(err) => tracing::error!(error = %err, "event=shutdown_error"),
    }

    result
}

fn init_logging() {
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "slmodem_sip_bridge=debug,info".to_string());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
