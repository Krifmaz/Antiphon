use crate::error::ProtocolError;

pub const VERSION: u8 = 1;

/// Header layout (big-endian):
/// `[version: u8][frame_type: u8][counter: u64][payload_len: u32][payload...]`
const HEADER_LEN: usize = 1 + 1 + 8 + 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// Handshake message: the encrypted-frame counter is unused (always 0)
    /// and the payload is a raw X25519 public key, sent before a [`Session`]
    /// exists (spec §3's ephemeral key exchange).
    ///
    /// [`Session`]: antiphon_crypto::Session
    Handshake,
    /// An AEAD-sealed audio frame: `payload` is ciphertext from [`antiphon_crypto::Session::encrypt`].
    Audio,
    /// An AEAD-sealed control message (mute state, PTT state, file-transfer
    /// offers/acks, etc.) -- small, infrequent, JSON-shaped.
    Control,
    /// An AEAD-sealed file-transfer chunk (spec §5): binary, not JSON, and
    /// kept off the `Control` type so a multi-megabyte transfer's volume of
    /// packets can't be confused with (or crowd out log/debugging visibility
    /// into) actual control traffic. `payload` is ciphertext whose plaintext
    /// is `[transfer_id: u64 BE][chunk_index: u32 BE][chunk bytes...]`.
    File,
    /// An AEAD-sealed video datagram: one part of one patch of a camera or
    /// screen picture (`antiphon_video::wire` defines the plaintext). Its
    /// own type, not `File`'s or `Control`'s, so a burst of video can be
    /// told apart, and a peer from before video drops it as unknown rather
    /// than misreading it.
    Video,
}

impl FrameType {
    fn to_byte(self) -> u8 {
        match self {
            FrameType::Handshake => 0,
            FrameType::Audio => 1,
            FrameType::Control => 2,
            FrameType::File => 3,
            FrameType::Video => 4,
        }
    }

    fn from_byte(b: u8) -> Result<Self, ProtocolError> {
        match b {
            0 => Ok(FrameType::Handshake),
            1 => Ok(FrameType::Audio),
            2 => Ok(FrameType::Control),
            3 => Ok(FrameType::File),
            4 => Ok(FrameType::Video),
            other => Err(ProtocolError::UnknownFrameType(other)),
        }
    }
}

/// A decoded packet straight off the wire, before decryption.
pub struct RawFrame {
    pub frame_type: FrameType,
    pub counter: u64,
    pub payload: Vec<u8>,
}

pub fn encode(frame_type: FrameType, counter: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.push(VERSION);
    out.push(frame_type.to_byte());
    out.extend_from_slice(&counter.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn decode(bytes: &[u8]) -> Result<RawFrame, ProtocolError> {
    if bytes.len() < HEADER_LEN {
        return Err(ProtocolError::TooShort { len: bytes.len(), min: HEADER_LEN });
    }
    let version = bytes[0];
    if version != VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }
    let frame_type = FrameType::from_byte(bytes[1])?;
    let counter = u64::from_be_bytes(bytes[2..10].try_into().unwrap());
    let payload_len = u32::from_be_bytes(bytes[10..14].try_into().unwrap()) as usize;

    let payload = bytes.get(HEADER_LEN..HEADER_LEN + payload_len)
        .ok_or(ProtocolError::TooShort { len: bytes.len(), min: HEADER_LEN + payload_len })?
        .to_vec();

    Ok(RawFrame { frame_type, counter, payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let encoded = encode(FrameType::Audio, 42, b"hello");
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.frame_type, FrameType::Audio);
        assert_eq!(decoded.counter, 42);
        assert_eq!(decoded.payload, b"hello");
    }

    #[test]
    fn control_frame_type_round_trips_too() {
        let encoded = encode(FrameType::Control, 7, br#"{"display_name":"Alice"}"#);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.frame_type, FrameType::Control);
        assert_eq!(decoded.counter, 7);
    }

    #[test]
    fn file_frame_type_round_trips_too() {
        let encoded = encode(FrameType::File, 3, b"chunk bytes");
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.frame_type, FrameType::File);
        assert_eq!(decoded.counter, 3);
        assert_eq!(decoded.payload, b"chunk bytes");
    }

    #[test]
    fn video_frame_type_round_trips_too() {
        let encoded = encode(FrameType::Video, 9, b"patch part");
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.frame_type, FrameType::Video);
        assert_eq!(decoded.payload, b"patch part");
    }

    #[test]
    fn rejects_truncated_packets() {
        let encoded = encode(FrameType::Audio, 1, b"hello world");
        let truncated = &encoded[..encoded.len() - 3];
        assert!(decode(truncated).is_err());
    }
}
