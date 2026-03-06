use clap::Parser;
use std::time::Duration;

#[derive(Debug, Clone, Parser)]
#[command(name = "slmodem-sip-bridge")]
#[command(about = "Bridge slmodemd audio socket to a SIP/RTP endpoint")]
pub struct Cli {
    #[arg(help = "Dial string from slmodemd")]
    pub dial_string: String,

    #[arg(help = "Inherited socket file descriptor from slmodemd")]
    pub socket_fd: i32,

    #[arg(
        long,
        env = "SIP_LOCAL_ADDR",
        default_value = "0.0.0.0",
        help = "Local address for SIP and RTP sockets"
    )]
    pub sip_local_addr: String,

    #[arg(
        long,
        env = "SIP_LOCAL_RTP_PORT",
        default_value_t = 20000u16,
        help = "Local port for RTP media"
    )]
    pub sip_local_rtp_port: u16,

    #[arg(
        long,
        env = "SIP_INVITE_TIMEOUT_SECS",
        default_value_t = 60u64,
        help = "Timeout waiting for INVITE response (ring + answer)"
    )]
    pub invite_timeout_secs: u64,

    #[arg(
        long,
        env = "TELNYX_SIP_USER",
        help = "Telnyx SIP username for REGISTER/INVITE authentication"
    )]
    pub telnyx_sip_user: String,

    #[arg(
        long,
        env = "TELNYX_SIP_PASS",
        help = "Telnyx SIP password"
    )]
    pub telnyx_sip_pass: String,

    #[arg(
        long,
        env = "TELNYX_SIP_DOMAIN",
        default_value = "sip.telnyx.com",
        help = "Telnyx SIP registrar domain"
    )]
    pub telnyx_sip_domain: String,

    #[arg(
        long,
        env = "TELNYX_OUTBOUND_CID",
        default_value = "",
        help = "Outbound caller ID number (your Telnyx DID)"
    )]
    pub telnyx_outbound_cid: String,

    #[arg(
        long,
        env = "TELNYX_OUTBOUND_NAME",
        default_value = "OOB-Console-Hub",
        help = "Outbound caller display name"
    )]
    pub telnyx_outbound_name: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub dial_string: String,
    pub socket_fd: i32,
    pub sip_local_addr: String,
    pub sip_local_rtp_port: u16,
    pub invite_timeout: Duration,
    pub telnyx_sip_user: String,
    pub telnyx_sip_pass: String,
    pub telnyx_sip_domain: String,
    pub caller_id: String,
    pub caller_name: String,
}

impl From<Cli> for Config {
    fn from(cli: Cli) -> Self {
        Self {
            dial_string: cli.dial_string,
            socket_fd: cli.socket_fd,
            sip_local_addr: cli.sip_local_addr,
            sip_local_rtp_port: cli.sip_local_rtp_port,
            invite_timeout: Duration::from_secs(cli.invite_timeout_secs),
            telnyx_sip_user: cli.telnyx_sip_user,
            telnyx_sip_pass: cli.telnyx_sip_pass,
            telnyx_sip_domain: cli.telnyx_sip_domain,
            caller_id: cli.telnyx_outbound_cid,
            caller_name: cli.telnyx_outbound_name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            dial_string: "15551234567".to_string(),
            socket_fd: 3,
            sip_local_addr: "0.0.0.0".to_string(),
            sip_local_rtp_port: 20000,
            invite_timeout: Duration::from_secs(60),
            telnyx_sip_user: "testuser".to_string(),
            telnyx_sip_pass: "testpass".to_string(),
            telnyx_sip_domain: "sip.telnyx.com".to_string(),
            caller_id: "+15551234567".to_string(),
            caller_name: "OOB-Console-Hub".to_string(),
        }
    }

    #[test]
    fn config_from_cli_maps_fields() {
        let cli = Cli {
            dial_string: "18005551212".to_string(),
            socket_fd: 4,
            sip_local_addr: "10.0.0.1".to_string(),
            sip_local_rtp_port: 30000,
            invite_timeout_secs: 90,
            telnyx_sip_user: "user1".to_string(),
            telnyx_sip_pass: "pass1".to_string(),
            telnyx_sip_domain: "custom.sip.com".to_string(),
            telnyx_outbound_cid: "+18885551212".to_string(),
            telnyx_outbound_name: "My Bridge".to_string(),
        };

        let cfg: Config = cli.into();
        assert_eq!(cfg.dial_string, "18005551212");
        assert_eq!(cfg.socket_fd, 4);
        assert_eq!(cfg.sip_local_addr, "10.0.0.1");
        assert_eq!(cfg.sip_local_rtp_port, 30000);
        assert_eq!(cfg.invite_timeout, Duration::from_secs(90));
        assert_eq!(cfg.telnyx_sip_user, "user1");
        assert_eq!(cfg.telnyx_sip_pass, "pass1");
        assert_eq!(cfg.telnyx_sip_domain, "custom.sip.com");
        assert_eq!(cfg.caller_id, "+18885551212");
        assert_eq!(cfg.caller_name, "My Bridge");
    }

    #[test]
    fn default_invite_timeout_is_60s() {
        let cfg = test_config();
        assert_eq!(cfg.invite_timeout, Duration::from_secs(60));
    }

    #[test]
    fn default_rtp_port_is_20000() {
        let cfg = test_config();
        assert_eq!(cfg.sip_local_rtp_port, 20000);
    }
}
