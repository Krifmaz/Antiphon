//! Forward error correction for audio (spec §2, layer one).
//!
//! > "Sends a low-quality backup copy of each moment of audio alongside the
//! > real one. Small gaps get filled instantly with no delay."
//!
//! Every audio frame carries the *previous* frame in a cheap, lossy form.
//! Lose one frame and the next one repairs it, with no round trip and no
//! added latency -- which is the whole point, and what separates this from
//! the retransmit layer above it.
//!
//! The backup is the previous frame decimated by [`DECIMATE`] and
//! quantised to 8 bits: roughly a twelfth the size of the primary. It
//! sounds muffled, because it is. That is the correct trade -- a dull
//! hundred milliseconds is far less noticeable than a hole, and paying
//! full price for a copy that is usually thrown away would double the
//! bandwidth of every call to insure against the minority of frames that
//! go missing.
//!
//! Only single-frame losses are repaired. Two in a row and the second has
//! no surviving carrier, which is what the retransmit and concealment
//! layers are for.
//!
//! # Why the frame carries its own sequence number
//!
//! The jitter buffer used to key on the crypto session's message counter,
//! which was free and already monotonic. That works right up until you try
//! to *resend* a lost frame: the resend has to claim the same position in
//! the stream, but the crypto counter is the AEAD nonce, and reusing a
//! nonce with the same key is catastrophic -- it leaks the XOR of two
//! plaintexts and destroys the integrity guarantee.
//!
//! So the application's frame sequence lives here, inside the encrypted
//! payload, independent of the counter on the outside. A resend is a *new*
//! crypto counter carrying an *old* sequence number, which is exactly what
//! is needed and impossible to express otherwise.
//!
//! Wire format:
//!
//! ```text
//! [version: u8 = 1]
//! [sequence: u32 BE]           // application frame number
//! [primary_len: u16 BE]        // f32 samples
//! [primary: f32 LE * primary_len]
//! [backup_original_len: u16 BE] // 0 when there is no backup
//! [backup_len: u16 BE]          // i8 samples actually carried
//! [backup: i8 * backup_len]
//! ```

pub const VERSION: u8 = 1;

/// Keep one sample in this many for the backup copy. At 48 kHz this leaves
/// the backup with an effective bandwidth of about 8 kHz -- muddy, but
/// entirely intelligible for speech.
pub const DECIMATE: usize = 3;

const HEADER_LEN: usize = 1 + 4 + 2;

/// A decoded audio payload.
pub struct Decoded {
    /// This frame's position in the stream, independent of the crypto
    /// counter it arrived under.
    pub sequence: u32,
    /// This frame's audio.
    pub primary: Vec<f32>,
    /// The previous frame, reconstructed from its backup copy, if one was
    /// carried.
    pub backup: Option<Vec<f32>>,
}

/// Builds an audio payload carrying `primary`, plus a backup copy of
/// `previous` when there is one.
pub fn encode(sequence: u32, primary: &[f32], previous: Option<&[f32]>) -> Vec<u8> {
    let backup: Vec<i8> = previous.map(quantize).unwrap_or_default();
    let original_len = previous.map(<[f32]>::len).unwrap_or(0);

    let mut out = Vec::with_capacity(HEADER_LEN + primary.len() * 4 + 4 + backup.len());
    out.push(VERSION);
    out.extend_from_slice(&sequence.to_be_bytes());
    out.extend_from_slice(&(primary.len() as u16).to_be_bytes());
    for s in primary {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out.extend_from_slice(&(original_len as u16).to_be_bytes());
    out.extend_from_slice(&(backup.len() as u16).to_be_bytes());
    out.extend(backup.iter().map(|b| *b as u8));
    out
}

/// Parses a payload produced by [`encode`].
///
/// Returns `None` for anything malformed rather than panicking or
/// returning partial audio: a corrupt frame should be dropped like a lost
/// one, and the layers above already handle a gap.
pub fn decode(bytes: &[u8]) -> Option<Decoded> {
    if bytes.len() < HEADER_LEN || bytes[0] != VERSION {
        return None;
    }
    let sequence = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
    let primary_len = u16::from_be_bytes([bytes[5], bytes[6]]) as usize;
    let primary_bytes = primary_len.checked_mul(4)?;
    let primary_end = HEADER_LEN.checked_add(primary_bytes)?;
    if bytes.len() < primary_end + 4 {
        return None;
    }

    let mut primary = Vec::with_capacity(primary_len);
    for chunk in bytes[HEADER_LEN..primary_end].chunks_exact(4) {
        primary.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }

    let original_len = u16::from_be_bytes([bytes[primary_end], bytes[primary_end + 1]]) as usize;
    let backup_len = u16::from_be_bytes([bytes[primary_end + 2], bytes[primary_end + 3]]) as usize;
    let backup_start = primary_end + 4;
    let backup_end = backup_start.checked_add(backup_len)?;
    if bytes.len() < backup_end {
        return None;
    }

    let backup = if original_len == 0 || backup_len == 0 {
        None
    } else {
        let quantized: Vec<i8> = bytes[backup_start..backup_end].iter().map(|b| *b as i8).collect();
        Some(dequantize(&quantized, original_len))
    };

    Some(Decoded { sequence, primary, backup })
}

/// Decimate and quantise to 8 bits.
fn quantize(samples: &[f32]) -> Vec<i8> {
    samples
        .iter()
        .step_by(DECIMATE)
        .map(|s| (s.clamp(-1.0, 1.0) * 127.0).round() as i8)
        .collect()
}

/// Expand back to `target_len`, interpolating between the kept samples.
///
/// Linear interpolation rather than sample-and-hold: holding produces
/// stair-steps, which are broadband noise and audibly worse than the
/// smoothing this does.
fn dequantize(backup: &[i8], target_len: usize) -> Vec<f32> {
    if backup.is_empty() || target_len == 0 {
        return vec![0.0; target_len];
    }
    let last = *backup.last().unwrap() as f32 / 127.0;
    (0..target_len)
        .map(|i| {
            let pos = i as f32 / DECIMATE as f32;
            let idx = pos.floor() as usize;
            let frac = pos - idx as f32;
            let a = backup.get(idx).map(|v| *v as f32 / 127.0).unwrap_or(last);
            let b = backup.get(idx + 1).map(|v| *v as f32 / 127.0).unwrap_or(a);
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(len: usize) -> Vec<f32> {
        (0..len).map(|i| (i as f32 * 0.1).sin() * 0.5).collect()
    }

    #[test]
    fn the_sequence_number_survives_the_round_trip() {
        // A resend reuses this while getting a fresh crypto counter, which
        // is the entire reason it exists.
        let decoded = decode(&encode(4_000_000_000, &tone(48), None)).unwrap();
        assert_eq!(decoded.sequence, 4_000_000_000);
    }

    #[test]
    fn primary_audio_survives_exactly() {
        let primary = tone(480);
        let decoded = decode(&encode(7, &primary, None)).unwrap();

        // The real copy is bit-exact; only the backup is lossy.
        assert_eq!(decoded.primary, primary);
        assert!(decoded.backup.is_none());
    }

    #[test]
    fn a_backup_copy_reconstructs_the_previous_frame_approximately() {
        let previous = tone(480);
        let primary = tone(480);

        let decoded = decode(&encode(7, &primary, Some(&previous))).unwrap();
        let backup = decoded.backup.expect("backup should be present");

        assert_eq!(backup.len(), previous.len());

        // Lossy, but close enough to stand in for the real thing.
        let error: f32 = previous
            .iter()
            .zip(&backup)
            .map(|(a, b)| (a - b).abs())
            .sum::<f32>()
            / previous.len() as f32;
        assert!(error < 0.05, "mean absolute error {error} is too high to be usable");
    }

    #[test]
    fn the_backup_is_much_smaller_than_the_primary() {
        let frame = tone(480);
        let with_backup = encode(7, &frame, Some(&frame));
        let without = encode(7, &frame, None);

        let overhead = with_backup.len() - without.len();
        let primary_bytes = frame.len() * 4;
        assert!(
            overhead * 6 < primary_bytes,
            "backup added {overhead} bytes against a {primary_bytes}-byte primary -- \
             that is too close to just sending it twice"
        );
    }

    #[test]
    fn silence_reconstructs_as_silence() {
        let previous = vec![0.0f32; 480];
        let decoded = decode(&encode(7, &tone(480), Some(&previous))).unwrap();
        let backup = decoded.backup.unwrap();

        assert!(backup.iter().all(|s| s.abs() < 1e-3), "silence gained noise");
    }

    #[test]
    fn loud_samples_are_clamped_not_wrapped() {
        // A sample beyond full scale must saturate. Wrapping would turn a
        // loud moment into a burst of noise at the opposite polarity.
        let previous = vec![4.0f32; 30];
        let decoded = decode(&encode(7, &tone(30), Some(&previous))).unwrap();
        let backup = decoded.backup.unwrap();

        assert!(backup.iter().all(|s| *s > 0.9), "clipping wrapped: {backup:?}");
    }

    #[test]
    fn an_empty_frame_round_trips() {
        let decoded = decode(&encode(7, &[], None)).unwrap();
        assert!(decoded.primary.is_empty());
        assert!(decoded.backup.is_none());
    }

    #[test]
    fn a_truncated_payload_is_rejected_rather_than_panicking() {
        let full = encode(7, &tone(480), Some(&tone(480)));
        for cut in [0, 1, 2, 3, 10, full.len() / 2, full.len() - 1] {
            assert!(decode(&full[..cut]).is_none(), "accepted a payload cut at {cut}");
        }
    }

    #[test]
    fn a_payload_claiming_more_audio_than_it_carries_is_rejected() {
        let mut bytes = encode(7, &tone(48), None);
        bytes[5..7].copy_from_slice(&u16::MAX.to_be_bytes());

        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn an_unknown_version_is_rejected() {
        let mut bytes = encode(7, &tone(48), None);
        bytes[0] = 99;

        assert!(decode(&bytes).is_none());
    }
}
