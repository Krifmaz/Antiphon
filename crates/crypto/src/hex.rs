//! Hex text for keys, hashes and ids: the one encoder and decoder every
//! crate uses.
//!
//! Keys, digests and ids appear as lower-case hex in invite links, config
//! files, logs and the relay's admin commands. Each crate used to carry its
//! own copy of these few lines (twenty-odd of them, some building a
//! throwaway `String` per byte); they all call here now, so a link written
//! by one part of the app is read back by any other the same way.

/// The digits [`encode`] writes, indexed by nibble.
const DIGITS: &[u8; 16] = b"0123456789abcdef";

/// `bytes` as lower-case hex, two characters per byte.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// The value of one hex digit, either case; `None` for anything else.
fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Hex text back to bytes, upper or lower case.
///
/// `None` for an odd length or any character that is not a hex digit --
/// stricter than the `u8::from_str_radix` loops this replaced, which took
/// a sign (`"+f"`) as a digit pair.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let text = text.as_bytes();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.chunks_exact(2).map(|pair| Some(nibble(pair[0])? << 4 | nibble(pair[1])?)).collect()
}

/// Exactly `N` bytes of hex text (`2 * N` characters), as an array: the
/// shape of every key and digest the app stores.
pub fn decode_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    let text = text.as_bytes();
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (byte, pair) in out.iter_mut().zip(text.chunks_exact(2)) {
        *byte = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let bytes: Vec<u8> = (0..=255).collect();
        let text = encode(&bytes);
        assert_eq!(text.len(), 512);
        assert!(text.starts_with("000102"));
        assert!(text.ends_with("fdfeff"));
        assert_eq!(decode(&text).unwrap(), bytes);
    }

    #[test]
    fn either_case_decodes() {
        assert_eq!(decode("aBcD"), Some(vec![0xab, 0xcd]));
        assert_eq!(decode_array::<2>("ABcd"), Some([0xab, 0xcd]));
    }

    #[test]
    fn junk_is_refused() {
        assert_eq!(decode("abc"), None, "odd length");
        assert_eq!(decode("+f"), None, "a sign is not a digit");
        assert_eq!(decode("zz"), None);
        assert_eq!(decode("é0"), None, "non-ASCII");
        assert_eq!(decode_array::<2>("abcdef"), None, "wrong length");
        assert_eq!(decode_array::<1>("g0"), None);
    }

    #[test]
    fn empty_is_empty() {
        assert_eq!(encode(&[]), "");
        assert_eq!(decode(""), Some(vec![]));
        assert_eq!(decode_array::<0>(""), Some([]));
    }
}
