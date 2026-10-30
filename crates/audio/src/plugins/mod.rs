//! Spec §1 "Plugin Host": your own LADSPA, LV2 and VST3 audio plugins in
//! the microphone chain (desktops only).
//!
//! A plugin is found by [`scan`] in the places each format's plugins are
//! installed, chosen by the user, and saved in the chain's preset as a
//! [`PluginConfig`]: which file, which plugin inside it, and the values of
//! its parameters. [`load`] turns that back into a [`Stage`] the chain runs
//! like its own; the chain runs plugins after its tone stages (EQ,
//! compressor, limiter, level) and before pitch shift and reverb.
//!
//! **Mono.** The chain is one channel. A plugin is fed it on every audio
//! input and the average of its audio outputs comes back, so stereo
//! plugins work too; one with no audio input (a synth) is refused.
//!
//! **Trust.** A plugin is native code running inside the app with the
//! same rights as the app, which is how every plugin host works and why
//! nothing here loads a plugin the user didn't pick. A plugin that fails
//! to load says why ([`PluginError`]) and the chain runs without it; a
//! plugin that crashes takes the app with it, which this host cannot
//! prevent without running plugins in another process.

// The loaders: desktops only. The types above them are everywhere, so a
// preset saved on a desktop loads on a phone (which keeps its plugins in
// the list and skips them).
#[cfg(desktop)]
mod ladspa;
#[cfg(desktop)]
mod lv2;
#[cfg(desktop)]
mod ttl;
#[cfg(desktop)]
mod vst3;

#[cfg(desktop)]
use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::stages::Stage;

/// The plugin standards the host speaks.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PluginFormat {
    /// The original Linux standard: one shared library, many plugins,
    /// float parameters only.
    Ladspa,
    /// LADSPA's successor: a bundle directory described in Turtle files
    /// beside the library.
    Lv2,
    /// Steinberg's standard, the common one on Windows and macOS.
    Vst3,
}

impl PluginFormat {
    /// The name people know it by.
    pub fn label(self) -> &'static str {
        match self {
            PluginFormat::Ladspa => "LADSPA",
            PluginFormat::Lv2 => "LV2",
            PluginFormat::Vst3 => "VST3",
        }
    }
}

/// One parameter a plugin offers.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ParamInfo {
    /// Stable within the plugin: a port symbol, index or parameter id.
    pub id: String,
    /// What the plugin calls it.
    pub name: String,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    /// An on/off switch rather than a range.
    pub toggle: bool,
}

/// A plugin found on this computer.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginInfo {
    pub format: PluginFormat,
    /// The library (LADSPA), bundle directory (LV2) or `.vst3` bundle.
    pub path: PathBuf,
    /// Which plugin inside it: the LADSPA label, the LV2 URI, the VST3
    /// class id.
    pub id: String,
    pub name: String,
    pub maker: String,
    pub params: Vec<ParamInfo>,
}

impl PluginInfo {
    /// A chain entry for this plugin, on, with every parameter at its
    /// default.
    pub fn to_config(&self) -> PluginConfig {
        PluginConfig {
            format: self.format,
            path: self.path.clone(),
            id: self.id.clone(),
            name: self.name.clone(),
            enabled: true,
            params: self.params.iter().map(|p| ParamValue { id: p.id.clone(), value: p.default }).collect(),
            described: self.params.clone(),
        }
    }
}

/// A parameter's saved value.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ParamValue {
    pub id: String,
    pub value: f32,
}

/// A plugin in the chain, as a preset saves it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PluginConfig {
    pub format: PluginFormat,
    pub path: PathBuf,
    pub id: String,
    /// Shown even when the plugin is missing, so the list can say which.
    pub name: String,
    pub enabled: bool,
    /// Parameters the user moved; the rest stay at the plugin's default.
    #[serde(default)]
    pub params: Vec<ParamValue>,
    /// What each parameter is (name, range), as found when it was added,
    /// so the settings can show sliders without loading the plugin.
    #[serde(default)]
    pub described: Vec<ParamInfo>,
}

impl PluginConfig {
    /// The saved value of parameter `id`, if there is one.
    pub fn param(&self, id: &str) -> Option<f32> {
        self.params.iter().find(|p| p.id == id).map(|p| p.value)
    }

    /// Sets parameter `id` to `value`, kept inside its range when the
    /// range is known.
    pub fn set_param(&mut self, id: &str, value: f32) {
        let value = match self.described.iter().find(|p| p.id == id) {
            Some(p) => value.clamp(p.min.min(p.max), p.max.max(p.min)),
            None => value,
        };
        match self.params.iter_mut().find(|p| p.id == id) {
            Some(p) => p.value = value,
            None => self.params.push(ParamValue { id: id.to_string(), value }),
        }
    }
}

/// Why a plugin didn't load.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PluginError {
    #[error("{0} isn't there any more")]
    Missing(PathBuf),
    #[error("couldn't open {path}: {reason}")]
    Open { path: PathBuf, reason: String },
    #[error("{0} doesn't contain that plugin")]
    NotFound(String),
    #[error("{0} has no audio input, so it can't process a microphone")]
    NoAudioInput(String),
    #[error("{name} needs {feature}, which this host doesn't provide")]
    Unsupported { name: String, feature: String },
    #[error("{0} refused to start")]
    Instantiate(String),
    #[error("plugins aren't supported on this system")]
    NotOnThisSystem,
}

/// Every plugin installed in the usual places for each format, sorted by
/// name. Libraries that fail to load are skipped.
pub fn scan() -> Vec<PluginInfo> {
    #[cfg(not(desktop))]
    return Vec::new();
    #[cfg(desktop)]
    scan_desktop()
}

/// The plugins of one format in one folder, as [`scan`] finds them; for
/// a folder the user points at.
#[cfg(desktop)]
pub fn scan_in(format: PluginFormat, dir: &Path) -> Vec<PluginInfo> {
    match format {
        PluginFormat::Ladspa => ladspa::scan_dir(dir),
        PluginFormat::Lv2 => lv2::scan_dir(dir),
        PluginFormat::Vst3 => vst3::scan_dir(dir),
    }
}

#[cfg(desktop)]
fn scan_desktop() -> Vec<PluginInfo> {
    let mut found = Vec::new();
    for dir in ladspa::search_path() {
        found.extend(ladspa::scan_dir(&dir));
    }
    for dir in lv2::search_path() {
        found.extend(lv2::scan_dir(&dir));
    }
    for dir in vst3::search_path() {
        found.extend(vst3::scan_dir(&dir));
    }
    found.sort_by_key(|p| p.name.to_lowercase());
    found.dedup_by(|a, b| a.format == b.format && a.id == b.id);
    found
}

/// Starts the plugin `config` names at `sample_rate`, with its saved
/// parameter values, as a chain stage.
pub fn load(config: &PluginConfig, sample_rate: u32) -> Result<Box<dyn Stage>, PluginError> {
    if !config.path.exists() {
        return Err(PluginError::Missing(config.path.clone()));
    }
    #[cfg(not(desktop))]
    {
        let _ = sample_rate;
        Err(PluginError::NotOnThisSystem)
    }
    #[cfg(desktop)]
    match config.format {
        PluginFormat::Ladspa => ladspa::load(config, sample_rate),
        PluginFormat::Lv2 => lv2::load(config, sample_rate),
        PluginFormat::Vst3 => vst3::load(config, sample_rate),
    }
}

#[cfg(desktop)]
/// The directories named in environment variable `var` (a `:`-separated
/// list, `;` on Windows), or `defaults` when it is unset, keeping only
/// those that exist.
fn dirs_from_env(var: &str, defaults: Vec<PathBuf>) -> Vec<PathBuf> {
    let listed = std::env::var_os(var).map(|v| std::env::split_paths(&v).collect::<Vec<_>>());
    listed.unwrap_or(defaults).into_iter().filter(|d| d.is_dir()).collect()
}

#[cfg(desktop)]
/// The user's home directory, for the per-user plugin folders.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

#[cfg(desktop)]
/// The files in `dir` (not recursing) whose extension is `ext`.
fn files_with_extension(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<PathBuf> =
        entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext))).collect();
    out.sort();
    out
}

#[cfg(desktop)]
/// Runs a plugin's multi-channel audio from the chain's one channel:
/// every input gets the block, the outputs are averaged back. The buffers
/// grow to the largest block seen and are reused, so a steady stream
/// never allocates.
struct MonoAdapter {
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
}

#[cfg(desktop)]
impl MonoAdapter {
    fn new(inputs: usize, outputs: usize) -> Self {
        Self { inputs: vec![Vec::new(); inputs], outputs: vec![Vec::new(); outputs] }
    }

    /// Fills every input with `samples` and sizes every output to match.
    /// Returns whether any buffer moved (they must be connected again).
    fn prepare(&mut self, samples: &[f32]) -> bool {
        let n = samples.len();
        let mut moved = false;
        for b in self.inputs.iter_mut().chain(self.outputs.iter_mut()) {
            if b.capacity() < n {
                moved = true;
            }
            let before = b.as_ptr();
            b.resize(n, 0.0);
            moved |= before != b.as_ptr();
        }
        for b in &mut self.inputs {
            b.copy_from_slice(samples);
        }
        moved
    }

    /// Averages the outputs into `samples`.
    fn finish(&self, samples: &mut [f32]) {
        if self.outputs.is_empty() {
            return;
        }
        let scale = 1.0 / self.outputs.len() as f32;
        for (i, s) in samples.iter_mut().enumerate() {
            *s = self.outputs.iter().map(|o| o[i]).sum::<f32>() * scale;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_keeps_its_parameters_and_reads_them_back() {
        let info = PluginInfo {
            format: PluginFormat::Ladspa,
            path: "/x.so".into(),
            id: "amp".into(),
            name: "Amp".into(),
            maker: String::new(),
            params: vec![ParamInfo { id: "0".into(), name: "Gain".into(), min: 0.0, max: 2.0, default: 1.0, toggle: false }],
        };
        let mut config = info.to_config();
        assert_eq!(config.param("0"), Some(1.0));
        config.params[0].value = 1.5;
        let json = serde_json::to_string(&config).unwrap();
        let mut back: PluginConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.param("0"), Some(1.5));
        assert_eq!(back.param("nope"), None);
        back.set_param("0", 9.0);
        assert_eq!(back.param("0"), Some(2.0), "kept in range");
    }

    #[test]
    fn a_missing_plugin_says_so() {
        let config = PluginConfig {
            format: PluginFormat::Lv2,
            path: "/no/such/bundle.lv2".into(),
            id: "urn:x".into(),
            name: "X".into(),
            enabled: true,
            params: vec![],
            described: vec![],
        };
        assert_eq!(load(&config, 48_000).err(), Some(PluginError::Missing("/no/such/bundle.lv2".into())));
    }

    #[cfg(desktop)]
    #[test]
    fn stereo_outputs_are_averaged_back_to_mono() {
        let mut a = MonoAdapter::new(2, 2);
        a.prepare(&[1.0, 2.0]);
        assert_eq!(a.inputs, vec![vec![1.0, 2.0]; 2]);
        a.outputs[0].copy_from_slice(&[1.0, 0.0]);
        a.outputs[1].copy_from_slice(&[0.0, 1.0]);
        let mut out = [9.0, 9.0];
        a.finish(&mut out);
        assert_eq!(out, [0.5, 0.5]);
    }
}
