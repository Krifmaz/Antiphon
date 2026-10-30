use serde::{Deserialize, Serialize};

use crate::stages::{BandType,
    AutoGain, Compressor, DeEsser, Gain, HighPassFilter, Limiter, LowPassFilter, NoiseGate, NeuralSuppressor, NoiseSuppressor, PeakingEq, PitchShift, Reverb, Stage,
};

struct ChainStage {
    enabled: bool,
    stage: Box<dyn Stage>,
}

/// The full mic processing chain (spec §1): an ordered list of independently
/// toggleable stages, run in sequence, in spec order -- Cleanup (gate,
/// filters, de-esser), Tone (EQ, compressor, limiter, gain), Creative
/// (pitch shift, reverb). Noise suppression runs first of all; echo
/// cancellation lives outside the chain because it needs the far end.
pub struct AudioChain {
    stages: Vec<ChainStage>,
}

impl AudioChain {
    pub fn from_config(sample_rate: u32, config: &ChainConfig) -> Self {
        let mut stages: Vec<ChainStage> = Vec::new();

        // Before the gate: the gate decides whether you are speaking from
        // the level it sees, so cleaning the background up first means it
        // is judging your voice rather than your fan.
        let ns = &config.noise_suppress;
        stages.push(ChainStage {
            enabled: ns.enabled,
            stage: if ns.neural && NeuralSuppressor::supports(sample_rate) {
                Box::new(NeuralSuppressor::new(ns.strength))
            } else {
                Box::new(NoiseSuppressor::new(ns.enabled, ns.strength))
            },
        });

        let g = &config.noise_gate;
        stages.push(ChainStage {
            enabled: g.enabled,
            stage: Box::new(NoiseGate::new(sample_rate, g.threshold_db, g.attack_ms, g.hold_ms, g.release_ms)),
        });

        let hp = &config.high_pass;
        stages.push(ChainStage {
            enabled: hp.enabled,
            stage: Box::new(HighPassFilter::new(sample_rate, hp.cutoff_hz)),
        });

        let lp = &config.low_pass;
        stages.push(ChainStage {
            enabled: lp.enabled,
            stage: Box::new(LowPassFilter::new(sample_rate, lp.cutoff_hz)),
        });

        let de = &config.de_esser;
        stages.push(ChainStage {
            enabled: de.enabled,
            stage: Box::new(DeEsser::new(sample_rate, de.split_hz, de.threshold_db, de.ratio, de.attack_ms, de.release_ms)),
        });

        for band in &config.eq_bands {
            stages.push(ChainStage {
                enabled: band.enabled,
                stage: Box::new(PeakingEq::with_type(
                    sample_rate,
                    band.band_type,
                    band.center_hz,
                    band.q,
                    band.gain_db,
                )),
            });
        }

        let c = &config.compressor;
        stages.push(ChainStage {
            enabled: c.enabled,
            stage: Box::new(Compressor::new(sample_rate, c.threshold_db, c.ratio, c.attack_ms, c.release_ms, c.makeup_gain_db)),
        });

        let l = &config.limiter;
        stages.push(ChainStage {
            enabled: l.enabled,
            stage: Box::new(Limiter::new(sample_rate, l.ceiling_db, l.release_ms)),
        });

        match &config.gain {
            GainConfig::Fixed { enabled, gain_db } => stages.push(ChainStage {
                enabled: *enabled,
                stage: Box::new(Gain::new(*gain_db)),
            }),
            GainConfig::Auto { enabled, target_db, min_gain_db, max_gain_db, adapt_time_ms } => {
                stages.push(ChainStage {
                    enabled: *enabled,
                    stage: Box::new(AutoGain::new(sample_rate, *target_db, *min_gain_db, *max_gain_db, *adapt_time_ms)),
                });
            }
        }

        // The user's plugins. One that fails to load is left out and
        // logged, never allowed to stop the chain.
        for plugin in &config.plugins {
            match crate::plugins::load(plugin, sample_rate) {
                Ok(stage) => stages.push(ChainStage { enabled: plugin.enabled, stage }),
                Err(e) => eprintln!("[audio] leaving out plugin {}: {e}", plugin.name),
            }
        }

        let ps = &config.pitch_shift;
        stages.push(ChainStage {
            enabled: ps.enabled,
            stage: Box::new(PitchShift::new(sample_rate, ps.semitones, ps.preserve_formants)),
        });

        let rv = &config.reverb;
        stages.push(ChainStage {
            enabled: rv.enabled,
            stage: Box::new(Reverb::new(sample_rate, rv.room_size, rv.damping, rv.wet_dry)),
        });

        Self { stages }
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        for s in &mut self.stages {
            if s.enabled {
                s.stage.process(samples);
            }
        }
    }

    /// Toggle a stage on/off by name (e.g. "noise_gate"), matching spec §0's
    /// "every automatic behavior has a manual override". Returns false if no
    /// stage with that name exists.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        if let Some(s) = self.stages.iter_mut().find(|s| s.stage.name() == name) {
            s.enabled = enabled;
            true
        } else {
            false
        }
    }

    /// Current gain reduction of the named stage, or 0 when that stage is
            /// absent, disabled, or does not report one.
    ///
    /// Zero when nothing is running is the right answer rather than a
    /// missing one: an idle meter should read empty, not unknown.
    pub fn gain_reduction_db(&self, name: &str) -> f32 {
        self.stages
            .iter()
            .find(|s| s.enabled && s.stage.name() == name)
            .and_then(|s| s.stage.gain_reduction_db())
            .unwrap_or(0.0)
    }

    pub fn stage_names(&self) -> Vec<&'static str> {
        self.stages.iter().map(|s| s.stage.name()).collect()
    }
}

/// `count` frequencies log-spaced between `min_hz` and `max_hz` -- the
/// standard x-axis for an EQ curve graph (a linear Hz axis would crush the
/// entire bass/midrange into a sliver next to the treble).
pub fn log_spaced_frequencies(min_hz: f32, max_hz: f32, count: usize) -> Vec<f32> {
    if count < 2 {
        return vec![min_hz];
    }
    let ratio = max_hz / min_hz;
    (0..count).map(|i| min_hz * ratio.powf(i as f32 / (count - 1) as f32)).collect()
}

/// The combined magnitude response (dB) of every *enabled* band, at each of
/// `freqs_hz` -- what an EQ curve graph plots. Cascaded (series) filters
/// multiply in linear magnitude, which is a sum in dB, so this is just
/// `sum` per frequency, not a re-derivation of the composite transfer
/// function.
pub fn eq_curve_db(bands: &[EqBandConfig], sample_rate: u32, freqs_hz: &[f32]) -> Vec<f32> {
    freqs_hz
        .iter()
        .map(|&f| {
            bands
                .iter()
                .filter(|b| b.enabled)
                .map(|b| {
                    PeakingEq::with_type(sample_rate, b.band_type, b.center_hz, b.q, b.gain_db)
                        .magnitude_db(sample_rate, f)
                })
                .sum()
        })
        .collect()
}

/// The starter EQ curves from spec §1 ("Starter presets: Warm, Clear,
/// Broadcast, Thin Mic Rescue").
///
/// Deliberately gentle. A starter preset is a nudge for someone who does
/// not want to learn what Q means, and one that obviously colours the
/// voice teaches people to distrust the whole EQ. Nothing here moves more
/// than 6 dB, and every curve is reachable by hand afterwards -- loading a
/// preset just fills the band list in, it does not lock anything.
pub fn eq_preset_names() -> &'static [&'static str] {
    &["Warm", "Clear", "Broadcast", "Thin Mic Rescue"]
}

/// Bands for a named starter preset, or `None` if the name is unknown.
pub fn eq_preset(name: &str) -> Option<Vec<EqBandConfig>> {
    let bands = match name {
        // Body without mud: lift the chest register, ease off the fizz.
        "Warm" => vec![
            band(BandType::LowShelf, 220.0, 0.7, 3.0),
            band(BandType::Bell, 500.0, 1.2, -2.0),
            band(BandType::HighShelf, 7000.0, 0.7, -2.0),
        ],
        // Intelligibility: cut the boxy region, lift consonants.
        "Clear" => vec![
            band(BandType::Bell, 300.0, 1.0, -3.0),
            band(BandType::Bell, 3000.0, 1.0, 3.0),
            band(BandType::HighShelf, 6000.0, 0.7, 2.0),
        ],
        // The radio sound: firm low end, scooped mids, open top.
        "Broadcast" => vec![
            band(BandType::LowShelf, 120.0, 0.7, 3.0),
            band(BandType::Bell, 400.0, 1.4, -4.0),
            band(BandType::Bell, 2500.0, 1.0, 2.0),
            band(BandType::HighShelf, 5000.0, 0.7, 4.0),
        ],
        // For a mic with no bottom end: put weight back without booming.
        "Thin Mic Rescue" => vec![
            band(BandType::LowShelf, 180.0, 0.7, 6.0),
            band(BandType::Bell, 350.0, 1.2, 2.0),
            band(BandType::Bell, 4000.0, 1.5, -2.0),
        ],
        _ => return None,
    };
    Some(bands)
}

fn band(band_type: BandType, center_hz: f32, q: f32, gain_db: f32) -> EqBandConfig {
    EqBandConfig { enabled: true, center_hz, q, gain_db, band_type }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NoiseSuppressConfig {
    pub enabled: bool,
    /// 0..1. Zero is a passthrough; one is severe enough to thin a voice.
    pub strength: f32,
    /// Use the RNNoise voice model (spec §1) rather than spectral
    /// subtraction. Falls back to spectral when the device isn't running
    /// at 48 kHz, the only rate the model knows. Defaulted on, so a config
    /// saved before the model existed gets it.
    #[serde(default = "default_true")]
    pub neural: bool,
}

fn default_true() -> bool {
    true
}

impl Default for NoiseSuppressConfig {
    fn default() -> Self {
        // On, with the voice model: it removes background without the
        // thinning spectral subtraction causes, so there is no reason to
        // make everyone find the switch.
        Self { enabled: true, strength: 1.0, neural: true }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NoiseGateConfig {
    pub enabled: bool,
    pub threshold_db: f32,
    pub attack_ms: f32,
    pub hold_ms: f32,
    pub release_ms: f32,
}

impl Default for NoiseGateConfig {
    fn default() -> Self {
        Self { enabled: true, threshold_db: -45.0, attack_ms: 5.0, hold_ms: 150.0, release_ms: 120.0 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HighPassConfig {
    pub enabled: bool,
    pub cutoff_hz: f32,
}

impl Default for HighPassConfig {
    fn default() -> Self {
        Self { enabled: true, cutoff_hz: 80.0 } // spec §1 default
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LowPassConfig {
    pub enabled: bool,
    pub cutoff_hz: f32,
}

impl Default for LowPassConfig {
    fn default() -> Self {
        Self { enabled: false, cutoff_hz: 12_000.0 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DeEsserConfig {
    pub enabled: bool,
    /// Hz -- the crossover between "untouched voice body" and "sibilance
    /// band that gets compressed".
    pub split_hz: f32,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

impl Default for DeEsserConfig {
    fn default() -> Self {
        Self { enabled: true, split_hz: 4000.0, threshold_db: -26.0, ratio: 4.0, attack_ms: 3.0, release_ms: 40.0 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct EqBandConfig {
    pub enabled: bool,
    pub center_hz: f32,
    pub q: f32,
    pub gain_db: f32,
    /// Bell / low shelf / high shelf / notch. Defaulted so presets and
    /// configs written before band types existed still load, as bells --
    /// which is what they were.
    #[serde(default)]
    pub band_type: BandType,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CompressorConfig {
    pub enabled: bool,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_gain_db: f32,
}

impl Default for CompressorConfig {
    fn default() -> Self {
        Self { enabled: true, threshold_db: -22.0, ratio: 3.0, attack_ms: 8.0, release_ms: 120.0, makeup_gain_db: 4.0 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LimiterConfig {
    pub enabled: bool,
    pub ceiling_db: f32,
    pub release_ms: f32,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        Self { enabled: true, ceiling_db: -1.0, release_ms: 60.0 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "mode")]
pub enum GainConfig {
    Fixed { enabled: bool, gain_db: f32 },
    Auto { enabled: bool, target_db: f32, min_gain_db: f32, max_gain_db: f32, adapt_time_ms: f32 },
}

impl Default for GainConfig {
    fn default() -> Self {
        GainConfig::Auto { enabled: true, target_db: -18.0, min_gain_db: -12.0, max_gain_db: 24.0, adapt_time_ms: 500.0 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PitchShiftConfig {
    pub enabled: bool,
    pub semitones: f32,
    /// Keep the vocal-tract resonances where they were instead of dragging
    /// them along with the pitch -- the difference between "chipmunk" and
    /// "same voice, different pitch". Costs a 24-band filter bank; see
    /// `stages::formant`. Defaults on, since the natural result is what
    /// people expect and the artificial one is the special effect.
    #[serde(default = "default_preserve_formants")]
    pub preserve_formants: bool,
}

fn default_preserve_formants() -> bool {
    true
}

impl Default for PitchShiftConfig {
    fn default() -> Self {
        Self { enabled: false, semitones: 0.0, preserve_formants: true }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReverbConfig {
    pub enabled: bool,
    pub room_size: f32,
    pub damping: f32,
    pub wet_dry: f32,
}

impl Default for ReverbConfig {
    fn default() -> Self {
        Self { enabled: false, room_size: 0.5, damping: 0.5, wet_dry: 0.3 }
    }
}

/// A whole chain setup, save-able as a named preset (spec §1 "Voice
/// Presets / Chains" -- export it, share it, load someone else's).
///
/// `#[serde(default)]` on the newer fields so a preset saved before they
/// existed still loads with sensible defaults instead of failing to parse.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ChainConfig {
    pub name: String,
    /// Spectral noise suppression (spec §1). Defaulted so a config
    /// written before it existed loads with it off, which is what the
    /// absence of the field means.
    #[serde(default)]
    pub noise_suppress: NoiseSuppressConfig,
    pub noise_gate: NoiseGateConfig,
    pub high_pass: HighPassConfig,
    pub low_pass: LowPassConfig,
    #[serde(default)]
    pub de_esser: DeEsserConfig,
    pub eq_bands: Vec<EqBandConfig>,
    pub compressor: CompressorConfig,
    pub limiter: LimiterConfig,
    pub gain: GainConfig,
    #[serde(default)]
    pub pitch_shift: PitchShiftConfig,
    #[serde(default)]
    pub reverb: ReverbConfig,
    /// Spec §1 "Plugin Host": the user's own LADSPA / LV2 / VST3 plugins,
    /// run in this order after the tone stages and before pitch shift and
    /// reverb. Desktops only; a phone keeps them in the preset and skips
    /// them.
    #[serde(default)]
    pub plugins: Vec<crate::plugins::PluginConfig>,
}

impl ChainConfig {
    /// The spec's "good default" preset for people who don't want to build one.
    pub fn default_preset() -> Self {
        Self { name: "Default".to_string(), ..Default::default() }
    }
}

#[cfg(test)]
mod curve_tests {
    use super::*;

    #[test]
    fn log_spaced_frequencies_covers_the_requested_range() {
        let freqs = log_spaced_frequencies(20.0, 20_000.0, 10);
        assert_eq!(freqs.len(), 10);
        assert!((freqs[0] - 20.0).abs() < 0.01);
        assert!((freqs[9] - 20_000.0).abs() < 1.0);
        // Log spacing: each step multiplies by the same ratio, so the gaps
        // grow -- a linear axis would have equal gaps instead.
        assert!(freqs[1] - freqs[0] < freqs[9] - freqs[8]);
    }

    #[test]
    fn every_advertised_preset_exists() {
        for name in eq_preset_names() {
            assert!(eq_preset(name).is_some(), "{name} is offered but not defined");
        }
    }

    #[test]
    fn an_unknown_preset_is_none_rather_than_a_panic() {
        assert!(eq_preset("Nonexistent").is_none());
        assert!(eq_preset("").is_none());
    }

    #[test]
    fn no_starter_preset_is_drastic() {
        // A starter preset that obviously colours the voice teaches people
        // to distrust the EQ entirely.
        for name in eq_preset_names() {
            for b in eq_preset(name).unwrap() {
                assert!(b.gain_db.abs() <= 6.0, "{name} moves {} dB at {} Hz", b.gain_db, b.center_hz);
            }
        }
    }

    #[test]
    fn thin_mic_rescue_actually_adds_low_end() {
        let bands = eq_preset("Thin Mic Rescue").unwrap();
        let curve = eq_curve_db(&bands, 48_000, &[80.0, 1000.0]);
        assert!(curve[0] > 4.0, "expected a real low boost, got {} dB", curve[0]);
    }

    #[test]
    fn broadcast_scoops_the_mids_and_opens_the_top() {
        let bands = eq_preset("Broadcast").unwrap();
        let curve = eq_curve_db(&bands, 48_000, &[400.0, 9000.0]);
        assert!(curve[0] < 0.0, "mids should be cut, got {} dB", curve[0]);
        assert!(curve[1] > 2.0, "top should be lifted, got {} dB", curve[1]);
    }

    #[test]
    fn presets_only_contain_enabled_bands() {
        // Loading a preset and hearing nothing because its bands arrived
        // switched off would be baffling.
        for name in eq_preset_names() {
            assert!(eq_preset(name).unwrap().iter().all(|b| b.enabled), "{name} has a disabled band");
        }
    }

    #[test]
    fn disabled_bands_dont_affect_the_curve() {
        let sample_rate = 48_000;
        let freqs = [1000.0];
        let flat = eq_curve_db(&[], sample_rate, &freqs);
        let with_disabled_boost =
            eq_curve_db(&[EqBandConfig { enabled: false, center_hz: 1000.0, q: 1.0, gain_db: 12.0, band_type: BandType::Bell }], sample_rate, &freqs);
        assert!((flat[0] - with_disabled_boost[0]).abs() < 0.01);
    }

    #[test]
    fn two_enabled_boosts_at_the_same_frequency_add_up() {
        let sample_rate = 48_000;
        let freqs = [1000.0];
        let one_band = eq_curve_db(&[EqBandConfig { enabled: true, center_hz: 1000.0, q: 1.0, gain_db: 6.0, band_type: BandType::Bell }], sample_rate, &freqs);
        let two_bands = eq_curve_db(
            &[
                EqBandConfig { enabled: true, center_hz: 1000.0, q: 1.0, gain_db: 6.0, band_type: BandType::Bell },
                EqBandConfig { enabled: true, center_hz: 1000.0, q: 1.0, gain_db: 6.0, band_type: BandType::Bell },
            ],
            sample_rate,
            &freqs,
        );
        assert!((two_bands[0] - one_band[0] * 2.0).abs() < 0.5, "two identical +6dB boosts should sum to ~+12dB, got {}", two_bands[0]);
    }
}
