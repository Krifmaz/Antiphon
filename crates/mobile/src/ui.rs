//! The interface: three tabs -- Call, Voice, Identity -- in iced.
//!
//! Plain Elm-style state, messages and view, so it reads like any other
//! iced app; [`crate::runner`] is what feeds it events and draws it.

use std::path::PathBuf;

use antiphon_audio::{eq_preset, eq_preset_names, ChainConfig, GainConfig, THIN_VOICE_WARNING_STRENGTH};
use antiphon_crypto::{IdentityKeyPair, Invite};
use iced_core::text::Wrapping;
use iced_core::{Alignment, Element, Font, Length, Padding, Theme};
use iced_widget::{
    button, column, container, mouse_area, pick_list, progress_bar, row, scrollable, slider, space, text, text_input,
    toggler,
};

use crate::call::{self, CallEngine, Phase, Waker};
use crate::platform;
use crate::settings::Settings;

pub type Renderer = iced_wgpu::Renderer;

/// The face bundled through iced's `fira-sans` feature, named so cosmic-text
/// never falls back to whatever the vendor ships as sans-serif.
pub const FONT: Font = Font::with_name("Fira Sans");


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Call,
    Voice,
    Identity,
}

#[derive(Debug, Clone, Copy)]
pub enum Stage {
    NoiseSuppress,
    NoiseGate,
    HighPass,
    LowPass,
    DeEsser,
    Compressor,
    Limiter,
    AutoGain,
    PitchShift,
    Reverb,
}

#[derive(Debug, Clone, Copy)]
pub enum Param {
    SuppressStrength,
    GateThreshold,
    HighPassCutoff,
    LowPassCutoff,
    CompressorThreshold,
    CompressorRatio,
    PitchSemitones,
    ReverbMix,
    ReverbRoom,
}

#[derive(Debug, Clone)]
pub enum Message {
    Tab(Tab),
    PortChanged(String),
    RemoteChanged(String),
    PasteRemote,
    Listen,
    Call,
    Stop,
    PttPressed,
    PttReleased,
    OpenMic(bool),
    Speaker(bool),
    Copy(String),
    ShowPhrase(bool),
    RestoreChanged(String),
    PasteRestore,
    Restore,
    CancelRestore,
    RequestMic,
    MicTest(bool),
    EqPreset(String),
    Toggle(Stage, bool),
    Param(Param, f32),
    ResetChain,
}

pub struct App {
    dir: PathBuf,
    settings: Settings,
    identity: Result<IdentityKeyPair, String>,
    engine: CallEngine,
    tab: Tab,
    port_text: String,
    eq_choice: Option<String>,
    phrase_visible: bool,
    restore_text: String,
    restore_armed: bool,
    notice: Option<String>,
    mic_permission: bool,
    /// Looked up on start and resume rather than every frame: it opens a
    /// socket to ask.
    local_ip: Option<String>,
}

fn load_identity(dir: &std::path::Path) -> Result<IdentityKeyPair, String> {
    let path = antiphon_crypto::store::identity_path(dir);
    antiphon_crypto::store::load_or_create(&path).map_err(|e| format!("Could not load the identity at {}: {e}", path.display()))
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl App {
    pub fn new(dir: PathBuf, wake: Waker) -> Result<Self, String> {
        let settings = Settings::load(&dir);
        let identity = load_identity(&dir);
        let engine = CallEngine::new(settings.chain.clone(), settings.open_mic, settings.speaker, wake)
            .map_err(|e| format!("could not start the call engine: {e}"))?;
        Ok(Self {
            port_text: settings.listen_port.to_string(),
            dir,
            settings,
            identity,
            engine,
            tab: Tab::Call,
            eq_choice: None,
            phrase_visible: false,
            restore_text: String::new(),
            restore_armed: false,
            notice: None,
            mic_permission: platform::has_mic_permission(),
            local_ip: call::local_ip(),
        })
    }

    /// Something is moving on screen -- a call or mic test's level meter --
    /// so the runner should keep redrawing rather than wait for input.
    pub fn is_animating(&self) -> bool {
        self.engine.mic_test_running() || matches!(self.engine.phase(), Phase::Connected { .. })
    }

    /// The activity came back to the front, possibly from the permission
    /// prompt, whose answer only shows up this way.
    pub fn resumed(&mut self) {
        self.mic_permission = platform::has_mic_permission();
        self.local_ip = call::local_ip();
    }

    /// Something the phone reported from outside the interface: a picked
    /// file, a notification button, a push.
    pub fn platform_event(&mut self, event: platform::PlatformEvent) {
        log::info!("platform event: {event:?}");
    }

    /// Every finger is off the screen. Push-to-talk must never stay keyed
    /// because the release landed somewhere the button did not see.
    pub fn touch_ended(&mut self) {
        self.engine.set_ptt(false);
    }

    fn save(&self) {
        self.settings.save(&self.dir);
    }

    fn chain_changed(&mut self) {
        self.engine.set_chain(self.settings.chain.clone());
        self.save();
    }

    pub fn update(&mut self, message: Message) {
        if !matches!(message, Message::PttPressed | Message::PttReleased) {
            self.notice = None;
        }
        match message {
            Message::Tab(tab) => self.tab = tab,
            Message::PortChanged(value) => {
                self.port_text = value.chars().filter(char::is_ascii_digit).take(5).collect();
                if let Ok(port) = self.port_text.parse::<u16>() {
                    self.settings.listen_port = port;
                    self.save();
                }
            }
            Message::RemoteChanged(value) => {
                self.settings.last_remote = value;
                self.save();
            }
            Message::PasteRemote => {
                if let Some(text) = platform::read_clipboard() {
                    self.settings.last_remote = text.trim().to_string();
                    self.save();
                }
            }
            Message::Listen | Message::Call => {
                if !self.mic_permission {
                    platform::request_mic_permission();
                    self.notice = Some("Antiphon needs the microphone for calls. Allow it, then try again.".into());
                    return;
                }
                let identity = match &self.identity {
                    // Not Clone, deliberately; the seed rebuilds an equal copy
                    // for the call task to own.
                    Ok(identity) => IdentityKeyPair::from_master_seed(identity.master_seed()),
                    Err(e) => {
                        self.notice = Some(e.clone());
                        return;
                    }
                };
                if matches!(message, Message::Listen) {
                    let Ok(port) = self.port_text.parse::<u16>() else {
                        self.notice = Some("Enter a port number between 1 and 65535.".into());
                        return;
                    };
                    self.engine.listen(port, identity);
                } else {
                    let target = self.settings.last_remote.trim().to_string();
                    if target.is_empty() {
                        self.notice = Some("Enter their address (ip:port) or paste their invite link.".into());
                        return;
                    }
                    self.engine.call(&target, identity);
                }
            }
            Message::Stop => self.engine.stop(),
            Message::PttPressed => self.engine.set_ptt(true),
            Message::PttReleased => self.engine.set_ptt(false),
            Message::OpenMic(on) => {
                self.settings.open_mic = on;
                self.engine.set_open_mic(on);
                self.save();
            }
            Message::Speaker(on) => {
                self.settings.speaker = on;
                self.engine.set_speaker(on);
                self.save();
            }
            Message::Copy(value) => {
                platform::copy_to_clipboard(&value);
                self.notice = Some("Copied.".into());
            }
            Message::ShowPhrase(show) => self.phrase_visible = show,
            Message::RestoreChanged(value) => {
                self.restore_text = value;
                self.restore_armed = false;
            }
            Message::PasteRestore => {
                if let Some(text) = platform::read_clipboard() {
                    self.restore_text = text;
                    self.restore_armed = false;
                }
            }
            Message::Restore => self.restore(),
            Message::CancelRestore => {
                self.restore_armed = false;
                self.restore_text.clear();
            }
            Message::RequestMic => platform::request_mic_permission(),
            Message::MicTest(on) => {
                if on {
                    if !self.mic_permission {
                        platform::request_mic_permission();
                        return;
                    }
                    self.engine.start_mic_test();
                } else {
                    self.engine.stop();
                }
            }
            Message::EqPreset(name) => {
                self.settings.chain.eq_bands = eq_preset(&name).unwrap_or_default();
                self.eq_choice = Some(name);
                self.chain_changed();
            }
            Message::Toggle(stage, on) => {
                let chain = &mut self.settings.chain;
                match stage {
                    Stage::NoiseSuppress => chain.noise_suppress.enabled = on,
                    Stage::NoiseGate => chain.noise_gate.enabled = on,
                    Stage::HighPass => chain.high_pass.enabled = on,
                    Stage::LowPass => chain.low_pass.enabled = on,
                    Stage::DeEsser => chain.de_esser.enabled = on,
                    Stage::Compressor => chain.compressor.enabled = on,
                    Stage::Limiter => chain.limiter.enabled = on,
                    Stage::AutoGain => match &mut chain.gain {
                        GainConfig::Fixed { enabled, .. } | GainConfig::Auto { enabled, .. } => *enabled = on,
                    },
                    Stage::PitchShift => chain.pitch_shift.enabled = on,
                    Stage::Reverb => chain.reverb.enabled = on,
                }
                self.chain_changed();
            }
            Message::Param(param, value) => {
                let chain = &mut self.settings.chain;
                match param {
                    Param::SuppressStrength => chain.noise_suppress.strength = value,
                    Param::GateThreshold => chain.noise_gate.threshold_db = value,
                    Param::HighPassCutoff => chain.high_pass.cutoff_hz = value,
                    Param::LowPassCutoff => chain.low_pass.cutoff_hz = value,
                    Param::CompressorThreshold => chain.compressor.threshold_db = value,
                    Param::CompressorRatio => chain.compressor.ratio = value,
                    Param::PitchSemitones => chain.pitch_shift.semitones = value,
                    Param::ReverbMix => chain.reverb.wet_dry = value,
                    Param::ReverbRoom => chain.reverb.room_size = value,
                }
                self.chain_changed();
            }
            Message::ResetChain => {
                self.settings.chain = ChainConfig::default_preset();
                self.eq_choice = None;
                self.chain_changed();
            }
        }
    }

    /// Restoring takes two presses: replacing an identity makes this phone
    /// a different person to every contact who pinned the old key, so it
    /// should not happen because someone pasted a phrase to see what it did.
    fn restore(&mut self) {
        if self.engine.is_busy() {
            self.notice = Some("Hang up before restoring an identity.".into());
            return;
        }
        let seed = match antiphon_crypto::mnemonic::seed_from_phrase(self.restore_text.trim()) {
            Ok(seed) => seed,
            Err(e) => {
                self.notice = Some(format!("That phrase is not valid: {e}"));
                return;
            }
        };
        let restored = IdentityKeyPair::from_master_seed(seed);
        if let Ok(current) = &self.identity {
            if current.public().identity_key == restored.public().identity_key {
                self.notice = Some("That phrase already matches this phone's identity. Nothing to do.".into());
                self.restore_text.clear();
                return;
            }
            if !self.restore_armed {
                self.restore_armed = true;
                return;
            }
        }
        let path = antiphon_crypto::store::identity_path(&self.dir);
        match antiphon_crypto::store::save(&path, &restored) {
            Ok(()) => {
                self.identity = Ok(restored);
                self.notice = Some("Identity restored.".into());
                self.restore_text.clear();
                self.restore_armed = false;
                self.phrase_visible = false;
            }
            Err(e) => self.notice = Some(format!("Could not save the identity: {e}")),
        }
    }

    /// `top` and `bottom` are the logical pixels the system draws over.
    pub fn view(&self, top: f32, bottom: f32) -> Element<'_, Message, Theme, Renderer> {
        let tab_button = |label: &'static str, tab: Tab| {
            button(text(label).width(Length::Fill).align_x(Alignment::Center))
                .width(Length::Fill)
                .padding(12)
                .style(if self.tab == tab { button::primary } else { button::secondary })
                .on_press(Message::Tab(tab))
        };
        let tabs = row![
            tab_button("Call", Tab::Call),
            tab_button("Voice", Tab::Voice),
            tab_button("Identity", Tab::Identity)
        ]
        .spacing(8);

        let body = match self.tab {
            Tab::Call => self.call_view(),
            Tab::Voice => self.voice_view(),
            Tab::Identity => self.identity_view(),
        };

        let mut page = column![tabs].spacing(16);
        if let Some(notice) = &self.notice {
            page = page.push(container(text(notice.as_str())).padding(10).width(Length::Fill).style(container::rounded_box));
        }
        page = page.push(scrollable(container(body).padding(Padding { right: 12.0, ..Padding::ZERO })).height(Length::Fill));

        container(page)
            .padding(Padding { top: top + 12.0, bottom: bottom + 12.0, left: 16.0, right: 16.0 })
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn mic_banner(&self) -> Option<Element<'_, Message, Theme, Renderer>> {
        (!self.mic_permission).then(|| {
            container(
                column![
                    text("Microphone access is off. Calls and the mic test need it."),
                    button("Allow microphone").on_press(Message::RequestMic),
                ]
                .spacing(8),
            )
            .padding(12)
            .width(Length::Fill)
            .style(container::rounded_box)
            .into()
        })
    }

    fn call_view(&self) -> Element<'_, Message, Theme, Renderer> {
        let mut content = column![].spacing(14);
        if let Some(banner) = self.mic_banner() {
            content = content.push(banner);
        }

        match self.engine.phase() {
            Phase::Connected { remote, code } => {
                return self.connected_view(remote, code);
            }
            Phase::Listening { local, public } => {
                content = content.push(text("Waiting for a call").size(24));
                content = content.push(text(format!("On your network: {local}")));
                match &public {
                    Some(public) => content = content.push(text(format!("From the internet: {public}"))),
                    None => content = content.push(text("Looking up your internet address...")),
                }
                if let Ok(identity) = &self.identity {
                    let address = public.unwrap_or(local);
                    let link = Invite::new(identity.public().identity_key, address).to_link();
                    content = content.push(text("Send them this invite. It carries your address and who you are, so nobody else can answer in your place."));
                    content = content.push(text(link.clone()).size(13).wrapping(Wrapping::Glyph));
                    content = content.push(button("Copy invite").on_press(Message::Copy(link)));
                }
                content = content.push(button("Stop waiting").style(button::danger).on_press(Message::Stop));
                return content.into();
            }
            Phase::Dialing { remote } => {
                content = content.push(text("Calling...").size(24));
                content = content.push(text(remote).wrapping(Wrapping::Glyph));
                content = content.push(button("Cancel").style(button::danger).on_press(Message::Stop));
                return content.into();
            }
            Phase::Ended { message } => {
                content = content.push(container(text(message)).padding(10).width(Length::Fill).style(container::rounded_box));
            }
            Phase::Idle => {}
        }

        content = content.push(text("Call someone").size(22));
        content = content.push(text("Their address as ip:port, or the invite link they sent you."));
        content = content.push(
            text_input("192.168.1.20:5000 or antiphon://...", &self.settings.last_remote)
                .on_input(Message::RemoteChanged)
                .on_submit(Message::Call)
                .padding(10),
        );
        content = content.push(
            row![
                button("Paste").style(button::secondary).on_press(Message::PasteRemote),
                button("Call").style(button::success).on_press(Message::Call),
            ]
            .spacing(10),
        );

        content = content.push(space().height(8));
        content = content.push(text("Wait for a call").size(22));
        content = content.push(text("Opens this port and shows an invite to send. Over the internet, your router may need to forward the port to this phone."));
        content = content.push(
            row![
                text_input("5000", &self.port_text)
                    .on_input(Message::PortChanged)
                    .on_submit(Message::Listen)
                    .padding(10)
                    .width(Length::Fixed(110.0)),
                button("Wait for a call").on_press(Message::Listen),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
        if let Some(ip) = &self.local_ip {
            content = content.push(text(format!("This phone on your network: {ip}")).size(14));
        }
        content.into()
    }

    fn connected_view(&self, remote: String, code: String) -> Element<'_, Message, Theme, Renderer> {
        let transmitting = self.engine.transmitting();
        let mut content = column![
            text("In a call").size(24),
            text(remote),
            text("Verification code").size(14),
            text(code).size(30),
            text("Read it to each other out loud. If it differs, someone is in the middle of this call: hang up."),
            progress_bar(0.0..=1.0, self.engine.level().min(1.0)).girth(10),
        ]
        .spacing(10);

        if self.settings.open_mic {
            content = content.push(text("Open mic: you are always transmitting."));
        } else {
            let label = if transmitting { "Talking..." } else { "Hold to talk" };
            let pad = container(text(label).size(26))
                .center(Length::Fill)
                .height(Length::Fixed(160.0))
                .style(if transmitting { container::success } else { container::secondary });
            content = content.push(mouse_area(pad).on_press(Message::PttPressed).on_release(Message::PttReleased));
        }

        content = content
            .push(toggler(self.settings.open_mic).label("Open mic").on_toggle(Message::OpenMic))
            .push(toggler(self.settings.speaker).label("Speaker").on_toggle(Message::Speaker))
            .push(
                button(text("Hang up").width(Length::Fill).align_x(Alignment::Center))
                    .width(Length::Fill)
                    .padding(14)
                    .style(button::danger)
                    .on_press(Message::Stop),
            );
        content.into()
    }

    fn voice_view(&self) -> Element<'_, Message, Theme, Renderer> {
        let chain = &self.settings.chain;
        let mut content = column![
            text("Your voice").size(22),
            text("Every stage runs on this phone before anything is sent. Changes apply straight away, even mid-call."),
        ]
        .spacing(12);
        if let Some(banner) = self.mic_banner() {
            content = content.push(banner);
        }

        let testing = self.engine.mic_test_running();
        content = content.push(toggler(testing).label("Hear yourself (use headphones)").on_toggle(Message::MicTest));
        if testing {
            content = content.push(progress_bar(0.0..=1.0, self.engine.level().min(1.0)).girth(10));
        }

        let mut presets: Vec<String> = vec!["Flat".to_string()];
        presets.extend(eq_preset_names().iter().map(|s| s.to_string()));
        content = content.push(
            row![
                text("EQ"),
                pick_list(presets, self.eq_choice.clone(), Message::EqPreset).placeholder("Choose a starting EQ"),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );

        let stage = |label: &'static str, help: &'static str, on: bool, which: Stage| {
            column![toggler(on).label(label).on_toggle(move |v| Message::Toggle(which, v)), text(help).size(13)].spacing(4)
        };
        let knob = |label: String, range: std::ops::RangeInclusive<f32>, value: f32, step: f32, which: Param| {
            column![text(label).size(14), slider(range, value, move |v| Message::Param(which, v)).step(step)].spacing(4)
        };

        let mut suppress = stage(
            "Noise suppression",
            "Removes steady background sound -- fans, hiss, air conditioning -- even under your voice.",
            chain.noise_suppress.enabled,
            Stage::NoiseSuppress,
        );
        if chain.noise_suppress.enabled {
            suppress = suppress.push(knob(
                format!("Strength {:.0}%", chain.noise_suppress.strength * 100.0),
                0.0..=1.0,
                chain.noise_suppress.strength,
                0.05,
                Param::SuppressStrength,
            ));
            if chain.noise_suppress.strength > THIN_VOICE_WARNING_STRENGTH {
                suppress = suppress.push(text("This strong, it starts removing parts of your voice too: speech and background share frequencies.").size(13));
            }
        }
        content = content.push(suppress);

        let mut gate = stage("Noise gate", "Mutes the mic between words, so the room is not heard while you are quiet.", chain.noise_gate.enabled, Stage::NoiseGate);
        if chain.noise_gate.enabled {
            gate = gate.push(knob(format!("Threshold {:.0} dB", chain.noise_gate.threshold_db), -80.0..=-10.0, chain.noise_gate.threshold_db, 1.0, Param::GateThreshold));
        }
        content = content.push(gate);

        let mut high = stage("High-pass filter", "Cuts rumble and handling noise below your voice.", chain.high_pass.enabled, Stage::HighPass);
        if chain.high_pass.enabled {
            high = high.push(knob(format!("Cutoff {:.0} Hz", chain.high_pass.cutoff_hz), 20.0..=300.0, chain.high_pass.cutoff_hz, 5.0, Param::HighPassCutoff));
        }
        content = content.push(high);

        let mut low = stage("Low-pass filter", "Cuts hiss above your voice.", chain.low_pass.enabled, Stage::LowPass);
        if chain.low_pass.enabled {
            low = low.push(knob(format!("Cutoff {:.0} Hz", chain.low_pass.cutoff_hz), 3000.0..=20000.0, chain.low_pass.cutoff_hz, 250.0, Param::LowPassCutoff));
        }
        content = content.push(low);

        content = content.push(stage("De-esser", "Softens harsh s and sh sounds.", chain.de_esser.enabled, Stage::DeEsser));

        let mut comp = stage("Compressor", "Evens out loud and quiet moments so you stay at one level.", chain.compressor.enabled, Stage::Compressor);
        if chain.compressor.enabled {
            comp = comp
                .push(knob(format!("Threshold {:.0} dB", chain.compressor.threshold_db), -50.0..=0.0, chain.compressor.threshold_db, 1.0, Param::CompressorThreshold))
                .push(knob(format!("Ratio {:.1}:1", chain.compressor.ratio), 1.0..=10.0, chain.compressor.ratio, 0.5, Param::CompressorRatio));
        }
        content = content.push(comp);

        content = content.push(stage("Limiter", "A hard ceiling, so a shout or a bump never clips.", chain.limiter.enabled, Stage::Limiter));

        let gain_on = match chain.gain {
            GainConfig::Fixed { enabled, .. } | GainConfig::Auto { enabled, .. } => enabled,
        };
        content = content.push(stage("Auto gain", "Turns a quiet mic up and a loud one down, slowly.", gain_on, Stage::AutoGain));

        let mut pitch = stage("Pitch shift", "Raises or lowers your voice, keeping it sounding like speech.", chain.pitch_shift.enabled, Stage::PitchShift);
        if chain.pitch_shift.enabled {
            pitch = pitch.push(knob(format!("{:+.0} semitones", chain.pitch_shift.semitones), -12.0..=12.0, chain.pitch_shift.semitones, 1.0, Param::PitchSemitones));
        }
        content = content.push(pitch);

        let mut reverb = stage("Reverb", "Adds a room around your voice.", chain.reverb.enabled, Stage::Reverb);
        if chain.reverb.enabled {
            reverb = reverb
                .push(knob(format!("Room {:.0}%", chain.reverb.room_size * 100.0), 0.0..=1.0, chain.reverb.room_size, 0.05, Param::ReverbRoom))
                .push(knob(format!("Mix {:.0}%", chain.reverb.wet_dry * 100.0), 0.0..=1.0, chain.reverb.wet_dry, 0.05, Param::ReverbMix));
        }
        content = content.push(reverb);

        content = content.push(button("Reset to defaults").style(button::secondary).on_press(Message::ResetChain));
        content.into()
    }

    fn identity_view(&self) -> Element<'_, Message, Theme, Renderer> {
        let mut content = column![
            text("Your identity").size(22),
            text("A key made on this phone. There is no account and no server: this key is who you are to your contacts."),
        ]
        .spacing(12);

        match &self.identity {
            Ok(identity) => {
                let key = hex(&identity.public().identity_key);
                let link = Invite::new(identity.public().identity_key, String::new()).to_link();
                content = content
                    .push(text(key.clone()).size(14).wrapping(Wrapping::Glyph))
                    .push(
                        row![
                            button("Copy key").style(button::secondary).on_press(Message::Copy(key)),
                            button("Copy invite").style(button::secondary).on_press(Message::Copy(link)),
                        ]
                        .spacing(10),
                    );

                content = content.push(space().height(8)).push(text("Recovery phrase").size(20));
                if self.phrase_visible {
                    let phrase = antiphon_crypto::mnemonic::phrase_from_seed(&identity.master_seed());
                    content = content
                        .push(text("Write these 24 words down and keep them somewhere safe. Anyone who reads them becomes you, and nobody can reset your identity if you lose them."))
                        .push(container(text(phrase).size(15)).padding(12).width(Length::Fill).style(container::rounded_box))
                        .push(button("Hide").style(button::secondary).on_press(Message::ShowPhrase(false)));
                } else {
                    content = content
                        .push(text("24 words that rebuild this identity on a new phone."))
                        .push(button("Show recovery phrase").on_press(Message::ShowPhrase(true)));
                }
            }
            Err(e) => {
                content = content.push(text(e.as_str()));
            }
        }

        content = content
            .push(space().height(8))
            .push(text("Restore from a phrase").size(20))
            .push(text("Replaces this phone's identity with the one the phrase belongs to."))
            .push(
                text_input("24 words, separated by spaces", &self.restore_text)
                    .on_input(Message::RestoreChanged)
                    .padding(10),
            );
        if self.restore_armed {
            content = content
                .push(text("This phone already has a different identity. Every contact who knows it will see a key-change warning. Replace it?"))
                .push(
                    row![
                        button("Yes, replace it").style(button::danger).on_press(Message::Restore),
                        button("Cancel").style(button::secondary).on_press(Message::CancelRestore),
                    ]
                    .spacing(10),
                );
        } else {
            content = content.push(
                row![
                    button("Paste").style(button::secondary).on_press(Message::PasteRestore),
                    button("Restore").on_press(Message::Restore),
                ]
                .spacing(10),
            );
        }
        content.into()
    }
}
