//! wg-quick `.conf` files (spec §6 "bring your own VPN").
//!
//! The format every provider hands out: an `[Interface]` with our private
//! key and tunnel address, a `[Peer]` with the server. Keys this app has no
//! use for (`MTU`, `Table`, `ListenPort`, `FwMark`, `SaveConfig`) are
//! accepted and ignored. `PreUp`/`PostUp`/`PreDown`/`PostDown` are shell
//! commands for wg-quick to run as root; they are ignored too, and never
//! run -- importing a file must not execute anything in it.

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use base64::Engine;
use zeroize::Zeroizing;

/// An address with a prefix length, as in `Address = 10.64.0.2/32` or
/// `AllowedIPs = 0.0.0.0/0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpNet {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl IpNet {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix as u32) };
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = if self.prefix == 0 { 0 } else { u128::MAX << (128 - self.prefix as u32) };
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }

    fn parse(text: &str) -> Result<Self, ConfigError> {
        let bad = || ConfigError::BadValue(format!("`{text}` is not an address"));
        let (addr, prefix) = match text.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (text, None),
        };
        let addr: IpAddr = addr.trim().parse().map_err(|_| bad())?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p.trim().parse::<u8>().ok().filter(|p| *p <= max).ok_or_else(bad)?,
            None => max,
        };
        Ok(Self { addr, prefix })
    }
}

impl fmt::Display for IpNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// Why a file could not be imported. Each names the line or key to fix,
/// because "invalid config" sends people back to the provider for nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("this is not a WireGuard file: it has no [Interface] section")]
    NoInterface,
    #[error("the file has no [Peer] section, so there is no server to connect to")]
    NoPeer,
    #[error("the file has {0} [Peer] sections; only single-server files are supported")]
    ManyPeers(usize),
    #[error("[{section}] is missing {key}")]
    Missing { section: &'static str, key: &'static str },
    #[error("{0} is not a WireGuard key (expected 44 characters of base64)")]
    BadKey(&'static str),
    #[error("{0}")]
    BadValue(String),
    #[error("line {0} is not `Key = Value` or a [Section]")]
    BadLine(usize),
}

/// A parsed wg-quick file, reduced to what a userspace tunnel needs.
#[derive(Clone, PartialEq, Eq)]
pub struct WgConfig {
    pub private_key: Zeroizing<[u8; 32]>,
    pub addresses: Vec<IpNet>,
    pub dns: Vec<IpAddr>,
    pub peer_public_key: [u8; 32],
    pub preshared_key: Option<Zeroizing<[u8; 32]>>,
    /// `host:port`, resolved when the tunnel starts so a provider's
    /// hostname can move.
    pub endpoint: String,
    pub allowed_ips: Vec<IpNet>,
    pub persistent_keepalive: Option<Duration>,
}

impl fmt::Debug for WgConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the private or preshared key, even in a debug print.
        f.debug_struct("WgConfig")
            .field("addresses", &self.addresses)
            .field("dns", &self.dns)
            .field("endpoint", &self.endpoint)
            .field("allowed_ips", &self.allowed_ips)
            .finish_non_exhaustive()
    }
}

fn key(value: &str, name: &'static str) -> Result<[u8; 32], ConfigError> {
    base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .ok_or(ConfigError::BadKey(name))
}

fn list(value: &str) -> impl Iterator<Item = &str> {
    value.split(',').map(str::trim).filter(|s| !s.is_empty())
}

#[derive(PartialEq)]
enum Section {
    None,
    Interface,
    Peer,
    Other,
}

impl WgConfig {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut section = Section::None;
        let mut interfaces = 0;
        let mut peers = 0;
        let mut private_key = None;
        let mut addresses = Vec::new();
        let mut dns = Vec::new();
        let mut public = None;
        let mut psk = None;
        let mut endpoint = None;
        let mut allowed = Vec::new();
        let mut keepalive = None;

        for (n, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = match line[1..line.len() - 1].trim().to_ascii_lowercase().as_str() {
                    "interface" => {
                        interfaces += 1;
                        Section::Interface
                    }
                    "peer" => {
                        peers += 1;
                        Section::Peer
                    }
                    _ => Section::Other,
                };
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { return Err(ConfigError::BadLine(n + 1)) };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            match (&section, k.as_str()) {
                (Section::Interface, "privatekey") => private_key = Some(Zeroizing::new(key(v, "PrivateKey")?)),
                (Section::Interface, "address") => {
                    for a in list(v) {
                        addresses.push(IpNet::parse(a)?);
                    }
                }
                (Section::Interface, "dns") => {
                    // wg-quick also allows search domains here; only
                    // addresses mean anything to a resolver.
                    dns.extend(list(v).filter_map(|d| d.parse::<IpAddr>().ok()));
                }
                (Section::Peer, _) if peers > 1 => {}
                (Section::Peer, "publickey") => public = Some(key(v, "PublicKey")?),
                (Section::Peer, "presharedkey") => psk = Some(Zeroizing::new(key(v, "PresharedKey")?)),
                (Section::Peer, "endpoint") => {
                    let ok = v.rsplit_once(':').is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok());
                    if !ok {
                        return Err(ConfigError::BadValue(format!("Endpoint `{v}` needs a host and a port")));
                    }
                    endpoint = Some(v.to_string());
                }
                (Section::Peer, "allowedips") => {
                    for a in list(v) {
                        allowed.push(IpNet::parse(a)?);
                    }
                }
                (Section::Peer, "persistentkeepalive") => {
                    keepalive = match v.to_ascii_lowercase().as_str() {
                        "off" | "0" => None,
                        s => Some(Duration::from_secs(s.parse::<u16>().map_err(|_| {
                            ConfigError::BadValue(format!("PersistentKeepalive `{v}` is not a number of seconds"))
                        })? as u64)),
                    }
                }
                _ => {}
            }
        }

        if interfaces == 0 {
            return Err(ConfigError::NoInterface);
        }
        match peers {
            0 => return Err(ConfigError::NoPeer),
            1 => {}
            n => return Err(ConfigError::ManyPeers(n)),
        }
        let missing = |section, key| ConfigError::Missing { section, key };
        let config = Self {
            private_key: private_key.ok_or(missing("Interface", "PrivateKey"))?,
            peer_public_key: public.ok_or(missing("Peer", "PublicKey"))?,
            preshared_key: psk,
            endpoint: endpoint.ok_or(missing("Peer", "Endpoint"))?,
            persistent_keepalive: keepalive,
            dns,
            addresses,
            allowed_ips: allowed,
        };
        if config.addresses.is_empty() {
            return Err(missing("Interface", "Address"));
        }
        if config.allowed_ips.is_empty() {
            return Err(missing("Peer", "AllowedIPs"));
        }
        Ok(config)
    }

    /// Our public key, for showing which config is which.
    pub fn public_key(&self) -> [u8; 32] {
        super::noise::public_key(&self.private_key)
    }

    /// Whether traffic to `ip` may go through this tunnel at all. A
    /// split-tunnel file (`AllowedIPs = 10.0.0.0/8`) does not reach the
    /// internet, and the server would drop anything else.
    pub fn routes(&self, ip: IpAddr) -> bool {
        self.allowed_ips.iter().any(|net| net.contains(ip))
    }

    /// The tunnel address to send from for a destination of this family.
    pub fn source_for(&self, dst: IpAddr) -> Option<IpAddr> {
        self.addresses.iter().map(|a| a.addr).find(|a| a.is_ipv4() == dst.is_ipv4())
    }

    /// Writes the file back out in wg-quick form -- the form it is stored
    /// in (sealed), so a stored config is exactly what was imported.
    pub fn to_conf(&self) -> Zeroizing<String> {
        let b64 = |k: &[u8; 32]| base64::engine::general_purpose::STANDARD.encode(k);
        let join = |v: &[IpNet]| v.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
        let mut s = format!("[Interface]\nPrivateKey = {}\nAddress = {}\n", b64(&self.private_key), join(&self.addresses));
        if !self.dns.is_empty() {
            let dns: Vec<String> = self.dns.iter().map(ToString::to_string).collect();
            s += &format!("DNS = {}\n", dns.join(", "));
        }
        s += &format!("\n[Peer]\nPublicKey = {}\n", b64(&self.peer_public_key));
        if let Some(psk) = &self.preshared_key {
            s += &format!("PresharedKey = {}\n", b64(psk));
        }
        s += &format!("Endpoint = {}\nAllowedIPs = {}\n", self.endpoint, join(&self.allowed_ips));
        if let Some(k) = self.persistent_keepalive {
            s += &format!("PersistentKeepalive = {}\n", k.as_secs());
        }
        Zeroizing::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shaped like a Mullvad download: IPv4 and IPv6 addresses, DNS, a
    // full-tunnel AllowedIPs, and wg-quick's shell hooks.
    const MULLVAD_STYLE: &str = "\
[Interface]
# Device: Quick Otter
PrivateKey = yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=
Address = 10.68.12.34/32,fc00:bbbb:bbbb:bb01::5:c21/128
DNS = 10.64.0.1
PostUp = iptables -I OUTPUT ! -o %i -j REJECT

[Peer]
PublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=
AllowedIPs = 0.0.0.0/0,::0/0
Endpoint = 185.213.154.68:51820
";

    // Shaped like a self-hosted server: preshared key, keepalive, split
    // tunnel, a hostname endpoint, odd spacing and case.
    const SELF_HOSTED: &str = "
[interface]
privatekey=   oK56DE9Ue9zK76rAc8pBl6opph+1v36lm7cXXsQKrQM=
Address = 10.8.0.2/24
DNS = 1.1.1.1, 2606:4700:4700::1111, home.lan
MTU = 1420

[Peer]
PublicKey = HIgo9xNzJMWLKASShiTqIybxZ0U3wGLiUeJ1PKf8ykw=
PresharedKey = FpCyhws9cxwWoV4xELtfJvjJN+zQVRPISllRWgeopVE=
AllowedIPs = 10.8.0.0/24, 192.168.1.0/24
Endpoint = vpn.example.org:51820
PersistentKeepalive = 25
";

    #[test]
    fn parses_a_provider_file() {
        let c = WgConfig::parse(MULLVAD_STYLE).unwrap();
        assert_eq!(c.addresses.len(), 2);
        assert_eq!(c.addresses[0].to_string(), "10.68.12.34/32");
        assert_eq!(c.dns, vec!["10.64.0.1".parse::<IpAddr>().unwrap()]);
        assert_eq!(c.endpoint, "185.213.154.68:51820");
        assert!(c.preshared_key.is_none());
        assert!(c.persistent_keepalive.is_none());
        assert!(c.routes("8.8.8.8".parse().unwrap()));
        assert!(c.routes("2001:db8::1".parse().unwrap()));
        assert_eq!(c.source_for("8.8.8.8".parse().unwrap()), Some("10.68.12.34".parse().unwrap()));
    }

    #[test]
    fn parses_a_self_hosted_file() {
        let c = WgConfig::parse(SELF_HOSTED).unwrap();
        assert_eq!(c.addresses[0].prefix, 24);
        assert_eq!(c.dns.len(), 2, "the search domain is skipped");
        assert!(c.preshared_key.is_some());
        assert_eq!(c.persistent_keepalive, Some(Duration::from_secs(25)));
        assert_eq!(c.endpoint, "vpn.example.org:51820");
        assert!(c.routes("192.168.1.20".parse().unwrap()));
        assert!(!c.routes("8.8.8.8".parse().unwrap()), "split tunnel");
        assert!(c.source_for("2001:db8::1".parse().unwrap()).is_none());
    }

    #[test]
    fn round_trips_through_to_conf() {
        for text in [MULLVAD_STYLE, SELF_HOSTED] {
            let c = WgConfig::parse(text).unwrap();
            assert_eq!(WgConfig::parse(&c.to_conf()).unwrap(), c);
        }
    }

    #[test]
    fn debug_never_shows_keys() {
        let c = WgConfig::parse(SELF_HOSTED).unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("oK56DE9U") && !shown.contains("FpCyhws9"));
    }

    #[test]
    fn says_what_is_wrong() {
        assert_eq!(WgConfig::parse("hello").unwrap_err(), ConfigError::BadLine(1));
        assert_eq!(WgConfig::parse("[Peer]\n").unwrap_err(), ConfigError::NoInterface);
        let no_endpoint = SELF_HOSTED.replace("Endpoint = vpn.example.org:51820", "");
        assert_eq!(
            WgConfig::parse(&no_endpoint).unwrap_err(),
            ConfigError::Missing { section: "Peer", key: "Endpoint" }
        );
        let short_key = MULLVAD_STYLE.replace("yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=", "abc");
        assert_eq!(WgConfig::parse(&short_key).unwrap_err(), ConfigError::BadKey("PrivateKey"));
        let two = format!("{SELF_HOSTED}\n[Peer]\nPublicKey = HIgo9xNzJMWLKASShiTqIybxZ0U3wGLiUeJ1PKf8ykw=\n");
        assert_eq!(WgConfig::parse(&two).unwrap_err(), ConfigError::ManyPeers(2));
        let no_port = MULLVAD_STYLE.replace(":51820", "");
        assert!(matches!(WgConfig::parse(&no_port).unwrap_err(), ConfigError::BadValue(_)));
    }
}
