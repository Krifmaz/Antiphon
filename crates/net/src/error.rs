#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Protocol(#[from] antiphon_protocol::ProtocolError),
    #[error(transparent)]
    Crypto(#[from] antiphon_crypto::CryptoError),
    #[error("handshake payload was not a 32-byte X25519 public key")]
    BadHandshake,
    #[error("no STUN server answered, so the public address is unknown")]
    StunFailed,
    #[error("the peer never answered -- both sides have to be punching at roughly the same time")]
    PunchTimeout,
    #[error("no router on this network answered a NAT-PMP request")]
    PortMapUnavailable,
    #[error("the relay has nobody waiting under that token -- they are not listening, or use a different relay")]
    RelayNotFound,
    #[error("the relay did not answer")]
    RelayTimeout,
    /// An anonymity path was asked for with fewer than two or more than three hops.
    #[error("an anonymity circuit needs two or three hops")]
    OnionPathLength,
    /// A hop answered the circuit-building message with a refusal.
    #[error("a hop refused the circuit: it does not offer hops, is full, or will not exit there")]
    OnionRefused,
    /// A hop on the circuit went quiet before the circuit was built.
    #[error("the circuit could not be built: a hop did not answer")]
    OnionTimeout,
    #[error("the mailbox refused: {}", .0.describe())]
    MailboxRefused(crate::mailbox::Refusal),
    #[error("the mailbox did not answer")]
    MailboxTimeout,
    #[error("{}", .0.describe())]
    FileRefused(crate::blob::Refusal),
    #[error("nobody holding that file answered")]
    FileHolderTimeout,
    /// The SFU answered a join or frame with a refusal.
    #[error("the group-call server refused: {}", .0.describe())]
    SfuRefused(crate::sfu::Refusal),
    /// The SFU did not answer a join in time.
    #[error("the group-call server did not answer")]
    SfuTimeout,
    /// A media frame was empty or too large for one SFU datagram.
    #[error("a group-call frame must be 1 to {max} bytes, not {0}", max = crate::sfu::MAX_MEDIA_PAYLOAD)]
    SfuPayloadSize(usize),
    /// The relay answered a federation request saying it does not federate.
    #[error("that relay does not peer with other relays")]
    FederationRefused,
    /// The relay did not answer a federation request in time.
    #[error("the relay did not say which relays it peers with")]
    FederationTimeout,
    #[error("the VPN is down and the kill switch is on, so this was not sent over your real connection")]
    KillSwitch,
    #[error("none of the DHT bootstrap nodes answered")]
    DhtUnreachable,
    #[error("{}", .0.explain())]
    PortMapRefused(crate::port_map::ResultCode),
}
