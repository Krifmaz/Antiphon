//! On-device live captions (spec §7 "Accessibility -- on-device live
//! captions").
//!
//! Speech recognition runs inside the app, on this machine: received call
//! audio is resampled ([`resample`]), cut into utterances ([`vad`]), and
//! transcribed by Whisper ([`whisper`]) on worker threads that the call
//! audio never waits for ([`pipeline`]). No audio, and no text, is sent
//! anywhere. The model is fetched once, only with the user's agreement,
//! and checked against a pinned hash ([`model_files`]).

pub mod model_files;
pub mod pipeline;
pub mod resample;
pub mod vad;
pub mod whisper;

pub use pipeline::{Caption, CaptionTap, Captioner, Event, Stats, Transcriber};
pub use whisper::WhisperModel;
