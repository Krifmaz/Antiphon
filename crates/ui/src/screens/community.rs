//! Group chats and communities: the list of them with any invitations,
//! one channel's messages, and the parts of the window around a channel
//! (its channels and members on a wide window, chips on a phone).
//!
//! A group chat and a community share these screens; a group chat is a
//! community with one channel and no roles, so it simply shows no channel
//! list. [`voice_bar`] is where a community's voice room goes (it draws
//! nothing yet); [`member_badges`] shows each member's role.

use iced_core::text::Wrapping;
use iced_core::{Alignment, Length};
use iced_widget::{column, container, hover, row, scrollable, space};

use super::{clip, fill};
use crate::a11y::{Role, Spec};
use crate::icons::{icon, Icon};
use crate::theme::{self, Btn};
use crate::widgets::*;
use crate::{Action, Channel, Community, Dialog, El, Line, Member, Model, Msg, Notify, Route, SpaceChange, Ui};

/// How wide the channel column beside a channel is.
const CHANNELS_W: f32 = 232.0;

// ------------------------------------------------------------------ list

/// Every group chat and community, invitations first, and the way to
/// start one. The phone's Spaces tab, and the wide window's page for them.
pub fn list<'a>(ui: &'a Ui, m: &'a Model, phone: bool) -> El<'a, Msg> {
    let p = theme::look();
    let mut col = column![].spacing(16);
    if !phone {
        col = col.push(
            row![
                column![h1("Group chats & communities"), muted("Talk with several people at once. Nothing sits on a server: each member's app keeps the conversation.").size(13.5)]
                    .spacing(6)
                    .width(Length::Fill),
                btn_icon(Icon::Search, "Find", Btn::Default, Some(Msg::Dialog(Some(Dialog::Directory)))),
                btn_icon(Icon::Plus, "New", Btn::Primary, Some(Msg::Dialog(Some(Dialog::NewSpace)))),
            ]
            .spacing(16)
            .align_y(Alignment::Center),
        );
    }
    if !m.space_invites.is_empty() {
        let mut invites = column![].spacing(10);
        for (i, inv) in m.space_invites.iter().enumerate() {
            invites = invites.push(
                row![
                    avatar(&inv.name, &inv.name, 36.0),
                    column![t(inv.name.as_str()).size(14).font(theme::semibold()), faint(inv.detail.as_str()).size(12)].spacing(2).width(Length::Fill),
                    btn_sm("Decline", Btn::Ghost, Some(Msg::Do(Action::DeclineSpaceInvite(i)))),
                    btn_sm("Join", Btn::Primary, Some(Msg::Do(Action::AcceptSpaceInvite(i)))),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            );
        }
        col = col.push(titled_card("Invitations", Some("Joining tells the person who invited you; they let you in when their app next hears from yours."), invites));
    }
    if m.communities.is_empty() {
        col = col.push(
            container(empty(Icon::Spaces, "None yet", "Start a group chat for a few friends, or a community with channels for more."))
                .width(Length::Fill)
                .padding([24, 0]),
        );
        if phone {
            col = col.push(container(btn_icon(Icon::Plus, "Start one", Btn::Primary, Some(Msg::Dialog(Some(Dialog::NewSpace))))).center_x(Length::Fill));
        }
    } else {
        let mut rows = column![].spacing(2);
        for c in &m.communities {
            rows = rows.push(space_row(ui, c, phone));
        }
        col = col.push(container(rows).padding(6).style(theme::card));
    }
    let _ = p;
    let body = scrollable(container(col).padding(if phone { pad(12.0, 12.0, 16.0, 12.0) } else { pad(28.0, 32.0, 28.0, 32.0) }).max_width(820))
        .height(Length::Fill)
        .style(theme::scroll);
    if phone {
        let bar = row![
            t("Spaces").size(18).font(theme::bold()),
            fill(),
            icon_btn(Icon::Search, "Find communities", Some(Msg::Dialog(Some(Dialog::Directory)))),
            icon_btn(Icon::Plus, "Start one", Some(Msg::Dialog(Some(Dialog::NewSpace)))),
        ]
            .spacing(8)
            .align_y(Alignment::Center);
        column![container(bar).height(56).padding([0, 16]).align_y(Alignment::Center), divider(), body].into()
    } else {
        body.into()
    }
}

/// One group chat or community in a list: its initial, name, a line about
/// it, and unread messages. Opens its first channel.
pub fn space_row<'a>(ui: &'a Ui, c: &'a Community, phone: bool) -> El<'a, Msg> {
    let p = theme::look();
    let selected = ui.open_channel().is_some_and(|(id, _)| id == c.id);
    let mut right = column![].align_x(Alignment::End);
    if c.unread > 0 {
        right = right.push(container(t(c.unread.to_string()).size(11).font(theme::bold()).color(theme::hex("#16161f"))).padding([1, 7]).style(theme::unread));
    }
    let name_color = if selected || c.unread > 0 { p.text } else { p.muted };
    let size = if phone { 40.0 } else { 32.0 };
    let body = row![
        avatar(&c.name, &c.id, size),
        container(
            column![
                t(clip(&c.name, 24)).size(14).font(theme::semibold()).color(name_color).wrapping(Wrapping::None),
                t(clip(c.waiting.as_deref().unwrap_or(&c.detail), 32)).size(12).color(p.faint).wrapping(Wrapping::None),
            ]
            .spacing(1)
        )
        .width(Length::Fill)
        .clip(true),
        right,
    ]
    .spacing(11)
    .align_y(Alignment::Center);
    let first = c.channels.first().map(|ch| ch.id).unwrap_or(0);
    let name = if c.unread > 0 { format!("{}, {} unread", c.name, c.unread) } else { c.name.clone() };
    pressable(container(body).clip(true), Role::Tab, &name, selected, if selected { Btn::RowOn } else { Btn::Row }, Msg::Nav(Route::Community(c.id.clone(), first)))
        .width(Length::Fill)
        .padding(if phone { [10, 10] } else { [6, 10] })
        .into()
}

// --------------------------------------------------------------- channel

/// A channel on a wide window: its channels and members on the left,
/// the messages and the composer on the right.
pub fn view<'a>(ui: &'a Ui, m: &'a Model, id: &'a str, channel: u32) -> El<'a, Msg> {
    let Some(c) = m.communities.iter().find(|c| c.id == id) else {
        return container(empty(Icon::Spaces, "Not here any more", "You may have left it, or it was forgotten on this device.")).center(Length::Fill).into();
    };
    let ch = c.channels.iter().find(|x| x.id == channel).or(c.channels.first());
    let main = column![
        header(c, ch, false),
        divider(),
        voice_bar(c, ch),
        container(messages(ui, m, c, ch, false)).height(Length::Fill),
        container(composer(ui, c, ch)).padding(pad(4.0, 20.0, 18.0, 20.0)),
    ]
    .height(Length::Fill);
    row![container(side(ui, c, channel)).width(CHANNELS_W).height(Length::Fill).style(theme::stage_flat), vdivider(), main].into()
}

/// A channel on a phone: a back arrow and the name on top, the channels
/// as chips to switch between, the messages, and the composer.
pub fn phone<'a>(ui: &'a Ui, m: &'a Model, id: &'a str, channel: u32) -> El<'a, Msg> {
    let Some(c) = m.communities.iter().find(|c| c.id == id) else {
        return list(ui, m, true);
    };
    let ch = c.channels.iter().find(|x| x.id == channel).or(c.channels.first());
    let mut col = column![header(c, ch, true), divider()];
    if !c.group && c.channels.len() > 1 {
        let mut chips = row![].spacing(6);
        for x in &c.channels {
            let on = Some(x.id) == ch.map(|h| h.id);
            chips = chips.push(
                pressable(
                    row![icon(Icon::Hash, 13.0, if on { theme::look().text } else { theme::look().muted }), t(x.name.as_str()).size(13).font(theme::semibold())].spacing(4).align_y(Alignment::Center),
                    Role::Tab,
                    &format!("#{}", x.name),
                    on,
                    if on { Btn::TabOn } else { Btn::Tab },
                    Msg::Nav(Route::Community(c.id.clone(), x.id)),
                )
                .padding([6, 12]),
            );
        }
        col = col.push(scrollable(container(chips).padding([8, 10])).direction(scrollable::Direction::Horizontal(scrollable::Scrollbar::new().width(0).scroller_width(0))));
    }
    col = col.push(voice_bar(c, ch));
    col = col.push(container(messages(ui, m, c, ch, true)).height(Length::Fill));
    col = col.push(container(composer(ui, c, ch)).padding(pad(4.0, 10.0, 12.0, 10.0)));
    col.into()
}

/// The bar over a channel: which channel, what it is for, and what you
/// can do (invite, leave).
fn header<'a>(c: &'a Community, ch: Option<&'a Channel>, phone: bool) -> El<'a, Msg> {
    let p = theme::look();
    let mut r = row![].spacing(10).align_y(Alignment::Center);
    if phone {
        r = r.push(icon_btn(Icon::Back, "", Some(Msg::Back)));
    }
    let (title, sub): (El<'a, Msg>, String) = match ch {
        // A group chat is one conversation: its own name, no hash.
        Some(_) if c.group => (t(c.name.as_str()).size(16).font(theme::bold()).wrapping(Wrapping::None).into(), c.detail.clone()),
        Some(ch) => (
            row![icon(Icon::Hash, 16.0, p.muted), t(ch.name.as_str()).size(16).font(theme::bold()).wrapping(Wrapping::None)].spacing(4).align_y(Alignment::Center).into(),
            if ch.topic.is_empty() { c.name.clone() } else { format!("{} · {}", c.name, ch.topic) },
        ),
        None => (t(c.name.as_str()).size(16).font(theme::bold()).into(), c.detail.clone()),
    };
    if phone || c.group {
        r = r.push(avatar(&c.name, &c.id, 32.0));
    }
    r = r.push(column![title, faint(clip(c.waiting.as_deref().unwrap_or(&sub), 60)).size(12).wrapping(Wrapping::None)].spacing(1).width(Length::Fill));
    if c.moderator || c.can_set_slow_mode || !c.pending_invites.is_empty() {
        let tip = if c.reports.is_empty() { "Moderation" } else { "Moderation: reports waiting" };
        let kind = if c.reports.is_empty() { Btn::Icon } else { Btn::RowOn };
        r = r.push(icon_btn_kind(Icon::ShieldCheck, tip, kind, Some(Msg::Dialog(Some(Dialog::Moderation(c.id.clone()))))));
    }
    let invite = c.can_invite.then(|| Msg::Dialog(Some(Dialog::InviteToSpace(c.id.clone()))));
    if phone {
        r = r.push(icon_btn(Icon::UserPlus, "Invite", invite));
    } else {
        r = r.push(btn_icon(Icon::UserPlus, "Invite", Btn::Default, invite));
    }
    let quiet = c.notify != Notify::All;
    r = r.push(icon_btn_kind(
        if c.notify == Notify::Nothing { Icon::BellOff } else { Icon::Bell },
        "Notifications",
        if quiet { Btn::IconOn } else { Btn::Icon },
        Some(Msg::Dialog(Some(Dialog::SpaceNotify(c.id.clone())))),
    ));
    r = r.push(icon_btn(Icon::LogOut, "Leave", Some(Msg::Dialog(Some(Dialog::ConfirmLeave(c.id.clone()))))));
    container(r).height(theme::HEADER_H).padding([0, if phone { 8 } else { 20 }]).align_y(Alignment::Center).into()
}

/// The column beside a channel: the community's name, its channels, and
/// who is in it.
fn side<'a>(ui: &'a Ui, c: &'a Community, channel: u32) -> El<'a, Msg> {
    let p = theme::look();
    let top = container(
        row![
            avatar(&c.name, &c.id, 30.0),
            column![t(clip(&c.name, 18)).size(14.5).font(theme::bold()).wrapping(Wrapping::None), faint(clip(&c.detail, 26)).size(11.5).wrapping(Wrapping::None)].spacing(1).width(Length::Fill),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .height(theme::HEADER_H)
    .padding([0, 14])
    .align_y(Alignment::Center);
    let mut col = column![].spacing(2);
    if !c.group {
        let mut head = row![label("Channels"), fill()].align_y(Alignment::Center);
        if c.can_manage {
            head = head.push(icon_btn(Icon::Smile, "Custom emoji", Some(Msg::Dialog(Some(Dialog::SpaceEmoji(c.id.clone()))))));
        }
        if c.can_manage_channels {
            head = head.push(icon_btn(Icon::Plus, "Add a channel", Some(Msg::Dialog(Some(Dialog::AddChannel(c.id.clone()))))));
        }
        col = col.push(container(head).padding(pad(10.0, 4.0, 4.0, 8.0)));
        for x in &c.channels {
            let on = x.id == channel;
            // A private channel shows a lock where the others show a hash.
            let mark = if x.voice {
                Icon::Volume
            } else if x.private {
                Icon::Lock
            } else {
                Icon::Hash
            };
            let name = if x.private { format!("#{}, private", x.name) } else { format!("#{}", x.name) };
            let entry = pressable(
                row![icon(mark, 15.0, if on { p.text } else { p.faint }), t(clip(&x.name, 20)).size(13.5).font(theme::semibold()).color(if on { p.text } else { p.muted })]
                    .spacing(8)
                    .align_y(Alignment::Center),
                Role::Tab,
                &name,
                on,
                if on { Btn::RowOn } else { Btn::Row },
                Msg::Nav(Route::Community(c.id.clone(), x.id)),
            )
            .width(Length::Fill)
            .padding([6, 10]);
            if c.can_manage_channels {
                let gear = icon_btn(Icon::Gear, "Who can read it", Some(Msg::Dialog(Some(Dialog::ChannelSettings(c.id.clone(), x.id)))));
                col = col.push(row![entry, gear].spacing(2).align_y(Alignment::Center));
            } else {
                col = col.push(entry);
            }
        }
    }
    let mut members_head = row![label("Members"), faint(c.members.len().to_string()).size(11), fill()].spacing(8).align_y(Alignment::Center);
    if !c.group && (c.can_edit_roles || c.can_assign_roles) {
        members_head = members_head.push(icon_btn(Icon::Shield, "Roles", Some(Msg::Dialog(Some(Dialog::Roles(c.id.clone()))))));
    }
    col = col.push(container(members_head).padding(pad(16.0, 4.0, 4.0, 8.0)));
    for member in &c.members {
        col = col.push(member_row(member));
    }
    let _ = ui;
    column![top, divider(), scrollable(container(col).padding([4, 8])).height(Length::Fill).style(theme::scroll)].into()
}

/// One member: avatar, name, role and status.
fn member_row<'a>(member: &'a Member) -> El<'a, Msg> {
    let p = theme::look();
    let name = if member.me { format!("{} (you)", member.name) } else { member.name.clone() };
    let mut line = row![t(clip(&name, 20)).size(13).font(theme::semibold()).wrapping(Wrapping::None)].spacing(6).align_y(Alignment::Center);
    line = line.push(member_badges(member));
    let sub = if member.status.is_empty() { member.role.as_str() } else { member.status.as_str() };
    container(
        row![avatar(&member.name, &member.id, 28.0), column![line, t(clip(sub, 26)).size(11.5).color(p.faint).wrapping(Wrapping::None)].spacing(1).width(Length::Fill)]
            .spacing(10)
            .align_y(Alignment::Center),
    )
    .padding([5, 8])
    .into()
}

/// Over a voice channel's messages: who is in it ("Voice active · 3
/// people"), and Join or Leave. Nothing over a text channel.
pub fn voice_bar<'a>(c: &'a Community, ch: Option<&'a Channel>) -> El<'a, Msg> {
    let p = theme::look();
    let Some(ch) = ch.filter(|x| x.voice) else { return space().height(0).into() };
    let change = |change: SpaceChange| Msg::Do(Action::ManageSpace { community: c.id.clone(), change });
    let n = ch.in_voice.len();
    let (title, names) = if n == 0 {
        ("Nobody's here yet".to_string(), "Join, and whoever joins next is put through to you.".to_string())
    } else {
        (format!("Voice active · {n} {}", if n == 1 { "person" } else { "people" }), clip(&ch.in_voice.join(", "), 80))
    };
    let here = c.voice_room == Some(ch.id);
    let action = if here {
        btn_icon(Icon::PhoneOff, "Leave", Btn::Danger, Some(change(SpaceChange::LeaveVoice(ch.id))))
    } else {
        btn_icon(Icon::Phone, "Join", Btn::Primary, (c.waiting.is_none()).then(|| change(SpaceChange::JoinVoice(ch.id))))
    };
    let bar = row![
        icon(Icon::Volume, 18.0, if n > 0 { p.ok } else { p.faint }),
        column![t(title).size(13.5).font(theme::semibold()), faint(names).size(12).wrapping(Wrapping::None)].spacing(1).width(Length::Fill),
        action,
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    column![container(bar).padding([10, 20]).width(Length::Fill).style(theme::inset), divider()].into()
}

/// A member's role, after their name, unless it is the plain one
/// newcomers get (which says nothing about them).
pub fn member_badges<'a>(member: &'a Member) -> El<'a, Msg> {
    match member.role_index {
        _ if member.plain || member.role.is_empty() => space().width(0).into(),
        None => badge(member.role.as_str(), theme::Badge::Accent),
        Some(_) => badge(member.role.as_str(), theme::Badge::Plain),
    }
}

/// The start of the channel, then every message, newest at the bottom.
fn messages<'a>(ui: &'a Ui, m: &'a Model, c: &'a Community, ch: Option<&'a Channel>, phone: bool) -> El<'a, Msg> {
    let side = if phone { 12.0 } else { 24.0 };
    let mut col = column![intro(c, ch)].spacing(2).padding(pad(18.0, side, 12.0, side));
    let channel = ch.map(|x| x.id).unwrap_or(0);
    let lines: &[Line] = m.channel.as_ref().filter(|v| v.community == c.id && v.channel == channel).map(|v| v.lines.as_slice()).unwrap_or(&[]);
    let mut prev: Option<&Line> = None;
    for line in lines {
        if let Some(day) = line.day.as_deref() {
            col = col.push(day_rule(day));
            prev = None;
        }
        let first = prev.is_none_or(|q| q.author_id != line.author_id);
        if first && prev.is_some() {
            col = col.push(space().height(8));
        }
        col = col.push(message(ui, c, channel, line, first, phone));
        prev = Some(line);
    }
    if lines.is_empty() {
        col = col.push(container(faint("No messages yet. Say hello.").size(12.5)).padding([12, 0]));
    }
    scrollable(col).anchor_bottom().height(Length::Fill).width(Length::Fill).style(theme::scroll).into()
}

/// The big name at the top of a channel's history.
fn intro<'a>(c: &'a Community, ch: Option<&'a Channel>) -> El<'a, Msg> {
    let (title, line) = match ch {
        Some(ch) if !c.group => (format!("Welcome to #{}", ch.name), if ch.topic.is_empty() { format!("This is the start of #{} in {}.", ch.name, c.name) } else { ch.topic.clone() }),
        _ => (c.name.clone(), format!("This is the start of {}. Everyone in it reads what's posted here, and nobody else can.", c.name)),
    };
    column![avatar(&c.name, &c.id, 64.0), h1(title), muted(line).size(13.5)].spacing(10).padding(pad(8.0, 0.0, 22.0, 0.0)).into()
}

/// "Today", centred between two hairlines.
fn day_rule<'a>(day: &'a str) -> El<'a, Msg> {
    row![container(divider()).width(Length::Fill), faint(day).size(11.5), container(divider()).width(Length::Fill)]
        .spacing(12)
        .align_y(Alignment::Center)
        .padding([14, 0])
        .into()
}

/// One message, everyone's on the left: the author's avatar and name on
/// the first of a run, then the words. Actions appear under the pointer.
fn message<'a>(ui: &'a Ui, c: &'a Community, channel: u32, line: &'a Line, first: bool, phone: bool) -> El<'a, Msg> {
    // Its own keyboard group, as in a conversation (see `a11y::group`).
    crate::a11y::group(Some(line.id));
    let el = message_body(ui, c, channel, line, first, phone);
    crate::a11y::pause(false);
    crate::a11y::group(None);
    el
}

/// [`message`]'s drawing, inside its keyboard group.
fn message_body<'a>(ui: &'a Ui, c: &'a Community, channel: u32, line: &'a Line, first: bool, phone: bool) -> El<'a, Msg> {
    let p = theme::look();
    let editing = ui.editing.as_ref().filter(|(id, _)| *id == line.id);
    // Read out as "who: what", in the order the channel runs.
    let who = if line.mine { "You" } else { line.author.as_str() };
    let said = if line.deleted { format!("{who}: message deleted") } else { format!("{who}: {}", line.text) };
    let focused = Spec::new(Role::Message, said).add();
    let active = ui.active_message == Some(line.id);
    let content: El<'a, Msg> = if let Some((_, draft)) = editing {
        column![
            input("Edit message", crate::EDIT_BOX, "", draft).on_input(Msg::EditDraft).on_submit(Msg::EditSave).padding([8, 10]).size(14),
            row![faint("Enter to save · Esc to cancel").size(11), fill(), btn_sm("Cancel", Btn::Ghost, Some(Msg::EditCancel)), btn_sm("Save", Btn::Primary, Some(Msg::EditSave))]
                .spacing(6)
                .align_y(Alignment::Center),
        ]
        .spacing(6)
        .max_width(520)
        .into()
    } else {
        let words: El<'a, Msg> = if line.deleted { t("Message deleted").size(13.5).color(p.faint).into() } else { with_emoji(&line.text, c) };
        let mut words = column![words].spacing(2);
        if line.edited {
            words = words.push(faint("edited").size(10.5));
        }
        words.into()
    };
    let mut body = column![].spacing(2).width(Length::Fill);
    if first {
        let who = if line.mine { "You" } else { line.author.as_str() };
        body = body.push(row![t(who).size(13.5).font(theme::semibold()), faint(line.time.as_str()).size(11)].spacing(8).align_y(Alignment::Center));
    }
    body = body.push(content);
    let lead: El<'a, Msg> = if first { avatar(&line.author, &line.author_id, 34.0) } else { space().width(34).into() };
    let row_el = container(row![lead, body].spacing(12).align_y(Alignment::Start)).padding([3, 0]).width(Length::Fill);
    if editing.is_some() || phone || line.deleted {
        return row_el.into();
    }
    // Tools built for every message: in the keyboard's list only for the
    // message with focus, as in a conversation.
    crate::a11y::pause(!active);
    let mut tools = row![].spacing(2).align_y(Alignment::Center);
    tools = tools.push(icon_btn(Icon::Copy, "Copy", Some(Msg::Copy(line.text.clone(), "Message"))));
    if !line.mine && c.waiting.is_none() && !c.group {
        tools = tools.push(icon_btn(Icon::Info, "Report to the moderators", Some(Msg::Dialog(Some(Dialog::Report(c.id.clone(), channel, line.id))))));
    }
    if line.mine && c.waiting.is_none() {
        tools = tools.push(icon_btn(Icon::Edit, "Edit", Some(Msg::EditStart(line.id, line.text.clone()))));
        tools = tools.push(icon_btn(
            Icon::Trash,
            "Delete",
            Some(Msg::Do(Action::DeletePost { community: c.id.clone(), id: line.id, author: line.author_id.clone() })),
        ));
    }
    crate::a11y::pause(false);
    let bar = container(tools).padding(3).style(theme::menu_pop);
    let overlay = container(bar).width(Length::Fill).align_x(Alignment::End).padding([0, 6]);
    let row_el = crate::a11y::ring(row_el, focused, 10.0);
    if active {
        iced_widget::stack![row_el, overlay].into()
    } else {
        hover(row_el, overlay)
    }
}

/// Where a channel's messages are typed.
fn composer<'a>(ui: &'a Ui, c: &'a Community, ch: Option<&'a Channel>) -> El<'a, Msg> {
    let p = theme::look();
    let placeholder = match (c.waiting.as_deref(), ch) {
        (Some(why), _) => why.to_string(),
        _ if !c.can_post => "You can't post here".to_string(),
        (None, Some(ch)) if !c.group => format!("Message #{}", ch.name),
        _ => format!("Message {}", c.name),
    };
    let key = format!("#{}/{}", c.id, ch.map(|x| x.id).unwrap_or(0));
    let draft = ui.drafts.get(&key).map(String::as_str).unwrap_or("");
    let can_send = c.can_post && c.waiting.is_none();
    let mut input = input("Message", crate::COMPOSER, &placeholder, draft).size(14).padding([11, 4]).style(theme::input_bare);
    if can_send {
        input = input.on_input(Msg::Draft).on_submit(Msg::Send);
    }
    let picker = (!c.emoji.is_empty() && can_send).then(|| Msg::Dialog(Some(Dialog::EmojiPicker(c.id.clone()))));
    let bar = row![
        icon_btn(Icon::Smile, "Emoji", picker),
        container(input).width(Length::Fill).padding([0, 6]),
        icon_btn_kind(Icon::Send, "Send", if draft.trim().is_empty() { Btn::Icon } else { Btn::RowOn }, (can_send && !draft.trim().is_empty()).then_some(Msg::Send)),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    let hint = row![icon(Icon::Lock, 11.0, p.faint), faint("End-to-end encrypted to everyone in it · It reaches people who are away when they're next online").size(11)]
        .spacing(5)
        .align_y(Alignment::Center);
    column![container(bar).padding([2, 8]).style(theme::composer), container(hint).padding([0, 6])].spacing(6).into()
}

// ---------------------------------------------------------------- dialogs

/// Name it, and pick a group chat or a community.
pub fn new_space<'a>(ui: &'a Ui) -> El<'a, Msg> {
    let kind = tabs(vec![("Community", Msg::SpaceGroup(false)), ("Group chat", Msg::SpaceGroup(true))], usize::from(ui.space_group));
    let about = if ui.space_group {
        "One conversation for a few people. Anyone in it can invite others."
    } else {
        "Channels for different topics, with owners and moderators who decide who can invite, post and remove messages."
    };
    column![
        kind,
        muted(about).size(13.5),
        field(
            "Name",
            input("Name", "stoop-space-name", if ui.space_group { "Saturday crew" } else { "The listening club" }, &ui.space_name)
                .on_input(Msg::SpaceName)
                .on_submit(Msg::SpaceCreate)
                .padding([10, 12])
                .size(14)
        ),
        faint("You can invite people from inside once it's made.").size(12),
    ]
    .spacing(14)
    .into()
}

/// Pick one of your people to invite.
pub fn invite<'a>(m: &'a Model, id: &'a str) -> El<'a, Msg> {
    let Some(c) = m.communities.iter().find(|c| c.id == id) else { return faint("It's gone.").into() };
    let mut col = column![muted(format!("They get an invitation to {}, and are let in once they accept.", c.name)).size(13.5)].spacing(6);
    let mut any = false;
    for person in &m.people {
        // Already in it under their main key: not offered again.
        if c.members.iter().any(|x| x.id == person.id) {
            continue;
        }
        any = true;
        col = col.push(
            pressable(
                row![avatar(&person.name, &person.id, 32.0), t(person.name.as_str()).size(14).font(theme::semibold()).width(Length::Fill), faint("Invite").size(12.5)]
                    .spacing(10)
                    .align_y(Alignment::Center),
                Role::Button,
                &format!("Invite {}", person.name),
                false,
                Btn::Row,
                Msg::Do(Action::InviteToSpace { community: c.id.clone(), person: person.id.clone() }),
            )
            .width(Length::Fill)
            .padding([6, 8]),
        );
    }
    if !any {
        col = col.push(faint("Everyone you know is in it already, or you haven't added anyone yet.").size(12.5));
    }
    scrollable(col).height(Length::Shrink).style(theme::scroll).into()
}

/// Which messages notify you: for the whole space, then (in a community)
/// for each channel, which can follow the space or choose for itself.
pub fn notify<'a>(m: &'a Model, id: &'a str) -> El<'a, Msg> {
    let Some(c) = m.communities.iter().find(|c| c.id == id) else { return faint("It's gone.").into() };
    let level = |l: Notify| match l {
        Notify::All => 0,
        Notify::Mentions => 1,
        Notify::Nothing => 2,
    };
    let set = |channel: Option<u32>, l: Option<Notify>| Msg::Do(Action::SpaceNotify { community: c.id.clone(), channel, level: l });
    let whole = tabs(
        vec![("Every message", set(None, Some(Notify::All))), ("Mentions only", set(None, Some(Notify::Mentions))), ("Nothing", set(None, Some(Notify::Nothing)))],
        level(c.notify),
    );
    let mut col = column![
        muted(format!("A mention is your name in {} after an @. Unread counts go up either way.", c.name)).size(13.5),
        field(if c.group { "Notify me about" } else { "For the whole community" }, whole),
    ]
    .spacing(14);
    if !c.group && !c.channels.is_empty() {
        let mut rows = column![label("Channels")].spacing(8);
        for ch in &c.channels {
            let on = ch.notify.map_or(0, |l| level(l) + 1);
            rows = rows.push(
                row![
                    row![icon(Icon::Hash, 14.0, theme::look().muted), t(clip(&ch.name, 18)).size(13.5).font(theme::semibold())].spacing(4).align_y(Alignment::Center).width(Length::Fill),
                    tabs(
                        vec![
                            ("Same", set(Some(ch.id), None)),
                            ("All", set(Some(ch.id), Some(Notify::All))),
                            ("Mentions", set(Some(ch.id), Some(Notify::Mentions))),
                            ("Off", set(Some(ch.id), Some(Notify::Nothing))),
                        ],
                        on
                    ),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            );
        }
        col = col.push(rows);
    }
    col.into()
}

/// A message's words with its community's `:name:` emoji drawn as
/// pictures. A message of nothing but emoji draws them large. Words that
/// aren't a known emoji stay words, so `:unknown:` reads as typed.
fn with_emoji<'a>(text: &'a str, c: &'a Community) -> El<'a, Msg> {
    let emoji_at = |word: &str| -> Option<iced_core::image::Handle> {
        let name = word.strip_prefix(':')?.strip_suffix(':')?;
        c.emoji.iter().any(|e| e == name).then(|| crate::avatars::picture(&crate::avatars::emoji_id(&c.id, name)))?
    };
    let words: Vec<&str> = text.split(' ').collect();
    if c.emoji.is_empty() || !words.iter().any(|w| emoji_at(w).is_some()) {
        return t(text).size(14).wrapping(Wrapping::WordOrGlyph).into();
    }
    let only_emoji = words.iter().all(|w| w.is_empty() || emoji_at(w).is_some());
    let size = if only_emoji { 40.0 } else { 20.0 };
    let mut r = row![].spacing(0).align_y(Alignment::Center);
    for (i, w) in words.iter().enumerate() {
        let gap = if i + 1 < words.len() { " " } else { "" };
        match emoji_at(w) {
            Some(handle) => {
                r = r.push(with_tip(iced_widget::image(handle).width(size).height(size), *w, iced_widget::tooltip::Position::Top));
                if !gap.is_empty() {
                    r = r.push(t(gap).size(14));
                }
            }
            None => r = r.push(t(format!("{w}{gap}")).size(14)),
        }
    }
    r.wrap().into()
}

/// The community's emoji to pick from; one goes into the message.
pub fn emoji_picker<'a>(m: &'a Model, id: &'a str) -> El<'a, Msg> {
    let Some(c) = m.communities.iter().find(|c| c.id == id) else { return faint("It's gone.").into() };
    let mut grid = row![].spacing(6);
    for name in &c.emoji {
        let face: El<'a, Msg> = match crate::avatars::picture(&crate::avatars::emoji_id(&c.id, name)) {
            Some(h) => iced_widget::image(h).width(32).height(32).into(),
            None => t(format!(":{name}:")).size(12).into(),
        };
        let b = pressable(container(face).center(40), Role::Button, &format!(":{name}:"), false, Btn::Ghost, Msg::InsertEmoji(name.clone())).padding(2);
        grid = grid.push(with_tip(b, format!(":{name}:"), iced_widget::tooltip::Position::Top));
    }
    let mut col = column![grid.wrap()].spacing(14);
    if c.can_manage {
        col = col.push(row![fill(), btn_sm("Manage emoji", Btn::Ghost, Some(Msg::Dialog(Some(Dialog::SpaceEmoji(c.id.clone())))))]);
    }
    col.into()
}

/// A community's custom emoji, for someone who may change them: each with
/// its name and a way to remove it, and adding another.
pub fn emoji_manager<'a>(m: &'a Model, id: &'a str) -> El<'a, Msg> {
    let Some(c) = m.communities.iter().find(|c| c.id == id) else { return faint("It's gone.").into() };
    let mut col = column![muted(format!(
        "Everyone in {} sees these, written :name: in a message. Pick a small, square picture; its file name becomes the emoji's name. Up to 16.",
        c.name
    ))
    .size(13.5)]
    .spacing(8);
    for name in &c.emoji {
        let face: El<'a, Msg> = match crate::avatars::picture(&crate::avatars::emoji_id(&c.id, name)) {
            Some(h) => iced_widget::image(h).width(28).height(28).into(),
            None => space().width(28).into(),
        };
        col = col.push(
            row![
                face,
                t(format!(":{name}:")).size(13.5).font(theme::mono()).width(Length::Fill),
                icon_btn(Icon::Trash, "Remove", c.can_manage.then(|| Msg::Do(Action::RemoveEmoji { community: c.id.clone(), name: name.clone() }))),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
    }
    if c.emoji.is_empty() {
        col = col.push(faint("None yet.").size(12.5));
    }
    let full = c.emoji.len() >= 16;
    col = col.push(row![fill(), btn_icon(Icon::Plus, "Add emoji", Btn::Primary, (c.can_manage && !full).then(|| Msg::Do(Action::AddEmoji(c.id.clone()))))]);
    col.into()
}

/// Name a new channel.
pub fn add_channel<'a>(ui: &'a Ui, id: &'a str) -> El<'a, Msg> {
    column![
        muted("Everyone in the community can read and post in it. You can make it private afterwards, from its gear.").size(13.5),
        setting("Voice channel", Some("A standing call people drop in and out of, as well as messages."), switch("Voice channel", ui.channel_voice, Msg::ChannelVoice)),
        field(
            "Name",
            input("Channel name", "stoop-channel-name", "band-practice", &ui.channel_name).on_input(Msg::ChannelName).on_submit(Msg::ChannelCreate(id.to_string())).padding([10, 12]).size(14)
        ),
    ]
    .spacing(14)
    .into()
}

/// Who can read a channel: open to everyone, or private to the members
/// ticked here (owners always can).
pub fn channel_settings<'a>(m: &'a Model, id: &'a str, channel: u32) -> El<'a, Msg> {
    let Some(c) = m.communities.iter().find(|c| c.id == id) else { return faint("It's gone.").into() };
    let Some(ch) = c.channels.iter().find(|x| x.id == channel) else { return faint("That channel is gone.").into() };
    let change = |change: SpaceChange| Msg::Do(Action::ManageSpace { community: c.id.clone(), change });
    let private = ch.private;
    let mut col = column![setting(
        "Private",
        Some("Only the people ticked below, and the owners, get its messages. Nobody else sees it at all."),
        switch("Private", private, move |on| change(SpaceChange::ChannelPrivate { channel, private: on })),
    )]
    .spacing(10);
    if private {
        col = col.push(label("Who can read it"));
        for member in &c.members {
            let owner = member.role_index.is_none();
            let reads = owner || ch.readers.contains(&member.id);
            let sub = if owner { "Owner: always can" } else { member.role.as_str() };
            let mut r = row![avatar(&member.name, &member.id, 28.0), column![t(clip(&member.name, 24)).size(13.5).font(theme::semibold()), faint(sub).size(11.5)].spacing(1).width(Length::Fill)]
                .spacing(10)
                .align_y(Alignment::Center);
            if !owner {
                let who = member.id.clone();
                r = r.push(switch(&format!("{} can read it", member.name), reads, move |on| change(SpaceChange::ChannelReader { channel, member: who.clone(), on })));
            }
            col = col.push(r);
        }
    }
    scrollable(col).height(Length::Shrink).style(theme::scroll).into()
}

/// A community's roles, most powerful first: what each may do (owners
/// change that), a way to add one, and who holds which.
pub fn roles<'a>(ui: &'a Ui, m: &'a Model, id: &'a str) -> El<'a, Msg> {
    let Some(c) = m.communities.iter().find(|c| c.id == id) else { return faint("It's gone.").into() };
    let change = |change: SpaceChange| Msg::Do(Action::ManageSpace { community: c.id.clone(), change });
    let mut col = column![muted("Higher roles outrank lower ones: someone can only change the role of people below them, and only to a role below their own.").size(13)].spacing(12);
    let last = c.roles.len().saturating_sub(1);
    for (i, role) in c.roles.iter().enumerate() {
        let holders = format!("{} {}", role.holders, if role.holders == 1 { "member" } else { "members" });
        let mut head = row![t(role.name.as_str()).size(14).font(theme::semibold()), faint(holders).size(12), fill()].spacing(8).align_y(Alignment::Center);
        if i == last {
            head = head.push(faint("Newcomers get this").size(11.5));
        } else if c.can_edit_roles {
            head = head.push(icon_btn(Icon::Trash, "Remove this role", Some(change(SpaceChange::RemoveRole(i)))));
        }
        let mut perms = column![].spacing(6);
        for (n, (what, on)) in role.perms.iter().enumerate() {
            let toggle: El<'a, Msg> = if c.can_edit_roles {
                switch(&format!("{}: {what}", role.name), *on, move |on| change(SpaceChange::RolePerm { role: i, perm: n, on }))
            } else {
                faint(if *on { "Yes" } else { "No" }).size(12).into()
            };
            perms = perms.push(row![t(what.as_str()).size(13).width(Length::Fill), toggle].spacing(10).align_y(Alignment::Center));
        }
        col = col.push(card(column![head, perms].spacing(8)).padding([10, 12]));
    }
    if c.can_edit_roles {
        col = col.push(
            row![
                input("New role name", "stoop-role-name", "New role, e.g. Helper", &ui.role_name).on_input(Msg::RoleName).on_submit(Msg::RoleCreate(c.id.clone())).padding([8, 10]).size(13.5),
                btn_sm("Add role", Btn::Primary, (!ui.role_name.trim().is_empty()).then(|| Msg::RoleCreate(c.id.clone()))),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }
    let mut people = column![label("Who holds which")].spacing(8);
    for member in &c.members {
        let mut r = row![avatar(&member.name, &member.id, 26.0), t(clip(&member.name, 18)).size(13).font(theme::semibold()).width(Length::Fill)].spacing(8).align_y(Alignment::Center);
        if member.assignable.is_empty() {
            r = r.push(faint(member.role.as_str()).size(12));
        } else {
            let mut chips = row![].spacing(4);
            for &at in &member.assignable {
                let name = c.roles.get(at).map(|r| r.name.as_str()).unwrap_or("");
                let on = member.role_index == Some(at);
                let pick = (!on).then(|| change(SpaceChange::MemberRole { member: member.id.clone(), role: at }));
                chips = chips.push(btn_sm(name, if on { Btn::Primary } else { Btn::Ghost }, pick));
            }
            r = r.push(chips);
        }
        people = people.push(r);
    }
    col = col.push(people);
    scrollable(col).height(Length::Fixed(520.0)).style(theme::scroll).into()
}

/// Report messages: the one picked and any others of the channel's
/// latest, with why. Only the moderators get it.
pub fn report<'a>(ui: &'a Ui, m: &'a Model, id: &'a str, channel: u32) -> El<'a, Msg> {
    let lines: &[Line] = m.channel.as_ref().filter(|v| v.community == id && v.channel == channel).map(|v| v.lines.as_slice()).unwrap_or(&[]);
    let mut col = column![muted("The messages you tick go, as your app shows them, to the people here who can remove messages or members. Nobody else sees the report.").size(13)].spacing(10);
    let mut list = column![].spacing(4);
    for line in lines.iter().rev().filter(|l| !l.deleted && !l.mine).take(20).collect::<Vec<_>>().into_iter().rev() {
        let on = ui.report_pick.contains(&line.id);
        list = list.push(
            row![
                avatar(&line.author, &line.author_id, 24.0),
                column![t(line.author.as_str()).size(12.5).font(theme::semibold()), t(clip(&line.text, 120)).size(13).wrapping(Wrapping::WordOrGlyph)].spacing(1).width(Length::Fill),
                switch(&format!("Report {}'s message", line.author), on, { let id = line.id; move |_| Msg::ReportPick(id) }),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
    }
    col = col.push(scrollable(list).height(Length::Fixed(280.0)).style(theme::scroll));
    col = col.push(field(
        "Why (optional)",
        input("Why you're reporting it", "stoop-report-reason", "Harassing another member", &ui.report_reason).on_input(Msg::ReportReason).padding([8, 10]).size(13.5),
    ));
    col.into()
}

/// Slow mode, being listed, the reports waiting, and the invitations you
/// sent.
pub fn moderation<'a>(ui: &'a Ui, m: &'a Model, id: &'a str) -> El<'a, Msg> {
    let Some(c) = m.communities.iter().find(|c| c.id == id) else { return faint("It's gone.").into() };
    let change = |change: SpaceChange| Msg::Do(Action::ManageSpace { community: c.id.clone(), change });
    let mut col = column![].spacing(14);
    if c.can_set_slow_mode {
        const STEPS: [(&str, u32); 6] = [("Off", 0), ("10 s", 10), ("30 s", 30), ("1 min", 60), ("5 min", 300), ("15 min", 900)];
        let on = STEPS.iter().position(|(_, s)| *s == c.slow_mode_secs).unwrap_or(0);
        let picks = STEPS.iter().map(|(label, secs)| (*label, change(SpaceChange::SlowMode(*secs)))).collect();
        col = col.push(column![
            setting("Slow mode", Some("How long each member waits between posts. Everyone's app holds to it, and drops posts that come sooner. People who can remove messages aren't held to it."), space().width(0)),
            tabs(picks, on),
        ]
        .spacing(8));
    } else if c.slow_mode_secs > 0 {
        col = col.push(faint(format!("Slow mode is on: one post every {} seconds.", c.slow_mode_secs)).size(12.5));
    }
    if c.can_set_slow_mode && !c.group {
        let listed = c.listed;
        let mut listing = column![setting(
            "Open to people asking",
            Some("Anyone with its join link can ask to join, and is invited without anyone having to say yes. A directory can list the link."),
            switch("Open to people asking", listed, move |on| change(SpaceChange::Listed(on))),
        )]
        .spacing(8);
        if let Some(link) = c.join_link.as_ref().filter(|_| listed) {
            listing = listing.push(
                row![faint(clip(link, 48)).size(12).font(theme::mono()).width(Length::Fill), btn_sm("Copy link", Btn::Ghost, Some(Msg::Copy(link.clone(), "Join link")))]
                    .spacing(8)
                    .align_y(Alignment::Center),
            );
            listing = listing.push(
                row![
                    input("Directory address", "stoop-listing-directory", "directory.example.org:7000", &ui.listing_directory).on_input(Msg::ListingDirectory).padding([8, 10]).size(13).width(Length::FillPortion(2)),
                    input("What it says about itself", "stoop-listing-description", "What it's about", &ui.listing_description).on_input(Msg::ListingDescription).padding([8, 10]).size(13).width(Length::FillPortion(3)),
                ]
                .spacing(8),
            );
            let dir = (!ui.listing_directory.is_empty()).then(|| ui.listing_directory.clone());
            listing = listing.push(
                row![
                    faint("A directory that takes submissions lists it straight away; only you can change or withdraw it.").size(12).width(Length::Fill),
                    btn_sm("Withdraw", Btn::Ghost, dir.clone().map(|d| change(SpaceChange::WithdrawListing(d)))),
                    btn_sm("List it", Btn::Primary, dir.map(|_| Msg::ListingSubmit(c.id.clone()))),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
        col = col.push(listing);
    }
    if c.moderator {
        let mut reports = column![label("Reports")].spacing(8);
        if c.reports.is_empty() {
            reports = reports.push(faint("None waiting.").size(12.5));
        }
        for r in &c.reports {
            let head = row![
                column![
                    t(format!("{} reported #{}", r.from, r.channel)).size(13.5).font(theme::semibold()),
                    faint(if r.reason.is_empty() { r.when.clone() } else { format!("\u{201c}{}\u{201d} · {}", r.reason, r.when) }).size(12),
                ]
                .spacing(2)
                .width(Length::Fill),
                btn_sm("Dismiss", Btn::Ghost, Some(change(SpaceChange::DismissReport(r.id)))),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            let mut body = column![head].spacing(8);
            for e in &r.excerpts {
                let check = match e.verified {
                    Some(true) => badge("Matches your copy", theme::Badge::Ok),
                    Some(false) => badge("Differs from your copy", theme::Badge::Warn),
                    None => badge("Not in your copy", theme::Badge::Plain),
                };
                let mut acts = row![check, fill()].spacing(6).align_y(Alignment::Center);
                if e.can_remove {
                    acts = acts.push(btn_sm("Remove message", Btn::Ghost, Some(Msg::Do(Action::DeletePost { community: c.id.clone(), id: e.id, author: e.author_id.clone() }))));
                }
                if e.can_kick {
                    acts = acts.push(btn_sm("Remove member", Btn::Ghost, Some(change(SpaceChange::RemoveMember { member: e.author_id.clone(), ban: false }))));
                }
                if e.can_ban {
                    acts = acts.push(btn_sm("Ban", Btn::Danger, Some(change(SpaceChange::RemoveMember { member: e.author_id.clone(), ban: true }))));
                }
                body = body.push(
                    container(column![row![t(e.author.as_str()).size(12.5).font(theme::semibold())], t(e.text.as_str()).size(13).wrapping(Wrapping::WordOrGlyph), acts].spacing(4))
                        .padding([8, 10])
                        .width(Length::Fill)
                        .style(theme::inset),
                );
            }
            reports = reports.push(card(body).padding([10, 12]));
        }
        col = col.push(reports);
    }
    if !c.pending_invites.is_empty() {
        let mut invites = column![label("Invitations you sent"), faint("Withdrawing one means their answer is refused. Unanswered ones run out after two weeks.").size(12)].spacing(6);
        for p in &c.pending_invites {
            invites = invites.push(
                row![avatar(&p.name, &p.id, 26.0), t(p.name.as_str()).size(13).font(theme::semibold()).width(Length::Fill), faint(p.when.as_str()).size(12), btn_sm("Withdraw", Btn::Ghost, Some(change(SpaceChange::WithdrawInvite(p.id.clone()))))]
                    .spacing(8)
                    .align_y(Alignment::Center),
            );
        }
        col = col.push(invites);
    }
    scrollable(col).height(Length::Shrink).style(theme::scroll).into()
}

/// Communities the directories you added list: a search over them, the
/// way to add a directory, and Ask to join.
pub fn directory<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let d = &m.directory;
    let mut col = column![muted("A directory lists only communities that asked to be listed, never people. Add one someone told you about.").size(13)].spacing(12);
    let mut dirs = column![].spacing(4);
    for addr in &d.directories {
        dirs = dirs.push(
            row![icon(Icon::Globe, 14.0, theme::look().faint), t(addr.as_str()).size(13).width(Length::Fill), icon_btn(Icon::Trash, "Stop browsing it", Some(Msg::Do(Action::RemoveDirectory(addr.clone()))))]
                .spacing(8)
                .align_y(Alignment::Center),
        );
    }
    col = col.push(dirs);
    col = col.push(
        row![
            input("Directory to add", "stoop-directory-add", "directory.example.org:7000", &ui.directory_address).on_input(Msg::DirectoryAddress).on_submit(Msg::DirectoryAdd).padding([8, 10]).size(13),
            btn_sm("Add", Btn::Ghost, (!ui.directory_address.is_empty()).then_some(Msg::DirectoryAdd)),
            btn_sm(if d.browsing { "Looking…" } else { "Refresh" }, Btn::Ghost, (!d.browsing && !d.directories.is_empty()).then_some(Msg::Do(Action::BrowseDirectories))),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    );
    for problem in &d.problems {
        col = col.push(faint(problem.as_str()).size(12));
    }
    if !d.listed.is_empty() {
        col = col.push(input("Search the listings", "stoop-directory-search", "Search by name or what it's about", &ui.directory_query).on_input(Msg::DirectoryQuery).padding([8, 10]).size(13.5));
    }
    let words: Vec<String> = ui.directory_query.split_whitespace().map(str::to_lowercase).collect();
    let mut list = column![].spacing(6);
    let mut shown = 0;
    for l in &d.listed {
        let text = format!("{} {}", l.name, l.description).to_lowercase();
        if !words.iter().all(|w| text.contains(w.as_str())) {
            continue;
        }
        shown += 1;
        let join = if l.link.starts_with("stoop://join/") {
            btn_sm("Ask to join", Btn::Primary, Some(Msg::Do(Action::AskToJoin(l.link.clone()))))
        } else {
            btn_sm("Add host", Btn::Ghost, Some(Msg::Do(Action::AddPerson { name: l.name.clone(), invite: l.link.clone() })))
        };
        list = list.push(
            row![
                avatar(&l.name, &l.link, 30.0),
                column![t(l.name.as_str()).size(13.5).font(theme::semibold()), faint(format!("{} · {}", if l.description.is_empty() { "No description" } else { l.description.as_str() }, l.directory)).size(12)]
                    .spacing(1)
                    .width(Length::Fill),
                join,
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
    }
    if !d.listed.is_empty() && shown == 0 {
        list = list.push(faint("Nothing listed matches that.").size(12.5));
    }
    col = col.push(scrollable(list).height(Length::Fixed(300.0)).style(theme::scroll));
    col.into()
}
