//! LV2 plugins: a `.lv2` bundle directory holding the library and Turtle
//! files describing it. `manifest.ttl` names each plugin's URI, binary and
//! further description files (`rdfs:seeAlso`); those give the ports.
//! Parameters are the control input ports, identified by their symbol.
//!
//! **Host features.** A plugin lists what it needs from its host
//! (`lv2:requiredFeature`). This host provides URID map and unmap, the
//! options interface (sample rate and block lengths), bounded block length
//! and is-live; a plugin requiring anything else is not offered, and says
//! which feature if loaded from an old preset. Atom (event) inputs get an
//! empty sequence each block, CV ports silence, and optional ports
//! nothing.

use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::ttl::{Graph, Term, RDFS};
use super::{dirs_from_env, home, MonoAdapter, ParamInfo, PluginConfig, PluginError, PluginFormat, PluginInfo};
use crate::stages::Stage;

const LV2: &str = "http://lv2plug.in/ns/lv2core#";
const DOAP: &str = "http://usefulinc.com/ns/doap#";
const FOAF: &str = "http://xmlns.com/foaf/0.1/";
const ATOM: &str = "http://lv2plug.in/ns/ext/atom#";
const URID_MAP: &str = "http://lv2plug.in/ns/ext/urid#map";
const URID_UNMAP: &str = "http://lv2plug.in/ns/ext/urid#unmap";
const OPTIONS: &str = "http://lv2plug.in/ns/ext/options#options";
const BOUNDED_BLOCK: &str = "http://lv2plug.in/ns/ext/buf-size#boundedBlockLength";
const MAX_BLOCK_KEY: &str = "http://lv2plug.in/ns/ext/buf-size#maxBlockLength";
const MIN_BLOCK_KEY: &str = "http://lv2plug.in/ns/ext/buf-size#minBlockLength";
const NOMINAL_BLOCK_KEY: &str = "http://lv2plug.in/ns/ext/buf-size#nominalBlockLength";
const SAMPLE_RATE_KEY: &str = "http://lv2plug.in/ns/ext/parameters#sampleRate";
const IS_LIVE: &str = "http://lv2plug.in/ns/lv2core#isLive";
const IN_PLACE_BROKEN: &str = "http://lv2plug.in/ns/lv2core#inPlaceBroken";

/// Features a plugin may require of this host. `inPlaceBroken` is listed
/// because the host never runs in place anyway.
const PROVIDED: &[&str] = &[URID_MAP, URID_UNMAP, OPTIONS, BOUNDED_BLOCK, IS_LIVE, IN_PLACE_BROKEN];

/// The most samples one `run` gets; longer blocks are split.
const MAX_BLOCK: usize = 4096;

/// Bytes kept for an atom output port to write into.
const ATOM_CAPACITY: usize = 8192;

type Handle = *mut c_void;

/// `LV2_Feature`.
#[repr(C)]
struct Feature {
    uri: *const c_char,
    data: *mut c_void,
}

/// `LV2_Descriptor`.
#[repr(C)]
struct Descriptor {
    uri: *const c_char,
    instantiate: Option<unsafe extern "C" fn(*const Descriptor, f64, *const c_char, *const *const Feature) -> Handle>,
    connect_port: Option<unsafe extern "C" fn(Handle, u32, *mut c_void)>,
    activate: Option<unsafe extern "C" fn(Handle)>,
    run: Option<unsafe extern "C" fn(Handle, u32)>,
    deactivate: Option<unsafe extern "C" fn(Handle)>,
    cleanup: Option<unsafe extern "C" fn(Handle)>,
    extension_data: Option<unsafe extern "C" fn(*const c_char) -> *const c_void>,
}

type DescriptorFn = unsafe extern "C" fn(u32) -> *const Descriptor;

/// `LV2_URID_Map`.
#[repr(C)]
struct UridMapFeature {
    handle: *mut c_void,
    map: unsafe extern "C" fn(*mut c_void, *const c_char) -> u32,
}

/// `LV2_URID_Unmap`.
#[repr(C)]
struct UridUnmapFeature {
    handle: *mut c_void,
    unmap: unsafe extern "C" fn(*mut c_void, u32) -> *const c_char,
}

/// `LV2_Options_Option`.
#[repr(C)]
struct OptionEntry {
    context: u32,
    subject: u32,
    key: u32,
    size: u32,
    kind: u32,
    value: *const c_void,
}

/// URIs to numbers and back, for the life of one plugin instance. Ids
/// start at 1 (0 means "none" in LV2).
#[derive(Default)]
struct Urids {
    inner: Mutex<(HashMap<String, u32>, Vec<CString>)>,
}

impl Urids {
    fn map(&self, uri: &str) -> u32 {
        let mut guard = self.inner.lock().unwrap();
        let (ids, names) = &mut *guard;
        if let Some(&id) = ids.get(uri) {
            return id;
        }
        names.push(CString::new(uri).unwrap_or_default());
        let id = names.len() as u32;
        ids.insert(uri.to_string(), id);
        id
    }
}

unsafe extern "C" fn urid_map(handle: *mut c_void, uri: *const c_char) -> u32 {
    if uri.is_null() {
        return 0;
    }
    // SAFETY: `handle` is the instance's `Urids`, alive as long as it is.
    let urids = unsafe { &*(handle as *const Urids) };
    urids.map(&unsafe { CStr::from_ptr(uri) }.to_string_lossy())
}

unsafe extern "C" fn urid_unmap(handle: *mut c_void, id: u32) -> *const c_char {
    // SAFETY: as above; the strings are never removed, so the pointer
    // stays valid for the instance's life.
    let urids = unsafe { &*(handle as *const Urids) };
    let guard = urids.inner.lock().unwrap();
    guard.1.get((id as usize).wrapping_sub(1)).map_or(std::ptr::null(), |s| s.as_ptr())
}

/// Where LV2 bundles live: `$LV2_PATH`, else the usual folders.
pub(super) fn search_path() -> Vec<PathBuf> {
    let mut defaults = Vec::new();
    if let Some(home) = home() {
        defaults.push(home.join(".lv2"));
        if cfg!(target_os = "macos") {
            defaults.push(home.join("Library/Audio/Plug-Ins/LV2"));
        }
    }
    if cfg!(target_os = "windows") {
        for var in ["APPDATA", "COMMONPROGRAMFILES"] {
            if let Some(dir) = std::env::var_os(var) {
                defaults.push(PathBuf::from(dir).join("LV2"));
            }
        }
    } else if cfg!(target_os = "macos") {
        defaults.push("/Library/Audio/Plug-Ins/LV2".into());
    } else {
        for dir in ["/usr/lib/lv2", "/usr/local/lib/lv2", "/usr/lib64/lv2", "/usr/lib/x86_64-linux-gnu/lv2", "/usr/lib/aarch64-linux-gnu/lv2"] {
            defaults.push(dir.into());
        }
    }
    dirs_from_env("LV2_PATH", defaults)
}

/// A `file://` URI for `path`, the form Turtle files resolve against.
fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

/// The path a `file://` URI names.
fn uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = if cfg!(windows) { rest.trim_start_matches('/') } else { rest };
    Some(PathBuf::from(rest.replace("%20", " ")))
}

/// A bundle's manifest plus every file it points to, as one graph.
fn read_bundle(bundle: &Path) -> Option<Graph> {
    let manifest = bundle.join("manifest.ttl");
    let mut graph = Graph::default();
    graph.parse(&std::fs::read_to_string(&manifest).ok()?, &file_uri(&manifest)).ok()?;
    let see_also: HashSet<String> =
        graph.triples.iter().filter(|(_, p, _)| p == &format!("{RDFS}seeAlso")).filter_map(|(_, _, o)| o.iri().map(str::to_string)).collect();
    for uri in see_also {
        let Some(path) = uri_path(&uri) else { continue };
        if let Ok(text) = std::fs::read_to_string(&path) {
            // A broken extra file leaves out what it would have added.
            let _ = graph.parse(&text, &uri);
        }
    }
    Some(graph)
}

/// What a port is, from its types.
#[derive(Clone, Copy, PartialEq, Debug)]
enum PortKind {
    AudioIn,
    AudioOut,
    ControlIn,
    ControlOut,
    AtomIn,
    AtomOut,
    Cv,
    /// Something this host doesn't know; fine if optional.
    Other,
}

struct Port {
    index: u32,
    kind: PortKind,
    optional: bool,
    param: ParamInfo,
}

fn ports(graph: &Graph, plugin: &Term) -> Vec<Port> {
    let lv2 = |s: &str| format!("{LV2}{s}");
    let mut out: Vec<Port> = graph
        .objects(plugin, &lv2("port"))
        .filter_map(|port| {
            let index = graph.object(port, &lv2("index"))?.number()? as u32;
            let is = |class: &str| graph.is_a(port, class);
            let input = is(&lv2("InputPort"));
            let kind = if is(&lv2("AudioPort")) {
                if input { PortKind::AudioIn } else { PortKind::AudioOut }
            } else if is(&lv2("ControlPort")) {
                if input { PortKind::ControlIn } else { PortKind::ControlOut }
            } else if is(&format!("{ATOM}AtomPort")) {
                if input { PortKind::AtomIn } else { PortKind::AtomOut }
            } else if is(&lv2("CVPort")) {
                PortKind::Cv
            } else {
                PortKind::Other
            };
            let property = lv2("portProperty");
            let props: Vec<&str> = graph.objects(port, &property).filter_map(Term::iri).collect();
            let number = |p: &str| graph.object(port, &lv2(p)).and_then(Term::number);
            let symbol = graph.object(port, &lv2("symbol")).and_then(Term::text).unwrap_or_default().to_string();
            let name = graph.object(port, &lv2("name")).and_then(Term::text).map_or_else(|| symbol.clone(), str::to_string);
            let toggle = props.contains(&lv2("toggled").as_str());
            let (min, max) = if toggle { (0.0, 1.0) } else { (number("minimum").unwrap_or(0.0), number("maximum").unwrap_or(1.0)) };
            let default = number("default").unwrap_or(min).clamp(min.min(max), max.max(min));
            Some(Port {
                index,
                kind,
                optional: props.contains(&lv2("connectionOptional").as_str()),
                param: ParamInfo { id: symbol, name, min, max, default, toggle },
            })
        })
        .collect();
    out.sort_by_key(|p| p.index);
    out
}

/// The features `plugin` requires that this host doesn't provide.
fn missing_features(graph: &Graph, plugin: &Term) -> Vec<String> {
    graph
        .objects(plugin, &format!("{LV2}requiredFeature"))
        .filter_map(Term::iri)
        .filter(|f| !PROVIDED.contains(f))
        .map(str::to_string)
        .collect()
}

fn info(bundle: &Path, graph: &Graph, plugin: &Term) -> PluginInfo {
    let name = graph.object(plugin, &format!("{DOAP}name")).and_then(Term::text).unwrap_or_default().to_string();
    let maker = ["maintainer", "developer"]
        .iter()
        .filter_map(|role| graph.object(plugin, &format!("{DOAP}{role}")))
        .filter_map(|who| graph.object(who, &format!("{FOAF}name")).and_then(Term::text))
        .next()
        .unwrap_or_default()
        .to_string();
    PluginInfo {
        format: PluginFormat::Lv2,
        path: bundle.to_path_buf(),
        id: plugin.iri().unwrap_or_default().to_string(),
        name,
        maker,
        params: ports(graph, plugin).into_iter().filter(|p| p.kind == PortKind::ControlIn).map(|p| p.param).collect(),
    }
}

/// The plugins in one bundle this host can run on a microphone.
pub(super) fn scan_bundle(bundle: &Path) -> Vec<PluginInfo> {
    let Some(graph) = read_bundle(bundle) else { return Vec::new() };
    let plugin_class = Term::Iri(format!("{LV2}Plugin"));
    let mut plugins: Vec<Term> = Vec::new();
    // A plugin is usually typed in the manifest and again in its own file.
    for p in graph.subjects(super::ttl::RDF_TYPE, &plugin_class) {
        if !plugins.contains(p) {
            plugins.push(p.clone());
        }
    }
    plugins
        .iter()
        .filter(|p| missing_features(&graph, p).is_empty())
        .filter(|p| {
            let ports = ports(&graph, p);
            ports.iter().any(|port| port.kind == PortKind::AudioIn) && !ports.iter().any(|port| port.kind == PortKind::Other && !port.optional)
        })
        .map(|p| info(bundle, &graph, p))
        .collect()
}

/// Every bundle in `dir`.
pub(super) fn scan_dir(dir: &Path) -> Vec<PluginInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut bundles: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.join("manifest.ttl").is_file()).collect();
    bundles.sort();
    bundles.iter().flat_map(|b| scan_bundle(b)).collect()
}

/// The atom header LV2 event buffers start with.
#[repr(C)]
struct AtomSequence {
    size: u32,
    kind: u32,
    unit: u32,
    pad: u32,
}

/// A running LV2 plugin.
struct Lv2Stage {
    handle: Handle,
    descriptor: *const Descriptor,
    audio_in: Vec<u32>,
    audio_out: Vec<u32>,
    adapter: MonoAdapter,
    /// Per atom input port: an empty sequence, reset each block.
    atom_in: Vec<(u32, Box<AtomSequence>)>,
    /// Per atom output port: room to write, its size reset each block.
    atom_out: Vec<(u32, Box<[u64]>)>,
    sequence_type: u32,
    chunk_type: u32,
    /// What the plugin reads its controls and CV from; never moved.
    _controls: Box<[f32]>,
    _cv: Box<[f32]>,
    /// Kept alive for the plugin, which may hold pointers into them.
    _urids: Box<Urids>,
    _features: Box<FeatureSet>,
    _lib: libloading::Library,
}

// SAFETY: an LV2 instance's audio-class calls are made from one thread at
// a time, which the chain guarantees.
unsafe impl Send for Lv2Stage {}

/// The features handed to `instantiate`, with everything they point to.
struct FeatureSet {
    _uris: Vec<CString>,
    _map: Box<UridMapFeature>,
    _unmap: Box<UridUnmapFeature>,
    _options: Box<[OptionEntry]>,
    _values: Box<(i32, i32, i32, f32)>,
    _features: Vec<Feature>,
    pointers: Vec<*const Feature>,
}

impl FeatureSet {
    fn new(urids: &Urids, sample_rate: f32) -> Box<Self> {
        let handle = urids as *const Urids as *mut c_void;
        let map = Box::new(UridMapFeature { handle, map: urid_map });
        let unmap = Box::new(UridUnmapFeature { handle, unmap: urid_unmap });
        let values = Box::new((MAX_BLOCK as i32, 1i32, 512i32, sample_rate));
        let int = urids.map(&format!("{ATOM}Int"));
        let float = urids.map(&format!("{ATOM}Float"));
        let option = |key: &str, kind: u32, value: *const c_void, size: u32| OptionEntry { context: 0, subject: 0, key: urids.map(key), size, kind, value };
        let options: Box<[OptionEntry]> = vec![
            option(MAX_BLOCK_KEY, int, &values.0 as *const i32 as *const c_void, 4),
            option(MIN_BLOCK_KEY, int, &values.1 as *const i32 as *const c_void, 4),
            option(NOMINAL_BLOCK_KEY, int, &values.2 as *const i32 as *const c_void, 4),
            option(SAMPLE_RATE_KEY, float, &values.3 as *const f32 as *const c_void, 4),
            OptionEntry { context: 0, subject: 0, key: 0, size: 0, kind: 0, value: std::ptr::null() },
        ]
        .into_boxed_slice();
        let uris: Vec<CString> = [URID_MAP, URID_UNMAP, OPTIONS, BOUNDED_BLOCK, IS_LIVE].iter().map(|u| CString::new(*u).expect("no NUL")).collect();
        let data: [*mut c_void; 5] = [
            &*map as *const UridMapFeature as *mut c_void,
            &*unmap as *const UridUnmapFeature as *mut c_void,
            options.as_ptr() as *mut c_void,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ];
        let features: Vec<Feature> = uris.iter().zip(data).map(|(u, d)| Feature { uri: u.as_ptr(), data: d }).collect();
        let mut pointers: Vec<*const Feature> = features.iter().map(|f| f as *const Feature).collect();
        pointers.push(std::ptr::null());
        Box::new(Self { _uris: uris, _map: map, _unmap: unmap, _options: options, _values: values, _features: features, pointers })
    }
}

impl Lv2Stage {
    fn descriptor(&self) -> &Descriptor {
        // SAFETY: valid while `_lib` is loaded.
        unsafe { &*self.descriptor }
    }

    /// Connects the audio buffers at `offset` into the adapter's.
    fn connect_audio(&mut self, offset: usize) {
        let connect = self.descriptor().connect_port.expect("checked at load");
        for (k, &port) in self.audio_in.iter().enumerate() {
            unsafe { connect(self.handle, port, self.adapter.inputs[k][offset..].as_mut_ptr() as *mut c_void) };
        }
        for (k, &port) in self.audio_out.iter().enumerate() {
            unsafe { connect(self.handle, port, self.adapter.outputs[k][offset..].as_mut_ptr() as *mut c_void) };
        }
    }

    /// Empties the event buffers for the next `run`.
    fn reset_atoms(&mut self) {
        for (_, seq) in &mut self.atom_in {
            **seq = AtomSequence { size: 8, kind: self.sequence_type, unit: 0, pad: 0 };
        }
        for (_, buf) in &mut self.atom_out {
            // An output buffer says how much room it has, as a Chunk.
            let header = buf.as_mut_ptr() as *mut u32;
            unsafe {
                *header = (ATOM_CAPACITY - 8) as u32;
                *header.add(1) = self.chunk_type;
            }
        }
    }
}

impl Stage for Lv2Stage {
    fn name(&self) -> &'static str {
        "plugin"
    }

    fn process(&mut self, samples: &mut [f32]) {
        if samples.is_empty() {
            return;
        }
        self.adapter.prepare(samples);
        let run = self.descriptor().run.expect("checked at load");
        let mut offset = 0;
        while offset < samples.len() {
            let n = (samples.len() - offset).min(MAX_BLOCK);
            // Connected every block: cheap, and the buffers may have moved.
            self.connect_audio(offset);
            self.reset_atoms();
            // SAFETY: every required port is connected to a buffer of at
            // least `n` samples.
            unsafe { run(self.handle, n as u32) };
            offset += n;
        }
        self.adapter.finish(samples);
    }
}

impl Drop for Lv2Stage {
    fn drop(&mut self) {
        let d = self.descriptor();
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
    let bundle = &config.path;
    let graph = read_bundle(bundle).ok_or_else(|| PluginError::NotFound(bundle.display().to_string()))?;
    let plugin = Term::Iri(config.id.clone());
    if !graph.is_a(&plugin, &format!("{LV2}Plugin")) {
        return Err(PluginError::NotFound(bundle.display().to_string()));
    }
    let name = graph.object(&plugin, &format!("{DOAP}name")).and_then(Term::text).unwrap_or(&config.name).to_string();
    if let Some(feature) = missing_features(&graph, &plugin).into_iter().next() {
        return Err(PluginError::Unsupported { name, feature });
    }
    let binary = graph
        .object(&plugin, &format!("{LV2}binary"))
        .and_then(Term::iri)
        .and_then(uri_path)
        .ok_or_else(|| PluginError::NotFound(bundle.display().to_string()))?;
    // SAFETY: loading the plugin the user chose runs its initialisers.
    let lib = unsafe { libloading::Library::new(&binary) }
        .map_err(|e| PluginError::Open { path: binary.clone(), reason: e.to_string() })?;
    let descriptor = {
        let f = unsafe { lib.get::<DescriptorFn>(b"lv2_descriptor\0") }
            .map_err(|e| PluginError::Open { path: binary.clone(), reason: e.to_string() })?;
        let mut found = std::ptr::null();
        for i in 0..4096 {
            let d = unsafe { f(i) };
            if d.is_null() {
                break;
            }
            if unsafe { CStr::from_ptr((*d).uri) }.to_string_lossy() == config.id {
                found = d;
                break;
            }
        }
        if found.is_null() {
            return Err(PluginError::NotFound(binary.display().to_string()));
        }
        found
    };
    let d = unsafe { &*descriptor };
    let (Some(instantiate), Some(connect), Some(_run)) = (d.instantiate, d.connect_port, d.run) else {
        return Err(PluginError::Instantiate(name));
    };
    let ports = ports(&graph, &plugin);
    if !ports.iter().any(|p| p.kind == PortKind::AudioIn) {
        return Err(PluginError::NoAudioInput(name));
    }
    if let Some(p) = ports.iter().find(|p| p.kind == PortKind::Other && !p.optional) {
        return Err(PluginError::Unsupported { name, feature: format!("port '{}'", p.param.id) });
    }

    let urids = Box::<Urids>::default();
    let features = FeatureSet::new(&urids, sample_rate as f32);
    let mut bundle_path = bundle.to_string_lossy().into_owned();
    if !bundle_path.ends_with(std::path::MAIN_SEPARATOR) {
        bundle_path.push(std::path::MAIN_SEPARATOR);
    }
    let bundle_c = CString::new(bundle_path).map_err(|_| PluginError::Instantiate(name.clone()))?;
    // SAFETY: valid descriptor, bundle path and NULL-terminated features
    // that outlive the instance (owned by the stage).
    let handle = unsafe { instantiate(descriptor, f64::from(sample_rate), bundle_c.as_ptr(), features.pointers.as_ptr()) };
    if handle.is_null() {
        return Err(PluginError::Instantiate(name));
    }

    let size = ports.iter().map(|p| p.index as usize + 1).max().unwrap_or(0);
    let mut controls = vec![0.0f32; size].into_boxed_slice();
    let mut cv = vec![0.0f32; MAX_BLOCK].into_boxed_slice();
    let mut atom_in = Vec::new();
    let mut atom_out = Vec::new();
    for p in &ports {
        match p.kind {
            PortKind::ControlIn => {
                controls[p.index as usize] =
                    config.param(&p.param.id).unwrap_or(p.param.default).clamp(p.param.min.min(p.param.max), p.param.max.max(p.param.min));
                unsafe { connect(handle, p.index, &mut controls[p.index as usize] as *mut f32 as *mut c_void) };
            }
            PortKind::ControlOut => unsafe { connect(handle, p.index, &mut controls[p.index as usize] as *mut f32 as *mut c_void) },
            PortKind::Cv => unsafe { connect(handle, p.index, cv.as_mut_ptr() as *mut c_void) },
            PortKind::AtomIn => atom_in.push((p.index, Box::new(AtomSequence { size: 8, kind: 0, unit: 0, pad: 0 }))),
            PortKind::AtomOut => atom_out.push((p.index, vec![0u64; ATOM_CAPACITY / 8].into_boxed_slice())),
            PortKind::AudioIn | PortKind::AudioOut | PortKind::Other => {}
        }
    }
    for (index, seq) in &mut atom_in {
        unsafe { connect(handle, *index, &mut **seq as *mut AtomSequence as *mut c_void) };
    }
    for (index, buf) in &mut atom_out {
        unsafe { connect(handle, *index, buf.as_mut_ptr() as *mut c_void) };
    }
    let audio_in: Vec<u32> = ports.iter().filter(|p| p.kind == PortKind::AudioIn).map(|p| p.index).collect();
    let audio_out: Vec<u32> = ports.iter().filter(|p| p.kind == PortKind::AudioOut).map(|p| p.index).collect();
    let mut stage = Lv2Stage {
        handle,
        descriptor,
        adapter: MonoAdapter::new(audio_in.len(), audio_out.len()),
        audio_in,
        audio_out,
        atom_in,
        atom_out,
        sequence_type: urids.map(&format!("{ATOM}Sequence")),
        chunk_type: urids.map(&format!("{ATOM}Chunk")),
        _controls: controls,
        _cv: cv,
        _urids: urids,
        _features: features,
        _lib: lib,
    };
    stage.adapter.prepare(&[0.0; 64]);
    stage.connect_audio(0);
    stage.reset_atoms();
    if let Some(activate) = d.activate {
        unsafe { activate(handle) };
    }
    Ok(Box::new(stage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urids_are_stable_and_unmap_back() {
        let urids = Urids::default();
        let a = urids.map("urn:a");
        assert_eq!(a, 1);
        assert_eq!(urids.map("urn:b"), 2);
        assert_eq!(urids.map("urn:a"), a);
        let handle = &urids as *const Urids as *mut c_void;
        let back = unsafe { CStr::from_ptr(urid_unmap(handle, 2)) };
        assert_eq!(back.to_str().unwrap(), "urn:b");
        assert!(unsafe { urid_unmap(handle, 99) }.is_null());
    }

    #[test]
    fn a_plugin_needing_an_unknown_feature_is_not_offered() {
        let dir = std::env::temp_dir().join(format!("antiphon-lv2-test-{}", std::process::id()));
        let bundle = dir.join("needy.lv2");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(
            bundle.join("manifest.ttl"),
            "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
             <urn:needy> a lv2:Plugin ; lv2:binary <needy.so> ; rdfs:seeAlso <needy.ttl> .\n\
             <urn:fine> a lv2:Plugin ; lv2:binary <needy.so> ; rdfs:seeAlso <needy.ttl> .",
        )
        .unwrap();
        std::fs::write(
            bundle.join("needy.ttl"),
            "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n@prefix doap: <http://usefulinc.com/ns/doap#> .\n\
             <urn:needy> doap:name \"Needy\" ; lv2:requiredFeature <urn:something:else> ;\n\
               lv2:port [ a lv2:AudioPort , lv2:InputPort ; lv2:index 0 ; lv2:symbol \"in\" ] .\n\
             <urn:fine> doap:name \"Fine\" ; lv2:requiredFeature <http://lv2plug.in/ns/ext/urid#map> ;\n\
               lv2:port [ a lv2:AudioPort , lv2:InputPort ; lv2:index 0 ; lv2:symbol \"in\" ] ,\n\
                        [ a lv2:ControlPort , lv2:InputPort ; lv2:index 1 ; lv2:symbol \"amount\" ; lv2:name \"Amount\" ;\n\
                          lv2:default 0.5 ; lv2:minimum 0 ; lv2:maximum 2 ] .",
        )
        .unwrap();
        let found = scan_dir(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].name, "Fine");
        assert_eq!(found[0].params[0].id, "amount");
        assert_eq!((found[0].params[0].min, found[0].params[0].max, found[0].params[0].default), (0.0, 2.0, 0.5));
    }
}
