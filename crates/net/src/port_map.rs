//! Automatic port forwarding (spec §10: "built-in NAT-PMP client,
//! auto-renewing").
//!
//! STUN answers "what does the outside world see me as?". It does not make
//! anyone able to *reach* that address: behind most home routers an
//! unsolicited inbound packet has nowhere to go, which is why calling out
//! works and being called does not. NAT-PMP (RFC 6886) is how a program
//! asks the router for an inbound port without the user opening the admin
//! page and typing one in by hand.
//!
//! Only NAT-PMP, not PCP or UPnP-IGD. NAT-PMP is four packet shapes and
//! covers Apple and most consumer routers; UPnP is SOAP over HTTP after an
//! SSDP discovery round and carries a long history of being exploitable.
//! Adding the small protocol first is the right order, and a router that
//! refuses simply leaves the user where they already were.
//!
//! **Mappings expire on purpose.** A mapping is requested with a lifetime
//! and renewed at half of it, as the RFC prescribes: a router that reboots
//! forgets everything, and a client that assumed permanence would sit
//! there believing it was reachable. Requesting a *long* lifetime instead
//! would leave a hole open long after the app closed, which is the same
//! mistake in the other direction.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;

use crate::error::NetError;

/// The port every NAT-PMP router listens on.
const NAT_PMP_PORT: u16 = 5351;

/// Protocol version 0 is NAT-PMP; 2 would be PCP, which this is not.
const VERSION: u8 = 0;

const OP_EXTERNAL_ADDRESS: u8 = 0;
const OP_MAP_UDP: u8 = 1;

/// A response's opcode is the request's with the high bit set.
const RESPONSE_FLAG: u8 = 0x80;

/// How long a mapping is requested for.
///
/// An hour, renewed at half. Long enough that renewal is not constant
/// chatter, short enough that a hole left behind by a crash closes on its
/// own within the hour rather than lasting until the router reboots.
pub const MAPPING_LIFETIME_SECS: u32 = 3600;

/// The result codes the RFC defines. Kept as a named type rather than a
/// bare number so a refusal can be *explained* -- "your router says no"
/// and "your router is too old" need different advice, and a caller with
/// only a `bool` cannot give either.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResultCode {
    Success,
    UnsupportedVersion,
    /// The router understands the request and refuses it -- typically
    /// because the feature is switched off in its settings.
    NotAuthorised,
    NetworkFailure,
    OutOfResources,
    UnsupportedOpcode,
    Unknown(u16),
}

impl ResultCode {
    fn from_u16(code: u16) -> Self {
        match code {
            0 => ResultCode::Success,
            1 => ResultCode::UnsupportedVersion,
            2 => ResultCode::NotAuthorised,
            3 => ResultCode::NetworkFailure,
            4 => ResultCode::OutOfResources,
            5 => ResultCode::UnsupportedOpcode,
            other => ResultCode::Unknown(other),
        }
    }

    /// What to tell the user. Every arm names a next step, because a
    /// failure they cannot act on is just noise in a status line.
    pub fn explain(&self) -> &'static str {
        match self {
            ResultCode::Success => "Your router opened a port automatically.",
            ResultCode::UnsupportedVersion | ResultCode::UnsupportedOpcode => {
                "Your router answered but does not speak NAT-PMP. Forward a port by hand, \
                 or call out rather than waiting to be called."
            }
            ResultCode::NotAuthorised => {
                "Your router refused. Automatic port forwarding (NAT-PMP) is usually a \
                 switch in its settings."
            }
            ResultCode::NetworkFailure => {
                "Your router says it has no working internet connection of its own."
            }
            ResultCode::OutOfResources => {
                "Your router has no free ports to hand out. Another program may be holding \
                 them, or it may need a restart."
            }
            ResultCode::Unknown(_) => "Your router refused, without saying why.",
        }
    }
}

/// A port the router agreed to forward.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mapping {
    /// The port on *this* machine.
    pub internal_port: u16,
    /// The port on the router's public side. Routers are allowed to hand
    /// back a different number than was asked for, which is exactly why
    /// this is read from the response rather than assumed.
    pub external_port: u16,
    /// How long the router says it will keep it, in seconds.
    pub lifetime_secs: u32,
}

impl Mapping {
    /// When to renew, per the RFC: halfway through the lifetime, so a lost
    /// renewal still leaves a second chance before the mapping lapses.
    pub fn renew_after(&self) -> Duration {
        Duration::from_secs((self.lifetime_secs.max(2) / 2) as u64)
    }
}

fn build_external_address_request() -> [u8; 2] {
    [VERSION, OP_EXTERNAL_ADDRESS]
}

/// `[version][op][reserved u16][internal u16][external u16][lifetime u32]`
fn build_map_request(internal_port: u16, suggested_external: u16, lifetime_secs: u32) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[0] = VERSION;
    out[1] = OP_MAP_UDP;
    // bytes 2..4 stay zero (reserved)
    out[4..6].copy_from_slice(&internal_port.to_be_bytes());
    out[6..8].copy_from_slice(&suggested_external.to_be_bytes());
    out[8..12].copy_from_slice(&lifetime_secs.to_be_bytes());
    out
}

/// Checks the two fields every response shares, returning its opcode.
fn response_header(bytes: &[u8], expected_op: u8) -> Option<ResultCode> {
    if bytes.len() < 4 || bytes[0] != VERSION || bytes[1] != expected_op | RESPONSE_FLAG {
        return None;
    }
    Some(ResultCode::from_u16(u16::from_be_bytes([bytes[2], bytes[3]])))
}

/// `[version][op|0x80][result u16][epoch u32][external ipv4]`
pub fn parse_external_address(bytes: &[u8]) -> Option<(ResultCode, Option<Ipv4Addr>)> {
    let code = response_header(bytes, OP_EXTERNAL_ADDRESS)?;
    if code != ResultCode::Success {
        return Some((code, None));
    }
    if bytes.len() < 12 {
        return None;
    }
    Some((code, Some(Ipv4Addr::new(bytes[8], bytes[9], bytes[10], bytes[11]))))
}

/// `[version][op|0x80][result u16][epoch u32][internal u16][external u16][lifetime u32]`
pub fn parse_map_response(bytes: &[u8]) -> Option<(ResultCode, Option<Mapping>)> {
    let code = response_header(bytes, OP_MAP_UDP)?;
    if code != ResultCode::Success {
        return Some((code, None));
    }
    if bytes.len() < 16 {
        return None;
    }
    Some((
        code,
        Some(Mapping {
            internal_port: u16::from_be_bytes([bytes[8], bytes[9]]),
            external_port: u16::from_be_bytes([bytes[10], bytes[11]]),
            lifetime_secs: u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        }),
    ))
}

/// Likely gateway addresses for a machine at `local`.
///
/// A guess, and deliberately a narrow one: reading the real default route
/// needs a different syscall on every platform, and the overwhelmingly
/// common case is `x.y.z.1` on the same /24. Anything wrong here simply
/// times out and the caller falls back to manual forwarding, so a wrong
/// guess costs a second, not correctness.
pub fn likely_gateways(local: Ipv4Addr) -> Vec<Ipv4Addr> {
    let o = local.octets();
    let mut out = vec![Ipv4Addr::new(o[0], o[1], o[2], 1)];
    // Some routers sit at .254 instead. Only worth trying on a private
    // range -- guessing at addresses on a public network is not something
    // this should do uninvited.
    if local.is_private() {
        out.push(Ipv4Addr::new(o[0], o[1], o[2], 254));
    }
    out.retain(|candidate| *candidate != local);
    out
}

/// Sends `request` to the gateway and waits for a matching reply.
///
/// Two attempts rather than the RFC's nine doubling retries: this runs
/// while a user is watching a status line, and a router that has not
/// answered in a couple of seconds is a router that is not going to.
async fn exchange(gateway: Ipv4Addr, request: &[u8]) -> Result<Vec<u8>, NetError> {
    // Its own socket, unlike STUN: a NAT-PMP reply comes from the gateway
    // on port 5351 and has nothing to do with the call's NAT mapping, so
    // there is no reason to put router chatter on the call's socket.
    let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))).await?;
    let target = SocketAddr::V4(SocketAddrV4::new(gateway, NAT_PMP_PORT));
    let mut buf = vec![0u8; 64];

    for _ in 0..2 {
        socket.send_to(request, target).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
                Ok(Ok((n, from))) => {
                    if from.ip() != std::net::IpAddr::V4(gateway) {
                        continue;
                    }
                    return Ok(buf[..n].to_vec());
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => break,
            }
        }
    }
    Err(NetError::PortMapUnavailable)
}

/// Asks the gateway for its public address.
pub async fn external_address(gateway: Ipv4Addr) -> Result<Ipv4Addr, NetError> {
    let reply = exchange(gateway, &build_external_address_request()).await?;
    match parse_external_address(&reply) {
        Some((ResultCode::Success, Some(addr))) => Ok(addr),
        Some((code, _)) => Err(NetError::PortMapRefused(code)),
        None => Err(NetError::PortMapUnavailable),
    }
}

/// Asks the gateway to forward `internal_port` inbound.
///
/// The same call renews an existing mapping -- NAT-PMP has no separate
/// renew operation, and repeating the request is how the RFC says to do
/// it, so renewal needs no code of its own here.
pub async fn request_mapping(gateway: Ipv4Addr, internal_port: u16) -> Result<Mapping, NetError> {
    let request = build_map_request(internal_port, internal_port, MAPPING_LIFETIME_SECS);
    let reply = exchange(gateway, &request).await?;
    match parse_map_response(&reply) {
        Some((ResultCode::Success, Some(mapping))) => Ok(mapping),
        Some((code, _)) => Err(NetError::PortMapRefused(code)),
        None => Err(NetError::PortMapUnavailable),
    }
}

/// Releases a mapping, by re-requesting it with a lifetime of zero.
///
/// Best effort on the way out: if this does not land, the mapping expires
/// on its own within the hour. That is the reason the lifetime is an hour
/// and not a day.
pub async fn release_mapping(gateway: Ipv4Addr, internal_port: u16) {
    let request = build_map_request(internal_port, 0, 0);
    let _ = exchange(gateway, &request).await;
}

/// This machine's address on the local network.
///
/// Found by asking the OS which source address it *would* use to reach a
/// public address. Nothing is sent -- `connect` on a UDP socket only fixes
/// the peer and picks a route -- so this costs no traffic and works with
/// no internet connection present, which matters because a machine behind
/// a router that is itself offline still has a router to ask.
pub async fn local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))).await.ok()?;
    // Any routable address will do; this one is reserved for documentation
    // and is never contacted.
    socket.connect(SocketAddr::from(([203, 0, 113, 1], 9))).await.ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(v4) => Some(*v4.ip()),
        SocketAddr::V6(_) => None,
    }
}

/// Tries each likely gateway until one forwards the port.
pub async fn open_port(local: Ipv4Addr, internal_port: u16) -> Result<(Ipv4Addr, Mapping), NetError> {
    let mut last = NetError::PortMapUnavailable;
    for gateway in likely_gateways(local) {
        match request_mapping(gateway, internal_port).await {
            Ok(mapping) => return Ok((gateway, mapping)),
            // A refusal is the gateway's real answer -- keep it, rather
            // than reporting the last timeout from an address that was
            // never a router at all.
            Err(e @ NetError::PortMapRefused(_)) => return Err(e),
            Err(e) => last = e,
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_response(code: u16, internal: u16, external: u16, lifetime: u32) -> Vec<u8> {
        let mut out = vec![VERSION, OP_MAP_UDP | RESPONSE_FLAG];
        out.extend_from_slice(&code.to_be_bytes());
        out.extend_from_slice(&7u32.to_be_bytes()); // epoch
        out.extend_from_slice(&internal.to_be_bytes());
        out.extend_from_slice(&external.to_be_bytes());
        out.extend_from_slice(&lifetime.to_be_bytes());
        out
    }

    /// Talks to whatever router is actually on this network.
    ///
    /// Ignored by default: it needs a real gateway, and CI has none. Run
    /// it by hand with `cargo test -p antiphon-net live_router -- --ignored
    /// --nocapture` to see what your own router does.
    #[ignore]
    #[tokio::test]
    async fn live_router_probe() {
        let local = local_ipv4().await.expect("this machine has a local IPv4 address");
        println!("local address: {local}");
        for gateway in likely_gateways(local) {
            println!("  trying gateway {gateway}");
            match external_address(gateway).await {
                Ok(addr) => println!("    external address: {addr}"),
                Err(e) => println!("    no address: {e}"),
            }
            match request_mapping(gateway, 47399).await {
                Ok(mapping) => {
                    println!("    mapped: {mapping:?}");
                    release_mapping(gateway, 47399).await;
                    println!("    released");
                }
                Err(e) => println!("    no mapping: {e}"),
            }
        }
    }

    #[test]
    fn a_mapping_request_has_the_shape_the_rfc_describes() {
        let request = build_map_request(6000, 6000, 3600);
        assert_eq!(request[0], 0, "NAT-PMP is version 0");
        assert_eq!(request[1], OP_MAP_UDP);
        assert_eq!(&request[2..4], &[0, 0], "reserved bytes must be zero");
        assert_eq!(u16::from_be_bytes([request[4], request[5]]), 6000);
        assert_eq!(u32::from_be_bytes(request[8..12].try_into().unwrap()), 3600);
    }

    #[test]
    fn the_external_port_is_read_from_the_reply_not_assumed() {
        // Routers are allowed to hand back a different port than the one
        // asked for. Assuming otherwise would advertise an address nobody
        // can reach, which is worse than not mapping at all.
        let reply = map_response(0, 6000, 51820, 3600);
        let (code, mapping) = parse_map_response(&reply).expect("well-formed");
        assert_eq!(code, ResultCode::Success);
        assert_eq!(mapping.unwrap().external_port, 51820);
    }

    #[test]
    fn a_refusal_is_reported_with_its_reason_rather_than_as_a_timeout() {
        let (code, mapping) = parse_map_response(&map_response(2, 0, 0, 0)).expect("well-formed");
        assert_eq!(code, ResultCode::NotAuthorised);
        assert!(mapping.is_none());
    }

    #[test]
    fn every_refusal_tells_the_user_what_to_do_about_it() {
        // A failure someone cannot act on is noise in a status line.
        for code in [
            ResultCode::UnsupportedVersion,
            ResultCode::NotAuthorised,
            ResultCode::NetworkFailure,
            ResultCode::OutOfResources,
            ResultCode::UnsupportedOpcode,
            ResultCode::Unknown(9),
        ] {
            let text = code.explain();
            assert!(!text.is_empty(), "{code:?} says nothing");
            assert!(text.len() > 30, "{code:?} says too little: {text}");
        }
    }

    #[test]
    fn a_reply_to_a_different_request_is_not_accepted() {
        // The address and mapping replies differ only by an opcode byte;
        // reading one as the other would produce a plausible-looking
        // mapping out of an address.
        let mut reply = map_response(0, 6000, 6000, 3600);
        reply[1] = OP_EXTERNAL_ADDRESS | RESPONSE_FLAG;
        assert!(parse_map_response(&reply).is_none());
    }

    #[test]
    fn a_request_echoed_back_is_not_mistaken_for_a_reply() {
        // Without the response flag check, our own multicast-looped
        // request would parse as a successful mapping of port 0.
        let request = build_map_request(6000, 6000, 3600);
        assert!(parse_map_response(&request).is_none());
    }

    #[test]
    fn a_truncated_reply_is_refused_rather_than_read_past() {
        let reply = map_response(0, 6000, 6000, 3600);
        for length in 4..reply.len() {
            assert!(parse_map_response(&reply[..length]).is_none(), "length {length}");
        }
        assert!(parse_map_response(&[]).is_none());
    }

    #[test]
    fn a_reply_from_a_router_speaking_another_version_is_ignored() {
        // Version 2 is PCP. Its layout differs, so reading it as NAT-PMP
        // would produce nonsense that looks like success.
        let mut reply = map_response(0, 6000, 6000, 3600);
        reply[0] = 2;
        assert!(parse_map_response(&reply).is_none());
    }

    #[test]
    fn an_external_address_reply_round_trips() {
        let mut reply = vec![VERSION, OP_EXTERNAL_ADDRESS | RESPONSE_FLAG, 0, 0];
        reply.extend_from_slice(&1u32.to_be_bytes());
        reply.extend_from_slice(&[203, 0, 113, 5]);
        let (code, addr) = parse_external_address(&reply).expect("well-formed");
        assert_eq!(code, ResultCode::Success);
        assert_eq!(addr, Some(Ipv4Addr::new(203, 0, 113, 5)));
    }

    #[test]
    fn renewal_happens_at_half_the_lifetime() {
        // Per the RFC. A lost renewal then still has a second chance
        // before the mapping lapses and the user quietly stops being
        // reachable.
        let mapping = Mapping { internal_port: 1, external_port: 1, lifetime_secs: 3600 };
        assert_eq!(mapping.renew_after(), Duration::from_secs(1800));
    }

    #[test]
    fn an_absurdly_short_lifetime_does_not_produce_a_zero_renewal_interval() {
        // A router answering with 0 or 1 second must not turn the renewal
        // timer into a busy loop hammering it.
        for lifetime in [0, 1, 2] {
            let mapping = Mapping { internal_port: 1, external_port: 1, lifetime_secs: lifetime };
            assert!(mapping.renew_after() >= Duration::from_secs(1), "{lifetime}s");
        }
    }

    #[test]
    fn a_release_is_a_mapping_request_with_no_lifetime() {
        // NAT-PMP has no delete operation; zero lifetime is the delete.
        let request = build_map_request(6000, 0, 0);
        assert_eq!(u32::from_be_bytes(request[8..12].try_into().unwrap()), 0);
        assert_eq!(u16::from_be_bytes([request[6], request[7]]), 0);
    }

    #[test]
    fn the_gateway_guess_covers_the_common_layouts() {
        let gateways = likely_gateways(Ipv4Addr::new(192, 168, 1, 47));
        assert!(gateways.contains(&Ipv4Addr::new(192, 168, 1, 1)));
        assert!(gateways.contains(&Ipv4Addr::new(192, 168, 1, 254)));
    }

    #[test]
    fn a_machine_that_is_itself_dot_one_does_not_ask_itself() {
        let gateways = likely_gateways(Ipv4Addr::new(192, 168, 1, 1));
        assert!(!gateways.contains(&Ipv4Addr::new(192, 168, 1, 1)));
        assert!(!gateways.is_empty(), "there is still another address worth trying");
    }

    #[test]
    fn a_public_address_is_not_probed_at_a_guessed_second_gateway() {
        // Guessing at addresses on a public network is not something this
        // should do uninvited.
        let gateways = likely_gateways(Ipv4Addr::new(203, 0, 113, 47));
        assert_eq!(gateways, vec![Ipv4Addr::new(203, 0, 113, 1)]);
    }
}
