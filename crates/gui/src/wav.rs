//! Minimal WAV output for the Replay feature (spec §2 "Replay") -- 16-bit
//! PCM mono is universally readable, and the RIFF header is 44 fixed bytes,
//! so hand-rolling it beats pulling in an audio-container dependency for
//! one write path.

pub fn write_wav_16bit_mono(path: &std::path::Path, sample_rate: u32, samples: &[f32]) -> anyhow::Result<()> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes());
    }

    std::fs::write(path, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_valid_header_and_correct_length() {
        let dir = std::env::temp_dir();
        let path = dir.join("voip_wav_test.wav");
        let samples = vec![0.0f32, 0.5, -0.5, 1.0, -1.0];
        write_wav_16bit_mono(&path, 48000, &samples).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(bytes.len(), 44 + samples.len() * 2);
        // Sample rate field at offset 24.
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 48000);
        // Full-scale positive sample maps to i16::MAX, not wrapped.
        assert_eq!(i16::from_le_bytes(bytes[50..52].try_into().unwrap()), i16::MAX);
        let _ = std::fs::remove_file(&path);
    }
}
