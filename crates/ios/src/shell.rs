//! Starts the event loop and wraps the shared [`Runner`] for one thing
//! iOS does that Android does not: launch the app without ever bringing
//! it to the front.
//!
//! A VoIP push starts the process in the background. winit only reports
//! `resumed` when the app becomes active, and the shared runner creates
//! the app (identity, call engine, the platform event queue's waker) in
//! `resumed`. Without help, a push received on the lock screen would sit
//! in the queue until the user opened the app, and answering from the
//! CallKit screen would do nothing. So the platform code calls [`wake`]
//! after posting an event, and a wake that finds the app never started
//! and the process in the background brings it up headless: `resumed`
//! then `suspended`, which creates the app and drops the window again
//! before anything is drawn, since iOS refuses GPU work from the
//! background.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use antiphon_mobile::runner::{Runner, Wake};
use objc2_ui_kit::{UIApplication, UIApplicationState};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::WindowId;

use crate::{audio, callkit, logger, network, platform, screen};

/// A way into the event loop from other threads, set once the loop exists.
/// Behind a Mutex because the proxy is not promised to be `Sync`. A wake
/// before it is set is dropped, which is harmless: what was posted stays
/// in the queue, and the loop drains it once it runs.
static PROXY: OnceLock<Mutex<EventLoopProxy<Wake>>> = OnceLock::new();

/// Wakes the event loop from any thread. Called after every
/// `platform::post` the iOS code makes, which on its own only wakes the
/// loop once the shared app exists.
pub fn wake() {
    if let Some(proxy) = PROXY.get() {
        if let Ok(proxy) = proxy.lock() {
            let _ = proxy.send_event(Wake);
        }
    }
}

/// The app's private storage: `Library/Application Support`, which is
/// backed up but, unlike `Documents`, not shown in the Files app.
fn data_dir() -> PathBuf {
    let home = objc2_foundation::NSHomeDirectory().to_string();
    let dir = PathBuf::from(home).join("Library").join("Application Support").join("Antiphon");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("could not create {}: {e}", dir.display());
    }
    dir
}

/// Sets up logging, the platform modules and the event loop, then runs
/// the app. Must be called on the main thread, from `main`; it returns
/// only if the event loop cannot be created.
pub fn run() {
    logger::init();
    // A panic would otherwise vanish: nothing reads an iOS app's stderr.
    std::panic::set_hook(Box::new(|info| log::error!("panic: {info}")));
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        log::error!("not started on the main thread");
        return;
    };
    antiphon_mobile::platform::install(platform::Ios);
    audio::init();
    network::start();
    screen::observe_keyboard(mtm);
    // Before the event loop, so that when a VoIP push is what launched
    // the process, PushKit has its delegate by the time it delivers.
    callkit::restore_push(mtm);

    let event_loop = match EventLoop::<Wake>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(e) => {
            log::error!("could not create the event loop: {e}");
            return;
        }
    };
    let _ = PROXY.set(Mutex::new(event_loop.create_proxy()));
    let mut shell = Shell { runner: Runner::new(data_dir(), event_loop.create_proxy()), started: false };
    // Never returns on iOS: UIApplicationMain owns the thread from here.
    if let Err(e) = event_loop.run_app(&mut shell) {
        log::error!("event loop ended with an error: {e}");
    }
}

/// Whether UIKit says the app is in the background. `false` off the main
/// thread, where UIApplication cannot be asked, which is never the case
/// for its one caller: winit's handlers run on the main thread.
fn in_background() -> bool {
    objc2::MainThreadMarker::new().is_some_and(|mtm| {
        UIApplication::sharedApplication(mtm).applicationState() == UIApplicationState::Background
    })
}

/// The [`ApplicationHandler`] winit runs: the shared [`Runner`], plus the
/// headless start described in the module comment. Every method runs on
/// the main thread.
struct Shell {
    /// The shared app, which does all the real work.
    runner: Runner,
    /// Whether the runner has been resumed at least once, so the shared
    /// app exists.
    started: bool,
}

impl ApplicationHandler<Wake> for Shell {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        self.runner.new_events(event_loop, cause);
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.started = true;
        self.runner.resumed(event_loop);
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        self.runner.suspended(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Wake) {
        // Only in the background: at an ordinary launch an early wake (the
        // network monitor's first report) can beat `resumed` by a moment,
        // and the app is about to start with its window anyway.
        if !self.started && in_background() {
            log::info!("woken in the background; starting the app without a window");
            self.started = true;
            self.runner.resumed(event_loop);
            self.runner.suspended(event_loop);
        }
        self.runner.user_event(event_loop, event);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        self.runner.window_event(event_loop, id, event);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.runner.about_to_wait(event_loop);
    }
}
