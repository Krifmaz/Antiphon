#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("packet is only {len} bytes, shorter than the {min} byte header")]
    TooShort { len: usize, min: usize },
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u8),
    #[error("unknown frame type byte {0}")]
    UnknownFrameType(u8),
    #[error(transparent)]
    Crypto(#[from] voip_crypto::CryptoError),
}
