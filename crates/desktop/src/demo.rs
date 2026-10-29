//! Made-up people and a call in progress, for `--demo` and the screenshot
//! tour: the whole interface with nothing on the network.

use antiphon_ui::{self as ui, Action, CheckState, Delivery, Line, Person, Phase, Presence};
use iced::widget::qr_code;

/// Robin's made-up id: the contact the demo call is with.
const ROBIN: &str = "7f3a9c21d4e8b6f0a1c2e3d4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4";
/// The made-up community's id.
const CLUB: &str = "c1ab0000000000000000000000000000000000000000000000000000000000c1";
/// My made-up id, as in the model.
const ME: &str = "4be1c07d9f2a83e6b5d4c3f2a1908e7d6c5b4a39281706f5e4d3c2b1a0f9e8d7";

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
                bio: "Synths, long walks, terrible puns.".into(),
                color: "sky".into(),
                has_picture: false,
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
            transfers: ui::Transfers {
                active: vec![ui::Transfer { name: "porch-photos.zip".into(), incoming: false, progress: 0.42, detail: "18 MB of 43 MB".into(), paused: false }],
                queued: vec![
                    ui::QueuedFile { id: 1, name: "kind-of-blue-liner-notes.pdf".into(), detail: "2.4 MB".into(), held: false },
                    ui::QueuedFile { id: 2, name: "setlist.txt".into(), detail: "3 KB".into(), held: true },
                ],
                ..ui::Transfers::default()
            },
            network: ui::Network {
                checks: vec![
                    check("Calling out", CheckState::Good, "Connections you start are what every home router already allows."),
                    check("Your public address", CheckState::Good, "This is the address to give someone so they can reach you."),
                    check("Being called", CheckState::Good, "Your router is forwarding a port inward and renewing it while the app is open."),
                ],
                port: 6002,
                relay: String::new(),
                ..Default::default()
            },
            settings: ui::Settings {
                theme: "stoop".into(),
                accent: "logo".into(),
                font: "inter".into(),
                inputs: vec!["Blue Yeti".into(), "MacBook Pro Microphone".into()],
                outputs: vec!["AirPods Pro".into(), "MacBook Pro Speakers".into()],
                input: "Blue Yeti".into(),
                output: String::new(),
                transmit: ui::Transmit::Voice,
                ptt_key: "Caps Lock".into(),
                noise_suppression: true,
                echo_cancel: true,
                low_data: false,
                screen_audio: true,
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
            device: ui::Device { phone: false, files: true, ptt_keys: true, communities: true, voice_messages: true },
            phone: None,
            account: ui::Account { can_pick_file: true, ..Default::default() },
            communities: communities(),
            space_invites: vec![ui::SpaceInvite { name: "Bass players".into(), detail: "Community · from June Alvarez · Mon".into() }],
            channel: None,
            directory: ui::Directory::default(),
        };
        // Chosen colours, as the engine registers saved ones.
        ui::avatars::set(&model.me.id, "", &model.me.color, |_| None);
        ui::avatars::set(ROBIN, "", "sage", |_| None);
        // The club's emoji: small made-up pictures.
        for (name, color) in [("vinyl", [30, 30, 34, 255]), ("porch", [214, 160, 92, 255]), ("yes", [120, 200, 130, 255])] {
            ui::avatars::set(&ui::avatars::emoji_id(CLUB, name), name, "", |_| Some(emoji_picture(color)));
        }
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
            can_camera: true,
            can_share: true,
            ..ui::Call::default()
        };
    }

    /// Turns video on in the demo call: Robin sharing a screen and on
    /// camera, and my own camera. Robin's pictures go through the real
    /// encoder and decoder, so the tour shows what the codec delivers.
    pub fn video_on(&mut self) {
        self.model.call.camera_on = true;
        self.model.call.video = vec![
            through_codec(antiphon_video::Encoder::screen(), &fake_screen(1600, 900), "their-screen", "Robin's screen"),
            through_codec(antiphon_video::Encoder::camera(), &fake_face(640, 480, [214, 160, 120], [58, 92, 140], [236, 222, 200]), "their-camera", "Robin"),
            mine("my-camera", "You", &fake_face(480, 360, [190, 140, 105], [140, 58, 64], [200, 214, 228])),
        ];
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
            talking: false,
        });
    }

    /// The made-up community's id, for the tour to open it.
    pub fn club() -> String {
        CLUB.into()
    }

    /// A channel's made-up messages: #general of the club has some.
    fn open_channel(&mut self, community: &str, channel: u32) {
        let post = |id: u64, who: (&str, &str), text: &str, time: &str, day: Option<&str>| Line {
            mine: who.0 == ME,
            author: who.1.into(),
            author_id: who.0.into(),
            ..line(id, false, text, time, day)
        };
        let mara = ("c0ffee00112233445566778899aabbccddeeff00112233445566778899aabbcc", "Mara Lindqvist");
        let theo = ("a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9", "Theo Park");
        let lines = if community == CLUB && channel == 0 {
            vec![
                post(1, (ROBIN, "Robin Okafor"), "welcome everyone! this is the place for the thursday listening nights", "18:02", Some("Yesterday")),
                post(2, mara, "so glad this exists", "18:05", None),
                post(3, mara, "who's picking the record this week?", "18:05", None),
                post(4, (ME, "Azule"), "I can bring Kind of Blue, the old pressing", "18:11", None),
                Line { edited: true, ..post(5, theo, "yes please. I'll do snacks", "18:14", None) },
                post(6, (ROBIN, "Robin Okafor"), "porch opens at 8. bring a chair if you have one :porch:", "09:30", Some("Today")),
                post(8, mara, ":vinyl: :yes:", "09:33", None),
                Line { deleted: true, ..post(7, theo, "", "09:41", None) },
            ]
        } else {
            Vec::new()
        };
        if let Some(c) = self.model.communities.iter_mut().find(|c| c.id == community) {
            c.unread = 0;
        }
        self.model.channel = Some(ui::ChannelView { community: community.into(), channel, lines });
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
                Line { voice: Some(ui::VoiceClip { seconds: 14, ..Default::default() }), ..line(9, false, "Voice message (0:14)", "09:05", None) },
                line(7, true, "got it. calling you in a sec", "13:58", None),
                Line { delivery: Delivery::Delivered, ..line(8, false, "See you on the porch at 8?", "14:02", None) },
            ]
        } else {
            vec![line(1, false, "hey! got your invite", "13:30", Some("Today")), line(2, true, "welcome to my stoop", "13:31", None)]
        };
        self.model.conversation = Some(ui::Conversation { person: id.into(), lines, live: self.model.call.with(id), queues: !self.model.call.with(id), expiry: None, recording: None });
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
            Action::OpenChannel { community, channel } => self.open_channel(&community, channel),
            Action::PlayVoice { id, .. } => {
                if let Some(c) = self.model.conversation.as_mut() {
                    for l in c.lines.iter_mut() {
                        if let Some(v) = l.voice.as_mut() {
                            v.playing = l.id == id && !v.playing;
                        }
                    }
                }
            }
            Action::RecordVoice(_) => {
                if let Some(c) = self.model.conversation.as_mut() {
                    c.recording = Some(7);
                }
            }
            Action::FinishVoice { .. } => {
                if let Some(c) = self.model.conversation.as_mut() {
                    c.recording = None;
                }
            }
            Action::SpaceNotify { community, channel, level } => {
                if let Some(c) = self.model.communities.iter_mut().find(|c| c.id == community) {
                    match channel {
                        None => c.notify = level.unwrap_or_default(),
                        Some(ch) => {
                            if let Some(x) = c.channels.iter_mut().find(|x| x.id == ch) {
                                x.notify = level;
                            }
                        }
                    }
                }
            }
            Action::RemoveEmoji { community, name } => {
                if let Some(c) = self.model.communities.iter_mut().find(|c| c.id == community) {
                    c.emoji.retain(|e| *e != name);
                }
            }
            Action::QueueMove { id, up } => {
                let q = &mut self.model.transfers.queued;
                if let Some(at) = q.iter().position(|f| f.id == id) {
                    let to = if up { at.checked_sub(1) } else { (at + 1 < q.len()).then_some(at + 1) };
                    if let Some(to) = to {
                        q.swap(at, to);
                    }
                }
            }
            Action::QueueHold { id, held } => {
                if let Some(f) = self.model.transfers.queued.iter_mut().find(|f| f.id == id) {
                    f.held = held;
                }
            }
            Action::QueueRemove(id) => self.model.transfers.queued.retain(|f| f.id != id),
            Action::Post { community, channel, text } => {
                self.next_id += 1;
                if let Some(v) = self.model.channel.as_mut().filter(|v| v.community == community && v.channel == channel) {
                    v.lines.push(Line { author: "Azule".into(), author_id: ME.into(), ..line(self.next_id, true, &text, "now", None) });
                }
            }
            Action::Verify(id, on) => {
                if let Some(p) = self.model.people.iter_mut().find(|p| p.id == id) {
                    p.verified = on;
                }
            }
            Action::Theme(t) => self.model.settings.theme = t,
            Action::Accent(a) => self.model.settings.accent = a,
            Action::CustomAccent(hex) => {
                self.model.settings.accent = "custom".into();
                self.model.settings.custom_accent = hex;
            }
            Action::Font(f) => self.model.settings.font = f,
            Action::Transmit(t) => self.model.settings.transmit = t,
            Action::NoiseSuppression(on) => self.model.settings.noise_suppression = on,
            Action::EchoCancel(on) => self.model.settings.echo_cancel = on,
            Action::AutoGain(on) => self.model.settings.auto_gain = on,
            Action::LowData(on) => self.model.settings.low_data = on,
            Action::ScreenAudio(on) => self.model.settings.screen_audio = on,
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
            Action::Camera(on) => {
                self.model.call.camera_on = on;
                self.model.call.video.retain(|t| t.key != "my-camera");
                if on {
                    self.model.call.video.push(mine("my-camera", "You", &fake_face(480, 360, [190, 140, 105], [140, 58, 64], [200, 214, 228])));
                }
            }
            Action::SetBio(bio) => self.model.me.bio = bio,
            Action::ProfileColor(color) => {
                ui::avatars::set(&self.model.me.id, "", &color, |_| None);
                self.model.me.color = color;
            }
            Action::ShareScreen(on) => {
                self.model.call.sharing = on;
                self.model.call.video.retain(|t| t.key != "my-screen");
                if on {
                    self.model.call.video.push(mine("my-screen", "Your screen", &fake_screen(800, 450)));
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
        bio: if id == ROBIN { "Bass, coffee, records. Usually around after eight.".into() } else { String::new() },
    }
}

/// A made-up emoji: a disc of `color` with a lighter centre, 32 pixels.
fn emoji_picture(color: [u8; 4]) -> (u32, u32, Vec<u8>) {
    let mut px = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32 {
        for x in 0..32 {
            let d = ((x as f32 - 15.5).powi(2) + (y as f32 - 15.5).powi(2)).sqrt();
            px.extend_from_slice(&match d {
                d if d < 5.0 => [240, 230, 210, 255],
                d if d < 15.0 => color,
                _ => [0, 0, 0, 0],
            });
        }
    }
    (32, 32, px)
}

/// The made-up group chats and communities.
fn communities() -> Vec<ui::Community> {
    let member = |id: &str, name: &str, role: &str, status: &str| ui::Member { id: id.into(), name: name.into(), status: status.into(), role: role.into(), me: id == ME, plain: role == "Member", role_index: (role != "Owner").then_some(0), ..Default::default() };
    let channel = |id: u32, name: &str, topic: &str| ui::Channel { id, name: name.into(), topic: topic.into(), ..Default::default() };
    vec![
        ui::Community {
            id: "5a7c0000000000000000000000000000000000000000000000000000000000a7".into(),
            name: "Saturday crew".into(),
            group: true,
            detail: "Group chat · 3 members".into(),
            channels: vec![channel(0, "general", "")],
            members: vec![member(ME, "Azule", "Member", ""), member(ROBIN, "Robin Okafor", "Member", ""), member("deadbeef00112233445566778899aabbccddeeff00112233445566778899aabb", "June Alvarez", "Member", "")],
            unread: 0,
            can_post: true,
            can_invite: true,
            ..ui::Community::default()
        },
        ui::Community {
            id: CLUB.into(),
            name: "The listening club".into(),
            group: false,
            detail: "Community · 4 members".into(),
            channels: vec![channel(0, "general", "Thursday nights on the porch"), channel(1, "records", "What's on the turntable"), channel(2, "gigs", "Shows worth going to")],
            members: vec![
                member(ROBIN, "Robin Okafor", "Owner", "Hosting thursday"),
                member(ME, "Azule", "Moderator", "Around this evening"),
                member("c0ffee00112233445566778899aabbccddeeff00112233445566778899aabbcc", "Mara Lindqvist", "Member", ""),
                member("a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9", "Theo Park", "Member", "snacks guy"),
            ],
            unread: 3,
            can_post: true,
            can_invite: true,
            can_manage_channels: true,
            can_manage: true,
            emoji: vec!["vinyl".into(), "porch".into(), "yes".into()],
            ..ui::Community::default()
        },
    ]
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

/// Sends `frame` through `encoder` and a decoder, and makes a tile of what
/// comes out: the picture exactly as the other side would see it.
fn through_codec(mut encoder: antiphon_video::Encoder, frame: &antiphon_video::Frame, key: &str, label: &str) -> ui::VideoTile {
    let screen = encoder.stream() == antiphon_video::Stream::Screen;
    let mut decoder = antiphon_video::Decoder::new();
    for datagram in encoder.encode(frame) {
        decoder.push(&datagram);
    }
    let p = decoder.picture(encoder.stream()).expect("a whole first frame decodes");
    ui::VideoTile {
        key: key.into(),
        label: label.into(),
        image: iced::widget::image::Handle::from_rgba(p.width, p.height, p.rgba),
        width: p.width,
        height: p.height,
        mine: false,
        screen,
    }
}

/// A tile of my own, straight from a frame.
fn mine(key: &str, label: &str, frame: &antiphon_video::Frame) -> ui::VideoTile {
    ui::VideoTile {
        key: key.into(),
        label: label.into(),
        image: iced::widget::image::Handle::from_rgba(frame.width, frame.height, frame.rgba.clone()),
        width: frame.width,
        height: frame.height,
        mine: true,
        screen: key.ends_with("screen"),
    }
}

/// Fills the rectangle `(x, y, w, h)` of `f` with `rgb`, clipped.
fn fill_rect(f: &mut antiphon_video::Frame, x: u32, y: u32, w: u32, h: u32, rgb: [u8; 3]) {
    for row in y..(y + h).min(f.height) {
        for col in x..(x + w).min(f.width) {
            let at = ((row * f.width + col) * 4) as usize;
            f.rgba[at..at + 3].copy_from_slice(&rgb);
        }
    }
}

/// A made-up shared screen: an editor window with a sidebar and lines of
/// coloured "code", the kind of picture screen sharing is for.
fn fake_screen(w: u32, h: u32) -> antiphon_video::Frame {
    let mut f = antiphon_video::Frame::solid(w, h, [30, 31, 36, 255]);
    let u = |v: u32| v * w / 1600; // lay out at 1600 wide, scale to `w`
    fill_rect(&mut f, 0, 0, w, u(36), [22, 22, 26]);
    for (i, c) in [[237, 106, 94], [245, 191, 79], [98, 197, 84]].iter().enumerate() {
        fill_rect(&mut f, u(16 + i as u32 * 22), u(12), u(12), u(12), *c);
    }
    fill_rect(&mut f, 0, u(36), u(260), h, [25, 26, 30]);
    for i in 0..14 {
        let width = [150, 120, 170, 90, 140][i % 5];
        fill_rect(&mut f, u(28), u(64 + i as u32 * 34), u(width), u(10), if i == 3 { [240, 234, 214] } else { [92, 96, 110] });
    }
    let palette = [[198, 120, 221], [97, 175, 239], [152, 195, 121], [229, 192, 123], [171, 178, 191]];
    let mut seed = 7u32;
    for line in 0..24 {
        let y = u(64 + line * 32);
        let mut x = u(300 + (line % 4) * 32);
        fill_rect(&mut f, u(280), y, u(18), u(10), [70, 74, 86]);
        for _ in 0..(3 + line % 5) {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let len = u(40 + (seed >> 16) % 160);
            fill_rect(&mut f, x, y, len, u(10), palette[((seed >> 8) % 5) as usize]);
            x += len + u(14);
        }
    }
    f
}

/// A made-up camera picture: a person's head and shoulders against a wall,
/// with a soft light from the left.
fn fake_face(w: u32, h: u32, skin: [u8; 3], top: [u8; 3], wall: [u8; 3]) -> antiphon_video::Frame {
    let mut f = antiphon_video::Frame::solid(w, h, [0, 0, 0, 255]);
    let (cx, head_y, r) = (w as f32 * 0.5, h as f32 * 0.42, h as f32 * 0.2);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32, y as f32);
            let light = 1.05 - 0.35 * fx / w as f32;
            let head = ((fx - cx) / (r * 0.85)).powi(2) + ((fy - head_y) / r).powi(2) <= 1.0;
            let hair = ((fx - cx) / (r * 0.92)).powi(2) + ((fy - head_y + r * 0.25) / (r * 0.9)).powi(2) <= 1.0 && fy < head_y - r * 0.35;
            let body = ((fx - cx) / (w as f32 * 0.32)).powi(2) + ((fy - h as f32 * 1.02) / (h as f32 * 0.38)).powi(2) <= 1.0;
            let c = if hair { [52, 38, 32] } else if head { skin } else if body { top } else { wall };
            let at = ((y * w + x) * 4) as usize;
            for (out, v) in f.rgba[at..at + 3].iter_mut().zip(c) {
                *out = (v as f32 * light).min(255.0) as u8;
            }
        }
    }
    f
}
