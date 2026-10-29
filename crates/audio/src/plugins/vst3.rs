//! VST3 plugins: a `.vst3` bundle (a folder on Linux and macOS, a folder
//! or a single DLL on Windows) whose library exports `GetPluginFactory`.
//! The factory lists classes; an "Audio Module Class" is a plugin, made of
//! a processor (`IComponent` + `IAudioProcessor`) and an edit controller
//! (`IEditController`, either the same object or a class of its own) that
//! describes the parameters. Parameters are identified by their numeric
//! id and saved as normalised values, 0 to 1, which is how VST3 itself
//! passes them; they reach the processor through the first block's
//! parameter changes.
//!
//! The VST3 SDK is MIT-licensed since 2025, and the bindings used here
//! ([`vst3`]) are generated from its headers.

use std::cell::Cell;
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::ptr;

use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper, Interface};

use super::{dirs_from_env, home, ParamInfo, PluginConfig, PluginError, PluginFormat, PluginInfo};
use crate::stages::Stage;

/// The most samples one `process` gets; longer blocks are split.
const MAX_BLOCK: usize = 4096;

/// Where VST3 bundles live, per platform (`$VST3_PATH` overrides).
pub(super) fn search_path() -> Vec<PathBuf> {
    let mut defaults = Vec::new();
    if cfg!(target_os = "windows") {
        if let Some(dir) = std::env::var_os("COMMONPROGRAMFILES") {
            defaults.push(PathBuf::from(dir).join("VST3"));
        }
        if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
            defaults.push(PathBuf::from(dir).join("Programs/Common/VST3"));
        }
    } else if cfg!(target_os = "macos") {
        if let Some(home) = home() {
            defaults.push(home.join("Library/Audio/Plug-Ins/VST3"));
        }
        defaults.push("/Library/Audio/Plug-Ins/VST3".into());
    } else {
        if let Some(home) = home() {
            defaults.push(home.join(".vst3"));
        }
        defaults.push("/usr/lib/vst3".into());
        defaults.push("/usr/local/lib/vst3".into());
    }
    dirs_from_env("VST3_PATH", defaults)
}

/// Every `.vst3` bundle in `dir`, looking a few folders deep since
/// vendors often keep theirs in a folder of their own.
fn bundles(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("vst3")) {
            out.push(path);
        } else if path.is_dir() && depth > 0 {
            bundles(&path, depth - 1, out);
        }
    }
}

pub(super) fn scan_dir(dir: &Path) -> Vec<PluginInfo> {
    let mut found = Vec::new();
    bundles(dir, 3, &mut found);
    found.iter().flat_map(|b| scan_bundle(b)).collect()
}

/// The library inside a bundle, for this platform.
fn library_path(bundle: &Path) -> Option<PathBuf> {
    if bundle.is_file() {
        // A single-file plugin (older Windows layout).
        return Some(bundle.to_path_buf());
    }
    let stem = bundle.file_stem()?.to_string_lossy().into_owned();
    let contents = bundle.join("Contents");
    let path = if cfg!(target_os = "windows") {
        contents.join(if cfg!(target_arch = "aarch64") { "arm64-win" } else { "x86_64-win" }).join(format!("{stem}.vst3"))
    } else if cfg!(target_os = "macos") {
        contents.join("MacOS").join(&stem)
    } else {
        let arch = if cfg!(target_arch = "aarch64") { "aarch64-linux" } else { "x86_64-linux" };
        contents.join(arch).join(format!("{stem}.so"))
    };
    path.is_file().then_some(path)
}

/// A loaded VST3 library, entered and (on drop) exited the way the SDK
/// asks each platform to.
struct Module {
    /// Always `Some` until drop, which releases it before the exit call.
    factory: Option<ComPtr<IPluginFactory>>,
    lib: Option<libloading::Library>,
}

impl Module {
    fn open(bundle: &Path) -> Result<Self, PluginError> {
        let path = library_path(bundle).ok_or_else(|| PluginError::NotFound(bundle.display().to_string()))?;
        let open_err = |reason: String| PluginError::Open { path: path.clone(), reason };
        // SAFETY: loading the plugin the user chose runs its initialisers.
        let lib = unsafe { libloading::Library::new(&path) }.map_err(|e| open_err(e.to_string()))?;
        unsafe { enter(&lib, bundle) }.map_err(open_err)?;
        let get = unsafe { lib.get::<unsafe extern "system" fn() -> *mut IPluginFactory>(b"GetPluginFactory\0") }
            .map_err(|e| open_err(e.to_string()))?;
        // SAFETY: the factory is returned with a reference we now own.
        let factory = unsafe { ComPtr::from_raw(get()) }.ok_or_else(|| open_err("no plugin factory".into()))?;
        Ok(Self { factory: Some(factory), lib: Some(lib) })
    }

    /// The plugins (audio module classes) in this module.
    fn factory(&self) -> &ComPtr<IPluginFactory> {
        self.factory.as_ref().expect("released only on drop")
    }

    fn classes(&self) -> Vec<(TUID, String)> {
        let count = unsafe { self.factory().countClasses() };
        (0..count)
            .filter_map(|i| {
                let mut info: PClassInfo = unsafe { std::mem::zeroed() };
                if unsafe { self.factory().getClassInfo(i, &mut info) } != kResultOk {
                    return None;
                }
                (c_text(&info.category) == "Audio Module Class").then(|| (info.cid, c_text(&info.name)))
            })
            .collect()
    }

    /// Makes an instance of class `cid` as interface `I`.
    fn create<I: Interface>(&self, cid: &TUID) -> Option<ComPtr<I>> {
        let mut obj: *mut c_void = ptr::null_mut();
        let iid = I::IID;
        let r = unsafe { self.factory().createInstance(cid.as_ptr(), iid.as_ptr() as FIDString, &mut obj) };
        if r != kResultOk {
            return None;
        }
        unsafe { ComPtr::from_raw(obj as *mut I) }
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        // The factory goes first, then the exit function, then the library.
        self.factory = None;
        if let Some(lib) = self.lib.take() {
            // SAFETY: nothing from the module is used after this.
            unsafe { exit(&lib) };
        }
    }
}

/// Calls the module's entry function, as the SDK requires before
/// `GetPluginFactory`.
///
/// # Safety
/// `lib` is a VST3 module.
unsafe fn enter(lib: &libloading::Library, _bundle: &Path) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        // ModuleEntry gets the library handle (dlopen's).
        if let Ok(entry) = unsafe { lib.get::<unsafe extern "C" fn(*mut c_void) -> bool>(b"ModuleEntry\0") } {
            let handle = libc_handle(lib);
            if !unsafe { entry(handle) } {
                return Err("its ModuleEntry failed".into());
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(entry) = unsafe { lib.get::<unsafe extern "system" fn() -> bool>(b"InitDll\0") } {
            if !unsafe { entry() } {
                return Err("its InitDll failed".into());
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let entry = unsafe { lib.get::<unsafe extern "C" fn(*mut c_void) -> bool>(b"bundleEntry\0") }
            .or_else(|_| unsafe { lib.get::<unsafe extern "C" fn(*mut c_void) -> bool>(b"BundleEntry\0") });
        if let Ok(entry) = entry {
            let bundle_ref = mac::bundle_ref(_bundle);
            if !unsafe { entry(bundle_ref) } {
                return Err("its bundleEntry failed".into());
            }
        }
    }
    let _ = lib;
    Ok(())
}

/// The module's exit function, called before it is unloaded.
///
/// # Safety
/// `lib` is a VST3 module entered with [`enter`].
unsafe fn exit(lib: &libloading::Library) {
    let names: &[&[u8]] = if cfg!(target_os = "linux") {
        &[b"ModuleExit\0"]
    } else if cfg!(target_os = "windows") {
        &[b"ExitDll\0"]
    } else {
        &[b"bundleExit\0", b"BundleExit\0"]
    };
    for name in names {
        if let Ok(f) = unsafe { lib.get::<unsafe extern "C" fn() -> bool>(name) } {
            unsafe { f() };
            return;
        }
    }
}

/// The `dlopen` handle behind a loaded library, which `ModuleEntry` wants.
#[cfg(target_os = "linux")]
fn libc_handle(lib: &libloading::Library) -> *mut c_void {
    // libloading's unix Library is a thin wrapper round that handle.
    // SAFETY: same size and representation, read without taking it.
    unsafe { *(lib as *const libloading::Library as *const *mut c_void) }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;
    use std::path::Path;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(alloc: *const c_void, buf: *const u8, len: isize, dir: u8) -> *const c_void;
        fn CFBundleCreate(alloc: *const c_void, url: *const c_void) -> *mut c_void;
    }

    /// A `CFBundleRef` for the plugin's bundle, for its `bundleEntry`.
    /// Leaked deliberately: the module may keep it for its lifetime.
    pub(super) fn bundle_ref(bundle: &Path) -> *mut c_void {
        let bytes = bundle.as_os_str().as_encoded_bytes();
        // SAFETY: CoreFoundation calls with a valid buffer.
        unsafe {
            let url = CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, 1);
            if url.is_null() {
                return std::ptr::null_mut();
            }
            CFBundleCreate(std::ptr::null(), url)
        }
    }
}

/// A fixed C string field.
fn c_text(field: &[c_char]) -> String {
    let bytes: Vec<u8> = field.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A fixed UTF-16 string field.
fn w_text(field: &[TChar]) -> String {
    let len = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    String::from_utf16_lossy(&field[..len])
}

/// A class id as text, for saving which plugin a bundle's entry is.
fn cid_text(cid: &TUID) -> String {
    cid.iter().map(|b| format!("{:02X}", *b as u8)).collect()
}

fn cid_from_text(text: &str) -> Option<TUID> {
    if text.len() != 32 {
        return None;
    }
    let mut cid: TUID = [0; 16];
    for (i, out) in cid.iter_mut().enumerate() {
        *out = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()? as c_char;
    }
    Some(cid)
}

/// The host object plugins are initialised with: it gives the host's name
/// and makes nothing else.
struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        let name = unsafe { &mut *name };
        for (dst, src) in name.iter_mut().zip("Antiphon".encode_utf16().chain(std::iter::once(0))) {
            *dst = src;
        }
        kResultOk
    }

    unsafe fn createInstance(&self, _cid: *mut TUID, _iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        if !obj.is_null() {
            unsafe { *obj = ptr::null_mut() };
        }
        kResultFalse
    }
}

/// One parameter's value, as a change queue with a single point at the
/// start of the block.
struct ValueQueue {
    id: ParamID,
    value: f64,
}

impl Class for ValueQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ValueQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id
    }

    unsafe fn getPointCount(&self) -> int32 {
        1
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        if index != 0 {
            return kResultFalse;
        }
        unsafe {
            *offset = 0;
            *value = self.value;
        }
        kResultTrue
    }

    unsafe fn addPoint(&self, _offset: int32, _value: ParamValue, _index: *mut int32) -> tresult {
        kResultFalse
    }
}

/// The saved parameter values, delivered in the first block only: after
/// that the processor holds them.
struct Changes {
    queues: Vec<ComPtr<IParamValueQueue>>,
    pending: Cell<bool>,
}

impl Class for Changes {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for Changes {
    unsafe fn getParameterCount(&self) -> int32 {
        if self.pending.get() {
            self.queues.len() as int32
        } else {
            0
        }
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        self.queues.get(index as usize).map_or(ptr::null_mut(), ComPtr::as_ptr)
    }

    unsafe fn addParameterData(&self, _id: *const ParamID, _index: *mut int32) -> *mut IParamValueQueue {
        ptr::null_mut()
    }
}

/// A processor and its controller, initialised.
struct Instance {
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    /// `None` when the component is its own controller.
    controller: Option<ComPtr<IEditController>>,
    host: ComPtr<IHostApplication>,
}

impl Instance {
    fn new(module: &Module, cid: &TUID) -> Option<Self> {
        let host = ComWrapper::new(HostApp).to_com_ptr::<IHostApplication>()?;
        let component: ComPtr<IComponent> = module.create(cid)?;
        let context = host.as_ptr() as *mut FUnknown;
        if unsafe { component.initialize(context) } != kResultOk {
            return None;
        }
        let processor = component.cast::<IAudioProcessor>()?;
        let controller = if component.cast::<IEditController>().is_some() {
            None
        } else {
            let mut ccid: TUID = [0; 16];
            if unsafe { component.getControllerClassId(&mut ccid) } == kResultOk {
                module.create::<IEditController>(&ccid).filter(|c| unsafe { c.initialize(context) } == kResultOk)
            } else {
                None
            }
        };
        Some(Self { component, processor, controller, host })
    }

    fn controller(&self) -> Option<ComPtr<IEditController>> {
        self.controller.clone().or_else(|| self.component.cast::<IEditController>())
    }

    /// The parameters a user may set: not read-only, not hidden, not the
    /// bypass switch (the chain has its own).
    fn params(&self) -> Vec<ParamInfo> {
        let Some(c) = self.controller() else { return Vec::new() };
        let count = unsafe { c.getParameterCount() };
        let skip = ParameterInfo_::ParameterFlags_::kIsReadOnly | ParameterInfo_::ParameterFlags_::kIsHidden | ParameterInfo_::ParameterFlags_::kIsBypass;
        (0..count)
            .filter_map(|i| {
                let mut info: ParameterInfo = unsafe { std::mem::zeroed() };
                if unsafe { c.getParameterInfo(i, &mut info) } != kResultOk || info.flags & skip != 0 {
                    return None;
                }
                Some(ParamInfo {
                    id: info.id.to_string(),
                    name: w_text(&info.title),
                    min: 0.0,
                    max: 1.0,
                    default: info.defaultNormalizedValue as f32,
                    toggle: info.stepCount == 1,
                })
            })
            .collect()
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            if let Some(c) = &self.controller {
                c.terminate();
            }
            self.component.terminate();
        }
        let _ = &self.host;
    }
}

pub(super) fn scan_bundle(bundle: &Path) -> Vec<PluginInfo> {
    let Ok(module) = Module::open(bundle) else { return Vec::new() };
    let classes = module.classes();
    classes
        .into_iter()
        .filter_map(|(cid, name)| {
            let instance = Instance::new(&module, &cid)?;
            if audio_buses(&instance.component, BusDirections_::kInput) == 0 {
                return None;
            }
            let params = instance.params();
            drop(instance);
            Some(PluginInfo { format: PluginFormat::Vst3, path: bundle.to_path_buf(), id: cid_text(&cid), name, maker: String::new(), params })
        })
        .collect()
}

fn audio_buses(component: &ComPtr<IComponent>, dir: BusDirections) -> i32 {
    unsafe { component.getBusCount(MediaTypes_::kAudio as MediaType, dir as BusDirection) }
}

/// Buffers for one direction's buses: per bus, its channels.
struct Buses {
    channels: Vec<Vec<Vec<f32>>>,
    pointers: Vec<Vec<*mut f32>>,
    buffers: Vec<AudioBusBuffers>,
}

impl Buses {
    fn new(counts: &[usize]) -> Self {
        let channels: Vec<Vec<Vec<f32>>> = counts.iter().map(|&n| vec![vec![0.0; MAX_BLOCK]; n]).collect();
        let mut buses = Self { channels, pointers: Vec::new(), buffers: Vec::new() };
        buses.pointers = buses.channels.iter_mut().map(|bus| bus.iter_mut().map(|c| c.as_mut_ptr()).collect()).collect();
        buses.buffers = buses
            .pointers
            .iter_mut()
            .map(|p| AudioBusBuffers {
                numChannels: p.len() as int32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 { channelBuffers32: p.as_mut_ptr() },
            })
            .collect();
        buses
    }
}

/// A running VST3 plugin.
struct Vst3Stage {
    inputs: Buses,
    outputs: Buses,
    changes: ComPtr<IParameterChanges>,
    pending: *const Cell<bool>,
    instance: Option<Instance>,
    /// Dropped after the instance.
    _module: Module,
}

// SAFETY: the plugin's audio calls are made from one thread at a time,
// which the chain guarantees.
unsafe impl Send for Vst3Stage {}

impl Stage for Vst3Stage {
    fn name(&self) -> &'static str {
        "plugin"
    }

    fn process(&mut self, samples: &mut [f32]) {
        let Some(instance) = &self.instance else { return };
        for chunk in samples.chunks_mut(MAX_BLOCK) {
            let n = chunk.len();
            // The main input bus gets the microphone on every channel; any
            // other input bus (a sidechain) stays silent.
            if let Some(main) = self.inputs.channels.first_mut() {
                for ch in main {
                    ch[..n].copy_from_slice(chunk);
                }
            }
            let mut data = ProcessData {
                processMode: ProcessModes_::kRealtime as int32,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
                numSamples: n as int32,
                numInputs: self.inputs.buffers.len() as int32,
                numOutputs: self.outputs.buffers.len() as int32,
                inputs: self.inputs.buffers.as_mut_ptr(),
                outputs: self.outputs.buffers.as_mut_ptr(),
                inputParameterChanges: self.changes.as_ptr(),
                outputParameterChanges: ptr::null_mut(),
                inputEvents: ptr::null_mut(),
                outputEvents: ptr::null_mut(),
                processContext: ptr::null_mut(),
            };
            // SAFETY: every buffer holds MAX_BLOCK samples per channel.
            unsafe { instance.processor.process(&mut data) };
            // SAFETY: points into the `Changes` the stage keeps alive.
            unsafe { (*self.pending).set(false) };
            let Some(main) = self.outputs.channels.first() else { continue };
            if main.is_empty() {
                continue;
            }
            let scale = 1.0 / main.len() as f32;
            for (i, s) in chunk.iter_mut().enumerate() {
                *s = main.iter().map(|ch| ch[i]).sum::<f32>() * scale;
            }
        }
    }
}

impl Drop for Vst3Stage {
    fn drop(&mut self) {
        if let Some(instance) = &self.instance {
            unsafe {
                instance.processor.setProcessing(0);
                instance.component.setActive(0);
            }
        }
        self.instance = None;
    }
}

/// The channel count of each audio bus in `dir`, after arrangement.
fn bus_channels(instance: &Instance, dir: BusDirections) -> Vec<usize> {
    (0..audio_buses(&instance.component, dir))
        .map(|i| {
            let mut arr: SpeakerArrangement = 0;
            if unsafe { instance.processor.getBusArrangement(dir as BusDirection, i, &mut arr) } == kResultOk {
                arr.count_ones() as usize
            } else {
                let mut info: BusInfo = unsafe { std::mem::zeroed() };
                unsafe { instance.component.getBusInfo(MediaTypes_::kAudio as MediaType, dir as BusDirection, i, &mut info) };
                info.channelCount.max(0) as usize
            }
        })
        .collect()
}

pub(super) fn load(config: &PluginConfig, sample_rate: u32) -> Result<Box<dyn Stage>, PluginError> {
    let module = Module::open(&config.path)?;
    let cid = cid_from_text(&config.id).ok_or_else(|| PluginError::NotFound(config.path.display().to_string()))?;
    let name = module.classes().into_iter().find(|(c, _)| *c == cid).map(|(_, n)| n).ok_or_else(|| PluginError::NotFound(config.path.display().to_string()))?;
    let instance = Instance::new(&module, &cid).ok_or_else(|| PluginError::Instantiate(name.clone()))?;
    let ins = audio_buses(&instance.component, BusDirections_::kInput);
    let outs = audio_buses(&instance.component, BusDirections_::kOutput);
    if ins == 0 {
        return Err(PluginError::NoAudioInput(name));
    }
    unsafe {
        if instance.processor.canProcessSampleSize(SymbolicSampleSizes_::kSample32 as int32) != kResultOk {
            return Err(PluginError::Unsupported { name, feature: "32-bit float audio".into() });
        }
        // Mono in and out on the main buses if it will; otherwise whatever
        // it already has (stereo, usually), which the stage averages.
        let mut in_arr = vec![SpeakerArr::kMono; ins as usize];
        let mut out_arr = vec![SpeakerArr::kMono; outs as usize];
        if instance.processor.setBusArrangements(in_arr.as_mut_ptr(), ins, out_arr.as_mut_ptr(), outs) != kResultTrue {
            // Kept as the plugin has them.
        }
        for i in 0..ins {
            instance.component.activateBus(MediaTypes_::kAudio as MediaType, BusDirections_::kInput as BusDirection, i, (i == 0) as TBool);
        }
        for i in 0..outs {
            instance.component.activateBus(MediaTypes_::kAudio as MediaType, BusDirections_::kOutput as BusDirection, i, (i == 0) as TBool);
        }
        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime as int32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            maxSamplesPerBlock: MAX_BLOCK as int32,
            sampleRate: f64::from(sample_rate),
        };
        if instance.processor.setupProcessing(&mut setup) != kResultOk {
            return Err(PluginError::Instantiate(name));
        }
    }
    // Saved values, told to the controller and queued for the processor.
    let params = instance.params();
    let controller = instance.controller();
    let queues: Vec<ComPtr<IParamValueQueue>> = params
        .iter()
        .filter_map(|p| {
            let id: ParamID = p.id.parse().ok()?;
            let value = f64::from(config.param(&p.id).unwrap_or(p.default).clamp(0.0, 1.0));
            if let Some(c) = &controller {
                unsafe { c.setParamNormalized(id, value) };
            }
            ComWrapper::new(ValueQueue { id, value }).to_com_ptr::<IParamValueQueue>()
        })
        .collect();
    let changes = ComWrapper::new(Changes { queues, pending: Cell::new(true) });
    let pending: *const Cell<bool> = &changes.pending;
    let changes = changes.to_com_ptr::<IParameterChanges>().ok_or_else(|| PluginError::Instantiate(name.clone()))?;
    let inputs = Buses::new(&bus_channels(&instance, BusDirections_::kInput));
    let outputs = Buses::new(&bus_channels(&instance, BusDirections_::kOutput));
    unsafe {
        instance.component.setActive(1);
        instance.processor.setProcessing(1);
    }
    Ok(Box::new(Vst3Stage { inputs, outputs, changes, pending, instance: Some(instance), _module: module }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_ids_round_trip_as_text() {
        let cid: TUID = vst3::uid(0x6E332252, 0x54224A00, 0xAA69301A, 0xF318797D);
        let text = cid_text(&cid);
        assert_eq!(text, "6E33225254224A00AA69301AF318797D");
        assert_eq!(cid_from_text(&text), Some(cid));
        assert_eq!(cid_from_text("nope"), None);
    }

    #[test]
    fn strings_stop_at_their_terminator() {
        let mut field = [0 as c_char; 8];
        for (d, s) in field.iter_mut().zip(b"Gain") {
            *d = *s as c_char;
        }
        assert_eq!(c_text(&field), "Gain");
        let wide: Vec<TChar> = "Mix".encode_utf16().chain([0, 65]).collect();
        assert_eq!(w_text(&wide), "Mix");
    }
}
