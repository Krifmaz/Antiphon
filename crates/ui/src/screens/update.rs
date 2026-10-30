//! A newer version of Stoop: the line across the top while one is waiting,
//! and the card in Settings > Updates.
//!
//! Both draw [`crate::Update`], which the app fills in from its updater;
//! every button sends one of the `Action::Update*` actions back.

use iced_core::{Alignment, Background, Border, Length};
use iced_widget::{column, container, progress_bar, row};

use super::fill;
use crate::icons::{icon, Icon};
use crate::theme::{self, Btn};
use crate::widgets::*;
use crate::{Action, El, Model, Msg, UpdateState};

/// Whether [`banner`] has something to show: a newer version out, on its
/// way in, or installing, and not put off with "Later".
pub fn shown(m: &Model) -> bool {
    let u = &m.update;
    u.available && u.banner && matches!(u.state, UpdateState::Available(_) | UpdateState::Downloading(..) | UpdateState::Installing(_))
}

/// The line across the top for a newer version, or `None` when there is
/// nothing to say there (see [`shown`]).
pub fn banner<'a>(m: &'a Model, narrow: bool) -> Option<El<'a, Msg>> {
    if !shown(m) {
        return None;
    }
    let u = &m.update;
    let (text, busy, progress) = match &u.state {
        UpdateState::Available(v) => (format!("Stoop {v} is out."), false, None),
        UpdateState::Downloading(v, p) => (
            match p {
                Some(p) => format!("Downloading Stoop {v}… {}%", (p * 100.0).round() as u32),
                None => format!("Downloading Stoop {v}…"),
            },
            true,
            *p,
        ),
        UpdateState::Installing(v) => (format!("Installing Stoop {v}…"), true, None),
        _ => return None,
    };
    let _ = crate::a11y::Spec::new(crate::a11y::Role::Alert, text.as_str()).add();
    let p = theme::look();
    let mut line = row![icon(Icon::Download, 16.0, p.accent), t(text).size(13), fill()].spacing(10).align_y(Alignment::Center);
    if !busy {
        let label = if u.can_install { "Update" } else { "Get it" };
        line = line.push(btn(label, Btn::Primary, Some(Msg::Do(Action::UpdateInstall))));
        if !narrow {
            line = line.push(btn("What's new", Btn::Ghost, Some(Msg::Do(Action::UpdatePage))));
        }
    }
    // Not while installing: by then it is too late to stop.
    if !matches!(u.state, UpdateState::Installing(_)) {
        line = line.push(btn(if busy { "Stop" } else { "Later" }, Btn::Ghost, Some(Msg::Do(Action::UpdateLater))));
    }
    let mut body = column![line].spacing(8);
    if let Some(f) = progress {
        body = body.push(progress_bar(0.0..=1.0, f).girth(4).style(theme::progress));
    }
    Some(
        container(body)
            .padding([8, 18])
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(p.accent.scale_alpha(0.10))),
                border: Border { color: p.accent.scale_alpha(0.3), width: 0.0, radius: if narrow { 0.0.into() } else { iced_core::border::Radius::new(0.0).top_left(16.0) } },
                ..container::Style::default()
            })
            .into(),
    )
}

/// The card in Settings > Updates: where things stand, the button for the
/// next step, and the switch for looking automatically. Nothing when this
/// copy cannot look for updates.
pub fn card<'a>(m: &'a Model) -> Option<El<'a, Msg>> {
    let u = &m.update;
    if !u.available {
        return None;
    }
    let get = if u.can_install { "Update now" } else { "Get it" };
    let (words, action): (String, El<'a, Msg>) = match &u.state {
        UpdateState::Idle => ("Stoop can look for a newer version on its release page.".into(), check_btn(true)),
        UpdateState::Checking => ("Looking for a newer version…".into(), check_btn(false)),
        UpdateState::UpToDate => (format!("You have the newest version, {}.", m.settings.version), check_btn(true)),
        UpdateState::Available(v) => (
            format!("Stoop {v} is out."),
            row![btn("What's new", Btn::Ghost, Some(Msg::Do(Action::UpdatePage))), btn(get, Btn::Primary, Some(Msg::Do(Action::UpdateInstall)))].spacing(8).into(),
        ),
        UpdateState::Downloading(v, p) => (
            match p {
                Some(p) => format!("Downloading Stoop {v}… {}%", (p * 100.0).round() as u32),
                None => format!("Downloading Stoop {v}…"),
            },
            btn("Stop", Btn::Default, Some(Msg::Do(Action::UpdateLater))).into(),
        ),
        UpdateState::Installing(v) => (format!("Installing Stoop {v}. Stoop restarts when it's done."), column![].into()),
        UpdateState::Failed(why) => (why.clone(), check_btn(true)),
    };
    let status = row![t(words).size(14).width(Length::Fill), action].spacing(12).align_y(Alignment::Center);
    let mut body = column![status].spacing(12);
    if let UpdateState::Downloading(_, Some(f)) = u.state {
        body = body.push(progress_bar(0.0..=1.0, f).girth(5).style(theme::progress));
    }
    body = body.push(divider());
    body = body.push(setting(
        "Look for updates automatically",
        Some("When Stoop starts, and every few hours. It asks GitHub, where Stoop's releases are; nothing about you is sent."),
        switch("Look for updates automatically", u.auto, |on| Msg::Do(Action::UpdateAuto(on))),
    ));
    Some(titled_card("Updates", None, body))
}

/// "Check now", pressable or not.
fn check_btn<'a>(on: bool) -> El<'a, Msg> {
    btn_icon(Icon::Refresh, "Check now", Btn::Default, on.then_some(Msg::Do(Action::UpdateCheck))).into()
}
