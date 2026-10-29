//! The toolbox every screen builds with: text styles, buttons and boxes
//! already wired to the theme, so screens never set colours themselves.
//! Encore's `widgets/mod.rs`, with Stoop's own pieces (avatars with
//! presence, the speaking ring, setting rows).

use iced_core::text::Shaping;
use iced_core::{Alignment, Background, Border, Color, Length, Padding};
use iced_widget::text::IntoFragment;
use iced_widget::{button, column, container, mouse_area, opaque, row, space, stack, text, tooltip, Text};

use crate::a11y::{self, Role, Spec};
use crate::icons::{icon, Icon};
use crate::theme::{self, Badge, Btn};
use crate::El;

/// Text with advanced shaping, so names in any script render right.
pub fn t<'a>(s: impl IntoFragment<'a>) -> Text<'a, iced_core::Theme, iced_widget::Renderer> {
    text(s).shaping(Shaping::Advanced).font(theme::font()).size(14)
}

/// A screen title: 24 px bold.
pub fn h1<'a>(s: impl IntoFragment<'a>) -> Text<'a, iced_core::Theme, iced_widget::Renderer> {
    let s = s.into_fragment();
    let _ = Spec::new(Role::Heading, s.as_ref()).level(1).add();
    t(s).size(24).font(theme::bold())
}

/// A section title: 17 px bold.
pub fn h2<'a>(s: impl IntoFragment<'a>) -> Text<'a, iced_core::Theme, iced_widget::Renderer> {
    let s = s.into_fragment();
    let _ = Spec::new(Role::Heading, s.as_ref()).level(2).add();
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

/// Registers a button that reads as `label` and presses `on` (see
/// [`a11y`]); a disabled one (`on` None) is skipped by Tab and readers.
/// True when it has keyboard focus.
pub fn reg_button<M: Clone + 'static>(role: Role, label: &str, on: Option<&M>) -> bool {
    on.is_some() && Spec::new(role, label).on(on).add()
}

/// `style` with the keyboard focus ring drawn as the button's border when
/// `focused`, so a focused button shows it without changing size.
pub fn focus_style(
    style: impl Fn(&iced_core::Theme, button::Status) -> button::Style,
    focused: bool,
) -> impl Fn(&iced_core::Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut s = style(theme, status);
        if focused {
            s.border.color = theme::look().accent2;
            s.border.width = 2.0;
        }
        s
    }
}

/// A button with a text label. `on` None draws it disabled.
pub fn btn<'a, M: Clone + 'static>(label: impl IntoFragment<'a>, kind: Btn, on: Option<M>) -> button::Button<'a, M, iced_core::Theme, iced_widget::Renderer> {
    let label = label.into_fragment();
    let focused = reg_button(Role::Button, &label, on.as_ref());
    button(a11y::mark(t(label).size(14).font(theme::semibold()), focused))
        .padding([9, 16])
        .style(focus_style(theme::btn(kind), focused))
        .on_press_maybe(on)
}

/// A smaller button.
pub fn btn_sm<'a, M: Clone + 'static>(label: impl IntoFragment<'a>, kind: Btn, on: Option<M>) -> button::Button<'a, M, iced_core::Theme, iced_widget::Renderer> {
    let label = label.into_fragment();
    let focused = reg_button(Role::Button, &label, on.as_ref());
    button(a11y::mark(t(label).size(13).font(theme::semibold()), focused))
        .padding([6, 12])
        .style(focus_style(theme::btn(kind), focused))
        .on_press_maybe(on)
}

/// A button with an icon before its label.
pub fn btn_icon<'a, M: Clone + 'static>(
    i: Icon,
    label: impl IntoFragment<'a>,
    kind: Btn,
    on: Option<M>,
) -> button::Button<'a, M, iced_core::Theme, iced_widget::Renderer> {
    let label = label.into_fragment();
    let focused = reg_button(Role::Button, &label, on.as_ref());
    let color = theme::btn_fg(kind, false);
    let color = if on.is_some() { color } else { color.scale_alpha(0.45) };
    button(a11y::mark(row![icon(i, 16.0, color), t(label).size(14).font(theme::semibold())].spacing(8).align_y(Alignment::Center), focused))
        .padding([9, 14])
        .style(focus_style(theme::btn(kind), focused))
        .on_press_maybe(on)
}

/// A square icon button with a tooltip. `on` None draws it disabled. The
/// tooltip is also its name for the keyboard and screen readers.
pub fn icon_btn<'a, M: Clone + 'static>(i: Icon, tip: &'a str, on: Option<M>) -> El<'a, M> {
    icon_btn_kind(i, tip, Btn::Icon, on)
}

/// [`icon_btn`] in a chosen kind (`IconOn` for a switched-on control).
pub fn icon_btn_kind<'a, M: Clone + 'static>(i: Icon, tip: &'a str, kind: Btn, on: Option<M>) -> El<'a, M> {
    icon_btn_named(i, tip, tip, kind, on)
}

/// [`icon_btn_kind`] with a name for the keyboard and screen readers that
/// differs from the tooltip (an empty tooltip shows none).
pub fn icon_btn_named<'a, M: Clone + 'static>(i: Icon, name: &str, tip: &'a str, kind: Btn, on: Option<M>) -> El<'a, M> {
    let p = theme::look();
    let color = match kind {
        Btn::IconOn => p.err,
        _ if on.is_some() => p.muted,
        _ => p.faint.scale_alpha(0.6),
    };
    let focused = reg_button(Role::Button, name, on.as_ref());
    let b = button(a11y::mark(icon(i, 18.0, color), focused)).padding(7).style(focus_style(theme::btn(kind), focused)).on_press_maybe(on);
    with_tip(b, tip, tooltip::Position::Top)
}

/// A round call control: a big icon in a circle.
pub fn round_btn<'a, M: Clone + 'static>(i: Icon, tip: &'a str, kind: Btn, on: Option<M>, size: f32) -> El<'a, M> {
    let p = theme::look();
    let focused = reg_button(Role::Button, tip, on.as_ref());
    let color = match kind {
        Btn::RoundOn => p.bg,
        Btn::DangerSolid | Btn::Ok => theme::hex("#16161f"),
        _ => p.text,
    };
    let b = button(a11y::mark(container(icon(i, size * 0.42, color)).center(Length::Fill), focused))
        .width(size)
        .height(size)
        .padding(0)
        .style(focus_style(theme::btn(kind), focused))
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
    // Their picture when they have one, in a circle; otherwise their
    // initial on their chosen colour (or one picked from their key).
    if let Some(picture) = crate::avatars::picture(seed) {
        return iced_widget::image(picture).width(size).height(size).border_radius(size / 2.0).into();
    }
    let color = crate::avatars::color(seed).unwrap_or_else(|| theme::avatar_color(seed));
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

/// A switch named `name` for the keyboard and screen readers (the words
/// beside it are drawn by the caller, usually [`setting`]).
pub fn switch<'a, M: Clone + 'static>(name: &str, on: bool, msg: impl Fn(bool) -> M + 'a) -> El<'a, M> {
    let flip = msg(!on);
    let focused = Spec::new(Role::Toggle, name).checked(on).on(Some(&flip)).add();
    a11y::ring(iced_widget::toggler(on).on_toggle(msg).size(20).style(theme::switch), focused, 99.0)
}

/// A text box named `name` for the keyboard and screen readers, with the
/// widget id `id` so keyboard focus can put the typing cursor in it.
pub fn input<'a, M: Clone + 'static>(name: &str, id: &'static str, placeholder: &str, value: &str) -> iced_widget::TextInput<'a, M, iced_core::Theme, iced_widget::Renderer> {
    let _ = Spec::new(Role::Input, name).value(value).input(id).add();
    iced_widget::text_input(placeholder, value).id(id).font(theme::font()).style(theme::input)
}

/// A password box named `name`: like [`input`], but what is typed is
/// hidden and never handed to a screen reader as the box's value.
pub fn secret_input<'a, M: Clone + 'static>(name: &str, id: &'static str, placeholder: &str, value: &str) -> iced_widget::TextInput<'a, M, iced_core::Theme, iced_widget::Renderer> {
    let _ = Spec::new(Role::Input, name).input(id).add();
    iced_widget::text_input(placeholder, value).id(id).secure(true).font(theme::font()).style(theme::input)
}

/// A drop-down named `name`: pick one of `options`. The arrow keys step
/// to the next or previous option without opening it. `shape` sets its
/// size and padding.
pub fn choice<'a, M: Clone + 'static>(
    name: &str,
    options: &'a [String],
    selected: Option<String>,
    placeholder: &'a str,
    on: impl Fn(String) -> M + 'a,
    shape: impl FnOnce(PickList<'a, M>) -> PickList<'a, M>,
) -> El<'a, M> {
    let at = selected.as_ref().and_then(|s| options.iter().position(|o| o == s));
    let pick = |i: Option<usize>| i.and_then(|i| options.get(i)).map(|o| on(o.clone()));
    let next = pick(Some(at.map_or(0, |i| i + 1)));
    let prev = pick(at.and_then(|i| i.checked_sub(1)));
    let value = selected.clone().unwrap_or_else(|| placeholder.to_string());
    let focused = Spec::new(Role::Choice, name).value(value).steps(next.as_ref(), prev.as_ref()).add();
    let style = move |th: &iced_core::Theme, st| {
        let mut s = theme::pick(th, st);
        if focused {
            s.border.color = theme::look().accent2;
            s.border.width = 2.0;
        }
        s
    };
    let pick = iced_widget::pick_list(options, selected, on).placeholder(placeholder).font(theme::font()).style(style).menu_style(theme::pick_menu);
    a11y::mark(shape(pick), focused)
}

/// The drop-down [`choice`] builds, for its `shape` argument.
pub type PickList<'a, M> = iced_widget::PickList<'a, String, &'a [String], String, M, iced_core::Theme, iced_widget::Renderer>;

/// A button around any content, named `name` for the keyboard and screen
/// readers, in the style of `kind`. For rows, tiles and navigation.
pub fn pressable<'a, M: Clone + 'static>(content: impl Into<El<'a, M>>, role: Role, name: &str, selected: bool, kind: Btn, on: M) -> button::Button<'a, M, iced_core::Theme, iced_widget::Renderer> {
    let focused = Spec::new(role, name).selected(selected).on(Some(&on)).add();
    button(a11y::mark(content, focused)).style(focus_style(theme::btn(kind), focused)).on_press(on)
}

/// A row of tabs; `on` is the index of the selected one.
pub fn tabs<'a, M: Clone + 'static>(items: Vec<(&'a str, M)>, on: usize) -> El<'a, M> {
    let mut r = row![].spacing(4);
    for (i, (label, msg)) in items.into_iter().enumerate() {
        let kind = if i == on { Btn::TabOn } else { Btn::Tab };
        let focused = Spec::new(Role::Tab, label).selected(i == on).on(Some(&msg)).add();
        r = r.push(button(a11y::mark(t(label).size(13).font(theme::semibold()), focused)).padding([6, 14]).style(focus_style(theme::btn(kind), focused)).on_press(msg));
    }
    container(r).padding(4).style(theme::tabs_frame).into()
}

/// A dialog over a dimmed backdrop. `on_close` comes from the X and from
/// a click on the backdrop. `rise` (0..1) slides the card up into place
/// and fades the backdrop in as it opens.
pub fn modal<'a, M: Clone + 'static>(
    title: &'a str,
    body: impl Into<El<'a, M>>,
    footer: Option<El<'a, M>>,
    width: f32,
    on_close: M,
    rise: f32,
) -> El<'a, M> {
    let head = row![h2(title).width(Length::Fill), icon_btn_named(Icon::X, "Close", "", Btn::Icon, Some(on_close.clone()))].align_y(Alignment::Center).spacing(10);
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
pub fn swatch<'a, M: Clone + 'static>(a: Color, b: Color, selected: bool, tip: &'a str, on: M) -> El<'a, M> {
    let p = theme::look();
    let focused = Spec::new(Role::Tab, tip).selected(selected).on(Some(&on)).add();
    let fill = container(space()).width(28).height(28).style(move |_| container::Style {
        background: Some(Background::Gradient(iced_core::Gradient::Linear(
            iced_core::gradient::Linear::new(iced_core::Radians(2.356)).add_stop(0.0, a).add_stop(1.0, b),
        ))),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 99.0.into() },
        ..container::Style::default()
    });
    let ring = if selected { p.text } else { Color::TRANSPARENT };
    let ring = if focused { p.accent2 } else { ring };
    let b = button(a11y::mark(container(fill).padding(3).style(theme::ring(ring, 2.0)), focused)).padding(0).style(theme::btn(Btn::Ghost)).on_press(on);
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
/// Named `name` for the keyboard and screen readers; the arrow keys step it
/// by 5%.
pub fn volume_slider<'a, M: Clone + 'static>(name: &str, db: f32, width: iced_core::Length, on: impl Fn(f32) -> M + 'a) -> El<'a, M> {
    let pct = db_to_percent(db).round();
    let up = on(percent_to_db((pct + 5.0).min(200.0)));
    let down = on(percent_to_db((pct - 5.0).max(0.0)));
    let focused = Spec::new(Role::Slider, name).value(format!("{pct:.0}%")).steps(Some(&up), Some(&down)).add();
    iced_widget::row![
        a11y::ring(iced_widget::slider(0.0..=200.0, pct, move |v| on(percent_to_db(v))).step(5.0_f32).width(width).style(theme::slider_style), focused, 8.0),
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
