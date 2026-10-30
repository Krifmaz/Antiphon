//! Stoop for the desktop.
//!
//! The interface is `antiphon-ui`, the same screens the phone app draws;
//! this binary supplies the call engine behind them (`engine`) and the iced
//! window around them (`app`).
//!
//! Flags:
//! - `--profile <name>`: a separate identity and settings, for running two
//!   copies side by side.
//! - `--demo`: made-up people and a call, nothing on the network.
//! - `--screenshot-tour <dir>`: the demo, walked through each screen, saved
//!   as PNGs in `dir`, then exits.

mod a11y;
mod account;
mod api;
mod app;
mod demo;
mod engine;
mod my_devices;
mod spaces;

use std::path::PathBuf;

use antiphon_client::{config, crash_report};
use antiphon_ui::theme;

/// The value after flag `name` (`--name value` or `--name=value`).
fn flag_value(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next();
        }
        if let Some(rest) = arg.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Whether flag `name` was given.
fn has_flag(name: &str) -> bool {
    std::env::args().skip(1).any(|a| a == name)
}

/// Writes a crash report on panic, if the person allowed it (spec §13).
fn install_crash_hook() {
    let enabled = config::load().crash_reporting;
    let dir = config::config_dir();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = &dir {
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .map(ToString::to_string)
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panicked".to_string());
            let location = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
            let report = crash_report::build(&message, &location, env!("CARGO_PKG_VERSION"), engine::now_secs());
            crash_report::save_if_enabled(dir, enabled, &report);
        }
        previous(info);
    }));
}

/// Runs with no window: the engine, its door and the local API, for a bot
/// (spec §11 "Bots / API, local-first"). Usually with a profile of its
/// own, so the bot is a separate identity: `--profile mybot --headless`.
/// What the window would show as a toast is printed instead. Runs until
/// the process is stopped.
fn headless(state: std::sync::Arc<antiphon_client::state::AppState>) -> anyhow::Result<()> {
    let mut engine = engine::Engine::new(state);
    engine.headless();
    loop {
        engine.tick(None);
        for out in std::mem::take(&mut engine.out) {
            if let engine::Out::Toast(text, _) = out {
                println!("{text}");
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// The swirl, for the title bar and taskbar.
fn window_icon() -> Option<iced::window::Icon> {
    let img = image::load_from_memory(include_bytes!("../../gui/ui/logo.png")).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    iced::window::icon::from_rgba(img.into_raw(), w, h).ok()
}

fn main() -> anyhow::Result<()> {
    let profile = config::set_profile(flag_value("--profile").as_deref());
    install_crash_hook();

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    // Background work keeps spawning onto the runtime for as long as the
    // window is open; it lives until the process does.
    let handle = runtime.handle().clone();
    std::mem::forget(runtime);

    if has_flag("--headless") {
        let _guard = handle.enter();
        return headless(engine::start_state(handle.clone())?);
    }

    let mode = if let Some(dir) = flag_value("--screenshot-tour") {
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir)?;
        app::Mode::Tour(dir)
    } else if has_flag("--demo") {
        app::Mode::Demo
    } else {
        app::Mode::Live(engine::start_state(handle.clone())?)
    };
    let _guard = handle.enter();

    let title_suffix = profile.map(|p| format!(" ({p})")).unwrap_or_default();
    let mut app = iced::application(move || app::App::boot(mode.clone()), app::App::update, app::App::view)
        .title(move |a: &app::App| format!("{}{title_suffix}", a.title()))
        .theme(app::App::theme)
        .subscription(app::App::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(1280.0, 820.0),
            min_size: Some(iced::Size::new(360.0, 560.0)),
            icon: window_icon(),
            exit_on_close_request: false,
            // On Windows and macOS the screen-reader adapter must hook the
            // window before it is first shown; the app shows it after.
            visible: !a11y::START_HIDDEN,
            ..Default::default()
        })
        .antialiasing(true)
        .default_font(theme::FONT);
    for bytes in theme::FONT_FILES {
        app = app.font(bytes);
    }
    app.run()?;
    Ok(())
}
