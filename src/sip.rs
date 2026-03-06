//! Minimal SIP User Agent for placing outbound calls via a SIP trunk.
//!
//! Implements REGISTER, INVITE, ACK, BYE with RFC 2617 digest authentication.
//! Hand-rolled transaction logic — no external SIP stack. Uses `rsip` for
//! message parsing/building only.
//!
//! Designed for a single concurrent call to a credential-based SIP provider
//! (Telnyx). No call routing, no inbound handling, no SRTP.

use anyhow::{bail, Context, Result};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

/// Maximum SIP message size. RFC 3261 §18.1.1 says implementations MUST
/// handle messages up to the path MTU, and SHOULD handle up to 65535 bytes.
/// 8KB is generous for our simple messages.
const MAX_SIP_MSG_SIZE: usize = 8192;

/// INVITE retransmission Timer A initial value per RFC 3261 §17.1.1.2.
/// For unreliable transports (UDP), the client retransmits the INVITE at
/// intervals of T1, 2*T1, 4*T1, ... until a response or timeout.
const TIMER_A_INITIAL_MS: u64 = 500;

/// Maximum INVITE retransmission interval (cap on exponential backoff).
const TIMER_A_CAP_MS: u64 = 4000;

/// Maximum number of INVITE retransmissions before giving up. 7 retransmits
/// at 500ms doubling = ~32 seconds, beyond which most SIP proxies would
/// have responded or timed out.
const INVITE_RETRANSMIT_LIMIT: u32 = 7;

/// Maximum time to wait for a final response to REGISTER.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);

/// SIP transport (UDP) receive timeout for individual recv_from calls.
const RECV_TIMEOUT: Duration = Duration::from_millis(500);

/// Maximum iterations in any SIP response-wait loop. Prevents unbounded
/// spinning if the remote sends endless provisional responses.
const RESPONSE_LOOP_LIMIT: u32 = 1000;

/// SIP call states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    Trying,
    EarlyMedia,
    Answered,
}

/// Active SIP call state.
#[derive(Debug)]
pub struct Call {
    pub call_id: String,
    pub cseq: u32,
    pub local_tag: String,
    pub remote_tag: Option<String>,
    pub remote_rtp: Option<SocketAddr>,
    pub state: CallState,
    /// The INVITE request URI, needed for ACK.
    invite_uri: String,
    /// Via branch for transaction matching.
    via_branch: String,
}

/// SIP client for outbound calls through a SIP trunk.
pub struct SipClient {
    pub local_addr: SocketAddr,
    sip_server: SocketAddr,
    sip_domain: String,
    username: String,
    password: String,
    caller_id: String,
    caller_name: String,
    socket: UdpSocket,
    local_ip: IpAddr,
}

impl SipClient {
    pub async fn new(
        local_addr: SocketAddr,
        sip_server: SocketAddr,
        sip_domain: String,
        username: String,
        password: String,
        caller_id: String,
        caller_name: String,
    ) -> Result<Self> {
        assert!(!username.is_empty(), "SIP username must not be empty");
        assert!(!password.is_empty(), "SIP password must not be empty");
        assert!(!sip_domain.is_empty(), "SIP domain must not be empty");

        let socket = UdpSocket::bind(local_addr)
            .await
            .with_context(|| format!("failed to bind SIP UDP socket on {local_addr}"))?;

        let bound_addr = socket.local_addr()?;
        let local_ip = if bound_addr.ip().is_unspecified() {
            // When bound to 0.0.0.0, use a routable IP for SIP headers.
            // Connect (without sending) to the SIP server to determine
            // which local interface would be used.
            let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
            probe.connect(sip_server)?;
            probe.local_addr()?.ip()
        } else {
            bound_addr.ip()
        };

        info!(
            local = %bound_addr,
            local_ip = %local_ip,
            server = %sip_server,
            domain = %sip_domain,
            "event=sip_client_created"
        );

        Ok(Self {
            local_addr: bound_addr,
            sip_server,
            sip_domain,
            username,
            password,
            caller_id,
            caller_name,
            socket,
            local_ip,
        })
    }

    /// Send REGISTER to the SIP server with digest auth (handles 401 challenge).
    pub async fn register(&self) -> Result<()> {
        let call_id = generate_call_id();
        let tag = generate_tag();
        let branch = generate_branch();

        let register = format!(
            "REGISTER sip:{domain} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {local_ip}:{local_port};branch={branch};rport\r\n\
             From: <sip:{user}@{domain}>;tag={tag}\r\n\
             To: <sip:{user}@{domain}>\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 1 REGISTER\r\n\
             Contact: <sip:{user}@{local_ip}:{local_port}>\r\n\
             Expires: 180\r\n\
             Max-Forwards: 70\r\n\
             User-Agent: slmodem-sip-bridge\r\n\
             Content-Length: 0\r\n\
             \r\n",
            domain = self.sip_domain,
            local_ip = self.local_ip,
            local_port = self.local_addr.port(),
            user = self.username,
            branch = branch,
            tag = tag,
            call_id = call_id,
        );

        self.send_msg(&register).await?;
        info!("event=register_sent");

        // Wait for response — expect 401 Unauthorized with WWW-Authenticate
        let resp = self.recv_response_timeout(REGISTER_TIMEOUT).await?;
        let status = parse_status_code(&resp);

        if status == Some(200) {
            info!("event=register_success, reason=no auth challenge");
            return Ok(());
        }

        if status != Some(401) {
            bail!(
                "REGISTER failed with status {}, expected 200 or 401",
                status.unwrap_or(0)
            );
        }

        // Parse WWW-Authenticate and build authenticated REGISTER
        let auth_header = extract_header(&resp, "WWW-Authenticate")
            .ok_or_else(|| anyhow::anyhow!("401 response missing WWW-Authenticate header"))?;
        debug!(www_authenticate = %auth_header, "event=register_401_challenge");

        let digest = parse_digest_challenge(&auth_header)?;
        let register_uri = format!("sip:{}", self.sip_domain);
        let auth_value = digest.auth_header_value(
            &self.username,
            &self.password,
            "REGISTER",
            &register_uri,
        );

        let branch2 = generate_branch();
        let auth_register = format!(
            "REGISTER sip:{domain} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {local_ip}:{local_port};branch={branch};rport\r\n\
             From: <sip:{user}@{domain}>;tag={tag}\r\n\
             To: <sip:{user}@{domain}>\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 2 REGISTER\r\n\
             Contact: <sip:{user}@{local_ip}:{local_port}>\r\n\
             Expires: 180\r\n\
             Max-Forwards: 70\r\n\
             User-Agent: slmodem-sip-bridge\r\n\
             Authorization: {auth_value}\r\n\
             Content-Length: 0\r\n\
             \r\n",
            domain = self.sip_domain,
            local_ip = self.local_ip,
            local_port = self.local_addr.port(),
            user = self.username,
            branch = branch2,
            tag = tag,
            call_id = call_id,
        );

        self.send_msg(&auth_register).await?;
        info!("event=register_auth_sent");

        // Read responses, skipping stale retransmissions from CSeq 1.
        let deadline = tokio::time::Instant::now() + REGISTER_TIMEOUT;
        loop {
            if tokio::time::Instant::now() >= deadline {
                bail!("authenticated REGISTER timed out");
            }

            let resp2 = match self.try_recv_response().await {
                Some(r) => r,
                None => continue,
            };

            // Skip retransmitted responses from the first REGISTER (CSeq 1)
            if let Some(resp_cseq) = extract_cseq_num(&resp2) {
                if resp_cseq != 2 {
                    debug!(
                        resp_cseq,
                        "event=ignoring_stale_register_response, reason=CSeq mismatch"
                    );
                    continue;
                }
            }

            let status2 = parse_status_code(&resp2);
            if status2 == Some(200) {
                info!("event=register_success");
                return Ok(());
            } else {
                debug!(response = %resp2, "event=register_auth_rejected");
                bail!(
                    "authenticated REGISTER failed with status {}",
                    status2.unwrap_or(0)
                );
            }
        }
    }

    /// Send INVITE to place an outbound call. Returns Call state which is
    /// updated as responses arrive via `process_invite_responses`.
    pub async fn invite(
        &self,
        dial: &str,
        sdp_offer: &str,
        invite_timeout: Duration,
    ) -> Result<Call> {
        assert!(!dial.is_empty(), "dial string must not be empty");

        let call_id = generate_call_id();
        let local_tag = generate_tag();
        let branch = generate_branch();
        let invite_uri = format!("sip:{}@{}", dial, self.sip_domain);

        let invite = self.build_invite(
            &invite_uri,
            &call_id,
            &local_tag,
            &branch,
            1,
            sdp_offer,
            None,
        );

        self.send_msg(&invite).await?;
        info!(dial, "event=invite_sent");

        let mut call = Call {
            call_id: call_id.clone(),
            cseq: 1,
            local_tag,
            remote_tag: None,
            remote_rtp: None,
            state: CallState::Trying,
            invite_uri: invite_uri.clone(),
            via_branch: branch.clone(),
        };

        // INVITE response processing with retransmission (RFC 3261 §17.1.1.2)
        let deadline = tokio::time::Instant::now() + invite_timeout;
        let mut retransmit_timer = Duration::from_millis(TIMER_A_INITIAL_MS);
        let mut last_send = tokio::time::Instant::now();
        let mut retransmit_count: u32 = 0;
        let mut iterations: u32 = 0;
        let mut got_provisional = false;
        let mut auth_retry_done = false;

        loop {
            assert!(
                iterations < RESPONSE_LOOP_LIMIT,
                "INVITE response loop exceeded {RESPONSE_LOOP_LIMIT} iterations"
            );
            iterations += 1;

            if tokio::time::Instant::now() >= deadline {
                bail!("INVITE timed out after {:?}", invite_timeout);
            }

            // Retransmit INVITE if no provisional received and timer expired.
            // Per RFC 3261: stop retransmitting once a provisional (1xx) arrives.
            if !got_provisional
                && retransmit_count < INVITE_RETRANSMIT_LIMIT
                && last_send.elapsed() >= retransmit_timer
            {
                debug!(
                    retransmit_count,
                    timer_ms = retransmit_timer.as_millis(),
                    "event=invite_retransmit"
                );
                self.send_msg(&invite).await?;
                retransmit_count += 1;
                last_send = tokio::time::Instant::now();
                retransmit_timer = retransmit_timer
                    .saturating_mul(2)
                    .min(Duration::from_millis(TIMER_A_CAP_MS));
            }

            let resp = match self.try_recv_response().await {
                Some(r) => r,
                None => continue,
            };

            let status = match parse_status_code(&resp) {
                Some(s) => s,
                None => continue,
            };

            // Filter by CSeq to ignore retransmitted responses from previous
            // transactions (e.g. a stale 407 from the first INVITE arriving
            // after we've already sent an authenticated re-INVITE with CSeq+1).
            if let Some(resp_cseq) = extract_cseq_num(&resp) {
                if resp_cseq != call.cseq {
                    debug!(
                        resp_cseq,
                        expected_cseq = call.cseq,
                        status,
                        "event=ignoring_stale_response, reason=CSeq mismatch"
                    );
                    continue;
                }
            }

            // Extract remote tag from To header if present
            if call.remote_tag.is_none() {
                if let Some(to) = extract_header(&resp, "To") {
                    if let Some(tag) = extract_tag_param(&to) {
                        call.remote_tag = Some(tag);
                    }
                }
            }

            match status {
                100 => {
                    got_provisional = true;
                    debug!("event=sip_100_trying");
                }
                180 | 183 => {
                    got_provisional = true;
                    // Early media: extract SDP if present for RTP relay
                    if let Some(sdp) = extract_sdp_body(&resp) {
                        if let Ok(answer) = crate::sdp::parse_answer(&sdp) {
                            let rtp_addr = SocketAddr::new(answer.addr, answer.port);
                            call.remote_rtp = Some(rtp_addr);
                            call.state = CallState::EarlyMedia;
                            info!(
                                status,
                                remote_rtp = %rtp_addr,
                                "event=early_media, reason=remote SDP received in provisional"
                            );
                            // Return immediately — we have everything needed to
                            // start relaying audio. The 200 OK and ACK are
                            // handled by wait_for_answer() during the relay.
                            return Ok(call);
                        }
                    } else {
                        debug!(status, "event=provisional_no_sdp");
                    }
                }
                200 => {
                    // Extract SDP from 200 OK
                    if let Some(sdp) = extract_sdp_body(&resp) {
                        if let Ok(answer) = crate::sdp::parse_answer(&sdp) {
                            let rtp_addr = SocketAddr::new(answer.addr, answer.port);
                            call.remote_rtp = Some(rtp_addr);
                        }
                    }
                    call.state = CallState::Answered;
                    info!("event=invite_200_ok");
                    return Ok(call);
                }
                401 | 407 => {
                    if auth_retry_done {
                        bail!("INVITE authentication failed after retry (status {status})");
                    }
                    auth_retry_done = true;

                    let auth_hdr_name = if status == 401 {
                        "WWW-Authenticate"
                    } else {
                        "Proxy-Authenticate"
                    };
                    let auth_resp_name = if status == 401 {
                        "Authorization"
                    } else {
                        "Proxy-Authorization"
                    };

                    let auth_header = extract_header(&resp, auth_hdr_name)
                        .ok_or_else(|| anyhow::anyhow!("{status} missing {auth_hdr_name}"))?;
                    debug!(challenge = %auth_header, status, "event=invite_auth_challenge");
                    let digest = parse_digest_challenge(&auth_header)?;
                    let auth_value = digest.auth_header_value(
                        &self.username,
                        &self.password,
                        "INVITE",
                        &invite_uri,
                    );

                    // Send ACK for the 401/407 (required per RFC 3261 §17.1.1.3)
                    let ack = self.build_ack(&call);
                    self.send_msg(&ack).await?;

                    // Re-INVITE with auth
                    call.cseq += 1;
                    let new_branch = generate_branch();
                    call.via_branch = new_branch.clone();

                    let auth_invite = self.build_invite(
                        &invite_uri,
                        &call_id,
                        &call.local_tag,
                        &new_branch,
                        call.cseq,
                        sdp_offer,
                        Some((auth_resp_name, &auth_value)),
                    );

                    self.send_msg(&auth_invite).await?;
                    info!("event=invite_auth_retry");

                    // Reset retransmission state for the new transaction
                    retransmit_count = 0;
                    retransmit_timer = Duration::from_millis(TIMER_A_INITIAL_MS);
                    last_send = tokio::time::Instant::now();
                    got_provisional = false;
                }
                s if (400..700).contains(&s) => {
                    // Send ACK for error responses (required)
                    let ack = self.build_ack(&call);
                    self.send_msg(&ack).await?;
                    bail!("INVITE rejected with status {s}");
                }
                s => {
                    debug!(status = s, "event=unexpected_sip_response");
                }
            }
        }
    }

    /// Poll for 200 OK on a call in EarlyMedia state. Returns true when
    /// answered (ACK is sent automatically), false if nothing yet.
    /// Non-blocking: returns immediately if no SIP response is available.
    pub async fn poll_answer(&self, call: &mut Call) -> Result<bool> {
        assert!(
            call.state == CallState::EarlyMedia,
            "poll_answer called in wrong state: {:?}",
            call.state
        );

        let resp = match self.try_recv_response().await {
            Some(r) => r,
            None => return Ok(false),
        };

        let status = match parse_status_code(&resp) {
            Some(s) => s,
            None => return Ok(false),
        };

        // Ignore retransmitted provisionals
        if status >= 100 && status < 200 {
            return Ok(false);
        }

        if status == 200 {
            // Update remote RTP if 200 OK carries SDP (may differ from 183)
            if let Some(sdp) = extract_sdp_body(&resp) {
                if let Ok(answer) = crate::sdp::parse_answer(&sdp) {
                    let rtp_addr = SocketAddr::new(answer.addr, answer.port);
                    call.remote_rtp = Some(rtp_addr);
                }
            }
            // Extract remote tag if not yet set
            if call.remote_tag.is_none() {
                if let Some(to) = extract_header(&resp, "To") {
                    if let Some(tag) = extract_tag_param(&to) {
                        call.remote_tag = Some(tag);
                    }
                }
            }
            call.state = CallState::Answered;
            info!("event=invite_200_ok");
            self.ack(call).await?;
            return Ok(true);
        }

        // Any non-2xx final response during early media means call failed
        warn!(status, "event=call_failed_during_early_media");
        bail!("call failed with status {status} during early media");
    }

    /// Send ACK for a 200 OK response (or error response).
    pub async fn ack(&self, call: &Call) -> Result<()> {
        let ack = self.build_ack(call);
        self.send_msg(&ack).await?;
        info!(call_id = %call.call_id, "event=ack_sent");
        Ok(())
    }

    /// Send BYE to end the call.
    pub async fn bye(&self, call: &Call) -> Result<()> {
        let branch = generate_branch();
        let cseq = call.cseq + 1;

        let to_tag = call
            .remote_tag
            .as_deref()
            .map(|t| format!(";tag={t}"))
            .unwrap_or_default();

        let bye = format!(
            "BYE {uri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {local_ip}:{local_port};branch={branch};rport\r\n\
             From: \"{caller_name}\" <sip:{caller_id}@{domain}>;tag={local_tag}\r\n\
             To: <sip:{uri_stripped}>{to_tag}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} BYE\r\n\
             Max-Forwards: 70\r\n\
             User-Agent: slmodem-sip-bridge\r\n\
             Content-Length: 0\r\n\
             \r\n",
            uri = call.invite_uri,
            local_ip = self.local_ip,
            local_port = self.local_addr.port(),
            branch = branch,
            caller_name = self.caller_name,
            caller_id = self.caller_id,
            domain = self.sip_domain,
            local_tag = call.local_tag,
            uri_stripped = call.invite_uri.trim_start_matches("sip:"),
            to_tag = to_tag,
            call_id = call.call_id,
            cseq = cseq,
        );

        self.send_msg(&bye).await?;
        info!(call_id = %call.call_id, "event=bye_sent");

        // Wait briefly for 200 OK to BYE, but don't fail if it doesn't come.
        // The call is being torn down regardless.
        match tokio::time::timeout(Duration::from_secs(3), self.recv_response_timeout(Duration::from_secs(3))).await {
            Ok(Ok(resp)) => {
                let status = parse_status_code(&resp);
                debug!(status, "event=bye_response");
            }
            _ => {
                debug!("event=bye_no_response, reason=timed out waiting for BYE response");
            }
        }

        Ok(())
    }

    fn build_invite(
        &self,
        uri: &str,
        call_id: &str,
        local_tag: &str,
        branch: &str,
        cseq: u32,
        sdp: &str,
        auth: Option<(&str, &str)>,
    ) -> String {
        let auth_line = match auth {
            Some((name, value)) => format!("{name}: {value}\r\n"),
            None => String::new(),
        };

        format!(
            "INVITE {uri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {local_ip}:{local_port};branch={branch};rport\r\n\
             From: \"{caller_name}\" <sip:{caller_id}@{domain}>;tag={local_tag}\r\n\
             To: <{uri}>\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} INVITE\r\n\
             Contact: <sip:{user}@{local_ip}:{local_port}>\r\n\
             Max-Forwards: 70\r\n\
             User-Agent: slmodem-sip-bridge\r\n\
             Allow: INVITE, ACK, BYE, CANCEL\r\n\
             {auth_line}\
             Content-Type: application/sdp\r\n\
             Content-Length: {content_len}\r\n\
             \r\n\
             {sdp}",
            uri = uri,
            local_ip = self.local_ip,
            local_port = self.local_addr.port(),
            branch = branch,
            caller_name = self.caller_name,
            caller_id = self.caller_id,
            domain = self.sip_domain,
            local_tag = local_tag,
            call_id = call_id,
            cseq = cseq,
            user = self.username,
            auth_line = auth_line,
            content_len = sdp.len(),
            sdp = sdp,
        )
    }

    fn build_ack(&self, call: &Call) -> String {
        let to_tag = call
            .remote_tag
            .as_deref()
            .map(|t| format!(";tag={t}"))
            .unwrap_or_default();

        format!(
            "ACK {uri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {local_ip}:{local_port};branch={branch};rport\r\n\
             From: \"{caller_name}\" <sip:{caller_id}@{domain}>;tag={local_tag}\r\n\
             To: <{uri}>{to_tag}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} ACK\r\n\
             Max-Forwards: 70\r\n\
             User-Agent: slmodem-sip-bridge\r\n\
             Content-Length: 0\r\n\
             \r\n",
            uri = call.invite_uri,
            local_ip = self.local_ip,
            local_port = self.local_addr.port(),
            branch = call.via_branch,
            caller_name = self.caller_name,
            caller_id = self.caller_id,
            domain = self.sip_domain,
            local_tag = call.local_tag,
            to_tag = to_tag,
            call_id = call.call_id,
            cseq = call.cseq,
        )
    }

    async fn send_msg(&self, msg: &str) -> Result<()> {
        self.socket
            .send_to(msg.as_bytes(), self.sip_server)
            .await
            .context("failed to send SIP message")?;
        Ok(())
    }

    async fn recv_response_timeout(&self, timeout: Duration) -> Result<String> {
        let mut buf = [0u8; MAX_SIP_MSG_SIZE];
        let (len, _addr) = tokio::time::timeout(timeout, self.socket.recv_from(&mut buf))
            .await
            .context("SIP response timeout")?
            .context("SIP socket recv error")?;
        Ok(String::from_utf8_lossy(&buf[..len]).into_owned())
    }

    async fn try_recv_response(&self) -> Option<String> {
        let mut buf = [0u8; MAX_SIP_MSG_SIZE];
        match tokio::time::timeout(RECV_TIMEOUT, self.socket.recv_from(&mut buf)).await {
            Ok(Ok((len, _addr))) => Some(String::from_utf8_lossy(&buf[..len]).into_owned()),
            _ => None,
        }
    }

    /// The local IP that will be used in SDP and SIP headers.
    pub fn local_ip(&self) -> IpAddr {
        self.local_ip
    }
}

// --- Digest Authentication (RFC 2617) ---

struct DigestChallenge {
    realm: String,
    nonce: String,
    opaque: Option<String>,
    qop: Option<String>,
}

impl DigestChallenge {
    /// Build the full Authorization header value for this challenge.
    fn auth_header_value(
        &self,
        username: &str,
        password: &str,
        method: &str,
        uri: &str,
    ) -> String {
        let digest_resp = compute_digest_response(
            username,
            password,
            &self.realm,
            &self.nonce,
            method,
            uri,
            self.qop.as_deref(),
        );

        let mut header = format!(
            "Digest username=\"{username}\", realm=\"{realm}\", nonce=\"{nonce}\", \
             uri=\"{uri}\", response=\"{resp}\", algorithm=MD5",
            realm = self.realm,
            nonce = self.nonce,
            resp = digest_resp.response,
        );

        if let Some(ref opaque) = self.opaque {
            header.push_str(&format!(", opaque=\"{opaque}\""));
        }

        if self.qop.is_some() {
            header.push_str(&format!(
                ", qop=auth, nc={NC}, cnonce=\"{cnonce}\"",
                cnonce = digest_resp.cnonce,
            ));
        }

        header
    }
}

/// Nonce count — always 1 since we get a fresh nonce per challenge.
const NC: &str = "00000001";

fn parse_digest_challenge(header: &str) -> Result<DigestChallenge> {
    let realm = extract_quoted_param(header, "realm")
        .ok_or_else(|| anyhow::anyhow!("digest challenge missing realm"))?;
    let nonce = extract_quoted_param(header, "nonce")
        .ok_or_else(|| anyhow::anyhow!("digest challenge missing nonce"))?;
    let opaque = extract_quoted_param(header, "opaque");
    let qop = extract_quoted_param(header, "qop");
    Ok(DigestChallenge { realm, nonce, opaque, qop })
}

/// Result of a digest response computation, including cnonce when qop is used.
#[derive(Debug)]
struct DigestResponse {
    response: String,
    cnonce: String,
}

impl std::fmt::Display for DigestResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.response)
    }
}

/// Compute MD5 digest response per RFC 2617 §3.2.2.
/// When qop is Some("auth"), uses the extended formula with cnonce and nc.
fn compute_digest_response(
    username: &str,
    password: &str,
    realm: &str,
    nonce: &str,
    method: &str,
    uri: &str,
    qop: Option<&str>,
) -> DigestResponse {
    let ha1 = md5_hex(&format!("{username}:{realm}:{password}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    let cnonce = generate_cnonce();
    let response = match qop {
        Some("auth") => {
            md5_hex(&format!("{ha1}:{nonce}:{NC}:{cnonce}:auth:{ha2}"))
        }
        _ => md5_hex(&format!("{ha1}:{nonce}:{ha2}")),
    };
    DigestResponse { response, cnonce }
}

fn generate_cnonce() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}", md5::compute(ts.to_le_bytes()))
}

fn md5_hex(input: &str) -> String {
    format!("{:x}", md5::compute(input.as_bytes()))
}

// --- SIP Message Parsing Helpers ---

fn parse_status_code(msg: &str) -> Option<u16> {
    // SIP response first line: "SIP/2.0 200 OK"
    let first_line = msg.lines().next()?;
    if !first_line.starts_with("SIP/2.0 ") {
        return None;
    }
    let parts: Vec<&str> = first_line.splitn(3, ' ').collect();
    if parts.len() < 2 {
        return None;
    }
    parts[1].parse().ok()
}

fn extract_header<'a>(msg: &'a str, name: &str) -> Option<String> {
    let prefix = format!("{}:", name);
    for line in msg.lines() {
        let trimmed = line.trim();
        if trimmed.len() > prefix.len()
            && trimmed[..prefix.len()].eq_ignore_ascii_case(&prefix)
        {
            return Some(trimmed[prefix.len()..].trim().to_string());
        }
    }
    None
}

/// Extract the numeric CSeq value from a SIP response.
/// CSeq header format: "CSeq: 1 INVITE"
fn extract_cseq_num(msg: &str) -> Option<u32> {
    let cseq_value = extract_header(msg, "CSeq")?;
    let num_str = cseq_value.split_whitespace().next()?;
    num_str.parse().ok()
}

fn extract_quoted_param(header: &str, param: &str) -> Option<String> {
    let search = format!("{param}=\"");
    let start = header.find(&search)? + search.len();
    let end = header[start..].find('"')? + start;
    Some(header[start..end].to_string())
}

fn extract_tag_param(header: &str) -> Option<String> {
    let tag_prefix = "tag=";
    let start = header.find(tag_prefix)? + tag_prefix.len();
    // Tag ends at ';', '>', or end of string
    let rest = &header[start..];
    let end = rest
        .find(|c: char| c == ';' || c == '>' || c == ',' || c.is_whitespace())
        .unwrap_or(rest.len());
    let tag = rest[..end].trim();
    if tag.is_empty() {
        None
    } else {
        Some(tag.to_string())
    }
}

fn extract_sdp_body(msg: &str) -> Option<String> {
    // SDP body follows the blank line after SIP headers.
    // Check Content-Type is application/sdp first.
    let has_sdp_content_type = msg
        .lines()
        .any(|l| {
            let t = l.trim().to_ascii_lowercase();
            t.starts_with("content-type:") && t.contains("application/sdp")
        });

    if !has_sdp_content_type {
        return None;
    }

    // Find the blank line separating headers from body.
    // SIP uses \r\n\r\n but be lenient with \n\n too.
    let body_start = if let Some(pos) = msg.find("\r\n\r\n") {
        pos + 4
    } else if let Some(pos) = msg.find("\n\n") {
        pos + 2
    } else {
        return None;
    };

    let body = &msg[body_start..];
    if body.trim().is_empty() {
        None
    } else {
        Some(body.to_string())
    }
}

// --- ID Generation ---

fn generate_call_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    format!("{}-{}", now, std::process::id())
}

fn generate_tag() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let s = RandomState::new();
    let mut h = s.build_hasher();
    h.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64,
    );
    format!("{:x}", h.finish())
}

fn generate_branch() -> String {
    // RFC 3261 §8.1.1.7: branch must start with "z9hG4bK"
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let s = RandomState::new();
    let mut h = s.build_hasher();
    h.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64,
    );
    format!("z9hG4bK{:x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 2617 §3.5 test vector (without qop — our implementation)
    #[test]
    fn digest_auth_rfc2617_calculation() {
        // HA1 = MD5("Mufasa:testrealm@host.com:Circle Of Life")
        //      = "939e7578ed9e3c518a452acee763bce9"
        // HA2 = MD5("GET:/dir/index.html")
        //      = "39aff3a2bab6126f332b942af5e6afc3"
        // response = MD5(HA1:nonce:HA2)  (no qop)
        let result = compute_digest_response(
            "Mufasa",
            "Circle Of Life",
            "testrealm@host.com",
            "dcd98b7102dd2f0e8b11d0f600bfb0c093",
            "GET",
            "/dir/index.html",
            None,
        );
        // Verify HA1 and HA2 individually to confirm algorithm structure
        let ha1 = format!("{:x}", md5::compute("Mufasa:testrealm@host.com:Circle Of Life"));
        let ha2 = format!("{:x}", md5::compute("GET:/dir/index.html"));
        let expected = format!("{:x}", md5::compute(format!("{ha1}:dcd98b7102dd2f0e8b11d0f600bfb0c093:{ha2}")));
        assert_eq!(result.response, expected);
    }

    #[test]
    fn parse_status_code_200() {
        let msg = "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 10.0.0.1\r\n\r\n";
        assert_eq!(parse_status_code(msg), Some(200));
    }

    #[test]
    fn parse_status_code_401() {
        let msg = "SIP/2.0 401 Unauthorized\r\n\r\n";
        assert_eq!(parse_status_code(msg), Some(401));
    }

    #[test]
    fn parse_status_code_100() {
        let msg = "SIP/2.0 100 Trying\r\n\r\n";
        assert_eq!(parse_status_code(msg), Some(100));
    }

    #[test]
    fn parse_status_code_not_sip() {
        assert_eq!(parse_status_code("HTTP/1.1 200 OK\r\n\r\n"), None);
    }

    #[test]
    fn extract_header_case_insensitive() {
        let msg = "SIP/2.0 401 Unauthorized\r\n\
                   www-authenticate: Digest realm=\"test\", nonce=\"abc\"\r\n\r\n";
        let val = extract_header(msg, "WWW-Authenticate");
        assert!(val.is_some());
        assert!(val.unwrap().contains("Digest"));
    }

    #[test]
    fn extract_cseq_num_from_response() {
        let msg = "SIP/2.0 407 Proxy Authentication Required\r\n\
                   CSeq: 1 INVITE\r\n\r\n";
        assert_eq!(extract_cseq_num(msg), Some(1));

        let msg2 = "SIP/2.0 200 OK\r\n\
                    CSeq: 2 INVITE\r\n\r\n";
        assert_eq!(extract_cseq_num(msg2), Some(2));
    }

    #[test]
    fn extract_quoted_param_realm() {
        let header = "Digest realm=\"telnyx.com\", nonce=\"abc123\"";
        assert_eq!(extract_quoted_param(header, "realm"), Some("telnyx.com".to_string()));
        assert_eq!(extract_quoted_param(header, "nonce"), Some("abc123".to_string()));
    }

    #[test]
    fn extract_tag_from_to_header() {
        let header = "<sip:user@domain.com>;tag=abc123";
        assert_eq!(extract_tag_param(header), Some("abc123".to_string()));
    }

    #[test]
    fn extract_tag_missing() {
        let header = "<sip:user@domain.com>";
        assert_eq!(extract_tag_param(header), None);
    }

    #[test]
    fn extract_sdp_body_present() {
        let msg = "SIP/2.0 200 OK\r\n\
                   Content-Type: application/sdp\r\n\
                   Content-Length: 100\r\n\
                   \r\n\
                   v=0\r\n\
                   o=test 1 1 IN IP4 10.0.0.1\r\n\
                   c=IN IP4 10.0.0.1\r\n\
                   m=audio 20000 RTP/AVP 0\r\n";
        let sdp = extract_sdp_body(msg).expect("should extract SDP");
        assert!(sdp.contains("v=0"));
        assert!(sdp.contains("m=audio"));
    }

    #[test]
    fn extract_sdp_body_absent() {
        let msg = "SIP/2.0 100 Trying\r\n\r\n";
        assert!(extract_sdp_body(msg).is_none());
    }

    #[test]
    fn register_message_structure() {
        // Verify the generated branch format
        let branch = generate_branch();
        assert!(
            branch.starts_with("z9hG4bK"),
            "branch must start with magic cookie per RFC 3261"
        );
    }

    #[test]
    fn parse_digest_challenge_extracts_fields() {
        let header = "Digest realm=\"telnyx.com\", nonce=\"aef321\", algorithm=MD5, qop=\"auth\", opaque=\"abc/123\"";
        let digest = parse_digest_challenge(header).unwrap();
        assert_eq!(digest.realm, "telnyx.com");
        assert_eq!(digest.nonce, "aef321");
        assert_eq!(digest.qop.as_deref(), Some("auth"));
        assert_eq!(digest.opaque.as_deref(), Some("abc/123"));
    }

    #[test]
    fn parse_digest_challenge_rejects_missing_realm() {
        let header = "Digest nonce=\"abc\"";
        assert!(parse_digest_challenge(header).is_err());
    }

    #[test]
    fn parse_digest_challenge_rejects_missing_nonce() {
        let header = "Digest realm=\"test\"";
        assert!(parse_digest_challenge(header).is_err());
    }
}
