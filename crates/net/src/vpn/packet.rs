//! Just enough IP to carry UDP through the tunnel.
//!
//! The app only ever sends UDP, so a full TCP/IP stack would be dead
//! weight: the tunnel wraps each datagram in an IPv4 or IPv6 header and a
//! UDP header with real checksums (the server's kernel checks them), and
//! unwraps the replies. Fragments, options and extension headers are not
//! produced, and inbound packets that use them are dropped.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const UDP: u8 = 17;

fn fold(mut sum: u32) -> u16 {
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn add_bytes(mut sum: u32, bytes: &[u8]) -> u32 {
    let mut chunks = bytes.chunks_exact(2);
    for c in &mut chunks {
        sum += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    if let [last] = chunks.remainder() {
        sum += (*last as u32) << 8;
    }
    sum
}

fn udp_header(src_port: u16, dst_port: u16, payload: &[u8], pseudo: u32) -> [u8; 8] {
    let len = (8 + payload.len()) as u16;
    let mut h = [0u8; 8];
    h[0..2].copy_from_slice(&src_port.to_be_bytes());
    h[2..4].copy_from_slice(&dst_port.to_be_bytes());
    h[4..6].copy_from_slice(&len.to_be_bytes());
    let mut sum = pseudo + len as u32 + UDP as u32;
    sum = add_bytes(sum, &h);
    sum = add_bytes(sum, payload);
    let mut check = fold(sum);
    if check == 0 {
        check = 0xffff; // zero means "no checksum" in UDP
    }
    h[6..8].copy_from_slice(&check.to_be_bytes());
    h
}

/// Wraps `payload` in UDP and IP. `src` and `dst` must be the same family.
pub fn build_udp(src: SocketAddr, dst: SocketAddr, payload: &[u8]) -> Option<Vec<u8>> {
    match (src.ip(), dst.ip()) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            let total = 20 + 8 + payload.len();
            if total > u16::MAX as usize {
                return None;
            }
            let mut p = Vec::with_capacity(total);
            p.extend_from_slice(&[0x45, 0]);
            p.extend_from_slice(&(total as u16).to_be_bytes());
            p.extend_from_slice(&[0, 0, 0, 0, 64, UDP, 0, 0]);
            p.extend_from_slice(&s.octets());
            p.extend_from_slice(&d.octets());
            let check = fold(add_bytes(0, &p[..20]));
            p[10..12].copy_from_slice(&check.to_be_bytes());
            let pseudo = add_bytes(add_bytes(0, &s.octets()), &d.octets());
            p.extend_from_slice(&udp_header(src.port(), dst.port(), payload, pseudo));
            p.extend_from_slice(payload);
            Some(p)
        }
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            let udp_len = 8 + payload.len();
            if udp_len > u16::MAX as usize {
                return None;
            }
            let mut p = Vec::with_capacity(40 + udp_len);
            p.extend_from_slice(&[0x60, 0, 0, 0]);
            p.extend_from_slice(&(udp_len as u16).to_be_bytes());
            p.extend_from_slice(&[UDP, 64]);
            p.extend_from_slice(&s.octets());
            p.extend_from_slice(&d.octets());
            let pseudo = add_bytes(add_bytes(0, &s.octets()), &d.octets());
            p.extend_from_slice(&udp_header(src.port(), dst.port(), payload, pseudo));
            p.extend_from_slice(payload);
            Some(p)
        }
        _ => None,
    }
}

/// A UDP datagram unwrapped from an inbound IP packet.
#[derive(Debug, PartialEq, Eq)]
pub struct UdpDatagram<'a> {
    pub src: SocketAddr,
    pub dst: SocketAddr,
    pub payload: &'a [u8],
}

fn udp_payload(src: IpAddr, dst: IpAddr, udp: &[u8]) -> Option<UdpDatagram<'_>> {
    if udp.len() < 8 {
        return None;
    }
    let len = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    if len < 8 || len > udp.len() {
        return None;
    }
    Some(UdpDatagram {
        src: SocketAddr::new(src, u16::from_be_bytes([udp[0], udp[1]])),
        dst: SocketAddr::new(dst, u16::from_be_bytes([udp[2], udp[3]])),
        payload: &udp[8..len],
    })
}

/// Unwraps a UDP datagram, or `None` for anything else (TCP, ICMP,
/// fragments, IPv6 extension headers).
pub fn parse_udp(packet: &[u8]) -> Option<UdpDatagram<'_>> {
    match packet.first()? >> 4 {
        4 => {
            if packet.len() < 20 {
                return None;
            }
            let ihl = (packet[0] & 0x0f) as usize * 4;
            let total = u16::from_be_bytes([packet[2], packet[3]]) as usize;
            let fragmented = packet[6] & 0x20 != 0 || u16::from_be_bytes([packet[6] & 0x1f, packet[7]]) != 0;
            if ihl < 20 || total < ihl || total > packet.len() || packet[9] != UDP || fragmented {
                return None;
            }
            let src = Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]);
            let dst = Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]);
            udp_payload(src.into(), dst.into(), &packet[ihl..total])
        }
        6 => {
            if packet.len() < 40 || packet[6] != UDP {
                return None;
            }
            let len = u16::from_be_bytes([packet[4], packet[5]]) as usize;
            if 40 + len > packet.len() {
                return None;
            }
            let src = Ipv6Addr::from(<[u8; 16]>::try_from(&packet[8..24]).ok()?);
            let dst = Ipv6Addr::from(<[u8; 16]>::try_from(&packet[24..40]).ok()?);
            udp_payload(src.into(), dst.into(), &packet[40..40 + len])
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_round_trip_with_valid_checksums() {
        let src: SocketAddr = "10.64.0.2:50000".parse().unwrap();
        let dst: SocketAddr = "198.51.100.7:3478".parse().unwrap();
        let p = build_udp(src, dst, b"odd length payload!").unwrap();
        // A correct IPv4 header checksums to zero.
        assert_eq!(fold(add_bytes(0, &p[..20])), 0);
        // So does a correct UDP checksum over its pseudo-header.
        let pseudo = add_bytes(add_bytes(0, &p[12..16]), &p[16..20]);
        let udp = &p[20..];
        assert_eq!(fold(add_bytes(pseudo + udp.len() as u32 + 17, udp)), 0);
        let got = parse_udp(&p).unwrap();
        assert_eq!((got.src, got.dst, got.payload), (src, dst, &b"odd length payload!"[..]));
    }

    #[test]
    fn ipv6_round_trip_with_valid_checksum() {
        let src: SocketAddr = "[fd00::2]:40000".parse().unwrap();
        let dst: SocketAddr = "[2001:db8::1]:9000".parse().unwrap();
        let p = build_udp(src, dst, b"v6").unwrap();
        let pseudo = add_bytes(add_bytes(0, &p[8..24]), &p[24..40]);
        let udp = &p[40..];
        assert_eq!(fold(add_bytes(pseudo + udp.len() as u32 + 17, udp)), 0);
        let got = parse_udp(&p).unwrap();
        assert_eq!((got.src, got.dst, got.payload), (src, dst, &b"v6"[..]));
    }

    #[test]
    fn mixed_families_and_junk_are_refused() {
        let v4: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let v6: SocketAddr = "[::1]:1".parse().unwrap();
        assert!(build_udp(v4, v6, b"").is_none());
        assert!(parse_udp(&[0x45, 0, 0]).is_none());
        let mut tcp = build_udp(v4, v4, b"x").unwrap();
        tcp[9] = 6;
        assert!(parse_udp(&tcp).is_none());
    }
}
