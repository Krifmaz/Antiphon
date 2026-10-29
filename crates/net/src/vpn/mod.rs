//! Bring your own VPN (spec §6): import a WireGuard file, run it inside the
//! app, and choose which kinds of traffic go through it.
//!
//! - [`config`] parses wg-quick `.conf` files.
//! - [`noise`] is the WireGuard protocol.
//! - [`packet`] builds and parses the IP/UDP packets carried inside it.
//! - [`tunnel`] runs one tunnel and the sockets inside it.
//! - [`Router`] decides, per kind of traffic, whether a new socket is
//!   direct or tunneled, and enforces the kill switch.

pub mod config;
pub mod noise;
pub mod packet;
pub mod tunnel;

use std::net::SocketAddr;
use std::sync::Arc;

pub use config::{ConfigError, IpNet, WgConfig};
pub use tunnel::{Tunnel, TunnelSocket, TunnelStatus};

use crate::error::NetError;
use crate::socket::AppSocket;

/// The kinds of traffic that can be routed separately (spec §6 "per-mode
/// routing -- file transfers through the VPN while voice stays direct, or
/// the reverse").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrafficKind {
    /// Calls: the call socket, STUN and relay for it, and anything that
    /// rides inside the call (in-call file chunks, control messages).
    Voice,
    /// Shared files outside a call: seeding, pinning, swarm downloads.
    Files,
    /// Offline messages: mailbox deposits and fetches.
    Messages,
}

/// Which kinds go through the tunnel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Routes {
    pub voice: bool,
    pub files: bool,
    pub messages: bool,
}

impl Routes {
    pub fn includes(&self, kind: TrafficKind) -> bool {
        match kind {
            TrafficKind::Voice => self.voice,
            TrafficKind::Files => self.files,
            TrafficKind::Messages => self.messages,
        }
    }
}

/// Hands out sockets according to the routing choice.
#[derive(Clone, Default)]
pub struct Router {
    tunnel: Option<Arc<Tunnel>>,
    routes: Routes,
    kill_switch: bool,
}

impl Router {
    /// Everything direct: what the app does with the VPN off.
    pub fn direct() -> Self {
        Self::default()
    }

    pub fn new(tunnel: Option<Arc<Tunnel>>, routes: Routes, kill_switch: bool) -> Self {
        Self { tunnel, routes, kill_switch }
    }

    pub fn tunnel(&self) -> Option<&Arc<Tunnel>> {
        self.tunnel.as_ref()
    }

    /// Whether a new socket of this kind would be inside the tunnel.
    pub fn tunnels(&self, kind: TrafficKind) -> bool {
        self.routes.includes(kind) && self.tunnel.as_ref().is_some_and(|t| t.is_running())
    }

    /// A socket for `kind`, on `port` (0 for any).
    ///
    /// Routed through the tunnel when the user chose that for this kind and
    /// the tunnel is running. When they chose it but the tunnel is not
    /// running, the kill switch decides: on, this refuses
    /// ([`NetError::KillSwitch`]) rather than quietly sending from the real
    /// address the user was trying to hide; off, the socket is direct.
    pub async fn bind(&self, kind: TrafficKind, port: u16) -> Result<AppSocket, NetError> {
        if self.routes.includes(kind) {
            match &self.tunnel {
                Some(t) if t.is_running() => return Ok(AppSocket::Tunnel(t.bind(port)?)),
                _ if self.kill_switch => return Err(NetError::KillSwitch),
                _ => {}
            }
        }
        Ok(AppSocket::bind_direct(SocketAddr::from(([0, 0, 0, 0], port))).await?)
    }

    /// Resolves `host:port` the way a socket of `kind` would reach it:
    /// through the VPN's DNS when tunneled, so the name looked up does not
    /// leak to the local network either.
    pub async fn resolve(&self, kind: TrafficKind, host_port: &str) -> Result<SocketAddr, NetError> {
        if self.routes.includes(kind) {
            match &self.tunnel {
                Some(t) if t.is_running() => return Ok(t.resolve(host_port).await?),
                _ if self.kill_switch => return Err(NetError::KillSwitch),
                _ => {}
            }
        }
        tokio::net::lookup_host(host_port).await?.next().ok_or_else(|| {
            NetError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("{host_port} did not resolve")))
        })
    }
}

#[cfg(test)]
mod tests;
