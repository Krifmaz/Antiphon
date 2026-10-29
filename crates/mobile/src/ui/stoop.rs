//! The phone drawn with Stoop's shared interface (`antiphon-ui`), the same
//! screens the desktop shows.
//!
//! The interface draws a plain [`Model`] and hands back [`Action`]s; this
//! module is the phone's half of that bargain. [`App::refresh`] turns the
//! app's state (the call engine, contacts, the open conversation, files,
//! settings) into a model, and [`App::act`] carries each action out, mostly
//! by sending the app the [`Message`] its own screens used to send, so the
//! rules behind every button stay where they were written.
//!
//! A child of `ui` so it can read and change the app's state directly.

use std::collections::HashMap;
use std::sync::Arc;

use antiphon_crypto::Invite;
use antiphon_ui::{self as stoop, Action, Delivery, Line, Model, Person, Presence, ToastKind, Transmit};
use iced_widget::qr_code;

use super::{App, Message};
use crate::call::{now_secs, Phase};
use crate::contacts::hex;
use crate::data_rules;

/// The voice preset name that means "no EQ", as on the desktop.
const NATURAL: &str = "Natural";

/// Progress as a fraction from 0 to 1. Nothing to do reads as finished
/// rather than dividing by zero.
fn fraction(done: u32, total: u32) -> f32 {
    if total == 0 {
        1.0
    } else {
        done as f32 / total as f32
    }
}

impl App {
    /// Rebuilds the model the interface draws. Called after every change;
    /// cheap enough for that, since it reads what is already in memory.
    pub(super) fn refresh(&mut self) {
        let model = self.build_model();
        self.model = model;
        let s = &self.model.settings;
        let look = (s.theme.clone(), s.accent.clone());
        if look != self.look {
            // A phone follows its own dark palette for "system": there is
            // no portable way to ask Android or iOS which one it is in here.
            stoop::theme::set(stoop::theme::palette(&look.0, &look.1, "", true));
            self.look = look;
        }
        self.stoop.set_reduced_motion(self.settings.reduced_motion);
    }

    /// The model, built from scratch.
    fn build_model(&mut self) -> Model {
        let phase = self.engine.phase();
        let live = self.engine.live();
        let live_peer = live.as_ref().map(|l| hex(&l.peer_key));
        let open = self.stoop.open_person().map(str::to_string);

        // The conversation on screen comes from disk; keep the open one in
        // step with what the interface shows.
        if let Some(id) = &open {
            if self.chat.as_ref().is_none_or(|c| &c.peer_hex != id) {
                self.open_chat(id);
            }
            if self.foreground {
                self.unread = 0;
            }
        }
        if let Some(chat) = &self.chat {
            if let Some(last) = chat.messages.iter().rev().find(|m| !m.deleted) {
                let text = if last.from_me { format!("You: {}", last.text) } else { last.text.clone() };
                self.previews.insert(chat.peer_hex.clone(), (text, last.at_secs));
            }
        }

        let identity_key = self.identity.as_ref().ok().map(|i| i.public().identity_key);
        let id = identity_key.map(|k| hex(&k)).unwrap_or_default();
        let (door_open, local_addr, public_addr) = match &phase {
            Phase::Listening { local, public } => (true, Some(local.clone()), public.clone()),
            _ => (false, self.local_ip.as_ref().map(|ip| format!("{ip}:{}", self.settings.listen_port)), None),
        };
        // An invite carries an address only once one is known to work: the
        // public one while listening, or none, so the link still adds the
        // key and the call goes out from their side's contact.
        let invite = identity_key.map(|k| Invite::new(k, public_addr.clone().unwrap_or_default()).to_link()).unwrap_or_default();
        if self.qr.as_ref().is_none_or(|(link, _)| link != &invite) {
            self.qr = qr_code::Data::new(invite.as_bytes()).ok().map(Arc::new).map(|d| (invite.clone(), d));
        }
        let me = stoop::Me {
            name: self.settings.display_name.clone(),
            status: String::new(),
            fingerprint: id.as_bytes().chunks(4).take(8).map(|c| String::from_utf8_lossy(c).to_string()).collect::<Vec<_>>().join(" "),
            id,
            qr: self.qr.as_ref().map(|(_, d)| d.clone()),
            invite,
            door_open,
            local_addr,
            public_addr,
            greeting: "Welcome back".into(),
        };

        let now = now_secs();
        let mut people: Vec<Person> = self
            .contacts
            .list
            .iter()
            .map(|c| Person { fingerprint: c.fingerprint(), ..self.person(&c.identity_key, c.name.clone(), c.address.clone(), c.verified, live_peer.as_deref(), now) })
            .collect();
        // Someone not saved can still call: they get a row for the length
        // of the call, so their conversation and the call have a place.
        if let Some(peer) = &live_peer {
            if self.contacts.by_key(peer).is_none() {
                let name = self.peer_display_name.clone().unwrap_or_else(|| self.contacts.name_for(peer));
                people.insert(0, self.person(peer, name, String::new(), false, live_peer.as_deref(), now));
            }
        }
        // Most recent conversation first, as on the desktop.
        people.sort_by_key(|p| std::cmp::Reverse(self.previews.get(&p.id).map(|(_, at)| *at).unwrap_or(0)));

        let mut call = self.call_model(&phase);
        call.video = self.video_tiles(live.as_deref());
        self.camera_model(live.as_deref(), &mut call);
        let conversation = open.as_deref().and_then(|id| self.conversation_model(id, live_peer.as_deref() == Some(id)));
        let transfers = self.transfers_model();

        let chain = &self.settings.chain;
        let mut presets = vec![NATURAL.to_string()];
        presets.extend(antiphon_audio::eq_preset_names().iter().map(|s| s.to_string()));
        let preset = self.eq_choice.clone().unwrap_or_else(|| NATURAL.to_string());
        let settings = stoop::Settings {
            theme: self.settings.theme.clone(),
            accent: self.settings.accent.clone(),
            transmit: if self.settings.open_mic { Transmit::Open } else { Transmit::PushToTalk },
            noise_suppression: chain.noise_suppress.enabled,
            echo_cancel: false,
            auto_gain: matches!(chain.gain, antiphon_audio::GainConfig::Auto { enabled: true, .. }),
            preset,
            presets,
            mic_test: self.engine.mic_test_running().then(|| self.engine.level()),
            reduced_motion: self.settings.reduced_motion,
            version: env!("CARGO_PKG_VERSION").into(),
            data_dir: self.dir.display().to_string(),
            ..stoop::Settings::default()
        };
        let link = self.link();
        let checks = vec![
            stoop::Check {
                label: "Network".into(),
                state: match link {
                    data_rules::Link::Offline => stoop::CheckState::Bad,
                    data_rules::Link::Metered => stoop::CheckState::Warn,
                    data_rules::Link::Wifi => stoop::CheckState::Good,
                    data_rules::Link::Unknown => stoop::CheckState::Unknown,
                },
                detail: match link {
                    data_rules::Link::Offline => "No connection.".into(),
                    data_rules::Link::Metered => "Mobile data. Files follow your data rules.".into(),
                    data_rules::Link::Wifi => "Wi-Fi.".into(),
                    data_rules::Link::Unknown => "The phone has not said.".into(),
                },
            },
            stoop::Check {
                label: "Your address on this network".into(),
                state: if self.local_ip.is_some() { stoop::CheckState::Good } else { stoop::CheckState::Unknown },
                detail: self.local_ip.clone().map(|ip| format!("{ip}:{}", self.settings.listen_port)).unwrap_or_else(|| "Not found.".into()),
            },
        ];

        Model {
            me,
            people,
            call,
            conversation,
            transfers,
            network: stoop::Network { checks, port: self.settings.listen_port, relay: String::new() },
            settings,
            notice: self.notice.clone(),
            first_run: self.settings.display_name.trim().is_empty(),
            device: stoop::Device { phone: true, files: true, ptt_keys: false },
            phone: Some(stoop::Phone {
                speaker: self.settings.speaker,
                mic_allowed: self.mic_permission,
                wifi_only: self.settings.wifi_only_transfers,
                data_saver: self.settings.data_saver,
                cellular_limit_mb: self.settings.cellular_limit_mb,
                cellular_used: format!("Used this month: {}.", data_rules::size(self.meter.used(now))),
                keep_listening: self.settings.keep_listening_in_background,
                listen_on_saver: self.settings.listen_on_battery_saver,
                phrase: match (&self.identity, self.phrase_visible) {
                    (Ok(identity), true) => Some(antiphon_crypto::mnemonic::phrase_from_seed(&identity.master_seed())),
                    _ => None,
                },
                talking: self.talking,
            }),
        }
    }

    /// One row in the people list.
    fn person(&self, key_hex: &str, name: String, address: String, verified: bool, live_peer: Option<&str>, now: u64) -> Person {
        let on_call = live_peer == Some(key_hex);
        let preview = self.previews.get(key_hex);
        Person {
            id: key_hex.to_string(),
            name,
            address,
            verified,
            presence: if on_call { Presence::Online } else { Presence::Unknown },
            last: preview.map(|(text, _)| text.clone()),
            last_at: preview.map(|(_, at)| data_rules::ago(now, *at)),
            unread: if on_call { self.unread as u32 } else { 0 },
            fingerprint: key_hex.as_bytes().chunks(4).take(8).map(|c| String::from_utf8_lossy(c).to_string()).collect::<Vec<_>>().join(" "),
            note: String::new(),
            ring_rule: String::new(),
            route: "Direct".into(),
            volume_db: 0.0,
            muted: false,
        }
    }

    /// The call as the interface shows it.
    fn call_model(&self, phase: &Phase) -> stoop::Call {
        let talk_hint = Some(if self.settings.open_mic { "Mic is open".to_string() } else { "Hold the pad to talk".to_string() });
        match phase {
            Phase::Dialing { remote } => stoop::Call {
                phase: stoop::Phase::Dialing,
                peer_name: self.contacts.list.iter().find(|c| c.dial_target().as_deref() == Some(remote.as_str())).map(|c| c.name.clone()).unwrap_or_else(|| remote.clone()),
                ..stoop::Call::default()
            },
            Phase::Connected { remote, code, peer_key } => {
                let peer = hex(peer_key);
                stoop::Call {
                    phase: stoop::Phase::Connected,
                    peer_name: self.display_name_for(&peer),
                    peer: Some(peer),
                    code: code.clone(),
                    mode: "Direct".into(),
                    mode_detail: format!("Straight to {remote}, nothing in between."),
                    seconds: self.connected_at.map(|t| t.elapsed().as_secs()).unwrap_or(0),
                    muted: self.muted,
                    my_level: self.engine.level(),
                    sending: self.engine.transmitting(),
                    talk_hint,
                    ..stoop::Call::default()
                }
            }
            Phase::Ended { message } => stoop::Call { phase: stoop::Phase::Ended(message.clone()), ..stoop::Call::default() },
            Phase::Idle | Phase::Listening { .. } => stoop::Call::default(),
        }
    }

    /// The other side's pictures on the call, then our own camera. A
    /// picture that has not changed since the last frame keeps its handle,
    /// so iced keeps the texture it already has.
    fn video_tiles(&mut self, live: Option<&crate::call::Live>) -> Vec<stoop::VideoTile> {
        let Some(live) = live else {
            self.tiles.clear();
            return Vec::new();
        };
        let name = self.display_name_for(&hex(&live.peer_key));
        let first = name.split_whitespace().next().unwrap_or("Their").to_string();
        let mut tiles = Vec::new();
        for (stream, version) in live.video.remote_versions() {
            let screen = stream == antiphon_video::Stream::Screen;
            let key = if screen { "their-screen" } else { "their-camera" };
            if let Some((v, tile)) = self.tiles.get(key) {
                if *v == version {
                    tiles.push(tile.clone());
                    continue;
                }
            }
            let Some(p) = live.video.remote_picture(stream) else { continue };
            let tile = stoop::VideoTile {
                key: key.into(),
                label: if screen { format!("{first}'s screen") } else { first.clone() },
                image: iced_core::image::Handle::from_rgba(p.width, p.height, p.rgba),
                width: p.width,
                height: p.height,
                mine: false,
                screen,
            };
            self.tiles.insert(key.into(), (version, tile.clone()));
            tiles.push(tile);
        }
        tiles.extend(self.my_camera_tile(live));
        self.tiles.retain(|k, _| tiles.iter().any(|t| &t.key == k));
        tiles
    }

    /// The open conversation's lines, from the history on disk.
    fn conversation_model(&self, id: &str, live: bool) -> Option<stoop::Conversation> {
        let chat = self.chat.as_ref().filter(|c| c.peer_hex == id)?;
        let now = now_secs();
        let mut reactions: HashMap<u64, Vec<(String, u32)>> = HashMap::new();
        for r in &chat.reactions {
            let list = reactions.entry(r.message_id).or_default();
            match list.iter_mut().find(|(e, _)| *e == r.emoji) {
                Some((_, n)) => *n += 1,
                None => list.push((r.emoji.clone(), 1)),
            }
        }
        let lines = chat
            .messages
            .iter()
            .filter(|m| !m.deleted)
            .map(|m| Line {
                id: m.id,
                mine: m.from_me,
                author: if m.from_me { self.settings.display_name.clone() } else { String::new() },
                text: m.text.clone(),
                time: data_rules::ago(now, m.at_secs),
                day: None,
                delivery: match (m.from_me, chat.status(m.id, live)) {
                    (false, _) => Delivery::None,
                    (true, crate::outbox::Status::Waiting) => Delivery::Queued,
                    (true, crate::outbox::Status::Sending) => Delivery::Sent,
                    (true, crate::outbox::Status::Delivered) => Delivery::Delivered,
                },
                edited: m.edited,
                pinned: m.pinned,
                reactions: reactions.remove(&m.id).unwrap_or_default(),
                file: None,
            })
            .collect();
        Some(stoop::Conversation {
            person: id.to_string(),
            lines,
            live,
            queues: !live,
            expiry: chat.expire_after_secs().map(|s| format!("Messages vanish after {}", super::Expiry::from_secs(Some(s)))),
        })
    }

    /// The offer waiting, and the files moving either way.
    fn transfers_model(&self) -> stoop::Transfers {
        let mut t = stoop::Transfers::default();
        let live = self.engine.live();
        if let Some(live) = &live {
            let files = live.files.lock().unwrap();
            if let Some(offer) = &files.pending_offer {
                t.offer = Some((offer.file_name.clone(), data_rules::size(offer.file_size)));
            }
            if let Some(incoming) = &files.incoming {
                t.active.push(stoop::Transfer {
                    name: incoming.file_name.clone(),
                    incoming: true,
                    progress: fraction(incoming.chunks_received, incoming.total_chunks),
                    detail: if incoming.verifying { "Checking it arrived intact".into() } else { data_rules::size(incoming.file_size) },
                    paused: false,
                });
            }
        }
        if let Some(active) = &self.files.active {
            let files = live.as_ref().map(|l| l.files.lock().unwrap());
            let (progress, detail) = match files.as_ref().and_then(|f| f.outgoing.as_ref()) {
                _ if active.preparing() => (0.0, "Getting the file ready".to_string()),
                Some(o) if !o.accepted => (0.0, match active.entry.item.size {
                    Some(size) => format!("{}, waiting for them to accept", data_rules::size(size)),
                    None => "Waiting for them to accept".to_string(),
                }),
                Some(o) => {
                    let p = fraction(o.chunks_sent, o.total_chunks);
                    let detail = match active.held {
                        Some(hold) if !active.user_paused => hold.explain().to_string(),
                        _ => format!("{:.0}%", p * 100.0),
                    };
                    (p, detail)
                }
                None => (0.0, "Starting".to_string()),
            };
            t.active.push(stoop::Transfer { name: active.entry.item.name.clone(), incoming: false, progress, detail, paused: active.user_paused });
        }
        t
    }

    /// Carries out one thing the interface asked for.
    pub(super) fn act(&mut self, action: Action) {
        match action {
            Action::Open(id) => self.open_chat(&id),
            Action::Door(true) => self.update(Message::Listen),
            Action::Door(false) => {
                if matches!(self.engine.phase(), Phase::Listening { .. }) {
                    self.engine.stop();
                }
            }
            Action::Call(id) => self.update(Message::CallContact(id)),
            Action::CallAddress(address) => self.start_call(address.trim()),
            Action::Hangup => self.update(Message::Stop),
            Action::Mute(on) => {
                // Muting stops sending whichever way the mic is keyed; the
                // saved open-mic setting is left as it is.
                self.muted = on;
                self.engine.set_ptt(false);
                self.engine.set_open_mic(!on && self.settings.open_mic);
            }
            Action::Deafen(_) | Action::PeerVolume(_) | Action::Volume(..) | Action::MuteFor(..) | Action::OutputVolume(_) => {
                self.stoop.toast("The phone's volume buttons set this for now", ToastKind::Info);
            }
            Action::Send { to, text } => {
                self.open_chat(&to);
                self.draft = text;
                self.update(Message::SendChat);
            }
            Action::Edit { to, id, text } => {
                self.open_chat(&to);
                self.editing = Some(id);
                self.draft = text;
                self.update(Message::SendChat);
            }
            Action::Delete { to, id } => {
                self.open_chat(&to);
                self.update(Message::DeleteChat(id));
            }
            Action::React { to, id, emoji } => {
                self.open_chat(&to);
                // Only the reactions the wire allows; anything else is
                // dropped by the receiver anyway.
                if let Some(e) = crate::reactions::AVAILABLE.iter().find(|e| **e == emoji) {
                    self.update(Message::React(id, e));
                }
            }
            Action::Pin { .. } => self.stoop.toast("Pinning is on the desktop for now", ToastKind::Info),
            Action::SendFile => self.update(Message::PickFile),
            Action::AcceptFile => self.update(Message::AcceptOffer),
            Action::DeclineFile => self.update(Message::DeclineOffer),
            Action::CancelTransfer => {
                if self.files.active.is_some() {
                    self.update(Message::CancelSend);
                } else {
                    self.update(Message::CancelIncoming);
                }
            }
            Action::AddPerson { name, invite } => match self.contacts.add_from_invite(&name, &invite, now_secs()) {
                Ok(_) => {
                    self.contacts.save(&self.dir);
                    if let Some(id) = self.contacts.list.last().map(|c| c.identity_key.clone()) {
                        for a in self.stoop.added(id) {
                            self.act(a);
                        }
                    }
                }
                Err(e) => self.stoop.add_failed(e),
            },
            Action::Remove(id) => {
                self.contacts.remove(&id);
                self.contacts.save(&self.dir);
            }
            Action::Block(_) => self.stoop.toast("Blocking is on the desktop for now", ToastKind::Info),
            Action::Verify(id, on) => self.update(Message::MarkVerified(id, on)),
            Action::Rename(id, name) => {
                if let Some(c) = self.contacts.by_key_mut(&id) {
                    c.name = name;
                    self.contacts.save(&self.dir);
                }
            }
            Action::Note(..) | Action::SetStatus(_) | Action::Relay(_) | Action::Input(_) | Action::Output(_) | Action::BindPtt => {}
            Action::NotifyMessages(_) | Action::OpenOnStart(_) | Action::RefreshInvite | Action::OpenDownloads => {}
            Action::Copy(text) => crate::platform::copy_to_clipboard(&text),
            Action::SetName(name) | Action::Welcome(name) => self.update(Message::DisplayName(name)),
            Action::Theme(theme) => {
                self.settings.theme = theme;
                self.save();
            }
            Action::Accent(accent) => {
                self.settings.accent = accent;
                self.save();
            }
            Action::ReducedMotion(on) => {
                self.settings.reduced_motion = on;
                self.save();
            }
            Action::Transmit(mode) => self.update(Message::OpenMic(mode != Transmit::PushToTalk)),
            Action::NoiseSuppression(on) => self.update(Message::Toggle(super::Stage::NoiseSuppress, on)),
            Action::AutoGain(on) => self.update(Message::Toggle(super::Stage::AutoGain, on)),
            Action::EchoCancel(_) => {}
            Action::Preset(name) if name == NATURAL => self.update(Message::ResetChain),
            Action::Preset(name) => self.update(Message::EqPreset(name)),
            Action::MicTest(on) => self.update(Message::MicTest(on)),
            Action::Port(port) => self.update(Message::PortChanged(port.to_string())),
            Action::CheckNetwork => {
                self.local_ip = crate::call::local_ip();
                self.network = crate::platform::network();
            }
            Action::Speaker(on) => self.update(Message::Speaker(on)),
            Action::Camera(on) => self.set_camera(on),
            Action::FlipCamera => self.flip_camera(),
            // Sharing a phone's screen needs the system's screen recording
            // (MediaProjection, ReplayKit), so its button is not offered
            // (`can_share` stays off).
            Action::ShareScreen(_) => self.stoop.toast("Sharing a phone's screen is coming; you can already see theirs", ToastKind::Info),
            Action::Talk(on) => {
                self.talking = on && !self.muted;
                self.update(if self.talking { Message::PttPressed } else { Message::PttReleased });
            }
            Action::RequestMic => self.update(Message::RequestMic),
            Action::WifiOnly(on) => self.update(Message::WifiOnly(on)),
            Action::DataSaver(on) => self.update(Message::DataSaver(on)),
            Action::CellularLimit(mb) => self.update(Message::CellularLimit(mb.to_string())),
            Action::KeepListening(on) => self.update(Message::KeepListening(on)),
            Action::ListenOnSaver(on) => self.update(Message::ListenOnSaver(on)),
            Action::ShowPhrase(on) => self.update(Message::ShowPhrase(on)),
            Action::Restore(phrase) => {
                // The interface already asked "are you sure", so this is
                // the second press the restore rule wants.
                self.restore_text = phrase;
                self.restore_armed = true;
                self.update(Message::Restore);
            }
        }
    }
}
