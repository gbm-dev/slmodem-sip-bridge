//! Minimal SDP offer/answer for ulaw-only modem calls.
//!
//! Builds SDP offers with a single codec (PCMU, payload type 0) and parses
//! SDP answers to extract the remote RTP endpoint. No codec negotiation
//! beyond verifying the answer includes PT 0.

use anyhow::{bail, Result};
use std::net::IpAddr;

/// SDP protocol version. Always 0 per RFC 4566 §5.1.
const SDP_VERSION: u8 = 0;

/// ptime in milliseconds. 20ms is the standard for G.711 and matches
/// slmodemd's 160-sample frame size at 8kHz.
const PTIME_MS: u16 = 20;

/// Build an SDP offer for a ulaw-only audio session.
///
/// The offer advertises a single RTP stream on the given local IP:port,
/// with PCMU (PT 0) as the only codec. This prevents any codec negotiation
/// ambiguity — the remote end must accept ulaw or reject the call.
pub fn build_offer(local_ip: IpAddr, rtp_port: u16, session_id: u64) -> String {
    assert!(rtp_port > 0, "RTP port must be positive, got 0");

    let ip_ver = match local_ip {
        IpAddr::V4(_) => "IP4",
        IpAddr::V6(_) => "IP6",
    };

    // RFC 4566 minimal SDP with single audio m-line.
    // Session-level c= line applies to all media.
    format!(
        "v={version}\r\n\
         o=slmodem-sip-bridge {sess_id} {sess_id} IN {ip_ver} {ip}\r\n\
         s=slmodem-sip-bridge\r\n\
         c=IN {ip_ver} {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 0\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=ptime:{ptime}\r\n\
         a=sendrecv\r\n",
        version = SDP_VERSION,
        sess_id = session_id,
        ip_ver = ip_ver,
        ip = local_ip,
        port = rtp_port,
        ptime = PTIME_MS,
    )
}

/// Parsed remote endpoint from an SDP answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdpAnswer {
    pub addr: IpAddr,
    pub port: u16,
}

/// Parse an SDP answer to extract the remote RTP address and port.
///
/// Extracts the connection address from `c=` line and port from `m=audio`
/// line. Rejects answers that don't include payload type 0 (PCMU) in the
/// m-line, since we only support ulaw.
pub fn parse_answer(sdp: &str) -> Result<SdpAnswer> {
    let mut connection_addr: Option<IpAddr> = None;
    let mut media_port: Option<u16> = None;
    let mut has_audio_line = false;

    for line in sdp.lines() {
        let line = line.trim();

        // c=IN IP4 1.2.3.4  or  c=IN IP6 ::1
        if line.starts_with("c=") {
            let parts: Vec<&str> = line[2..].split_whitespace().collect();
            if parts.len() >= 3 {
                if let Ok(ip) = parts[2].parse::<IpAddr>() {
                    connection_addr = Some(ip);
                }
            }
        }

        // m=audio 20000 RTP/AVP 0 ...
        if line.starts_with("m=audio ") {
            has_audio_line = true;
            let parts: Vec<&str> = line[8..].split_whitespace().collect();
            if parts.is_empty() {
                bail!("SDP m=audio line has no port");
            }

            // Parse port
            media_port = Some(parts[0].parse::<u16>().map_err(|e| {
                anyhow::anyhow!("invalid port in SDP m=audio line: {e}")
            })?);

            // Verify PCMU (PT 0) is offered. Payload types start at parts[2]
            // (after port and proto).
            if parts.len() < 3 {
                bail!("SDP m=audio line has no payload types");
            }
            let payload_types = &parts[2..];
            if !payload_types.iter().any(|&pt| pt == "0") {
                bail!(
                    "SDP answer does not include PCMU (PT 0), offered: {:?}",
                    payload_types
                );
            }
        }
    }

    if !has_audio_line {
        bail!("SDP answer has no m=audio line");
    }

    let addr = connection_addr.ok_or_else(|| anyhow::anyhow!("SDP answer has no c= connection line"))?;
    let port = media_port.ok_or_else(|| anyhow::anyhow!("SDP answer has no media port"))?;

    Ok(SdpAnswer { addr, port })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn offer_format_contains_required_lines() {
        let offer = build_offer(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 20000, 12345);

        assert!(offer.contains("v=0\r\n"), "must have SDP version");
        assert!(offer.contains("o=slmodem-sip-bridge 12345 12345 IN IP4 10.0.0.1\r\n"));
        assert!(offer.contains("c=IN IP4 10.0.0.1\r\n"));
        assert!(offer.contains("m=audio 20000 RTP/AVP 0\r\n"));
        assert!(offer.contains("a=rtpmap:0 PCMU/8000\r\n"));
        assert!(offer.contains("a=ptime:20\r\n"));
        assert!(offer.contains("a=sendrecv\r\n"));
    }

    #[test]
    fn offer_uses_ip6_when_given_v6_addr() {
        let offer = build_offer("::1".parse().unwrap(), 30000, 1);
        assert!(offer.contains("IN IP6 ::1"));
    }

    #[test]
    fn parse_answer_extracts_ip_and_port() {
        let sdp = "\
            v=0\r\n\
            o=telnyx 1 1 IN IP4 192.168.1.100\r\n\
            s=session\r\n\
            c=IN IP4 192.168.1.100\r\n\
            t=0 0\r\n\
            m=audio 30000 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n";

        let answer = parse_answer(sdp).unwrap();
        assert_eq!(answer.addr, Ipv4Addr::new(192, 168, 1, 100));
        assert_eq!(answer.port, 30000);
    }

    #[test]
    fn parse_answer_rejects_no_ulaw() {
        let sdp = "\
            v=0\r\n\
            c=IN IP4 10.0.0.1\r\n\
            m=audio 20000 RTP/AVP 8\r\n"; // PT 8 = PCMA (alaw), no ulaw

        let err = parse_answer(sdp).expect_err("must reject non-ulaw answer");
        assert!(err.to_string().contains("PCMU"));
    }

    #[test]
    fn parse_answer_accepts_multiple_codecs_including_ulaw() {
        let sdp = "\
            v=0\r\n\
            c=IN IP4 10.0.0.1\r\n\
            m=audio 20000 RTP/AVP 0 8 101\r\n";

        let answer = parse_answer(sdp).unwrap();
        assert_eq!(answer.port, 20000);
    }

    #[test]
    fn parse_answer_rejects_missing_audio_line() {
        let sdp = "\
            v=0\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n";

        let err = parse_answer(sdp).expect_err("must reject SDP without m=audio");
        assert!(err.to_string().contains("m=audio"));
    }

    #[test]
    fn parse_answer_rejects_missing_connection_line() {
        let sdp = "\
            v=0\r\n\
            m=audio 20000 RTP/AVP 0\r\n";

        let err = parse_answer(sdp).expect_err("must reject SDP without c= line");
        assert!(err.to_string().contains("connection"));
    }
}
