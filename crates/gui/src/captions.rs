//! Live captions in the call view (spec §7 "Accessibility -- on-device
//! live captions").
//!
//! The recogniser lives in `antiphon-captions`; this is the app around it:
//! the switch (off by default), fetching the model only after the user
//! agrees, starting and stopping with the call, and showing the words.
//!
//! **What is captioned.** What the other people in the call say, and your
//! own voice only if you ask. The tap sits *before* the deafen switch and
//! the volume sliders: someone who cannot hear the call is exactly who
//! captions are for, and they may well have the sound off. A person you
//! muted is not captioned -- muting means "I don't want them".
//!
//! **Where it goes.** Nowhere. Captions are held in memory for the current
//! call and dropped when it ends. The only way they reach the disk is the
//! user pressing "Save transcript" and choosing a file.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use antiphon_captions::model_files::{self, Progress};
use antiphon_captions::{CaptionTap, Captioner, Event, WhisperModel};
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};

use crate::state::AppState;
use crate::{CaptionLine, MainWindow};

/// The source id for your own voice. The other side is 0, and group call
/// legs are their leg id + 1, matching the mixer's numbering.
pub const OWN_VOICE: u64 = u64::MAX;

/// Lines shown in the call view. Captions are for following along, not
/// reading back -- the transcript is there for that.
const SHOWN: usize = 3;
/// Lines kept for the transcript, so a very long call cannot grow without
/// bound.
const KEPT: usize = 2_000;

static TAP: Mutex<Option<CaptionTap>> = Mutex::new(None);
static OWN: AtomicBool = AtomicBool::new(false);

/// The tap for the call's audio path, while captions are running.
pub fn tap() -> Option<CaptionTap> {
    TAP.lock().ok()?.clone()
}

/// Whether to caption your own voice too.
pub fn own_voice() -> bool {
    OWN.load(Ordering::Relaxed)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub source: u64,
    pub text: String,
    /// Filled in on the UI thread, which can see the call.
    pub who: Option<String>,
}

#[derive(Default)]
struct Shared {
    lines: Mutex<Vec<Line>>,
    rev: AtomicU64,
    status: Mutex<String>,
}

impl Shared {
    fn push(&self, source: u64, text: String) {
        let mut lines = self.lines.lock().unwrap();
        lines.push(Line { source, text, who: None });
        let excess = lines.len().saturating_sub(KEPT);
        lines.drain(..excess);
        self.rev.fetch_add(1, Ordering::Relaxed);
    }
}

struct Captions {
    captioner: Option<Captioner>,
    shared: Arc<Shared>,
    /// Which call the transcript belongs to.
    call_started: Option<Instant>,
    shown_rev: u64,
    download: Option<Arc<Progress>>,
    /// The outcome of an import, waiting to be shown.
    import_result: Arc<Mutex<Option<String>>>,
}

fn model_dir() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| model_files::model_dir(&d))
}

/// Plain text, one line per caption, for "Save transcript".
pub fn transcript(lines: &[Line]) -> String {
    let mut out = String::new();
    for l in lines {
        out.push_str(l.who.as_deref().unwrap_or("Someone"));
        out.push_str(": ");
        out.push_str(&l.text);
        out.push('\n');
    }
    out
}

fn size_text() -> String {
    format!(
        "about {} MB, from {}",
        (model_files::total_size() as f64 / 1e6).round() as u64,
        model_files::HOST
    )
}

pub fn wire(main_window: &MainWindow, app_state: &Arc<AppState>) {
    let captions = Rc::new(RefCell::new(Captions {
        captioner: None,
        shared: Arc::new(Shared::default()),
        call_started: None,
        shown_rev: 0,
        download: None,
        import_result: Arc::new(Mutex::new(None)),
    }));
    main_window.set_captions_size_text(size_text().into());
    main_window.set_captions_download_allowed(model_files::download_available());

    {
        let window_weak = main_window.as_weak();
        main_window.on_captions_settings_changed(move || {
            let Some(app) = window_weak.upgrade() else { return };
            crate::config::save(&crate::current_config(&app));
        });
    }
    {
        let c = captions.clone();
        main_window.on_captions_download(move || {
            let Some(dir) = model_dir() else { return };
            let mut c = c.borrow_mut();
            if c.download.is_some() {
                return;
            }
            let progress = Arc::new(Progress::default());
            c.download = Some(progress.clone());
            std::thread::spawn(move || model_files::download(&dir, progress));
        });
    }
    {
        let c = captions.clone();
        main_window.on_captions_cancel_download(move || {
            if let Some(p) = &c.borrow().download {
                p.cancel.store(true, Ordering::Relaxed);
            }
        });
    }
    {
        let c = captions.clone();
        main_window.on_captions_import(move || {
            let Some(dir) = model_dir() else { return };
            let Some(src) = rfd::FileDialog::new()
                .set_title("Folder with config.json, tokenizer.json and model.safetensors")
                .pick_folder()
            else {
                return;
            };
            let result = c.borrow().import_result.clone();
            *result.lock().unwrap() = Some("Copying...".into());
            std::thread::spawn(move || {
                let outcome = match model_files::import_from(&src, &dir) {
                    Ok(()) => "Model installed.".to_string(),
                    Err(e) => format!("Not installed: {e}"),
                };
                *result.lock().unwrap() = Some(outcome);
            });
        });
    }
    {
        let c = captions.clone();
        main_window.on_captions_remove_model(move || {
            let mut c = c.borrow_mut();
            // Stop first: the recogniser has the files open on Windows.
            c.captioner = None;
            *TAP.lock().unwrap() = None;
            if let Some(dir) = model_dir() {
                let msg = match model_files::remove(&dir) {
                    Ok(()) => "Model removed.".to_string(),
                    Err(e) => format!("Could not remove the model: {e}"),
                };
                *c.import_result.lock().unwrap() = Some(msg);
            }
        });
    }
    {
        let c = captions.clone();
        main_window.on_captions_save_transcript(move || {
            let text = transcript(&c.borrow().shared.lines.lock().unwrap());
            if text.is_empty() {
                return;
            }
            let Some(path) = rfd::FileDialog::new()
                .set_title("Save captions")
                .set_file_name("captions.txt")
                .add_filter("Text", &["txt"])
                .save_file()
            else {
                return;
            };
            if let Err(e) = std::fs::write(&path, text) {
                eprintln!("[captions] saving transcript: {e}");
            }
        });
    }

    let timer = Box::leak(Box::new(Timer::default()));
    let window_weak = main_window.as_weak();
    let state = app_state.clone();
    timer.start(TimerMode::Repeated, Duration::from_millis(250), move || {
        let Some(app) = window_weak.upgrade() else { return };
        tick(&app, &state, &mut captions.borrow_mut());
    });
}

fn tick(app: &MainWindow, state: &AppState, c: &mut Captions) {
    let dir = model_dir();
    let installed = dir.as_deref().is_some_and(model_files::installed);
    OWN.store(app.get_captions_own_voice(), Ordering::Relaxed);

    // Download progress.
    if let Some(p) = c.download.clone() {
        let done = p.done.load(Ordering::Relaxed);
        app.set_captions_downloading(!done);
        app.set_captions_download_fraction(p.fraction());
        if done {
            let msg = match p.error.lock().unwrap().take() {
                Some(e) => format!("Download failed: {e}"),
                None => "Model installed.".to_string(),
            };
            *c.import_result.lock().unwrap() = Some(msg);
            c.download = None;
        }
    } else {
        app.set_captions_downloading(false);
    }
    if let Some(msg) = c.import_result.lock().unwrap().take() {
        app.set_captions_model_message(msg.into());
    }
    app.set_captions_model_installed(installed);

    let call = state.call.lock().unwrap();
    let call_started = call.as_ref().map(|call| call.started_at);
    let want = app.get_captions_enabled() && installed && call_started.is_some();

    // A new call starts a new transcript; the last one is gone, as
    // promised, unless it was saved.
    if call_started.is_some() && call_started != c.call_started {
        c.shared.lines.lock().unwrap().clear();
        c.shared.rev.fetch_add(1, Ordering::Relaxed);
    }
    c.call_started = call_started;

    if want && c.captioner.is_none() {
        let dir = dir.clone().unwrap();
        let shared = c.shared.clone();
        *shared.status.lock().unwrap() = "Loading the caption model...".into();
        let captioner = Captioner::start(
            move || WhisperModel::load(&dir),
            move |event| match event {
                Event::Ready => *shared.status.lock().unwrap() = String::new(),
                Event::Failed(e) => *shared.status.lock().unwrap() = format!("Captions unavailable: {e}"),
                Event::Caption(cap) => shared.push(cap.source, cap.text),
            },
        );
        *TAP.lock().unwrap() = Some(captioner.tap());
        c.captioner = Some(captioner);
    } else if !want && c.captioner.is_some() {
        *TAP.lock().unwrap() = None;
        c.captioner = None;
        c.shared.status.lock().unwrap().clear();
    }
    app.set_captions_live(c.captioner.is_some());

    let mut status = c.shared.status.lock().unwrap().clone();
    if let Some(cap) = &c.captioner {
        let stats = cap.stats();
        let shed = stats.segments_dropped.load(Ordering::Relaxed) + stats.segments_stale.load(Ordering::Relaxed);
        if status.is_empty() && shed > 0 {
            status = format!("This computer is too slow to caption everything ({shed} skipped).");
        }
    }
    app.set_captions_status(status.into());

    let rev = c.shared.rev.load(Ordering::Relaxed);
    if rev == c.shown_rev {
        return;
    }
    c.shown_rev = rev;
    let mut lines = c.shared.lines.lock().unwrap();
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
    let shown: Vec<CaptionLine> = lines[lines.len().saturating_sub(SHOWN)..]
        .iter()
        .map(|l| CaptionLine {
            who: l.who.clone().unwrap_or_default().into(),
            text: l.text.clone().into(),
        })
        .collect();
    app.set_caption_lines(ModelRc::new(VecModel::from(shown)));
    app.set_captions_have_transcript(!lines.is_empty());
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
        let c = crate::config::AppConfig::default();
        assert!(!c.captions_enabled);
        assert!(!c.captions_own_voice);
        assert!(tap().is_none());
    }
}
