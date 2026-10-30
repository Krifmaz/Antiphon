//! The window: picks the wide or narrow layout and lays dialogs, toasts and
//! the notice over it.

mod account;
mod call;
mod community;
mod conversation;
mod dialogs;
mod first_run;
mod home;
mod menu;
mod panel;
mod phone;
mod profile;
mod reactions;
mod settings;
mod sidebar;
mod update;

use iced_core::{Alignment, Background, Border, Color, Length};
use iced_widget::{button, column, container, row, space, stack};

use crate::icons::{icon, Icon};
use crate::theme;
use crate::widgets::*;
use crate::{El, Model, Msg, Presence, Route, ToastKind, Ui};

/// The whole window.
pub fn view<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    // Everything but the phone layout's own bars keeps clear of the
    // system's strips; the phone layout runs its bar colours under them.
    let (top, bottom) = ui.insets;
    let inset = iced_core::Padding { top, bottom, ..iced_core::Padding::ZERO };
    if m.first_run {
        return container(first_run::view(ui, m)).padding(inset).width(Length::Fill).height(Length::Fill).style(theme::app_bg).into();
    }
    let base = if ui.narrow() { phone::view(ui, m) } else { container(wide(ui, m)).padding(inset).into() };
    let mut layers = match &m.settings.backdrop {
        // The ground, the picture over it, and the interface over both
        // with nothing of its own behind it, so the picture shows.
        Some(b) => stack![
            container(space()).width(Length::Fill).height(Length::Fill).style(theme::app_bg),
            backdrop(ui, b),
            container(base).width(Length::Fill).height(Length::Fill),
        ],
        None => stack![container(base).width(Length::Fill).height(Length::Fill).style(theme::app_bg)],
    };
    if let Some(d) = ui.dialog_shown.as_ref() {
        let rise = ui.dialog_rise();
        if rise > 0.001 {
            // A dialog takes the keyboard while it is up.
            crate::a11y::layer();
            layers = layers.push(container(dialogs::view(ui, m, d, rise)).padding(inset));
        }
    }
    if ui.menu.is_some() {
        // So does a right-click menu.
        crate::a11y::layer();
    }
    if let Some(layer) = menu::layer(ui, m) {
        layers = layers.push(layer);
    }
    if !ui.toasts.is_empty() {
        layers = layers.push(container(toasts(ui)).padding(inset));
    }
    layers.into()
}

/// The background picture at its current frame, cropped, turned, fitted
/// and faded as Settings says.
fn backdrop<'a>(ui: &Ui, b: &crate::Backdrop) -> El<'a, Msg> {
    let fit = match b.fit {
        crate::ImageFit::Cover => iced_core::ContentFit::Cover,
        crate::ImageFit::Contain => iced_core::ContentFit::Contain,
        crate::ImageFit::Stretch => iced_core::ContentFit::Fill,
        crate::ImageFit::Original => iced_core::ContentFit::None,
    };
    iced_widget::image(b.frame(ui.now, ui.reduced_motion).clone())
        .crop(b.crop_region())
        .content_fit(fit)
        // Floating: turning it doesn't change the space it is laid out in.
        .rotation(iced_core::Rotation::Floating(iced_core::Degrees(b.rotation).into()))
        .opacity(b.opacity.clamp(0.0, 1.0))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The wide layout: people list, the screen, and the details panel.
fn wide<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    // Built first so the keyboard and screen readers meet the people list
    // before the screen beside it, as the eye does.
    let side = sidebar::view(ui, m);
    let main: El<'a, Msg> = match &ui.route {
        Route::Home => home::view(ui, m),
        Route::Person(id) => conversation::view(ui, m, id),
        Route::Settings(section) => settings::view(ui, m, *section),
        // The list is the sidebar here, so it opens on the first section.
        Route::SettingsMenu => settings::view(ui, m, crate::Section::Profile),
        Route::Spaces => community::list(ui, m, false),
        Route::Community(id, channel) => community::view(ui, m, id, *channel),
        // Profiles are pages of their own here too, as on the phone, so
        // both apps have the same editor and the same placeable QR code.
        Route::Profile(None) => profile::mine(m, 0.0, false),
        Route::ProfileEdit => profile::edit(ui, m, false),
        Route::Profile(Some(id)) => match m.people.iter().find(|p| &p.id == id) {
            Some(person) => profile::theirs(m, person, 0.0, false),
            None => conversation::view(ui, m, id),
        },
    };
    // The notice first, then a newer version, then the screen.
    let main = match (m.notice.as_deref(), update::banner(m, false)) {
        (Some(n), Some(b)) => column![notice(n), b, main].into(),
        (Some(n), None) => column![notice(n), main].into(),
        (None, Some(b)) => column![b, main].into(),
        (None, None) => main,
    };
    let mut r = row![side, container(main).width(Length::Fill).height(Length::Fill).style(theme::stage)];
    if let Route::Person(id) = &ui.route {
        let open = ui.panel_open_amount();
        if ui.size.width >= theme::PANEL_FITS && open > 0.01 {
            if let Some(person) = m.people.iter().find(|p| &p.id == id) {
                let w = theme::PANEL * open;
                r = r.push(container(panel::view(ui, m, person)).width(w).height(Length::Fill).clip(true).style(theme::stage_flat));
            }
        }
    }
    r.into()
}

/// The line across the top for something that needs attention now.
fn notice<'a>(text: &'a str) -> El<'a, Msg> {
    let _ = crate::a11y::Spec::new(crate::a11y::Role::Alert, text).add();
    let p = theme::look();
    container(row![icon(Icon::Info, 16.0, p.warn), t(text).size(13)].spacing(10).align_y(Alignment::Center))
        .padding([10, 18])
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(p.warn.scale_alpha(0.10))),
            border: Border { color: p.warn.scale_alpha(0.3), width: 0.0, radius: iced_core::border::Radius::new(0.0).top_left(16.0) },
            ..container::Style::default()
        })
        .into()
}

/// The toasts: bottom right on a wide window, along the top on a phone.
fn toasts<'a>(ui: &'a Ui) -> El<'a, Msg> {
    let p = theme::look();
    let mut col = column![].spacing(8).align_x(if ui.narrow() { Alignment::Center } else { Alignment::End });
    for toast in &ui.toasts {
        // Read out as it appears; errors at once.
        let _ = crate::a11y::Spec::new(crate::a11y::Role::Alert, toast.text.as_str()).urgent(toast.kind == ToastKind::Err).add();
        let (edge, i) = match toast.kind {
            ToastKind::Info => (p.info, Icon::Info),
            ToastKind::Ok => (p.ok, Icon::Check),
            ToastKind::Warn => (p.warn, Icon::Info),
            ToastKind::Err => (p.err, Icon::X),
        };
        let body = row![
            container(icon(i, 14.0, theme::hex("#16161f"))).center(22).style(theme::avatar(edge)),
            t(toast.text.as_str()).size(13.5),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        col = col.push(
            button(container(body).padding([10, 14]).max_width(420).style(theme::toast))
                .padding(0)
                .style(|_, _| button::Style::default())
                .on_press(Msg::DismissToast(toast.id)),
        );
    }
    let c = container(col).width(Length::Fill).height(Length::Fill);
    if ui.narrow() {
        c.align_x(Alignment::Center).align_y(Alignment::Start).padding([16, 16]).into()
    } else {
        c.align_x(Alignment::End).align_y(Alignment::End).padding([22, 22]).into()
    }
}

/// The colour of a presence dot, or none when they said nothing.
fn presence_color(p: Presence) -> Option<Color> {
    let look = theme::look();
    match p {
        Presence::Online => Some(look.ok),
        Presence::Away => Some(look.warn),
        Presence::Busy => Some(look.err),
        Presence::Unknown => None,
    }
}

/// Shortens `s` to `max` characters with an ellipsis, for one-line previews.
fn clip(s: &str, max: usize) -> String {
    let s = s.lines().next().unwrap_or_default();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// `mm:ss`, or `h:mm:ss` past an hour.
fn clock(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// An empty, flexible gap.
fn fill<'a>() -> El<'a, Msg> {
    space().width(Length::Fill).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_are_one_short_line() {
        assert_eq!(clip("hello", 10), "hello");
        assert_eq!(clip("hello there friend", 8), "hello t…");
        assert_eq!(clip("first\nsecond", 20), "first");
    }

    #[test]
    fn call_clock() {
        assert_eq!(clock(5), "00:05");
        assert_eq!(clock(125), "02:05");
        assert_eq!(clock(3725), "1:02:05");
    }
}
