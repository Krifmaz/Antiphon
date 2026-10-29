//! Made-up people and a call in progress, for `--demo` and the screenshot
//! tour: the whole interface with nothing on the network.

use antiphon_ui::{self as ui, Action, CheckState, Delivery, Line, Person, Phase, Presence};
use iced::widget::qr_code;

/// Robin's made-up id: the contact the demo call is with.
const ROBIN: &str = "7f3a9c21d4e8b6f0a1c2e3d4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4";

/// The demo's whole state: just a model the actions edit.
pub struct Demo {
    /// Everything the interface draws.
    pub model: ui::Model,
    /// The next message id to hand out.
    next_id: u64,
}

impl Demo {
    /// The demo's starting state: five people, no call, the door open.
    pub fn new() -> Self {
        let invite = "antiphon://4be1c07d9f2a83e6b5d4c3f2a1908e7d6c5b4a39281706f5e4d3c2b1a0f9e8d7@203.0.113.24:6002".to_string();
        let people = vec![
            person(ROBIN, "Robin Okafor", Presence::Online, true, "See you on the porch at 8?", "14:02", 0),
            person("a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9", "Theo Park", Presence::Online, false, "sent the photos from saturday", "13:40", 2),
            person("c0ffee00112233445566778899aabbccddeeff00112233445566778899aabbcc", "Mara Lindqvist", Presence::Away, true, "You: talk tomorrow!", "Tue", 0),
            person("deadbeef00112233445566778899aabbccddeeff00112233445566778899aabb", "June Alvarez", Presence::Busy, false, "haha yes", "Mon", 0),
            person("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", "Sam Whitfield", Presence::Unknown, false, "You: here's my invite", "12 Oct", 0),
        ];
        let model = ui::Model {
            me: ui::Me {
                name: "Azule".into(),
                status: "Around this evening".into(),
                id: "4be1c07d9f2a83e6b5d4c3f2a1908e7d6c5b4a39281706f5e4d3c2b1a0f9e8d7".into(),
                fingerprint: "4b:e1:c0:7d:9f:2a:83:e6".into(),
                qr: qr_code::Data::new(invite.as_bytes()).ok().map(std::sync::Arc::new),
                invite,
                door_open: true,
                local_addr: Some("192.168.1.20:6002".into()),
                public_addr: Some("203.0.113.24".into()),
                greeting: "Good evening".into(),
            },
            people,
            call: ui::Call::default(),
            conversation: None,
            transfers: ui::Transfers::default(),
            network: ui::Network {
                checks: vec![
                    check("Calling out", CheckState::Good, "Connections you start are what every home router already allows."),
                    check("Your public address", CheckState::Good, "This is the address to give someone so they can reach you."),
                    check("Being called", CheckState::Good, "Your router is forwarding a port inward and renewing it while the app is open."),
                ],
                port: 6002,
                relay: String::new(),
            },
            settings: ui::Settings {
                theme: "stoop".into(),
                accent: "logo".into(),
                inputs: vec!["Blue Yeti".into(), "MacBook Pro Microphone".into()],
                outputs: vec!["AirPods Pro".into(), "MacBook Pro Speakers".into()],
                input: "Blue Yeti".into(),
                output: String::new(),
                transmit: ui::Transmit::Voice,
                ptt_key: "Caps Lock".into(),
                noise_suppression: true,
                echo_cancel: true,
                auto_gain: true,
                preset: "Natural".into(),
                presets: ["Natural", "Warm", "Clear", "Broadcast", "Thin Mic Rescue"].map(String::from).to_vec(),
                output_db: 0.0,
                notify_messages: true,
                open_on_start: true,
                version: env!("CARGO_PKG_VERSION").into(),
                data_dir: "~/.config/antiphon".into(),
                ..ui::Settings::default()
            },
            notice: None,
            first_run: false,
            device: ui::Device { phone: false, files: true, ptt_keys: true },
            phone: None,
        };
        Demo { model, next_id: 100 }
    }

    /// Puts a call with Robin on, as if it had been going a while.
    pub fn start_call(&mut self) {
        self.model.call = ui::Call {
            phase: Phase::Connected,
            peer: Some(ROBIN.into()),
            peer_name: "Robin Okafor".into(),
            code: "amber · lantern · orbit · tulip".into(),
            mode: "Direct".into(),
            mode_detail: "Straight to them, nothing in between.".into(),
            seconds: 754,
            my_level: 0.2,
            sending: false,
            their_level: 0.75,
            quality: Some("Clear".into()),
            talk_hint: Some("Speak to talk".into()),
            ..ui::Call::default()
        };
    }

    /// Makes the demo a phone (push to talk, the loudspeaker, battery and
    /// data settings), or a desktop again.
    pub fn as_phone(&mut self, on: bool) {
        self.model.device.phone = on;
        self.model.settings.transmit = if on { ui::Transmit::PushToTalk } else { ui::Transmit::Voice };
        self.model.phone = on.then(|| ui::Phone {
            speaker: true,
            mic_allowed: true,
            wifi_only: true,
            data_saver: true,
            cellular_limit_mb: 500,
            cellular_used: "Used this month: 38 MB.".into(),
            keep_listening: true,
            listen_on_saver: false,
            phrase: None,
            talking: false,
        });
    }

    /// Robin's id, for the tour to open their conversation.
    pub fn robin() -> String {
        ROBIN.into()
    }

    /// The conversation for whoever is open.
    pub fn open(&mut self, id: &str) {
        if let Some(p) = self.model.people.iter_mut().find(|p| p.id == id) {
            p.unread = 0;
        }
        let lines = if id == ROBIN {
            vec![
                line(1, false, "are you around this weekend? the porch is finally fixed", "19:12", Some("Yesterday")),
                line(2, true, "yes!! saturday works", "19:15", None),
                line(3, true, "should I bring anything", "19:15", None),
                Line { reactions: vec![("👍".into(), 1)], ..line(4, false, "just yourself. and maybe that lemon thing", "19:20", None) },
                line(5, true, "deal 🍋", "19:21", None),
                Line { pinned: true, ..line(6, false, "Address is 14 Larkspur Lane, ring twice", "09:03", Some("Today")) },
                line(7, true, "got it. calling you in a sec", "13:58", None),
                Line { delivery: Delivery::Delivered, ..line(8, false, "See you on the porch at 8?", "14:02", None) },
            ]
        } else {
            vec![line(1, false, "hey! got your invite", "13:30", Some("Today")), line(2, true, "welcome to my stoop", "13:31", None)]
        };
        self.model.conversation = Some(ui::Conversation { person: id.into(), lines, live: self.model.call.with(id), queues: !self.model.call.with(id), expiry: None });
    }

    /// What an action does to made-up data: enough to click around.
    pub fn act(&mut self, action: Action) {
        match action {
            Action::Open(id) => self.open(&id),
            Action::Door(on) => self.model.me.door_open = on,
            Action::Call(id) => {
                self.start_call();
                self.model.call.peer = Some(id.clone());
                if let Some(p) = self.model.people.iter().find(|p| p.id == id) {
                    self.model.call.peer_name = p.name.clone();
                }
                self.model.call.seconds = 0;
            }
            Action::Volume(id, db) => {
                if let Some(p) = self.model.people.iter_mut().find(|p| p.id == id) {
                    p.volume_db = db;
                }
                if self.model.call.peer.as_deref() == Some(id.as_str()) {
                    self.model.call.peer_volume_db = db;
                }
            }
            Action::MuteFor(id, muted) => {
                if let Some(p) = self.model.people.iter_mut().find(|p| p.id == id) {
                    p.muted = muted;
                }
            }
            Action::Hangup => self.model.call = ui::Call::default(),
            Action::Mute(on) => self.model.call.muted = on,
            Action::Deafen(on) => self.model.call.deafened = on,
            Action::PeerVolume(db) => self.model.call.peer_volume_db = db,
            Action::Send { to, text } => {
                self.next_id += 1;
                if let Some(c) = self.model.conversation.as_mut().filter(|c| c.person == to) {
                    let delivery = if c.live { Delivery::Delivered } else { Delivery::Queued };
                    c.lines.push(Line { delivery, ..line(self.next_id, true, &text, "now", None) });
                }
            }
            Action::Verify(id, on) => {
                if let Some(p) = self.model.people.iter_mut().find(|p| p.id == id) {
                    p.verified = on;
                }
            }
            Action::Theme(t) => self.model.settings.theme = t,
            Action::Accent(a) => self.model.settings.accent = a,
            Action::Transmit(t) => self.model.settings.transmit = t,
            Action::NoiseSuppression(on) => self.model.settings.noise_suppression = on,
            Action::EchoCancel(on) => self.model.settings.echo_cancel = on,
            Action::AutoGain(on) => self.model.settings.auto_gain = on,
            Action::ReducedMotion(on) => self.model.settings.reduced_motion = on,
            Action::SetName(n) => self.model.me.name = n,
            Action::SetStatus(s) => self.model.me.status = s,
            Action::Preset(p) => self.model.settings.preset = p,
            Action::Talk(on) => {
                if let Some(ph) = self.model.phone.as_mut() {
                    ph.talking = on;
                }
            }
            Action::Speaker(on) => {
                if let Some(ph) = self.model.phone.as_mut() {
                    ph.speaker = on;
                }
            }
            _ => {}
        }
    }

    /// Moves the meters so the call looks alive.
    pub fn tick(&mut self, t: f32) {
        if self.model.call.phase == Phase::Connected {
            self.model.call.their_level = (0.5 + 0.45 * (t * 5.3).sin() * (t * 1.7).cos()).clamp(0.0, 1.0);
        }
    }
}

/// A made-up contact.
fn person(id: &str, name: &str, presence: Presence, verified: bool, last: &str, at: &str, unread: u32) -> Person {
    Person {
        id: id.into(),
        name: name.into(),
        address: "203.0.113.50:6002".into(),
        verified,
        presence,
        last: Some(last.into()),
        last_at: Some(at.into()),
        unread,
        fingerprint: id.as_bytes().chunks(2).take(8).map(|c| String::from_utf8_lossy(c).to_string()).collect::<Vec<_>>().join(":"),
        note: if id == ROBIN { "Neighbour from the old flat. Plays bass.".into() } else { String::new() },
        ring_rule: "Rings out loud".into(),
        route: "Automatic".into(),
        volume_db: 0.0,
        muted: false,
    }
}

/// A made-up message; `day` starts a new day above it.
fn line(id: u64, mine: bool, text: &str, time: &str, day: Option<&str>) -> Line {
    Line {
        id,
        mine,
        author: String::new(),
        text: text.into(),
        time: time.into(),
        day: day.map(String::from),
        delivery: if mine { Delivery::Delivered } else { Delivery::None },
        ..Line::default()
    }
}

/// A made-up reachability check.
fn check(label: &str, state: CheckState, detail: &str) -> ui::Check {
    ui::Check { label: label.into(), state, detail: detail.into() }
}
