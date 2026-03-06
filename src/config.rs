use clap::Parser;
use std::time::Duration;

#[derive(Debug, Clone, Parser)]
#[command(name = "slmodem-asterisk-bridge")]
#[command(about = "Bridge slmodemd audio socket to an Asterisk-side media endpoint")]
pub struct Cli {
    #[arg(help = "Dial string from slmodemd")]
    pub dial_string: String,

    #[arg(help = "Inherited socket file descriptor from slmodemd")]
    pub socket_fd: i32,

    #[arg(
        long,
        env = "BRIDGE_MEDIA_ADDR",
        default_value = "127.0.0.1:21000",
        help = "Target media endpoint address"
    )]
    pub media_addr: String,

    #[arg(
        long,
        env = "ARI_BASE_URL",
        default_value = "http://127.0.0.1:8088/ari",
        help = "Asterisk ARI base URL"
    )]
    pub ari_base_url: String,

    #[arg(
        long,
        env = "ARI_USERNAME",
        default_value = "slmodem",
        help = "Asterisk ARI username"
    )]
    pub ari_username: String,

    #[arg(
        long,
        env = "ARI_PASSWORD",
        default_value = "slmodem",
        help = "Asterisk ARI password"
    )]
    pub ari_password: String,

    #[arg(
        long,
        env = "ARI_DIAL_ENDPOINT_TEMPLATE",
        default_value = "PJSIP/{dial}@telnyx-out",
        help = "Dial endpoint template, where {dial} is replaced with dial string"
    )]
    pub ari_dial_endpoint_template: String,

    #[arg(
        long,
        env = "TELNYX_OUTBOUND_CID",
        default_value = "",
        help = "Outbound caller ID number (your Telnyx DID). Required by most SIP providers."
    )]
    pub caller_id: String,

    #[arg(
        long,
        env = "ARI_ORIGINATE_TIMEOUT_SECS",
        default_value_t = 60u64,
        help = "Timeout waiting for outbound channel to reach Up state"
    )]
    pub originate_timeout_secs: u64,

    #[arg(
        long,
        env = "BRIDGE_CONNECT_TIMEOUT_MS",
        default_value_t = 3000u64,
        help = "Connect timeout to media endpoint in milliseconds"
    )]
    pub connect_timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub dial_string: String,
    pub socket_fd: i32,
    pub media_addr: String,
    pub connect_timeout: Duration,
    pub ari_base_url: String,
    pub ari_username: String,
    pub ari_password: String,
    pub ari_dial_endpoint_template: String,
    pub originate_timeout: Duration,
    pub caller_id: String,
}

impl From<Cli> for Config {
    fn from(cli: Cli) -> Self {
        Self {
            dial_string: cli.dial_string,
            socket_fd: cli.socket_fd,
            media_addr: cli.media_addr,
            connect_timeout: Duration::from_millis(cli.connect_timeout_ms),
            ari_base_url: cli.ari_base_url,
            ari_username: cli.ari_username,
            ari_password: cli.ari_password,
            ari_dial_endpoint_template: cli.ari_dial_endpoint_template,
            originate_timeout: Duration::from_secs(cli.originate_timeout_secs),
            caller_id: cli.caller_id,
        }
    }
}
