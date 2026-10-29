//! The right-click menu on a person: how loud they are for you, silence
//! them for you, and the usual ways in. What is set here is remembered for
//! every call with them.

use iced_core::{Alignment, Length, Padding};
use iced_widget::{button, column, container, mouse_area, row, space, stack};

use crate::icons::{icon, Icon};
use crate::theme::{self, Btn};
use crate::widgets::*;
use crate::{Action, Dialog, El, Model, Msg, Route, Ui};

const WIDTH: f32 = 264.0;
/// Roughly how tall the menu is, to keep it on screen near the bottom edge.
const HEIGHT: f32 = 236.0;

/// The menu as a layer over the window, or nothing when it is closed.
pub fn layer<'a>(ui: &'a Ui, m: &'a Model) -> Option<El<'a, Msg>> {
    let (id, at) = ui.menu.as_ref()?;
    let person = m.people.iter().find(|p| &p.id == id)?;
    let p = theme::look();

    let first = person.name.split_whitespace().next().unwrap_or(&person.name);
    let mute_label = if person.muted { format!("Hear {first} again") } else { format!("Mute {first} for me") };
    let item = |i: Icon, label: String, msg: Msg, danger: bool| -> El<'a, Msg> {
        let color = if danger { p.err } else { p.text };
        button(row![icon(i, 15.0, color), t(label).size(13.5).color(color)].spacing(10).align_y(Alignment::Center))
            .width(Length::Fill)
            .padding([7, 10])
            .style(theme::btn(Btn::Row))
            .on_press(msg)
            .into()
    };

    let volume = column![
        row![t("Volume").size(12).font(theme::semibold()).color(p.muted), space().width(Length::Fill), faint("Only you hear this").size(11)].align_y(Alignment::Center),
        row![
            icon(if person.muted { Icon::VolumeOff } else { Icon::Volume }, 15.0, p.muted),
            volume_slider(person.volume_db, Length::Fill, {
                let id = id.clone();
                move |db| Msg::Do(Action::Volume(id.clone(), db))
            }),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    ]
    .spacing(6);

    let body = column![
        row![avatar(&person.name, &person.id, 28.0), t(person.name.as_str()).size(14).font(theme::semibold())].spacing(10).align_y(Alignment::Center),
        container(volume).padding([4, 2]),
        item(
            if person.muted { Icon::Volume } else { Icon::VolumeOff },
            mute_label,
            Msg::Do(Action::MuteFor(id.clone(), !person.muted)),
            false
        ),
        divider(),
        item(Icon::Phone, format!("Call {first}"), Msg::Do(Action::Call(id.clone())), false),
        item(Icon::Message, "Open conversation".into(), Msg::Nav(Route::Person(id.clone())), false),
        item(Icon::Trash, "Remove…".into(), Msg::Dialog(Some(Dialog::ConfirmRemove(id.clone()))), true),
    ]
    .spacing(4);
    let menu = container(body).padding(10).width(WIDTH).style(theme::menu_pop);

    // Opens under the pointer, nudged back inside the window near an edge.
    let x = at.x.min(ui.size.width - WIDTH - 8.0).max(8.0);
    let y = at.y.min(ui.size.height - HEIGHT - 8.0).max(8.0);
    let placed = container(menu).padding(Padding { top: y, left: x, right: 0.0, bottom: 0.0 });

    // A click anywhere else closes it.
    let catcher = mouse_area(container(space()).width(Length::Fill).height(Length::Fill)).on_press(Msg::Menu(None)).on_right_press(Msg::Menu(None));
    Some(stack![catcher, placed].into())
}
