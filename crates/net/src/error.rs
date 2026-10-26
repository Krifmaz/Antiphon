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
    #[error("the mailbox refused: {}", .0.describe())]
    MailboxRefused(crate::mailbox::Refusal),
    #[error("the mailbox did not answer")]
    MailboxTimeout,
    #[error("{}", .0.describe())]
    FileRefused(crate::blob::Refusal),
    #[error("nobody holding that file answered")]
    FileHolderTimeout,
    #[error("the group-call server refused: {}", .0.describe())]
    SfuRefused(crate::sfu::Refusal),
    #[error("the group-call server did not answer")]
    SfuTimeout,
    #[error("a group-call frame must be 1 to {max} bytes, not {0}", max = crate::sfu::MAX_MEDIA_PAYLOAD)]
    SfuPayloadSize(usize),
    #[error("{}", .0.explain())]
    PortMapRefused(crate::port_map::ResultCode),
}
