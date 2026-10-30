use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no default input device found")]
    NoInputDevice,
    #[error("no default output device found")]
    NoOutputDevice,
    #[error("this system cannot record what it plays")]
    NoLoopback,
    #[error(transparent)]
    Cpal(#[from] cpal::Error),
}

/// An open stream from whichever API opened it. It is only held so that
/// dropping it closes the stream.
enum StreamHolder {
    /// A stream cpal opened: every desktop, iOS, and Android before 9.
    Cpal { _stream: cpal::Stream },
    /// A call stream opened through AAudio directly, with the
    /// voice-communication attributes cpal cannot set (see `aaudio.rs`).
    #[cfg(target_os = "android")]
    AAudio { _stream: crate::aaudio::Stream },
}

/// Keeps the input stream alive; dropping it stops capture.
pub struct CaptureHandle {
    _stream: StreamHolder,
    pub sample_rate: u32,
}

/// Which audio system the desktop talks to on Linux.
///
/// PipeWire is preferred because it is what current distributions run: its
/// device list names the real sinks and sources (a USB headset, the HDMI
/// output) where ALSA on a PipeWire system mostly shows its "default" and
/// "pipewire" plug-ins, and talking to it directly skips the ALSA
/// compatibility layer and its extra buffering. ALSA stays as the fallback
/// for systems without a PipeWire daemon (PulseAudio or bare ALSA), and as
/// an escape hatch should a PipeWire setup misbehave. Other platforms have
/// one audio API each, so this changes nothing there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioBackend {
    /// PipeWire when its daemon is running, otherwise ALSA.
    Auto,
    /// ALSA even when PipeWire is running.
    Alsa,
}

impl AudioBackend {
    /// The environment variable that overrides the automatic choice. It is
    /// an environment variable rather than a settings entry because the only
    /// reason to change it is a broken audio setup, and the person debugging
    /// that is better served by something that works before the app's own
    /// audio does.
    pub const ENV: &'static str = "ANTIPHON_AUDIO_BACKEND";

    /// Reads a value of [`Self::ENV`]. `alsa` forces ALSA; anything else,
    /// including nothing, is [`AudioBackend::Auto`], so a typo never leaves
    /// someone without sound.
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if v == "alsa" => AudioBackend::Alsa,
            _ => AudioBackend::Auto,
        }
    }

    /// The choice in effect for this process, from [`Self::ENV`].
    pub fn from_env() -> Self {
        Self::parse(std::env::var(Self::ENV).ok().as_deref())
    }
}

/// The cpal host to list and open devices on. Only Linux has a choice to
/// make; see [`AudioBackend`].
fn host() -> cpal::Host {
    #[cfg(target_os = "linux")]
    if AudioBackend::from_env() == AudioBackend::Alsa {
        if let Ok(host) = cpal::host_from_id(cpal::HostId::Alsa) {
            return host;
        }
    }
    // With cpal's `pipewire` feature on (see Cargo.toml), the default host
    // on Linux is PipeWire when its socket exists and connecting to it
    // succeeds, and ALSA otherwise. Everywhere else it is the platform's
    // one API: WASAPI, CoreAudio, or AAudio.
    cpal::default_host()
}

/// The name of the audio system devices are opened on, such as "PipeWire"
/// or "ALSA", so a settings screen or a log line can show which one the
/// automatic choice made.
pub fn audio_backend_name() -> &'static str {
    host().id().name()
}

/// Finds an input device by its display name (as returned by
/// [`list_input_devices`]), falling back to the system default if `name` is
/// `None` or doesn't match anything currently connected (e.g. it was
/// unplugged since being saved to a preset).
fn find_input_device(host: &cpal::Host, name: Option<&str>) -> Option<cpal::Device> {
    if let Some(name) = name {
        if let Some(found) =
            host.input_devices().ok()?.find(|d| d.description().map(|desc| desc.name() == name).unwrap_or(false))
        {
            return Some(found);
        }
    }
    host.default_input_device()
}

fn find_output_device(host: &cpal::Host, name: Option<&str>) -> Option<cpal::Device> {
    if let Some(name) = name {
        if let Some(found) =
            host.output_devices().ok()?.find(|d| d.description().map(|desc| desc.name() == name).unwrap_or(false))
        {
            return Some(found);
        }
    }
    host.default_output_device()
}

/// Starts capturing from `device_name` (matched against [`list_input_devices`]),
/// or the system default microphone if `None` or not found (spec §1 "Device
/// Hot-Swap" -- swapping the saved device out from under a running call
/// isn't implemented, but picking one up front is). `on_frame` is called
/// from the audio callback thread with mono f32 samples -- keep it fast
/// (push to a queue), never block or allocate heavily there.
///
/// On Android 9 and later the default microphone is opened through AAudio
/// with the voice-communication preset instead, at 48 kHz (see `aaudio.rs`),
/// so the phone's own echo canceller and noise suppressor run. The phone
/// app only opens the microphone for calls, so that preset is always the
/// right one there. If AAudio refuses, cpal opens it as before.
pub fn start_capture(
    device_name: Option<&str>,
    on_frame: impl FnMut(&[f32]) + Send + 'static,
) -> Result<CaptureHandle, AudioError> {
    #[cfg(target_os = "android")]
    let on_frame = match device_name {
        Some(_) => on_frame,
        None => match crate::aaudio::open_capture(on_frame) {
            Ok(stream) => {
                let handle = StreamHolder::AAudio { _stream: stream };
                return Ok(CaptureHandle { _stream: handle, sample_rate: crate::aaudio::SAMPLE_RATE });
            }
            Err((e, on_frame)) => {
                eprintln!("[audio] opening the microphone through cpal instead: {e}");
                on_frame
            }
        },
    };
    start_cpal_capture(device_name, on_frame)
}

/// [`start_capture`] through cpal, which every platform supports.
fn start_cpal_capture(
    device_name: Option<&str>,
    on_frame: impl FnMut(&[f32]) + Send + 'static,
) -> Result<CaptureHandle, AudioError> {
    let host = host();
    let device = find_input_device(&host, device_name).ok_or(AudioError::NoInputDevice)?;
    let config = device.default_input_config()?;
    capture_from(&device, config, on_frame)
}

/// Starts recording **what this computer plays** -- the system's output
/// mix -- for sharing a screen's sound (spec §5). `on_frame` gets mono
/// samples as [`start_capture`] does.
///
/// - **Linux:** a PipeWire stream that captures the default sink, the same
///   thing pavucontrol's "Monitor of ..." records. Needs PipeWire (every
///   current desktop); on plain ALSA there is no loopback, so
///   [`AudioError::NoLoopback`].
/// - **Windows:** WASAPI loopback on the default output device.
/// - **macOS:** CoreAudio's process tap on the default output device
///   (macOS 14.2 and later), which asks for permission the first time.
///
/// The mix includes the call itself: whoever records it must take the
/// call's own playback back out (the client does, with an echo canceller
/// fed what the call played).
pub fn start_system_capture(on_frame: impl FnMut(&[f32]) + Send + 'static) -> Result<CaptureHandle, AudioError> {
    #[cfg(target_os = "linux")]
    let host = cpal::host_from_id(cpal::HostId::PipeWire).map_err(|_| AudioError::NoLoopback)?;
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let host = cpal::default_host();
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        drop(on_frame);
        Err(AudioError::NoLoopback)
    }
    #[cfg(target_os = "linux")]
    {
        // cpal's PipeWire host lists the default sink as a device that
        // both plays and records; recording from it captures the sink.
        let device = host
            .devices()
            .map_err(|_| AudioError::NoLoopback)?
            .filter(|d| d.supports_input() && d.supports_output())
            .min_by_key(|d| d.description().map(|desc| desc.name() != "default_sink").unwrap_or(true))
            .ok_or(AudioError::NoLoopback)?;
        let config = device.default_input_config()?;
        capture_from(&device, config, on_frame)
    }
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        // An output device opened for input is loopback on both: the
        // stream takes the output's own format.
        let device = host.default_output_device().ok_or(AudioError::NoOutputDevice)?;
        let config = device.default_output_config()?;
        capture_from(&device, config, on_frame)
    }
}

/// Records from `device` (a microphone, or an output device opened for
/// loopback) in `config`, mixed down to mono.
fn capture_from(
    device: &cpal::Device,
    config: cpal::SupportedStreamConfig,
    mut on_frame: impl FnMut(&[f32]) + Send + 'static,
) -> Result<CaptureHandle, AudioError> {
    let sample_rate = config.sample_rate();
    let channels = config.channels() as usize;
    let stream_config: cpal::StreamConfig = config.into();

    let mut mono_buf: Vec<f32> = Vec::new();
    let stream = device.build_input_stream(
        stream_config,
        move |data: &[f32], _info: &cpal::InputCallbackInfo| {
            if channels == 1 {
                on_frame(data);
            } else {
                mono_buf.clear();
                mono_buf.extend(data.chunks_exact(channels).map(|frame| frame.iter().sum::<f32>() / channels as f32));
                on_frame(&mono_buf);
            }
        },
        |err| eprintln!("[audio] input stream error: {err}"),
        None,
    )?;
    stream.play()?;

    Ok(CaptureHandle { _stream: StreamHolder::Cpal { _stream: stream }, sample_rate })
}

/// Keeps the output stream alive; dropping it stops playback.
pub struct PlaybackHandle {
    _stream: StreamHolder,
    pub sample_rate: u32,
}

/// A handle for pushing decoded mono samples into the playback queue from
/// another thread (e.g. the network receive loop).
pub struct PlaybackSink {
    producer: ringbuf::HeapProd<f32>,
}

impl PlaybackSink {
    /// Pushes samples into the playback buffer, dropping any that don't fit
    /// rather than blocking -- for a live call, a bounded amount of extra
    /// latency beats blocking the network thread.
    pub fn push(&mut self, samples: &[f32]) {
        for &s in samples {
            let _ = self.producer.try_push(s);
        }
    }
}

/// How many mono samples the playback queue holds: `buffer_seconds` of
/// audio at `sample_rate`, and never fewer than 256, so that a tiny or zero
/// buffer still leaves room for one callback's worth.
fn ring_capacity(sample_rate: u32, buffer_seconds: f32) -> usize {
    ((sample_rate as f32) * buffer_seconds).max(256.0) as usize
}

/// Starts playback to `device_name` (matched against [`list_output_devices`]),
/// or the system default if `None` or not found, mixed down/up to its
/// native channel count from a mono source. Returns a [`PlaybackSink`] to
/// feed it decoded audio.
///
/// On Android 9 and later the default output is opened through AAudio with
/// the voice-communication usage instead, at 48 kHz (see `aaudio.rs`), so it
/// follows a phone call's earpiece and speakerphone routing. As with
/// [`start_capture`], the phone app only plays audio this way during calls,
/// and cpal takes over if AAudio refuses.
pub fn start_playback(device_name: Option<&str>, buffer_seconds: f32) -> Result<(PlaybackHandle, PlaybackSink), AudioError> {
    #[cfg(target_os = "android")]
    if device_name.is_none() {
        let ring = HeapRb::<f32>::new(ring_capacity(crate::aaudio::SAMPLE_RATE, buffer_seconds));
        let (producer, mut consumer) = ring.split();
        let fill = move |out: &mut [f32]| {
            for sample in out.iter_mut() {
                *sample = consumer.try_pop().unwrap_or(0.0);
            }
        };
        match crate::aaudio::open_playback(fill) {
            Ok(stream) => {
                let handle = PlaybackHandle {
                    _stream: StreamHolder::AAudio { _stream: stream },
                    sample_rate: crate::aaudio::SAMPLE_RATE,
                };
                return Ok((handle, PlaybackSink { producer }));
            }
            Err(e) => eprintln!("[audio] opening the speaker through cpal instead: {e}"),
        }
    }

    let host = host();
    let device = find_output_device(&host, device_name).ok_or(AudioError::NoOutputDevice)?;
    let config = device.default_output_config()?;
    let sample_rate = config.sample_rate();
    let channels = config.channels() as usize;
    let stream_config: cpal::StreamConfig = config.into();

    let ring = HeapRb::<f32>::new(ring_capacity(sample_rate, buffer_seconds));
    let (producer, mut consumer) = ring.split();

    let stream = device.build_output_stream(
        stream_config,
        move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
            for frame in data.chunks_mut(channels) {
                let sample = consumer.try_pop().unwrap_or(0.0);
                for out in frame.iter_mut() {
                    *out = sample;
                }
            }
        },
        |err| eprintln!("[audio] output stream error: {err}"),
        None,
    )?;
    stream.play()?;

    Ok((PlaybackHandle { _stream: StreamHolder::Cpal { _stream: stream }, sample_rate }, PlaybackSink { producer }))
}

/// Lists input device names for a device picker UI (spec §1 "Device Hot-Swap").
/// The names come from the same host [`start_capture`] opens on, so a name
/// picked here is one it can find.
pub fn list_input_devices() -> Vec<String> {
    let host = host();
    host.input_devices()
        .map(|it| it.filter_map(|d| d.description().ok().map(|desc| desc.name().to_string())).collect())
        .unwrap_or_default()
}

/// Lists output device names for a device picker UI, from the same host
/// [`start_playback`] opens on.
pub fn list_output_devices() -> Vec<String> {
    let host = host();
    host.output_devices()
        .map(|it| it.filter_map(|d| d.description().ok().map(|desc| desc.name().to_string())).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_override_only_recognises_alsa() {
        assert_eq!(AudioBackend::parse(None), AudioBackend::Auto);
        assert_eq!(AudioBackend::parse(Some("alsa")), AudioBackend::Alsa);
        assert_eq!(AudioBackend::parse(Some(" ALSA\n")), AudioBackend::Alsa);
        assert_eq!(AudioBackend::parse(Some("pipewire")), AudioBackend::Auto);
        assert_eq!(AudioBackend::parse(Some("alas")), AudioBackend::Auto);
        assert_eq!(AudioBackend::parse(Some("")), AudioBackend::Auto);
    }

    #[test]
    fn ring_holds_the_requested_time_with_a_floor() {
        assert_eq!(ring_capacity(48_000, 0.5), 24_000);
        assert_eq!(ring_capacity(44_100, 1.0), 44_100);
        assert_eq!(ring_capacity(48_000, 0.0), 256);
    }
}
