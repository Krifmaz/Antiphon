//! One gain plugin in all three formats antiphon-audio hosts -- LADSPA,
//! LV2 and VST3 -- built as a shared library so the host's tests can load
//! it the way they would load a real one. Not shipped.
//!
//! Each multiplies its input by a "gain" parameter: LADSPA and LV2 take it
//! as a plain value (0 to 4, default 1), VST3 as a normalised one where
//! 0.5 is unity (gain = 2 x value). The LV2 version also has an atom input
//! port, so the host's event-buffer handling is exercised, and its
//! descriptions are written by [`write_lv2_bundle`].

#![allow(non_snake_case, non_upper_case_globals)]

use std::ffi::{c_char, c_int, c_ulong, c_void};
use std::path::Path;

/// The label (LADSPA), URI (LV2) the plugin is found by.
pub const LADSPA_LABEL: &str = "antiphon_test_gain";
pub const LV2_URI: &str = "urn:antiphon:test:gain";

// ---- LADSPA ---------------------------------------------------------------

#[repr(C)]
pub struct LadspaHint {
    hint: c_int,
    lower: f32,
    upper: f32,
}

#[repr(C)]
pub struct LadspaDescriptor {
    unique_id: c_ulong,
    label: *const c_char,
    properties: c_int,
    name: *const c_char,
    maker: *const c_char,
    copyright: *const c_char,
    port_count: c_ulong,
    port_descriptors: *const c_int,
    port_names: *const *const c_char,
    port_range_hints: *const LadspaHint,
    implementation_data: *mut c_void,
    instantiate: unsafe extern "C" fn(*const LadspaDescriptor, c_ulong) -> *mut c_void,
    connect_port: unsafe extern "C" fn(*mut c_void, c_ulong, *mut f32),
    activate: Option<unsafe extern "C" fn(*mut c_void)>,
    run: unsafe extern "C" fn(*mut c_void, c_ulong),
    run_adding: Option<unsafe extern "C" fn(*mut c_void, c_ulong)>,
    set_run_adding_gain: Option<unsafe extern "C" fn(*mut c_void, f32)>,
    deactivate: Option<unsafe extern "C" fn(*mut c_void)>,
    cleanup: unsafe extern "C" fn(*mut c_void),
}

struct Sync<T>(T);
// SAFETY: the statics below are immutable data that C hosts read.
unsafe impl<T> std::marker::Sync for Sync<T> {}

/// Ports: 0 gain (control in), 1 in (audio in), 2 out (audio out).
#[derive(Default)]
struct Gain {
    ports: [*mut f32; 4],
}

unsafe extern "C" fn gain_new(_d: *const LadspaDescriptor, _rate: c_ulong) -> *mut c_void {
    Box::into_raw(Box::<Gain>::default()) as *mut c_void
}

unsafe extern "C" fn gain_connect(h: *mut c_void, port: c_ulong, data: *mut f32) {
    let g = unsafe { &mut *(h as *mut Gain) };
    if let Some(slot) = g.ports.get_mut(port as usize) {
        *slot = data;
    }
}

unsafe extern "C" fn gain_run(h: *mut c_void, n: c_ulong) {
    let g = unsafe { &*(h as *const Gain) };
    let gain = unsafe { *g.ports[0] };
    for i in 0..n as usize {
        unsafe { *g.ports[2].add(i) = *g.ports[1].add(i) * gain };
    }
}

unsafe extern "C" fn gain_free(h: *mut c_void) {
    drop(unsafe { Box::from_raw(h as *mut Gain) });
}

static PORT_KINDS: [c_int; 3] = [0x1 | 0x4, 0x1 | 0x8, 0x2 | 0x8];
static PORT_NAMES: Sync<[*const c_char; 3]> =
    Sync([c"Gain".as_ptr(), c"In".as_ptr(), c"Out".as_ptr()]);
// Bounded 0..4, default 1.
static PORT_HINTS: [LadspaHint; 3] = [
    LadspaHint { hint: 0x1 | 0x2 | 0x240, lower: 0.0, upper: 4.0 },
    LadspaHint { hint: 0, lower: 0.0, upper: 0.0 },
    LadspaHint { hint: 0, lower: 0.0, upper: 0.0 },
];

static LADSPA: Sync<LadspaDescriptor> = Sync(LadspaDescriptor {
    unique_id: 4242,
    label: c"antiphon_test_gain".as_ptr(),
    properties: 0,
    name: c"Antiphon Test Gain".as_ptr(),
    maker: c"Antiphon".as_ptr(),
    copyright: c"MIT".as_ptr(),
    port_count: 3,
    port_descriptors: PORT_KINDS.as_ptr(),
    port_names: PORT_NAMES.0.as_ptr(),
    port_range_hints: PORT_HINTS.as_ptr(),
    implementation_data: std::ptr::null_mut(),
    instantiate: gain_new,
    connect_port: gain_connect,
    activate: None,
    run: gain_run,
    run_adding: None,
    set_run_adding_gain: None,
    deactivate: None,
    cleanup: gain_free,
});

#[no_mangle]
pub extern "C" fn ladspa_descriptor(index: c_ulong) -> *const LadspaDescriptor {
    if index == 0 {
        &LADSPA.0
    } else {
        std::ptr::null()
    }
}

// ---- LV2 ------------------------------------------------------------------

#[repr(C)]
pub struct Lv2Descriptor {
    uri: *const c_char,
    instantiate: unsafe extern "C" fn(*const Lv2Descriptor, f64, *const c_char, *const *const c_void) -> *mut c_void,
    connect_port: unsafe extern "C" fn(*mut c_void, u32, *mut c_void),
    activate: Option<unsafe extern "C" fn(*mut c_void)>,
    run: unsafe extern "C" fn(*mut c_void, u32),
    deactivate: Option<unsafe extern "C" fn(*mut c_void)>,
    cleanup: unsafe extern "C" fn(*mut c_void),
    extension_data: Option<unsafe extern "C" fn(*const c_char) -> *const c_void>,
}

/// `LV2_Feature`.
#[repr(C)]
struct Feature {
    uri: *const c_char,
    data: *mut c_void,
}

/// `LV2_URID_Map`.
#[repr(C)]
struct UridMap {
    handle: *mut c_void,
    map: unsafe extern "C" fn(*mut c_void, *const c_char) -> u32,
}

unsafe extern "C" fn lv2_new(_d: *const Lv2Descriptor, _rate: f64, bundle: *const c_char, features: *const *const c_void) -> *mut c_void {
    // Like real plugins, insist on a URID map and use it, and check the
    // bundle path ends with a separator as the spec says it must.
    let bundle = unsafe { std::ffi::CStr::from_ptr(bundle) }.to_string_lossy();
    if !bundle.ends_with('/') && !bundle.ends_with('\\') {
        return std::ptr::null_mut();
    }
    let mut mapped = false;
    let mut f = features as *const *const Feature;
    while !f.is_null() && unsafe { !(*f).is_null() } {
        let feature = unsafe { &**f };
        if unsafe { std::ffi::CStr::from_ptr(feature.uri) }.to_bytes() == b"http://lv2plug.in/ns/ext/urid#map" {
            let map = unsafe { &*(feature.data as *const UridMap) };
            mapped = unsafe { (map.map)(map.handle, c"http://lv2plug.in/ns/ext/atom#Sequence".as_ptr()) } != 0;
        }
        f = unsafe { f.add(1) };
    }
    if !mapped {
        return std::ptr::null_mut();
    }
    Box::into_raw(Box::<Gain>::default()) as *mut c_void
}

unsafe extern "C" fn lv2_connect(h: *mut c_void, port: u32, data: *mut c_void) {
    unsafe { gain_connect(h, c_ulong::from(port), data as *mut f32) }
}

unsafe extern "C" fn lv2_run(h: *mut c_void, n: u32) {
    let g = unsafe { &*(h as *const Gain) };
    // The atom input must be connected to a valid (empty) sequence.
    let atom = g.ports[3] as *const u32;
    if atom.is_null() || unsafe { *atom } < 8 {
        return;
    }
    unsafe { gain_run(h, c_ulong::from(n)) }
}

static LV2: Sync<Lv2Descriptor> = Sync(Lv2Descriptor {
    uri: c"urn:antiphon:test:gain".as_ptr(),
    instantiate: lv2_new,
    connect_port: lv2_connect,
    activate: None,
    run: lv2_run,
    deactivate: None,
    cleanup: gain_free,
    extension_data: None,
});

#[no_mangle]
pub extern "C" fn lv2_descriptor(index: u32) -> *const Lv2Descriptor {
    if index == 0 {
        &LV2.0
    } else {
        std::ptr::null()
    }
}

/// Makes an LV2 bundle in `dir` around `library` (this crate's built
/// library, copied in), returning the bundle's path.
pub fn write_lv2_bundle(dir: &Path, library: &Path) -> std::io::Result<std::path::PathBuf> {
    let bundle = dir.join("antiphon-test-gain.lv2");
    std::fs::create_dir_all(&bundle)?;
    let binary = format!("gain.{}", library.extension().and_then(|e| e.to_str()).unwrap_or("so"));
    std::fs::copy(library, bundle.join(&binary))?;
    std::fs::write(
        bundle.join("manifest.ttl"),
        format!(
            "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\n\
             <{LV2_URI}> a lv2:Plugin ;\n    lv2:binary <{binary}> ;\n    rdfs:seeAlso <gain.ttl> .\n"
        ),
    )?;
    std::fs::write(
        bundle.join("gain.ttl"),
        format!(
            "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n@prefix doap: <http://usefulinc.com/ns/doap#> .\n\
             @prefix atom: <http://lv2plug.in/ns/ext/atom#> .\n@prefix urid: <http://lv2plug.in/ns/ext/urid#> .\n\n\
             <{LV2_URI}> a lv2:Plugin , lv2:AmplifierPlugin ;\n    doap:name \"Antiphon Test Gain\" ;\n\
             lv2:requiredFeature urid:map ;\n    lv2:port [\n        a lv2:InputPort , lv2:ControlPort ;\n\
             lv2:index 0 ; lv2:symbol \"gain\" ; lv2:name \"Gain\" ;\n        lv2:default 1.0 ; lv2:minimum 0.0 ; lv2:maximum 4.0\n\
             ] , [\n        a lv2:InputPort , lv2:AudioPort ; lv2:index 1 ; lv2:symbol \"in\" ; lv2:name \"In\"\n\
             ] , [\n        a lv2:OutputPort , lv2:AudioPort ; lv2:index 2 ; lv2:symbol \"out\" ; lv2:name \"Out\"\n\
             ] , [\n        a lv2:InputPort , atom:AtomPort ; atom:bufferType atom:Sequence ;\n\
             lv2:index 3 ; lv2:symbol \"events\" ; lv2:name \"Events\"\n    ] .\n"
        ),
    )?;
    Ok(bundle)
}

// ---- VST3 -----------------------------------------------------------------

pub mod vst {
    use std::cell::Cell;
    use std::ffi::{c_char, c_void};
    use std::path::Path;
    use std::ptr;
    use std::sync::atomic::{AtomicU64, Ordering};

    use vst3::{uid, Class, ComRef, ComWrapper, Steinberg::Vst::*, Steinberg::*};

    /// The processor's class id, as the host saves it.
    pub const CID_TEXT: &str = "A1B2C3D4E5F60718293A4B5C6D7E8F90";
    const PROCESSOR: TUID = uid(0xA1B2C3D4, 0xE5F60718, 0x293A4B5C, 0x6D7E8F90);
    const CONTROLLER: TUID = uid(0x0F1E2D3C, 0x4B5A6978, 0x8796A5B4, 0xC3D2E1F0);

    fn copy(src: &str, dst: &mut [c_char]) {
        for (d, s) in dst.iter_mut().zip(src.bytes().chain(std::iter::once(0))) {
            *d = s as c_char;
        }
    }

    fn copy_w(src: &str, dst: &mut [TChar]) {
        for (d, s) in dst.iter_mut().zip(src.encode_utf16().chain(std::iter::once(0))) {
            *d = s;
        }
    }

    struct Processor {
        gain: AtomicU64,
    }

    impl Class for Processor {
        type Interfaces = (IComponent, IAudioProcessor);
    }

    impl IPluginBaseTrait for Processor {
        unsafe fn initialize(&self, context: *mut FUnknown) -> tresult {
            // A real host passes its application object.
            if context.is_null() {
                kResultFalse
            } else {
                kResultOk
            }
        }
        unsafe fn terminate(&self) -> tresult {
            kResultOk
        }
    }

    impl IComponentTrait for Processor {
        unsafe fn getControllerClassId(&self, id: *mut TUID) -> tresult {
            unsafe { *id = CONTROLLER };
            kResultOk
        }
        unsafe fn setIoMode(&self, _: IoMode) -> tresult {
            kResultOk
        }
        unsafe fn getBusCount(&self, media: MediaType, _dir: BusDirection) -> i32 {
            (media == MediaTypes_::kAudio as MediaType) as i32
        }
        unsafe fn getBusInfo(&self, media: MediaType, dir: BusDirection, index: i32, bus: *mut BusInfo) -> tresult {
            if media != MediaTypes_::kAudio as MediaType || index != 0 {
                return kInvalidArgument;
            }
            let bus = unsafe { &mut *bus };
            bus.mediaType = media;
            bus.direction = dir;
            bus.channelCount = 2;
            copy_w("Main", &mut bus.name);
            bus.busType = BusTypes_::kMain as BusType;
            // the SDK's enums are i32 on Windows and u32 elsewhere
            bus.flags = BusInfo_::BusFlags_::kDefaultActive as _;
            kResultOk
        }
        unsafe fn getRoutingInfo(&self, _: *mut RoutingInfo, _: *mut RoutingInfo) -> tresult {
            kNotImplemented
        }
        unsafe fn activateBus(&self, _: MediaType, _: BusDirection, _: i32, _: TBool) -> tresult {
            kResultOk
        }
        unsafe fn setActive(&self, _: TBool) -> tresult {
            kResultOk
        }
        unsafe fn setState(&self, _: *mut IBStream) -> tresult {
            kResultOk
        }
        unsafe fn getState(&self, _: *mut IBStream) -> tresult {
            kResultOk
        }
    }

    impl IAudioProcessorTrait for Processor {
        unsafe fn setBusArrangements(&self, _: *mut SpeakerArrangement, _: i32, _: *mut SpeakerArrangement, _: i32) -> tresult {
            // Stereo only, like most plugins: the host must cope.
            kResultFalse
        }
        unsafe fn getBusArrangement(&self, _: BusDirection, index: i32, arr: *mut SpeakerArrangement) -> tresult {
            if index != 0 {
                return kInvalidArgument;
            }
            unsafe { *arr = SpeakerArr::kStereo };
            kResultOk
        }
        unsafe fn canProcessSampleSize(&self, size: i32) -> tresult {
            if size == SymbolicSampleSizes_::kSample32 as i32 {
                kResultOk
            } else {
                kNotImplemented
            }
        }
        unsafe fn getLatencySamples(&self) -> u32 {
            0
        }
        unsafe fn setupProcessing(&self, _: *mut ProcessSetup) -> tresult {
            kResultOk
        }
        unsafe fn setProcessing(&self, _: TBool) -> tresult {
            kResultOk
        }
        unsafe fn process(&self, data: *mut ProcessData) -> tresult {
            let data = unsafe { &*data };
            if let Some(changes) = unsafe { ComRef::from_raw(data.inputParameterChanges) } {
                for i in 0..unsafe { changes.getParameterCount() } {
                    let Some(queue) = (unsafe { ComRef::from_raw(changes.getParameterData(i)) }) else { continue };
                    let (mut offset, mut value) = (0, 0.0);
                    if unsafe { queue.getParameterId() } == 0 && unsafe { queue.getPoint(0, &mut offset, &mut value) } == kResultTrue {
                        self.gain.store(value.to_bits(), Ordering::Relaxed);
                    }
                }
            }
            let gain = 2.0 * f64::from_bits(self.gain.load(Ordering::Relaxed)) as f32;
            let n = data.numSamples as usize;
            let (ins, outs) = unsafe { (&*data.inputs, &*data.outputs) };
            for ch in 0..2 {
                let input = unsafe { *ins.__field0.channelBuffers32.add(ch) };
                let output = unsafe { *outs.__field0.channelBuffers32.add(ch) };
                for i in 0..n {
                    unsafe { *output.add(i) = *input.add(i) * gain };
                }
            }
            kResultOk
        }
        unsafe fn getTailSamples(&self) -> u32 {
            0
        }
    }

    struct Controller {
        gain: Cell<f64>,
    }

    impl Class for Controller {
        type Interfaces = (IEditController,);
    }

    impl IPluginBaseTrait for Controller {
        unsafe fn initialize(&self, _: *mut FUnknown) -> tresult {
            kResultOk
        }
        unsafe fn terminate(&self) -> tresult {
            kResultOk
        }
    }

    impl IEditControllerTrait for Controller {
        unsafe fn setComponentState(&self, _: *mut IBStream) -> tresult {
            kResultOk
        }
        unsafe fn setState(&self, _: *mut IBStream) -> tresult {
            kResultOk
        }
        unsafe fn getState(&self, _: *mut IBStream) -> tresult {
            kResultOk
        }
        unsafe fn getParameterCount(&self) -> i32 {
            2
        }
        unsafe fn getParameterInfo(&self, index: i32, info: *mut ParameterInfo) -> tresult {
            let info = unsafe { &mut *info };
            match index {
                0 => {
                    info.id = 0;
                    copy_w("Gain", &mut info.title);
                    info.defaultNormalizedValue = 0.5;
                    info.flags = ParameterInfo_::ParameterFlags_::kCanAutomate;
                }
                1 => {
                    // A bypass switch, which the host must leave out.
                    info.id = 1;
                    copy_w("Bypass", &mut info.title);
                    info.stepCount = 1;
                    info.flags = ParameterInfo_::ParameterFlags_::kIsBypass;
                }
                _ => return kInvalidArgument,
            }
            kResultOk
        }
        unsafe fn getParamStringByValue(&self, _: u32, _: f64, _: *mut String128) -> tresult {
            kNotImplemented
        }
        unsafe fn getParamValueByString(&self, _: u32, _: *mut TChar, _: *mut f64) -> tresult {
            kNotImplemented
        }
        unsafe fn normalizedParamToPlain(&self, _: u32, v: f64) -> f64 {
            v
        }
        unsafe fn plainParamToNormalized(&self, _: u32, v: f64) -> f64 {
            v
        }
        unsafe fn getParamNormalized(&self, _: u32) -> f64 {
            self.gain.get()
        }
        unsafe fn setParamNormalized(&self, _: u32, v: f64) -> tresult {
            self.gain.set(v);
            kResultOk
        }
        unsafe fn setComponentHandler(&self, _: *mut IComponentHandler) -> tresult {
            kResultOk
        }
        unsafe fn createView(&self, _: *const c_char) -> *mut IPlugView {
            ptr::null_mut()
        }
    }

    struct Factory;

    impl Class for Factory {
        type Interfaces = (IPluginFactory,);
    }

    impl IPluginFactoryTrait for Factory {
        unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
            copy("Antiphon", unsafe { &mut (*info).vendor });
            kResultOk
        }
        unsafe fn countClasses(&self) -> i32 {
            2
        }
        unsafe fn getClassInfo(&self, index: i32, info: *mut PClassInfo) -> tresult {
            let info = unsafe { &mut *info };
            let (cid, category) = match index {
                0 => (PROCESSOR, "Audio Module Class"),
                1 => (CONTROLLER, "Component Controller Class"),
                _ => return kInvalidArgument,
            };
            info.cid = cid;
            info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as i32;
            copy(category, &mut info.category);
            copy("Antiphon Test Gain", &mut info.name);
            kResultOk
        }
        unsafe fn createInstance(&self, cid: FIDString, iid: FIDString, obj: *mut *mut c_void) -> tresult {
            let instance = match unsafe { *(cid as *const TUID) } {
                PROCESSOR => ComWrapper::new(Processor { gain: AtomicU64::new(0.5f64.to_bits()) }).to_com_ptr::<FUnknown>(),
                CONTROLLER => ComWrapper::new(Controller { gain: Cell::new(0.5) }).to_com_ptr::<FUnknown>(),
                _ => None,
            };
            let Some(instance) = instance else { return kInvalidArgument };
            let p = instance.as_ptr();
            unsafe { ((*(*p).vtbl).queryInterface)(p, iid as *mut TUID, obj) }
        }
    }

    #[no_mangle]
    extern "system" fn GetPluginFactory() -> *mut IPluginFactory {
        ComWrapper::new(Factory).to_com_ptr::<IPluginFactory>().map_or(ptr::null_mut(), |f| f.into_raw())
    }

    #[cfg(target_os = "linux")]
    #[no_mangle]
    extern "C" fn ModuleEntry(handle: *mut c_void) -> bool {
        // The host must pass the library's handle.
        !handle.is_null()
    }

    #[cfg(target_os = "linux")]
    #[no_mangle]
    extern "C" fn ModuleExit() -> bool {
        true
    }

    #[cfg(target_os = "windows")]
    #[no_mangle]
    extern "system" fn InitDll() -> bool {
        true
    }

    #[cfg(target_os = "windows")]
    #[no_mangle]
    extern "system" fn ExitDll() -> bool {
        true
    }

    #[cfg(target_os = "macos")]
    #[no_mangle]
    extern "C" fn bundleEntry(_: *mut c_void) -> bool {
        true
    }

    #[cfg(target_os = "macos")]
    #[no_mangle]
    extern "C" fn bundleExit() -> bool {
        true
    }

    /// Makes a `.vst3` bundle in `dir` around `library` (this crate's
    /// built library), laid out for this platform.
    pub fn write_bundle(dir: &Path, library: &Path) -> std::io::Result<std::path::PathBuf> {
        let bundle = dir.join("AntiphonTestGain.vst3");
        let target = if cfg!(target_os = "windows") {
            bundle.join(if cfg!(target_arch = "aarch64") { "Contents/arm64-win" } else { "Contents/x86_64-win" }).join("AntiphonTestGain.vst3")
        } else if cfg!(target_os = "macos") {
            bundle.join("Contents/MacOS/AntiphonTestGain")
        } else {
            let arch = if cfg!(target_arch = "aarch64") { "aarch64-linux" } else { "x86_64-linux" };
            bundle.join("Contents").join(arch).join("AntiphonTestGain.so")
        };
        std::fs::create_dir_all(target.parent().expect("has a folder"))?;
        std::fs::copy(library, &target)?;
        Ok(bundle)
    }
}
