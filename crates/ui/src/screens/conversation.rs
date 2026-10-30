//! A conversation: the header, the call stage when you are on a call with
//! them, the messages, files on their way, and the composer.

use iced_core::text::Wrapping;
use iced_core::{Alignment, Border, ContentFit, Length};
use iced_widget::{button, column, container, hover, image, progress_bar, row, scrollable, space};

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
        // Who it is, as one button into their profile, the way a chat
        // app's header opens the person.
        let who = row![
            avatar_dot(&person.name, &person.id, 34.0, super::presence_color(person.presence), p.panel),
            container(title.push(icon(Icon::Chevron, 14.0, p.faint))).clip(true),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        r = r.push(
            pressable(who, crate::a11y::Role::Button, &format!("{}'s profile", person.name), false, Btn::Row, Msg::Nav(crate::Route::Profile(Some(person.id.clone()))))
                .padding([4, 6])
                .width(Length::Fill),
        );
    } else {
        // Their pronouns go before where they were reached, as the line a
        // person reads first; the picture and name open their profile.
        let sub = if person.pronouns.is_empty() { sub.to_string() } else { format!("{} · {sub}", person.pronouns) };
        let who = row![
            avatar_dot(&person.name, &person.id, 34.0, super::presence_color(person.presence), p.panel),
            column![title, faint(sub).size(12).wrapping(Wrapping::None)].spacing(1),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        let who = pressable(who, crate::a11y::Role::Button, &format!("{}'s profile", person.name), false, Btn::Row, Msg::Nav(crate::Route::Profile(Some(person.id.clone())))).padding([4, 6]);
        r = r.push(container(who).width(Length::Fill).clip(true));
    }
    let on_this_call = m.call.with(&person.id);
    if !on_this_call && m.call.phase == Phase::Connected {
        // On a call with someone else: this one can be brought into it.
        let add = Some(Msg::Do(Action::AddToCall(person.id.clone())));
        if phone {
            r = r.push(icon_btn(Icon::UserPlus, "Add to call", add));
        } else {
            r = r.push(btn_icon(Icon::UserPlus, "Add to call", Btn::Primary, add));
        }
    } else if !on_this_call {
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
        // A reply always starts a new group, so its quote sits over a name.
        let first = prev.is_none_or(|q| q.mine != line.mine || q.author != line.author) || line.reply_to.is_some();
        if first && prev.is_some() {
            col = col.push(space().height(if phone { 14 } else { 10 }));
        }
        col = col.push(if phone { flat_message(ui, m, person, line, lines, first) } else { message(ui, m, person, line, lines, first) });
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
/// in a tinted bubble, a reply with a quote of what it answers over it.
/// Actions appear under the pointer. `lines` is the whole conversation,
/// where a reply's quote is looked up.
fn message<'a>(ui: &'a Ui, m: &'a Model, person: &'a Person, line: &'a Line, lines: &'a [Line], first: bool) -> El<'a, Msg> {
    // Everything this message registers belongs to it, so focus on any of
    // it keeps its tools within the keyboard's reach (see `a11y::group`).
    crate::a11y::group(Some(line.id));
    let el = message_body(ui, m, person, line, lines, first);
    crate::a11y::pause(false);
    crate::a11y::group(None);
    el
}

/// [`message`]'s drawing, inside its keyboard group.
fn message_body<'a>(ui: &'a Ui, m: &'a Model, person: &'a Person, line: &'a Line, lines: &'a [Line], first: bool) -> El<'a, Msg> {
    let p = theme::look();
    let editing = ui.editing.as_ref().filter(|(id, _)| *id == line.id);
    // Read out as "who: what", in the order the conversation runs.
    let who = if line.mine {
        "You"
    } else if line.author.is_empty() {
        person.name.as_str()
    } else {
        line.author.as_str()
    };
    let said = match line.file.as_ref() {
        Some((name, size)) => format!("{who} sent a file: {name}, {size}"),
        None => format!("{who}: {}", line.text),
    };
    // One stop per message; while it (or one of its tools) has focus, its
    // tools follow it in the keyboard's list and stay on screen.
    let focused = crate::a11y::Spec::new(crate::a11y::Role::Message, said).add();
    let active = ui.active_message == Some(line.id);

    let content: El<'a, Msg> = if let Some((_, draft)) = editing {
        column![
            input("Edit message", crate::EDIT_BOX, "", draft).on_input(Msg::EditDraft).on_submit(Msg::EditSave).padding([8, 10]).size(14),
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

    let mut stack_col = column![].spacing(4).align_x(if line.mine { Alignment::End } else { Alignment::Start });
    if let Some(quoted) = line.reply_to.and_then(|id| lines.iter().find(|l| l.id == id)) {
        stack_col = stack_col.push(bubble_quote(m, person, quoted));
    }
    stack_col = stack_col.push(content);
    if !line.reactions.is_empty() {
        let mut rr = row![].spacing(4);
        for (emoji, n) in &line.reactions {
            rr = rr.push(
                pressable(
                    row![t(emoji.as_str()).size(13), t(n.to_string()).size(11.5).color(p.muted)].spacing(4).align_y(Alignment::Center),
                    crate::a11y::Role::Button,
                    &format!("{emoji} {n}"),
                    false,
                    Btn::Default,
                    Msg::Do(Action::React { to: person.id.clone(), id: line.id, emoji: emoji.clone() }),
                )
                .padding([2, 8]),
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
    // The actions float over the far side of the row while the pointer is
    // on it. They are built for every message, so they join the keyboard's
    // list only for the message with focus: Tab would otherwise stop seven
    // times per message.
    crate::a11y::pause(!active);
    let mut tools = row![].spacing(2).align_y(Alignment::Center);
    for e in QUICK {
        tools = tools.push(
            pressable(t(e).size(14), crate::a11y::Role::Button, &format!("React {e}"), false, Btn::Ghost, Msg::Do(Action::React { to: person.id.clone(), id: line.id, emoji: e.to_string() }))
                .padding([3, 6]),
        );
    }
    tools = tools.push(icon_btn(Icon::Back, "Reply", Some(Msg::Reply(Some(line.id)))));
    tools = tools.push(icon_btn(Icon::Sparkle, if line.pinned { "Unpin" } else { "Pin" }, Some(Msg::Do(Action::Pin { to: person.id.clone(), id: line.id, pinned: !line.pinned }))));
    if line.mine {
        tools = tools.push(icon_btn(Icon::Edit, "Edit", Some(Msg::EditStart(line.id, line.text.clone()))));
        tools = tools.push(icon_btn(Icon::Trash, "Delete", Some(Msg::Do(Action::Delete { to: person.id.clone(), id: line.id }))));
    }
    tools = tools.push(icon_btn(Icon::Copy, "Copy", Some(Msg::Copy(line.text.clone(), "Message"))));
    crate::a11y::pause(false);
    let bar = container(tools).padding(3).style(theme::menu_pop);
    let overlay = container(bar).width(Length::Fill).align_x(if line.mine { Alignment::Start } else { Alignment::End }).padding([0, 6]);
    let row_el = crate::a11y::ring(container(body).padding([2, 0]).width(Length::Fill), focused, 10.0);
    if active {
        // Reached by keyboard: the tools stay up while focus is here.
        iced_widget::stack![row_el, overlay].into()
    } else {
        hover(row_el, overlay)
    }
}

/// One message as a phone draws it, the way the big chat apps do: every
/// message down the left, the first of a run under the writer's picture,
/// name and time, the rest just their words. A reply carries a small quote
/// of what it answers above it, a link a card below it. Tapping a message
/// brings up its tools (react, reply, copy, and edit and delete for mine);
/// on a touch screen the "pointer" stays where the finger lifted.
fn flat_message<'a>(ui: &'a Ui, m: &'a Model, person: &'a Person, line: &'a Line, lines: &'a [Line], first: bool) -> El<'a, Msg> {
    crate::a11y::group(Some(line.id));
    let el = flat_message_body(ui, m, person, line, lines, first);
    crate::a11y::pause(false);
    crate::a11y::group(None);
    el
}

/// Who wrote `line`, by name: me, the person the conversation is with, or
/// the author it names in a conversation with more people in it.
fn writer<'a>(m: &'a Model, person: &'a Person, line: &'a Line) -> &'a str {
    if line.mine {
        m.me.name.as_str()
    } else if line.author.is_empty() {
        person.name.as_str()
    } else {
        line.author.as_str()
    }
}

/// The id whose picture and colour stand for `line`'s writer.
fn writer_id<'a>(m: &'a Model, person: &'a Person, line: &'a Line) -> &'a str {
    if line.mine {
        m.me.id.as_str()
    } else if line.author_id.is_empty() {
        person.id.as_str()
    } else {
        line.author_id.as_str()
    }
}

/// [`flat_message`]'s drawing, inside its keyboard group.
fn flat_message_body<'a>(ui: &'a Ui, m: &'a Model, person: &'a Person, line: &'a Line, lines: &'a [Line], first: bool) -> El<'a, Msg> {
    let p = theme::look();
    let who = writer(m, person, line);
    let said = match line.file.as_ref() {
        Some((name, size)) => format!("{who} sent a file: {name}, {size}"),
        None => format!("{who}: {}", line.text),
    };
    let focused = crate::a11y::Spec::new(crate::a11y::Role::Message, said).add();
    let active = ui.active_message == Some(line.id);
    let editing = ui.editing.as_ref().filter(|(id, _)| *id == line.id);

    // What it says: an edit box while I change it, a voice clip or file
    // as a card, otherwise its words with a card per link under them.
    let content: El<'a, Msg> = if let Some((_, draft)) = editing {
        column![
            input("Edit message", crate::EDIT_BOX, "", draft).on_input(Msg::EditDraft).on_submit(Msg::EditSave).padding([8, 10]).size(15),
            row![fill(), btn_sm("Cancel", Btn::Ghost, Some(Msg::EditCancel)), btn_sm("Save", Btn::Primary, Some(Msg::EditSave))].spacing(6).align_y(Alignment::Center),
        ]
        .spacing(6)
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
        container(row![play, column![t("Voice message").size(14).font(theme::semibold()), faint(sub).size(12)].spacing(2)].spacing(10).align_y(Alignment::Center))
            .padding([8, 12])
            .style(theme::card_raised)
            .into()
    } else if let Some((name, size)) = line.file.as_ref() {
        container(
            row![
                container(icon(Icon::File, 18.0, p.accent2)).center(36).style(theme::inset),
                column![t(name.as_str()).size(14).font(theme::semibold()), faint(size.as_str()).size(12)].spacing(2),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .padding([10, 12])
        .style(theme::card_raised)
        .into()
    } else if line.deleted {
        faint("Message deleted").size(14).into()
    } else {
        let mut col = column![t(line.text.as_str()).size(15.5).wrapping(Wrapping::WordOrGlyph)].spacing(6);
        if line.edited {
            col = col.push(faint("(edited)").size(11.5));
        }
        for url in links(&line.text).into_iter().take(3) {
            col = col.push(link_card(url));
        }
        col.into()
    };

    let mut body = column![].spacing(3).width(Length::Fill);
    if first {
        let mut head = row![t(who).size(15.5).font(theme::bold()).color(if line.mine { p.accent2 } else { p.text }).wrapping(Wrapping::None), faint(line.time.as_str()).size(11.5)]
            .spacing(8)
            .align_y(Alignment::Center);
        if line.pinned {
            head = head.push(icon(Icon::Sparkle, 11.0, p.accent2));
        }
        body = body.push(head);
    }
    body = body.push(content);
    if !line.reactions.is_empty() {
        let mut rr = row![].spacing(4);
        for (emoji, n) in &line.reactions {
            rr = rr.push(
                pressable(
                    row![t(emoji.as_str()).size(14), t(n.to_string()).size(12).color(p.muted)].spacing(4).align_y(Alignment::Center),
                    crate::a11y::Role::Button,
                    &format!("{emoji} {n}"),
                    false,
                    Btn::Default,
                    Msg::Do(Action::React { to: person.id.clone(), id: line.id, emoji: emoji.clone() }),
                )
                .padding([3, 9]),
            );
        }
        body = body.push(rr);
    }
    if line.mine {
        let (label, color) = match line.delivery {
            Delivery::Queued => ("Waiting until they're reachable", p.warn),
            Delivery::Failed => ("Not delivered", p.err),
            _ => ("", p.faint),
        };
        if !label.is_empty() {
            body = body.push(t(label).size(11.5).color(color));
        }
    }

    let lead: El<'a, Msg> = if first { avatar(who, writer_id(m, person, line), 40.0) } else { space().width(40).into() };
    let mut whole = column![].spacing(4);
    if let Some(quoted) = line.reply_to.and_then(|id| lines.iter().find(|l| l.id == id)) {
        whole = whole.push(reply_quote(m, person, quoted));
    }
    whole = whole.push(row![lead, body].spacing(12).align_y(Alignment::Start));

    if editing.is_some() || line.deleted {
        return container(whole).padding([2, 0]).into();
    }
    // The tools, over the message's top right while it is tapped (or has
    // the keyboard's focus); only then do they join the keyboard's list.
    crate::a11y::pause(!active);
    let mut tools = row![].spacing(2).align_y(Alignment::Center);
    for e in QUICK {
        tools = tools.push(
            pressable(t(e).size(15), crate::a11y::Role::Button, &format!("React {e}"), false, Btn::Ghost, Msg::Do(Action::React { to: person.id.clone(), id: line.id, emoji: e.to_string() }))
                .padding([4, 7]),
        );
    }
    if line.file.is_none() && line.voice.is_none() {
        tools = tools.push(icon_btn(Icon::Back, "Reply", Some(Msg::Reply(Some(line.id)))));
        tools = tools.push(icon_btn(Icon::Copy, "Copy", Some(Msg::Copy(line.text.clone(), "Message"))));
        if line.mine {
            tools = tools.push(icon_btn(Icon::Edit, "Edit", Some(Msg::EditStart(line.id, line.text.clone()))));
        }
    }
    if line.mine {
        tools = tools.push(icon_btn(Icon::Trash, "Delete", Some(Msg::Do(Action::Delete { to: person.id.clone(), id: line.id }))));
    }
    crate::a11y::pause(false);
    let bar = container(tools).padding(3).style(theme::menu_pop);
    let overlay = container(bar).width(Length::Fill).align_x(Alignment::End);
    let row_el = crate::a11y::ring(container(whole).padding([2, 0]).width(Length::Fill), focused, 10.0);
    if active {
        iced_widget::stack![row_el, overlay].into()
    } else {
        hover(row_el, overlay)
    }
}

/// The small quote over a reply: a hooked line from the avatar column,
/// the quoted writer's picture and name, and the start of what they said.
fn reply_quote<'a>(m: &'a Model, person: &'a Person, quoted: &'a Line) -> El<'a, Msg> {
    let p = theme::look();
    let who = writer(m, person, quoted);
    let what = if quoted.deleted {
        "Message deleted".to_string()
    } else if let Some((name, _)) = quoted.file.as_ref() {
        format!("File: {name}")
    } else if quoted.voice.is_some() {
        "Voice message".to_string()
    } else {
        super::clip(&quoted.text, 44)
    };
    // The hook: a corner from over the avatar's middle across to the
    // quote, as chat apps join a reply to what it answers. Two hairlines
    // laid over each other, since a border can't leave out sides.
    let stroke = move |_: &iced_core::Theme| container::Style { background: Some(iced_core::Background::Color(p.faint.scale_alpha(0.7))), ..container::Style::default() };
    let hook = iced_widget::stack![container(space()).width(24).height(2).style(stroke), container(space()).width(2).height(11).style(stroke)];
    row![
        container(hook).padding(pad(10.0, 0.0, 0.0, 19.0)).width(52),
        avatar(who, writer_id(m, person, quoted), 16.0),
        t(who).size(13).font(theme::semibold()).color(p.muted).wrapping(Wrapping::None),
        container(faint(what).size(13).wrapping(Wrapping::None)).width(Length::Fill).clip(true),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

/// The quote over a reply in a wide window's bubbles: a small inset with
/// a hooked arrow, the quoted writer's name, and the start of what they
/// said, on the same side as the reply's bubble.
fn bubble_quote<'a>(m: &'a Model, person: &'a Person, quoted: &'a Line) -> El<'a, Msg> {
    let p = theme::look();
    let who = writer(m, person, quoted);
    let what = if quoted.deleted {
        "Message deleted".to_string()
    } else if let Some((name, _)) = quoted.file.as_ref() {
        format!("File: {name}")
    } else if quoted.voice.is_some() {
        "Voice message".to_string()
    } else {
        super::clip(&quoted.text, 60)
    };
    container(
        row![
            icon(Icon::Back, 12.0, p.faint),
            avatar(who, writer_id(m, person, quoted), 16.0),
            t(who).size(12.5).font(theme::semibold()).color(p.muted).wrapping(Wrapping::None),
            container(faint(what).size(12.5).wrapping(Wrapping::None)).clip(true),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([4, 10])
    .max_width(480)
    .style(theme::inset)
    .into()
}

/// The web links in `text`: each word that starts `http://` or `https://`,
/// without the punctuation a sentence may have put after it.
pub fn links(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .filter(|w| w.starts_with("https://") || w.starts_with("http://"))
        .map(|w| w.trim_end_matches(['.', ',', ')', '!', '?', ';', ':', '"', '\'']))
        .filter(|w| host(w).is_some_and(|h| !h.is_empty()))
        .collect()
}

/// The site a link points at, without `www.`: "youtube.com" for
/// `https://www.youtube.com/watch?v=…`. `None` for something that isn't a
/// web link.
pub fn host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..end];
    // Credentials before an @, and a port after a colon, are not where it
    // goes.
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    Some(host.strip_prefix("www.").unwrap_or(host))
}

/// A card under a message for a link in it: the site's name and the link
/// itself, with an accent edge down its left, as chat apps show a preview.
/// Nothing is fetched from the site: asking it for a title would tell it
/// who is talking to whom, which this app never does. A tap opens it.
fn link_card<'a>(url: &'a str) -> El<'a, Msg> {
    let p = theme::look();
    let site = host(url).unwrap_or(url);
    let words = column![
        t(site).size(12.5).color(p.muted),
        t(url).size(14.5).font(theme::semibold()).color(p.info).wrapping(Wrapping::Glyph),
        faint("Tap to open in your browser").size(12),
    ]
    .spacing(4);
    let card = container(words).padding(pad(10.0, 12.0, 10.0, 14.0)).width(Length::Fill).style(move |_| container::Style {
        background: Some(iced_core::Background::Color(p.bg2)),
        // The accent edge: a thick left border, as a quote is drawn.
        border: Border { color: p.border, width: 1.0, radius: 10.0.into() },
        shadow: iced_core::Shadow { color: p.accent2, offset: iced_core::Vector::new(-3.0, 0.0), blur_radius: 0.0 },
        ..container::Style::default()
    });
    pressable(container(card).max_width(520), crate::a11y::Role::Button, &format!("Open {site}"), false, Btn::Ghost, Msg::Do(Action::OpenLink(url.to_string())))
        .padding(pad(0.0, 0.0, 0.0, 3.0))
        .into()
}

/// A file they offered, files on their way, and those waiting their turn.
pub fn transfers<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let mut col = column![].spacing(8).padding([0, 20]);
    if let Some((name, size)) = m.transfers.offer.as_ref() {
        // The sender's own preview of a picture, when they sent one, in
        // place of the generic download mark.
        let mark: El<'a, Msg> = match &m.transfers.offer_thumbnail {
            Some(picture) => image(picture.clone()).width(56).height(56).content_fit(ContentFit::Cover).border_radius(6).into(),
            None => container(icon(Icon::Download, 18.0, p.accent2)).center(38).style(theme::inset).into(),
        };
        col = col.push(
            container(
                row![
                    mark,
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
        let focused = crate::a11y::Spec::new(crate::a11y::Role::Toggle, format!("Up next, {n} files")).checked(ui.queue_open).on(Some(&Msg::ToggleQueue)).add();
        let head = button(crate::a11y::mark(
            row![
                label("Up next"),
                faint(n.to_string()).size(11),
                fill(),
                faint(if ui.queue_open { "Hide" } else { "Show" }).size(11.5),
                icon(if ui.queue_open { Icon::Up } else { Icon::Down }, 13.0, p.faint),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            focused,
        ))
        .width(Length::Fill)
        .padding(0)
        .style(focus_style(|_, _| button::Style::default(), focused))
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
        format!("Message @{}", person.name)
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
    let mut input = input("Message", crate::COMPOSER, &placeholder, draft).size(14).padding([11, 4]).style(theme::input_bare);
    if can_send {
        input = input.on_input(Msg::Draft).on_submit(Msg::Send);
    }
    // With a queue that outlives the call, a file can be chosen for anyone
    // at any time and goes at the next call with them.
    let on_call = live && m.call.with(&person.id);
    let file_ok = m.device.files && (on_call || m.transfers.pick_anytime);
    let file_tip = match (file_ok, on_call) {
        (true, true) => "Send a file",
        (true, false) => "Send a file on your next call",
        (false, _) => "Files go during a call",
    };
    let bar = row![
        icon_btn(Icon::Paperclip, file_tip, file_ok.then_some(Msg::Do(Action::SendFile))),
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
    let mut col = column![].spacing(6);
    // Answering a message: who and what, over the bar, with a way out.
    let lines = conv.map(|c| c.lines.as_slice()).unwrap_or(&[]);
    if let Some(quoted) = ui.replying.and_then(|id| lines.iter().find(|l| l.id == id)) {
        let who = writer(m, person, quoted);
        col = col.push(
            container(
                row![
                    icon(Icon::Back, 14.0, p.muted),
                    t(format!("Replying to {who}")).size(13).color(p.muted).width(Length::Fill),
                    icon_btn(Icon::X, "Stop replying", Some(Msg::Reply(None))),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .padding(pad(0.0, 4.0, 0.0, 12.0))
            .style(theme::inset),
        );
    }
    col.push(container(bar).padding([2, 8]).style(theme::composer)).push(container(hint).padding([0, 6])).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_found_without_trailing_punctuation() {
        assert_eq!(links("see https://example.com/a?b=1, and http://x.org."), vec!["https://example.com/a?b=1", "http://x.org"]);
        assert!(links("no links, just ftp://nope and https://").is_empty());
    }

    #[test]
    fn a_link_names_its_site() {
        assert_eq!(host("https://www.youtube.com/shorts/abc"), Some("youtube.com"));
        assert_eq!(host("http://user@example.org:8080/x"), Some("example.org"));
        assert_eq!(host("stoop://abc"), None);
    }
}
