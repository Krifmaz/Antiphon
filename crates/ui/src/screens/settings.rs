//! Settings: a short list of sections on the left, cards of settings on
//! the right. On a phone the sections stack in one scroll.

use iced_core::{Alignment, Length};
use iced_widget::{button, column, container, pick_list, progress_bar, row, scrollable, text_input};

use super::fill;
use crate::icons::{icon, logo, Icon};
use crate::theme::{self, Btn, ACCENTS, THEMES};
use crate::widgets::*;
use crate::{Action, CheckState, Dialog, El, Model, Msg, Route, Section, Transmit, Ui};

/// Settings on a wide window: the section list beside the chosen section.
pub fn view<'a>(ui: &'a Ui, m: &'a Model, section: Section) -> El<'a, Msg> {
    let p = theme::look();
    let mut nav = column![container(h2("Settings")).padding(pad(6.0, 0.0, 12.0, 10.0))].spacing(2);
    for s in Section::ALL.into_iter().filter(|s| shown(m, *s)) {
        let on = s == section;
        nav = nav.push(
            button(row![icon(s.icon(), 16.0, if on { p.text } else { p.muted }), t(s.label()).size(14).font(theme::semibold())].spacing(10).align_y(Alignment::Center))
                .width(Length::Fill)
                .padding([8, 10])
                .style(theme::btn(if on { Btn::RowOn } else { Btn::Row }))
                .on_press(Msg::Nav(Route::Settings(s))),
        );
    }
    let body = scrollable(container(section_view(ui, m, section)).padding([30, 36]).max_width(760)).height(Length::Fill).style(theme::scroll);
    row![container(nav).width(220).padding([24, 14]).height(Length::Fill), vdivider(), body].into()
}

/// Every section, one after another, for the phone.
pub fn stacked<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let mut col = column![].spacing(28);
    for s in Section::ALL.into_iter().filter(|s| shown(m, *s)) {
        col = col.push(section_view(ui, m, s));
    }
    scrollable(container(col).padding([18, 14])).height(Length::Fill).style(theme::scroll).into()
}

/// Whether this device has section `s`: the phone's own section only on a phone.
fn shown(m: &Model, s: Section) -> bool {
    s != Section::Phone || m.phone.is_some()
}

/// The cards for one section.
fn section_view<'a>(ui: &'a Ui, m: &'a Model, section: Section) -> El<'a, Msg> {
    let body = match section {
        Section::Profile => profile(ui, m),
        Section::Appearance => appearance(m),
        Section::Voice => voice(m),
        Section::Network => network(ui, m),
        Section::About => about(m),
        Section::Phone => phone(ui, m),
    };
    column![h1(section.label()), body].spacing(18).into()
}

/// An input that saves on Enter, with a Save button while it differs.
fn edit_box<'a>(placeholder: &'a str, edited: &'a Option<String>, saved: &'a str, on_input: fn(String) -> Msg, save: Msg) -> El<'a, Msg> {
    let value = edited.as_deref().unwrap_or(saved);
    let dirty = edited.as_deref().is_some_and(|e| e != saved);
    let mut r = row![text_input(placeholder, value).on_input(on_input).on_submit(save.clone()).padding([9, 12]).size(14).style(theme::input)].spacing(8).align_y(Alignment::Center);
    if dirty {
        r = r.push(btn("Save", Btn::Primary, Some(save)));
    }
    r.into()
}

/// Your name, status, and the key that proves who you are.
fn profile<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let me = row![
        avatar(&m.me.name, &m.me.id, 64.0),
        column![t(m.me.name.as_str()).size(18).font(theme::bold()), faint(if m.me.status.is_empty() { "No status" } else { m.me.status.as_str() }).size(13)].spacing(3),
    ]
    .spacing(16)
    .align_y(Alignment::Center);
    let fields = column![
        field("Name", edit_box("What people see", &ui.name_edit, &m.me.name, Msg::NameEdit, Msg::NameSave)),
        field("Status", edit_box("Around after six", &ui.status_edit, &m.me.status, Msg::StatusEdit, Msg::StatusSave)),
    ]
    .spacing(14);
    let identity = column![
        container(t(m.me.fingerprint.as_str()).size(12.5).font(theme::mono()).color(p.muted)).padding([10, 12]).width(Length::Fill).style(theme::inset),
        row![
            btn_icon(Icon::Copy, "Copy fingerprint", Btn::Default, Some(Msg::Copy(m.me.fingerprint.clone(), "Fingerprint"))),
            btn_icon(Icon::Qr, "Show invite", Btn::Ghost, Some(Msg::Dialog(Some(Dialog::Invite)))),
        ]
        .spacing(8),
    ]
    .spacing(12);
    let notify = setting(
        "Message notifications",
        Some("Show a notification when a message arrives and Stoop is in the background."),
        switch(m.settings.notify_messages, |on| Msg::Do(Action::NotifyMessages(on))),
    );
    column![
        card(column![me, fields].spacing(20)),
        titled_card("Your identity", Some("Made on this device and never sent anywhere. People who verified you know this fingerprint."), identity),
        card(notify),
    ]
    .spacing(14)
    .into()
}

/// Theme, accent colour and motion.
fn appearance<'a>(m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let mut themes = row![].spacing(10);
    for (id, name) in THEMES {
        let on = m.settings.theme == id;
        let look = theme::palette(id, &m.settings.accent, "", true);
        let preview = row![
            container(iced_widget::space()).width(14).height(40).style(move |_| flat(look.bg2, 6.0)),
            column![
                container(iced_widget::space()).width(Length::Fill).height(10).style(move |_| flat(look.text.scale_alpha(0.8), 3.0)),
                container(iced_widget::space()).width(Length::Fixed(28.0)).height(10).style(move |_| flat(look.accent, 3.0)),
            ]
            .spacing(6)
            .width(Length::Fill),
        ]
        .spacing(8);
        let tile = button(
            column![
                container(preview).padding(10).width(Length::Fill).style(move |_| iced_widget::container::Style {
                    background: Some(iced_core::Background::Color(look.panel)),
                    border: iced_core::Border { color: look.border, width: 1.0, radius: 10.0.into() },
                    ..Default::default()
                }),
                t(name).size(12.5).font(theme::semibold()),
            ]
            .spacing(8)
            .align_x(Alignment::Center),
        )
        .width(Length::Fill)
        .padding(8)
        .style(move |th, st| {
            let mut s = theme::btn(if on { Btn::RowOn } else { Btn::Row })(th, st);
            if on {
                s.border = iced_core::Border { color: p.accent2, width: 1.5, radius: 12.0.into() };
            }
            s
        })
        .on_press(Msg::Do(Action::Theme(id.to_string())));
        themes = themes.push(tile);
    }
    let mut accents = row![].spacing(8).align_y(Alignment::Center);
    for (id, name, a, b) in ACCENTS {
        accents = accents.push(swatch(theme::hex(a), theme::hex(b), m.settings.accent == id, name, Msg::Do(Action::Accent(id.to_string()))));
    }
    let motion = setting("Reduce motion", Some("Dialogs and panels appear at once instead of sliding."), switch(m.settings.reduced_motion, |on| Msg::Do(Action::ReducedMotion(on))));
    column![
        titled_card("Theme", None, themes),
        titled_card("Accent", Some("Buttons, highlights and the glow behind a call."), accents.wrap()),
        card(motion),
    ]
    .spacing(14)
    .into()
}

/// A plain filled box of colour `c` with corner radius `r`, for swatches.
fn flat(c: iced_core::Color, r: f32) -> iced_widget::container::Style {
    iced_widget::container::Style {
        background: Some(iced_core::Background::Color(c)),
        border: iced_core::Border { radius: r.into(), ..Default::default() },
        ..Default::default()
    }
}

/// Microphone, speakers, how you talk (voice or push to talk), clean-up and output volume.
fn voice<'a>(m: &'a Model) -> El<'a, Msg> {
    let s = &m.settings;
    let device = |label: &'a str, list: &'a [String], chosen: &'a str, f: fn(String) -> Action| -> El<'a, Msg> {
        let selected = if chosen.is_empty() { None } else { Some(chosen.to_string()) };
        field(
            label,
            pick_list(list, selected, move |v| Msg::Do(f(v)))
                .placeholder("System default")
                .width(Length::Fill)
                .padding([9, 12])
                .text_size(14)
                .style(theme::pick)
                .menu_style(theme::pick_menu),
        )
    };
    let devices = column![
        row![device("Microphone", &s.inputs, &s.input, Action::Input), device("Speakers or headphones", &s.outputs, &s.output, Action::Output)].spacing(14),
        field(
            "Output volume",
            volume_slider(s.output_db, Length::Fill, |db| Msg::Do(Action::OutputVolume(db)))
        ),
    ]
    .spacing(16);

    let test: El<'a, Msg> = match s.mic_test {
        Some(level) => row![
            btn_icon(Icon::X, "Stop test", Btn::Default, Some(Msg::Do(Action::MicTest(false)))),
            progress_bar(0.0..=1.0, level).girth(8).style(theme::meter),
        ]
        .spacing(14)
        .align_y(Alignment::Center)
        .into(),
        None => row![btn_icon(Icon::Mic, "Test my microphone", Btn::Default, Some(Msg::Do(Action::MicTest(true)))), faint("You will hear yourself.").size(12.5)]
            .spacing(14)
            .align_y(Alignment::Center)
            .into(),
    };

    let mut seg = row![].spacing(4);
    for mode in Transmit::ALL {
        let on = s.transmit == mode;
        seg = seg.push(
            button(t(mode.label()).size(13).font(theme::semibold()))
                .padding([7, 14])
                .style(theme::btn(if on { Btn::TabOn } else { Btn::Tab }))
                .on_press(Msg::Do(Action::Transmit(mode))),
        );
    }
    let mut talk = column![container(seg).padding(4).style(theme::tabs_frame)].spacing(14);
    if s.transmit == Transmit::PushToTalk && m.device.ptt_keys {
        let key: El<'a, Msg> = if s.ptt_capturing {
            btn("Press a key…", Btn::Primary, None).into()
        } else {
            btn_icon(Icon::Keyboard, if s.ptt_key.is_empty() { "Choose a key" } else { s.ptt_key.as_str() }, Btn::Default, Some(Msg::Do(Action::BindPtt))).into()
        };
        talk = talk.push(setting("Talk key", Some("Hold it to talk. It works while Stoop is in the background."), key));
    }

    let mut processing = column![
        setting("Noise suppression", Some("Takes out fans, keyboards and traffic."), switch(s.noise_suppression, |on| Msg::Do(Action::NoiseSuppression(on)))),
        divider(),
        setting("Echo cancellation", Some("Stops them hearing themselves when you use speakers."), switch(s.echo_cancel, |on| Msg::Do(Action::EchoCancel(on)))),
        divider(),
        setting("Automatic level", Some("Keeps your voice at an even loudness."), switch(s.auto_gain, |on| Msg::Do(Action::AutoGain(on)))),
    ]
    .spacing(12);
    if !s.presets.is_empty() {
        processing = processing.push(divider());
        processing = processing.push(setting(
            "Voice preset",
            Some("A starting point for the whole chain."),
            pick_list(s.presets.as_slice(), (!s.preset.is_empty()).then(|| s.preset.clone()), |v| Msg::Do(Action::Preset(v)))
                .placeholder("Custom")
                .padding([8, 12])
                .text_size(13.5)
                .style(theme::pick)
                .menu_style(theme::pick_menu),
        ));
    }

    let mut cards = column![].spacing(14);
    // A phone's microphone and speaker are the system's to choose.
    if m.phone.is_none() {
        cards = cards.push(titled_card("Devices", None, devices));
    }
    cards
        .push(column![
        titled_card("Microphone test", None, test),
        titled_card("When to send your voice", None, talk),
        titled_card("Processing", None, processing),
    ]
    .spacing(14))
    .into()
}

/// Whether people can reach you, the port, and the relay to fall back on.
fn network<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let mut checks = column![].spacing(10);
    if m.network.checks.is_empty() {
        checks = checks.push(faint("Not checked yet.").size(13));
    }
    for c in &m.network.checks {
        let color = match c.state {
            CheckState::Good => p.ok,
            CheckState::Warn => p.warn,
            CheckState::Bad => p.err,
            CheckState::Unknown => p.faint,
        };
        checks = checks.push(
            row![
                container(iced_widget::space()).width(10).height(10).style(theme::dot(color, color)),
                column![t(c.label.as_str()).size(13.5).font(theme::semibold()), faint(c.detail.as_str()).size(12.5)].spacing(2).width(Length::Fill),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        );
    }
    let check_card = column![row![h3("Can people reach you?"), fill(), btn_icon(Icon::Refresh, "Check again", Btn::Ghost, Some(Msg::Do(Action::CheckNetwork)))].align_y(Alignment::Center), checks].spacing(14);

    let port_text = m.network.port.to_string();
    let port = ui.port_edit.as_deref().unwrap_or(&port_text);
    let port_dirty = ui.port_edit.as_deref().is_some_and(|e| e != port_text);
    let mut port_row = row![text_input("7000", port).on_input(Msg::PortEdit).on_submit(Msg::PortSave).width(110).padding([9, 12]).size(14).style(theme::input)].spacing(8);
    if port_dirty {
        port_row = port_row.push(btn("Save", Btn::Primary, Some(Msg::PortSave)));
    }
    let listen = column![
        setting("Port", Some("Where your door listens. Change it if another app uses this one."), port_row),
        divider(),
        setting("Open my door when Stoop starts", None, switch(m.settings.open_on_start, |on| Msg::Do(Action::OpenOnStart(on)))),
    ]
    .spacing(12);
    let relay = field("Relay", edit_box("relay.example.org:7100", &ui.relay_edit, &m.network.relay, Msg::RelayEdit, Msg::RelaySave));
    column![
        card(check_card),
        titled_card("Your door", None, listen),
        titled_card("Relay", Some("For people who cannot reach you directly. The relay passes sealed packets; it cannot hear the call."), relay),
    ]
    .spacing(14)
    .into()
}

/// Version, where your data lives, and the crash-report switch.
fn about<'a>(m: &'a Model) -> El<'a, Msg> {
    let s = &m.settings;
    column![
        card(
            row![
                logo(56.0),
                column![t("Stoop").size(20).font(theme::bold()), faint(format!("Version {}", s.version)).size(12.5), muted("Calls and messages that go straight to the people you talk to.").size(13)].spacing(4),
            ]
            .spacing(18)
            .align_y(Alignment::Center)
        ),
        titled_card(
            "Your data",
            Some("Settings, contacts and history stay on this device."),
            column![container(t(s.data_dir.as_str()).size(12).font(theme::mono())).padding([8, 10]).width(Length::Fill).style(theme::inset), row![btn_icon(
                Icon::Download,
                "Open received files",
                Btn::Default,
                Some(Msg::Do(Action::OpenDownloads))
            )]]
            .spacing(10)
        ),
    ]
    .spacing(14)
    .into()
}

/// The phone's own settings: the microphone permission, listening in the
/// background, mobile data, and the recovery phrase.
fn phone<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let Some(ph) = m.phone.as_ref() else { return column![].into() };
    let mut cards = column![].spacing(14);
    if !ph.mic_allowed {
        cards = cards.push(card(setting(
            "Microphone",
            Some("Stoop needs it to send your voice. Nothing is recorded."),
            btn("Allow", Btn::Primary, Some(Msg::Do(Action::RequestMic))),
        )));
    }
    let background = column![
        setting(
            "Take calls in the background",
            Some("Keeps your door open when Stoop is not on screen. Uses a little battery."),
            switch(ph.keep_listening, |on| Msg::Do(Action::KeepListening(on)))
        ),
        divider(),
        setting(
            "Even on battery saver",
            Some("Otherwise the door closes in the background while battery saver is on."),
            switch(ph.listen_on_saver, |on| Msg::Do(Action::ListenOnSaver(on)))
        ),
    ]
    .spacing(12);
    let limit_text = if ph.cellular_limit_mb == 0 { String::new() } else { ph.cellular_limit_mb.to_string() };
    let limit_value = ui.limit_edit.as_deref().unwrap_or(&limit_text);
    let mut limit = row![text_input("No limit", limit_value).on_input(Msg::LimitEdit).on_submit(Msg::LimitSave).width(110).padding([9, 12]).size(14).style(theme::input), faint("MB").size(13)]
        .spacing(8)
        .align_y(Alignment::Center);
    if ui.limit_edit.as_deref().is_some_and(|e| e != limit_text) {
        limit = limit.push(btn("Save", Btn::Primary, Some(Msg::LimitSave)));
    }
    let data = column![
        setting("Files only on Wi-Fi", Some("Files wait for Wi-Fi instead of using mobile data."), switch(ph.wifi_only, |on| Msg::Do(Action::WifiOnly(on)))),
        divider(),
        setting("Data saver", Some("Smaller pictures and fewer checks on mobile data."), switch(ph.data_saver, |on| Msg::Do(Action::DataSaver(on)))),
        divider(),
        setting("Monthly limit for files", Some(ph.cellular_used.as_str()), limit),
    ]
    .spacing(12);
    let phrase: El<'a, Msg> = match ph.phrase.as_deref() {
        Some(words) => column![
            container(t(words).size(13.5).font(theme::mono()).color(p.text)).padding([10, 12]).width(Length::Fill).style(theme::inset),
            row![
                btn_icon(Icon::Copy, "Copy", Btn::Default, Some(Msg::Copy(words.to_string(), "Recovery phrase"))),
                btn("Hide", Btn::Ghost, Some(Msg::Do(Action::ShowPhrase(false)))),
            ]
            .spacing(8),
        ]
        .spacing(10)
        .into(),
        None => row![btn_icon(Icon::Lock, "Show my recovery phrase", Btn::Default, Some(Msg::Do(Action::ShowPhrase(true))))].into(),
    };
    let ready = !ui.restore_edit.trim().is_empty();
    let restore = column![
        text_input("The 24 words, in order", &ui.restore_edit).on_input(Msg::RestoreEdit).padding([10, 12]).size(14).style(theme::input),
        row![btn("Restore", Btn::Danger, ready.then_some(Msg::Dialog(Some(Dialog::ConfirmRestore))))],
    ]
    .spacing(10);
    cards
        .push(titled_card("In the background", None, background))
        .push(titled_card("Mobile data", None, data))
        .push(titled_card(
            "Recovery phrase",
            Some("These words bring your identity back on a new phone. Anyone who has them can be you, so keep them somewhere only you can reach."),
            phrase,
        ))
        .push(titled_card("Restore from a phrase", Some("Moving from another device? Type its phrase here."), restore))
        .into()
}
