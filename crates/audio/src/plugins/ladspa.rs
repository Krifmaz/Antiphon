//! LADSPA plugins: a shared library exporting `ladspa_descriptor(index)`,
//! which hands out one C descriptor per plugin until it returns null.
//! Ports are audio or control, input or output, all `float`; parameters
//! are the control inputs, identified by port index.

use std::ffi::{c_char, c_int, c_ulong, c_void, CStr};
use std::path::{Path, PathBuf};

use super::{dirs_from_env, files_with_extension, home, MonoAdapter, ParamInfo, PluginConfig, PluginError, PluginFormat, PluginInfo};
use crate::stages::Stage;

const PORT_INPUT: c_int = 0x1;
const PORT_OUTPUT: c_int = 0x2;
const PORT_CONTROL: c_int = 0x4;
const PORT_AUDIO: c_int = 0x8;

const HINT_BOUNDED_BELOW: c_int = 0x1;
const HINT_BOUNDED_ABOVE: c_int = 0x2;
const HINT_TOGGLED: c_int = 0x4;
const HINT_SAMPLE_RATE: c_int = 0x8;
const HINT_LOGARITHMIC: c_int = 0x10;
const HINT_INTEGER: c_int = 0x20;
const HINT_DEFAULT_MASK: c_int = 0x3C0;
const HINT_DEFAULT_MINIMUM: c_int = 0x40;
const HINT_DEFAULT_LOW: c_int = 0x80;
const HINT_DEFAULT_MIDDLE: c_int = 0xC0;
const HINT_DEFAULT_HIGH: c_int = 0x100;
const HINT_DEFAULT_MAXIMUM: c_int = 0x140;
const HINT_DEFAULT_0: c_int = 0x200;
const HINT_DEFAULT_1: c_int = 0x240;
const HINT_DEFAULT_100: c_int = 0x280;
const HINT_DEFAULT_440: c_int = 0x2C0;

type Handle = *mut c_void;

#[repr(C)]
struct RangeHint {
    hint: c_int,
    lower: f32,
    upper: f32,
}

/// `LADSPA_Descriptor`, field for field.
#[repr(C)]
struct Descriptor {
    unique_id: c_ulong,
    label: *const c_char,
    properties: c_int,
    name: *const c_char,
    maker: *const c_char,
    copyright: *const c_char,
    port_count: c_ulong,
    port_descriptors: *const c_int,
    port_names: *const *const c_char,
    port_range_hints: *const RangeHint,
    implementation_data: *mut c_void,
    instantiate: Option<unsafe extern "C" fn(*const Descriptor, c_ulong) -> Handle>,
    connect_port: Option<unsafe extern "C" fn(Handle, c_ulong, *mut f32)>,
    activate: Option<unsafe extern "C" fn(Handle)>,
    run: Option<unsafe extern "C" fn(Handle, c_ulong)>,
    run_adding: Option<unsafe extern "C" fn(Handle, c_ulong)>,
    set_run_adding_gain: Option<unsafe extern "C" fn(Handle, f32)>,
    deactivate: Option<unsafe extern "C" fn(Handle)>,
    cleanup: Option<unsafe extern "C" fn(Handle)>,
}

type DescriptorFn = unsafe extern "C" fn(c_ulong) -> *const Descriptor;

/// Where LADSPA plugins live: `$LADSPA_PATH`, else the usual folders.
pub(super) fn search_path() -> Vec<PathBuf> {
    let mut defaults: Vec<PathBuf> =
        ["/usr/lib/ladspa", "/usr/local/lib/ladspa", "/usr/lib64/ladspa", "/usr/lib/x86_64-linux-gnu/ladspa", "/usr/lib/aarch64-linux-gnu/ladspa"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
    if let Some(home) = home() {
        defaults.push(home.join(".ladspa"));
    }
    if cfg!(target_os = "macos") {
        defaults.push("/Library/Audio/Plug-Ins/LADSPA".into());
    }
    dirs_from_env("LADSPA_PATH", defaults)
}

fn library_extension() -> &'static str {
    if cfg!(target_os = "windows") {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

/// Every plugin in every library in `dir`.
pub(super) fn scan_dir(dir: &Path) -> Vec<PluginInfo> {
    files_with_extension(dir, library_extension()).iter().flat_map(|lib| scan_library(lib)).collect()
}

/// Copies a C string, or empty for null.
///
/// # Safety
/// `p` is null or a NUL-terminated string that outlives the call.
unsafe fn text(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// An opened library and its descriptor function.
struct Library {
    lib: libloading::Library,
}

impl Library {
    fn open(path: &Path) -> Result<Self, PluginError> {
        // SAFETY: loading a plugin runs its initialisers; that is what the
        // user asked for by installing and choosing it.
        let lib = unsafe { libloading::Library::new(path) }
            .map_err(|e| PluginError::Open { path: path.to_path_buf(), reason: e.to_string() })?;
        Ok(Self { lib })
    }

    /// Every descriptor the library hands out.
    fn descriptors(&self) -> Vec<&Descriptor> {
        // SAFETY: the symbol is LADSPA's entry point with this signature;
        // descriptors stay valid while the library is loaded, which `self`
        // keeps it.
        let Ok(f) = (unsafe { self.lib.get::<DescriptorFn>(b"ladspa_descriptor\0") }) else { return Vec::new() };
        let mut out = Vec::new();
        for i in 0.. {
            let d = unsafe { f(i) };
            if d.is_null() || i > 4096 {
                break;
            }
            out.push(unsafe { &*d });
        }
        out
    }
}

/// One port, read out of a descriptor.
struct Port {
    index: usize,
    kind: c_int,
    name: String,
    hint: c_int,
    lower: f32,
    upper: f32,
}

impl Port {
    fn is(&self, bits: c_int) -> bool {
        self.kind & bits == bits
    }

    /// The control's range and default at `rate`, per the LADSPA hints.
    fn param(&self, rate: f32) -> ParamInfo {
        let scale = if self.hint & HINT_SAMPLE_RATE != 0 { rate } else { 1.0 };
        let toggle = self.hint & HINT_TOGGLED != 0;
        let (mut min, mut max) = (
            if self.hint & HINT_BOUNDED_BELOW != 0 { self.lower * scale } else { 0.0 },
            if self.hint & HINT_BOUNDED_ABOVE != 0 { self.upper * scale } else { 1.0 },
        );
        if toggle {
            (min, max) = (0.0, 1.0);
        }
        if max < min {
            std::mem::swap(&mut min, &mut max);
        }
        let log = self.hint & HINT_LOGARITHMIC != 0 && min > 0.0 && max > 0.0;
        let between = |t: f32| if log { (min.ln() * (1.0 - t) + max.ln() * t).exp() } else { min * (1.0 - t) + max * t };
        let mut default = match self.hint & HINT_DEFAULT_MASK {
            HINT_DEFAULT_MINIMUM => min,
            HINT_DEFAULT_LOW => between(0.25),
            HINT_DEFAULT_MIDDLE => between(0.5),
            HINT_DEFAULT_HIGH => between(0.75),
            HINT_DEFAULT_MAXIMUM => max,
            HINT_DEFAULT_0 => 0.0,
            HINT_DEFAULT_1 => 1.0,
            HINT_DEFAULT_100 => 100.0,
            HINT_DEFAULT_440 => 440.0,
            _ => min.max(0.0).min(max),
        };
        if self.hint & HINT_INTEGER != 0 {
            default = default.round();
        }
        ParamInfo { id: self.index.to_string(), name: self.name.clone(), min, max, default, toggle }
    }
}

/// The ports of a descriptor.
fn ports(d: &Descriptor) -> Vec<Port> {
    (0..d.port_count as usize)
        .map(|i| {
            // SAFETY: LADSPA guarantees `port_count` entries in each array.
            let kind = unsafe { *d.port_descriptors.add(i) };
            let name = unsafe { text(*d.port_names.add(i)) };
            let hint = unsafe { &*d.port_range_hints.add(i) };
            Port { index: i, kind, name, hint: hint.hint, lower: hint.lower, upper: hint.upper }
        })
        .collect()
}

fn info(path: &Path, d: &Descriptor) -> PluginInfo {
    PluginInfo {
        format: PluginFormat::Ladspa,
        path: path.to_path_buf(),
        // SAFETY: the descriptor's strings live as long as the library.
        id: unsafe { text(d.label) },
        name: unsafe { text(d.name) },
        maker: unsafe { text(d.maker) },
        params: ports(d)
            .iter()
            .filter(|p| p.is(PORT_INPUT | PORT_CONTROL))
            .map(|p| p.param(48_000.0))
            .collect(),
    }
}

/// The plugins in one library that can process a microphone.
pub(super) fn scan_library(path: &Path) -> Vec<PluginInfo> {
    let Ok(lib) = Library::open(path) else { return Vec::new() };
    lib.descriptors()
        .into_iter()
        .filter(|d| ports(d).iter().any(|p| p.is(PORT_INPUT | PORT_AUDIO)) && d.instantiate.is_some() && d.run.is_some())
        .map(|d| info(path, d))
        .collect()
}

/// A running LADSPA plugin.
struct LadspaStage {
    handle: Handle,
    descriptor: *const Descriptor,
    /// Control values, one per port index (unused for audio ports); the
    /// plugin reads them through the pointers given at connect time, so
    /// this never reallocates after `load`. Only ever read by the plugin.
    _controls: Box<[f32]>,
    audio_in: Vec<usize>,
    audio_out: Vec<usize>,
    adapter: MonoAdapter,
    /// Dropped last: the code above lives in it.
    _lib: Library,
}

// SAFETY: a LADSPA instance may be used from any thread as long as calls
// aren't concurrent; the chain owns the stage and calls it from one
// thread at a time.
unsafe impl Send for LadspaStage {}

impl LadspaStage {
    fn descriptor(&self) -> &Descriptor {
        // SAFETY: valid while `_lib` is loaded.
        unsafe { &*self.descriptor }
    }

    fn connect_audio(&mut self) {
        let connect = self.descriptor().connect_port.expect("checked at load");
        for (k, &port) in self.audio_in.iter().enumerate() {
            unsafe { connect(self.handle, port as c_ulong, self.adapter.inputs[k].as_mut_ptr()) };
        }
        for (k, &port) in self.audio_out.iter().enumerate() {
            unsafe { connect(self.handle, port as c_ulong, self.adapter.outputs[k].as_mut_ptr()) };
        }
    }
}

impl Stage for LadspaStage {
    fn name(&self) -> &'static str {
        "plugin"
    }

    fn process(&mut self, samples: &mut [f32]) {
        if samples.is_empty() {
            return;
        }
        if self.adapter.prepare(samples) {
            self.connect_audio();
        }
        let run = self.descriptor().run.expect("checked at load");
        // SAFETY: every port is connected to a buffer of at least
        // `samples.len()` floats.
        unsafe { run(self.handle, samples.len() as c_ulong) };
        self.adapter.finish(samples);
    }
}

impl Drop for LadspaStage {
    fn drop(&mut self) {
        let d = self.descriptor();
        // SAFETY: the instance was activated in `load` and is used no more.
        unsafe {
            if let Some(deactivate) = d.deactivate {
                deactivate(self.handle);
            }
            if let Some(cleanup) = d.cleanup {
                cleanup(self.handle);
            }
        }
    }
}

pub(super) fn load(config: &PluginConfig, sample_rate: u32) -> Result<Box<dyn Stage>, PluginError> {
    let lib = Library::open(&config.path)?;
    let descriptor: *const Descriptor = lib
        .descriptors()
        .into_iter()
        // SAFETY: the label lives as long as the library.
        .find(|d| unsafe { text(d.label) } == config.id)
        .ok_or_else(|| PluginError::NotFound(config.path.display().to_string()))?;
    let d = unsafe { &*descriptor };
    let name = unsafe { text(d.name) };
    let (Some(instantiate), Some(connect), Some(_run)) = (d.instantiate, d.connect_port, d.run) else {
        return Err(PluginError::Instantiate(name));
    };
    let ports = ports(d);
    let audio_in: Vec<usize> = ports.iter().filter(|p| p.is(PORT_INPUT | PORT_AUDIO)).map(|p| p.index).collect();
    let audio_out: Vec<usize> = ports.iter().filter(|p| p.is(PORT_OUTPUT | PORT_AUDIO)).map(|p| p.index).collect();
    if audio_in.is_empty() {
        return Err(PluginError::NoAudioInput(name));
    }
    // SAFETY: a valid descriptor and a positive rate.
    let handle = unsafe { instantiate(descriptor, c_ulong::from(sample_rate)) };
    if handle.is_null() {
        return Err(PluginError::Instantiate(name));
    }
    let mut controls = vec![0.0f32; ports.len()].into_boxed_slice();
    for p in ports.iter().filter(|p| p.is(PORT_CONTROL)) {
        if p.is(PORT_INPUT) {
            let info = p.param(sample_rate as f32);
            controls[p.index] = config.param(&info.id).unwrap_or(info.default).clamp(info.min, info.max);
        }
        // Outputs (meters) are connected too: a plugin may write them.
        unsafe { connect(handle, p.index as c_ulong, &mut controls[p.index]) };
    }
    let mut stage = LadspaStage {
        handle,
        descriptor,
        _controls: controls,
        adapter: MonoAdapter::new(audio_in.len(), audio_out.len()),
        audio_in,
        audio_out,
        _lib: lib,
    };
    // Connected once with an empty block's buffers so a plugin that reads
    // its ports during activate sees valid (if empty) pointers.
    stage.adapter.prepare(&[0.0; 64]);
    stage.connect_audio();
    if let Some(activate) = d.activate {
        unsafe { activate(handle) };
    }
    Ok(Box::new(stage))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(hint: c_int, lower: f32, upper: f32) -> Port {
        Port { index: 3, kind: PORT_INPUT | PORT_CONTROL, name: "Freq".into(), hint, lower, upper }
    }

    #[test]
    fn defaults_follow_the_hints() {
        let bounded = HINT_BOUNDED_BELOW | HINT_BOUNDED_ABOVE;
        assert_eq!(port(bounded | HINT_DEFAULT_MIDDLE, 0.0, 10.0).param(48_000.0).default, 5.0);
        assert_eq!(port(bounded | HINT_DEFAULT_440, 20.0, 20_000.0).param(48_000.0).default, 440.0);
        let log = port(bounded | HINT_LOGARITHMIC | HINT_DEFAULT_MIDDLE, 10.0, 1000.0).param(48_000.0);
        assert!((log.default - 100.0).abs() < 0.01, "geometric middle, got {}", log.default);
        let rate = port(bounded | HINT_SAMPLE_RATE | HINT_DEFAULT_MAXIMUM, 0.0, 0.5).param(48_000.0);
        assert_eq!((rate.max, rate.default), (24_000.0, 24_000.0));
        let toggle = port(HINT_TOGGLED | HINT_DEFAULT_1, 0.0, 0.0).param(48_000.0);
        assert!(toggle.toggle);
        assert_eq!((toggle.min, toggle.max, toggle.default), (0.0, 1.0, 1.0));
        assert_eq!(port(bounded | HINT_INTEGER | HINT_DEFAULT_LOW, 0.0, 5.0).param(48_000.0).default, 1.0);
        assert_eq!(port(bounded, 0.0, 5.0).param(48_000.0).id, "3");
    }
}
