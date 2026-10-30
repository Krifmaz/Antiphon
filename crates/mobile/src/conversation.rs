//! One conversation: its messages, reactions and anything still waiting
//! to be sent.
//!
//! The messages are the desktop's history file, written through its own
//! `history` module, and what goes over the wire is its `ControlMessage`,
//! so a conversation started on a phone reads the same on a desktop and
//! the other way round. Messages typed while the other person is not on a
//! call wait in the desktop's `outbox` format and go out when they next
//! connect, as the desktop's do.

use std::path::{Path, PathBuf};

use antiphon_crypto::BackupKey;

use crate::call::{now_secs, Live};
use crate::control::ControlMessage;
use crate::history::{self, HistorySink, StoredMessage};
use crate::outbox::{self, Queued};
use crate::reactions::{self, Reaction};

/// One open conversation: its history loaded into memory, plus the
/// sink that writes every change back to the sealed files.
///
/// The interface keeps one open for the chat on screen, and opens one
/// briefly to file a message from anyone else. Everything is on the
/// interface thread; nothing here is shared between threads.
pub struct Conversation {
    /// Whose conversation this is: their identity key, hex.
    pub peer_hex: String,
    /// The history as last loaded, oldest first. Reloaded from disk after
    /// every change, so it always matches what the sink wrote.
    pub messages: Vec<StoredMessage>,
    /// Every reaction on every message, ours and theirs.
    pub reactions: Vec<Reaction>,
    /// Our messages still waiting to be sent, oldest first.
    pub outbox: Vec<Queued>,
    /// Writes the sealed history file, and holds the directory, peer and
    /// key the reaction and outbox files are written with too.
    sink: HistorySink,
    /// The id our next message gets: one past the largest id seen in the
    /// history or the outbox, ours or theirs.
    next_id: u64,
}

/// Reactions live next to the history file, sealed the same way. The
/// desktop keeps them only for the length of a call; keeping them here
/// costs one small file and means a thumbs-up does not vanish when the
/// phone hangs up. The desktop never reads this file, so it changes
/// nothing about how the two talk.
fn reactions_path(dir: &Path, peer_hex: &str) -> PathBuf {
    let mut path = history::history_path(dir, peer_hex);
    path.set_extension("react");
    path
}

impl Conversation {
    /// Loads the history, outbox and reactions for `peer_hex` from `dir`,
    /// sealed with `key`. Unreadable files read as empty rather than
    /// failing, as the desktop's do. `expire_after_secs` is the contact's
    /// disappearing-message timer, applied to messages written from now on.
    pub fn open(dir: &Path, key: &BackupKey, peer_hex: &str, expire_after_secs: Option<u64>) -> Self {
        let sink = HistorySink { dir: dir.to_path_buf(), peer_hex: peer_hex.to_string(), key: key.clone(), expire_after_secs };
        let messages = sink.load_all();
        let outbox = outbox::load(dir, peer_hex, key);
        let reactions = std::fs::read(reactions_path(dir, peer_hex))
            .ok()
            .and_then(|sealed| key.open_content(&sealed).ok())
            .and_then(|plain| serde_json::from_slice(&plain).ok())
            .unwrap_or_default();
        // Past ids are never reused, ours or theirs, as on the desktop: an
        // edit naming a recycled id would rewrite an older message instead.
        let next_id = messages.iter().map(|m| m.id).chain(outbox.iter().map(|q| q.id)).max().map_or(0, |m| m + 1);
        Self { peer_hex: peer_hex.to_string(), messages, reactions, outbox, sink, next_id }
    }

    /// The disappearing-message timer new messages are written with, in
    /// seconds, or `None` to keep them.
    pub fn expire_after_secs(&self) -> Option<u64> {
        self.sink.expire_after_secs
    }

    /// Applies to messages sent or received from now on; ones already
    /// written keep the expiry they were written with, as on the desktop.
    pub fn set_expiry(&mut self, secs: Option<u64>) {
        self.sink.expire_after_secs = secs;
    }

    /// Rereads the history from disk, which also drops expired messages.
    fn reload(&mut self) {
        self.messages = self.sink.load_all();
    }

    /// Seals and writes the reactions file. A failure is logged and
    /// otherwise ignored, as for the call log.
    fn save_reactions(&self) {
        let result = serde_json::to_vec(&self.reactions)
            .map_err(|e| e.to_string())
            .and_then(|plain| self.sink.key.seal_content(&plain).map_err(|e| e.to_string()))
            .and_then(|sealed| {
                let path = reactions_path(&self.sink.dir, &self.peer_hex);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::write(path, sealed).map_err(|e| e.to_string())
            });
        if let Err(e) = result {
            log::warn!("could not save reactions: {e}");
        }
    }

    /// Seals and writes the outbox. A failure is logged and otherwise
    /// ignored; the queue stays in memory until the next save.
    fn save_outbox(&self) {
        if let Err(e) = outbox::save(&self.sink.dir, &self.peer_hex, &self.sink.key, &self.outbox) {
            log::warn!("could not save the outbox: {e}");
        }
    }

    /// Sends a message now if they are on the call, or queues it for when
    /// they next are.
    ///
    /// `reply_to` is the message it answers, by id, when it was written as
    /// a reply; it is kept with it here and goes with it to them.
    pub fn send(&mut self, text: &str, reply_to: Option<u64>, live: Option<&Live>) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        // Only a message that is still in this conversation can be
        // answered; anything else is sent as a plain message.
        let reply_to = reply_to.filter(|r| self.messages.iter().any(|m| m.id == *r));
        let id = self.next_id;
        self.next_id += 1;
        self.sink.record_reply(id, true, text, reply_to);
        match live {
            Some(live) => live.send(ControlMessage::Chat { id, text: text.to_string(), reply_to }),
            None => {
                outbox::enqueue(&mut self.outbox, Queued { id, text: text.to_string(), queued_at_secs: now_secs(), attempts: 0, reply_to });
                self.save_outbox();
            }
        }
        self.reload();
    }

    /// Sends everything that waited for this call.
    pub fn flush_outbox(&mut self, live: &Live) {
        if self.outbox.is_empty() {
            return;
        }
        for queued in std::mem::take(&mut self.outbox) {
            live.send(ControlMessage::Chat { id: queued.id, text: queued.text, reply_to: queued.reply_to });
        }
        self.save_outbox();
    }

    /// Whether one of our messages can still be changed: always during a
    /// call with them, and outside one only while it has not left the
    /// phone -- an edit they will never hear about would make the two
    /// sides' records quietly disagree.
    pub fn can_change(&self, id: u64, live: bool) -> bool {
        let ours = self.messages.iter().any(|m| m.id == id && m.from_me && !m.deleted);
        ours && (live || self.outbox.iter().any(|q| q.id == id))
    }

    /// Changes one of our messages. Returns `false`, changing nothing, for
    /// empty text or a message that [`can_change`](Self::can_change) refuses.
    pub fn edit(&mut self, id: u64, text: &str, live: Option<&Live>) -> bool {
        let text = text.trim();
        if text.is_empty() || !self.can_change(id, live.is_some()) || !self.sink.edit(id, true, text) {
            return false;
        }
        if let Some(queued) = self.outbox.iter_mut().find(|q| q.id == id) {
            // Not sent yet, so the edit simply becomes what will be sent.
            queued.text = text.to_string();
            self.save_outbox();
        } else if let Some(live) = live {
            live.send(ControlMessage::ChatEdit { id, text: text.to_string() });
        }
        self.reload();
        true
    }

    /// Deletes one of our messages, telling them if they are on the call,
    /// and drops its reactions. Returns `false`, changing nothing, when
    /// [`can_change`](Self::can_change) refuses.
    pub fn delete(&mut self, id: u64, live: Option<&Live>) -> bool {
        if !self.can_change(id, live.is_some()) || !self.sink.delete(id, true) {
            return false;
        }
        if outbox::mark_delivered(&mut self.outbox, id) {
            // Withdrawn before it was ever sent; they never see it.
            self.save_outbox();
        } else if let Some(live) = live {
            live.send(ControlMessage::ChatDelete { id });
        }
        if reactions::forget_message(&mut self.reactions, id) > 0 {
            self.save_reactions();
        }
        self.reload();
        true
    }

    /// Toggles our reaction. Only during a call: the reaction message has
    /// no queue of its own, and one the other side never sees is a
    /// reaction to nobody.
    pub fn react(&mut self, id: u64, emoji: &str, live: &Live) {
        if reactions::toggle(&mut self.reactions, id, emoji, true) {
            self.save_reactions();
            live.send(ControlMessage::ChatReact { message_id: id, emoji: emoji.to_string() });
        }
    }

    /// Applies a chat message from them. Returns the text of a new message,
    /// for a notification.
    ///
    /// An edit or delete may only touch a message *they* sent: that is the
    /// `from_me: false` in each call, as on the desktop, and without it a
    /// peer could rewrite our own words in our own history.
    pub fn receive(&mut self, message: ControlMessage) -> Option<String> {
        let mut new_text = None;
        match message {
            ControlMessage::Chat { id, text, reply_to } => {
                // A reply to something not in this conversation (never
                // here, or long since gone) is kept as a plain message.
                let reply_to = reply_to.filter(|r| self.messages.iter().any(|m| m.id == *r));
                self.sink.record_reply(id, false, &text, reply_to);
                self.next_id = self.next_id.max(id + 1);
                new_text = Some(text);
            }
            ControlMessage::ChatEdit { id, text } => {
                self.sink.edit(id, false, &text);
            }
            ControlMessage::ChatDelete { id } => {
                if self.sink.delete(id, false) && reactions::forget_message(&mut self.reactions, id) > 0 {
                    self.save_reactions();
                }
            }
            ControlMessage::ChatReact { message_id, emoji } => {
                // `toggle` refuses anything outside the fixed set, so a
                // peer cannot turn a reaction row into a message of its own.
                if reactions::toggle(&mut self.reactions, message_id, &emoji, false) {
                    self.save_reactions();
                }
            }
            _ => return None,
        }
        self.reload();
        new_text
    }

    /// Drops messages whose disappearing timer ran out. Returns whether any
    /// did, so the screen can redraw.
    pub fn prune_expired(&mut self) -> bool {
        let now = now_secs();
        if !self.messages.iter().any(|m| m.is_expired(now)) {
            return false;
        }
        // Loading prunes; saving makes it stick on disk too.
        self.reload();
        if let Err(e) = history::save(&self.sink.dir, &self.peer_hex, &self.sink.key, &self.messages) {
            log::warn!("could not save history after expiry: {e}");
        }
        true
    }

    /// How far one of our messages has got: sent, sending, or waiting
    /// for them to come online.
    pub fn status(&self, id: u64, live: bool) -> outbox::Status {
        outbox::status_of(&self.outbox, id, live)
    }
}
