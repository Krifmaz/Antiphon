//! The tray icon, so Stoop keeps its door open with the window closed
//! (spec §7 "lives in the tray"): closing the window hides it, and the
//! tray's menu brings it back, mutes, or quits.
//!
//! This is a StatusNotifierItem over D-Bus ([`ksni`]), which every current
//! Linux and BSD desktop's panel shows without GTK; the module is compiled
//! only there. Where no panel is listening, [`Tray::start`] gives `None`
//! and closing the window quits as it always did.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

/// What the tray's menu asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    /// Show the window again (also a click on the icon).
    Show,
    /// Mute or unmute the call.
    ToggleMute,
    /// Quit Stoop.
    Quit,
}

/// The running tray icon: where its menu's requests arrive.
pub struct Tray {
    /// Requests from the menu, read on the app's tick.
    asks: Receiver<Ask>,
    /// A panel is showing the icon right now. It goes false when the
    /// panel goes away (a shell restart, the tray applet removed), so a
    /// window is never hidden with no icon to bring it back.
    shown: Arc<AtomicBool>,
    /// Keeps the icon on the panel; dropping it takes the icon away.
    _handle: ksni::Handle<Icon>,
}

impl Tray {
    /// Puts the icon on the panel, or `None` where there is no panel to
    /// show it. Must be called inside the tokio runtime.
    pub fn start() -> Option<Self> {
        use ksni::TrayMethods;
        let (tx, asks) = std::sync::mpsc::channel();
        let shown = Arc::new(AtomicBool::new(true));
        let icon = Icon { tx, pixmap: pixmap(), shown: shown.clone() };
        // Called once at start, on the window's thread, before any of the
        // runtime's tasks need it.
        let handle = tokio::runtime::Handle::current().block_on(icon.spawn());
        match handle {
            Ok(handle) => Some(Tray { asks, shown, _handle: handle }),
            Err(e) => {
                eprintln!("[tray] no tray icon: {e}");
                None
            }
        }
    }

    /// Whether a panel is showing the icon, so hiding the window is safe.
    pub fn shown(&self) -> bool {
        self.shown.load(Ordering::Relaxed)
    }

    /// The requests made since the last call, oldest first.
    pub fn asks(&self) -> Vec<Ask> {
        self.asks.try_iter().collect()
    }
}

/// The icon itself, as the D-Bus service sees it.
pub struct Icon {
    /// Where menu requests go.
    tx: std::sync::mpsc::Sender<Ask>,
    /// The picture, decoded once.
    pixmap: Vec<ksni::Icon>,
    /// Shared with [`Tray::shown`].
    shown: Arc<AtomicBool>,
}

impl ksni::Tray for Icon {
    fn id(&self) -> String {
        "stoop".into()
    }

    fn title(&self) -> String {
        "Stoop".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.pixmap.clone()
    }

    fn watcher_online(&self) {
        self.shown.store(true, Ordering::Relaxed);
    }

    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        self.shown.store(false, Ordering::Relaxed);
        // Keep running: the panel usually comes back.
        true
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(Ask::Show);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        // Each item only passes its request on; the app does the work.
        let item = |label: &str, ask: Ask| -> ksni::MenuItem<Self> {
            StandardItem { label: label.into(), activate: Box::new(move |me: &mut Self| {
                    let _ = me.tx.send(ask);
                }), ..Default::default() }.into()
        };
        vec![item("Show Stoop", Ask::Show), item("Mute or unmute", Ask::ToggleMute), ksni::MenuItem::Separator, item("Quit", Ask::Quit)]
    }
}

/// The tray picture as ARGB32 in network byte order, which is what the
/// StatusNotifierItem protocol carries; empty if it cannot be decoded
/// (the panel then shows a generic icon).
fn pixmap() -> Vec<ksni::Icon> {
    let Ok(img) = image::load_from_memory(include_bytes!("../assets/tray-icon.png")) else { return Vec::new() };
    let img = img.to_rgba8();
    let (width, height) = img.dimensions();
    let data = img.pixels().flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
    vec![ksni::Icon { width: width as i32, height: height as i32, data }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tray_picture_decodes_to_argb() {
        let icons = pixmap();
        assert_eq!(icons.len(), 1);
        let icon = &icons[0];
        assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
    }
}
