//! Getting call audio to the recogniser without ever slowing the call.
//!
//! The model is slow next to audio: a few hundred milliseconds of CPU per
//! clip on a good machine, seconds on a weak one. If captions fall behind,
//! the only acceptable outcome is *fewer captions* -- never late, choppy,
//! or stalled call audio. So:
//!
//! - The call's audio path only ever calls [`CaptionTap::push`], which
//!   copies a frame into a bounded channel with `try_send`. Full means the
//!   frame is dropped and counted; it never waits.
//! - A segmenter thread resamples to 16 kHz and cuts utterances
//!   ([`crate::vad`]) per speaker. That work is light and keeps up.
//! - Utterances go to the recogniser thread through a second, tiny queue.
//!   If it is still busy with an earlier one the new clip is dropped, and
//!   one that waited too long is skipped: a caption for something said
//!   ten seconds ago is noise, not help.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::resample::Resampler;
use crate::vad::Vad;

/// Frames waiting for the segmenter. At 10-20 ms a frame this is several
/// seconds -- far more than the segmenter ever lags, so drops here mean
/// something is badly wrong rather than merely slow.
pub const FRAME_QUEUE: usize = 512;
/// Utterances waiting for the recogniser. Small on purpose: anything that
/// waits behind two others is already too late to be live.
pub const SEGMENT_QUEUE: usize = 2;
/// An utterance older than this when the recogniser reaches it is skipped.
pub const MAX_LAG: Duration = Duration::from_secs(10);

/// Who said it: the caller's own numbering of the audio sources.
pub type SourceId = u64;

pub trait Transcriber {
    /// Text for one utterance of 16 kHz mono audio; empty for "nothing
    /// worth showing".
    fn transcribe(&mut self, pcm16k: &[f32]) -> Result<String, String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Caption {
    pub source: SourceId,
    pub text: String,
}

#[derive(Debug)]
pub enum Event {
    /// The model loaded; captions will follow.
    Ready,
    Caption(Caption),
    /// The model could not be loaded. Nothing more will come.
    Failed(String),
}

/// What was thrown away, so the UI can say "captions are falling behind"
/// instead of silently missing words.
#[derive(Default, Debug)]
pub struct Stats {
    pub frames_dropped: AtomicU64,
    pub segments_dropped: AtomicU64,
    pub segments_stale: AtomicU64,
    pub segments_transcribed: AtomicU64,
}

struct Frame {
    source: SourceId,
    rate: u32,
    samples: Vec<f32>,
}

struct Segment {
    source: SourceId,
    samples: Vec<f32>,
    made: Instant,
}

/// The handle the audio path holds. Cheap to clone; never blocks.
#[derive(Clone)]
pub struct CaptionTap {
    tx: SyncSender<Frame>,
    stats: Arc<Stats>,
}

impl CaptionTap {
    /// Offers audio for captioning. Returns false when it was dropped.
    pub fn push(&self, source: SourceId, rate: u32, samples: &[f32]) -> bool {
        if samples.is_empty() {
            return true;
        }
        let frame = Frame { source, rate, samples: samples.to_vec() };
        match self.tx.try_send(frame) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }
}

pub struct Captioner {
    tap: CaptionTap,
    stop: Arc<AtomicBool>,
    stats: Arc<Stats>,
}

impl Captioner {
    /// Starts the two worker threads. `load` runs on the recogniser
    /// thread, so loading a 150 MB model never holds up the caller.
    pub fn start<T, L, E>(load: L, mut events: E) -> Self
    where
        T: Transcriber,
        L: FnOnce() -> Result<T, String> + Send + 'static,
        E: FnMut(Event) + Send + 'static,
    {
        let stats = Arc::new(Stats::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (frame_tx, frame_rx) = sync_channel::<Frame>(FRAME_QUEUE);
        let (seg_tx, seg_rx) = sync_channel::<Segment>(SEGMENT_QUEUE);

        let seg_stop = stop.clone();
        let seg_stats = stats.clone();
        thread::Builder::new()
            .name("captions-segmenter".into())
            .spawn(move || segmenter(&frame_rx, &seg_tx, &seg_stop, &seg_stats))
            .expect("spawn captions segmenter");

        let asr_stop = stop.clone();
        let asr_stats = stats.clone();
        thread::Builder::new()
            .name("captions-recogniser".into())
            .spawn(move || {
                let mut model = match load() {
                    Ok(m) => m,
                    Err(e) => {
                        events(Event::Failed(e));
                        return;
                    }
                };
                events(Event::Ready);
                recogniser(&mut model, &seg_rx, &asr_stop, &asr_stats, &mut events);
            })
            .expect("spawn captions recogniser");

        Self { tap: CaptionTap { tx: frame_tx, stats: stats.clone() }, stop, stats }
    }

    pub fn tap(&self) -> CaptionTap {
        self.tap.clone()
    }

    pub fn stats(&self) -> Arc<Stats> {
        self.stats.clone()
    }
}

impl Drop for Captioner {
    /// The threads notice within a poll interval and exit. Not joined: a
    /// clip mid-transcription can take a while, and whoever turned
    /// captions off should not wait for it.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

const POLL: Duration = Duration::from_millis(100);

fn segmenter(rx: &Receiver<Frame>, tx: &SyncSender<Segment>, stop: &AtomicBool, stats: &Stats) {
    let mut speakers: HashMap<SourceId, (u32, Resampler, Vad)> = HashMap::new();
    let mut resampled = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let frame = match rx.recv_timeout(POLL) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let entry = speakers
            .entry(frame.source)
            .or_insert_with(|| (frame.rate, Resampler::new(frame.rate, 16_000), Vad::default()));
        if entry.0 != frame.rate {
            *entry = (frame.rate, Resampler::new(frame.rate, 16_000), Vad::default());
        }
        resampled.clear();
        entry.1.process(&frame.samples, &mut resampled);
        for samples in entry.2.push(&resampled) {
            let seg = Segment { source: frame.source, samples, made: Instant::now() };
            if tx.try_send(seg).is_err() {
                stats.segments_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn recogniser<T: Transcriber>(
    model: &mut T,
    rx: &Receiver<Segment>,
    stop: &AtomicBool,
    stats: &Stats,
    events: &mut impl FnMut(Event),
) {
    while !stop.load(Ordering::Relaxed) {
        let seg = match rx.recv_timeout(POLL) {
            Ok(s) => s,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if seg.made.elapsed() > MAX_LAG {
            stats.segments_stale.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        match model.transcribe(&seg.samples) {
            Ok(text) => {
                stats.segments_transcribed.fetch_add(1, Ordering::Relaxed);
                let text = text.trim().to_string();
                if !text.is_empty() && !stop.load(Ordering::Relaxed) {
                    events(Event::Caption(Caption { source: seg.source, text }));
                }
            }
            // One bad clip is not a reason to stop captioning the call.
            Err(e) => eprintln!("[captions] {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        delay: Duration,
    }

    impl Transcriber for Fake {
        fn transcribe(&mut self, pcm: &[f32]) -> Result<String, String> {
            thread::sleep(self.delay);
            Ok(format!("{} ms", pcm.len() / 16))
        }
    }

    /// 48 kHz: hiss, then `talk` seconds of voice-like tone, then hiss.
    fn utterance(talk: f32) -> Vec<f32> {
        let mut seed = 7u32;
        let mut out = Vec::new();
        let mut hiss = |n: usize, out: &mut Vec<f32>| {
            for _ in 0..n {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                out.push(((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.002);
            }
        };
        hiss(24_000, &mut out);
        for i in 0..(48_000.0 * talk) as usize {
            let t = i as f32 / 48_000.0;
            out.push(0.2 * (2.0 * std::f32::consts::PI * 180.0 * t).sin());
        }
        hiss(48_000, &mut out);
        out
    }

    fn collect(events: &Arc<Mutex<Vec<Caption>>>) -> impl FnMut(Event) + Send + 'static {
        let events = events.clone();
        move |e| {
            if let Event::Caption(c) = e {
                events.lock().unwrap().push(c);
            }
        }
    }

    fn wait_for(cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn utterances_come_back_labelled_with_their_speaker() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let c = Captioner::start(|| Ok(Fake { delay: Duration::ZERO }), collect(&got));
        let tap = c.tap();
        for chunk in utterance(1.0).chunks(480) {
            tap.push(3, 48_000, chunk);
        }
        wait_for(|| !got.lock().unwrap().is_empty());
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].source, 3);
    }

    #[test]
    fn two_speakers_are_segmented_separately() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let c = Captioner::start(|| Ok(Fake { delay: Duration::ZERO }), collect(&got));
        let tap = c.tap();
        let a = utterance(1.0);
        // Interleaved frame by frame, as the mixer would hand them over:
        // a shared detector would see one long run of speech.
        for (x, y) in a.chunks(480).zip(a.chunks(480)) {
            tap.push(1, 48_000, x);
            tap.push(2, 48_000, y);
            thread::sleep(Duration::from_micros(50));
        }
        wait_for(|| got.lock().unwrap().len() >= 2);
        let mut sources: Vec<_> = got.lock().unwrap().iter().map(|c| c.source).collect();
        sources.sort_unstable();
        assert_eq!(sources, vec![1, 2]);
    }

    #[test]
    fn a_slow_recogniser_never_blocks_the_audio_path() {
        // Each clip takes far longer to "transcribe" than it lasts.
        let got = Arc::new(Mutex::new(Vec::new()));
        let c = Captioner::start(|| Ok(Fake { delay: Duration::from_secs(2) }), collect(&got));
        let tap = c.tap();
        let stats = c.stats();

        // Twelve utterances back to back, pushed as fast as possible:
        // the recogniser can only take a couple of them.
        let mut audio = Vec::new();
        for _ in 0..12 {
            audio.extend(utterance(0.8));
        }
        let mut worst = Duration::ZERO;
        let started = Instant::now();
        for chunk in audio.chunks(480) {
            let t = Instant::now();
            tap.push(0, 48_000, chunk);
            worst = worst.max(t.elapsed());
        }
        let total = started.elapsed();
        assert!(worst < Duration::from_millis(20), "a push waited {worst:?}");
        assert!(total < Duration::from_secs(2), "pushing took {total:?}");

        wait_for(|| {
            stats.segments_dropped.load(Ordering::Relaxed)
                + stats.frames_dropped.load(Ordering::Relaxed)
                > 0
        });
        let dropped = stats.segments_dropped.load(Ordering::Relaxed)
            + stats.frames_dropped.load(Ordering::Relaxed);
        assert!(dropped > 0, "nothing was shed, so something must have waited");
    }

    #[test]
    fn a_full_frame_queue_drops_instead_of_waiting() {
        // A recogniser that never finishes loading, and a segmenter that
        // is starved by a flood: the tap still returns immediately.
        let got = Arc::new(Mutex::new(Vec::new()));
        let c = Captioner::start(
            || {
                thread::sleep(Duration::from_secs(3));
                Ok(Fake { delay: Duration::ZERO })
            },
            collect(&got),
        );
        let tap = c.tap();
        let frame = vec![0.1f32; 960];
        let t = Instant::now();
        let mut accepted = 0;
        for _ in 0..(FRAME_QUEUE * 20) {
            accepted += tap.push(0, 48_000, &frame) as usize;
        }
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(accepted >= FRAME_QUEUE);
    }

    #[test]
    fn a_model_that_fails_to_load_is_reported() {
        let failed = Arc::new(AtomicBool::new(false));
        let f = failed.clone();
        let c = Captioner::start(
            || Err::<Fake, _>("no model".to_string()),
            move |e| {
                if matches!(e, Event::Failed(_)) {
                    f.store(true, Ordering::Relaxed);
                }
            },
        );
        wait_for(|| failed.load(Ordering::Relaxed));
        assert!(failed.load(Ordering::Relaxed));
        // The audio path still returns at once with nobody listening.
        let t = Instant::now();
        for _ in 0..(FRAME_QUEUE * 2) {
            c.tap().push(0, 48_000, &[0.0; 960]);
        }
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
