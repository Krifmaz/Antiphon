use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no default input device found")]
    NoInputDevice,
    #[error("no default output device found")]
    NoOutputDevice,
    #[error(transparent)]
    Cpal(#[from] cpal::Error),
}

/// Keeps the input stream alive; dropping it stops capture.
pub struct CaptureHandle {
    _stream: cpal::Stream,
    pub sample_rate: u32,
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
pub fn start_capture(
    device_name: Option<&str>,
    mut on_frame: impl FnMut(&[f32]) + Send + 'static,
) -> Result<CaptureHandle, AudioError> {
    let host = cpal::default_host();
    let device = find_input_device(&host, device_name).ok_or(AudioError::NoInputDevice)?;
    let config = device.default_input_config()?;
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

    Ok(CaptureHandle { _stream: stream, sample_rate })
}

/// Keeps the output stream alive; dropping it stops playback.
pub struct PlaybackHandle {
    _stream: cpal::Stream,
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

/// Starts playback to `device_name` (matched against [`list_output_devices`]),
/// or the system default if `None` or not found, mixed down/up to its
/// native channel count from a mono source. Returns a [`PlaybackSink`] to
/// feed it decoded audio.
pub fn start_playback(device_name: Option<&str>, buffer_seconds: f32) -> Result<(PlaybackHandle, PlaybackSink), AudioError> {
    let host = cpal::default_host();
    let device = find_output_device(&host, device_name).ok_or(AudioError::NoOutputDevice)?;
    let config = device.default_output_config()?;
    let sample_rate = config.sample_rate();
    let channels = config.channels() as usize;
    let stream_config: cpal::StreamConfig = config.into();

    let capacity = ((sample_rate as f32) * buffer_seconds).max(256.0) as usize;
    let ring = HeapRb::<f32>::new(capacity);
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

    Ok((PlaybackHandle { _stream: stream, sample_rate }, PlaybackSink { producer }))
}

/// Lists input device names for a device picker UI (spec §1 "Device Hot-Swap").
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|it| it.filter_map(|d| d.description().ok().map(|desc| desc.name().to_string())).collect())
        .unwrap_or_default()
}

pub fn list_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|it| it.filter_map(|d| d.description().ok().map(|desc| desc.name().to_string())).collect())
        .unwrap_or_default()
}
