//! The toolbox every screen builds with: text styles, buttons and boxes
//! already wired to the theme, so screens never set colours themselves.
//! Encore's `widgets/mod.rs`, with Stoop's own pieces (avatars with
//! presence, the speaking ring, setting rows).

use iced_core::text::Shaping;
use iced_core::{Alignment, Background, Border, Color, Length, Padding};
use iced_widget::text::IntoFragment;
use iced_widget::{button, column, container, mouse_area, opaque, row, space, stack, text, tooltip, Text};

use crate::icons::{icon, Icon};
use crate::theme::{self, Badge, Btn};
use crate::El;

/// Text with advanced shaping, so names in any script render right.
pub fn t<'a>(s: impl IntoFragment<'a>) -> Text<'a, iced_core::Theme, iced_widget::Renderer> {
    text(s).shaping(Shaping::Advanced).font(theme::FONT).size(14)
}

/// A screen title: 24 px bold.
pub fn h1<'a>(s: impl IntoFragment<'a>) -> Text<'a, iced_core::Theme, iced_widget::Renderer> {
    t(s).size(24).font(theme::bold())
}

/// A section title: 17 px bold.
pub fn h2<'a>(s: impl IntoFragment<'a>) -> Text<'a, iced_core::Theme, iced_widget::Renderer> {
    t(s).size(17).font(theme::bold())
}

/// A small title: 14 px semibold.
pub fn h3<'a>(s: impl IntoFragment<'a>) -> Text<'a, iced_core::Theme, iced_widget::Renderer> {
    t(s).size(14).font(theme::semibold())
}

/// Secondary text.
pub fn muted<'a>(s: impl IntoFragment<'a>) -> Text<'a, iced_core::Theme, iced_widget::Renderer> {
    t(s).color(theme::look().muted)
}

/// Captions and hints: 12 px, faint.
pub fn faint<'a>(s: impl IntoFragment<'a>) -> Text<'a, iced_core::Theme, iced_widget::Renderer> {
    t(s).size(12).color(theme::look().faint)
}

/// An all-caps section label over a list, like "PEOPLE".
pub fn label<'a>(s: &str) -> Text<'a, iced_core::Theme, iced_widget::Renderer> {
    t(s.to_uppercase()).size(11).font(theme::bold()).color(theme::look().faint)
}

/// A button with a text label. `on` None draws it disabled.
pub fn btn<'a, M: Clone + 'a>(label: impl IntoFragment<'a>, kind: Btn, on: Option<M>) -> button::Button<'a, M, iced_core::Theme, iced_widget::Renderer> {
    button(t(label).size(14).font(theme::semibold()))
        .padding([9, 16])
        .style(theme::btn(kind))
        .on_press_maybe(on)
}

/// A smaller button.
pub fn btn_sm<'a, M: Clone + 'a>(label: impl IntoFragment<'a>, kind: Btn, on: Option<M>) -> button::Button<'a, M, iced_core::Theme, iced_widget::Renderer> {
    button(t(label).size(13).font(theme::semibold()))
        .padding([6, 12])
        .style(theme::btn(kind))
        .on_press_maybe(on)
}

/// A button with an icon before its label.
pub fn btn_icon<'a, M: Clone + 'a>(
    i: Icon,
    label: impl IntoFragment<'a>,
    kind: Btn,
    on: Option<M>,
) -> button::Button<'a, M, iced_core::Theme, iced_widget::Renderer> {
    let color = theme::btn_fg(kind, false);
    let color = if on.is_some() { color } else { color.scale_alpha(0.45) };
    button(row![icon(i, 16.0, color), t(label).size(14).font(theme::semibold())].spacing(8).align_y(Alignment::Center))
        .padding([9, 14])
        .style(theme::btn(kind))
        .on_press_maybe(on)
}

/// A square icon button with a tooltip. `on` None draws it disabled.
pub fn icon_btn<'a, M: Clone + 'a>(i: Icon, tip: &'a str, on: Option<M>) -> El<'a, M> {
    icon_btn_kind(i, tip, Btn::Icon, on)
}

/// [`icon_btn`] in a chosen kind (`IconOn` for a switched-on control).
pub fn icon_btn_kind<'a, M: Clone + 'a>(i: Icon, tip: &'a str, kind: Btn, on: Option<M>) -> El<'a, M> {
    let p = theme::look();
    let color = match kind {
        Btn::IconOn => p.err,
        _ if on.is_some() => p.muted,
        _ => p.faint.scale_alpha(0.6),
    };
    let b = button(icon(i, 18.0, color)).padding(7).style(theme::btn(kind)).on_press_maybe(on);
    with_tip(b, tip, tooltip::Position::Top)
}

/// A round call control: a big icon in a circle.
pub fn round_btn<'a, M: Clone + 'a>(i: Icon, tip: &'a str, kind: Btn, on: Option<M>, size: f32) -> El<'a, M> {
    let p = theme::look();
    let color = match kind {
        Btn::RoundOn => p.bg,
        Btn::DangerSolid | Btn::Ok => theme::hex("#16161f"),
        _ => p.text,
    };
    let b = button(container(icon(i, size * 0.42, color)).center(Length::Fill))
        .width(size)
        .height(size)
        .padding(0)
        .style(theme::btn(match kind {
            Btn::DangerSolid | Btn::Ok => kind,
            _ => kind,
        }))
        .on_press_maybe(on);
    let b: El<'a, M> = if matches!(kind, Btn::DangerSolid | Btn::Ok) {
        // A filled control is round too.
        container(b).style(|_| container::Style { border: Border { radius: 99.0.into(), ..Border::default() }, ..container::Style::default() }).into()
    } else {
        b.into()
    };
    with_tip(b, tip, tooltip::Position::Top)
}

/// Wraps `content` in a tooltip, or returns it as is when `tip` is empty.
pub fn with_tip<'a, M: 'a>(content: impl Into<El<'a, M>>, tip: impl IntoFragment<'a>, position: tooltip::Position) -> El<'a, M> {
    let tip = tip.into_fragment();
    if tip.is_empty() {
        return content.into();
    }
    tooltip(content, container(t(tip).size(12)).padding([5, 9]).style(theme::menu_pop), position).gap(6).into()
}

/// A card: hairline border, 18 px padding.
pub fn card<'a, M: 'a>(content: impl Into<El<'a, M>>) -> container::Container<'a, M, iced_core::Theme, iced_widget::Renderer> {
    container(content).padding(18).width(Length::Fill).style(theme::card_raised)
}

/// A card with a title and an optional line under it.
pub fn titled_card<'a, M: 'a>(title: &'a str, sub: Option<&'a str>, body: impl Into<El<'a, M>>) -> El<'a, M> {
    let mut head = column![h3(title)].spacing(3);
    if let Some(s) = sub {
        head = head.push(faint(s).size(12.5));
    }
    card(column![head, body.into()].spacing(14)).into()
}

/// A red box with a message; nothing when the message is empty.
pub fn error_box<'a, M: 'a>(msg: &str) -> El<'a, M> {
    if msg.is_empty() {
        return space().into();
    }
    container(t(msg.to_string()).size(13).color(theme::look().err)).padding([10, 14]).width(Length::Fill).style(theme::error_box).into()
}

/// An amber box with a message.
pub fn warn_box<'a, M: 'a>(content: impl Into<El<'a, M>>) -> El<'a, M> {
    container(content).padding([10, 14]).width(Length::Fill).style(theme::warn_box).into()
}

/// A pill-shaped badge.
pub fn badge<'a, M: 'a>(label: impl IntoFragment<'a>, kind: Badge) -> El<'a, M> {
    let (fg, _, _) = theme::badge_colors(kind);
    container(t(label).size(11.5).font(theme::semibold()).color(fg)).padding([2, 9]).style(theme::badge(kind)).into()
}

/// A badge with an icon in front.
pub fn badge_icon<'a, M: 'a>(i: Icon, label: impl IntoFragment<'a>, kind: Badge) -> El<'a, M> {
    let (fg, _, _) = theme::badge_colors(kind);
    container(row![icon(i, 12.0, fg), t(label).size(11.5).font(theme::semibold()).color(fg)].spacing(5).align_y(Alignment::Center))
        .padding([3, 9])
        .style(theme::badge(kind))
        .into()
}

/// A one-pixel horizontal line.
pub fn divider<'a, M: 'a>() -> El<'a, M> {
    iced_widget::rule::horizontal(1).style(theme::rule_style).into()
}

/// A one-pixel vertical line.
pub fn vdivider<'a, M: 'a>() -> El<'a, M> {
    iced_widget::rule::vertical(1).style(theme::rule_style).into()
}

/// A label over an input.
pub fn field<'a, M: 'a>(label: &'a str, input: impl Into<El<'a, M>>) -> El<'a, M> {
    column![t(label).size(12.5).font(theme::semibold()).color(theme::look().muted), input.into()].spacing(6).width(Length::Fill).into()
}

/// Four-sided padding.
pub fn pad(top: f32, right: f32, bottom: f32, left: f32) -> Padding {
    Padding { top, right, bottom, left }
}

/// The first letter of a name, for a letter avatar.
pub fn initial(name: &str) -> String {
    name.trim().chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into())
}

/// A round letter avatar in the person's own colour.
pub fn avatar<'a, M: 'a>(name: &str, seed: &str, size: f32) -> El<'a, M> {
    let color = theme::avatar_color(seed);
    container(t(initial(name)).size(size * 0.42).font(theme::bold()).color(theme::hex("#16161f")))
        .width(size)
        .height(size)
        .center(size)
        .style(theme::avatar(color))
        .into()
}

/// An avatar with a presence dot on its lower right, cut out of `ground`
/// (the colour behind it) like a badge sitting in the avatar's edge.
pub fn avatar_dot<'a, M: 'a>(name: &str, seed: &str, size: f32, dot: Option<Color>, ground: Color) -> El<'a, M> {
    let a = avatar(name, seed, size);
    let Some(color) = dot else { return a };
    let d = (size * 0.36).max(11.0);
    stack![
        a,
        container(container(space()).width(d).height(d).style(theme::dot(color, ground)))
            .width(size + 2.0)
            .height(size + 2.0)
            .align_x(Alignment::End)
            .align_y(Alignment::End),
    ]
    .into()
}

/// An avatar inside a ring. `strength` 0..1 widens and brightens the ring:
/// the speaking indicator. Zero leaves a quiet hairline.
pub fn ringed_avatar<'a, M: 'a>(name: &str, seed: &str, size: f32, strength: f32, color: Color) -> El<'a, M> {
    let p = theme::look();
    let strength = strength.clamp(0.0, 1.0);
    let (ring_color, width) = if strength > 0.02 { (color.scale_alpha(0.55 + 0.45 * strength), 2.0 + 3.0 * strength) } else { (p.border, 2.0) };
    let gap = 5.0;
    let outer = size + 2.0 * (gap + 5.0);
    container(container(avatar(name, seed, size)).center(size + 2.0 * gap).style(theme::ring(ring_color, width)))
        .center(outer)
        .into()
}

/// A setting: its name and a line explaining it on the left, the control
/// on the right.
pub fn setting<'a, M: 'a>(name: &'a str, about: Option<&'a str>, control: impl Into<El<'a, M>>) -> El<'a, M> {
    let mut words = column![t(name).size(14).font(theme::semibold())].spacing(3).width(Length::Fill);
    if let Some(a) = about {
        words = words.push(faint(a).size(12.5));
    }
    row![words, control.into()].spacing(16).align_y(Alignment::Center).width(Length::Fill).into()
}

/// A switch.
pub fn switch<'a, M: Clone + 'a>(on: bool, msg: impl Fn(bool) -> M + 'a) -> El<'a, M> {
    iced_widget::toggler(on).on_toggle(msg).size(20).style(theme::switch).into()
}

/// A row of tabs; `on` is the index of the selected one.
pub fn tabs<'a, M: Clone + 'a>(items: Vec<(&'a str, M)>, on: usize) -> El<'a, M> {
    let mut r = row![].spacing(4);
    for (i, (label, msg)) in items.into_iter().enumerate() {
        let kind = if i == on { Btn::TabOn } else { Btn::Tab };
        r = r.push(button(t(label).size(13).font(theme::semibold())).padding([6, 14]).style(theme::btn(kind)).on_press(msg));
    }
    container(r).padding(4).style(theme::tabs_frame).into()
}

/// A dialog over a dimmed backdrop. `on_close` comes from the X and from
/// a click on the backdrop. `rise` (0..1) slides the card up into place
/// and fades the backdrop in as it opens.
pub fn modal<'a, M: Clone + 'a>(
    title: &'a str,
    body: impl Into<El<'a, M>>,
    footer: Option<El<'a, M>>,
    width: f32,
    on_close: M,
    rise: f32,
) -> El<'a, M> {
    let head = row![h2(title).width(Length::Fill), icon_btn(Icon::X, "", Some(on_close.clone()))].align_y(Alignment::Center).spacing(10);
    let mut inner = column![container(head).padding(pad(18.0, 18.0, 6.0, 24.0)), container(body).padding(pad(10.0, 24.0, 22.0, 24.0))];
    if let Some(f) = footer {
        inner = inner.push(divider());
        inner = inner.push(container(row![space().width(Length::Fill), f].spacing(10).align_y(Alignment::Center)).padding([14, 24]));
    }
    let card = container(inner).max_width(width).width(Length::Fill).style(theme::modal);
    let rise = rise.clamp(0.0, 1.0);
    let alpha = 0.62 * rise;
    stack![
        mouse_area(
            container(space()).width(Length::Fill).height(Length::Fill).style(move |_| container::Style {
                background: Some(Background::Color(Color::from_rgba8(6, 6, 8, alpha))),
                ..container::Style::default()
            })
        )
        .on_press(on_close),
        container(opaque(card))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(pad(20.0 + 16.0 * (1.0 - rise), 20.0, 20.0, 20.0))
            .center(Length::Fill),
    ]
    .into()
}

/// A square of colour, for the accent picker.
pub fn swatch<'a, M: Clone + 'a>(a: Color, b: Color, selected: bool, tip: &'a str, on: M) -> El<'a, M> {
    let p = theme::look();
    let fill = container(space()).width(28).height(28).style(move |_| container::Style {
        background: Some(Background::Gradient(iced_core::Gradient::Linear(
            iced_core::gradient::Linear::new(iced_core::Radians(2.356)).add_stop(0.0, a).add_stop(1.0, b),
        ))),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 99.0.into() },
        ..container::Style::default()
    });
    let ring = if selected { p.text } else { Color::TRANSPARENT };
    let b = button(container(fill).padding(3).style(theme::ring(ring, 2.0))).padding(0).style(theme::btn(Btn::Ghost)).on_press(on);
    with_tip(b, tip, tooltip::Position::Top)
}

/// An empty state: an icon in a soft circle, a title and a line.
pub fn empty<'a, M: 'a>(i: Icon, title: &'a str, line: &'a str) -> El<'a, M> {
    let p = theme::look();
    column![
        container(icon(i, 26.0, p.muted)).center(64).style(move |_| container::Style {
            background: Some(Background::Color(p.panel2)),
            border: Border { color: p.border, width: 1.0, radius: 99.0.into() },
            ..container::Style::default()
        }),
        h2(title),
        muted(line).size(13.5).align_x(Alignment::Center),
    ]
    .spacing(10)
    .align_x(Alignment::Center)
    .max_width(360)
    .into()
}

/// Volume as people read it: a percentage where 100% is unchanged, 0% is
/// silent and 200% is twice as loud (+6 dB). The engine works in dB; the
/// slider works in this, so "normal" sits in the middle of the track.
pub fn db_to_percent(db: f32) -> f32 {
    if db <= -59.0 { 0.0 } else { (100.0 * 10f32.powf(db / 20.0)).clamp(0.0, 200.0) }
}

/// The inverse of [`db_to_percent`]; 0% becomes -60 dB, which is silence.
pub fn percent_to_db(percent: f32) -> f32 {
    if percent <= 0.5 { -60.0 } else { 20.0 * (percent / 100.0).log10() }
}

/// A volume slider on the percentage scale, with its value beside it.
pub fn volume_slider<'a, M: Clone + 'a>(db: f32, width: iced_core::Length, on: impl Fn(f32) -> M + 'a) -> El<'a, M> {
    let pct = db_to_percent(db).round();
    iced_widget::row![
        iced_widget::slider(0.0..=200.0, pct, move |v| on(percent_to_db(v))).step(5.0_f32).width(width).style(theme::slider_style),
        faint(format!("{pct:.0}%")).size(12).width(40),
    ]
    .spacing(10)
    .align_y(iced_core::Alignment::Center)
    .into()
}

#[cfg(test)]
mod volume_tests {
    use super::*;

    #[test]
    fn normal_volume_is_the_middle() {
        assert_eq!(db_to_percent(0.0).round(), 100.0);
        assert_eq!(percent_to_db(100.0), 0.0);
        assert_eq!(db_to_percent(-60.0), 0.0);
        assert!((percent_to_db(200.0) - 6.02).abs() < 0.01);
        assert_eq!(db_to_percent(percent_to_db(50.0)).round(), 50.0);
    }
}
