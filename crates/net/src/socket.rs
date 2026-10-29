//! The one socket type the app's traffic goes through, so a call or a
//! transfer can be routed through the in-app VPN without the code that
//! speaks each protocol knowing or caring (spec §6 "per-mode routing").

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;

use crate::vpn::TunnelSocket;

/// Anything that sends and receives datagrams. The protocol code in this
/// crate (STUN, relay, mailbox, blob) is written against this, so it runs
/// unchanged over a plain [`UdpSocket`] (the relay node, tests) or an
/// [`AppSocket`] that may be inside the tunnel.
pub trait Datagram: Send + Sync {
    fn send_to(&self, buf: &[u8], target: SocketAddr) -> impl Future<Output = io::Result<usize>> + Send;
    fn recv_from(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<(usize, SocketAddr)>> + Send;
}

impl Datagram for UdpSocket {
    fn send_to(&self, buf: &[u8], target: SocketAddr) -> impl Future<Output = io::Result<usize>> + Send {
        UdpSocket::send_to(self, buf, target)
    }
    fn recv_from(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<(usize, SocketAddr)>> + Send {
        UdpSocket::recv_from(self, buf)
    }
}

impl<T: Datagram + ?Sized> Datagram for Arc<T> {
    fn send_to(&self, buf: &[u8], target: SocketAddr) -> impl Future<Output = io::Result<usize>> + Send {
        (**self).send_to(buf, target)
    }
    fn recv_from(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<(usize, SocketAddr)>> + Send {
        (**self).recv_from(buf)
    }
}

/// A UDP socket that is either an ordinary one or one inside the VPN
/// tunnel. A tunneled socket never falls back to the real connection: if
/// the tunnel goes down, it fails (spec §6 "kill switch").
pub enum AppSocket {
    Direct(UdpSocket),
    Tunnel(TunnelSocket),
}

impl From<UdpSocket> for AppSocket {
    fn from(socket: UdpSocket) -> Self {
        Self::Direct(socket)
    }
}

impl AppSocket {
    /// An ordinary socket, outside any tunnel.
    pub async fn bind_direct(addr: SocketAddr) -> io::Result<Self> {
        Ok(Self::Direct(UdpSocket::bind(addr).await?))
    }

    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        match self {
            Self::Direct(s) => s.send_to(buf, target).await,
            Self::Tunnel(s) => s.send_to(buf, target).await,
        }
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self {
            Self::Direct(s) => s.recv_from(buf).await,
            Self::Tunnel(s) => s.recv_from(buf).await,
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Direct(s) => s.local_addr(),
            Self::Tunnel(s) => Ok(s.local_addr()),
        }
    }

    pub fn is_tunneled(&self) -> bool {
        matches!(self, Self::Tunnel(_))
    }

    /// The OS socket, for options that only mean something on one.
    pub fn as_udp(&self) -> Option<&UdpSocket> {
        match self {
            Self::Direct(s) => Some(s),
            Self::Tunnel(_) => None,
        }
    }
}

impl Datagram for AppSocket {
    fn send_to(&self, buf: &[u8], target: SocketAddr) -> impl Future<Output = io::Result<usize>> + Send {
        AppSocket::send_to(self, buf, target)
    }
    fn recv_from(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<(usize, SocketAddr)>> + Send {
        AppSocket::recv_from(self, buf)
    }
}
