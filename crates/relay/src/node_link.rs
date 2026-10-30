//! A mailbox node as a link, for a QR code (spec §8 node picker "by
//! address, by QR"): `stoop://node/<host:port>`, with `?code=<members
//! code>` for a community node that serves only its members.
//!
//! The node prints its link and QR code when it starts (`--announce`
//! names the address people reach it at); the app reads one from a
//! picture of the code, or from the link pasted.

use std::fmt::Write as _;

/// How a node link starts.
pub const PREFIX: &str = "stoop://node/";

/// A mailbox node, as its link names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeLink {
    /// `host:port` the node is reached at.
    pub address: String,
    /// Its members code, empty for a node open to anyone.
    pub code: String,
}

impl NodeLink {
    /// The link: `stoop://node/host:port`, then `?code=` and the code
    /// percent-encoded, if there is one.
    pub fn to_link(&self) -> String {
        let mut link = format!("{PREFIX}{}", self.address);
        if !self.code.is_empty() {
            link.push_str("?code=");
            for b in self.code.bytes() {
                if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                    link.push(b as char);
                } else {
                    // Writing to a String cannot fail.
                    let _ = write!(link, "%{b:02X}");
                }
            }
        }
        link
    }

    /// Reads a link back; `None` for anything else, or an address
    /// without a port.
    pub fn parse(link: &str) -> Option<Self> {
        let rest = link.trim().strip_prefix(PREFIX)?;
        let (address, query) = rest.split_once('?').unwrap_or((rest, ""));
        let address = address.trim_end_matches('/');
        let (host, port) = address.rsplit_once(':')?;
        if host.is_empty() || port.parse::<u16>().is_err() || address.chars().any(char::is_whitespace) {
            return None;
        }
        let code = match query.strip_prefix("code=") {
            Some(encoded) => percent_decode(encoded)?,
            None if query.is_empty() => String::new(),
            None => return None,
        };
        Some(Self { address: address.to_string(), code })
    }
}

/// Undoes `%XX` escapes; `None` for a broken one or bytes that aren't
/// UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_link_round_trips_with_and_without_a_code() {
        let open = NodeLink { address: "mail.example.org:7000".into(), code: String::new() };
        assert_eq!(open.to_link(), "stoop://node/mail.example.org:7000");
        assert_eq!(NodeLink::parse(&open.to_link()), Some(open));
        let members = NodeLink { address: "[2001:db8::1]:7000".into(), code: "choir & friends".into() };
        assert_eq!(NodeLink::parse(&members.to_link()), Some(members));
        assert_eq!(NodeLink::parse("stoop://node/no-port"), None);
        assert_eq!(NodeLink::parse("https://example.org:7000"), None);
    }
}
