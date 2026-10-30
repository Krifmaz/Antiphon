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
//!   Drawn by [`profile`], which a wide window shares.
//!
//! The system's status bar and gesture bar are kept clear of every
//! control, with the app's colours running on underneath them, so the app
//! reaches the edges of the glass without anything under a clock or a
//! home bar.

use iced_core::text::Wrapping;
use iced_core::{Alignment, Background, Border, Color, Length};
use iced_widget::{button, column, container, row, scrollable, space, stack};

use super::{call, clip, community, conversation, fill, presence_color, profile, settings, sidebar, update};
use crate::icons::{icon, Icon};
use crate::theme::{self, Btn};
use crate::widgets::*;
use crate::{Action, Dialog, El, Model, Msg, Person, Route, Ui};

/// Width of the rail of places down the left edge.
const RAIL: f32 = 68.0;

/// Size of a place's tile on the rail.
const TILE: f32 = 48.0;

/// The whole phone layout: the current screen, with the call pinned as a
/// strip while you look at something else.
pub fn view<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let (top, bottom) = ui.insets;
    let on_call_screen = matches!((&ui.route, &m.call.peer), (Route::Person(id), Some(peer)) if id == peer);
    let call_strip = m.call.active() && !on_call_screen;
    // A profile's banner runs on up under the clock, as a photo would,
    // unless a notice, a newer version or the call's strip has to sit up
    // there first.
    let banner_on_top = matches!(ui.route, Route::Profile(_)) && !call_strip && m.notice.is_none() && !update::shown(m);
    let banner_top = if banner_on_top { top } else { 0.0 };
    let screen: El<'a, Msg> = match &ui.route {
        Route::Person(id) => match m.people.iter().find(|p| &p.id == id) {
            Some(person) => chat(ui, m, person),
            None => home(ui, m, messages_pane(ui, m)),
        },
        Route::SettingsMenu => column![top_bar("Settings", true), divider(), settings::menu(m)].into(),
        Route::Settings(section) => column![top_bar(section.label(), true), divider(), settings::page(ui, m, *section)].into(),
        Route::Spaces => home(ui, m, community::list(ui, m, true)),
        Route::Community(id, channel) => community::phone(ui, m, id, *channel),
        Route::Profile(None) => profile::mine(m, banner_top, true),
        Route::Profile(Some(id)) => match m.people.iter().find(|p| &p.id == id) {
            Some(person) => profile::theirs(m, person, banner_top, true),
            None => home(ui, m, messages_pane(ui, m)),
        },
        Route::ProfileEdit => profile::edit(ui, m, true),
        Route::Home => home(ui, m, messages_pane(ui, m)),
    };
    let mut col = column![];
    if !banner_on_top {
        col = col.push(container(space().height(top)).width(Length::Fill).style(theme::sidebar));
    }
    if let Some(n) = m.notice.as_deref() {
        col = col.push(warn_box(t(n).size(13)));
    }
    if let Some(b) = update::banner(m, true) {
        col = col.push(b);
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
        icon_btn(Icon::Gear, "Settings", Some(Msg::Nav(Route::SettingsMenu))),
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
