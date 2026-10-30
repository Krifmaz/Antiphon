//! Profile pages, shared by the phone and a wide window: yours
//! ([`Route::Profile`]`(None)`), a contact's (`Profile(Some(id))`) and the
//! editor for yours ([`Route::ProfileEdit`]).
//!
//! Each page is a banner in the person's colour with their picture
//! hanging off its bottom edge, then their name, pronouns and status, then
//! cards. A phone draws it edge to edge, the banner running on up under
//! the status bar; a wide window draws it as a centred column in the main
//! area, the way a document sits on a desk.
//!
//! Your own page follows your [`ProfileLayout`]: which cards it shows, in
//! what order, and where your invite's QR code is placed (hidden, on the
//! banner, by your name, or as a card). The editor arranges all of it.

use iced_core::text::Wrapping;
use iced_core::{Alignment, Background, Border, Color, Length};
use iced_widget::{button, column, container, qr_code, row, scrollable, space, stack};

use crate::icons::{icon, Icon};
use crate::theme::{self, Btn};
use crate::widgets::*;
use crate::{Action, Dialog, El, Model, Msg, Person, ProfileBlock, ProfileLayout, QrPlace, Route, Ui};

/// Height of the banner across the top of a profile.
const BANNER: f32 = 112.0;

/// Size of the picture on a profile, which sits half over the banner.
const PROFILE_AVATAR: f32 = 88.0;

/// The widest a profile page is drawn in a wide window; beyond this the
/// lines of an About get too long to read and the cards too wide to scan.
const PAGE: f32 = 720.0;

/// Side of the QR code in the banner's corner, white margin included.
const QR_BANNER: f32 = 84.0;

/// Side of the QR code beside the name.
const QR_NAME: f32 = 104.0;

/// Side of the QR code drawn as a card.
const QR_CARD: f32 = 200.0;

/// The colour of someone's banner: their avatar colour, deepened so white
/// text and the back button read on it.
fn banner_color(seed: &str) -> Color {
    let base = crate::avatars::color(seed).unwrap_or_else(|| theme::avatar_color(seed));
    theme::mix(base, Color::BLACK, 0.45)
}

/// My invite as a QR code `side` pixels across, on white with a margin
/// (the quiet zone a camera needs), or nothing while the app has not built
/// it yet.
fn qr<'a>(m: &'a Model, side: f32) -> Option<El<'a, Msg>> {
    let data = m.me.qr.as_deref()?;
    // A quarter of a module per side would be too thin to be read; six
    // pixels of white keeps the code scannable at any of these sizes.
    let margin = 6.0;
    let code = qr_code(data).total_size(side - margin * 2.0).style(|_| qr_code::Style { cell: Color::from_rgb8(22, 22, 23), background: Color::WHITE });
    Some(
        container(code)
            .padding(margin)
            .style(|_| container::Style { background: Some(Background::Color(Color::WHITE)), border: Border { radius: 8.0.into(), ..Border::default() }, ..container::Style::default() })
            .into(),
    )
}

/// The top of a profile: the banner in their colour (running up under the
/// status bar on a phone), a round button on it, and their picture
/// sitting half over its bottom edge with a ring of the page's colour
/// around it.
///
/// `ui_top` is the status bar's height, which the banner runs on under;
/// `close`, when given, puts a round X on the banner that sends it;
/// `corner`, when given, is drawn in the banner's top right corner (my QR
/// code, placed there).
fn head<'a>(ui_top: f32, name: &'a str, seed: &'a str, close: Option<Msg>, corner: Option<El<'a, Msg>>) -> El<'a, Msg> {
    let p = theme::look();
    let color = banner_color(seed);
    let banner = container(space()).width(Length::Fill).height(BANNER + ui_top).style(move |_| container::Style {
        background: Some(Background::Color(color)),
        ..container::Style::default()
    });
    let back: El<'a, Msg> = match close {
        Some(close) => button(container(icon(Icon::X, 18.0, Color::WHITE)).center(36))
            .width(36)
            .height(36)
            .padding(0)
            .style(|_, _| button::Style { background: Some(Background::Color(Color::BLACK.scale_alpha(0.45))), border: Border { radius: 99.0.into(), ..Border::default() }, ..button::Style::default() })
            .on_press(close)
            .into(),
        None => space().into(),
    };
    let ring = PROFILE_AVATAR + 12.0;
    let picture = container(avatar(name, seed, PROFILE_AVATAR)).center(ring).style(move |_| container::Style {
        background: Some(Background::Color(p.panel)),
        border: Border { radius: (ring / 2.0).into(), ..Border::default() },
        ..container::Style::default()
    });
    let height = BANNER + ui_top + ring / 2.0;
    let mut layers = stack![
        column![banner],
        container(back).padding(pad(ui_top + 10.0, 0.0, 0.0, 12.0)),
        container(picture).height(height).align_y(Alignment::End).padding(pad(0.0, 0.0, 0.0, 10.0)),
    ];
    if let Some(corner) = corner {
        // Inside the banner, clear of its bottom edge so it never meets
        // the picture.
        layers = layers.push(container(corner).width(Length::Fill).align_x(Alignment::End).padding(pad(ui_top + (BANNER - QR_BANNER) / 2.0, 14.0, 0.0, 0.0)));
    }
    layers.height(height).into()
}

/// A card on a profile: a small heading, then what it says.
fn card<'a>(heading: &'a str, body: El<'a, Msg>) -> El<'a, Msg> {
    container(column![t(heading).size(12.5).font(theme::bold()).color(theme::look().muted), body].spacing(6)).padding([14, 16]).width(Length::Fill).style(theme::card_raised).into()
}

/// The name, pronouns and status under a profile's picture.
fn name_block<'a>(name: &'a str, pronouns: &'a str, status: &'a str) -> El<'a, Msg> {
    let mut col = column![t(name).size(26).font(theme::bold())].spacing(3);
    if !pronouns.is_empty() {
        col = col.push(muted(pronouns).size(14));
    }
    if !status.is_empty() {
        col = col.push(row![icon(Icon::Smile, 14.0, theme::look().faint), faint(status).size(13.5)].spacing(6).align_y(Alignment::Center));
    }
    col.into()
}

/// A page's head and body, laid out for the phone (edge to edge) or a
/// wide window (a centred column with room around it), and scrolled.
fn page<'a>(head: El<'a, Msg>, body: El<'a, Msg>, phone: bool) -> El<'a, Msg> {
    let side = if phone { 16.0 } else { 0.0 };
    let col = column![head, container(body).padding(pad(8.0, side, 24.0, side))];
    let col: El<'a, Msg> = if phone { col.into() } else { container(col.max_width(PAGE)).padding([24, 24]).center_x(Length::Fill).into() };
    scrollable(col).height(Length::Fill).style(theme::scroll).into()
}

/// The big button under a profile's name ("Edit Profile").
fn wide_button<'a>(i: Icon, label: &'a str, msg: Msg) -> El<'a, Msg> {
    button(row![icon(i, 16.0, theme::look().on_accent), t(label).size(15).font(theme::semibold())].spacing(8).align_y(Alignment::Center))
        .width(Length::Fill)
        .padding([12, 16])
        .style(|th, st| {
            let mut s = theme::btn(Btn::Primary)(th, st);
            s.border.radius = 12.0.into();
            s
        })
        .on_press(msg)
        .into()
}

/// My profile: what the people I call see of me, arranged as my
/// [`ProfileLayout`] says, with Edit Profile. `top` is how far the
/// banner runs on up under a phone's status bar.
pub fn mine<'a>(m: &'a Model, top: f32, phone: bool) -> El<'a, Msg> {
    let layout = &m.me.layout;
    let corner = if layout.qr == QrPlace::Banner { qr(m, QR_BANNER) } else { None };
    let head = head(top, &m.me.name, &m.me.id, Some(Msg::Back), corner);
    let names = name_block(&m.me.name, &m.me.pronouns, &m.me.status);
    let names: El<'a, Msg> = match (layout.qr == QrPlace::Name).then(|| qr(m, QR_NAME)).flatten() {
        Some(code) => row![container(names).width(Length::Fill), code].spacing(12).align_y(Alignment::Center).into(),
        None => names,
    };
    let mut body = column![names, wide_button(Icon::Edit, "Edit Profile", Msg::Nav(Route::ProfileEdit))].spacing(14);
    for block in layout.visible() {
        body = body.push(my_card(m, block, phone));
    }
    page(head, body.into(), phone)
}

/// One of the cards on my profile.
fn my_card<'a>(m: &'a Model, block: ProfileBlock, phone: bool) -> El<'a, Msg> {
    let title = block.title();
    match block {
        ProfileBlock::About => {
            let hint = if phone { "Nothing yet. Tap Edit Profile to say a line about yourself." } else { "Nothing yet. Click Edit Profile to say a line about yourself." };
            let bio: El<'a, Msg> = if m.me.bio.is_empty() { faint(hint).size(14).into() } else { t(m.me.bio.as_str()).size(14.5).into() };
            card(title, bio)
        }
        ProfileBlock::Door => {
            let door = if m.me.door_open { "Open: people with your invite can call you now." } else { "Closed: nobody can ring until you open it." };
            card(title, row![faint(door).size(13.5).width(Length::Fill), switch("Door open", m.me.door_open, |on| Msg::Do(Action::Door(on)))].spacing(10).align_y(Alignment::Center).into())
        }
        ProfileBlock::Invite => card(
            title,
            row![faint("Anyone with it can add you.").size(13.5).width(Length::Fill), btn_icon(Icon::Qr, "Show", Btn::Default, Some(Msg::Dialog(Some(Dialog::Invite))))]
                .spacing(10)
                .align_y(Alignment::Center)
                .into(),
        ),
        ProfileBlock::Qr => {
            let code: El<'a, Msg> = match qr(m, QR_CARD) {
                Some(code) => column![container(code).center_x(Length::Fill), container(faint("Scan it with Stoop to add me.").size(12.5)).center_x(Length::Fill)].spacing(8).into(),
                None => faint("Your code appears once your invite is ready.").size(13.5).into(),
            };
            card(title, code)
        }
        ProfileBlock::Fingerprint => card(title, t(m.me.fingerprint.as_str()).size(12.5).font(theme::mono()).color(theme::look().muted).into()),
    }
}

/// Someone else's profile: who they say they are, and the two things you
/// do with them. `top` is how far the banner runs on up under a phone's
/// status bar.
pub fn theirs<'a>(m: &'a Model, person: &'a Person, top: f32, phone: bool) -> El<'a, Msg> {
    let p = theme::look();
    let head = head(top, &person.name, &person.id, Some(Msg::Back), None);
    let can_call = !m.call.active();
    let actions = row![
        btn_icon(Icon::Message, "Message", Btn::Primary, Some(Msg::Nav(Route::Person(person.id.clone())))).width(Length::Fill),
        btn_icon(Icon::Phone, "Call", Btn::Default, can_call.then(|| Msg::Do(Action::Call(person.id.clone())))).width(Length::Fill),
    ]
    .spacing(10);
    let bio: El<'a, Msg> = if person.bio.is_empty() { faint("They haven't said anything about themselves yet.").size(14).into() } else { t(person.bio.as_str()).size(14.5).into() };
    let trust: El<'a, Msg> = if person.verified {
        row![icon(Icon::ShieldCheck, 16.0, p.ok), t("Verified: you compared the words on a call").size(13.5).color(p.ok)].spacing(8).align_y(Alignment::Center).into()
    } else {
        faint("Not verified yet. On a call, compare the words on screen to make sure it's really them.").size(13.5).into()
    };
    let mut body = column![name_block(&person.name, &person.pronouns, ""), actions, card("About me", bio), card("Trust", trust)].spacing(14);
    if !person.fingerprint.is_empty() {
        body = body.push(card("Fingerprint", t(person.fingerprint.as_str()).size(12.5).font(theme::mono()).color(p.muted).into()));
    }
    if !person.note.is_empty() {
        body = body.push(card("Note (only you see this)", t(person.note.as_str()).size(14).into()));
    }
    page(head, body.into(), phone)
}

/// Editing my profile: the picture and colour, then name, pronouns,
/// status and About in one card (saved together from the bar's Save),
/// then how my page is laid out and where my QR code goes (saved as they
/// change, since they only ever change this device's page).
pub fn edit<'a>(ui: &'a Ui, m: &'a Model, phone: bool) -> El<'a, Msg> {
    let p = theme::look();
    let bar = row![
        icon_btn(Icon::X, "Cancel", Some(Msg::Back)),
        t("Edit Profile").size(18).font(theme::bold()).width(Length::Fill),
        button(t("Save").size(15).font(theme::semibold()).color(p.accent2)).padding([8, 12]).style(|_, _| button::Style::default()).on_press(Msg::ProfileSave),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let bar = container(bar).height(56).padding([0, if phone { 8 } else { 20 }]).align_y(Alignment::Center);

    let mut picture = row![btn_icon(Icon::Edit, if m.me.has_picture { "Change picture" } else { "Add a picture" }, Btn::Default, Some(Msg::Do(Action::PickAvatar)))].spacing(8);
    if m.me.has_picture {
        picture = picture.push(btn_icon(Icon::Trash, "Remove", Btn::Ghost, Some(Msg::Do(Action::RemoveAvatar))));
    }
    // The bar above already has the way out; the banner shows the code
    // where it would go, so its place can be judged here.
    let corner = if m.me.layout.qr == QrPlace::Banner { qr(m, QR_BANNER) } else { None };
    let head = head(0.0, &m.me.name, &m.me.id, None, corner);

    // Your colour sets your banner, and the circle behind your initial
    // while you have no picture.
    let auto = theme::avatar_color(&m.me.id);
    let mut colors = row![swatch(auto, auto, m.me.color.is_empty(), "Automatic", Msg::Do(Action::ProfileColor(String::new())))].spacing(8);
    for (id, name, hex) in crate::PROFILE_COLORS {
        let c = theme::hex(hex);
        colors = colors.push(swatch(c, c, m.me.color == id, name, Msg::Do(Action::ProfileColor(id.to_string()))));
    }

    let value = |edit: &'a Option<String>, saved: &'a str| edit.as_deref().unwrap_or(saved);
    let fields = column![
        field("Display name", input("Display name", "stoop-name", "What people see", value(&ui.name_edit, &m.me.name)).on_input(Msg::NameEdit).padding([11, 12]).size(15)),
        field("Pronouns", input("Pronouns", "stoop-pronouns", "they/them, she/her…", value(&ui.pronouns_edit, &m.me.pronouns)).on_input(Msg::PronounsEdit).padding([11, 12]).size(15)),
        field("Status", input("Status", "stoop-status", "Around after six", value(&ui.status_edit, &m.me.status)).on_input(Msg::StatusEdit).padding([11, 12]).size(15)),
        field("About me", input("About me", "stoop-bio", "A line or two about you", value(&ui.bio_edit, &m.me.bio)).on_input(Msg::BioEdit).on_submit(Msg::ProfileSave).padding([11, 12]).size(15)),
        faint("Your picture, colour, pronouns and About go to the people you call. They stay on your devices and theirs.").size(12),
    ]
    .spacing(14);
    let body = column![
        picture,
        container(scrollable(colors).direction(scrollable::Direction::Horizontal(scrollable::Scrollbar::new().width(0).scroller_width(0)))).width(Length::Fill),
        container(fields).padding(16).width(Length::Fill).style(theme::card_raised),
        layout_editor(&m.me.layout),
    ]
    .spacing(16);
    column![bar, divider(), page(head, body.into(), phone)].into()
}

/// The page's arrangement: where the QR code goes, then each card with
/// its switch and the arrows that move it. Every change is a whole new
/// [`ProfileLayout`] sent at once, so there is nothing to save or lose.
fn layout_editor<'a>(layout: &'a ProfileLayout) -> El<'a, Msg> {
    let p = theme::look();
    let set = |next: ProfileLayout| Msg::Do(Action::SetProfileLayout(next));

    let mut places = row![].spacing(6);
    for place in QrPlace::ALL {
        let kind = if layout.qr == place { Btn::Primary } else { Btn::Default };
        places = places.push(btn_sm(place.label(), kind, Some(set(layout.with_qr(place)))));
    }
    let qr_part = column![
        t("Your QR code").size(14).font(theme::semibold()),
        faint("Put your invite's code on your profile, for a friend to scan from your screen. Off by default, so it is never shown by accident while you share your screen.").size(12),
        container(scrollable(places).direction(scrollable::Direction::Horizontal(scrollable::Scrollbar::new().width(0).scroller_width(0)))).width(Length::Fill),
    ]
    .spacing(6);

    let last = layout.blocks.len().saturating_sub(1);
    let mut cards = column![].spacing(4);
    for (i, (block, shown)) in layout.blocks.iter().copied().enumerate() {
        // The QR card is shown by choosing its place above, so its switch
        // would only argue with that choice; it keeps its arrows, which
        // say where the card goes.
        let control: El<'a, Msg> = if block == ProfileBlock::Qr {
            faint(if layout.qr == QrPlace::Card { "Shown" } else { "Placed above" }).size(12).into()
        } else {
            switch(block.title(), shown, move |on| set(layout.with_shown(block, on)))
        };
        let dim = block != ProfileBlock::Qr && !shown || block == ProfileBlock::Qr && layout.qr != QrPlace::Card;
        let entry = row![
            t(block.title()).size(14).color(if dim { p.faint } else { p.text }).width(Length::Fill).wrapping(Wrapping::None),
            control,
            icon_btn(Icon::Up, "Move up", (i > 0).then(|| set(layout.moved(block, true)))),
            icon_btn(Icon::Down, "Move down", (i < last).then(|| set(layout.moved(block, false)))),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        cards = cards.push(container(entry).padding([4, 0]));
    }
    let cards_part = column![
        t("Cards on your profile").size(14).font(theme::semibold()),
        faint("Show, hide and order what your profile page shows. This arranges your page on this device; contacts only ever get your card.").size(12),
        cards,
    ]
    .spacing(6);
    container(column![qr_part, divider(), cards_part].spacing(14)).padding(16).width(Length::Fill).style(theme::card_raised).into()
}
