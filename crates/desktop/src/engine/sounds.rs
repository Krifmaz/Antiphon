//! The call's replay history and the soundboard (spec §2 "replay", §13
//! "soundboard"): offering and playing back moments the network damaged,
//! saving what you just heard, and the clips you can play into a call.
//!
//! Playback of either goes through its own output stream rather than the
//! call's, so listening back neither interrupts nor mixes into the live
//! audio still arriving. Clips sent *into* the call go through the call's
//! soundboard queue, mixed with the microphone.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use antiphon_client::{soundboard, state, wav};
use antiphon_ui::{self as ui, ToastKind};

use super::{Engine, Event};

impl Engine {
    /// Watches the call's replay history for a newly damaged moment and
    /// offers or plays it as the replay mode says. Called every tick.
    pub(super) fn watch_replay(&mut self) {
        let cut = self.state.call.lock().unwrap().as_ref().map(state::ActiveCall::replay_clips_cut);
        let Some(cut) = cut else {
            self.replay_seen = None;
            self.replay_offer = false;
            return;
        };
        // The first look at a call only learns where it stands, so a call
        // never opens by offering a moment from before it was watched.
        if self.replay_seen.is_some_and(|seen| cut > seen) {
            match replay_mode(self.cfg.replay_mode) {
                ui::ReplayMode::Off => {}
                ui::ReplayMode::Ask => self.replay_offer = true,
                ui::ReplayMode::Auto => {
                    self.toast("Their audio dropped out; replaying it now", ToastKind::Info);
                    self.play_replay(0);
                }
            }
        }
        self.replay_seen = Some(cut);
    }

    /// Carries out a replay request.
    pub(super) fn replay_do(&mut self, what: ui::ReplayDo) {
        match what {
            ui::ReplayDo::Mode(mode) => {
                self.cfg.replay_mode = match mode {
                    ui::ReplayMode::Off => 0,
                    ui::ReplayMode::Ask => 1,
                    ui::ReplayMode::Auto => 2,
                };
                self.save();
            }
            ui::ReplayDo::Play(index) => {
                self.replay_offer = false;
                self.play_replay(index);
            }
            ui::ReplayDo::Save => self.save_replay(),
            ui::ReplayDo::Clear => {
                self.with_call(state::ActiveCall::clear_replay_clips);
                self.replay_offer = false;
            }
            ui::ReplayDo::Dismiss => self.replay_offer = false,
        }
    }

    /// Plays the kept moment at `index` (newest first) on this computer.
    fn play_replay(&mut self, index: usize) {
        if let Some((rate, samples)) = self.with_call(|a| a.replay_clip(index)).flatten() {
            self.play_here(rate, &samples);
        }
    }

    /// Saves the replay ring (what you just heard) to a WAV file the person
    /// picks. The copy is taken here, the dialog and the write run off the
    /// window's thread.
    pub(super) fn save_replay(&mut self) {
        let Some((rate, samples)) = self.with_call(state::ActiveCall::take_replay_snapshot) else { return };
        if samples.is_empty() {
            self.toast("Nothing heard yet", ToastKind::Info);
            return;
        }
        let events = self.events.clone();
        std::thread::spawn(move || {
            let Some(path) = rfd::FileDialog::new().set_title("Save replay").add_filter("WAV audio", &["wav"]).set_file_name("replay.wav").save_file() else {
                return;
            };
            let seconds = samples.len() as f32 / rate.max(1) as f32;
            let (text, kind) = match wav::write_wav_16bit_mono(&path, rate, &samples) {
                Ok(()) => (format!("Saved {seconds:.0} s to {}", path.display()), ToastKind::Ok),
                Err(e) => (format!("Couldn't save the replay: {e}"), ToastKind::Err),
            };
            Self::push(&events, Event::Toast(text, kind));
        });
    }

    /// Carries out a soundboard request.
    pub(super) fn sound_do(&mut self, what: ui::SoundDo) {
        match what {
            ui::SoundDo::Add => self.add_sound(),
            ui::SoundDo::Remove(i) => {
                if i >= self.cfg.sound_clips.len() {
                    return;
                }
                let removed = self.cfg.sound_clips.remove(i);
                // The copy in the app's own folder is ours to delete; the
                // person's original file was never touched.
                if self.dir.as_deref().is_some_and(|d| Path::new(&removed.path).starts_with(soundboard::clips_dir(d))) {
                    let _ = std::fs::remove_file(&removed.path);
                }
                self.save();
            }
            ui::SoundDo::Gain(i, db) => {
                if let Some(clip) = self.cfg.sound_clips.get_mut(i) {
                    clip.gain_db = db.clamp(-24.0, 12.0);
                    self.save();
                }
            }
            ui::SoundDo::Preview(i) => {
                if let Some((rate, samples)) = self.load_sound(i) {
                    self.play_here(rate, &samples);
                }
            }
            ui::SoundDo::Send(i) => {
                let Some((rate, mut samples)) = self.load_sound(i) else { return };
                let sent = self.with_call(|a| {
                    // Resampled to the *call's* rate, not the clip's: a clip
                    // at the wrong rate plays at the wrong pitch.
                    let call_rate = a.sample_rate.load(Ordering::Relaxed);
                    if call_rate != rate {
                        samples = soundboard::resample(&soundboard::LoadedClip { sample_rate: rate, samples: std::mem::take(&mut samples) }, call_rate);
                    }
                    a.soundboard_queue.lock().unwrap().extend(samples);
                });
                if sent.is_none() {
                    self.toast("Clips go into a call; there is none right now", ToastKind::Info);
                }
            }
        }
    }

    /// Asks for a WAV file, checks it plays, copies it into the app's
    /// folder (so the clip keeps working after the original moves) and
    /// hands it back as [`Event::SoundAdded`], all off the window's thread.
    fn add_sound(&self) {
        let Some(dir) = self.dir.clone() else { return };
        let events = self.events.clone();
        std::thread::spawn(move || {
            let Some(picked) = rfd::FileDialog::new().add_filter("WAV audio", &["wav"]).set_title("Add a soundboard clip").pick_file() else { return };
            let added = soundboard::load_wav(&picked).and_then(|_| {
                let clips = soundboard::clips_dir(&dir);
                std::fs::create_dir_all(&clips).map_err(|e| e.to_string())?;
                let name = picked.file_stem().map_or_else(|| "Clip".to_string(), |s| s.to_string_lossy().into_owned());
                let dest = free_path(&clips, &file_stem(&name));
                std::fs::copy(&picked, &dest).map_err(|e| e.to_string())?;
                Ok(soundboard::SoundClip { name, path: dest.to_string_lossy().into_owned(), gain_db: 0.0 })
            });
            match added {
                Ok(clip) => Self::push(&events, Event::SoundAdded(clip)),
                Err(e) => Self::push(&events, Event::Toast(format!("Couldn't add that: {e}"), ToastKind::Err)),
            }
        });
    }

    /// The soundboard clip at `i`, read and at its level, with its sample
    /// rate; `None` (with a toast) when it cannot be read.
    fn load_sound(&mut self, i: usize) -> Option<(u32, Vec<f32>)> {
        let clip = self.cfg.sound_clips.get(i)?;
        match soundboard::load_wav(Path::new(&clip.path)) {
            Ok(loaded) => {
                let mut samples = loaded.samples;
                soundboard::apply_gain(&mut samples, clip.gain_db);
                Some((loaded.sample_rate, samples))
            }
            Err(e) => {
                self.toast(format!("Couldn't play that: {e}"), ToastKind::Err);
                None
            }
        }
    }

    /// Plays `samples` on this computer's speakers or headphones, on a
    /// stream of its own.
    pub(super) fn play_here(&mut self, rate: u32, samples: &[f32]) {
        match state::play_clip(self.cfg.output_device.as_deref(), rate, samples) {
            Ok(handle) => {
                // Held until the sound finishes: dropping it cuts it off.
                let seconds = samples.len() as f32 / rate.max(1) as f32 + 0.3;
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs_f32(seconds));
                    drop(handle);
                });
            }
            Err(e) => self.toast(format!("Couldn't play that: {e}"), ToastKind::Err),
        }
    }

    /// The call view's replay and soundboard parts.
    pub(super) fn sounds_model(&self, a: &state::ActiveCall, m: &mut ui::Call) {
        m.replay = a.replay_clip_labels();
        m.replay_offer = self.replay_offer && !m.replay.is_empty();
        m.sounds = self.cfg.sound_clips.iter().map(|c| c.name.clone()).collect();
    }

    /// The soundboard as Settings lists it.
    pub(super) fn sound_rows(&self) -> Vec<ui::SoundRow> {
        self.cfg.sound_clips.iter().map(|c| ui::SoundRow { name: c.name.clone(), gain_db: c.gain_db }).collect()
    }
}

/// The saved replay mode (0 off, 1 ask, 2 play at once) as the screens
/// name it.
pub(super) fn replay_mode(saved: i32) -> ui::ReplayMode {
    match saved {
        1 => ui::ReplayMode::Ask,
        2 => ui::ReplayMode::Auto,
        _ => ui::ReplayMode::Off,
    }
}

/// A clip's name made safe as a file name: letters, digits, `-`, `_` and
/// spaces, at most 48 of them; "clip" when nothing is left.
fn file_stem(name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' ')).take(48).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() { "clip".to_string() } else { trimmed.to_string() }
}

/// `<dir>/<stem>.wav`, or `<stem> 2.wav` and so on when that is taken, so
/// adding a second clip with the same name never overwrites the first.
fn free_path(dir: &Path, stem: &str) -> std::path::PathBuf {
    let mut path = dir.join(format!("{stem}.wav"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem} {n}.wav"));
        n += 1;
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_names_become_safe_file_names() {
        assert_eq!(file_stem("Air horn!"), "Air horn");
        assert_eq!(file_stem("../../etc/passwd"), "etcpasswd");
        assert_eq!(file_stem("???"), "clip");
    }

    #[test]
    fn a_second_clip_with_the_same_name_gets_its_own_file() {
        let dir = std::env::temp_dir().join(format!("stoop-sounds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = free_path(&dir, "horn");
        std::fs::write(&first, b"x").unwrap();
        assert_eq!(free_path(&dir, "horn"), dir.join("horn 2.wav"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_saved_modes_are_off() {
        assert_eq!(replay_mode(7), ui::ReplayMode::Off);
        assert_eq!(replay_mode(1), ui::ReplayMode::Ask);
    }
}
