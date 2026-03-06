//! Minimal RTP send/receive for PCMU (G.711 µ-law) audio.
//!
//! Implements only what's needed: 12-byte fixed header (RFC 3550), payload
//! type 0 (PCMU), no CSRC, no extensions. The modem use case is a single
//! point-to-point stream with no mixing or SSRC collision resolution.
//!
//! Split into `RtpSender` and `RtpReceiver` to allow concurrent send/recv
//! in separate tokio tasks without borrow conflicts.

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;

/// RTP header size in bytes. Fixed 12-byte header with V=2, no CSRC, no
/// extensions — the minimum for a valid RTP packet.
const RTP_HEADER_SIZE: usize = 12;

/// RTP payload type for PCMU (G.711 µ-law), per RFC 3551 Table 4.
const PT_PCMU: u8 = 0;

/// Timestamp increment per 20ms frame at 8kHz sample rate.
/// 8000 samples/sec * 0.020 sec = 160 samples.
const TIMESTAMP_INCREMENT: u32 = 160;

/// Maximum RTP packet size we'll accept. Standard Ethernet MTU minus IP/UDP
/// headers gives ~1472 bytes. 1500 is generous and catches jumbo payloads.
const MAX_RTP_PACKET_SIZE: usize = 1500;

/// Minimum valid RTP packet: 12-byte header + at least 1 byte payload.
const MIN_RTP_PACKET_SIZE: usize = RTP_HEADER_SIZE + 1;

/// RTP sender — owns mutable sequence/timestamp state. One per stream.
pub struct RtpSender {
    ssrc: u32,
    seq: u16,
    timestamp: u32,
    socket: Arc<UdpSocket>,
}

/// RTP receiver — immutable, shares the UDP socket with the sender.
pub struct RtpReceiver {
    socket: Arc<UdpSocket>,
}

/// Create a paired RTP sender and receiver sharing the same UDP socket.
pub fn create_rtp_pair(socket: UdpSocket, ssrc: u32) -> (RtpSender, RtpReceiver) {
    let socket = Arc::new(socket);
    let sender = RtpSender {
        ssrc,
        seq: 0,
        timestamp: 0,
        socket: Arc::clone(&socket),
    };
    let receiver = RtpReceiver { socket };
    (sender, receiver)
}

impl RtpSender {
    /// Build an RTP packet with PCMU payload and send it to the remote addr.
    pub async fn send_packet(
        &mut self,
        payload: &[u8],
        remote: SocketAddr,
    ) -> std::io::Result<usize> {
        assert!(!payload.is_empty(), "RTP payload must not be empty");
        assert!(
            payload.len() <= MAX_RTP_PACKET_SIZE - RTP_HEADER_SIZE,
            "RTP payload {} bytes exceeds maximum {}",
            payload.len(),
            MAX_RTP_PACKET_SIZE - RTP_HEADER_SIZE
        );

        let packet = self.build_packet(payload);
        self.socket.send_to(&packet, remote).await
    }

    /// Build an RTP packet, advancing sequence number and timestamp.
    fn build_packet(&mut self, payload: &[u8]) -> Vec<u8> {
        let mut packet = Vec::with_capacity(RTP_HEADER_SIZE + payload.len());

        // Byte 0: V=2, P=0, X=0, CC=0 → 0x80
        packet.push(0x80);
        // Byte 1: M=0, PT=0 (PCMU)
        packet.push(PT_PCMU);
        // Bytes 2-3: sequence number (big-endian)
        packet.extend_from_slice(&self.seq.to_be_bytes());
        // Bytes 4-7: timestamp (big-endian)
        packet.extend_from_slice(&self.timestamp.to_be_bytes());
        // Bytes 8-11: SSRC (big-endian)
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        // Payload
        packet.extend_from_slice(payload);

        // Advance for next packet. Wrapping is correct per RFC 3550 §5.1.
        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(TIMESTAMP_INCREMENT);

        packet
    }
}

impl RtpReceiver {
    /// Receive an RTP packet and return the payload bytes and sender address.
    /// Returns None if the packet is too small or has wrong version.
    pub async fn recv_packet(&self) -> std::io::Result<Option<(Vec<u8>, SocketAddr)>> {
        let mut buf = [0u8; MAX_RTP_PACKET_SIZE];
        let (len, addr) = self.socket.recv_from(&mut buf).await?;

        match parse_packet(&buf[..len]) {
            Some(payload) => Ok(Some((payload.to_vec(), addr))),
            None => Ok(None),
        }
    }
}

/// Parse an RTP packet, returning the payload slice if valid.
/// Rejects packets with wrong version or too short to contain a header.
pub fn parse_packet(data: &[u8]) -> Option<&[u8]> {
    if data.len() < MIN_RTP_PACKET_SIZE {
        return None;
    }

    // Check RTP version = 2 (top 2 bits of first byte)
    let version = (data[0] >> 6) & 0x03;
    if version != 2 {
        return None;
    }

    // CSRC count is bottom 4 bits of first byte
    let cc = (data[0] & 0x0F) as usize;
    let header_len = RTP_HEADER_SIZE + cc * 4;

    // Check extension bit (bit 4 of first byte)
    let has_extension = (data[0] & 0x10) != 0;

    if data.len() < header_len {
        return None;
    }

    let mut offset = header_len;

    // If extension header present, skip it
    if has_extension {
        // Extension header: 2 bytes profile, 2 bytes length (in 32-bit words)
        if data.len() < offset + 4 {
            return None;
        }
        let ext_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
        offset += 4 + ext_len * 4;
    }

    if offset >= data.len() {
        return None;
    }

    Some(&data[offset..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sender() -> RtpSender {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let socket = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { UdpSocket::from_std(socket).unwrap() });

        let (sender, _receiver) = create_rtp_pair(socket, 0xDEADBEEF);
        sender
    }

    #[test]
    fn build_packet_header_layout() {
        let mut sender = make_sender();
        let payload = [0xFF; 160]; // 160 bytes = 20ms of PCMU
        let packet = sender.build_packet(&payload);

        assert_eq!(packet.len(), RTP_HEADER_SIZE + 160);
        // V=2, P=0, X=0, CC=0
        assert_eq!(packet[0], 0x80);
        // M=0, PT=0
        assert_eq!(packet[1], PT_PCMU);
        // Seq = 0 (first packet)
        assert_eq!(&packet[2..4], &0u16.to_be_bytes());
        // Timestamp = 0 (first packet)
        assert_eq!(&packet[4..8], &0u32.to_be_bytes());
        // SSRC
        assert_eq!(&packet[8..12], &0xDEADBEEFu32.to_be_bytes());
        // Payload starts at offset 12
        assert_eq!(&packet[12..], &payload[..]);
    }

    #[test]
    fn seq_wraps_at_u16_max() {
        let mut sender = make_sender();
        sender.seq = u16::MAX;

        let packet = sender.build_packet(&[0xFF]);
        // Sequence in packet should be u16::MAX
        let seq_in_packet = u16::from_be_bytes([packet[2], packet[3]]);
        assert_eq!(seq_in_packet, u16::MAX);
        // After building, seq should have wrapped to 0
        assert_eq!(sender.seq, 0);
    }

    #[test]
    fn timestamp_increments_by_160() {
        let mut sender = make_sender();

        let p1 = sender.build_packet(&[0xFF]);
        let ts1 = u32::from_be_bytes([p1[4], p1[5], p1[6], p1[7]]);
        assert_eq!(ts1, 0);

        let p2 = sender.build_packet(&[0xFF]);
        let ts2 = u32::from_be_bytes([p2[4], p2[5], p2[6], p2[7]]);
        assert_eq!(ts2, 160);

        let p3 = sender.build_packet(&[0xFF]);
        let ts3 = u32::from_be_bytes([p3[4], p3[5], p3[6], p3[7]]);
        assert_eq!(ts3, 320);
    }

    #[test]
    fn timestamp_wraps_at_u32_max() {
        let mut sender = make_sender();
        sender.timestamp = u32::MAX - 100;

        let _ = sender.build_packet(&[0xFF]);
        // Should wrap: (u32::MAX - 100) + 160 = 59
        assert_eq!(sender.timestamp, 59);
    }

    #[test]
    fn parse_valid_packet() {
        let mut packet = vec![0x80, PT_PCMU, 0x00, 0x01]; // V=2, seq=1
        packet.extend_from_slice(&100u32.to_be_bytes()); // timestamp
        packet.extend_from_slice(&42u32.to_be_bytes()); // SSRC
        packet.extend_from_slice(&[0xAA, 0xBB, 0xCC]); // payload

        let payload = parse_packet(&packet).expect("valid packet should parse");
        assert_eq!(payload, &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn parse_rejects_too_short() {
        // 12-byte header with no payload
        let packet = vec![0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(parse_packet(&packet).is_none());
    }

    #[test]
    fn parse_rejects_wrong_version() {
        // Version 1 instead of 2
        let mut packet = vec![0x40, PT_PCMU, 0x00, 0x01];
        packet.extend_from_slice(&0u32.to_be_bytes());
        packet.extend_from_slice(&0u32.to_be_bytes());
        packet.push(0xFF);

        assert!(parse_packet(&packet).is_none());
    }

    #[test]
    fn parse_handles_csrc() {
        // CC=1 (one CSRC entry = 4 extra bytes in header)
        let mut packet = vec![0x81, PT_PCMU, 0x00, 0x01];
        packet.extend_from_slice(&0u32.to_be_bytes()); // timestamp
        packet.extend_from_slice(&0u32.to_be_bytes()); // SSRC
        packet.extend_from_slice(&0u32.to_be_bytes()); // CSRC[0]
        packet.extend_from_slice(&[0xDD, 0xEE]); // payload

        let payload = parse_packet(&packet).expect("packet with CSRC should parse");
        assert_eq!(payload, &[0xDD, 0xEE]);
    }
}
