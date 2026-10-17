//! Soundboard (spec §13: "reactions, custom emoji, soundboard -- the
//! soundboard is nearly free given the audio chain already exists").
//!
//! The spec is right that it's nearly free, but "nearly" is doing some
//! work. The chain gives you playback; what it does not give you is the
//! decision about *where* a clip goes, and that decision is the whole
//! feature:
//!
//! - Mixed into the **outgoing** stream, so the other side hears it. This
//!   is what people mean by a soundboard.
//! - Played into your **own** output only, which is a private cue and
//!   nobody else's business.
//!
//! Both are offered, because they are genuinely different intentions and
//! guessing wrong is the difference between a joke landing and blasting a
//! sound at someone who did not expect it. The UI gives them separate
//! buttons rather than one button and a mode switch -- a hidden toggle is
//! exactly how you send a sound you meant to preview.
//!
//! Which destination a clip goes to is carried as a plain `bool` across
//! the callback boundary, since Slint and Rust share no enum; the two
//! buttons are the readable form of that choice.
//!
//! Clips are WAV, mono or stereo, loaded from disk and resampled to the
//! call's rate. Only WAV: pulling in a decoder for compressed formats is a
//! dependency and an attack surface for a feature whose whole appeal is
//! that it costs almost nothing.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A clip the user has added.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SoundClip {
    pub name: String,
    pub path: String,
    /// Playback level in dB, so a clip recorded hot doesn't have to be
    /// re-edited to sit alongside the others.
    #[serde(default)]
    pub gain_db: f32,
}

/// Longest clip accepted, in seconds.
///
/// A soundboard is for punctuation. Anything longer is really "play a file
/// down the call", which is a different feature with different consent
/// questions attached.
pub const MAX_CLIP_SECONDS: f32 = 15.0;

/// A decoded clip, ready to mix.
///
/// `Debug` prints the sample count rather than the samples: a few seconds
/// of audio is tens of thousands of floats, and dumping them into a panic
/// message helps nobody.
pub struct LoadedClip {
    pub sample_rate: u32,
    pub samples: Vec<f32>,
}

/// Reads a mono or stereo 16-bit PCM WAV file.
///
/// Hand-rolled rather than pulling in a WAV crate: the app already writes
/// this exact format in `wav.rs`, the header is a dozen fields, and a
/// decoder is a parser exposed to files from other people.
pub fn load_wav(path: &Path) -> Result<LoadedClip, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("could not read the file: {e}"))?;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("that is not a WAV file".to_string());
    }

    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at = |at: usize| {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    };

    // Walk the chunk list rather than assuming `fmt ` and `data` sit at
    // fixed offsets: plenty of real files carry a LIST or fact chunk in
    // between, and hardcoding 44 would reject them for no reason.
    let mut pos = 12usize;
    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits = 0u16;
    let mut data: Option<(usize, usize)> = None;

    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32_at(pos + 4) as usize;
        let body = pos + 8;
        if body + size > bytes.len() {
            break;
        }
        if id == b"fmt " && size >= 16 {
            channels = u16_at(body + 2);
            sample_rate = u32_at(body + 4);
            bits = u16_at(body + 14);
        } else if id == b"data" {
            data = Some((body, size));
        }
        // Chunks are word-aligned, so an odd size is followed by a pad byte.
        pos = body + size + (size & 1);
    }

    let Some((data_at, data_len)) = data else {
        return Err("that WAV file has no audio in it".to_string());
    };
    if bits != 16 {
        return Err(format!("only 16-bit WAV files are supported, this one is {bits}-bit"));
    }
    if channels == 0 || channels > 2 {
        return Err("only mono or stereo WAV files are supported".to_string());
    }
    if sample_rate == 0 {
        return Err("that WAV file does not say what rate it was recorded at".to_string());
    }

    // Stereo is folded to mono: the call itself is mono, so keeping two
    // channels would only mean discarding one later anyway.
    let mut samples = Vec::with_capacity(data_len / 2 / channels as usize);
    let frame_bytes = 2 * channels as usize;
    for frame in bytes[data_at..data_at + data_len].chunks_exact(frame_bytes) {
        let mut sum = 0.0f32;
        for ch in 0..channels as usize {
            let raw = i16::from_le_bytes([frame[ch * 2], frame[ch * 2 + 1]]);
            sum += raw as f32 / 32768.0;
        }
        samples.push(sum / channels as f32);
    }

    let seconds = samples.len() as f32 / sample_rate as f32;
    if seconds > MAX_CLIP_SECONDS {
        return Err(format!(
            "that clip is {seconds:.0} seconds -- the limit is {MAX_CLIP_SECONDS:.0}"
        ));
    }

    Ok(LoadedClip { sample_rate, samples })
}

impl std::fmt::Debug for LoadedClip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedClip")
            .field("sample_rate", &self.sample_rate)
            .field("samples", &self.samples.len())
            .finish()
    }
}

/// Resamples to `target_rate` by linear interpolation.
///
/// Good enough for a sound effect and a handful of lines of code. Anything
/// better (a windowed sinc) would matter for music and is not worth the
/// complexity for a clip that plays for two seconds.
pub fn resample(clip: &LoadedClip, target_rate: u32) -> Vec<f32> {
    if clip.sample_rate == target_rate || clip.samples.is_empty() {
        return clip.samples.clone();
    }
    let ratio = target_rate as f64 / clip.sample_rate as f64;
    let out_len = ((clip.samples.len() as f64) * ratio).round() as usize;
    (0..out_len)
        .map(|i| {
            let src = i as f64 / ratio;
            let idx = src.floor() as usize;
            let frac = (src - idx as f64) as f32;
            let a = clip.samples.get(idx).copied().unwrap_or(0.0);
            let b = clip.samples.get(idx + 1).copied().unwrap_or(a);
            a + (b - a) * frac
        })
        .collect()
}

/// Applies a clip's gain and clamps, so a hot clip cannot clip the mix.
pub fn apply_gain(samples: &mut [f32], gain_db: f32) {
    let lin = 10f32.powf(gain_db / 20.0);
    for s in samples.iter_mut() {
        *s = (*s * lin).clamp(-1.0, 1.0);
    }
}

/// Where clips are copied to when added, so a soundboard keeps working
/// after the original file is moved or deleted.
pub fn clips_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("sounds")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a 16-bit PCM WAV in memory.
    fn wav(sample_rate: u32, channels: u16, frames: usize) -> Vec<u8> {
        let data_len = frames * channels as usize * 2;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
        b.extend_from_slice(b"WAVE");
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // PCM
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&sample_rate.to_le_bytes());
        b.extend_from_slice(&(sample_rate * channels as u32 * 2).to_le_bytes());
        b.extend_from_slice(&(channels * 2).to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data_len as u32).to_le_bytes());
        for i in 0..frames {
            for _ in 0..channels {
                let v = ((i as f32 * 0.05).sin() * 16000.0) as i16;
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        b
    }

    fn write_temp(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("antiphon-sb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_mono_wav_loads() {
        let path = write_temp("mono.wav", &wav(48_000, 1, 4_800));
        let clip = load_wav(&path).unwrap();

        assert_eq!(clip.sample_rate, 48_000);
        assert_eq!(clip.samples.len(), 4_800);
        assert!(clip.samples.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn a_stereo_wav_is_folded_to_mono() {
        let path = write_temp("stereo.wav", &wav(48_000, 2, 1_000));
        let clip = load_wav(&path).unwrap();

        // The call is mono, so keeping both channels would only mean
        // throwing one away later.
        assert_eq!(clip.samples.len(), 1_000);
    }

    #[test]
    fn a_wav_with_an_extra_chunk_before_the_data_still_loads() {
        // Real files carry LIST/fact chunks. Assuming data starts at byte
        // 44 would reject them for no reason.
        let mut bytes = wav(48_000, 1, 100);
        let mut spliced = bytes[..12].to_vec();
        spliced.extend_from_slice(b"LIST");
        spliced.extend_from_slice(&4u32.to_le_bytes());
        spliced.extend_from_slice(b"INFO");
        spliced.extend_from_slice(&bytes[12..]);
        bytes = spliced;

        let path = write_temp("chunked.wav", &bytes);
        assert!(load_wav(&path).is_ok());
    }

    #[test]
    fn a_non_wav_file_is_refused_with_a_readable_reason() {
        let path = write_temp("notawav.txt", b"this is not audio");
        let err = load_wav(&path).unwrap_err();

        assert!(err.contains("not a WAV"), "unhelpful error: {err}");
    }

    #[test]
    fn a_clip_past_the_length_limit_is_refused() {
        // A soundboard is for punctuation; longer is really "play a file
        // down the call", which is a different feature.
        let frames = (48_000.0 * (MAX_CLIP_SECONDS + 2.0)) as usize;
        let path = write_temp("long.wav", &wav(48_000, 1, frames));

        let err = load_wav(&path).unwrap_err();
        assert!(err.contains("limit"), "unhelpful error: {err}");
    }

    #[test]
    fn an_eight_bit_wav_is_refused_rather_than_played_as_noise() {
        let mut bytes = wav(48_000, 1, 100);
        // Rewrite bits-per-sample to 8.
        bytes[34..36].copy_from_slice(&8u16.to_le_bytes());
        let path = write_temp("eightbit.wav", &bytes);

        let err = load_wav(&path).unwrap_err();
        assert!(err.contains("16-bit"), "unhelpful error: {err}");
    }

    #[test]
    fn a_truncated_wav_is_an_error_not_a_panic() {
        let full = wav(48_000, 1, 500);
        for cut in [0, 12, 30, 44, full.len() / 2] {
            let path = write_temp("cut.wav", &full[..cut]);
            assert!(load_wav(&path).is_err(), "accepted a file cut at {cut}");
        }
    }

    #[test]
    fn resampling_up_and_down_keeps_the_duration() {
        let clip = LoadedClip {
            sample_rate: 24_000,
            samples: (0..2_400).map(|i| (i as f32 * 0.01).sin()).collect(),
        };

        let up = resample(&clip, 48_000);
        assert!((up.len() as i32 - 4_800).abs() <= 1, "got {} samples", up.len());

        let down = resample(&clip, 12_000);
        assert!((down.len() as i32 - 1_200).abs() <= 1, "got {} samples", down.len());
    }

    #[test]
    fn resampling_to_the_same_rate_is_a_passthrough() {
        let clip = LoadedClip { sample_rate: 48_000, samples: vec![0.1, 0.2, 0.3] };
        assert_eq!(resample(&clip, 48_000), clip.samples);
    }

    #[test]
    fn gain_is_applied_and_clamped() {
        let mut samples = vec![0.5f32, -0.5, 0.9];
        apply_gain(&mut samples, 20.0); // x10

        // Clamped rather than wrapped: a hot clip must not turn into a
        // burst of noise at the opposite polarity.
        assert!(samples.iter().all(|s| (-1.0..=1.0).contains(s)), "{samples:?}");
        assert_eq!(samples[0], 1.0);
        assert_eq!(samples[1], -1.0);
    }

    #[test]
    fn zero_gain_leaves_audio_alone() {
        let mut samples = vec![0.25f32, -0.5];
        apply_gain(&mut samples, 0.0);
        assert_eq!(samples, vec![0.25, -0.5]);
    }
}
