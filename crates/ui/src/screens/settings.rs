//! Settings, laid out the way Discord's are.
//!
//! - **Wide window** ([`view`]): a sidebar of sections gathered under
//!   group headings ([`Group`]), and the chosen section beside it: its
//!   title, a row of tabs when it has more than one page
//!   ([`Section::tabs`]), then its cards.
//! - **Phone** ([`menu`], then [`page`]): you at the top, then a grouped
//!   list of sections, each row opening that section on its own screen
//!   with the same tabs.
//!
//! Every section's cards come from one function each below, shared by
//! both layouts.

use iced_core::{Alignment, Length};
use iced_widget::{button, column, container, progress_bar, row, scrollable};

use super::fill;
use crate::icons::{icon, logo, Icon};
use crate::a11y::Role;
use crate::theme::{self, Btn, ACCENTS, FONTS, THEMES};
use crate::widgets::*;
use crate::{Action, CheckState, El, Group, Model, Msg, Route, Section, Transmit, Ui};

/// Settings on a wide window: the grouped section list beside the chosen
/// section.
pub fn view<'a>(ui: &'a Ui, m: &'a Model, section: Section) -> El<'a, Msg> {
    let p = theme::look();
    let mut nav = column![container(h2("Settings")).padding(pad(0.0, 0.0, 14.0, 10.0))].spacing(2);
    for (g, group) in Group::ALL.into_iter().enumerate() {
        let mut sections = group.sections().filter(|s| shown(m, *s)).peekable();
        if sections.peek().is_none() {
            continue;
        }
        // The group's heading, with more room above it than between its
        // rows, so the groups read as blocks.
        nav = nav.push(container(label(group.label())).padding(pad(if g == 0 { 0.0 } else { 16.0 }, 10.0, 6.0, 10.0)));
        for s in sections {
            let on = s == section;
            nav = nav.push(
                pressable(
                    row![icon(s.icon(), 16.0, if on { p.text } else { p.muted }), t(s.label()).size(14).font(theme::semibold())].spacing(10).align_y(Alignment::Center),
                    Role::Tab,
                    s.label(),
                    on,
                    if on { Btn::RowOn } else { Btn::Row },
                    Msg::Nav(Route::Settings(s)),
                )
                .width(Length::Fill)
                .padding([7, 10]),
            );
        }
    }
    let nav = scrollable(container(nav).padding([24, 14])).height(Length::Fill).style(theme::scroll);
    let body = scrollable(container(page_body(ui, m, section, true)).padding([30, 36]).max_width(760)).height(Length::Fill).style(theme::scroll);
    row![container(nav).width(232).height(Length::Fill), vdivider(), body].into()
}

/// The phone's settings list: you at the top, then each group's sections
/// as rows in a rounded block, each opening [`Route::Settings`].
pub fn menu<'a>(m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let status = if m.me.status.is_empty() { "Your profile" } else { m.me.status.as_str() };
    let me = pressable(
        row![
            avatar(&m.me.name, &m.me.id, 48.0),
            column![t(m.me.name.as_str()).size(16).font(theme::bold()), faint(status).size(12.5)].spacing(2).width(Length::Fill),
            icon(Icon::Chevron, 14.0, p.faint),
        ]
        .spacing(14)
        .align_y(Alignment::Center),
        Role::Button,
        "Your profile",
        false,
        Btn::Row,
        Msg::Nav(Route::Profile(None)),
    )
    .width(Length::Fill)
    .padding([12, 12]);
    let mut col = column![container(me).padding(2).style(theme::card_raised)];
    for group in Group::ALL {
        let mut rows = column![];
        for (i, s) in group.sections().filter(|s| shown(m, *s)).enumerate() {
            if i > 0 {
                // A hairline between rows, indented past the icon as a
                // phone's lists are.
                rows = rows.push(container(divider()).padding(pad(0.0, 0.0, 0.0, 44.0)));
            }
            rows = rows.push(
                pressable(
                    row![icon(s.icon(), 18.0, p.muted), t(s.label()).size(15).width(Length::Fill), icon(Icon::Chevron, 14.0, p.faint)]
                        .spacing(14)
                        .align_y(Alignment::Center),
                    Role::Button,
                    s.label(),
                    false,
                    Btn::Row,
                    Msg::Nav(Route::Settings(s)),
                )
                .width(Length::Fill)
                .padding([13, 12]),
            );
        }
        col = col.push(container(label(group.label())).padding(pad(20.0, 0.0, 8.0, 6.0)));
        col = col.push(container(rows).padding(2).style(theme::card_raised));
    }
    col = col.push(container(faint(format!("Stoop {}", m.settings.version)).size(12)).padding(pad(18.0, 0.0, 0.0, 6.0)));
    scrollable(container(col).padding([14, 12])).height(Length::Fill).style(theme::scroll).into()
}

/// One section on a phone, under the screen's own title bar: its tabs,
/// then its cards.
pub fn page<'a>(ui: &'a Ui, m: &'a Model, section: Section) -> El<'a, Msg> {
    scrollable(container(page_body(ui, m, section, false)).padding([16, 14])).height(Length::Fill).style(theme::scroll).into()
}

/// A section's page: its title (when `title`; a phone's title bar already
/// says it), its tabs when it has more than one page, then the cards of
/// the tab on view.
fn page_body<'a>(ui: &'a Ui, m: &'a Model, section: Section, title: bool) -> El<'a, Msg> {
    let names = section.tabs(!m.device.phone);
    // The index can outlive the tab it named (the plugins tab is not on a
    // phone), so keep it inside the list.
    let tab = ui.settings_tab.min(names.len().saturating_sub(1));
    let mut col = column![].spacing(18);
    if title {
        col = col.push(h1(section.label()));
    }
    if !names.is_empty() {
        let items = names.iter().enumerate().map(|(i, name)| (*name, Msg::SettingsTab(i))).collect();
        col = col.push(tabs(items, tab));
    }
    col.push(section_view(ui, m, section, tab)).into()
}

/// Whether this device has section `s`: Updates only where this copy can
/// update itself.
fn shown(m: &Model, s: Section) -> bool {
    s != Section::Updates || m.update.available
}

/// The cards for one section, on tab `tab` (see [`Section::tabs`]).
fn section_view<'a>(ui: &'a Ui, m: &'a Model, section: Section, tab: usize) -> El<'a, Msg> {
    match section {
        Section::Profile => profile(ui, m),
        Section::Account => super::account::view(ui, m, tab),
        Section::Devices => super::account::devices(ui, m),
        Section::Privacy => privacy(ui, m),
        Section::Notifications => notifications(m),
        Section::Voice => voice(m, tab),
        Section::Appearance => appearance(ui, m, tab),
        Section::Data => data(ui, m),
        Section::Network => network(ui, m),
        Section::Relays => relays(ui, m, tab),
        Section::Updates => updates(m),
        Section::About => about(m),
    }
}

/// An input named `name` (widget id `id`) that saves on Enter, with a Save
/// button while it differs.
fn edit_box<'a>(name: &str, id: &'static str, placeholder: &'a str, edited: &'a Option<String>, saved: &'a str, on_input: fn(String) -> Msg, save: Msg) -> El<'a, Msg> {
    let value = edited.as_deref().unwrap_or(saved);
    let dirty = edited.as_deref().is_some_and(|e| e != saved);
    let mut r = row![input(name, id, placeholder, value).on_input(on_input).on_submit(save.clone()).padding([9, 12]).size(14)].spacing(8).align_y(Alignment::Center);
    if dirty {
        r = r.push(btn("Save", Btn::Primary, Some(save)));
    }
    r.into()
}

/// Your picture, name, pronouns, status, About line and colour.
fn profile<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let mut picture_buttons = row![btn_icon(Icon::Edit, if m.me.has_picture { "Change picture" } else { "Add a picture" }, Btn::Default, Some(Msg::Do(Action::PickAvatar)))].spacing(8);
    if m.me.has_picture {
        picture_buttons = picture_buttons.push(btn_icon(Icon::Trash, "Remove", Btn::Ghost, Some(Msg::Do(Action::RemoveAvatar))));
    }
    let me = row![
        avatar(&m.me.name, &m.me.id, 72.0),
        column![
            t(m.me.name.as_str()).size(18).font(theme::bold()),
            faint(if m.me.status.is_empty() { "No status" } else { m.me.status.as_str() }).size(13),
            picture_buttons,
        ]
        .spacing(6),
    ]
    .spacing(16)
    .align_y(Alignment::Center);
    // The colour behind your initial when there is no picture, here and
    // on your contacts' screens: automatic, or one of the set.
    // The automatic swatch carries a sparkle, so it does not read as one
    // more fixed colour when the key's colour happens to match one.
    let auto = theme::avatar_color(&m.me.id);
    let automatic = iced_widget::stack![
        swatch(auto, auto, m.me.color.is_empty(), "Automatic", Msg::Do(Action::ProfileColor(String::new()))),
        container(icon(Icon::Sparkle, 14.0, theme::hex("#16161f"))).center(34),
    ];
    let mut colors = row![automatic].spacing(8);
    for (id, name, hex) in crate::PROFILE_COLORS {
        let c = theme::hex(hex);
        colors = colors.push(swatch(c, c, m.me.color == id, name, Msg::Do(Action::ProfileColor(id.to_string()))));
    }
    let fields = column![
        field("Name", edit_box("Name", "stoop-name", "What people see", &ui.name_edit, &m.me.name, Msg::NameEdit, Msg::NameSave)),
        field("Pronouns", edit_box("Pronouns", "stoop-pronouns", "they/them, she/her…", &ui.pronouns_edit, &m.me.pronouns, Msg::PronounsEdit, Msg::PronounsSave)),
        field("Status", edit_box("Status", "stoop-status", "Around after six", &ui.status_edit, &m.me.status, Msg::StatusEdit, Msg::StatusSave)),
        field("About you", edit_box("About you", "stoop-bio", "A line or two people see in your details", &ui.bio_edit, &m.me.bio, Msg::BioEdit, Msg::BioSave)),
        field("Colour", colors),
        faint("Your picture, pronouns, About line and colour go to the people you call. They stay on your devices and theirs.").size(12),
    ]
    .spacing(14);
    card(column![me, fields].spacing(20)).into()
}

/// Which notifications Stoop shows.
fn notifications<'a>(m: &'a Model) -> El<'a, Msg> {
    let notify = setting(
        "Message notifications",
        Some("Show a notification when a message arrives and Stoop is in the background."),
        switch("Message notifications", m.settings.notify_messages, |on| Msg::Do(Action::NotifyMessages(on))),
    );
    card(notify).into()
}

/// Appearance's tab `tab`: 0 the theme, accent colour and theme files;
/// 1 the reading font and motion.
fn appearance<'a>(ui: &'a Ui, m: &'a Model, tab: usize) -> El<'a, Msg> {
    let p = theme::look();
    let mut themes = row![].spacing(10);
    for (id, name) in THEMES {
        let on = m.settings.theme == id;
        let look = theme::palette(id, &m.settings.accent, &m.settings.custom_accent, true);
        let focused = crate::a11y::Spec::new(Role::Tab, format!("{name} theme")).selected(on).on(Some(&Msg::Do(Action::Theme(id.to_string())))).add();
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
        let tile = button(crate::a11y::mark(
            column![
                container(preview).padding(10).width(Length::Fill).style(move |_| container::Style {
                    background: Some(iced_core::Background::Color(look.panel)),
                    border: iced_core::Border { color: look.border, width: 1.0, radius: 10.0.into() },
                    ..Default::default()
                }),
                t(name).size(12.5).font(theme::semibold()),
            ]
            .spacing(8)
            .align_x(Alignment::Center),
            focused,
        ))
        .width(Length::Fill)
        .padding(8)
        .style(move |th, st| {
            let mut s = theme::btn(if on { Btn::RowOn } else { Btn::Row })(th, st);
            if on {
                s.border = iced_core::Border { color: p.accent2, width: 1.5, radius: 12.0.into() };
            }
            if focused {
                // The keyboard focus ring: thicker than the selected line.
                s.border = iced_core::Border { color: p.accent2, width: 2.5, radius: 12.0.into() };
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
    // Your own colour: typed as #rrggbb, its partner worked out as Encore does.
    let custom = theme::parse_hex(&m.settings.custom_accent);
    if let Some(c) = custom {
        accents = accents.push(swatch(c, theme::partner(c), m.settings.accent == "custom", "Custom", Msg::Do(Action::CustomAccent(m.settings.custom_accent.clone()))));
    }
    let custom_value = ui.accent_edit.as_deref().unwrap_or(&m.settings.custom_accent);
    let mut custom_row = row![input("Custom accent colour", "stoop-accent", "#8b5cf6", custom_value).on_input(Msg::AccentEdit).on_submit(Msg::AccentSave).width(130).padding([8, 12]).size(14)]
        .spacing(8)
        .align_y(Alignment::Center);
    if ui.accent_edit.as_deref().is_some_and(|e| e != m.settings.custom_accent) {
        custom_row = custom_row.push(btn("Use colour", Btn::Primary, Some(Msg::AccentSave)));
    }
    let accent_body = column![accents.wrap(), setting("Your own colour", Some("Any colour as #rrggbb."), custom_row)].spacing(14);

    let mut fonts = row![].spacing(10);
    for (id, name) in FONTS {
        let on = m.settings.font == id || (m.settings.font.is_empty() && id == FONTS[0].0);
        let sample = column![
            t("Aa Bb Qq 123").size(18).font(theme::font_for(id)),
            t(name).size(12.5).font(theme::weighted(theme::font_for(id), iced_core::font::Weight::Semibold)),
        ]
        .spacing(6)
        .align_x(Alignment::Center);
        fonts = fonts.push(
            pressable(sample, Role::Tab, &format!("{name} font"), on, if on { Btn::RowOn } else { Btn::Row }, Msg::Do(Action::Font(id.to_string())))
                .width(Length::Fill)
                .padding([12, 8]),
        );
    }

    let motion = setting("Reduce motion", Some("Dialogs and panels appear at once instead of sliding."), switch("Reduce motion", m.settings.reduced_motion, |on| Msg::Do(Action::ReducedMotion(on))));
    let files = row![
        btn_icon(Icon::Download, "Export theme…", Btn::Default, Some(Msg::Do(Action::ExportTheme))),
        btn_icon(Icon::Palette, "Import theme…", Btn::Ghost, Some(Msg::Do(Action::ImportTheme))),
    ]
    .spacing(8);
    if tab == 1 {
        return column![
            titled_card("Reading font", Some("OpenDyslexic's heavier letter bottoms help some people with dyslexia keep letters in place."), fonts),
            card(motion),
        ]
        .spacing(14)
        .into();
    }
    column![
        titled_card("Theme", None, themes),
        titled_card("Accent", Some("Buttons, highlights and the glow behind a call."), accent_body),
        titled_card(
            "Theme files",
            Some("Save your theme, accent and reading font to a file to share or keep, or load one. A theme file is plain settings: it can't run anything."),
            files
        ),
    ]
    .spacing(14)
    .into()
}

/// A plain filled box of colour `c` with corner radius `r`, for swatches.
fn flat(c: iced_core::Color, r: f32) -> container::Style {
    container::Style {
        background: Some(iced_core::Background::Color(c)),
        border: iced_core::Border { radius: r.into(), ..Default::default() },
        ..Default::default()
    }
}

/// Voice & video's tab `tab`: 0 the microphone, speakers, volume, the
/// microphone test and how you talk (voice or push to talk); 1 clean-up
/// and presets; 2 (computers only) audio plugins.
fn voice<'a>(m: &'a Model, tab: usize) -> El<'a, Msg> {
    let s = &m.settings;
    let device = |label: &'a str, list: &'a [String], chosen: &'a str, f: fn(String) -> Action| -> El<'a, Msg> {
        let selected = if chosen.is_empty() { None } else { Some(chosen.to_string()) };
        field(label, choice(label, list, selected, "System default", move |v| Msg::Do(f(v)), |p| p.width(Length::Fill).padding([9, 12]).text_size(14)))
    };
    let devices = column![
        row![device("Microphone", &s.inputs, &s.input, Action::Input), device("Speakers or headphones", &s.outputs, &s.output, Action::Output)].spacing(14),
        field(
            "Output volume",
            volume_slider("Output volume", s.output_db, Length::Fill, |db| Msg::Do(Action::OutputVolume(db)))
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
            pressable(t(mode.label()).size(13).font(theme::semibold()), Role::Tab, mode.label(), on, if on { Btn::TabOn } else { Btn::Tab }, Msg::Do(Action::Transmit(mode))).padding([7, 14]),
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
        setting("Noise suppression", Some("Takes out fans, keyboards and traffic."), switch("Noise suppression", s.noise_suppression, |on| Msg::Do(Action::NoiseSuppression(on)))),
        divider(),
        setting("Echo cancellation", Some("Stops them hearing themselves when you use speakers."), switch("Echo cancellation", s.echo_cancel, |on| Msg::Do(Action::EchoCancel(on)))),
        divider(),
        setting("Automatic level", Some("Keeps your voice at an even loudness."), switch("Automatic level", s.auto_gain, |on| Msg::Do(Action::AutoGain(on)))),
    ]
    .spacing(12);
    if !m.device.phone {
        processing = processing.push(divider());
        processing = processing.push(setting(
            "Share screen audio",
            Some("When you share your screen, they hear what your computer plays too. Your call itself is taken back out."),
            switch("Share screen audio", s.screen_audio, |on| Msg::Do(Action::ScreenAudio(on))),
        ));
    }
    if !s.presets.is_empty() {
        processing = processing.push(divider());
        processing = processing.push(setting(
            "Voice preset",
            Some("A starting point for the whole chain."),
            choice("Voice preset", s.presets.as_slice(), (!s.preset.is_empty()).then(|| s.preset.clone()), "Custom", |v| Msg::Do(Action::Preset(v)), |p| p.padding([8, 12]).text_size(13.5)),
        ));
    }

    let mut cards = column![].spacing(14);
    // Without the microphone there is nothing to set, so asking for it
    // comes first on every tab.
    if m.phone.as_ref().is_some_and(|ph| !ph.mic_allowed) {
        cards = cards.push(card(setting("Microphone", Some("Stoop needs it to send your voice. Nothing is recorded."), btn("Allow", Btn::Primary, Some(Msg::Do(Action::RequestMic))))));
    }
    let cards = match tab {
        1 => cards.push(titled_card("Processing", None, processing)),
        2 => fine_tune(cards, s),
        3 if !m.device.phone => cards.push(titled_card(
            "Plugins",
            Some("Your own LADSPA, LV2 and VST3 effects, run on your microphone after the processing. A plugin is a program: add only ones you trust."),
            plugins(&s.plugins),
        )),
        _ => {
            // A phone's microphone and speaker are the system's to choose.
            if m.phone.is_none() {
                cards = cards.push(titled_card("Devices", None, devices));
            }
            cards.push(titled_card("Microphone test", None, test)).push(titled_card("When to send your voice", None, talk))
        }
    };
    cards.into()
}

/// Voice's "Fine-tune" tab (spec §1 "every chain stage toggles
/// independently"): a card per stage of the chain, in the order the sound
/// runs through them, each with its switch and, while on, its settings;
/// then the EQ's bands, and saving the chain to a file and loading one.
fn fine_tune<'a>(mut cards: iced_widget::Column<'a, Msg>, s: &'a crate::Settings) -> iced_widget::Column<'a, Msg> {
    for stage in &s.stages {
        let id = stage.id;
        let mut body = column![setting(stage.name, Some(stage.help), switch(stage.name, stage.enabled, move |on| Msg::Do(Action::StageOn(id, on))))].spacing(12);
        if stage.enabled {
            for p in &stage.params {
                let param = p.id;
                body = body.push(param_row(p, move |v| Msg::Do(Action::StageParam(id, param, v))));
            }
        }
        if let Some(db) = stage.reduction_db {
            // How hard it is working right now: a stage that is on and
            // doing nothing looks the same as one doing its job until it
            // is shown.
            body = body.push(field(
                "Turning you down by",
                row![progress_bar(0.0..=24.0, db.min(24.0)).girth(8).style(theme::meter), faint(format_value(db, 0.1, "dB")).size(12).width(84)].spacing(10).align_y(Alignment::Center),
            ));
        }
        cards = cards.push(card(body));
    }

    let mut eq = column![].spacing(12);
    if s.bands.is_empty() {
        eq = eq.push(faint("No EQ bands. Pick a voice preset on the Processing tab to start from one.").size(13));
    }
    for (i, band) in s.bands.iter().enumerate() {
        let mut item = column![setting(band.shape, None, switch(band.shape, band.enabled, move |on| Msg::Do(Action::BandParam(i, "enabled", if on { 1.0 } else { 0.0 }))))].spacing(10);
        if band.enabled {
            for p in &band.params {
                let param = p.id;
                item = item.push(param_row(p, move |v| Msg::Do(Action::BandParam(i, param, v))));
            }
        }
        eq = eq.push(container(item).padding(10).style(theme::tabs_frame));
    }
    cards = cards.push(titled_card("EQ", Some("Shapes the tone of your voice, band by band."), eq));

    if s.preset_files {
        cards = cards.push(titled_card(
            "Preset files",
            Some("Save everything on this tab to a file to share it, or load someone else's. Plugins are never loaded from a file."),
            row![btn_icon(Icon::Download, "Save to a file…", Btn::Default, Some(Msg::Do(Action::ExportPreset))), btn_icon(Icon::File, "Load a file…", Btn::Ghost, Some(Msg::Do(Action::ImportPreset)))].spacing(10),
        ));
    }
    cards
}

/// One setting's label, slider and sentence; `on` turns a value into the
/// message to send.
fn param_row<'a>(p: &'a crate::ParamRow, on: impl Fn(f32) -> Msg + 'a) -> El<'a, Msg> {
    column![field(p.name, value_slider(p.name, p.value, p.min, p.max, p.step, p.unit, on)), faint(p.help).size(12)].spacing(4).into()
}

/// Spec §1 "Plugin Host": the chain's plugins with their controls, and
/// what can be added.
fn plugins<'a>(p: &'a crate::Plugins) -> El<'a, Msg> {
    let mut body = column![].spacing(12);
    if p.chain.is_empty() {
        body = body.push(faint("No plugins in your chain.").size(13));
    }
    for (i, plugin) in p.chain.iter().enumerate() {
        let mut head = row![
            column![t(plugin.name.as_str()).size(14).font(theme::semibold()), faint(plugin.format.as_str()).size(12)].spacing(2).width(Length::Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if i > 0 {
            head = head.push(icon_btn(Icon::Up, "Run earlier", Some(Msg::Do(Action::PluginEarlier(i)))));
        }
        head = head
            .push(switch(&plugin.name, plugin.enabled, move |on| Msg::Do(Action::PluginOn(i, on))))
            .push(icon_btn(Icon::Trash, "Remove", Some(Msg::Do(Action::RemovePlugin(i)))));
        let mut item = column![head].spacing(8);
        if let Some(problem) = &plugin.problem {
            item = item.push(error_box(problem));
        }
        for param in &plugin.params {
            let id = param.id.clone();
            let control: El<'a, Msg> = if param.toggle {
                switch(&param.name, param.value >= 0.5, move |on| Msg::Do(Action::PluginParam(i, id.clone(), if on { 1.0 } else { 0.0 })))
            } else {
                let step = ((param.max - param.min) / 100.0).max(f32::EPSILON);
                row![
                    iced_widget::slider(param.min..=param.max, param.value, move |v| Msg::Do(Action::PluginParam(i, id.clone(), v)))
                        .step(step)
                        .width(Length::Fill)
                        .style(theme::slider_style),
                    faint(format!("{:.2}", param.value)).size(12).width(52),
                ]
                .spacing(10)
                .align_y(Alignment::Center)
                .into()
            };
            item = item.push(field(param.name.as_str(), control));
        }
        body = body.push(container(item).padding(10).style(theme::tabs_frame));
    }
    match &p.found {
        None => {
            body = body.push(btn_icon(Icon::Search, if p.scanning { "Looking…" } else { "Find plugins" }, Btn::Default, (!p.scanning).then_some(Msg::Do(Action::FindPlugins))));
        }
        Some(found) if found.is_empty() => {
            body = body.push(faint("No plugins were found in the usual folders.").size(13));
        }
        Some(found) => {
            let mut list = column![faint("Found on this computer").size(12.5)].spacing(6);
            for (i, name) in found.iter().enumerate() {
                list = list.push(
                    row![t(name.as_str()).size(13).width(Length::Fill), btn_sm("Add", Btn::Ghost, Some(Msg::Do(Action::AddPlugin(i))))]
                        .spacing(8)
                        .align_y(Alignment::Center),
                );
            }
            body = body.push(scrollable(list).height(Length::Shrink));
        }
    }
    body.into()
}

/// Whether people can reach you, and the port your door listens on.
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
    let mut port_row = row![input("Port", "stoop-port", "7000", port).on_input(Msg::PortEdit).on_submit(Msg::PortSave).width(110).padding([9, 12]).size(14)].spacing(8);
    if port_dirty {
        port_row = port_row.push(btn("Save", Btn::Primary, Some(Msg::PortSave)));
    }
    let listen = column![
        setting("Port", Some("Where your door listens. Change it if another app uses this one."), port_row),
        divider(),
        setting("Open my door when Stoop starts", None, switch("Open my door when Stoop starts", m.settings.open_on_start, |on| Msg::Do(Action::OpenOnStart(on)))),
    ]
    .spacing(12);
    column![card(check_card), titled_card("Your door", None, listen)].spacing(14).into()
}

/// Relays & mailboxes' tab `tab`: 0 the relay to fall back on and
/// lending it; 1 the mailbox nodes that hold sealed mail.
fn relays<'a>(ui: &'a Ui, m: &'a Model, tab: usize) -> El<'a, Msg> {
    let p = theme::look();
    if tab == 0 {
        let mut relay = column![field("Relay", edit_box("Relay", "stoop-relay", "relay.example.org:7100", &ui.relay_edit, &m.network.relay, Msg::RelayEdit, Msg::RelaySave))].spacing(12);
        if !m.network.relay_note.is_empty() {
            relay = relay.push(faint(m.network.relay_note.as_str()).size(12.5));
        }
        relay = relay.push(divider()).push(setting(
            "Let contacts use my relay",
            Some("Friends with no relay of their own can be reached through yours. They use your bandwidth, never your keys. Off unless you turn it on."),
            switch("Let contacts use my relay", m.network.lend_relay, |on| Msg::Do(Action::LendRelay(on))),
        ));
        return titled_card("Relay", Some("For people who cannot reach you directly. The relay passes sealed packets; it cannot hear the call."), relay);
    }
    let mut nodes = column![].spacing(8);
    if m.network.mailboxes.is_empty() {
        nodes = nodes.push(faint("None yet: mail for you waits until you're online, and yours until they are.").size(12.5));
    }
    for n in &m.network.mailboxes {
        let sub = if n.status.is_empty() { n.route.clone() } else { format!("{} · {}", n.route, n.status) };
        nodes = nodes.push(
            row![
                icon(Icon::Door, 15.0, p.faint),
                column![t(n.address.as_str()).size(13.5).font(theme::semibold()), faint(sub).size(12)].spacing(1).width(Length::Fill),
                icon_btn(Icon::Trash, "Remove", Some(Msg::Do(Action::RemoveNode(n.address.clone())))),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
    }
    nodes = nodes.push(divider());
    nodes = nodes.push(
        row![
            input("Mailbox node to add", "stoop-node-add", "mail.example.org:7000 or stoop://node/…", &ui.node_edit).on_input(Msg::NodeEdit).on_submit(Msg::NodeAdd).padding([9, 12]).size(13.5),
            btn("Add", Btn::Primary, (!ui.node_edit.trim().is_empty()).then_some(Msg::NodeAdd)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    );
    let finding = m.network.finding_nodes;
    nodes = nodes.push(
        row![
            btn_icon(Icon::Qr, "Add by QR code", Btn::Ghost, Some(Msg::Do(Action::AddNodeFromQr))),
            btn_icon(Icon::Wifi, if finding { "Looking…" } else { "Find nodes on my network" }, Btn::Ghost, (!finding).then_some(Msg::Do(Action::FindNodes))),
        ]
        .spacing(8),
    );
    for found in m.network.found_nodes.iter().filter(|f| !m.network.mailboxes.iter().any(|n| n.address == **f)) {
        nodes = nodes.push(
            row![faint(format!("Found {found}")).size(12.5).width(Length::Fill), btn_sm("Add", Btn::Ghost, Some(Msg::Do(Action::AddNode(found.clone()))))]
                .spacing(8)
                .align_y(Alignment::Center),
        );
    }
    nodes = nodes.push(faint("A node prints its link and QR code when it starts. Pick a picture of the code (a screenshot or a photo) to add it.").size(12));
    titled_card("Mailboxes", Some("Nodes that hold sealed mail for you while you're away. They can't read it."), nodes)
}

/// Anonymity routing, and whether programs on this computer may act as
/// you.
fn privacy<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let anonymity = column![
        setting(
            "Anonymity mode",
            Some("Calls go through two or three relays, each knowing only the step before and after it. The strongest privacy, with noticeable delay. Needs at least two hops."),
            switch("Anonymity mode", m.network.anonymity, |on| Msg::Do(Action::Anonymity(on))),
        ),
        divider(),
        field("Hops", edit_box("Hops", "stoop-hops", "hop1.example:3479, hop2.example:3479#key", &ui.hops_edit, &m.network.hops, Msg::HopsEdit, Msg::HopsSave)),
        faint("Add #key after an address to pin the hop's key (shown when its operator starts it). Without one, the first key a hop gives is remembered and a different one is refused.").size(12.5),
    ]
    .spacing(12);
    let mut cards = column![titled_card("Anonymity", None, anonymity)].spacing(14);
    if !m.device.phone {
        let mut api = column![setting(
            "Local API",
            Some("Lets your own scripts and bots on this computer read and send messages and place calls as you. Only programs on this computer can reach it, and only with the key in the file below."),
            switch("Local API", m.network.local_api, |on| Msg::Do(Action::LocalApi(on))),
        )]
        .spacing(12);
        if m.network.local_api && !m.network.api_file.is_empty() {
            api = api.push(faint(format!("Port and key: {}", m.network.api_file)).size(12.5));
        }
        cards = cards.push(titled_card("Scripts and bots", None, api));
    }
    cards.into()
}

/// Version, and where your data lives.
fn about<'a>(m: &'a Model) -> El<'a, Msg> {
    let s = &m.settings;
    let cards = column![card(
        row![
            logo(56.0),
            column![t("Stoop").size(20).font(theme::bold()), faint(format!("Version {}", s.version)).size(12.5), muted("Calls and messages that go straight to the people you talk to.").size(13)].spacing(4),
        ]
        .spacing(18)
        .align_y(Alignment::Center)
    )]
    .spacing(14);
    cards
        .push(titled_card(
            "Your data",
            Some("Settings, contacts and history stay on this device."),
            column![container(t(s.data_dir.as_str()).size(12).font(theme::mono())).padding([8, 10]).width(Length::Fill).style(theme::inset), row![btn_icon(
                Icon::Download,
                "Open received files",
                Btn::Default,
                Some(Msg::Do(Action::OpenDownloads))
            )]]
            .spacing(10),
        ))
        .into()
}

/// Settings > Updates: the updater's card. The section is only listed
/// where this copy can update itself (see [`shown`]).
fn updates<'a>(m: &'a Model) -> El<'a, Msg> {
    super::update::card(m).unwrap_or_else(|| card(faint("This copy of Stoop is updated by the system that installed it.").size(13)).into())
}

/// Data & battery: low data calls everywhere, and on a phone, listening
/// in the background and mobile data.
fn data<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let low_data = setting(
        "Low data calls",
        Some("Voice only, at the least data that stays clear. Calls already lower their quality by themselves when the connection is poor."),
        switch("Low data calls", m.settings.low_data, |on| Msg::Do(Action::LowData(on))),
    );
    let cards = column![titled_card("Calls", None, low_data)].spacing(14);
    let Some(ph) = m.phone.as_ref() else { return cards.into() };
    let background = column![
        setting(
            "Take calls in the background",
            Some("Keeps your door open when Stoop is not on screen. Uses a little battery."),
            switch("Take calls in the background", ph.keep_listening, |on| Msg::Do(Action::KeepListening(on)))
        ),
        divider(),
        setting(
            "Even on battery saver",
            Some("Otherwise the door closes in the background while battery saver is on."),
            switch("Even on battery saver", ph.listen_on_saver, |on| Msg::Do(Action::ListenOnSaver(on)))
        ),
    ]
    .spacing(12);
    let limit_text = if ph.cellular_limit_mb == 0 { String::new() } else { ph.cellular_limit_mb.to_string() };
    let limit_value = ui.limit_edit.as_deref().unwrap_or(&limit_text);
    let mut limit = row![input("Monthly limit for files", "stoop-limit", "No limit", limit_value).on_input(Msg::LimitEdit).on_submit(Msg::LimitSave).width(110).padding([9, 12]).size(14), faint("MB").size(13)]
        .spacing(8)
        .align_y(Alignment::Center);
    if ui.limit_edit.as_deref().is_some_and(|e| e != limit_text) {
        limit = limit.push(btn("Save", Btn::Primary, Some(Msg::LimitSave)));
    }
    let data = column![
        setting("Files only on Wi-Fi", Some("Files wait for Wi-Fi instead of using mobile data."), switch("Files only on Wi-Fi", ph.wifi_only, |on| Msg::Do(Action::WifiOnly(on)))),
        divider(),
        setting("Data saver", Some("Smaller pictures and fewer checks on mobile data."), switch("Data saver", ph.data_saver, |on| Msg::Do(Action::DataSaver(on)))),
        divider(),
        setting("Monthly limit for files", Some(ph.cellular_used.as_str()), limit),
    ]
    .spacing(12);
    cards
        .push(titled_card("In the background", None, background))
        .push(titled_card("Mobile data", None, data))
        .into()
}
