//! The look: colour tokens, type, shape, and one style function per widget.
//!
//! Built the way Encore's desktop app is (its `theme.rs`): a [`Palette`] of
//! named tokens, one of them active at a time in a global ([`set`] /
//! [`look`]), so plain iced style functions can read it. The token names and
//! the theme and accent tables are Encore's; the default `stoop` theme is
//! Encore's charcoal-and-cream with Stoop's pastel status colours
//! (project brand notes, 2026-10-28).
//!
//! The themes themselves are not in this file: each is a markup file in
//! `crates/ui/themes` (see [`crate::markup`]), built into the binary and
//! read by [`bundled`], so a theme is edited as data rather than code.
//!
//! Pages never pick colours themselves. They call the helpers in
//! [`crate::widgets`], which call the style functions here.

use std::sync::{OnceLock, RwLock};

use iced_core::border::Radius;
use iced_core::{font, Background, Border, Color, Font, Shadow, Theme, Vector};
use iced_widget::{button, container, overlay::menu, pick_list, progress_bar, rule, scrollable, slider, text_input, toggler};

/// Corner radius of cards.
pub const RADIUS: f32 = 14.0;
/// Width of the people list on a wide window.
pub const SIDEBAR: f32 = 272.0;
/// Width of the details panel on the right.
pub const PANEL: f32 = 320.0;
/// Height of the bar over the conversation.
pub const HEADER_H: f32 = 60.0;
/// Below this window width the phone layout is used: one screen at a time.
pub const NARROW: f32 = 760.0;
/// Below this width the details panel no longer fits beside the conversation.
pub const PANEL_FITS: f32 = 1180.0;

/// The bundled typeface. Inter, in three weights, so a phone and a desktop
/// render the same letters whatever the system ships.
pub const FAMILY: &str = "Inter";

/// The bundled reading face for dyslexia: OpenDyslexic, whose weighted
/// bottoms keep letters from flipping and swapping. Licensed under the SIL
/// OFL like Inter, so it ships with the app (`assets/fonts/OpenDyslexic-OFL.txt`).
pub const DYSLEXIC_FAMILY: &str = "OpenDyslexic";

/// The font files to load before the first frame: Inter's three weights,
/// then OpenDyslexic's two (its semibold falls back to the nearest, bold).
pub const FONT_FILES: [&[u8]; 5] = [
    include_bytes!("../assets/fonts/Inter-Regular.otf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.otf"),
    include_bytes!("../assets/fonts/Inter-Bold.otf"),
    include_bytes!("../assets/fonts/OpenDyslexic-Regular.otf"),
    include_bytes!("../assets/fonts/OpenDyslexic-Bold.otf"),
];

/// Body text in the default face. Text drawn through [`crate::widgets`]
/// uses [`font`] instead, which follows the reading-font setting.
pub const FONT: Font = Font::with_name(FAMILY);

/// `(id, name)` of the reading fonts Settings offers, in its order.
pub const FONTS: [(&str, &str); 2] = [("inter", "Inter"), ("opendyslexic", "OpenDyslexic")];

/// The face for a reading-font id (see [`FONTS`]); Inter for anything unknown.
pub fn font_for(id: &str) -> Font {
    match id {
        "opendyslexic" => Font::with_name(DYSLEXIC_FAMILY),
        _ => FONT,
    }
}

/// `face` at `weight`, moved onto a weight the face actually ships.
///
/// The text engine only picks a face from the requested family when the
/// weight matches exactly; otherwise it falls back to another family. Inter
/// ships 400, 600 and 700, so it is asked for as is. OpenDyslexic ships
/// only 400 and 800 (its "Bold" file is marked extra bold), so anything
/// heavier than regular is asked for at 800, or the bold text would come
/// out in Inter.
pub fn weighted(face: Font, weight: font::Weight) -> Font {
    let weight = match face.family {
        font::Family::Name(DYSLEXIC_FAMILY) if weight != font::Weight::Normal => font::Weight::ExtraBold,
        _ => weight,
    };
    Font { weight, ..face }
}

/// The reading face every text helper reads, set by [`set_font`]. Kept
/// beside [`LOOK`] for the same reason: style and text helpers take no
/// settings argument.
static FACE: RwLock<Font> = RwLock::new(FONT);

/// Makes the reading font with this id (see [`FONTS`]) the one all text uses.
pub fn set_font(id: &str) {
    *FACE.write().unwrap_or_else(|e| e.into_inner()) = font_for(id);
}

/// The reading face in use: Inter until [`set_font`] picks another.
pub fn font() -> Font {
    *FACE.read().unwrap_or_else(|e| e.into_inner())
}

/// The colour tokens of one theme with one accent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    /// Window ground.
    pub bg: Color,
    /// The people list and other side surfaces.
    pub bg2: Color,
    /// Cards and the conversation.
    pub panel: Color,
    /// Nested boxes, hovered rows, inputs.
    pub panel2: Color,
    /// Hairlines.
    pub border: Color,
    /// Body text.
    pub text: Color,
    /// Secondary text.
    pub muted: Color,
    /// Captions and hints.
    pub faint: Color,
    /// First accent colour.
    pub accent: Color,
    /// Second accent colour: the end of the gradient, highlights, unread.
    pub accent2: Color,
    /// Text drawn on the accent, picked by luminance.
    pub on_accent: Color,
    /// Connected, speaking, encrypted.
    pub ok: Color,
    /// Needs attention.
    pub warn: Color,
    /// Leave the call, destructive actions.
    pub err: Color,
    /// Neutral information.
    pub info: Color,
    /// Letter avatars cycle through these, so people are told apart at a glance.
    pub avatars: [Color; 6],
    /// Whether this is a dark theme.
    pub dark: bool,
}

/// `(id, name)` of the themes Settings offers, in its order.
pub const THEMES: [(&str, &str); 5] = [
    ("stoop", "Stoop"),
    ("dark", "Midnight"),
    ("oled", "OLED black"),
    ("light", "Daylight"),
    ("system", "Follow the system"),
];

/// `(id, name, colour, second colour)`: Encore's accents.
pub const ACCENTS: [(&str, &str, &str, &str); 10] = [
    ("logo", "Cream", "#f0ead6", "#d6c49a"),
    ("encore", "Pastel", "#f2a7e4", "#9fd2fb"),
    ("violet", "Violet", "#8b5cf6", "#ec4899"),
    ("lilac", "Lilac", "#b48cf2", "#7cc4fa"),
    ("pink", "Pink", "#ec4899", "#f97316"),
    ("sky", "Sky", "#0ea5e9", "#8b5cf6"),
    ("teal", "Teal", "#14b8a6", "#0ea5e9"),
    ("green", "Green", "#22c55e", "#14b8a6"),
    ("amber", "Amber", "#f59e0b", "#ef4444"),
    ("red", "Red", "#ef4444", "#ec4899"),
];

/// `#rrggbb` (or `#rgb`, `#` optional) to a colour; `None` when it is not one.
pub fn parse_hex(s: &str) -> Option<Color> {
    let h = s.trim().trim_start_matches('#');
    // Expand `#rgb` to `#rrggbb`.
    let h: String = if h.len() == 3 { h.chars().flat_map(|c| [c, c]).collect() } else { h.to_string() };
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(&h, 16).ok()?;
    Some(Color::from_rgb8((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

/// `#rrggbb` (or `#rgb`) to a colour; grey when it does not parse.
pub fn hex(s: &str) -> Color {
    parse_hex(s).unwrap_or(Color::from_rgb8(128, 128, 128))
}

/// A colour back to `#rrggbb`.
pub fn to_hex(c: Color) -> String {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", b(c.r), b(c.g), b(c.b))
}

/// Relative luminance (0 dark to 1 light) per WCAG, from linearised sRGB.
fn luminance(c: Color) -> f32 {
    let f = |v: f32| if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b)
}

/// Mix `a` and `b`: `t` = 0 is all `a`.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    Color::from_rgba(a.r + (b.r - a.r) * t, a.g + (b.g - a.g) * t, a.b + (b.b - a.b) * t, a.a + (b.a - a.a) * t)
}

/// The second colour for a custom accent: the hue turned a little, as Encore does.
pub fn partner(c: Color) -> Color {
    let (r, g, b) = (c.r, c.g, c.b);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let (mut h, mut s) = (0.0, 0.0);
    if max != min {
        let d = max - min;
        s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
        h = if max == r {
            (g - b) / d + if g < b { 6.0 } else { 0.0 }
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        } / 6.0;
    }
    h = (h + 0.1) % 1.0;
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |t: f32| {
        let t = (t + 1.0) % 1.0;
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    Color::from_rgb(f(h + 1.0 / 3.0), f(h), f(h - 1.0 / 3.0))
}

/// The tokens for a theme id (see [`THEMES`]), an accent id (see [`ACCENTS`],
/// or `custom` with `custom` as `#rrggbb`). `system_dark` decides what
/// `system` means.
pub fn palette(theme: &str, accent: &str, custom: &str, system_dark: bool) -> Palette {
    let theme = if theme == "system" { if system_dark { "stoop" } else { "light" } } else { theme };
    // An unknown id falls back to Stoop, the first bundled theme.
    let base = bundled().iter().find(|b| b.id == theme).unwrap_or(&bundled()[0]);
    let dark = base.dark;
    let (a, b) = match ACCENTS.iter().find(|x| x.0 == accent) {
        Some(x) => (hex(x.2), hex(x.3)),
        None => {
            let a = parse_hex(custom).unwrap_or_else(|| hex("#f0ead6"));
            (a, partner(a))
        }
    };
    // A cream accent is unreadable on a light ground; deepen it to the gold.
    let (a, b) = if !dark && luminance(a) > 0.7 { (hex("#8a7440"), hex("#b0925a")) } else { (a, b) };
    let on_accent = if (luminance(a) + luminance(b)) / 2.0 > 0.45 { hex("#16161f") } else { Color::WHITE };
    Palette { accent: a, accent2: b, on_accent, ..base.palette }
}

/// The built-in themes' markup, one file each in `crates/ui/themes`, in
/// [`THEMES`] order (without `system`, which picks one of these).
const BUNDLED: [&str; 4] = [
    include_str!("../themes/stoop.stoop-theme"),
    include_str!("../themes/dark.stoop-theme"),
    include_str!("../themes/oled.stoop-theme"),
    include_str!("../themes/light.stoop-theme"),
];

/// One built-in theme, read from its markup.
#[derive(Clone, Debug)]
pub struct Base {
    /// The id Settings and theme files use, such as `dark`.
    pub id: String,
    /// The name Settings shows, such as `Midnight`.
    pub name: String,
    /// Whether this is a dark theme.
    pub dark: bool,
    /// Every token but the accent ones, which [`palette`] fills in.
    pub palette: Palette,
}

/// The built-in themes, parsed from [`BUNDLED`] the first time they are
/// asked for. The files ship inside the binary and a test parses each one,
/// so a failure here is a build mistake, not something a person can cause.
pub fn bundled() -> &'static [Base] {
    static CELL: OnceLock<Vec<Base>> = OnceLock::new();
    CELL.get_or_init(|| BUNDLED.iter().map(|text| read_base(text).unwrap_or_else(|e| panic!("a built-in theme is malformed: {e}"))).collect())
}

/// Reads one built-in theme's markup into a [`Base`]. Every token is
/// required, so a theme file cannot silently leave a colour grey.
pub fn read_base(text: &str) -> Result<Base, String> {
    let doc = crate::markup::parse(text)?;
    if doc.element != "stoop-theme" {
        return Err(format!("expected a stoop-theme element, found {}", doc.element));
    }
    let id = doc.text("id").ok_or("missing id")?.to_string();
    let name = doc.text("name").ok_or("missing name")?.to_string();
    let dark = doc.bool("dark").ok_or("missing dark: true or false")?;
    // Reads a required colour token, naming it when absent.
    let c = |k: &str| doc.color(k).ok_or_else(|| format!("{id}: missing colour {k}"));
    let avatars: [Color; 6] = doc
        .colors("avatars")
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| format!("{id}: avatars must list six colours"))?;
    let palette = Palette {
        bg: c("bg")?,
        bg2: c("bg2")?,
        panel: c("panel")?,
        panel2: c("panel2")?,
        border: c("border")?,
        text: c("text")?,
        muted: c("muted")?,
        faint: c("faint")?,
        // The accent tokens come from the accent setting, not the theme.
        accent: Color::BLACK,
        accent2: Color::BLACK,
        on_accent: Color::WHITE,
        ok: c("ok")?,
        warn: c("warn")?,
        err: c("err")?,
        info: c("info")?,
        avatars,
        dark,
    };
    Ok(Base { id, name, dark, palette })
}

/// The palette every style reads: set by `set`, read by `look`, so styles
/// need no palette argument (iced style functions only get the `Theme`).
static LOOK: RwLock<Option<Palette>> = RwLock::new(None);

/// Makes `p` the look every style reads.
pub fn set(p: Palette) {
    *LOOK.write().unwrap_or_else(|e| e.into_inner()) = Some(p);
}

/// The active look (Stoop with the cream accent before [`set`] was called).
pub fn look() -> Palette {
    (*LOOK.read().unwrap_or_else(|e| e.into_inner())).unwrap_or_else(|| palette("stoop", "logo", "", true))
}

/// An iced theme built from the look, so built-in widgets without our
/// styles still match.
pub fn iced_theme(p: &Palette) -> Theme {
    Theme::custom(
        "Stoop".to_string(),
        iced_core::theme::Palette { background: p.bg, text: p.text, primary: p.accent, success: p.ok, warning: p.warn, danger: p.err },
    )
}

/// The avatar colour for a name: the same person always gets the same one.
pub fn avatar_color(seed: &str) -> Color {
    let h = seed.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
    let p = look();
    p.avatars[(h % p.avatars.len() as u32) as usize]
}

/// Semibold text, for buttons and small titles.
pub fn semibold() -> Font {
    weighted(font(), font::Weight::Semibold)
}

/// Bold text, for headings.
pub fn bold() -> Font {
    weighted(font(), font::Weight::Bold)
}

/// Monospace, for fingerprints and addresses.
pub fn mono() -> Font {
    Font::MONOSPACE
}

/// No shadow at all.
fn no_shadow() -> Shadow {
    Shadow::default()
}

/// The soft drop shadow of things that float: dialogs, menus, toasts.
pub fn shadow(p: &Palette) -> Shadow {
    if p.dark && p.bg == Color::BLACK {
        return no_shadow();
    }
    Shadow {
        color: if p.dark { Color::from_rgba(0.0, 0.0, 0.0, 0.45) } else { Color::from_rgba8(40, 30, 10, 0.10) },
        offset: Vector::new(0.0, 12.0),
        blur_radius: 32.0,
    }
}

/// The accent fill, a diagonal blend of the two accent colours.
fn accent_gradient(p: &Palette) -> Background {
    Background::Gradient(iced_core::Gradient::Linear(
        iced_core::gradient::Linear::new(iced_core::Radians(2.356)).add_stop(0.0, p.accent).add_stop(1.0, p.accent2),
    ))
}

// ----- buttons -------------------------------------------------------------

/// The button kinds of the design system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Btn {
    /// Accent gradient: the one main action of a view.
    Primary,
    /// Panel background with a hairline.
    Default,
    /// No background until hovered.
    Ghost,
    /// Like `Default`, in the error colour.
    Danger,
    /// Filled in the error colour: hanging up.
    DangerSolid,
    /// Filled in the ok colour: answering.
    Ok,
    /// A square icon button.
    Icon,
    /// An icon button that is switched on (muted, deafened): tinted.
    IconOn,
    /// A row in a list (a person, a setting): invisible until hovered.
    Row,
    /// The selected row.
    RowOn,
    /// A tab or segment.
    Tab,
    /// The selected tab or segment.
    TabOn,
    /// A round call control on the call stage.
    Round,
    /// A round call control that is switched on.
    RoundOn,
}

/// `button(..).style(theme::btn(Btn::Primary))`.
pub fn btn(kind: Btn) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| button_style(kind, status)
}

/// How a button of `kind` looks in `status` (resting, hovered, pressed, disabled).
fn button_style(kind: Btn, status: button::Status) -> button::Style {
    use button::Status::{Disabled, Hovered, Pressed};
    let p = look();
    let hovered = matches!(status, Hovered | Pressed);
    let pressed = matches!(status, Pressed);
    let radius: Radius = match kind {
        Btn::Row | Btn::RowOn | Btn::Tab | Btn::TabOn => 10.0.into(),
        Btn::Round | Btn::RoundOn => 99.0.into(),
        Btn::Icon | Btn::IconOn => 9.0.into(),
        _ => 10.0.into(),
    };
    let border = |c: Color| Border { color: c, width: 1.0, radius };
    let none = Border { color: Color::TRANSPARENT, width: 0.0, radius };
    let base = button::Style { background: None, text_color: p.text, border: none, shadow: no_shadow(), snap: true };
    let mut style = match kind {
        Btn::Primary => button::Style {
            background: Some(if pressed { Background::Color(p.accent2) } else { accent_gradient(&p) }),
            text_color: p.on_accent,
            ..base
        },
        Btn::Default => button::Style {
            background: Some(Background::Color(if hovered { mix(p.panel2, p.text, 0.04) } else { p.panel2 })),
            border: border(if hovered { mix(p.border, p.text, 0.25) } else { p.border }),
            ..base
        },
        Btn::Danger => button::Style {
            background: Some(Background::Color(if hovered { p.err.scale_alpha(0.14) } else { p.panel2 })),
            text_color: p.err,
            border: border(if hovered { p.err.scale_alpha(0.6) } else { p.border }),
            ..base
        },
        Btn::DangerSolid => button::Style {
            background: Some(Background::Color(if hovered { mix(p.err, p.text, 0.12) } else { p.err })),
            text_color: hex("#16161f"),
            ..base
        },
        Btn::Ok => button::Style {
            background: Some(Background::Color(if hovered { mix(p.ok, p.text, 0.12) } else { p.ok })),
            text_color: hex("#16161f"),
            ..base
        },
        Btn::Ghost | Btn::Icon => button::Style {
            background: hovered.then_some(Background::Color(p.panel2)),
            text_color: if hovered { p.text } else { p.muted },
            ..base
        },
        Btn::IconOn => button::Style {
            background: Some(Background::Color(p.err.scale_alpha(if hovered { 0.22 } else { 0.14 }))),
            text_color: p.err,
            ..base
        },
        Btn::Row => button::Style {
            background: hovered.then_some(Background::Color(p.panel2.scale_alpha(0.7))),
            text_color: if hovered { p.text } else { p.muted },
            ..base
        },
        Btn::RowOn => button::Style { background: Some(Background::Color(p.panel2)), ..base },
        Btn::Tab => button::Style { text_color: if hovered { p.text } else { p.muted }, ..base },
        Btn::TabOn => button::Style { background: Some(Background::Color(p.panel2)), ..base },
        Btn::Round => button::Style {
            background: Some(Background::Color(if hovered { mix(p.panel2, p.text, 0.10) } else { p.panel2 })),
            border: border(p.border),
            ..base
        },
        Btn::RoundOn => button::Style {
            background: Some(Background::Color(if hovered { mix(p.text, p.panel2, 0.12) } else { p.text })),
            text_color: p.bg,
            ..base
        },
    };
    if let Disabled = status {
        style.text_color = style.text_color.scale_alpha(0.45);
        if let Some(Background::Color(c)) = style.background {
            style.background = Some(Background::Color(c.scale_alpha(0.5)));
        }
    }
    style
}

/// The colour an icon inside a button of `kind` should be drawn in, so it
/// follows the label colour (svg icons do not inherit it).
pub fn btn_fg(kind: Btn, hovered: bool) -> Color {
    button_style(kind, if hovered { button::Status::Hovered } else { button::Status::Active }).text_color
}

// ----- inputs --------------------------------------------------------------

/// A text input: inset fill, hairline, accent ring when focused.
pub fn input(_: &Theme, status: text_input::Status) -> text_input::Style {
    let p = look();
    let focused = matches!(status, text_input::Status::Focused { .. });
    let hovered = matches!(status, text_input::Status::Hovered);
    text_input::Style {
        background: Background::Color(p.panel2),
        border: Border {
            color: if focused {
                p.accent.scale_alpha(0.8)
            } else if hovered {
                mix(p.border, p.text, 0.2)
            } else {
                p.border
            },
            width: 1.0,
            radius: 10.0.into(),
        },
        icon: p.faint,
        placeholder: p.faint,
        value: p.text,
        selection: p.accent.scale_alpha(0.3),
    }
}

/// The composer's input: no box of its own, the bar around it is the box.
pub fn input_bare(_: &Theme, _status: text_input::Status) -> text_input::Style {
    let p = look();
    text_input::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 0.0.into() },
        icon: p.faint,
        placeholder: p.faint,
        value: p.text,
        selection: p.accent.scale_alpha(0.3),
    }
}

/// A switch.
pub fn switch(_: &Theme, status: toggler::Status) -> toggler::Style {
    let p = look();
    let on = matches!(status, toggler::Status::Active { is_toggled: true } | toggler::Status::Hovered { is_toggled: true });
    let hovered = matches!(status, toggler::Status::Hovered { .. });
    toggler::Style {
        background: Background::Color(if on {
            p.ok
        } else if hovered {
            mix(p.border, p.text, 0.15)
        } else {
            p.border
        }),
        background_border_width: 0.0,
        background_border_color: Color::TRANSPARENT,
        foreground: Background::Color(if on { hex("#16161f") } else { p.text }),
        foreground_border_width: 0.0,
        foreground_border_color: Color::TRANSPARENT,
        text_color: Some(p.text),
        border_radius: None,
        padding_ratio: 0.16,
    }
}

/// A drop-down.
pub fn pick(_: &Theme, status: pick_list::Status) -> pick_list::Style {
    let p = look();
    let hovered = matches!(status, pick_list::Status::Hovered | pick_list::Status::Opened { .. });
    pick_list::Style {
        text_color: p.text,
        placeholder_color: p.faint,
        handle_color: p.muted,
        background: Background::Color(p.panel2),
        border: Border { color: if hovered { mix(p.border, p.text, 0.25) } else { p.border }, width: 1.0, radius: 10.0.into() },
    }
}

/// The list a drop-down opens.
pub fn pick_menu(_: &Theme) -> menu::Style {
    let p = look();
    menu::Style {
        background: Background::Color(p.panel),
        border: Border { color: p.border, width: 1.0, radius: 12.0.into() },
        text_color: p.text,
        selected_text_color: p.text,
        selected_background: Background::Color(p.panel2),
        shadow: shadow(&p),
    }
}

/// A slider: accent fill, round handle that grows under the pointer.
pub fn slider_style(_: &Theme, status: slider::Status) -> slider::Style {
    let p = look();
    let active = matches!(status, slider::Status::Hovered | slider::Status::Dragged);
    slider::Style {
        rail: slider::Rail {
            backgrounds: (Background::Color(p.accent), Background::Color(p.border)),
            width: 4.0,
            border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 99.0.into() },
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle { radius: if active { 8.0 } else { 7.0 } },
            background: Background::Color(p.text),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
    }
}

/// A progress bar.
pub fn progress(_: &Theme) -> progress_bar::Style {
    let p = look();
    progress_bar::Style {
        background: Background::Color(p.border),
        bar: Background::Color(p.accent),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 99.0.into() },
    }
}

/// The microphone meter: green while it is loud enough to send.
pub fn meter(_: &Theme) -> progress_bar::Style {
    let p = look();
    progress_bar::Style {
        background: Background::Color(p.border),
        bar: Background::Color(p.ok),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 99.0.into() },
    }
}

/// A one-pixel hairline.
pub fn rule_style(_: &Theme) -> rule::Style {
    rule::Style { color: look().border, radius: 0.0.into(), fill_mode: rule::FillMode::Full, snap: true }
}

/// Thin scrollbars that only show up properly under the pointer.
pub fn scroll(_: &Theme, status: scrollable::Status) -> scrollable::Style {
    let p = look();
    let hovered = !matches!(status, scrollable::Status::Active { .. });
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: Background::Color(if hovered { p.faint.scale_alpha(0.8) } else { p.border.scale_alpha(0.7) }),
            border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 99.0.into() },
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll: scrollable::AutoScroll {
            background: Background::Color(p.panel2),
            border: Border::default(),
            shadow: no_shadow(),
            icon: p.text,
        },
    }
}

// ----- containers ----------------------------------------------------------

/// A rounded box with background `bg` and a 1 px `line` border.
fn boxed(bg: Color, line: Color, radius: f32) -> container::Style {
    container::Style {
        text_color: None,
        background: Some(Background::Color(bg)),
        border: Border { color: line, width: 1.0, radius: radius.into() },
        shadow: no_shadow(),
        snap: true,
    }
}

/// A square box of plain colour `bg`.
fn filled(bg: Color) -> container::Style {
    container::Style { text_color: Some(look().text), background: Some(Background::Color(bg)), ..container::Style::default() }
}

/// The window ground.
pub fn app_bg(_: &Theme) -> container::Style {
    filled(look().bg)
}

/// The people list.
pub fn sidebar(_: &Theme) -> container::Style {
    filled(look().bg2)
}

/// The conversation: the panel colour, rounded on the top-left where it
/// meets the people list, like a sheet laid over the ground.
pub fn stage(_: &Theme) -> container::Style {
    let p = look();
    container::Style {
        text_color: Some(p.text),
        background: Some(Background::Color(p.panel)),
        border: Border { color: p.border, width: 1.0, radius: Radius::new(0.0).top_left(16.0).bottom_left(16.0) },
        ..container::Style::default()
    }
}

/// The phone layout's screen: the whole width, no rounding.
pub fn stage_flat(_: &Theme) -> container::Style {
    filled(look().panel)
}

/// A card.
pub fn card(_: &Theme) -> container::Style {
    let p = look();
    boxed(p.panel, p.border, RADIUS)
}

/// A card sitting on the panel colour: one step lighter, so it still reads
/// as a card.
pub fn card_raised(_: &Theme) -> container::Style {
    let p = look();
    boxed(p.panel2, p.border, RADIUS)
}

/// A nested box inside a card.
pub fn inset(_: &Theme) -> container::Style {
    let p = look();
    boxed(p.bg2, p.border, 10.0)
}

/// The composer bar.
pub fn composer(_: &Theme) -> container::Style {
    let p = look();
    boxed(p.panel2, p.border, 14.0)
}

/// The accent-gradient banner behind a profile.
pub fn banner(_: &Theme) -> container::Style {
    let p = look();
    container::Style {
        text_color: Some(p.on_accent),
        background: Some(Background::Gradient(iced_core::Gradient::Linear(
            iced_core::gradient::Linear::new(iced_core::Radians(2.356))
                .add_stop(0.0, p.accent.scale_alpha(0.85))
                .add_stop(1.0, p.accent2.scale_alpha(0.55)),
        ))),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: Radius::new(0.0).top_left(RADIUS).top_right(RADIUS) },
        ..container::Style::default()
    }
}

/// A box in the accent gradient (the logo tile, the call-stage glow).
pub fn accent_box(_: &Theme) -> container::Style {
    let p = look();
    container::Style {
        text_color: Some(p.on_accent),
        background: Some(accent_gradient(&p)),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 12.0.into() },
        ..container::Style::default()
    }
}

/// The box a call picture sits in: near-black behind letterboxing, with
/// the panel's border and the given corner radius.
pub fn video_frame(_: &Theme, radius: f32) -> container::Style {
    let p = look();
    container::Style {
        background: Some(Background::Color(hex("#0e0e11"))),
        border: Border { color: p.border, width: 1.0, radius: radius.into() },
        ..container::Style::default()
    }
}

/// The name chip in a call picture's corner: dark glass, readable over
/// any picture in any theme.
pub fn video_label(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color { a: 0.62, ..hex("#0e0e11") })),
        border: Border { radius: 99.0.into(), ..Border::default() },
        ..container::Style::default()
    }
}

/// The call stage: a soft wash of the accent over the panel.
pub fn call_stage(_: &Theme) -> container::Style {
    let p = look();
    container::Style {
        text_color: Some(p.text),
        background: Some(Background::Gradient(iced_core::Gradient::Linear(
            iced_core::gradient::Linear::new(iced_core::Radians(std::f32::consts::PI))
                .add_stop(0.0, mix(p.panel, p.accent2, if p.dark { 0.10 } else { 0.16 }))
                .add_stop(1.0, p.panel),
        ))),
        border: Border { color: p.border, width: 1.0, radius: 0.0.into() },
        ..container::Style::default()
    }
}

/// A round avatar in a colour of its own.
pub fn avatar(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        text_color: Some(hex("#16161f")),
        background: Some(Background::Color(color)),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 99.0.into() },
        ..container::Style::default()
    }
}

/// The ring around an avatar: green while that person is speaking, the
/// ground colour otherwise so the avatar sits in a cut-out.
pub fn ring(color: Color, width: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: None,
        border: Border { color, width, radius: 99.0.into() },
        ..container::Style::default()
    }
}

/// A small dot: presence, unread.
pub fn dot(color: Color, outline: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(color)),
        border: Border { color: outline, width: 3.0, radius: 99.0.into() },
        ..container::Style::default()
    }
}

/// The unread count: gold pill, dark text.
pub fn unread(_: &Theme) -> container::Style {
    let p = look();
    container::Style {
        text_color: Some(hex("#16161f")),
        background: Some(Background::Color(p.accent2)),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 99.0.into() },
        ..container::Style::default()
    }
}

/// The dimmed backdrop behind a dialog.
pub fn modal_bg(_: &Theme) -> container::Style {
    container::Style { background: Some(Background::Color(Color::from_rgba8(6, 6, 8, 0.62))), ..container::Style::default() }
}

/// A dialog.
pub fn modal(_: &Theme) -> container::Style {
    let p = look();
    let mut s = boxed(p.panel, p.border, 18.0);
    s.shadow = shadow(&p);
    s
}

/// A pop-up menu or tooltip.
pub fn menu_pop(_: &Theme) -> container::Style {
    let p = look();
    let mut s = boxed(p.panel2, p.border, 10.0);
    s.shadow = shadow(&p);
    s
}

/// A toast.
pub fn toast(_: &Theme) -> container::Style {
    let p = look();
    let mut s = boxed(p.panel2, p.border, 12.0);
    s.shadow = shadow(&p);
    s
}

/// The red box an error is shown in.
pub fn error_box(_: &Theme) -> container::Style {
    let p = look();
    container::Style {
        text_color: Some(p.err),
        background: Some(Background::Color(p.err.scale_alpha(0.10))),
        border: Border { color: p.err.scale_alpha(0.35), width: 1.0, radius: 10.0.into() },
        ..container::Style::default()
    }
}

/// An amber box for something that needs attention but is not an error.
pub fn warn_box(_: &Theme) -> container::Style {
    let p = look();
    container::Style {
        text_color: Some(p.text),
        background: Some(Background::Color(p.warn.scale_alpha(0.10))),
        border: Border { color: p.warn.scale_alpha(0.4), width: 1.0, radius: 10.0.into() },
        ..container::Style::default()
    }
}

/// The frame around a row of tabs.
pub fn tabs_frame(_: &Theme) -> container::Style {
    let p = look();
    boxed(p.bg2, p.border, 12.0)
}

/// A message bubble from me: a soft accent tint.
pub fn bubble_mine(_: &Theme) -> container::Style {
    let p = look();
    boxed(mix(p.panel2, p.accent2, 0.10), Color::TRANSPARENT, 14.0)
}

/// The badge kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    /// Grey, for plain facts.
    Plain,
    /// Green, for good news.
    Ok,
    /// Amber, for something to look at.
    Warn,
    /// Red, for something wrong.
    Err,
    /// Blue, for a neutral note.
    Info,
    /// The accent colour, for what is yours.
    Accent,
}

/// The colours of a badge: (text, border, background).
pub fn badge_colors(kind: Badge) -> (Color, Color, Color) {
    let p = look();
    let tinted = |c: Color| (c, c.scale_alpha(0.35), c.scale_alpha(0.10));
    match kind {
        Badge::Plain => (p.muted, p.border, p.panel2),
        Badge::Ok => tinted(p.ok),
        Badge::Warn => tinted(p.warn),
        Badge::Err => tinted(p.err),
        Badge::Info => tinted(p.info),
        Badge::Accent => (p.accent2, p.accent2.scale_alpha(0.4), p.accent2.scale_alpha(0.12)),
    }
}

/// A badge as a container style.
pub fn badge(kind: Badge) -> impl Fn(&Theme) -> container::Style {
    move |_| {
        let (text, border, bg) = badge_colors(kind);
        container::Style {
            text_color: Some(text),
            background: Some(Background::Color(bg)),
            border: Border { color: border, width: 1.0, radius: 99.0.into() },
            ..container::Style::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_reading_text_stays_in_its_own_face() {
        let d = font_for("opendyslexic");
        assert_eq!(weighted(d, font::Weight::Semibold).weight, font::Weight::ExtraBold, "OpenDyslexic ships only 400 and 800");
        assert_eq!(weighted(d, font::Weight::Normal).weight, font::Weight::Normal);
        assert_eq!(weighted(FONT, font::Weight::Semibold).weight, font::Weight::Semibold, "Inter has its own semibold");
        assert_eq!(font_for("anything else"), FONT);
    }

    #[test]
    fn every_bundled_theme_parses_and_matches_settings() {
        let ids: Vec<&str> = bundled().iter().map(|b| b.id.as_str()).collect();
        // Settings' table, less `system`, which picks one of these.
        let offered: Vec<&str> = THEMES.iter().map(|t| t.0).filter(|&t| t != "system").collect();
        assert_eq!(ids, offered);
        for b in bundled() {
            let name = THEMES.iter().find(|t| t.0 == b.id).map(|t| t.1);
            assert_eq!(name, Some(b.name.as_str()), "{} is named differently in Settings", b.id);
            assert_eq!(b.dark, b.id != "light");
        }
    }

    #[test]
    fn a_built_in_theme_must_set_every_token() {
        let full = BUNDLED[0];
        assert!(read_base(full).is_ok());
        assert!(read_base(&full.replace("    muted:", "    // muted:")).is_err());
        assert!(read_base(&full.replace("avatars: [", "avatars: [#000, ")).is_err(), "seven avatars");
    }

    #[test]
    fn hex_colours_parse_strictly() {
        assert_eq!(parse_hex("#fff"), Some(Color::WHITE));
        assert_eq!(parse_hex("336699").map(to_hex).as_deref(), Some("#336699"));
        assert_eq!(parse_hex("#33669"), None);
        assert_eq!(parse_hex("#gg0000"), None);
        assert_eq!(parse_hex("+12345"), None, "from_str_radix would take a sign; a colour must not");
    }

    #[test]
    fn stoop_is_encore_with_pastel_status() {
        let p = palette("stoop", "logo", "", true);
        assert_eq!(to_hex(p.bg), "#161617");
        assert_eq!(to_hex(p.panel), "#222224");
        assert_eq!(to_hex(p.text), "#f0ead6");
        assert_eq!(to_hex(p.accent), "#f0ead6");
        assert_eq!(to_hex(p.on_accent), "#16161f", "a light accent gets dark text");
        assert_eq!(to_hex(p.ok), "#8ee3b0");
        assert_eq!(to_hex(p.err), "#f4a3a3");
    }

    #[test]
    fn system_follows_the_os() {
        assert!(!palette("system", "sky", "", false).dark);
        assert!(palette("system", "sky", "", true).dark);
    }

    #[test]
    fn cream_on_daylight_is_deepened() {
        let p = palette("light", "logo", "", true);
        assert!(luminance(p.accent) < 0.4, "the cream accent would vanish on a light ground");
        assert_eq!(to_hex(p.on_accent), "#ffffff");
    }

    #[test]
    fn custom_accent_gets_a_partner() {
        let p = palette("oled", "custom", "#e5e5e5", true);
        assert_eq!(to_hex(p.accent), "#e5e5e5");
        assert_eq!(to_hex(p.bg), "#000000");
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(to_hex(hex("#8b5cf6")), "#8b5cf6");
        assert_eq!(to_hex(hex("fff")), "#ffffff");
        assert_eq!(to_hex(hex("nonsense")), "#808080");
    }

    #[test]
    fn avatar_colours_are_stable() {
        assert_eq!(avatar_color("Robin"), avatar_color("Robin"));
    }
}
