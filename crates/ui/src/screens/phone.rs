//! The narrow layout a phone gets, laid out the way people already know
//! from the big chat apps: a rail of places down the left edge, your
//! conversations beside it, and yourself in a bar along the bottom.
//!
//! - **Home** ([`Route::Home`]): the rail (messages, each group chat or
//!   community, adding someone, your invite), "Messages" with search and
//!   an add button, then one row per conversation, newest first. Nobody
//!   yet shows the ways to bring someone in, in the same place.
//! - **A conversation** ([`Route::Person`]): back, who, call; the messages
//!   flat down the left with avatars, replies and link cards; the
//!   composer as one rounded bar above the gesture bar.
//! - **A profile** ([`Route::Profile`]): banner, picture, name, pronouns,
//!   status and About, with Edit Profile for your own
//!   ([`Route::ProfileEdit`]) and Message and Call for someone else's.
//!
//! The system's status bar and gesture bar are kept clear of every
//! control, with the app's colours running on underneath them, so the app
//! reaches the edges of the glass without anything under a clock or a
//! home bar.

use iced_core::text::Wrapping;
use iced_core::{Alignment, Background, Border, Color, Length};
use iced_widget::{button, column, container, row, scrollable, space, stack};

use super::{call, clip, community, conversation, fill, presence_color, settings, sidebar};
use crate::icons::{icon, Icon};
use crate::theme::{self, Btn};
use crate::widgets::*;
use crate::{Action, Dialog, El, Model, Msg, Person, Route, Section, Ui};

/// Width of the rail of places down the left edge.
const RAIL: f32 = 68.0;

/// Size of a place's tile on the rail.
const TILE: f32 = 48.0;

/// Height of the banner across the top of a profile.
const BANNER: f32 = 112.0;

/// Size of the picture on a profile, which sits half over the banner.
const PROFILE_AVATAR: f32 = 88.0;

/// The whole phone layout: the current screen, with the call pinned as a
/// strip while you look at something else.
pub fn view<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let (top, bottom) = ui.insets;
    let on_call_screen = matches!((&ui.route, &m.call.peer), (Route::Person(id), Some(peer)) if id == peer);
    let call_strip = m.call.active() && !on_call_screen;
    // A profile's banner runs on up under the clock, as a photo would,
    // unless a notice or the call's strip has to sit up there first.
    let banner_on_top = matches!(ui.route, Route::Profile(_)) && !call_strip && m.notice.is_none();
    let banner_top = if banner_on_top { top } else { 0.0 };
    let screen: El<'a, Msg> = match &ui.route {
        Route::Person(id) => match m.people.iter().find(|p| &p.id == id) {
            Some(person) => chat(ui, m, person),
            None => home(ui, m, messages_pane(ui, m)),
        },
        Route::Settings(_) => column![top_bar("Settings", true), divider(), settings::stacked(ui, m)].into(),
        Route::Spaces => home(ui, m, community::list(ui, m, true)),
        Route::Community(id, channel) => community::phone(ui, m, id, *channel),
        Route::Profile(None) => my_profile(m, banner_top),
        Route::Profile(Some(id)) => match m.people.iter().find(|p| &p.id == id) {
            Some(person) => their_profile(m, person, banner_top),
            None => home(ui, m, messages_pane(ui, m)),
        },
        Route::ProfileEdit => edit_profile(ui, m),
        Route::Home => home(ui, m, messages_pane(ui, m)),
    };
    let mut col = column![];
    if !banner_on_top {
        col = col.push(container(space().height(top)).width(Length::Fill).style(theme::sidebar));
    }
    if let Some(n) = m.notice.as_deref() {
        col = col.push(warn_box(t(n).size(13)));
    }
    if call_strip {
        col = col.push(container(sidebar::call_strip(m)).padding([8, 10]));
    }
    col = col.push(container(screen).height(Length::Fill).style(theme::stage_flat));
    // The gesture bar's (or keyboard's) strip, in the colour of whatever
    // sits just above it: the "you" bar on the home screens, the screen
    // itself everywhere else.
    let ground = if matches!(ui.route, Route::Home | Route::Spaces) { theme::sidebar } else { theme::stage_flat };
    col = col.push(container(space()).height(bottom).width(Length::Fill).style(ground));
    col.height(Length::Fill).into()
}

/// The home screens: the rail on the left, `pane` beside it, and the
/// "you" bar along the bottom.
fn home<'a>(ui: &'a Ui, m: &'a Model, pane: El<'a, Msg>) -> El<'a, Msg> {
    let body = row![rail(ui, m), container(pane).width(Length::Fill).height(Length::Fill).style(pane_style)].height(Length::Fill);
    column![body, you_bar(m)].into()
}

/// The pane beside the rail: the panel colour, rounded where it meets the
/// rail, like a sheet laid over the ground.
fn pane_style(_: &iced_core::Theme) -> container::Style {
    let p = theme::look();
    container::Style {
        text_color: Some(p.text),
        background: Some(Background::Color(p.panel)),
        border: Border { color: p.border, width: 1.0, radius: iced_core::border::Radius::new(0.0).top_left(18.0) },
        ..container::Style::default()
    }
}

/// The rail of places: messages at the top, then each group chat and
/// community, then adding someone and your invite.
fn rail<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let on_home = matches!(ui.route, Route::Home);
    let mut col = column![
        rail_tile(icon(Icon::Message, 22.0, if on_home { p.on_accent } else { p.text }), "Messages", on_home, Msg::Nav(Route::Home)),
        container(divider()).width(32).padding([2, 0]),
    ]
    .spacing(10)
    .align_x(Alignment::Center);
    if m.device.communities {
        for space in &m.communities {
            let on = matches!(&ui.route, Route::Community(id, _) if id == &space.id);
            let face: El<'a, Msg> = t(space_initials(&space.name)).size(15).font(theme::bold()).color(if on { p.on_accent } else { p.text }).into();
            let tile = rail_tile(face, &space.name, on, Msg::Nav(Route::Community(space.id.clone(), 0)));
            col = col.push(with_badge(tile, space.unread));
        }
        let on_spaces = matches!(ui.route, Route::Spaces);
        col = col.push(rail_tile(icon(Icon::Spaces, 20.0, if on_spaces { p.on_accent } else { p.ok }), "Group chats and communities", on_spaces, Msg::Nav(Route::Spaces)));
    }
    col = col
        .push(rail_tile(icon(Icon::UserPlus, 20.0, p.ok), "Add someone", ui.dialog == Some(Dialog::AddPerson), Msg::Dialog(Some(Dialog::AddPerson))))
        .push(rail_tile(icon(Icon::Qr, 20.0, p.ok), "Your invite", ui.dialog == Some(Dialog::Invite), Msg::Dialog(Some(Dialog::Invite))));
    container(scrollable(container(col).padding([10, 0]).width(RAIL).center_x(RAIL)).height(Length::Fill).style(theme::scroll))
        .width(RAIL)
        .height(Length::Fill)
        .style(theme::sidebar)
        .into()
}

/// One place on the rail: a rounded square when it is the one open, a
/// circle otherwise, as the big chat apps draw it; the open one is in the
/// accent colour with a pill on the rail's edge beside it.
fn rail_tile<'a>(face: El<'a, Msg>, name: &str, on: bool, msg: Msg) -> El<'a, Msg> {
    let p = theme::look();
    let focused = crate::a11y::Spec::new(crate::a11y::Role::Tab, name).selected(on).on(Some(&msg)).add();
    let bg = if on { p.accent } else { p.panel2 };
    let radius = if on { 16.0 } else { TILE / 2.0 };
    let tile = button(crate::a11y::mark(container(face).center(TILE), focused))
        .width(TILE)
        .height(TILE)
        .padding(0)
        .style(focus_style(
            move |_, status| {
                let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
                button::Style {
                    background: Some(Background::Color(if hovered && !on { theme::mix(p.panel2, p.accent, 0.25) } else { bg })),
                    border: Border { radius: radius.into(), ..Border::default() },
                    text_color: p.text,
                    ..button::Style::default()
                }
            },
            focused,
        ))
        .on_press(msg);
    // The pill on the rail's edge that marks the open place.
    let pill: El<'a, Msg> = container(space()).width(4).height(if on { 36 } else { 0 }).style(move |_| container::Style {
        background: Some(Background::Color(p.text)),
        border: Border { radius: iced_core::border::Radius::new(0.0).top_right(4.0).bottom_right(4.0), ..Border::default() },
        ..container::Style::default()
    }).into();
    row![pill, space().width(RAIL / 2.0 - TILE / 2.0 - 4.0), tile, fill()].align_y(Alignment::Center).width(RAIL).into()
}

/// `tile` with a red count on its lower right when `count` is above zero.
fn with_badge<'a>(tile: El<'a, Msg>, count: u32) -> El<'a, Msg> {
    if count == 0 {
        return tile;
    }
    let p = theme::look();
    let n = if count > 99 { "99+".to_string() } else { count.to_string() };
    let badge = container(t(n).size(11).font(theme::bold()).color(Color::WHITE)).padding([1, 6]).style(move |_| container::Style {
        background: Some(Background::Color(p.err)),
        border: Border { color: p.bg2, width: 2.5, radius: 99.0.into() },
        ..container::Style::default()
    });
    stack![tile, container(badge).width(RAIL - 6.0).height(TILE + 4.0).align_x(Alignment::End).align_y(Alignment::End)].into()
}

/// Up to two letters for a place with no picture: the first of each of
/// its first two words ("Band Practice" is "BP").
fn space_initials(name: &str) -> String {
    let letters: String = name.split_whitespace().filter_map(|w| w.chars().next()).take(2).collect();
    if letters.is_empty() {
        "?".into()
    } else {
        letters.to_uppercase()
    }
}

/// The pane of conversations: the title, search with the add button
/// beside it, then everyone, newest first.
fn messages_pane<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let head = column![
        t("Messages").size(22).font(theme::bold()),
        row![
            container(sidebar::search(ui)).width(Length::Fill),
            button(container(icon(Icon::Plus, 20.0, p.on_accent)).center(40))
                .width(40)
                .height(40)
                .padding(0)
                .style(|th, st| {
                    let mut s = theme::btn(Btn::Primary)(th, st);
                    s.border.radius = 12.0.into();
                    s
                })
                .on_press(Msg::Dialog(Some(Dialog::AddPerson))),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    ]
    .spacing(12)
    .padding(pad(16.0, 12.0, 8.0, 14.0));

    let body: El<'a, Msg> = if m.people.is_empty() {
        get_started()
    } else {
        let query = ui.search.trim().to_lowercase();
        let mut list = column![].spacing(2).padding(pad(0.0, 6.0, 12.0, 6.0));
        for person in m.people.iter().filter(|x| query.is_empty() || x.name.to_lowercase().contains(&query)) {
            list = list.push(dm_row(m, person));
        }
        scrollable(list).height(Length::Fill).style(theme::scroll).into()
    };
    column![head, body].height(Length::Fill).into()
}

/// What the messages pane shows before anyone is in it: what to do next,
/// as two big buttons, rather than an empty list.
fn get_started<'a>() -> El<'a, Msg> {
    let p = theme::look();
    let card = column![
        container(icon(Icon::Users, 30.0, p.accent2)).center(64).style(theme::inset),
        t("No messages yet").size(17).font(theme::bold()),
        muted("Add someone from the invite they sent you, or send them yours. They show up here, and a call is one tap.").size(13.5),
        btn_icon(Icon::UserPlus, "Add someone", Btn::Primary, Some(Msg::Dialog(Some(Dialog::AddPerson)))).width(Length::Fill),
        btn_icon(Icon::Qr, "Share your invite", Btn::Default, Some(Msg::Dialog(Some(Dialog::Invite)))).width(Length::Fill),
    ]
    .spacing(12)
    .align_x(Alignment::Center);
    container(container(card).padding(20).width(Length::Fill).style(theme::card_raised)).padding([12, 12]).into()
}

/// One conversation in the list: their picture with a presence dot, their
/// name, when the last message was, and the message itself.
fn dm_row<'a>(m: &'a Model, person: &'a Person) -> El<'a, Msg> {
    let p = theme::look();
    let on_call = m.call.with(&person.id);
    let unread = person.unread > 0;
    let dot = if on_call { Some(p.ok) } else { presence_color(person.presence) };
    let name = t(clip(&person.name, 26)).size(15.5).font(if unread { theme::bold() } else { theme::semibold() }).color(if unread { p.text } else { theme::mix(p.muted, p.text, 0.5) }).wrapping(Wrapping::None);
    let mut top = row![container(name).width(Length::Fill).clip(true)].spacing(8).align_y(Alignment::Center);
    if let Some(at) = person.last_at.as_deref() {
        top = top.push(faint(at).size(12));
    }
    let sub: El<'a, Msg> = if on_call {
        row![icon(Icon::Phone, 13.0, p.ok), t(if m.call.phase == crate::Phase::Dialing { "Calling…" } else { "On a call" }).size(13.5).color(p.ok)]
            .spacing(5)
            .align_y(Alignment::Center)
            .into()
    } else {
        let line = person.last.as_deref().map(|s| clip(s, 40)).unwrap_or_else(|| "Say hello".into());
        t(line).size(13.5).color(if unread { p.muted } else { p.faint }).wrapping(Wrapping::None).into()
    };
    let mut bottom = row![container(sub).width(Length::Fill).clip(true)].spacing(8).align_y(Alignment::Center);
    if unread {
        bottom = bottom.push(container(t(person.unread.to_string()).size(11).font(theme::bold()).color(Color::WHITE)).padding([1, 7]).style(move |_| container::Style {
            background: Some(Background::Color(p.err)),
            border: Border { radius: 99.0.into(), ..Border::default() },
            ..container::Style::default()
        }));
    }
    let body = row![avatar_dot(&person.name, &person.id, 44.0, dot, p.panel), column![top, bottom].spacing(3).width(Length::Fill)].spacing(12).align_y(Alignment::Center);
    let mut said = person.name.clone();
    if unread {
        said.push_str(&format!(", {} unread", person.unread));
    }
    let open = Msg::Nav(Route::Person(person.id.clone()));
    let menu = Msg::Menu(Some(person.id.clone()));
    let focused = crate::a11y::Spec::new(crate::a11y::Role::Tab, said).on(Some(&open)).menu(Some(&menu)).add();
    let row = button(crate::a11y::mark(container(body).clip(true), focused))
        .width(Length::Fill)
        .padding([9, 10])
        .style(focus_style(
            |th, st| {
                let mut s = theme::btn(Btn::Row)(th, st);
                s.border.radius = 12.0.into();
                s
            },
            focused,
        ))
        .on_press(open);
    iced_widget::mouse_area(row).on_right_press(Msg::Menu(Some(person.id.clone()))).into()
}

/// You, along the bottom of the home screens: your picture and name (a tap
/// opens your profile), whether your door is open, the door switch, and
/// Settings.
fn you_bar<'a>(m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let open = m.me.door_open;
    let dot = if open { p.ok } else { p.faint };
    let line = if open { "Door open · taking calls" } else { "Door closed" };
    let me = row![
        avatar_dot(&m.me.name, &m.me.id, 40.0, Some(dot), p.panel2),
        column![
            row![t(clip(&m.me.name, 18)).size(15).font(theme::bold()).wrapping(Wrapping::None), icon(Icon::Down, 13.0, p.muted)].spacing(4).align_y(Alignment::Center),
            t(line).size(12.5).color(if open { p.ok } else { p.faint }).wrapping(Wrapping::None),
        ]
        .spacing(1),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    let me = pressable(me, crate::a11y::Role::Button, "Your profile", false, Btn::Row, Msg::Nav(Route::Profile(None))).padding([4, 6]).width(Length::Fill);
    let r = row![
        me,
        switch("Door open", open, |on| Msg::Do(Action::Door(on))),
        icon_btn(Icon::Gear, "Settings", Some(Msg::Nav(Route::Settings(Section::Profile)))),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    let bar = container(r).padding([6, 8]).width(Length::Fill).style(move |_| container::Style {
        background: Some(Background::Color(p.panel2)),
        border: Border { color: p.border, width: 1.0, radius: 18.0.into() },
        ..container::Style::default()
    });
    container(bar).padding([8, 10]).width(Length::Fill).style(theme::sidebar).into()
}

/// A conversation: the header, the call stage while on a call with them,
/// the messages, files on their way, and the composer.
fn chat<'a>(ui: &'a Ui, m: &'a Model, person: &'a Person) -> El<'a, Msg> {
    let mut col = column![conversation::header(ui, m, person, true), divider()];
    if m.call.with(&person.id) {
        col = col.push(call::stage(ui, m, true));
    }
    col = col.push(container(conversation::messages(ui, m, person, true)).height(Length::Fill));
    col = col.push(conversation::transfers(ui, m));
    col = col.push(container(conversation::composer(ui, m, person)).padding(pad(4.0, 10.0, 10.0, 10.0)));
    col.into()
}

/// A plain bar with a title and, below the top level, a back arrow.
fn top_bar<'a>(title: &'a str, back: bool) -> El<'a, Msg> {
    let mut r = row![].spacing(8).align_y(Alignment::Center);
    if back {
        r = r.push(icon_btn(Icon::Back, "", Some(Msg::Back)));
    }
    r = r.push(t(title).size(17).font(theme::bold()));
    container(r).height(56).padding([0, 10]).align_y(Alignment::Center).into()
}

/// The colour of someone's banner: their avatar colour, deepened so white
/// text and the back button read on it.
fn banner_color(seed: &str) -> Color {
    let base = crate::avatars::color(seed).unwrap_or_else(|| theme::avatar_color(seed));
    theme::mix(base, Color::BLACK, 0.45)
}

/// The top of a profile: the banner in their colour (running up under the
/// status bar), a round back button on it, and their picture sitting half
/// over its bottom edge with a ring of the page's colour around it.
/// `ui_top` is the status bar's height, which the banner runs on under;
/// `close`, when given, puts a round X on the banner that sends it.
fn profile_head<'a>(ui_top: f32, name: &'a str, seed: &'a str, close: Option<Msg>) -> El<'a, Msg> {
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
    stack![
        column![banner],
        container(back).padding(pad(ui_top + 10.0, 0.0, 0.0, 12.0)),
        container(picture).height(height).align_y(Alignment::End).padding(pad(0.0, 0.0, 0.0, 10.0)),
    ]
    .height(height)
    .into()
}

/// A card on a profile: a small heading, then what it says.
fn profile_card<'a>(heading: &'a str, body: El<'a, Msg>) -> El<'a, Msg> {
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

/// My profile: what the people I call see of me, with Edit Profile.
/// `top` is how far the banner runs on up under the status bar.
fn my_profile<'a>(m: &'a Model, top: f32) -> El<'a, Msg> {
    let head = profile_head(top, &m.me.name, &m.me.id, Some(Msg::Back));
    let edit = button(
        row![icon(Icon::Edit, 16.0, theme::look().on_accent), t("Edit Profile").size(15).font(theme::semibold())].spacing(8).align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .padding([12, 16])
    .style(|th, st| {
        let mut s = theme::btn(Btn::Primary)(th, st);
        s.border.radius = 12.0.into();
        s
    })
    .on_press(Msg::Nav(Route::ProfileEdit));
    let bio: El<'a, Msg> = if m.me.bio.is_empty() { faint("Nothing yet. Tap Edit Profile to say a line about yourself.").size(14).into() } else { t(m.me.bio.as_str()).size(14.5).into() };
    let door = if m.me.door_open { "Open: people with your invite can call you now." } else { "Closed: nobody can ring until you open it." };
    let body = column![
        name_block(&m.me.name, &m.me.pronouns, &m.me.status),
        edit,
        profile_card("About me", bio),
        profile_card("Your door", row![faint(door).size(13.5).width(Length::Fill), switch("Door open", m.me.door_open, |on| Msg::Do(Action::Door(on)))].spacing(10).align_y(Alignment::Center).into()),
        profile_card(
            "Your invite",
            row![
                faint("Anyone with it can add you.").size(13.5).width(Length::Fill),
                btn_icon(Icon::Qr, "Show", Btn::Default, Some(Msg::Dialog(Some(Dialog::Invite)))),
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .into()
        ),
        profile_card("Fingerprint", t(m.me.fingerprint.as_str()).size(12.5).font(theme::mono()).color(theme::look().muted).into()),
    ]
    .spacing(14)
    .padding(pad(8.0, 16.0, 24.0, 16.0));
    scrollable(column![head, body]).height(Length::Fill).style(theme::scroll).into()
}

/// Someone else's profile: who they say they are, and the two things you
/// do with them.
/// `top` is how far the banner runs on up under the status bar.
fn their_profile<'a>(m: &'a Model, person: &'a Person, top: f32) -> El<'a, Msg> {
    let p = theme::look();
    let head = profile_head(top, &person.name, &person.id, Some(Msg::Back));
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
    let mut body = column![name_block(&person.name, &person.pronouns, ""), actions, profile_card("About me", bio), profile_card("Trust", trust)].spacing(14);
    if !person.fingerprint.is_empty() {
        body = body.push(profile_card("Fingerprint", t(person.fingerprint.as_str()).size(12.5).font(theme::mono()).color(p.muted).into()));
    }
    if !person.note.is_empty() {
        body = body.push(profile_card("Note (only you see this)", t(person.note.as_str()).size(14).into()));
    }
    scrollable(column![head, container(body).padding(pad(8.0, 16.0, 24.0, 16.0))]).height(Length::Fill).style(theme::scroll).into()
}

/// Editing my profile: the picture and colour, then name, pronouns, status
/// and About in one card, saved together from the bar's Save.
fn edit_profile<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let bar = row![
        icon_btn(Icon::X, "", Some(Msg::Back)),
        t("Profile").size(18).font(theme::bold()).width(Length::Fill),
        button(t("Save").size(15).font(theme::semibold()).color(p.accent2)).padding([8, 12]).style(|_, _| button::Style::default()).on_press(Msg::ProfileSave),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let bar = container(bar).height(56).padding([0, 8]).align_y(Alignment::Center);

    let mut picture = row![btn_icon(Icon::Edit, if m.me.has_picture { "Change picture" } else { "Add a picture" }, Btn::Default, Some(Msg::Do(Action::PickAvatar)))].spacing(8);
    if m.me.has_picture {
        picture = picture.push(btn_icon(Icon::Trash, "Remove", Btn::Ghost, Some(Msg::Do(Action::RemoveAvatar))));
    }
    // The bar above already has the way out.
    let head = profile_head(0.0, &m.me.name, &m.me.id, None);

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
    ]
    .spacing(16)
    .padding(pad(8.0, 16.0, 24.0, 16.0));
    column![bar, divider(), scrollable(column![head, body]).height(Length::Fill).style(theme::scroll)].into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_get_up_to_two_initials() {
        assert_eq!(space_initials("Band Practice"), "BP");
        assert_eq!(space_initials("porch club of friends"), "PC");
        assert_eq!(space_initials("solo"), "S");
        assert_eq!(space_initials("   "), "?");
    }
}
