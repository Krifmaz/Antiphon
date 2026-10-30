//! Every stage of the microphone chain and every parameter it has, as a
//! table the settings screens draw from (spec §1 "every chain stage
//! toggles independently", §0 "Explain everything").
//!
//! One table, so the desktop and the phone show the same controls with
//! the same ranges and the same plain-language sentence, and neither app
//! carries its own match over [`ChainConfig`]'s fields. A screen lists
//! [`STAGES`], reads values with [`Stage::is_on`] and [`Param::value`],
//! and hands changes back through [`set_stage`] and [`set_param`], which
//! clamp to the ranges here.
//!
//! The EQ's bands are a list rather than fixed fields, so they have their
//! own table, [`BAND_PARAMS`], and [`set_band`] edits one band.

use crate::chain::{ChainConfig, EqBandConfig, GainConfig};

/// One adjustable value of a stage, or of an EQ band when `T` is
/// [`EqBandConfig`].
pub struct Param<T: 'static = ChainConfig> {
    /// Stable id, unique within its stage, used in actions.
    pub id: &'static str,
    /// What the slider is labelled.
    pub name: &'static str,
    /// The sentence behind it, for the help text and the screen reader.
    pub help: &'static str,
    /// The unit shown beside the value ("dB", "Hz", "ms", ":1", "").
    pub unit: &'static str,
    /// The lowest value the slider reaches.
    pub min: f32,
    /// The highest value the slider reaches.
    pub max: f32,
    /// How far one step of the slider moves.
    pub step: f32,
    /// Reads the value.
    get: fn(&T) -> f32,
    /// Writes an already-clamped value.
    put: fn(&mut T, f32),
}

impl<T> Param<T> {
    /// The value in `of`.
    pub fn value(&self, of: &T) -> f32 {
        (self.get)(of)
    }

    /// Writes `value` into `of`, clamped to the range. `false`, with
    /// nothing written, for a value that is not a number.
    fn write(&self, of: &mut T, value: f32) -> bool {
        if !value.is_finite() {
            return false;
        }
        (self.put)(of, value.clamp(self.min, self.max));
        true
    }
}

/// One stage of the chain: its switch and its parameters.
pub struct Stage {
    /// Stable id, used in actions.
    pub id: &'static str,
    /// The stage's name on screen.
    pub name: &'static str,
    /// What it does, in a sentence.
    pub help: &'static str,
    /// Reads whether the stage is on.
    on: fn(&ChainConfig) -> bool,
    /// Turns the stage on or off.
    turn: fn(&mut ChainConfig, bool),
    /// Its parameters, in the order they are shown. Automatic level's
    /// apply only while it is automatic (see [`Stage::params`]).
    params: &'static [Param],
}

impl Stage {
    /// Whether the stage is on in `chain`.
    pub fn is_on(&self, chain: &ChainConfig) -> bool {
        (self.on)(chain)
    }

    /// The parameters that apply to `chain` as it stands. Automatic level
    /// has its targets only while it is automatic; a fixed gain has none
    /// worth a slider (it is off or 0 dB), so it shows the switch alone.
    pub fn params(&self, chain: &ChainConfig) -> &'static [Param] {
        if self.id == "gain" && !matches!(chain.gain, GainConfig::Auto { .. }) {
            return &[];
        }
        self.params
    }
}

/// Shorthand for a table row.
macro_rules! param {
    ($id:literal, $name:literal, $unit:literal, $min:expr, $max:expr, $step:expr, $help:literal, |$c:ident| $field:expr) => {
        Param {
            id: $id,
            name: $name,
            help: $help,
            unit: $unit,
            min: $min,
            max: $max,
            step: $step,
            get: |$c| $field,
            put: |$c, v| $field = v,
        }
    };
}

/// Shorthand for one of the automatic level's rows: reads `0.0` and
/// writes nothing while the gain is fixed (the rows are hidden then).
macro_rules! auto_param {
    ($id:literal, $name:literal, $unit:literal, $min:expr, $max:expr, $step:expr, $help:literal, $field:ident) => {
        Param {
            id: $id,
            name: $name,
            help: $help,
            unit: $unit,
            min: $min,
            max: $max,
            step: $step,
            get: |c| match c.gain {
                GainConfig::Auto { $field, .. } => $field,
                GainConfig::Fixed { .. } => 0.0,
            },
            put: |c, v| {
                if let GainConfig::Auto { $field, .. } = &mut c.gain {
                    *$field = v;
                }
            },
        }
    };
}

/// Shorthand for a stage whose switch is a plain `enabled` field.
macro_rules! stage {
    ($id:literal, $name:literal, $help:literal, $field:ident, [$($param:expr),* $(,)?]) => {
        Stage {
            id: $id,
            name: $name,
            help: $help,
            on: |c| c.$field.enabled,
            turn: |c, on| c.$field.enabled = on,
            params: &[$($param),*],
        }
    };
}

/// The whole chain, in the order the audio runs through it. The ranges
/// are the ones the classic settings used.
pub static STAGES: &[Stage] = &[
    stage!("noise_suppress", "Noise suppression", "Takes out steady background noise such as fans and hum.", noise_suppress, [
        param!("strength", "Strength", "", 0.0, 1.0, 0.05, "How hard it removes noise. Higher is quieter but can thin your voice.", |c| c.noise_suppress.strength),
    ]),
    stage!("noise_gate", "Noise gate", "Mutes the mic between words so quiet noise is not sent.", noise_gate, [
        param!("threshold", "Threshold", "dB", -80.0, -10.0, 1.0, "Sound below this level counts as silence and is muted.", |c| c.noise_gate.threshold_db),
        param!("attack", "Attack", "ms", 1.0, 50.0, 1.0, "How quickly the gate opens when you start speaking.", |c| c.noise_gate.attack_ms),
        param!("hold", "Hold", "ms", 0.0, 500.0, 10.0, "How long it stays open after you stop, so word endings are kept.", |c| c.noise_gate.hold_ms),
        param!("release", "Release", "ms", 10.0, 1000.0, 10.0, "How slowly it fades closed after the hold.", |c| c.noise_gate.release_ms),
    ]),
    stage!("high_pass", "Low cut", "Removes rumble below a pitch, such as desk bumps and traffic.", high_pass, [
        param!("cutoff", "Cutoff", "Hz", 20.0, 400.0, 5.0, "Sound lower than this is removed.", |c| c.high_pass.cutoff_hz),
    ]),
    stage!("low_pass", "High cut", "Removes hiss above a pitch.", low_pass, [
        param!("cutoff", "Cutoff", "Hz", 2000.0, 20000.0, 100.0, "Sound higher than this is removed.", |c| c.low_pass.cutoff_hz),
    ]),
    stage!("de_esser", "De-esser", "Softens sharp s and sh sounds.", de_esser, [
        param!("threshold", "Threshold", "dB", -50.0, -5.0, 1.0, "How loud an s must be before it is softened.", |c| c.de_esser.threshold_db),
        param!("ratio", "Ratio", ":1", 1.0, 10.0, 0.5, "How much a loud s is turned down.", |c| c.de_esser.ratio),
        param!("split", "Split", "Hz", 2000.0, 10000.0, 100.0, "The pitch above which sound counts as an s.", |c| c.de_esser.split_hz),
        param!("attack", "Attack", "ms", 1.0, 50.0, 1.0, "How quickly it reacts to an s.", |c| c.de_esser.attack_ms),
        param!("release", "Release", "ms", 5.0, 300.0, 5.0, "How quickly it lets go afterwards.", |c| c.de_esser.release_ms),
    ]),
    stage!("compressor", "Compressor", "Evens out loud and quiet parts of your voice.", compressor, [
        param!("threshold", "Threshold", "dB", -60.0, 0.0, 1.0, "Sound louder than this is turned down.", |c| c.compressor.threshold_db),
        param!("ratio", "Ratio", ":1", 1.0, 10.0, 0.5, "How much loud sound is turned down.", |c| c.compressor.ratio),
        param!("attack", "Attack", "ms", 0.5, 100.0, 0.5, "How quickly it reacts to a loud sound.", |c| c.compressor.attack_ms),
        param!("release", "Release", "ms", 10.0, 1000.0, 10.0, "How quickly it lets go afterwards.", |c| c.compressor.release_ms),
        param!("makeup", "Make-up gain", "dB", 0.0, 24.0, 0.5, "Turns everything back up after compressing.", |c| c.compressor.makeup_gain_db),
    ]),
    stage!("limiter", "Limiter", "Stops sudden loud sounds from clipping.", limiter, [
        param!("ceiling", "Ceiling", "dB", -12.0, 0.0, 0.5, "Nothing louder than this is sent.", |c| c.limiter.ceiling_db),
        param!("release", "Release", "ms", 10.0, 500.0, 5.0, "How quickly it lets go after a peak.", |c| c.limiter.release_ms),
    ]),
    Stage {
        id: "gain",
        name: "Automatic level",
        help: "Keeps your voice at a steady loudness however far you are from the mic.",
        // On means automatic and running; a fixed gain (the classic
        // app's other mode) reads as off, and turning the stage on makes
        // it automatic with the usual targets, as the Processing tab's
        // switch does.
        on: |c| matches!(c.gain, GainConfig::Auto { enabled: true, .. }),
        turn: |c, on| match &mut c.gain {
            GainConfig::Auto { enabled, .. } => *enabled = on,
            GainConfig::Fixed { .. } if on => c.gain = GainConfig::default(),
            GainConfig::Fixed { enabled, .. } => *enabled = false,
        },
        params: &[
            auto_param!("target", "Target", "dB", -40.0, 0.0, 0.5, "How loud it aims to keep your voice.", target_db),
            auto_param!("min", "Most cut", "dB", -24.0, 0.0, 0.5, "The furthest it will turn you down.", min_gain_db),
            auto_param!("max", "Most boost", "dB", 0.0, 36.0, 0.5, "The furthest it will turn you up.", max_gain_db),
            auto_param!("adapt", "Speed", "ms", 50.0, 3000.0, 50.0, "How long it takes to settle on a new level. Shorter reacts faster.", adapt_time_ms),
        ],
    },
    stage!("pitch_shift", "Pitch", "Moves your voice higher or lower.", pitch_shift, [
        param!("semitones", "Shift", "semitones", -12.0, 12.0, 0.5, "How far to move it. Twelve is one octave.", |c| c.pitch_shift.semitones),
    ]),
    stage!("reverb", "Reverb", "Adds the sound of a room.", reverb, [
        param!("room", "Room size", "", 0.0, 1.0, 0.05, "Bigger sounds like a larger space.", |c| c.reverb.room_size),
        param!("damping", "Damping", "", 0.0, 1.0, 0.05, "Higher makes the room sound softer.", |c| c.reverb.damping),
        param!("wet", "Mix", "", 0.0, 1.0, 0.05, "How much of the room is mixed in with your voice.", |c| c.reverb.wet_dry),
    ]),
];

/// Turns stage `id` on or off. `false` for an id not in [`STAGES`].
pub fn set_stage(chain: &mut ChainConfig, id: &str, on: bool) -> bool {
    let Some(stage) = STAGES.iter().find(|s| s.id == id) else { return false };
    (stage.turn)(chain, on);
    true
}

/// Sets parameter `param` of stage `stage`, clamped to its range. `false`
/// for ids not in [`STAGES`].
pub fn set_param(chain: &mut ChainConfig, stage: &str, param: &str, value: f32) -> bool {
    STAGES
        .iter()
        .find(|s| s.id == stage)
        .and_then(|s| s.params.iter().find(|p| p.id == param))
        .is_some_and(|p| p.write(chain, value))
}

/// The adjustable values of one EQ band.
pub static BAND_PARAMS: &[Param<EqBandConfig>] = &[
    param!("gain", "Boost or cut", "dB", -12.0, 12.0, 0.5, "Turns this band up or down.", |b| b.gain_db),
    param!("center", "Pitch", "Hz", 20.0, 20000.0, 10.0, "The pitch this band works on.", |b| b.center_hz),
    param!("q", "Width", "Q", 0.1, 10.0, 0.1, "How narrow the band is. Higher touches fewer pitches.", |b| b.q),
];

/// Sets value `param` of EQ band `index`, clamped to its range, or turns
/// the band on or off when `param` is `"enabled"` (any value above 0.5 is
/// on). `false` for a band or id that does not exist.
pub fn set_band(chain: &mut ChainConfig, index: usize, param: &str, value: f32) -> bool {
    let Some(band) = chain.eq_bands.get_mut(index) else { return false };
    if param == "enabled" {
        band.enabled = value > 0.5;
        return true;
    }
    BAND_PARAMS.iter().find(|p| p.id == param).is_some_and(|p| p.write(band, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        let mut stages: Vec<_> = STAGES.iter().map(|s| s.id).collect();
        stages.sort_unstable();
        stages.dedup();
        assert_eq!(stages.len(), STAGES.len());
        for stage in STAGES {
            let mut ids: Vec<_> = stage.params.iter().map(|p| p.id).collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), stage.params.len(), "{}", stage.id);
        }
    }

    #[test]
    fn every_param_reads_back_what_it_wrote() {
        let mut chain = ChainConfig::default();
        for stage in STAGES {
            assert!(set_stage(&mut chain, stage.id, false));
            assert!(!stage.is_on(&chain));
            assert!(set_stage(&mut chain, stage.id, true));
            assert!(stage.is_on(&chain));
            for p in stage.params(&chain) {
                let mid = (p.min + p.max) / 2.0;
                assert!(set_param(&mut chain, stage.id, p.id, mid));
                assert!((p.value(&chain) - mid).abs() < 1e-4, "{}.{}", stage.id, p.id);
            }
        }
    }

    #[test]
    fn values_are_clamped_and_nan_refused() {
        let mut chain = ChainConfig::default();
        assert!(set_param(&mut chain, "limiter", "ceiling", 5.0));
        assert!((chain.limiter.ceiling_db - 0.0).abs() < f32::EPSILON);
        assert!(!set_param(&mut chain, "limiter", "ceiling", f32::NAN));
        assert!(!set_param(&mut chain, "limiter", "nope", 1.0));
        assert!(!set_stage(&mut chain, "nope", true));
    }

    #[test]
    fn bands_are_edited_in_place() {
        let band = EqBandConfig { enabled: true, center_hz: 1000.0, q: 1.0, gain_db: 0.0, band_type: crate::BandType::Bell };
        let mut chain = ChainConfig { eq_bands: vec![band], ..ChainConfig::default() };
        assert!(set_band(&mut chain, 0, "gain", 30.0));
        assert!((chain.eq_bands[0].gain_db - 12.0).abs() < f32::EPSILON);
        assert!(set_band(&mut chain, 0, "enabled", 0.0));
        assert!(!chain.eq_bands[0].enabled);
        assert!(!set_band(&mut chain, 1, "gain", 1.0));
    }
}
