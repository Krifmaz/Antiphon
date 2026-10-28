//! The narrow layout a phone gets: one screen at a time, a bar on top, the
//! call pinned as a banner, and tabs along the bottom.

use iced_core::{Alignment, Length};
use iced_widget::{button, column, container, row, scrollable};

use super::{call, conversation, fill, home, settings, sidebar};
use crate::icons::{icon, logo, Icon};
use crate::theme::{self, Btn};
use crate::widgets::*;
use crate::{Dialog, El, Model, Msg, Route, Section, Ui};

pub fn view<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let screen: El<'a, Msg> = match &ui.route {
        Route::Person(id) => match m.people.iter().find(|p| &p.id == id) {
            Some(person) => {
                let mut col = column![conversation::header(ui, m, person, true), divider()];
                if m.call.with(id) {
                    col = col.push(call::stage(ui, m, true));
                }
                col = col.push(container(conversation::messages(ui, m, person, true)).height(Length::Fill));
                col = col.push(container(conversation::composer(ui, m, person)).padding(pad(4.0, 10.0, 12.0, 10.0)));
                col.into()
            }
            None => people(ui, m),
        },
        Route::Settings(_) => column![top_bar("Settings", true), divider(), settings::stacked(ui, m)].into(),
        Route::Home => people(ui, m),
    };
    let mut col = column![];
    if let Some(n) = m.notice.as_deref() {
        col = col.push(warn_box(t(n).size(13)));
    }
    let on_call_screen = matches!((&ui.route, &m.call.peer), (Route::Person(id), Some(peer)) if id == peer);
    if m.call.active() && !on_call_screen {
        col = col.push(container(sidebar::call_strip(m)).padding([8, 10]));
    }
    col = col.push(container(screen).height(Length::Fill).style(theme::stage_flat));
    if !matches!(ui.route, Route::Person(_)) {
        col = col.push(tab_bar(ui));
    }
    col.height(Length::Fill).into()
}

/// The people screen: brand bar, your door, search, everyone.
fn people<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let bar = row![
        logo(30.0),
        t("Stoop").size(18).font(theme::bold()),
        fill(),
        icon_btn(Icon::Qr, "", Some(Msg::Dialog(Some(Dialog::Invite)))),
        icon_btn(Icon::UserPlus, "", Some(Msg::Dialog(Some(Dialog::AddPerson)))),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    let mut list = column![].spacing(2);
    let query = ui.search.trim().to_lowercase();
    for person in m.people.iter().filter(|x| query.is_empty() || x.name.to_lowercase().contains(&query)) {
        list = list.push(sidebar::person_row(ui, m, person));
    }
    let mut col = column![
        home::door_card(m, true),
        sidebar::search(ui),
        row![label("People"), fill()].padding([0, 6]),
        list,
    ]
    .spacing(12);
    if m.people.is_empty() {
        col = col.push(home::starts(m, true));
    }
    column![
        container(bar).padding([10, 14]),
        scrollable(container(col).padding(pad(4.0, 12.0, 16.0, 12.0))).height(Length::Fill).style(theme::scroll),
    ]
    .into()
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

/// People, invite, settings.
fn tab_bar<'a>(ui: &'a Ui) -> El<'a, Msg> {
    let p = theme::look();
    let tab = |i: Icon, label: &'a str, on: bool, msg: Msg| -> El<'a, Msg> {
        let color = if on { p.text } else { p.faint };
        button(column![icon(i, 22.0, color), t(label).size(11).font(theme::semibold()).color(color)].spacing(3).align_x(Alignment::Center))
            .width(Length::Fill)
            .padding([8, 0])
            .style(theme::btn(Btn::Ghost))
            .on_press(msg)
            .into()
    };
    let r = row![
        tab(Icon::Users, "People", ui.route == Route::Home, Msg::Nav(Route::Home)),
        tab(Icon::Qr, "Invite", false, Msg::Dialog(Some(Dialog::Invite))),
        tab(Icon::Gear, "Settings", matches!(ui.route, Route::Settings(_)), Msg::Nav(Route::Settings(Section::Profile))),
    ];
    column![divider(), container(r).padding([2, 8]).style(theme::sidebar)].into()
}
