//! The people list on the left of a wide window: the brand, search, the
//! front step, everyone you know, the call you are on, and you.

use iced_core::text::Wrapping;
use iced_core::{Alignment, Length};
use iced_widget::{button, column, container, row, scrollable, space, text_input};

use super::{clip, clock, fill, presence_color};
use crate::icons::{icon, logo, Icon};
use crate::theme::{self, Btn};
use crate::widgets::*;
use crate::{Action, Dialog, El, Model, Msg, Person, Route, Section, Ui};

pub fn view<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let brand = row![
        logo(28.0),
        t("Stoop").size(17).font(theme::bold()),
        fill(),
        icon_btn(Icon::Qr, "Your invite", Some(Msg::Dialog(Some(Dialog::Invite)))),
        icon_btn(Icon::UserPlus, "Add someone", Some(Msg::Dialog(Some(Dialog::AddPerson)))),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let home_on = ui.route == Route::Home;
    let home = button(
        row![icon(Icon::DoorOpen, 18.0, if home_on { p.text } else { p.muted }), t("Front step").size(14).font(theme::semibold())]
            .spacing(12)
            .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .padding([9, 12])
    .style(theme::btn(if home_on { Btn::RowOn } else { Btn::Row }))
    .on_press(Msg::Nav(Route::Home));

    let query = ui.search.trim().to_lowercase();
    let shown: Vec<&Person> = m.people.iter().filter(|x| query.is_empty() || x.name.to_lowercase().contains(&query)).collect();
    let count = if query.is_empty() { format!("{}", m.people.len()) } else { format!("{} of {}", shown.len(), m.people.len()) };
    let head = row![label("People"), faint(count).size(11), fill(), icon_btn(Icon::Plus, "Add someone", Some(Msg::Dialog(Some(Dialog::AddPerson))))]
        .spacing(8)
        .align_y(Alignment::Center);

    let mut list = column![].spacing(2);
    for person in &shown {
        list = list.push(person_row(ui, m, person));
    }
    if m.people.is_empty() {
        list = list.push(container(faint("Nobody yet. Share your invite, or add someone from theirs.").size(12.5)).padding([8, 12]));
    } else if shown.is_empty() {
        list = list.push(container(faint("No one by that name.").size(12.5)).padding([8, 12]));
    }

    let mut col = column![
        container(brand).padding(pad(16.0, 12.0, 10.0, 16.0)),
        container(search(ui)).padding([0, 12]),
        container(home).padding(pad(10.0, 8.0, 4.0, 8.0)),
        container(head).padding(pad(10.0, 8.0, 2.0, 18.0)),
        scrollable(container(list).padding([0, 8])).height(Length::Fill).style(theme::scroll),
    ];
    if m.call.active() && !on_call_screen(ui, m) {
        col = col.push(container(call_strip(m)).padding([6, 8]));
    }
    col = col.push(me_bar(ui, m));
    container(col).width(theme::SIDEBAR).height(Length::Fill).style(theme::sidebar).into()
}

/// The search box, with its magnifier.
pub fn search<'a>(ui: &'a Ui) -> El<'a, Msg> {
    let p = theme::look();
    container(
        row![
            icon(Icon::Search, 15.0, p.faint),
            text_input("Find someone", &ui.search).id(crate::SEARCH).on_input(Msg::Search).size(13.5).padding([7, 0]).style(theme::input_bare),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([0, 12])
    .style(theme::composer)
    .into()
}

/// Whether the screen on view already shows the call.
fn on_call_screen(ui: &Ui, m: &Model) -> bool {
    match (&ui.route, &m.call.peer) {
        (Route::Person(id), Some(peer)) => id == peer,
        (Route::Home, None) => true,
        _ => false,
    }
}

/// One person: avatar with presence, name, the last message, unread count.
pub fn person_row<'a>(ui: &'a Ui, m: &'a Model, person: &'a Person) -> El<'a, Msg> {
    let p = theme::look();
    let selected = ui.open_person() == Some(person.id.as_str());
    let on_call = m.call.with(&person.id);
    let ground = if selected { p.panel2 } else { p.bg2 };
    let dot = if on_call { Some(p.ok) } else { presence_color(person.presence) };
    let name_color = if selected || person.unread > 0 { p.text } else { mix_muted() };
    let sub: El<'a, Msg> = if on_call {
        row![icon(Icon::Phone, 12.0, p.ok), t(if m.call.phase == crate::Phase::Dialing { "Calling…" } else { "On a call" }).size(12.5).color(p.ok)]
            .spacing(5)
            .align_y(Alignment::Center)
            .into()
    } else {
        let line = person.last.as_deref().map(|s| clip(s, 30)).unwrap_or_else(|| clip(&person.address, 30));
        t(line).size(12.5).color(if person.unread > 0 { p.muted } else { p.faint }).wrapping(Wrapping::None).into()
    };
    let mut right = column![].spacing(4).align_x(Alignment::End);
    if let Some(at) = person.last_at.as_deref() {
        right = right.push(faint(at).size(11));
    }
    if person.unread > 0 {
        right = right.push(container(t(person.unread.to_string()).size(11).font(theme::bold()).color(theme::hex("#16161f"))).padding([1, 7]).style(theme::unread));
    }
    let body = row![
        avatar_dot(&person.name, &person.id, 36.0, dot, ground),
        container(column![t(clip(&person.name, 22)).size(14).font(theme::semibold()).color(name_color).wrapping(Wrapping::None), sub].spacing(2)).width(Length::Fill).clip(true),
        right,
    ]
    .spacing(11)
    .align_y(Alignment::Center);
    let row = button(container(body).clip(true))
        .width(Length::Fill)
        .padding([7, 10])
        .style(theme::btn(if selected { Btn::RowOn } else { Btn::Row }))
        .on_press(Msg::Nav(Route::Person(person.id.clone())));
    iced_widget::mouse_area(row).on_right_press(Msg::Menu(Some(person.id.clone()))).into()
}

fn mix_muted() -> iced_core::Color {
    let p = theme::look();
    theme::mix(p.muted, p.text, 0.35)
}

/// The call you are on, while you look at something else: who, how long,
/// and the two controls you reach for.
pub fn call_strip<'a>(m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let c = &m.call;
    let (title, color) = match c.phase {
        crate::Phase::Dialing => ("Calling…".to_string(), p.warn),
        _ => (format!("On a call · {}", clock(c.seconds)), p.ok),
    };
    let target = match &c.peer {
        Some(id) => Route::Person(id.clone()),
        None => Route::Home,
    };
    let who = button(
        column![
            row![icon(Icon::Phone, 13.0, color), t(title).size(12.5).font(theme::semibold()).color(color)].spacing(6).align_y(Alignment::Center),
            t(clip(&c.peer_name, 24)).size(13).color(p.text).wrapping(Wrapping::None),
        ]
        .spacing(2),
    )
    .padding(0)
    .width(Length::Fill)
    .style(|_, _| button::Style::default())
    .on_press(Msg::Nav(target));
    container(
        row![
            who,
            icon_btn_kind(
                if c.muted { Icon::MicOff } else { Icon::Mic },
                if c.muted { "Unmute" } else { "Mute" },
                if c.muted { Btn::IconOn } else { Btn::Icon },
                Some(Msg::Do(Action::Mute(!c.muted)))
            ),
            icon_btn_kind(Icon::PhoneOff, "Leave the call", Btn::IconOn, Some(Msg::Do(Action::Hangup))),
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .padding([10, 12])
    .style(theme::card_raised)
    .into()
}

/// You: name, whether your door is open, and quick controls.
fn me_bar<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let open = m.me.door_open;
    let door = button(
        row![
            icon(if open { Icon::DoorOpen } else { Icon::Door }, 13.0, if open { p.ok } else { p.faint }),
            t(if open { "Door open" } else { "Door closed" }).size(12).color(if open { p.ok } else { p.faint }),
        ]
        .spacing(5)
        .align_y(Alignment::Center),
    )
    .padding(0)
    .style(|_, _| button::Style::default())
    .on_press(Msg::Do(Action::Door(!open)));
    let door = with_tip(door, if open { "People can call you. Click to close your door." } else { "Click to open your door and take calls." }, iced_widget::tooltip::Position::Top);
    let settings_on = matches!(ui.route, Route::Settings(_));
    let c = &m.call;
    let me = row![
        avatar_dot(&m.me.name, &m.me.id, 34.0, Some(if open { p.ok } else { p.faint }), p.bg),
        column![t(clip(&m.me.name, 16)).size(13.5).font(theme::semibold()).wrapping(Wrapping::None), door].spacing(1).width(Length::Fill),
        icon_btn_kind(
            if c.muted { Icon::MicOff } else { Icon::Mic },
            if c.muted { "Unmute" } else { "Mute" },
            if c.muted { Btn::IconOn } else { Btn::Icon },
            Some(Msg::Do(Action::Mute(!c.muted)))
        ),
        icon_btn_kind(
            if c.deafened { Icon::HeadphonesOff } else { Icon::Headphones },
            if c.deafened { "Hear again" } else { "Deafen" },
            if c.deafened { Btn::IconOn } else { Btn::Icon },
            Some(Msg::Do(Action::Deafen(!c.deafened)))
        ),
        icon_btn_kind(Icon::Gear, "Settings", if settings_on { Btn::RowOn } else { Btn::Icon }, Some(Msg::Nav(Route::Settings(Section::Profile)))),
    ]
    .spacing(4)
    .align_y(Alignment::Center);
    container(column![divider(), container(me).padding([10, 10])])
        .style(move |_| iced_widget::container::Style {
            background: Some(iced_core::Background::Color(p.bg)),
            ..Default::default()
        })
        .into()
}

#[allow(dead_code)]
fn gap<'a>(h: f32) -> El<'a, Msg> {
    space().height(h).into()
}
