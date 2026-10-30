//! Reactions on a message: the row of them under it, the quick ones in
//! its tools, and the picker with every other emoji -- the same for a
//! conversation and a community channel, on the desktop and the phone.

use iced_core::{Alignment, Length};
use iced_widget::{column, container, row, scrollable};

use crate::a11y::Role;
use crate::icons::Icon;
use crate::theme::{self, Btn};
use crate::widgets::*;
use crate::{Community, Dialog, El, Line, Model, Msg, ReactTarget};

/// The reactions in a message's tools, one tap each; the rest are in the
/// picker the tools' last button opens.
pub const QUICK: [&str; 4] = ["👍", "❤️", "😂", "😮"];

/// An emoji drawn at `size`: the community's picture for a custom one
/// written `:name:`, or the emoji itself as text. A `:name:` the community
/// does not have (or a message outside any community) stays as typed.
fn face<'a>(emoji: &'a str, community: Option<&'a Community>, size: f32) -> El<'a, Msg> {
    let picture = emoji.strip_prefix(':').and_then(|e| e.strip_suffix(':')).and_then(|name| {
        let c = community.filter(|c| c.emoji.iter().any(|e| e == name))?;
        crate::avatars::picture(&crate::avatars::emoji_id(&c.id, name))
    });
    match picture {
        Some(handle) => iced_widget::image(handle).width(size + 3.0).height(size + 3.0).into(),
        None => t(emoji).size(size).into(),
    }
}

/// The reactions under a message, each a chip with its count; mine are
/// drawn selected and a tap on any puts mine on or takes it off. `None`
/// when nobody has reacted, so the caller adds nothing.
pub fn chips<'a>(line: &'a Line, target: &ReactTarget, community: Option<&'a Community>, size: f32) -> Option<El<'a, Msg>> {
    if line.reactions.is_empty() {
        return None;
    }
    let p = theme::look();
    let mut r = row![].spacing(4);
    for (emoji, n) in &line.reactions {
        let mine = line.reacted.contains(emoji);
        let chip = row![face(emoji, community, size), t(n.to_string()).size(size - 2.0).color(p.muted)].spacing(4).align_y(Alignment::Center);
        let kind = if mine { Btn::TabOn } else { Btn::Default };
        r = r.push(pressable(chip, Role::Button, &format!("{emoji} {n}"), mine, kind, Msg::Do(target.action(emoji.clone()))).padding([2, 8]));
    }
    Some(r.wrap().into())
}

/// The quick reactions and the button that opens the picker, for a
/// message's tools.
pub fn quick<'a>(target: &ReactTarget, size: f32) -> El<'a, Msg> {
    let mut r = row![].spacing(2).align_y(Alignment::Center);
    for e in QUICK {
        r = r.push(pressable(t(e).size(size), Role::Button, &format!("React {e}"), false, Btn::Ghost, Msg::Do(target.action(e.to_string()))).padding([3, 6]));
    }
    r.push(icon_btn(Icon::Smile, "More reactions", Some(Msg::Dialog(Some(Dialog::ReactPicker(target.clone())))))).into()
}

/// Every emoji to react with: the community's own first when the message
/// is in one, then the groups in [`crate::emoji::GROUPS`].
pub fn picker<'a>(m: &'a Model, target: &'a ReactTarget) -> El<'a, Msg> {
    let community = target.community().and_then(|id| m.communities.iter().find(|c| c.id == id));
    let pick = |emoji: String, face: El<'a, Msg>| -> El<'a, Msg> {
        let b = pressable(container(face).center(36), Role::Button, &emoji, false, Btn::Ghost, Msg::Do(target.action(emoji.clone()))).padding(1);
        with_tip(b, emoji, iced_widget::tooltip::Position::Top)
    };
    let mut col = column![].spacing(10);
    if let Some(c) = community.filter(|c| !c.emoji.is_empty()) {
        let mut grid = row![].spacing(2);
        for name in &c.emoji {
            let emoji = format!(":{name}:");
            let face = match crate::avatars::picture(&crate::avatars::emoji_id(&c.id, name)) {
                Some(h) => iced_widget::image(h).width(26).height(26).into(),
                None => t(emoji.clone()).size(11).into(),
            };
            grid = grid.push(pick(emoji, face));
        }
        col = col.push(faint(c.name.as_str()).size(12)).push(grid.wrap());
    }
    for (name, list) in crate::emoji::GROUPS {
        let mut grid = row![].spacing(2);
        for e in *list {
            grid = grid.push(pick(e.to_string(), t(*e).size(22).into()));
        }
        col = col.push(faint(*name).size(12)).push(grid.wrap());
    }
    scrollable(col.padding(pad(0.0, 12.0, 0.0, 0.0))).height(Length::Fixed(380.0)).into()
}
