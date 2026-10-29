//! A conversation: the header, the call stage when you are on a call with
//! them, the messages, files on their way, and the composer.

use iced_core::text::Wrapping;
use iced_core::{Alignment, Length};
use iced_widget::{button, column, container, hover, progress_bar, row, scrollable, space, text_input};

use super::{call, fill};
use crate::icons::{icon, Icon};
use crate::theme::{self, Badge, Btn};
use crate::widgets::*;
use crate::{Action, Delivery, El, Line, Model, Msg, Person, Phase, Ui};

/// Reactions offered on a message.
const QUICK: [&str; 4] = ["👍", "❤️", "😂", "😮"];

/// The conversation with `person`: header, call stage, messages and composer.
pub fn view<'a>(ui: &'a Ui, m: &'a Model, id: &'a str) -> El<'a, Msg> {
    let Some(person) = m.people.iter().find(|p| p.id == id) else {
        return container(empty(Icon::Users, "They are not in your contacts", "They may have been removed.")).center(Length::Fill).into();
    };
    let mut col = column![header(ui, m, person, false), divider()];
    if m.call.with(id) || matches!(m.call.phase, Phase::Ended(_)) && m.call.peer.as_deref() == Some(id) {
        col = col.push(call::stage(ui, m, false));
    }
    col = col.push(container(messages(ui, m, person, false)).height(Length::Fill));
    col = col.push(transfers(ui, m));
    col = col.push(container(composer(ui, m, person)).padding(pad(4.0, 20.0, 18.0, 20.0)));
    col.height(Length::Fill).into()
}

/// The bar over the conversation: who, and what you can do.
pub fn header<'a>(ui: &'a Ui, m: &'a Model, person: &'a Person, phone: bool) -> El<'a, Msg> {
    let p = theme::look();
    let mut title = row![t(person.name.as_str()).size(16).font(theme::bold()).wrapping(Wrapping::None)].spacing(8).align_y(Alignment::Center);
    if person.verified {
        title = title.push(with_tip(icon(Icon::ShieldCheck, 15.0, p.ok), "Verified: you compared the words on a call", iced_widget::tooltip::Position::Bottom));
    }
    let sub = if !person.address.is_empty() { person.address.as_str() } else { "Not reached yet" };
    let mut r = row![].spacing(10).align_y(Alignment::Center);
    if phone {
        r = r.push(icon_btn(Icon::Back, "", Some(Msg::Back)));
    }
    r = r.push(avatar_dot(&person.name, &person.id, 34.0, super::presence_color(person.presence), p.panel));
    r = r.push(column![title, faint(sub).size(12).wrapping(Wrapping::None)].spacing(1).width(Length::Fill));
    let on_this_call = m.call.with(&person.id);
    if !on_this_call {
        let can_call = !m.call.active();
        if phone {
            r = r.push(icon_btn(Icon::Phone, "Call", can_call.then(|| Msg::Do(Action::Call(person.id.clone())))));
        } else {
            r = r.push(btn_icon(Icon::Phone, "Call", Btn::Primary, can_call.then(|| Msg::Do(Action::Call(person.id.clone())))));
        }
    }
    if !phone {
        r = r.push(icon_btn_kind(
            Icon::Panel,
            if ui.panel_open { "Hide details" } else { "Show details" },
            if ui.panel_open && ui.size.width >= theme::PANEL_FITS { Btn::RowOn } else { Btn::Icon },
            Some(Msg::TogglePanel),
        ));
    }
    container(r).height(theme::HEADER_H).padding([0, if phone { 8 } else { 20 }]).align_y(Alignment::Center).into()
}

/// The start of the conversation, then every message.
pub fn messages<'a>(ui: &'a Ui, m: &'a Model, person: &'a Person, phone: bool) -> El<'a, Msg> {
    let p = theme::look();
    let mut col = column![intro(m, person)].spacing(2).padding(pad(18.0, if phone { 12.0 } else { 24.0 }, 12.0, if phone { 12.0 } else { 24.0 }));
    let lines: &[Line] = m.conversation.as_ref().filter(|c| c.person == person.id).map(|c| c.lines.as_slice()).unwrap_or(&[]);
    let mut prev: Option<&Line> = None;
    for line in lines {
        if let Some(day) = line.day.as_deref() {
            col = col.push(day_rule(day));
            prev = None;
        }
        let first = prev.is_none_or(|q| q.mine != line.mine || q.author != line.author);
        if first && prev.is_some() {
            col = col.push(space().height(10));
        }
        col = col.push(message(ui, m, person, line, first));
        prev = Some(line);
    }
    if m.conversation.as_ref().is_some_and(|c| c.person == person.id && c.lines.is_empty()) {
        col = col.push(container(faint("No messages yet.").size(12.5)).padding([12, 0]));
    }
    let _ = p;
    scrollable(col).anchor_bottom().height(Length::Fill).width(Length::Fill).style(theme::scroll).into()
}

/// The big avatar and name at the top of the history.
fn intro<'a>(m: &'a Model, person: &'a Person) -> El<'a, Msg> {
    let mut line = format!("This is the start of your conversation with {}.", person.name);
    if !person.verified {
        line.push_str(" On your first call, compare the words on screen to make sure it is really them.");
    }
    let mut actions = row![].spacing(8);
    if !m.call.active() {
        actions = actions.push(btn_icon(Icon::Phone, "Call", Btn::Default, Some(Msg::Do(Action::Call(person.id.clone())))));
    }
    if !person.fingerprint.is_empty() {
        actions = actions.push(btn_icon(Icon::Copy, "Copy fingerprint", Btn::Ghost, Some(Msg::Copy(person.fingerprint.clone(), "Fingerprint"))));
    }
    column![avatar(&person.name, &person.id, 72.0), h1(person.name.as_str()), muted(line).size(13.5), actions]
        .spacing(10)
        .padding(pad(8.0, 0.0, 22.0, 0.0))
        .into()
}

/// "Today", centred between two hairlines.
fn day_rule<'a>(day: &'a str) -> El<'a, Msg> {
    row![container(divider()).width(Length::Fill), faint(day).size(11.5), container(divider()).width(Length::Fill)]
        .spacing(12)
        .align_y(Alignment::Center)
        .padding([14, 0])
        .into()
}

/// One message: theirs on the left with their avatar, mine on the right
/// in a tinted bubble. Actions appear under the pointer.
fn message<'a>(ui: &'a Ui, _m: &'a Model, person: &'a Person, line: &'a Line, first: bool) -> El<'a, Msg> {
    let p = theme::look();
    let editing = ui.editing.as_ref().filter(|(id, _)| *id == line.id);

    let content: El<'a, Msg> = if let Some((_, draft)) = editing {
        column![
            text_input("", draft).on_input(Msg::EditDraft).on_submit(Msg::EditSave).padding([8, 10]).size(14).style(theme::input),
            row![faint("Enter to save · Esc to cancel").size(11), fill(), btn_sm("Cancel", Btn::Ghost, Some(Msg::EditCancel)), btn_sm("Save", Btn::Primary, Some(Msg::EditSave))]
                .spacing(6)
                .align_y(Alignment::Center),
        ]
        .spacing(6)
        .width(Length::Fixed(420.0))
        .into()
    } else if let Some(clip) = line.voice {
        let play = round_btn(
            if clip.playing { Icon::Stop } else { Icon::Play },
            if clip.playing { "Stop" } else { "Play" },
            if clip.playing { Btn::RoundOn } else { Btn::Round },
            Some(Msg::Do(Action::PlayVoice { to: person.id.clone(), id: line.id })),
            34.0,
        );
        let length = format!("{}:{:02}", clip.seconds / 60, clip.seconds % 60);
        let sub = if clip.fetching { format!("{length} · getting it") } else { length };
        container(
            row![play, column![t("Voice message").size(13.5).font(theme::semibold()), faint(sub).size(11.5)].spacing(2)].spacing(10).align_y(Alignment::Center),
        )
        .padding([8, 12])
        .style(if line.mine { theme::bubble_mine } else { theme::card_raised })
        .into()
    } else if let Some((name, size)) = line.file.as_ref() {
        container(
            row![
                container(icon(Icon::File, 18.0, p.accent2)).center(36).style(theme::inset),
                column![t(name.as_str()).size(13.5).font(theme::semibold()), faint(size.as_str()).size(11.5)].spacing(2),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .padding([10, 12])
        .style(if line.mine { theme::bubble_mine } else { theme::inset })
        .into()
    } else {
        let mut words = column![t(line.text.as_str()).size(14).wrapping(Wrapping::WordOrGlyph)].spacing(2);
        if line.edited {
            words = words.push(faint("edited").size(10.5));
        }
        container(words).padding([8, 12]).max_width(520).style(if line.mine { theme::bubble_mine } else { theme::card_raised }).into()
    };

    let mut meta = row![].spacing(6).align_y(Alignment::Center);
    if line.pinned {
        meta = meta.push(icon(Icon::Sparkle, 11.0, p.accent2));
    }
    meta = meta.push(faint(line.time.as_str()).size(11));
    if line.mine {
        let (label, color) = match line.delivery {
            Delivery::Queued => ("Waiting until they're reachable", p.warn),
            Delivery::Sent => ("Sent", p.faint),
            Delivery::Delivered => ("Delivered", p.faint),
            Delivery::Failed => ("Not delivered", p.err),
            Delivery::None => ("", p.faint),
        };
        if !label.is_empty() {
            meta = meta.push(t(label).size(11).color(color));
        }
    }

    let mut stack_col = column![content].spacing(4).align_x(if line.mine { Alignment::End } else { Alignment::Start });
    if !line.reactions.is_empty() {
        let mut rr = row![].spacing(4);
        for (emoji, n) in &line.reactions {
            rr = rr.push(
                button(row![t(emoji.as_str()).size(13), t(n.to_string()).size(11.5).color(p.muted)].spacing(4).align_y(Alignment::Center))
                    .padding([2, 8])
                    .style(theme::btn(Btn::Default))
                    .on_press(Msg::Do(Action::React { to: person.id.clone(), id: line.id, emoji: emoji.clone() })),
            );
        }
        stack_col = stack_col.push(rr);
    }
    if first || !line.reactions.is_empty() || line.delivery == Delivery::Failed || line.delivery == Delivery::Queued {
        stack_col = stack_col.push(meta);
    }

    let lead: El<'a, Msg> = if line.mine {
        space().width(0).into()
    } else if first {
        avatar(if line.author.is_empty() { &person.name } else { &line.author }, &person.id, 30.0)
    } else {
        space().width(30).into()
    };

    let body: El<'a, Msg> = if line.mine {
        row![space().width(Length::Fill), stack_col].spacing(10).width(Length::Fill).into()
    } else {
        row![lead, stack_col, space().width(Length::Fill)].spacing(10).align_y(Alignment::Start).width(Length::Fill).into()
    };

    if editing.is_some() || line.file.is_some() || line.voice.is_some() {
        return container(body).padding([2, 0]).into();
    }
    // The actions float over the far side of the row while the pointer is on it.
    let mut tools = row![].spacing(2).align_y(Alignment::Center);
    for e in QUICK {
        tools = tools.push(
            button(t(e).size(14))
                .padding([3, 6])
                .style(theme::btn(Btn::Ghost))
                .on_press(Msg::Do(Action::React { to: person.id.clone(), id: line.id, emoji: e.to_string() })),
        );
    }
    tools = tools.push(icon_btn(Icon::Sparkle, if line.pinned { "Unpin" } else { "Pin" }, Some(Msg::Do(Action::Pin { to: person.id.clone(), id: line.id, pinned: !line.pinned }))));
    if line.mine {
        tools = tools.push(icon_btn(Icon::Edit, "Edit", Some(Msg::EditStart(line.id, line.text.clone()))));
        tools = tools.push(icon_btn(Icon::Trash, "Delete", Some(Msg::Do(Action::Delete { to: person.id.clone(), id: line.id }))));
    }
    tools = tools.push(icon_btn(Icon::Copy, "Copy", Some(Msg::Copy(line.text.clone(), "Message"))));
    let bar = container(tools).padding(3).style(theme::menu_pop);
    let overlay = container(bar).width(Length::Fill).align_x(if line.mine { Alignment::Start } else { Alignment::End }).padding([0, 6]);
    hover(container(body).padding([2, 0]).width(Length::Fill), overlay)
}

/// A file they offered, files on their way, and those waiting their turn.
pub fn transfers<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let mut col = column![].spacing(8).padding([0, 20]);
    if let Some((name, size)) = m.transfers.offer.as_ref() {
        col = col.push(
            container(
                row![
                    container(icon(Icon::Download, 18.0, p.accent2)).center(38).style(theme::inset),
                    column![t(format!("{} wants to send you a file", m.call.peer_name)).size(13.5).font(theme::semibold()), faint(format!("{name} · {size}")).size(12)].spacing(2).width(Length::Fill),
                    btn_sm("Decline", Btn::Ghost, Some(Msg::Do(Action::DeclineFile))),
                    btn_sm("Accept", Btn::Primary, Some(Msg::Do(Action::AcceptFile))),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            )
            .padding([10, 14])
            .style(theme::card_raised),
        );
    }
    for tr in &m.transfers.active {
        col = col.push(
            container(
                column![
                    row![
                        icon(if tr.incoming { Icon::Download } else { Icon::Send }, 14.0, p.muted),
                        t(tr.name.as_str()).size(13).font(theme::semibold()).width(Length::Fill),
                        faint(tr.detail.as_str()).size(11.5),
                        icon_btn(Icon::X, "Cancel", Some(Msg::Do(Action::CancelTransfer))),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                    progress_bar(0.0..=1.0, tr.progress).girth(5).style(theme::progress),
                ]
                .spacing(6),
            )
            .padding([8, 14])
            .style(theme::card_raised),
        );
    }
    if !m.transfers.queued.is_empty() {
        let n = m.transfers.queued.len();
        let head = button(
            row![
                label("Up next"),
                faint(n.to_string()).size(11),
                fill(),
                faint(if ui.queue_open { "Hide" } else { "Show" }).size(11.5),
                icon(if ui.queue_open { Icon::Up } else { Icon::Down }, 13.0, p.faint),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .padding(0)
        .style(|_, _| iced_widget::button::Style::default())
        .on_press(Msg::ToggleQueue);
        let mut rows = column![head].spacing(4);
        let last = n - 1;
        for (i, q) in m.transfers.queued.iter().enumerate().filter(|_| ui.queue_open) {
            let name = t(q.name.as_str()).size(13).font(theme::semibold()).color(if q.held { p.faint } else { p.text }).wrapping(Wrapping::None);
            let detail = if q.held { format!("{} · held", q.detail) } else { q.detail.clone() };
            rows = rows.push(
                row![
                    icon(Icon::File, 14.0, p.muted),
                    column![name, faint(detail).size(11.5).wrapping(Wrapping::None)].spacing(1).width(Length::Fill),
                    icon_btn(Icon::Up, "Sooner", (i > 0).then_some(Msg::Do(Action::QueueMove { id: q.id, up: true }))),
                    icon_btn(Icon::Down, "Later", (i < last).then_some(Msg::Do(Action::QueueMove { id: q.id, up: false }))),
                    icon_btn_kind(
                        Icon::Clock,
                        if q.held { "Let it go" } else { "Hold it back" },
                        if q.held { Btn::IconOn } else { Btn::Icon },
                        Some(Msg::Do(Action::QueueHold { id: q.id, held: !q.held }))
                    ),
                    icon_btn(Icon::X, "Remove", Some(Msg::Do(Action::QueueRemove(q.id)))),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            );
        }
        col = col.push(container(rows).padding([8, 14]).style(theme::card_raised));
    }
    col.into()
}

/// Where messages are typed, and what happens to them.
pub fn composer<'a>(ui: &'a Ui, m: &'a Model, person: &'a Person) -> El<'a, Msg> {
    let p = theme::look();
    let conv = m.conversation.as_ref().filter(|c| c.person == person.id);
    let live = conv.is_some_and(|c| c.live);
    let queues = conv.is_some_and(|c| c.queues);
    let placeholder = if live {
        format!("Message {}", person.name)
    } else if queues {
        format!("Leave {} a message", person.name)
    } else {
        format!("Call {} to message them", person.name)
    };
    let draft = ui.drafts.get(&person.id).map(String::as_str).unwrap_or("");
    let can_send = live || queues;
    // While a voice message records, the composer is its controls.
    if let Some(secs) = conv.and_then(|c| c.recording) {
        let max = 120;
        let bar = row![
            container(space()).width(10).height(10).style(theme::avatar(p.err)),
            t(format!("Recording {}:{:02}", secs / 60, secs % 60)).size(14).font(theme::semibold()),
            faint(format!("· up to {}:{:02}", max / 60, max % 60)).size(12),
            fill(),
            btn_sm("Cancel", Btn::Ghost, Some(Msg::Do(Action::FinishVoice { to: person.id.clone(), send: false }))),
            btn_icon(Icon::Send, "Send", Btn::Primary, Some(Msg::Do(Action::FinishVoice { to: person.id.clone(), send: true }))),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        return column![container(bar).padding([8, 12]).style(theme::composer)].into();
    }
    let mut input = text_input(&placeholder, draft).id(crate::COMPOSER).size(14).padding([11, 4]).style(theme::input_bare);
    if can_send {
        input = input.on_input(Msg::Draft).on_submit(Msg::Send);
    }
    let file_ok = live && m.device.files && m.call.with(&person.id);
    let bar = row![
        icon_btn(Icon::Paperclip, if file_ok { "Send a file" } else { "Files go during a call" }, file_ok.then_some(Msg::Do(Action::SendFile))),
        input,
        // With nothing typed, the button records a voice message instead.
        if draft.trim().is_empty() && can_send && m.device.voice_messages {
            icon_btn(Icon::Mic, "Record a voice message", Some(Msg::Do(Action::RecordVoice(person.id.clone()))))
        } else {
            icon_btn_kind(Icon::Send, "Send", if draft.trim().is_empty() { Btn::Icon } else { Btn::RowOn }, (can_send && !draft.trim().is_empty()).then_some(Msg::Send))
        },
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    let mut hint = row![icon(Icon::Lock, 11.0, p.faint), faint("End-to-end encrypted").size(11)].spacing(5).align_y(Alignment::Center);
    if queues && !live {
        hint = hint.push(faint("· It goes when they're next reachable").size(11));
    }
    if let Some(e) = conv.and_then(|c| c.expiry.as_deref()) {
        hint = hint.push(badge_icon(Icon::Clock, e, Badge::Plain));
    }
    column![container(bar).padding([2, 8]).style(theme::composer), container(hint).padding([0, 6])].spacing(6).into()
}
