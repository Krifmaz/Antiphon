pub(crate) mod biquad;
mod compressor;
mod de_esser;
mod formant;
mod gain;
mod limiter;
mod noise_gate;
mod noise_suppress;
mod peaking_eq;
mod pitch_shift;
mod reverb;

pub use biquad::{HighPassFilter, LowPassFilter};
pub use compressor::Compressor;
pub use de_esser::DeEsser;
pub use gain::{AutoGain, Gain};
pub use limiter::Limiter;
pub use noise_gate::NoiseGate;
pub use noise_suppress::{NoiseSuppressor, THIN_VOICE_WARNING_STRENGTH};
pub use peaking_eq::{BandType, PeakingEq};
pub use pitch_shift::PitchShift;
pub use reverb::Reverb;

/// One link in the microphone processing chain (spec §1 "Audio Suite").
/// Each stage owns its own DSP state and processes a buffer of mono f32
/// samples in place. Bypass/enable state is tracked by [`crate::chain::AudioChain`],
/// not by the stage itself, so implementations stay focused on the signal
/// processing.
pub trait Stage: Send {
    fn name(&self) -> &'static str;
    fn process(&mut self, samples: &mut [f32]);

    /// How much this stage is currently pulling the signal down, in dB,
    /// for stages where that is a meaningful thing to show (spec §1's
    /// "gain reduction meter").
    ///
    /// Opt-in rather than mandatory: most stages have no such number, and
    /// forcing every one of them to answer would mean a pile of
    /// `unreachable!()`. Returning `None` is the honest default.
    fn gain_reduction_db(&self) -> Option<f32> {
        None
    }
}
