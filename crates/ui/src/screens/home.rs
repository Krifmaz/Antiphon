//! The front step: your door, ways to bring people in, and the people you
//! talk to most.

use iced_core::text::Wrapping;
use iced_core::{Alignment, Background, Border, Length};
use iced_widget::{button, column, container, row, scrollable, space};

use super::{call, clip, fill};
use crate::icons::{icon, Icon};
use crate::theme::{self, Btn};
use crate::widgets::*;
use crate::{Action, CheckState, Dialog, El, Model, Msg, Route, Section, Ui};

/// The home screen: greeting, door switch, invite and add cards, and recent people.
pub fn view<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let mut col = column![].spacing(22).max_width(940);
    let hello = if m.me.greeting.is_empty() { format!("Hello, {}", m.me.name) } else { format!("{}, {}", m.me.greeting, m.me.name) };
    col = col.push(column![h1(hello), muted("Your stoop is where people find you. Open the door and they can call.").size(14)].spacing(6));
    if m.call.active() && m.call.peer.is_none() {
        col = col.push(container(call::stage(ui, m, false)).clip(true).style(theme::card));
    }
    col = col.push(door_card(m, false));
    col = col.push(starts(m, false));
    col = col.push(people(m));
    scrollable(container(col).padding([34, 40]).width(Length::Fill).center_x(Length::Fill)).height(Length::Fill).style(theme::scroll).into()
}

/// The door: open means listening for calls.
pub fn door_card<'a>(m: &'a Model, compact: bool) -> El<'a, Msg> {
    let p = theme::look();
    let open = m.me.door_open;
    let tint = if open { p.ok } else { p.faint };
    let badge_box = container(icon(if open { Icon::DoorOpen } else { Icon::Door }, if compact { 20.0 } else { 26.0 }, tint))
        .center(if compact { 44 } else { 56 })
        .style(move |_| container::Style {
            background: Some(Background::Color(tint.scale_alpha(0.12))),
            border: Border { color: tint.scale_alpha(0.35), width: 1.0, radius: 16.0.into() },
            ..container::Style::default()
        });
    let (title, line) = if open {
        ("Your door is open", "People with your invite can call you now.")
    } else {
        ("Your door is closed", "Open it to take calls. Nobody can ring while it's closed.")
    };
    let mut words = column![t(title).size(if compact { 15 } else { 17 }).font(theme::bold()), muted(line).size(13)].spacing(3).width(Length::Fill);
    if open {
        let mut addr = row![].spacing(10);
        if let Some(l) = m.me.local_addr.as_deref() {
            addr = addr.push(faint(format!("On this network: {l}")).size(12));
        }
        if let Some(pb) = m.me.public_addr.as_deref() {
            addr = addr.push(faint(format!("Internet: {pb}")).size(12));
        }
        words = words.push(addr);
    }
    let r = row![badge_box, words, switch(open, |on| Msg::Do(Action::Door(on)))].spacing(16).align_y(Alignment::Center);
    container(r)
        .padding(if compact { 14 } else { 20 })
        .width(Length::Fill)
        .style(move |_| {
            let mut s = theme::card_raised(&iced_core::Theme::Dark);
            if open {
                s.border.color = p.ok.scale_alpha(0.35);
            }
            s
        })
        .into()
}

/// The three ways to get started.
pub fn starts<'a>(m: &'a Model, stacked: bool) -> El<'a, Msg> {
    let p = theme::look();
    let worst = m.network.checks.iter().map(|c| c.state).fold(CheckState::Good, |a, b| match (a, b) {
        (CheckState::Bad, _) | (_, CheckState::Bad) => CheckState::Bad,
        (CheckState::Warn, _) | (_, CheckState::Warn) => CheckState::Warn,
        (CheckState::Unknown, _) | (_, CheckState::Unknown) => CheckState::Unknown,
        _ => CheckState::Good,
    });
    let net_line = match (m.network.checks.is_empty(), worst) {
        (true, _) | (_, CheckState::Unknown) => "See whether people can reach you",
        (_, CheckState::Good) => "Everything looks reachable",
        (_, CheckState::Warn) => "Reachable, with a catch",
        (_, CheckState::Bad) => "People may not reach you",
    };
    let tile = |i: Icon, title: &'a str, line: &'a str, msg: Msg| -> El<'a, Msg> {
        button(
            column![
                container(icon(i, 20.0, p.accent2)).center(40).style(theme::inset),
                column![t(title).size(14.5).font(theme::semibold()), faint(line).size(12.5)].spacing(3),
            ]
            .spacing(14),
        )
        .width(Length::Fill)
        .padding(18)
        .style(move |th, st| {
            let mut s = theme::btn(Btn::Default)(th, st);
            s.border.radius = theme::RADIUS.into();
            s
        })
        .on_press(msg)
        .into()
    };
    let a = tile(Icon::Qr, "Share your invite", "A link or QR code anyone can add you from", Msg::Dialog(Some(Dialog::Invite)));
    let b = tile(Icon::UserPlus, "Add someone", "Paste the invite they sent you", Msg::Dialog(Some(Dialog::AddPerson)));
    let c = tile(Icon::Wifi, "Check your connection", net_line, Msg::Nav(Route::Settings(Section::Network)));
    if stacked {
        column![a, b, c].spacing(10).into()
    } else {
        row![a, b, c].spacing(14).into()
    }
}

/// Recent people as cards.
fn people<'a>(m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    if m.people.is_empty() {
        return container(empty(Icon::Users, "Nobody on your stoop yet", "Send someone your invite, or add them from theirs. Once they are here, a call is one click."))
            .padding(30)
            .width(Length::Fill)
            .center_x(Length::Fill)
            .into();
    }
    let mut grid = row![].spacing(12);
    for person in m.people.iter().take(4) {
        let line = person.last.as_deref().map(|s| clip(s, 26)).unwrap_or_else(|| "No messages yet".into());
        let card = button(
            column![
                row![avatar_dot(&person.name, &person.id, 40.0, super::presence_color(person.presence), p.panel2), fill(), {
                    let e: El<'a, Msg> = if person.unread > 0 {
                        container(t(person.unread.to_string()).size(11).font(theme::bold()).color(theme::hex("#16161f"))).padding([1, 7]).style(theme::unread).into()
                    } else {
                        space().into()
                    };
                    e
                }]
                .align_y(Alignment::Center),
                column![t(clip(&person.name, 18)).size(14).font(theme::semibold()).wrapping(Wrapping::None), faint(line).size(12).wrapping(Wrapping::None)].spacing(2),
            ]
            .spacing(12),
        )
        .width(Length::Fill)
        .padding(16)
        .style(move |th, st| {
            let mut s = theme::btn(Btn::Default)(th, st);
            s.border.radius = theme::RADIUS.into();
            s
        })
        .on_press(Msg::Nav(Route::Person(person.id.clone())));
        grid = grid.push(iced_widget::mouse_area(card).on_right_press(Msg::Menu(Some(person.id.clone()))));
    }
    for _ in m.people.len()..4 {
        grid = grid.push(space().width(Length::Fill));
    }
    column![row![h2("People"), fill(), btn_icon(Icon::UserPlus, "Add someone", Btn::Ghost, Some(Msg::Dialog(Some(Dialog::AddPerson))))].align_y(Alignment::Center), grid]
        .spacing(12)
        .into()
}
