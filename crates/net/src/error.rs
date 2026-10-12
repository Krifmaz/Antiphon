#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Protocol(#[from] voip_protocol::ProtocolError),
    #[error(transparent)]
    Crypto(#[from] voip_crypto::CryptoError),
    #[error("handshake payload was not a 32-byte X25519 public key")]
    BadHandshake,
    #[error("no STUN server answered, so the public address is unknown")]
    StunFailed,
    #[error("the peer never answered -- both sides have to be punching at roughly the same time")]
    PunchTimeout,
    #[error("no router on this network answered a NAT-PMP request")]
    PortMapUnavailable,
    #[error("{}", .0.explain())]
    PortMapRefused(crate::port_map::ResultCode),
}
