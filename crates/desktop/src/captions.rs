//! Live captions in the call view (spec §7 "Accessibility -- on-device
//! live captions").
//!
//! The recogniser lives in `antiphon-captions`; this is the app around it:
//! the switch (off by default), fetching the model only after the person
//! agrees, starting and stopping with the call, and the words to show.
//!
//! **What is captioned.** What the other people in the call say, and your
//! own voice only if you ask. The tap sits *before* the deafen switch and
//! the volume sliders: someone who cannot hear the call is exactly who
//! captions are for, and they may well have the sound off. A person you
//! muted is not captioned: muting means "I don't want them".
//!
//! **Where it goes.** Nowhere. Captions are held in memory for the current
//! call and dropped when it ends. The only way they reach the disk is the
//! person pressing "Save transcript" and choosing a file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use antiphon_captions::model_files::{self, Progress};
use antiphon_captions::{Captioner, Event, WhisperModel};
use antiphon_client::captions::OWN_VOICE;
use antiphon_client::state::AppState;
use antiphon_ui as ui;

/// Lines shown in the call view. Captions are for following along, not
/// reading back; the transcript is there for that.
const SHOWN: usize = 3;
/// Lines kept for the transcript, so a very long call cannot grow without
/// bound.
const KEPT: usize = 2_000;

/// One caption.
#[derive(Clone, Debug, PartialEq)]
struct Line {
    /// Who said it: a leg of the call, `0` for a one-to-one peer, or
    /// [`OWN_VOICE`].
    source: u64,
    /// What was said.
    text: String,
    /// Their name, filled in on the engine's thread, which can see the
    /// call.
    who: Option<String>,
}

/// What the recogniser's thread and the engine share.
#[derive(Default)]
struct Shared {
    /// The current call's captions, oldest first, at most [`KEPT`].
    lines: Mutex<Vec<Line>>,
    /// Bumped on every change to `lines`, so names are filled in only
    /// when something arrived.
    rev: AtomicU64,
    /// What the recogniser is doing, when there is something to say
    /// ("Loading…", why it failed); empty while it just works.
    status: Mutex<String>,
}

impl Shared {
    /// Adds a caption, dropping the oldest past [`KEPT`].
    fn push(&self, source: u64, text: String) {
        let mut lines = self.lines.lock().unwrap();
        lines.push(Line { source, text, who: None });
        let excess = lines.len().saturating_sub(KEPT);
        lines.drain(..excess);
        self.rev.fetch_add(1, Ordering::Relaxed);
    }
}

/// The captions feature: the recogniser while it runs, the model files'
/// download, and the current call's transcript.
#[derive(Default)]
pub struct Captions {
    /// The running recogniser; `None` outside a call or while switched off.
    captioner: Option<Captioner>,
    /// What the recogniser's thread writes into.
    shared: Arc<Shared>,
    /// Which call the transcript belongs to.
    call_started: Option<Instant>,
    /// The last [`Shared::rev`] names were filled in for.
    named_rev: u64,
    /// The model download, while it runs.
    download: Option<Arc<Progress>>,
    /// The outcome of the last download, import or removal, to show
    /// beside the model; filled in from threads.
    message: Arc<Mutex<String>>,
}

impl Captions {
    /// Where the model files live under the data folder `dir`.
    fn model_dir(dir: &Path) -> PathBuf {
        model_files::model_dir(dir)
    }

    /// Starts or stops the recogniser to match the switch and the call,
    /// starts a new transcript with each new call, and names whoever
    /// spoke. Called on every engine tick.
    pub fn tick(&mut self, state: &AppState, dir: Option<&Path>, enabled: bool, own_voice: bool) {
        antiphon_client::captions::set_own_voice(own_voice);
        if let Some(p) = &self.download {
            if p.done.load(Ordering::Relaxed) {
                *self.message.lock().unwrap() = match p.error.lock().unwrap().take() {
                    Some(e) => format!("Download failed: {e}"),
                    None => "Model installed.".to_string(),
                };
                self.download = None;
            }
        }
        let model = dir.map(Self::model_dir).filter(|d| model_files::installed(d));

        let call = state.call.lock().unwrap();
        let call_started = call.as_ref().map(|c| c.started_at);
        // A new call starts a new transcript; the last one is gone, as
        // promised, unless it was saved.
        if call_started.is_some() && call_started != self.call_started {
            self.shared.lines.lock().unwrap().clear();
            self.shared.rev.fetch_add(1, Ordering::Relaxed);
        }
        self.call_started = call_started;

        match (enabled && call_started.is_some(), model, self.captioner.is_some()) {
            (true, Some(model), false) => {
                let shared = self.shared.clone();
                *shared.status.lock().unwrap() = "Loading the caption model…".into();
                let captioner = Captioner::start(
                    move || WhisperModel::load(&model),
                    move |event| match event {
                        Event::Ready => shared.status.lock().unwrap().clear(),
                        Event::Failed(e) => *shared.status.lock().unwrap() = format!("Captions unavailable: {e}"),
                        Event::Caption(cap) => shared.push(cap.source, cap.text),
                    },
                );
                antiphon_client::captions::set_tap(Some(captioner.tap()));
                self.captioner = Some(captioner);
            }
            (false, _, true) | (true, None, true) => self.stop(),
            _ => {}
        }

        let rev = self.shared.rev.load(Ordering::Relaxed);
        if rev == self.named_rev {
            return;
        }
        self.named_rev = rev;
        let mut lines = self.shared.lines.lock().unwrap();
        for line in lines.iter_mut().filter(|l| l.who.is_none()) {
            line.who = Some(match (line.source, call.as_ref()) {
                (OWN_VOICE, _) => "You".to_string(),
                (0, Some(call)) => match call.peer_profile.lock().unwrap().as_ref() {
                    Some(h) if !h.display_name.is_empty() => h.display_name.clone(),
                    _ => "Them".to_string(),
                },
                (leg, Some(call)) => call.group.leg_name(leg - 1).unwrap_or_else(|| "Someone".into()),
                _ => "Someone".to_string(),
            });
        }
    }

    /// Stops the recogniser and takes its tap out of the call.
    fn stop(&mut self) {
        antiphon_client::captions::set_tap(None);
        self.captioner = None;
        self.shared.status.lock().unwrap().clear();
    }

    /// Carries out a model or transcript request (the switches are the
    /// engine's, which saves them). File dialogs and copying run off the
    /// window's thread.
    pub fn act(&mut self, what: ui::CaptionsDo, dir: Option<&Path>) {
        let Some(models) = dir.map(Self::model_dir) else { return };
        match what {
            ui::CaptionsDo::Download => {
                if self.download.is_none() {
                    let progress = Arc::new(Progress::default());
                    self.download = Some(progress.clone());
                    std::thread::spawn(move || model_files::download(&models, &progress));
                }
            }
            ui::CaptionsDo::CancelDownload => {
                if let Some(p) = &self.download {
                    p.cancel.store(true, Ordering::Relaxed);
                }
            }
            ui::CaptionsDo::Import => {
                let message = self.message.clone();
                std::thread::spawn(move || {
                    let Some(src) = rfd::FileDialog::new().set_title("Folder with config.json, tokenizer.json and model.safetensors").pick_folder() else {
                        return;
                    };
                    *message.lock().unwrap() = "Copying…".into();
                    *message.lock().unwrap() = match model_files::import_from(&src, &models) {
                        Ok(()) => "Model installed.".to_string(),
                        Err(e) => format!("Not installed: {e}"),
                    };
                });
            }
            ui::CaptionsDo::RemoveModel => {
                // Stop first: the recogniser has the files open on Windows.
                self.stop();
                *self.message.lock().unwrap() = match model_files::remove(&models) {
                    Ok(()) => "Model removed.".to_string(),
                    Err(e) => format!("Couldn't remove the model: {e}"),
                };
            }
            ui::CaptionsDo::SaveTranscript => {
                let text = transcript(&self.shared.lines.lock().unwrap());
                if text.is_empty() {
                    return;
                }
                let message = self.message.clone();
                std::thread::spawn(move || {
                    let Some(path) = rfd::FileDialog::new().set_title("Save captions").set_file_name("captions.txt").add_filter("Text", &["txt"]).save_file() else {
                        return;
                    };
                    if let Err(e) = std::fs::write(&path, text) {
                        *message.lock().unwrap() = format!("Couldn't save the transcript: {e}");
                    }
                });
            }
            ui::CaptionsDo::On(_) | ui::CaptionsDo::OwnVoice(_) => {}
        }
    }

    /// What Settings and the call view show.
    pub fn model(&self, dir: Option<&Path>, enabled: bool, own_voice: bool) -> ui::Captions {
        let lines = self.shared.lines.lock().unwrap();
        let mut status = self.shared.status.lock().unwrap().clone();
        if let Some(cap) = &self.captioner {
            let stats = cap.stats();
            let shed = stats.segments_dropped.load(Ordering::Relaxed) + stats.segments_stale.load(Ordering::Relaxed);
            if status.is_empty() && shed > 0 {
                status = format!("This computer is too slow to caption everything ({shed} skipped).");
            }
        }
        ui::Captions {
            enabled,
            own_voice,
            installed: dir.map(Self::model_dir).is_some_and(|d| model_files::installed(&d)),
            can_download: model_files::download_available(),
            size: format!("about {} MB, from {}", (model_files::total_size() as f64 / 1e6).round() as u64, model_files::HOST),
            downloading: self.download.as_ref().map(|p| p.fraction()),
            message: self.message.lock().unwrap().clone(),
            status,
            live: self.captioner.is_some(),
            lines: lines[lines.len().saturating_sub(SHOWN)..]
                .iter()
                .map(|l| ui::CaptionLine { who: l.who.clone().unwrap_or_default(), text: l.text.clone() })
                .collect(),
            have_transcript: !lines.is_empty(),
        }
    }
}

/// Plain text, one line per caption, for "Save transcript".
fn transcript(lines: &[Line]) -> String {
    let mut out = String::new();
    for l in lines {
        out.push_str(l.who.as_deref().unwrap_or("Someone"));
        out.push_str(": ");
        out.push_str(&l.text);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_is_one_line_per_caption() {
        let lines = vec![
            Line { source: 0, text: "hello there".into(), who: Some("Ana".into()) },
            Line { source: OWN_VOICE, text: "hi".into(), who: Some("You".into()) },
            Line { source: 4, text: "late".into(), who: None },
        ];
        assert_eq!(transcript(&lines), "Ana: hello there\nYou: hi\nSomeone: late\n");
    }

    #[test]
    fn the_transcript_is_bounded() {
        let shared = Shared::default();
        for i in 0..(KEPT + 10) {
            shared.push(0, i.to_string());
        }
        let lines = shared.lines.lock().unwrap();
        assert_eq!(lines.len(), KEPT);
        assert_eq!(lines[0].text, "10");
    }

    #[test]
    fn captions_are_off_by_default() {
        let c = antiphon_client::config::AppConfig::default();
        assert!(!c.captions_enabled);
        assert!(!c.captions_own_voice);
    }
}
