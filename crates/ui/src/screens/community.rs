//! Group chats and communities: the list of them with any invitations,
//! one channel's messages, and the parts of the window around a channel
//! (its channels and members on a wide window, chips on a phone).
//!
//! A group chat and a community share these screens; a group chat is a
//! community with one channel and no roles, so it simply shows no channel
//! list. Two slots, [`voice_bar`] and [`member_badges`], are where a
//! community's voice room and members' role badges go; they draw nothing
//! yet.

use iced_core::text::Wrapping;
use iced_core::{Alignment, Length};
use iced_widget::{button, column, container, hover, row, scrollable, space, text_input};

use super::{clip, fill};
use crate::icons::{icon, Icon};
use crate::theme::{self, Btn};
use crate::widgets::*;
use crate::{Action, Channel, Community, Dialog, El, Line, Member, Model, Msg, Notify, Route, Ui};

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
        let bar = row![t("Spaces").size(18).font(theme::bold()), fill(), icon_btn(Icon::Plus, "Start one", Some(Msg::Dialog(Some(Dialog::NewSpace))))]
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
    button(container(body).clip(true))
        .width(Length::Fill)
        .padding(if phone { [10, 10] } else { [6, 10] })
        .style(theme::btn(if selected { Btn::RowOn } else { Btn::Row }))
        .on_press(Msg::Nav(Route::Community(c.id.clone(), first)))
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
        voice_bar(m, c),
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
                button(row![icon(Icon::Hash, 13.0, if on { theme::look().text } else { theme::look().muted }), t(x.name.as_str()).size(13).font(theme::semibold())].spacing(4).align_y(Alignment::Center))
                    .padding([6, 12])
                    .style(theme::btn(if on { Btn::TabOn } else { Btn::Tab }))
                    .on_press(Msg::Nav(Route::Community(c.id.clone(), x.id))),
            );
        }
        col = col.push(scrollable(container(chips).padding([8, 10])).direction(scrollable::Direction::Horizontal(scrollable::Scrollbar::new().width(0).scroller_width(0))));
    }
    col = col.push(voice_bar(m, c));
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
        if c.can_manage_channels {
            head = head.push(icon_btn(Icon::Plus, "Add a channel", Some(Msg::Dialog(Some(Dialog::AddChannel(c.id.clone()))))));
        }
        col = col.push(container(head).padding(pad(10.0, 4.0, 4.0, 8.0)));
        for x in &c.channels {
            let on = x.id == channel;
            col = col.push(
                button(
                    row![icon(Icon::Hash, 15.0, if on { p.text } else { p.faint }), t(clip(&x.name, 22)).size(13.5).font(theme::semibold()).color(if on { p.text } else { p.muted })]
                        .spacing(8)
                        .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding([6, 10])
                .style(theme::btn(if on { Btn::RowOn } else { Btn::Row }))
                .on_press(Msg::Nav(Route::Community(c.id.clone(), x.id))),
            );
        }
    }
    col = col.push(container(row![label("Members"), faint(c.members.len().to_string()).size(11)].spacing(8).align_y(Alignment::Center)).padding(pad(16.0, 4.0, 4.0, 8.0)));
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

/// Where a community's voice room goes, above its messages. Nothing yet.
pub fn voice_bar<'a>(_m: &'a Model, _c: &'a Community) -> El<'a, Msg> {
    space().height(0).into()
}

/// Where a member's role badges go, after their name. Nothing yet.
pub fn member_badges<'a>(_member: &'a Member) -> El<'a, Msg> {
    space().width(0).into()
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
        col = col.push(message(ui, c, line, first, phone));
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
fn message<'a>(ui: &'a Ui, c: &'a Community, line: &'a Line, first: bool, phone: bool) -> El<'a, Msg> {
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
        .max_width(520)
        .into()
    } else {
        let words = if line.deleted { t("Message deleted").size(13.5).color(p.faint) } else { t(line.text.as_str()).size(14).wrapping(Wrapping::WordOrGlyph) };
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
    let mut tools = row![].spacing(2).align_y(Alignment::Center);
    tools = tools.push(icon_btn(Icon::Copy, "Copy", Some(Msg::Copy(line.text.clone(), "Message"))));
    if line.mine && c.waiting.is_none() {
        tools = tools.push(icon_btn(Icon::Edit, "Edit", Some(Msg::EditStart(line.id, line.text.clone()))));
        tools = tools.push(icon_btn(
            Icon::Trash,
            "Delete",
            Some(Msg::Do(Action::DeletePost { community: c.id.clone(), id: line.id, author: line.author_id.clone() })),
        ));
    }
    let bar = container(tools).padding(3).style(theme::menu_pop);
    let overlay = container(bar).width(Length::Fill).align_x(Alignment::End).padding([0, 6]);
    hover(row_el, overlay)
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
    let mut input = text_input(&placeholder, draft).id(crate::COMPOSER).size(14).padding([11, 4]).style(theme::input_bare);
    if can_send {
        input = input.on_input(Msg::Draft).on_submit(Msg::Send);
    }
    let bar = row![
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
            text_input(if ui.space_group { "Saturday crew" } else { "The listening club" }, &ui.space_name)
                .on_input(Msg::SpaceName)
                .on_submit(Msg::SpaceCreate)
                .padding([10, 12])
                .size(14)
                .style(theme::input)
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
            button(
                row![avatar(&person.name, &person.id, 32.0), t(person.name.as_str()).size(14).font(theme::semibold()).width(Length::Fill), faint("Invite").size(12.5)]
                    .spacing(10)
                    .align_y(Alignment::Center),
            )
            .width(Length::Fill)
            .padding([6, 8])
            .style(theme::btn(Btn::Row))
            .on_press(Msg::Do(Action::InviteToSpace { community: c.id.clone(), person: person.id.clone() })),
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

/// Name a new channel.
pub fn add_channel<'a>(ui: &'a Ui, id: &'a str) -> El<'a, Msg> {
    column![
        muted("Everyone in the community can read and post in it, as with the others.").size(13.5),
        field(
            "Name",
            text_input("band-practice", &ui.channel_name).on_input(Msg::ChannelName).on_submit(Msg::ChannelCreate(id.to_string())).padding([10, 12]).size(14).style(theme::input)
        ),
    ]
    .spacing(14)
    .into()
}
