//! Minimal STUN client (RFC 5389) for discovering the public address a
//! home router hands out for one of our UDP sockets -- the first half of
//! making calls work between two houses instead of only across a LAN.
//!
//! Only the Binding Request/Response exchange is implemented, which is all
//! that's needed to answer "what does the outside world see me as?". No
//! authentication, no TURN, no ICE candidate gathering.
//!
//! The critical detail is that discovery must run on **the same socket**
//! the call will use. A NAT mapping belongs to a specific
//! (internal addr, internal port) tuple, so the address discovered through
//! a throwaway socket says nothing about the one the call runs on. Hence
//! [`discover_public_addr`] takes a borrowed socket rather than making one.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use tokio::net::UdpSocket;

use crate::error::NetError;

const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const HEADER_LEN: usize = 20;

/// Public STUN servers to try in order. Several, because any one of them
/// can be down or blocked, and a single failure shouldn't read to the user
/// as "NAT discovery doesn't work".
pub const DEFAULT_STUN_SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.cloudflare.com:3478",
];

/// A 12-byte transaction ID identifying one request/response pair.
pub type TransactionId = [u8; 12];

fn random_transaction_id() -> TransactionId {
    // Not security-critical -- it only has to be unlikely to collide with
    // another in-flight request on the same socket. Sourced from the OS
    // via `getrandom` (through x25519-dalek's dependency) would be
    // overkill; system time plus the address of a local gives ample spread.
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut id = [0u8; 12];
    id[..8].copy_from_slice(&(nanos as u64).to_be_bytes());
    let stack_spread = &id as *const _ as usize as u32;
    id[8..].copy_from_slice(&stack_spread.to_be_bytes());
    id
}

fn build_binding_request(txid: &TransactionId) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    out[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    out[2..4].copy_from_slice(&0u16.to_be_bytes()); // no attributes
    out[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out[8..20].copy_from_slice(txid);
    out
}

/// Pulls our public address out of a Binding Success response, preferring
/// XOR-MAPPED-ADDRESS and falling back to the legacy MAPPED-ADDRESS.
/// Returns `None` for anything that isn't a well-formed success response
/// for `txid` -- a stray packet on a shared socket must never be mistaken
/// for an answer.
fn parse_binding_response(bytes: &[u8], txid: &TransactionId) -> Option<SocketAddr> {
    if bytes.len() < HEADER_LEN {
        return None;
    }
    if u16::from_be_bytes([bytes[0], bytes[1]]) != BINDING_SUCCESS {
        return None;
    }
    if u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) != MAGIC_COOKIE {
        return None;
    }
    if &bytes[8..20] != txid.as_slice() {
        return None;
    }
    let attrs_len = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
    let attrs = bytes.get(HEADER_LEN..HEADER_LEN + attrs_len)?;

    let mut fallback = None;
    let mut cursor = 0usize;
    while cursor + 4 <= attrs.len() {
        let attr_type = u16::from_be_bytes([attrs[cursor], attrs[cursor + 1]]);
        let attr_len = u16::from_be_bytes([attrs[cursor + 2], attrs[cursor + 3]]) as usize;
        let value = attrs.get(cursor + 4..cursor + 4 + attr_len)?;

        match attr_type {
            ATTR_XOR_MAPPED_ADDRESS => {
                if let Some(addr) = parse_address(value, txid, true) {
                    return Some(addr);
                }
            }
            ATTR_MAPPED_ADDRESS => {
                if fallback.is_none() {
                    fallback = parse_address(value, txid, false);
                }
            }
            _ => {}
        }
        // Attribute values are padded out to a 4-byte boundary.
        cursor += 4 + attr_len.div_ceil(4) * 4;
    }
    fallback
}

/// Decodes a STUN address attribute. When `xor` is set, the port and
/// address are masked with the magic cookie (and, for IPv6, the
/// transaction ID) exactly as RFC 5389 specifies.
fn parse_address(value: &[u8], txid: &TransactionId, xor: bool) -> Option<SocketAddr> {
    if value.len() < 4 {
        return None;
    }
    let family = value[1];
    let raw_port = u16::from_be_bytes([value[2], value[3]]);
    let port = if xor { raw_port ^ (MAGIC_COOKIE >> 16) as u16 } else { raw_port };

    match family {
        0x01 => {
            let raw: [u8; 4] = value.get(4..8)?.try_into().ok()?;
            let octets = if xor {
                let mask = MAGIC_COOKIE.to_be_bytes();
                [raw[0] ^ mask[0], raw[1] ^ mask[1], raw[2] ^ mask[2], raw[3] ^ mask[3]]
            } else {
                raw
            };
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port))
        }
        0x02 => {
            let raw: [u8; 16] = value.get(4..20)?.try_into().ok()?;
            let mut octets = raw;
            if xor {
                let mut mask = [0u8; 16];
                mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
                mask[4..].copy_from_slice(txid);
                for i in 0..16 {
                    octets[i] ^= mask[i];
                }
            }
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
        }
        _ => None,
    }
}

/// Asks one STUN server what public address it sees `socket` coming from.
/// Retries a few times, since a lost UDP request is ordinary rather than
/// exceptional.
pub async fn discover_public_addr(socket: &UdpSocket, server: SocketAddr) -> Result<SocketAddr, NetError> {
    let txid = random_transaction_id();
    let request = build_binding_request(&txid);
    let mut buf = vec![0u8; 1024];

    for _ in 0..3 {
        socket.send_to(&request, server).await?;
        // Keep reading until the timeout: the call socket can carry other
        // traffic, and a non-matching packet must not end the attempt.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(700);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
                Ok(Ok((n, from))) => {
                    if from != server {
                        continue;
                    }
                    if let Some(addr) = parse_binding_response(&buf[..n], &txid) {
                        return Ok(addr);
                    }
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => break, // timed out, try another request
            }
        }
    }
    Err(NetError::StunFailed)
}

/// Tries each of [`DEFAULT_STUN_SERVERS`] until one answers.
pub async fn discover_public_addr_default(socket: &UdpSocket) -> Result<SocketAddr, NetError> {
    for name in DEFAULT_STUN_SERVERS {
        let Ok(addrs) = tokio::net::lookup_host(name).await else { continue };
        for server in addrs {
            // Our sockets are bound to IPv4 (0.0.0.0), which can't send to
            // an IPv6 destination -- skip rather than erroring out.
            if !server.is_ipv4() {
                continue;
            }
            if let Ok(addr) = discover_public_addr(socket, server).await {
                return Ok(addr);
            }
        }
    }
    Err(NetError::StunFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a Binding Success response carrying an XOR-MAPPED-ADDRESS,
    /// the way a real STUN server would.
    fn synthetic_xor_response(txid: &TransactionId, addr: Ipv4Addr, port: u16) -> Vec<u8> {
        let mask = MAGIC_COOKIE.to_be_bytes();
        let o = addr.octets();
        let mut value = vec![0u8, 0x01];
        value.extend_from_slice(&(port ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
        value.extend_from_slice(&[o[0] ^ mask[0], o[1] ^ mask[1], o[2] ^ mask[2], o[3] ^ mask[3]]);

        let mut out = Vec::new();
        out.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        out.extend_from_slice(&((4 + value.len()) as u16).to_be_bytes());
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(txid);
        out.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(&value);
        out
    }

    #[test]
    fn binding_request_has_the_shape_a_stun_server_expects() {
        let txid = [7u8; 12];
        let req = build_binding_request(&txid);
        assert_eq!(u16::from_be_bytes([req[0], req[1]]), BINDING_REQUEST);
        assert_eq!(u16::from_be_bytes([req[2], req[3]]), 0, "no attributes, so length is zero");
        assert_eq!(u32::from_be_bytes([req[4], req[5], req[6], req[7]]), MAGIC_COOKIE);
        assert_eq!(&req[8..20], &txid);
    }

    #[test]
    fn xor_mapped_address_decodes_back_to_the_original() {
        let txid = [3u8; 12];
        let response = synthetic_xor_response(&txid, Ipv4Addr::new(203, 0, 113, 47), 51820);
        let parsed = parse_binding_response(&response, &txid).expect("well-formed response should parse");
        assert_eq!(parsed, SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 47)), 51820));
    }

    #[test]
    fn response_for_a_different_transaction_is_rejected() {
        let txid = [3u8; 12];
        let response = synthetic_xor_response(&txid, Ipv4Addr::new(198, 51, 100, 9), 4000);
        assert!(
            parse_binding_response(&response, &[9u8; 12]).is_none(),
            "a response to somebody else's request must not be accepted as ours"
        );
    }

    #[test]
    fn non_stun_traffic_on_the_same_socket_is_ignored() {
        let txid = [1u8; 12];
        assert!(parse_binding_response(b"this is an audio packet, not STUN", &txid).is_none());
        assert!(parse_binding_response(&[], &txid).is_none());
        // Right length, wrong magic cookie.
        let mut bogus = vec![0u8; HEADER_LEN];
        bogus[0..2].copy_from_slice(&BINDING_SUCCESS.to_be_bytes());
        assert!(parse_binding_response(&bogus, &txid).is_none());
    }

    #[test]
    fn legacy_mapped_address_is_used_when_no_xor_attribute_is_present() {
        let txid = [5u8; 12];
        let value = [0u8, 0x01, 0x1f, 0x90, 192, 0, 2, 33]; // port 8080
        let mut out = Vec::new();
        out.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        out.extend_from_slice(&((4 + value.len()) as u16).to_be_bytes());
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&txid);
        out.extend_from_slice(&ATTR_MAPPED_ADDRESS.to_be_bytes());
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(&value);

        let parsed = parse_binding_response(&out, &txid).expect("legacy attribute should still parse");
        assert_eq!(parsed, SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 33)), 8080));
    }

    /// Talks to a real STUN server. Ignored by default so the suite stays
    /// offline and deterministic; run it deliberately with
    /// `cargo test -p antiphon-net -- --ignored --nocapture` to confirm
    /// discovery still works end to end against the live internet.
    #[tokio::test]
    #[ignore = "requires internet access"]
    async fn discovers_a_public_address_from_a_real_server() {
        let socket = UdpSocket::bind("0.0.0.0:0").await.expect("bind a local socket");
        let local = socket.local_addr().unwrap();
        let public = discover_public_addr_default(&socket).await.expect("a STUN server should answer");
        println!("local {local}  ->  public {public}");
        assert!(!public.ip().is_unspecified(), "a real answer is never 0.0.0.0");
        assert_ne!(public.port(), 0);
    }

    #[test]
    fn attribute_padding_is_walked_correctly() {
        // An unknown attribute with a 3-byte value (padded to 4) sitting
        // before the one we want -- a parser that forgets padding lands
        // mid-attribute and finds nothing.
        let txid = [2u8; 12];
        let mask = MAGIC_COOKIE.to_be_bytes();
        let o = [198u8, 51, 100, 7];
        let mut xor_value = vec![0u8, 0x01];
        xor_value.extend_from_slice(&(1234u16 ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
        xor_value.extend_from_slice(&[o[0] ^ mask[0], o[1] ^ mask[1], o[2] ^ mask[2], o[3] ^ mask[3]]);

        let mut attrs = Vec::new();
        attrs.extend_from_slice(&0x8022u16.to_be_bytes()); // SOFTWARE
        attrs.extend_from_slice(&3u16.to_be_bytes());
        attrs.extend_from_slice(&[b'a', b'b', b'c', 0]); // 3 bytes + 1 pad
        attrs.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        attrs.extend_from_slice(&(xor_value.len() as u16).to_be_bytes());
        attrs.extend_from_slice(&xor_value);

        let mut out = Vec::new();
        out.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        out.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&txid);
        out.extend_from_slice(&attrs);

        let parsed = parse_binding_response(&out, &txid).expect("should skip the padded attribute");
        assert_eq!(parsed, SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)), 1234));
    }
}
