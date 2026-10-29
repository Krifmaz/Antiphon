//! The window: picks the wide or narrow layout and lays dialogs, toasts and
//! the notice over it.

mod call;
mod conversation;
mod dialogs;
mod first_run;
mod home;
mod menu;
mod panel;
mod phone;
mod settings;
mod sidebar;

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
    let mut layers = stack![container(base).width(Length::Fill).height(Length::Fill).style(theme::app_bg)];
    if let Some(d) = ui.dialog_shown.as_ref() {
        let rise = ui.dialog_rise();
        if rise > 0.001 {
            layers = layers.push(container(dialogs::view(ui, m, d, rise)).padding(inset));
        }
    }
    if let Some(layer) = menu::layer(ui, m) {
        layers = layers.push(layer);
    }
    if !ui.toasts.is_empty() {
        layers = layers.push(container(toasts(ui)).padding(inset));
    }
    layers.into()
}

/// The wide layout: people list, the screen, and the details panel.
fn wide<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let main: El<'a, Msg> = match &ui.route {
        Route::Home => home::view(ui, m),
        Route::Person(id) => conversation::view(ui, m, id),
        Route::Settings(section) => settings::view(ui, m, *section),
    };
    let main = match m.notice.as_deref() {
        Some(n) => column![notice(n), main].into(),
        None => main,
    };
    let mut r = row![sidebar::view(ui, m), container(main).width(Length::Fill).height(Length::Fill).style(theme::stage)];
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
