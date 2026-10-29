//! The call stage: both of you, who is talking, how long, the words to
//! compare, and the controls. It sits at the top of the conversation, so
//! the call and the chat beside it are one place.

use iced_core::{Alignment, Length};
use iced_widget::{column, container, mouse_area, row, space};

use super::{clock, fill};
use crate::icons::{icon, Icon};
use crate::theme::{self, Badge, Btn};
use crate::widgets::*;
use crate::{Action, El, Model, Msg, Phase, Transmit, Ui};

/// The stage. `compact` is the phone's: smaller avatars, controls in one row.
pub fn stage<'a>(ui: &'a Ui, m: &'a Model, compact: bool) -> El<'a, Msg> {
    let p = theme::look();
    let c = &m.call;
    let size = if compact { 64.0 } else { 84.0 };
    let peer_seed = c.peer.clone().unwrap_or_else(|| c.peer_name.clone());

    let (title, title_color) = match &c.phase {
        Phase::Dialing => (format!("Calling {}…", c.peer_name), p.muted),
        Phase::Connected => (clock(c.seconds), p.text),
        Phase::Ended(why) => (why.clone(), p.muted),
        Phase::Idle => (String::new(), p.muted),
    };

    let me_strength = if c.muted { 0.0 } else if c.sending { c.my_level.max(0.25) } else { 0.0 };
    let them_strength = match c.phase {
        Phase::Dialing => ui.pulse(),
        _ if c.deafened => 0.0,
        _ => c.their_level,
    };
    let them_color = if c.phase == Phase::Dialing { p.accent2 } else { p.ok };

    let person = |name: &str, seed: &str, strength: f32, color, caption: El<'a, Msg>| -> El<'a, Msg> {
        column![ringed_avatar(name, seed, size, strength, color), t(name.to_string()).size(13.5).font(theme::semibold()), caption]
            .spacing(4)
            .align_x(Alignment::Center)
            .width(Length::Fixed(size + 70.0))
            .into()
    };
    let me_caption: El<'a, Msg> = if c.muted {
        badge_icon(Icon::MicOff, "Muted", Badge::Err)
    } else {
        faint(c.talk_hint.as_deref().unwrap_or("You")).size(11.5).into()
    };
    let them_caption: El<'a, Msg> = if c.deafened { badge_icon(Icon::HeadphonesOff, "Not hearing", Badge::Err) } else { faint(c.mode.as_str()).size(11.5).into() };

    let mut middle = column![t(title).size(if compact { 20 } else { 26 }).font(theme::bold()).color(title_color)].spacing(8).align_x(Alignment::Center);
    if c.phase == Phase::Connected {
        let mut badges = row![badge_icon(Icon::Lock, "Encrypted", Badge::Ok)].spacing(6);
        if !c.mode.is_empty() {
            badges = badges.push(badge(c.mode.as_str(), Badge::Plain));
        }
        if let Some(q) = c.quality.as_deref() {
            badges = badges.push(badge(q, Badge::Plain));
        }
        middle = middle.push(badges);
    } else if c.phase == Phase::Dialing {
        middle = middle.push(faint("Ringing their door").size(12));
    }

    let people: El<'a, Msg> = row![
        person(&m.me.name, &m.me.id, me_strength, p.ok, me_caption),
        container(middle).width(Length::Fill).center_x(Length::Fill),
        person(&c.peer_name, &peer_seed, them_strength, them_color, them_caption),
    ]
    .align_y(Alignment::Center)
    .into();

    let mut col = column![people].spacing(16).align_x(Alignment::Center);

    if c.phase == Phase::Connected && !c.code.is_empty() {
        col = col.push(code_strip(m));
    }
    if let Some(w) = c.warning.as_deref() {
        col = col.push(warn_box(row![icon(Icon::Shield, 16.0, p.warn), t(w).size(13)].spacing(10).align_y(Alignment::Center)));
    }
    if let Some(pad) = talk_pad(m) {
        col = col.push(pad);
    }
    col = col.push(controls(m, compact));

    container(col).padding(if compact { [16, 14] } else { [22, 28] }).width(Length::Fill).style(theme::call_stage).into()
}

/// The words both sides read out to rule out someone in the middle.
fn code_strip<'a>(m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let c = &m.call;
    let verified = c.peer.as_ref().and_then(|id| m.people.iter().find(|x| &x.id == id)).map(|x| x.verified);
    let mut r = row![
        icon(Icon::ShieldCheck, 16.0, p.accent2),
        column![faint("Read these out. If they match, nobody is listening in.").size(11.5), t(c.code.as_str()).size(14).font(theme::semibold())].spacing(2).width(Length::Fill),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    match (verified, &c.peer) {
        (Some(false), Some(id)) => r = r.push(btn_sm("They match", Btn::Default, Some(Msg::Do(Action::Verify(id.clone(), true))))),
        (Some(true), _) => r = r.push(badge_icon(Icon::Check, "Verified", Badge::Ok)),
        _ => {}
    }
    container(r).padding([10, 14]).max_width(560).style(theme::inset).into()
}

/// Mute, deafen, a file, their volume, leave.
fn controls<'a>(m: &'a Model, compact: bool) -> El<'a, Msg> {
    let p = theme::look();
    let c = &m.call;
    let size = if compact { 48.0 } else { 52.0 };
    if c.phase != Phase::Connected {
        return row![round_btn(Icon::PhoneOff, "Stop calling", Btn::DangerSolid, Some(Msg::Do(Action::Hangup)), size)].into();
    }
    let mut r = row![
        round_btn(
            if c.muted { Icon::MicOff } else { Icon::Mic },
            if c.muted { "Unmute" } else { "Mute" },
            if c.muted { Btn::RoundOn } else { Btn::Round },
            Some(Msg::Do(Action::Mute(!c.muted))),
            size
        ),
        round_btn(
            if c.deafened { Icon::HeadphonesOff } else { Icon::Headphones },
            if c.deafened { "Hear again" } else { "Deafen" },
            if c.deafened { Btn::RoundOn } else { Btn::Round },
            Some(Msg::Do(Action::Deafen(!c.deafened))),
            size
        ),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    if let Some(ph) = &m.phone {
        r = r.push(round_btn(
            if ph.speaker { Icon::Volume } else { Icon::Phone },
            if ph.speaker { "Earpiece" } else { "Loudspeaker" },
            if ph.speaker { Btn::RoundOn } else { Btn::Round },
            Some(Msg::Do(Action::Speaker(!ph.speaker))),
            size,
        ));
    }
    if m.device.files {
        r = r.push(round_btn(Icon::Paperclip, "Send a file", Btn::Round, Some(Msg::Do(Action::SendFile)), size));
    }
    if !compact {
        let vol = row![icon(Icon::Volume, 16.0, p.muted), volume_slider(c.peer_volume_db, Length::Fixed(110.0), {
            let peer = c.peer.clone();
            move |db| Msg::Do(match &peer {
                Some(id) => Action::Volume(id.clone(), db),
                None => Action::PeerVolume(db),
            })
        })]
            .spacing(8)
            .align_y(Alignment::Center);
        let first = c.peer_name.split_whitespace().next().unwrap_or("Their");
        let tip = format!("How loud {first} is for you. Only you hear the change.");
        r = r.push(with_tip(container(vol).padding([8, 14]).style(theme::composer), tip, iced_widget::tooltip::Position::Top));
    }
    r = r.push(round_btn(Icon::PhoneOff, "Leave the call", Btn::DangerSolid, Some(Msg::Do(Action::Hangup)), size));
    let _ = fill;
    row![space().width(Length::Fill), r, space().width(Length::Fill)].align_y(Alignment::Center).into()
}

/// The phone's push-to-talk pad: hold it to talk. Shown on a connected
/// call when the voice is sent only while talking.
fn talk_pad<'a>(m: &'a Model) -> Option<El<'a, Msg>> {
    let ph = m.phone.as_ref()?;
    if m.call.phase != Phase::Connected || m.settings.transmit != Transmit::PushToTalk {
        return None;
    }
    let p = theme::look();
    let (label, color) = if ph.talking { ("Talking…", p.ok) } else { ("Hold to talk", p.muted) };
    let pad = container(column![icon(Icon::Mic, 28.0, color), t(label).size(15).font(theme::semibold()).color(color)].spacing(6).align_x(Alignment::Center))
        .width(Length::Fill)
        .height(110)
        .center_x(Length::Fill)
        .center_y(110)
        .style(theme::inset);
    // Let go anywhere and it stops: the runner also ends it when every
    // finger leaves the screen.
    Some(mouse_area(pad).on_press(Msg::Do(Action::Talk(true))).on_release(Msg::Do(Action::Talk(false))).into())
}
