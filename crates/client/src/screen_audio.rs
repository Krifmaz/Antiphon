//! The sound of a shared screen (spec §5 "Screen share"): what this
//! computer plays, recorded while a screen is shared and mixed into the
//! call's voice after the mic chain, the way soundboard clips are.
//!
//! Recording the system's output records the call too, so the other side
//! would hear themselves back. [`ScreenAudio`] takes the call's own
//! playback back out with an [`EchoCanceller`] fed exactly what the call
//! played (the same signal the microphone's canceller gets). The loopback
//! is a clean digital copy of that signal, so the canceller has the
//! easiest version of its job; what's left is the video, game or music
//! the user is sharing.
//!
//! One switch for every call ([`set_enabled`]), on by default, read each
//! time a share starts or stops.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use antiphon_audio::{start_system_capture, CaptureHandle, EchoCanceller};
use antiphon_net::voice::Resampler;

/// Recorded audio kept waiting for the send loop, at most: half a second.
/// Past that the oldest is dropped, so a stalled loop never turns into a
/// growing delay between the picture and its sound.
const BACKLOG_SECONDS: f32 = 0.5;

/// "Share screen audio" for every call on this device.
static ENABLED: AtomicBool = AtomicBool::new(true);

/// Turns sharing a screen's sound on or off, including for a share that is
/// running now (it is stopped or started within a second).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether a shared screen's sound is sent.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// What the call has played, for taking it back out of the recording.
/// The call's playback loop feeds every call's one of these; it keeps at
/// most a second.
#[derive(Clone, Default)]
pub struct Played(Arc<Mutex<VecDeque<f32>>>);

impl Played {
    /// Adds what the speaker was just given, at the call's rate.
    pub fn push(&self, samples: &[f32], rate: u32) {
        let mut queue = self.0.lock().unwrap();
        queue.extend(samples.iter().copied());
        let cap = rate as usize;
        if queue.len() > cap {
            let excess = queue.len() - cap;
            queue.drain(..excess);
        }
    }

    /// The next `n` samples, zero where nothing was played.
    fn take(&self, n: usize) -> Vec<f32> {
        let mut queue = self.0.lock().unwrap();
        (0..n).map(|_| queue.pop_front().unwrap_or(0.0)).collect()
    }

    /// Drops what has built up, when a recording starts.
    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// A running recording of the system's output.
pub struct ScreenAudio {
    _capture: CaptureHandle,
    recorded: Arc<Mutex<VecDeque<f32>>>,
    played: Played,
    canceller: EchoCanceller,
}

impl ScreenAudio {
    /// Starts recording what this computer plays, delivered at `rate`
    /// (the call's). Fails on a system with no way to record its output
    /// (see [`antiphon_audio::start_system_capture`]).
    pub fn start(rate: u32, played: Played) -> Result<Self, String> {
        let recorded = Arc::new(Mutex::new(VecDeque::new()));
        let sink = recorded.clone();
        let backlog = (rate as f32 * BACKLOG_SECONDS) as usize;
        // The resampler is created on the first block, once the device's
        // rate is known; the callback owns it.
        let device_rate = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let device_rate_cb = device_rate.clone();
        let mut resampler: Option<Resampler> = None;
        let mut converted = Vec::new();
        let capture = start_system_capture(move |samples: &[f32]| {
            let from = device_rate_cb.load(Ordering::Relaxed);
            if from == 0 {
                return;
            }
            converted.clear();
            if from == rate {
                converted.extend_from_slice(samples);
            } else {
                resampler.get_or_insert_with(|| Resampler::new(from, rate)).process(samples, &mut converted);
            }
            let mut queue = sink.lock().unwrap();
            queue.extend(converted.iter().copied());
            if queue.len() > backlog {
                let excess = queue.len() - backlog;
                queue.drain(..excess);
            }
        })
        .map_err(|e| e.to_string())?;
        device_rate.store(capture.sample_rate, Ordering::Relaxed);
        played.clear();
        Ok(Self { _capture: capture, recorded, played, canceller: EchoCanceller::new(rate) })
    }

    /// The next `n` samples of the screen's sound with the call taken out,
    /// or `None` while fewer than `n` have been recorded.
    pub fn take(&mut self, n: usize) -> Option<Vec<f32>> {
        let mut block: Vec<f32> = {
            let mut queue = self.recorded.lock().unwrap();
            if queue.len() < n {
                return None;
            }
            queue.drain(..n).collect()
        };
        let played = self.played.take(n);
        self.canceller.process(&mut block, &played);
        Some(block)
    }
}

/// Mixes `screen` into `voice`, clamped, sample by sample.
pub fn mix_into(voice: &mut [f32], screen: &[f32]) {
    for (v, s) in voice.iter_mut().zip(screen) {
        *v = (*v + s).clamp(-1.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn played_audio_is_capped_at_a_second_and_zero_padded() {
        let played = Played::default();
        played.push(&vec![0.5; 70_000], 48_000);
        assert_eq!(played.0.lock().unwrap().len(), 48_000);
        played.clear();
        played.push(&[0.25; 3], 48_000);
        assert_eq!(played.take(5), vec![0.25, 0.25, 0.25, 0.0, 0.0]);
    }

    #[test]
    fn mixing_adds_and_clamps() {
        let mut voice = [0.5, -0.9, 0.0];
        mix_into(&mut voice, &[0.7, -0.5, 0.25]);
        assert_eq!(voice, [1.0, -1.0, 0.25]);
    }

    #[test]
    fn the_switch_is_on_by_default() {
        assert!(enabled());
    }
}
