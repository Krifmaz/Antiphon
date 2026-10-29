//! The narrow layout a phone gets: one screen at a time, a bar on top, the
//! call pinned as a banner, and tabs along the bottom.
//!
//! Laid out for one thumb: what is used most (the tabs, adding someone,
//! the door) sits in the bottom third, and the top holds only the name of
//! where you are. The system's status bar and gesture bar are kept clear
//! of every control, with the app's colours running on underneath them,
//! so the app reaches the edges of the glass without anything under a
//! clock or a home bar.

use iced_core::{Alignment, Background, Border, Color, Length, Padding};
use iced_widget::{button, column, container, row, scrollable, space};

use super::{call, community, conversation, fill, home, settings, sidebar};
use crate::icons::{icon, logo, Icon};
use crate::theme;
use crate::widgets::*;
use crate::{Dialog, El, Model, Msg, Route, Section, Ui};

/// The whole phone layout: top bar, the current screen, call banner and tab bar.
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
        Route::Spaces => community::list(ui, m, true),
        Route::Community(id, channel) => community::phone(ui, m, id, *channel),
        Route::Home => people(ui, m),
    };
    let (top, bottom) = ui.insets;
    // The status bar's strip: the app's own background under the clock.
    let mut col = column![space().height(top)];
    if let Some(n) = m.notice.as_deref() {
        col = col.push(warn_box(t(n).size(13)));
    }
    let on_call_screen = matches!((&ui.route, &m.call.peer), (Route::Person(id), Some(peer)) if id == peer);
    if m.call.active() && !on_call_screen {
        col = col.push(container(sidebar::call_strip(m)).padding([8, 10]));
    }
    col = col.push(container(screen).height(Length::Fill).style(theme::stage_flat));
    if matches!(ui.route, Route::Person(_) | Route::Community(..)) {
        // No tabs in a conversation: the composer sits just above the
        // gesture bar (or the keyboard).
        col = col.push(container(space()).height(bottom).width(Length::Fill).style(theme::stage_flat));
    } else {
        col = col.push(tab_bar(ui, m, bottom));
    }
    col.height(Length::Fill).into()
}

/// The people screen: the name at the top, search and everyone in the
/// middle, and the door at the bottom where a thumb reaches it.
fn people<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let bar = row![logo(28.0), t("Stoop").size(18).font(theme::bold())].spacing(10).align_y(Alignment::Center);
    let mut list = column![].spacing(2);
    let query = ui.search.trim().to_lowercase();
    for person in m.people.iter().filter(|x| query.is_empty() || x.name.to_lowercase().contains(&query)) {
        list = list.push(sidebar::person_row(ui, m, person));
    }
    let mut col = column![].spacing(12);
    if m.people.is_empty() {
        // Nobody yet: the ways to start, as rows a thumb can hit.
        col = col.push(row![label("Get started"), fill()].padding([0, 6])).push(home::starts(m, true));
    } else {
        col = col.push(sidebar::search(ui)).push(row![label("People"), fill()].padding([0, 6])).push(list);
    }
    column![
        container(bar).padding([12, 16]),
        scrollable(container(col).padding(pad(4.0, 12.0, 16.0, 12.0))).height(Length::Fill).style(theme::scroll),
        container(home::door_card(m, true)).padding(pad(0.0, 12.0, 10.0, 12.0)),
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

/// People, spaces (where the device has them), invite, add, settings:
/// equal tabs along the bottom, sitting above the gesture bar with their
/// colour running on under it. `bottom` is the gesture bar's (or
/// keyboard's) height.
fn tab_bar<'a>(ui: &'a Ui, m: &'a Model, bottom: f32) -> El<'a, Msg> {
    let p = theme::look();
    let tab = |i: Icon, label: &'a str, on: bool, msg: Msg| -> El<'a, Msg> {
        let color = if on { p.text } else { p.muted };
        // The tab you are on gets a pill behind its icon, Android-style;
        // a press shows nothing that stays behind, since a finger leaves
        // no pointer to hover.
        let pill = container(icon(i, 22.0, color)).padding([4, 18]).style(move |_| container::Style {
            background: on.then(|| Background::Color(p.accent2.scale_alpha(0.22))),
            border: Border { radius: 99.0.into(), ..Border::default() },
            ..container::Style::default()
        });
        let face = column![pill, t(label).size(11.5).font(theme::semibold()).color(color)].spacing(4).align_x(Alignment::Center);
        button(container(face).center_x(Length::Fill))
            .width(Length::Fill)
            .padding([8, 0])
            .style(|_, _| button::Style { background: Some(Background::Color(Color::TRANSPARENT)), ..button::Style::default() })
            .on_press(msg)
            .into()
    };
    let mut r = row![tab(Icon::Users, "People", ui.route == Route::Home, Msg::Nav(Route::Home))].align_y(Alignment::Center);
    if m.device.communities {
        r = r.push(tab(Icon::Spaces, "Spaces", ui.route == Route::Spaces, Msg::Nav(Route::Spaces)));
    }
    let r = r
        .push(tab(Icon::Qr, "Invite", ui.dialog == Some(Dialog::Invite), Msg::Dialog(Some(Dialog::Invite))))
        .push(tab(Icon::UserPlus, "Add", ui.dialog == Some(Dialog::AddPerson), Msg::Dialog(Some(Dialog::AddPerson))))
        .push(tab(Icon::Gear, "Settings", matches!(ui.route, Route::Settings(_)), Msg::Nav(Route::Settings(Section::Profile))));
    column![divider(), container(r).padding(Padding { top: 2.0, right: 6.0, bottom: 4.0 + bottom, left: 6.0 }).width(Length::Fill).style(theme::sidebar)].into()
}
