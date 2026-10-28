//! The replay buffer, encrypted in memory (spec §2: "Ephemeral by design:
//! the buffer is encrypted, in memory, and discarded on a rolling basis").
//!
//! Received audio is gathered into 20 ms blocks and each block is sealed
//! as soon as it fills, under a key made for this call alone
//! ([`MemorySealer`]). Old blocks fall off the front as the window rolls,
//! and when the call ends the ring and its key are dropped together, so
//! nothing that was held can be read back afterwards -- not from a memory
//! dump, a swap file or a crash report.
//!
//! Only the block still filling is plaintext: at most 20 ms of audio. The
//! replay clips are sealed under the same key, for the same reason.

use std::collections::VecDeque;

use antiphon_crypto::memory_seal::MemorySealer;

/// Samples per sealed block: one 20 ms frame at 48 kHz.
pub const BLOCK: usize = 960;

/// Audio sealed by a [`SealedRing`]'s key.
#[derive(Clone, Debug, PartialEq)]
pub struct SealedAudio {
    bytes: Vec<u8>,
    samples: usize,
}

#[derive(Default)]
pub struct SealedRing {
    sealer: MemorySealer,
    blocks: VecDeque<SealedAudio>,
    /// The block still filling, plaintext until it reaches [`BLOCK`].
    tail: Vec<f32>,
    sealed_len: usize,
}

impl SealedRing {
    pub fn len(&self) -> usize {
        self.sealed_len + self.tail.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends audio, then drops whole blocks from the front until no more
    /// than `cap` samples (plus less than one block) are held.
    pub fn push(&mut self, samples: &[f32], cap: usize) {
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (BLOCK - self.tail.len()).min(rest.len());
            self.tail.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.tail.len() == BLOCK {
                let block = self.seal(&self.tail.clone());
                self.tail.iter_mut().for_each(|s| *s = 0.0);
                self.tail.clear();
                self.sealed_len += block.samples;
                self.blocks.push_back(block);
            }
        }
        while self.len() > cap {
            let Some(front) = self.blocks.pop_front() else { break };
            self.sealed_len -= front.samples;
        }
    }

    /// `len` samples starting `start` from the oldest one held.
    pub fn copy_range(&self, start: usize, len: usize) -> Vec<f32> {
        if self.is_empty() {
            return Vec::new();
        }
        let end = (start + len).min(self.len());
        let mut out = Vec::with_capacity(end.saturating_sub(start));
        let mut at = 0;
        for block in &self.blocks {
            let block_end = at + block.samples;
            if block_end > start && at < end {
                let audio = self.open(block).unwrap_or_else(|| vec![0.0; block.samples]);
                let from = start.saturating_sub(at);
                let to = (end - at).min(block.samples);
                out.extend_from_slice(&audio[from..to]);
            }
            at = block_end;
            if at >= end {
                return out;
            }
        }
        let from = start.saturating_sub(at);
        let to = end.saturating_sub(at).min(self.tail.len());
        if from < to {
            out.extend_from_slice(&self.tail[from..to]);
        }
        out
    }

    /// Everything held, oldest first.
    pub fn snapshot(&self) -> Vec<f32> {
        self.copy_range(0, self.len())
    }

    /// Seals audio under this call's key.
    pub fn seal(&mut self, samples: &[f32]) -> SealedAudio {
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let bytes = zeroize::Zeroizing::new(bytes);
        SealedAudio { bytes: self.sealer.seal(&bytes), samples: samples.len() }
    }

    /// Opens audio sealed by this ring. `None` for anything sealed by
    /// another call's ring.
    pub fn open(&self, audio: &SealedAudio) -> Option<Vec<f32>> {
        let plain = self.sealer.open(&audio.bytes)?;
        Some(plain.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: usize, from: usize) -> Vec<f32> {
        (from..from + n).map(|i| i as f32).collect()
    }

    #[test]
    fn holds_what_was_pushed_in_order() {
        let mut r = SealedRing::default();
        r.push(&ramp(2_500, 0), usize::MAX);
        assert_eq!(r.len(), 2_500);
        assert_eq!(r.snapshot(), ramp(2_500, 0));
        assert_eq!(r.copy_range(950, 30), ramp(30, 950));
        assert_eq!(r.copy_range(2_000, 1_000), ramp(500, 2_000));
    }

    #[test]
    fn rolls_off_the_oldest_audio() {
        let mut r = SealedRing::default();
        for i in 0..20 {
            r.push(&ramp(480, i * 480), 4_800);
        }
        assert!(r.len() <= 4_800 + BLOCK);
        let held = r.snapshot();
        assert_eq!(*held.last().unwrap(), (20 * 480 - 1) as f32);
        assert_eq!(held, ramp(held.len(), 20 * 480 - held.len()));
    }

    #[test]
    fn only_the_filling_block_is_plaintext() {
        let mut r = SealedRing::default();
        r.push(&vec![0.123_456_7; BLOCK * 3 + 10], usize::MAX);
        assert_eq!(r.blocks.len(), 3);
        assert_eq!(r.tail.len(), 10);
        let needle = 0.123_456_7f32.to_le_bytes();
        for b in &r.blocks {
            assert!(!b.bytes.windows(4).any(|w| w == needle), "audio visible in a sealed block");
        }
    }

    #[test]
    fn a_clip_sealed_by_one_call_cannot_be_opened_by_another() {
        let mut a = SealedRing::default();
        let b = SealedRing::default();
        let clip = a.seal(&[0.5; 100]);
        assert_eq!(a.open(&clip).unwrap(), vec![0.5; 100]);
        assert!(b.open(&clip).is_none());
    }
}
