//! The details panel beside a conversation: who they are, how to be sure,
//! how calls with them work, and the ways to part.

use iced_core::text::Wrapping;
use iced_core::{Alignment, Length};
use iced_widget::{column, container, row, scrollable, space, stack, text_input};

use crate::icons::{icon, Icon};
use crate::theme::{self, Badge, Btn};
use crate::widgets::*;
use crate::{Action, Dialog, El, Model, Msg, Person, Phase, Ui};

pub fn view<'a>(ui: &'a Ui, m: &'a Model, person: &'a Person) -> El<'a, Msg> {
    let p = theme::look();
    // The avatar hangs off the banner's bottom edge; the first layer sets
    // the stack's full height so it is not cut off.
    let head = stack![
        column![container(space()).width(Length::Fill).height(84).style(theme::banner), space().height(42)],
        container(
            container(avatar(&person.name, &person.id, 76.0)).padding(5).style(move |_| iced_widget::container::Style {
                background: Some(iced_core::Background::Color(p.panel)),
                border: iced_core::Border { radius: 99.0.into(), ..Default::default() },
                ..Default::default()
            })
        )
        .padding(pad(40.0, 0.0, 0.0, 18.0)),
    ];
    let mut name_row = row![t(person.name.as_str()).size(19).font(theme::bold()).wrapping(Wrapping::None)].spacing(8).align_y(Alignment::Center);
    if person.verified {
        name_row = name_row.push(badge_icon(Icon::ShieldCheck, "Verified", Badge::Ok));
    }
    let who = column![name_row, faint(if person.address.is_empty() { "No address yet" } else { person.address.as_str() }).size(12).font(theme::mono())].spacing(4);

    let mut col = column![head, container(who).padding([4, 18])].spacing(10);

    if m.call.with(&person.id) && m.call.phase == Phase::Connected {
        let c = &m.call;
        let mut call = column![row![icon(Icon::Wifi, 14.0, p.ok), t(c.mode.as_str()).size(13).font(theme::semibold())].spacing(8).align_y(Alignment::Center)].spacing(6);
        if !c.mode_detail.is_empty() {
            call = call.push(faint(c.mode_detail.as_str()).size(12));
        }
        if let Some(q) = c.quality.as_deref() {
            call = call.push(faint(format!("Quality: {q}")).size(12));
        }
        col = col.push(section("This call", call.into()));
    }

    let fp: El<'a, Msg> = if person.fingerprint.is_empty() {
        faint("Known after your first call.").size(12.5).into()
    } else {
        container(t(person.fingerprint.as_str()).size(12).font(theme::mono()).color(p.muted)).padding([8, 10]).width(Length::Fill).style(theme::inset).into()
    };
    let verify = setting(
        "Verified",
        Some("You compared the words on a call and they matched."),
        switch(person.verified, {
            let id = person.id.clone();
            move |on| Msg::Do(Action::Verify(id.clone(), on))
        }),
    );
    col = col.push(section("Identity", column![fp, verify].spacing(10).into()));

    let note_value = match &ui.note_edit {
        Some((id, n)) if id == &person.id => n.as_str(),
        _ => person.note.as_str(),
    };
    let note = text_input("Only you can see this", note_value).on_input(Msg::NoteEdit).on_submit(Msg::NoteSave).size(13).padding([8, 10]).style(theme::input);
    let volume = column![
        row![
            icon(if person.muted { Icon::VolumeOff } else { Icon::Volume }, 15.0, p.muted),
            volume_slider(person.volume_db, Length::Fill, {
                let id = person.id.clone();
                move |db| Msg::Do(Action::Volume(id.clone(), db))
            }),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        row![
            column![
                t(format!("Hear {}", person.name.split_whitespace().next().unwrap_or("them"))).size(13.5).font(theme::semibold()),
                faint("Only for you, and kept for every call.").size(11.5),
            ]
            .spacing(2)
            .width(Length::Fill),
            switch(!person.muted, {
                let id = person.id.clone();
                move |on| Msg::Do(Action::MuteFor(id.clone(), !on))
            }),
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(8);
    col = col.push(section("Volume", volume.into()));
    col = col.push(section("Note", note.into()));

    let mut calls = column![].spacing(8);
    if !person.ring_rule.is_empty() {
        calls = calls.push(kv("Rings through", &person.ring_rule));
    }
    if !person.route.is_empty() {
        calls = calls.push(kv("Calls go", &person.route));
    }
    if !person.ring_rule.is_empty() || !person.route.is_empty() {
        col = col.push(section("Calls", calls.into()));
    }

    let manage = row![
        btn_icon(Icon::Trash, "Remove", Btn::Danger, Some(Msg::Dialog(Some(Dialog::ConfirmRemove(person.id.clone()))))),
        btn_icon(Icon::Shield, "Block", Btn::Danger, Some(Msg::Dialog(Some(Dialog::ConfirmBlock(person.id.clone()))))),
    ]
    .spacing(8);
    col = col.push(container(manage).padding(pad(4.0, 18.0, 24.0, 18.0)));

    container(scrollable(col).height(Length::Fill).style(theme::scroll)).width(theme::PANEL).height(Length::Fill).into()
}

/// A titled group in the panel.
fn section<'a>(title: &'a str, body: El<'a, Msg>) -> El<'a, Msg> {
    container(column![label(title), body].spacing(8)).padding([6, 18]).into()
}

/// A label and a value on one line.
fn kv<'a>(k: &'a str, v: &'a str) -> El<'a, Msg> {
    row![faint(k).size(12.5).width(Length::Fill), t(v).size(12.5)].spacing(8).into()
}
