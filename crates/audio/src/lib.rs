//! The microphone processing chain and audio device I/O (spec §1).
//!
//! Implemented: noise gate, high-pass/low-pass filters, a de-esser,
//! parametric (peaking) EQ, compressor, limiter, fixed and auto gain,
//! pitch shift, and reverb, all composable into a chain and save/loadable
//! as a preset. Deferred (each needs a real-time adaptive filter or ML
//! model, a different order of effort than the rest of this chain):
//! ML-based noise suppression, echo cancellation.

#[cfg(target_os = "android")]
mod aaudio;
mod chain;
mod echo_cancel;
mod device;
pub mod plugins;
mod stages;
mod time_compress;
mod util;

pub use chain::{
    ChainConfig, CompressorConfig, DeEsserConfig, EqBandConfig, GainConfig, HighPassConfig, LimiterConfig,
    LowPassConfig, NoiseGateConfig, NoiseSuppressConfig, PitchShiftConfig, ReverbConfig,
};
pub use chain::{eq_curve_db, eq_preset, eq_preset_names, log_spaced_frequencies};
pub use chain::AudioChain;
pub use device::{
    audio_backend_name, list_input_devices, list_output_devices, start_capture, start_playback, start_system_capture, AudioBackend, AudioError,
    CaptureHandle, PlaybackHandle, PlaybackSink,
};
pub use echo_cancel::EchoCanceller;
pub use time_compress::{TimeCompressor, MAX_SPEED, MIN_SPEED};
pub use stages::{NeuralSuppressor, NoiseSuppressor, THIN_VOICE_WARNING_STRENGTH, AutoGain, BandType, Compressor, DeEsser, Gain, HighPassFilter, Limiter, LowPassFilter, NoiseGate, PeakingEq, PitchShift, Reverb, Stage};
