//! The first screen: a name, and a word on what Stoop is.

use iced_core::{Alignment, Length};
use iced_widget::{column, container, row};

use crate::icons::{icon, logo, Icon};
use crate::theme::{self, Btn};
use crate::widgets::*;
use crate::{El, Model, Msg, Ui};

/// The welcome card: asks for a name before anything else.
pub fn view<'a>(ui: &'a Ui, _m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let point = |i: Icon, title: &'a str, line: &'a str| -> El<'a, Msg> {
        row![
            container(icon(i, 16.0, p.accent2)).center(34).style(theme::inset),
            column![t(title).size(13.5).font(theme::semibold()), faint(line).size(12.5)].spacing(2),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .into()
    };
    let ready = !ui.first_name.trim().is_empty();
    let card = container(
        column![
            logo(64.0),
            column![h1("Welcome to Stoop"), muted("Calls and messages that go straight to the people you talk to.").size(14)].spacing(6),
            column![
                point(Icon::Lock, "No accounts, no middleman", "Your identity is a key made on this device."),
                point(Icon::DoorOpen, "Your door, your rules", "Open it to take calls; close it and nobody rings."),
                point(Icon::ShieldCheck, "Know it's them", "Compare a short code on your first call."),
            ]
            .spacing(12),
            field(
                "What should people call you?",
                input("What should people call you?", "stoop-first-name", "Your name", &ui.first_name).on_input(Msg::FirstName).on_submit(Msg::FirstDone).padding([11, 12]).size(15)
            ),
            btn("Continue", Btn::Primary, ready.then_some(Msg::FirstDone)).width(Length::Fill),
        ]
        .spacing(22),
    )
    .padding(32)
    .max_width(440)
    .style(theme::modal);
    container(card).center(Length::Fill).padding(20).into()
}
