//! The icon set: 24-pixel line icons (two-pixel round strokes), drawn as
//! SVG and tinted to whatever colour the caller asks for, so an icon always
//! matches the text beside it in every theme.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use iced_core::{Color, Length};
use iced_widget::svg::{self, Handle};

use crate::El;

/// Every icon the app draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    /// A house: the home screen.
    Home,
    /// A handset: start a call.
    Phone,
    /// A handset, crossed: hang up.
    PhoneOff,
    /// A microphone.
    Mic,
    /// A microphone, crossed: muted.
    MicOff,
    /// Headphones.
    Headphones,
    /// Headphones, crossed: deafened.
    HeadphonesOff,
    /// A cog: settings.
    Gear,
    /// A plus: add.
    Plus,
    /// A cross: close or remove.
    X,
    /// A paper plane: send a message.
    Send,
    /// A paperclip: attach a file.
    Paperclip,
    /// A QR code: your invite to scan.
    Qr,
    /// A chain link: an invite link.
    Link,
    /// Two sheets: copy.
    Copy,
    /// A tick: done or delivered.
    Check,
    /// A magnifier: search.
    Search,
    /// A shield: trust and safety.
    Shield,
    /// A shield with a tick: verified.
    ShieldCheck,
    /// Two people: a group.
    Users,
    /// A person with a plus: add someone.
    UserPlus,
    /// Wi-Fi waves: the network.
    Wifi,
    /// A globe: your public address.
    Globe,
    /// A page: a file.
    File,
    /// An arrow into a tray: save.
    Download,
    /// A split window: show or hide the details panel.
    Panel,
    /// Three dots: more options.
    More,
    /// A left arrow: go back.
    Back,
    /// A small right arrow: go into.
    Chevron,
    /// A closed door: not taking calls.
    Door,
    /// An open door: taking calls.
    DoorOpen,
    /// A speech bubble: messages.
    Message,
    /// A smiling face: reactions.
    Smile,
    /// A padlock: encrypted.
    Lock,
    /// A circular arrow: check again.
    Refresh,
    /// A bin: delete.
    Trash,
    /// A pencil: edit.
    Edit,
    /// A bell: notifications.
    Bell,
    /// A bell, crossed: notifications off.
    BellOff,
    /// A speaker: volume.
    Volume,
    /// A speaker, crossed: silenced.
    VolumeOff,
    /// A sun: the light theme.
    Sun,
    /// A moon: the dark theme.
    Moon,
    /// A painter's palette: appearance.
    Palette,
    /// Sliders: voice clean-up.
    Sliders,
    /// An i in a circle: about.
    Info,
    /// A clock: time or expiry.
    Clock,
    /// A sparkle: something new.
    Sparkle,
    /// A keyboard: push-to-talk key.
    Keyboard,
    /// A chevron pointing up: earlier, sooner.
    Up,
    /// A chevron pointing down: later.
    Down,
    /// A hash: a channel in a community.
    Hash,
    /// A door with an arrow out: leave.
    LogOut,
    /// Two speech bubbles: group chats and communities.
    Spaces,
    /// A video camera: turn the camera on.
    Video,
    /// A video camera, crossed: the camera is off.
    VideoOff,
    /// A screen with an arrow up out of it: share your screen.
    Screen,
    /// A screen, crossed: stop sharing.
    ScreenOff,
}

impl Icon {
    /// The SVG body: everything inside the `<svg>` element.
    fn body(self) -> &'static str {
        match self {
            Icon::Home => r#"<path d="M3 10.2 12 3l9 7.2"/><path d="M5 8.8V19a2 2 0 0 0 2 2h3v-6h4v6h3a2 2 0 0 0 2-2V8.8"/>"#,
            Icon::Phone => r#"<path d="M22 16.9v3a2 2 0 0 1-2.2 2 19.8 19.8 0 0 1-8.6-3.1 19.5 19.5 0 0 1-6-6A19.8 19.8 0 0 1 2.1 4.2 2 2 0 0 1 4.1 2h3a2 2 0 0 1 2 1.7c.1.9.4 1.8.7 2.7a2 2 0 0 1-.5 2.1L8 9.8a16 16 0 0 0 6 6l1.3-1.3a2 2 0 0 1 2.1-.4c.9.3 1.8.6 2.7.7a2 2 0 0 1 1.7 2z"/>"#,
            Icon::PhoneOff => r#"<path d="M10.7 13.3a16 16 0 0 0 3.4 2.6l1.3-1.3a2 2 0 0 1 2.1-.4c.9.3 1.8.6 2.7.7a2 2 0 0 1 1.7 2v3a2 2 0 0 1-2.2 2 19.8 19.8 0 0 1-8.6-3.1 19.4 19.4 0 0 1-3.3-2.7M5.2 13.4A19.8 19.8 0 0 1 2.1 4.2 2 2 0 0 1 4.1 2h3a2 2 0 0 1 2 1.7c.1.9.4 1.8.7 2.7a2 2 0 0 1-.5 2.1L8 9.8"/><path d="M22 2 2 22"/>"#,
            Icon::Mic => r#"<rect x="9" y="2" width="6" height="12" rx="3"/><path d="M5 10v1a7 7 0 0 0 14 0v-1"/><path d="M12 18v4"/>"#,
            Icon::MicOff => r#"<path d="M15 9.3V5a3 3 0 0 0-5.7-1.3"/><path d="M9 9v2a3 3 0 0 0 5 2.2"/><path d="M19 10v1a7 7 0 0 1-.9 3.4M5 10v1a7 7 0 0 0 11.6 5.3"/><path d="M12 18v4"/><path d="m3 3 18 18"/>"#,
            Icon::Headphones => r#"<path d="M3 14v-2a9 9 0 0 1 18 0v2"/><rect x="3" y="14" width="5" height="7" rx="2"/><rect x="16" y="14" width="5" height="7" rx="2"/>"#,
            Icon::HeadphonesOff => r#"<path d="M5.6 5.6A9 9 0 0 0 3 12v2"/><path d="M8.7 3.6A9 9 0 0 1 21 12v2"/><rect x="3" y="14" width="5" height="7" rx="2"/><path d="M16 16v3a2 2 0 0 0 2 2h1"/><path d="m3 3 18 18"/>"#,
            Icon::Gear => r#"<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 0 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 0 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 0 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 0 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"/>"#,
            Icon::Plus => r#"<path d="M12 5v14M5 12h14"/>"#,
            Icon::X => r#"<path d="M18 6 6 18M6 6l12 12"/>"#,
            Icon::Send => r#"<path d="m22 2-11 11"/><path d="m22 2-7 20-4-9-9-4z"/>"#,
            Icon::Paperclip => r#"<path d="m21.4 11.1-9.2 9.2a6 6 0 0 1-8.5-8.5l9.2-9.2a4 4 0 0 1 5.7 5.7l-9.2 9.2a2 2 0 0 1-2.8-2.8l8.5-8.5"/>"#,
            Icon::Qr => r#"<rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><path d="M14 14h3v3h-3zM20 14v.01M14 20h.01M17 20h4v-3"/>"#,
            Icon::Link => r#"<path d="M10 13a5 5 0 0 0 7.5.5l3-3a5 5 0 0 0-7-7l-1.7 1.7"/><path d="M14 11a5 5 0 0 0-7.5-.5l-3 3a5 5 0 0 0 7 7l1.7-1.7"/>"#,
            Icon::Copy => r#"<rect x="9" y="9" width="12" height="12" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>"#,
            Icon::Check => r#"<path d="M20 6 9 17l-5-5"/>"#,
            Icon::Search => r#"<circle cx="11" cy="11" r="7"/><path d="m21 21-4.3-4.3"/>"#,
            Icon::Shield => r#"<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/>"#,
            Icon::ShieldCheck => r#"<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/><path d="m9 12 2 2 4-4"/>"#,
            Icon::Users => r#"<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.9M16 3.1a4 4 0 0 1 0 7.8"/>"#,
            Icon::UserPlus => r#"<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M19 8v6M22 11h-6"/>"#,
            Icon::Wifi => r#"<path d="M5 12.6a10 10 0 0 1 14 0M8.5 16.1a5 5 0 0 1 7 0M2 8.8a15 15 0 0 1 20 0M12 20h.01"/>"#,
            Icon::Globe => r#"<circle cx="12" cy="12" r="10"/><path d="M2 12h20"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/>"#,
            Icon::File => r#"<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/>"#,
            Icon::Download => r#"<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m7 10 5 5 5-5"/><path d="M12 15V3"/>"#,
            Icon::Panel => r#"<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M15 3v18"/>"#,
            Icon::More => r#"<circle cx="5" cy="12" r="1.3" fill="white"/><circle cx="12" cy="12" r="1.3" fill="white"/><circle cx="19" cy="12" r="1.3" fill="white"/>"#,
            Icon::Back => r#"<path d="m15 18-6-6 6-6"/>"#,
            Icon::Chevron => r#"<path d="m9 18 6-6-6-6"/>"#,
            Icon::Door => r#"<path d="M5 21V4a1 1 0 0 1 1-1h12a1 1 0 0 1 1 1v17"/><path d="M3 21h18"/><path d="M15 12h.01"/>"#,
            Icon::DoorOpen => r#"<path d="M13 4h4a1 1 0 0 1 1 1v16"/><path d="M3 21h18"/><path d="M5 21V4.6a1 1 0 0 1 .8-1l6-1.2a1 1 0 0 1 1.2 1V21"/><path d="M10 12h.01"/>"#,
            Icon::Message => r#"<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>"#,
            Icon::Smile => r#"<circle cx="12" cy="12" r="10"/><path d="M8 14s1.5 2 4 2 4-2 4-2"/><path d="M9 9h.01M15 9h.01"/>"#,
            Icon::Lock => r#"<rect x="4" y="11" width="16" height="10" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/>"#,
            Icon::Refresh => r#"<path d="M21 12a9 9 0 0 1-15.5 6.2L3 16"/><path d="M3 21v-5h5"/><path d="M3 12a9 9 0 0 1 15.5-6.2L21 8"/><path d="M21 3v5h-5"/>"#,
            Icon::Trash => r#"<path d="M3 6h18"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>"#,
            Icon::Edit => r#"<path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z"/>"#,
            Icon::Bell => r#"<path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9"/><path d="M10.3 21a1.9 1.9 0 0 0 3.4 0"/>"#,
            Icon::BellOff => r#"<path d="M8.7 3A6 6 0 0 1 18 8a21 21 0 0 0 .6 5"/><path d="M17 17H3s3-2 3-9a4.7 4.7 0 0 1 .3-1.7"/><path d="M10.3 21a1.9 1.9 0 0 0 3.4 0"/><path d="m2 2 20 20"/>"#,
            Icon::Volume => r#"<path d="M11 5 6 9H2v6h4l5 4z"/><path d="M15.5 8.5a5 5 0 0 1 0 7M19 5a10 10 0 0 1 0 14"/>"#,
            Icon::VolumeOff => r#"<path d="M11 5 6 9H2v6h4l5 4z"/><path d="m22 9-6 6M16 9l6 6"/>"#,
            Icon::Sun => r#"<circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/>"#,
            Icon::Moon => r#"<path d="M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9z"/>"#,
            Icon::Palette => r#"<circle cx="13.5" cy="6.5" r="1" fill="white"/><circle cx="17.5" cy="10.5" r="1" fill="white"/><circle cx="8.5" cy="7.5" r="1" fill="white"/><circle cx="6.5" cy="12.5" r="1" fill="white"/><path d="M12 2a10 10 0 0 0 0 20 2 2 0 0 0 2-2v-.5a2 2 0 0 1 2-2h1.5A4.5 4.5 0 0 0 22 13 11 11 0 0 0 12 2z"/>"#,
            Icon::Sliders => r#"<path d="M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6"/>"#,
            Icon::Info => r#"<circle cx="12" cy="12" r="10"/><path d="M12 16v-4M12 8h.01"/>"#,
            Icon::Clock => r#"<circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/>"#,
            Icon::Sparkle => r#"<path d="M12 3l1.9 5.1L19 10l-5.1 1.9L12 17l-1.9-5.1L5 10l5.1-1.9z"/><path d="M19 15l.8 2.2L22 18l-2.2.8L19 21l-.8-2.2L16 18l2.2-.8z"/>"#,
            Icon::Video => r#"<path d="m16 13 5.2 3.5a.5.5 0 0 0 .8-.4V7.9a.5.5 0 0 0-.8-.4L16 11"/><rect x="2" y="6" width="14" height="12" rx="2"/>"#,
            Icon::VideoOff => r#"<path d="M10.7 6H14a2 2 0 0 1 2 2v3.3l1 1L22 8v8"/><path d="M16 16a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h2"/><path d="m2 2 20 20"/>"#,
            Icon::Screen => r#"<rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4M9 10l3-3 3 3M12 7v6"/>"#,
            Icon::ScreenOff => r#"<path d="M7 3h13a2 2 0 0 1 2 2v10a2 2 0 0 1-1 1.7M4 3.3A2 2 0 0 0 2 5v10a2 2 0 0 0 2 2h13"/><path d="M8 21h8M12 17v4"/><path d="m2 2 20 20"/>"#,
            Icon::Keyboard => r#"<rect x="2" y="5" width="20" height="14" rx="2"/><path d="M6 9h.01M10 9h.01M14 9h.01M18 9h.01M6 13h.01M18 13h.01M8 16h8M10 13h4"/>"#,
            Icon::Up => r#"<path d="m18 15-6-6-6 6"/>"#,
            Icon::Down => r#"<path d="m6 9 6 6 6-6"/>"#,
            Icon::Hash => r#"<path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18"/>"#,
            Icon::LogOut => r#"<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><path d="m16 17 5-5-5-5M21 12H9"/>"#,
            Icon::Spaces => r#"<path d="M14 9a2 2 0 0 1-2 2H6l-4 4V4a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2z"/><path d="M18 9h2a2 2 0 0 1 2 2v11l-4-4h-6a2 2 0 0 1-2-2v-1"/>"#,
        }
    }

    /// A cached handle for the icon; svg handles hash their bytes, so
    /// building one per frame would rehash every icon on every redraw.
    fn handle(self) -> Handle {
        static CACHE: OnceLock<Mutex<HashMap<Icon, Handle>>> = OnceLock::new();
        let mut cache = CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        cache
            .entry(self)
            .or_insert_with(|| {
                let svg = format!(
                    r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">{}</svg>"##,
                    self.body()
                );
                Handle::from_memory(svg.into_bytes())
            })
            .clone()
    }
}

/// `icon` at `size` pixels in `color`.
pub fn icon<'a, M: 'a>(i: Icon, size: f32, color: Color) -> El<'a, M> {
    svg::Svg::new(i.handle())
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(move |_, _| svg::Style { color: Some(color) })
        .into()
}

/// Stoop's mark: the swirl turned 45 degrees, cream on charcoal.
pub fn logo<'a, M: 'a>(size: f32) -> El<'a, M> {
    static LOGO: OnceLock<Handle> = OnceLock::new();
    let handle = LOGO
        .get_or_init(|| {
            Handle::from_memory(
                &br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><circle cx="32" cy="32" r="30" fill="#222224"/><g transform="rotate(45 32 32)" fill="none" stroke="#F0EAD6" stroke-width="5.5" stroke-linecap="round"><path d="M19.34 14.3 A13 13 0 0 0 29 36"/><path d="M44.66 49.7 A13 13 0 0 0 35 28"/></g></svg>"##[..],
            )
        })
        .clone();
    svg::Svg::new(handle).width(Length::Fixed(size)).height(Length::Fixed(size)).into()
}
