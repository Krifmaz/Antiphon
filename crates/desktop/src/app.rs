//! The iced application: the shared interface on top of the engine (or on
//! top of made-up data, for `--demo` and `--screenshot-tour`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use antiphon_client::state::AppState;
use antiphon_ui::{self as ui, theme, Action, Msg, Route, Section, ToastKind, Ui};
use antiphon_update::{self as update, Updater};
use iced::keyboard::{self, key};
use iced::widget::operation;
use iced::{event, window, Element, Event, Subscription, Task, Theme};

use crate::demo::Demo;
use crate::engine::{Engine, Out};

/// How the app was started.
#[derive(Clone)]
pub enum Mode {
    /// The real engine, on this started client state.
    Live(Arc<AppState>),
    /// Made-up people and a call, nothing on the network.
    Demo,
    /// Demo data, walked through each screen, saved as PNGs in this folder.
    Tour(PathBuf),
}

/// Where the model comes from.
enum Source {
    /// The real engine.
    Live {
        /// The call engine.
        engine: Box<Engine>,
        /// The model last built from it: rebuilt after each change, not each frame.
        model: Box<ui::Model>,
    },
    /// The demo, which holds its own model.
    Demo(Box<Demo>),
}

/// The application: the shared interface and what feeds it.
pub struct App {
    /// The interface's own state: route, dialogs, drafts, animations.
    ui: Ui,
    /// Where the model comes from.
    source: Source,
    /// Where the screenshot tour saves its PNGs, when running one.
    tour: Option<PathBuf>,
    /// Whether the system asks for a dark look (for the "system" theme).
    system_dark: bool,
    /// What the look was last built from: theme, accent, custom accent,
    /// reading font, and whether the system is dark.
    look: (String, String, String, String, bool),
    /// The iced theme built from that look. Kept rather than built in
    /// [`App::theme`], which iced calls on every redraw: building one
    /// derives a whole extended palette.
    theme: Theme,
    /// When the app started, to animate the demo's meters.
    epoch: Instant,
    /// Looks for, downloads and installs newer versions. Only on the real
    /// engine: the demo and the tour never touch the network.
    updater: Option<Updater>,
    /// The version being installed, from the moment the download is handed
    /// over until the app closes for it (or the install fails).
    installing: Option<String>,
}

#[derive(Debug, Clone)]
/// Everything the app reacts to.
pub enum Message {
    /// Something the interface did.
    Ui(Msg),
    /// The regular tick: drain the engine and move timers on.
    Tick(Instant),
    /// A redraw while something animates.
    Frame(Instant),
    /// The system switched between light and dark.
    SystemMode(iced::theme::Mode),
    /// Escape was pressed: go back or close what is open.
    Escape,
    /// Cmd/Ctrl+K or F: jump to search.
    Find,
    /// The window's close button: hang up, close down, exit.
    Close,
    /// The window was made, this size: lay out for it, attach the
    /// screen-reader adapter, then show it.
    Opened(window::Id, iced::Size),
    /// The window gained (true) or lost the system's focus.
    WindowFocus(bool),
    /// Set up tour step `n`.
    TourStep(usize),
    /// Take tour step `n`'s screenshot.
    TourShoot(usize),
    /// Tour step `n`'s screenshot is ready to save.
    TourShot(usize, window::Screenshot),
}

/// One screen of the tour: a name, a window size, and how to get there.
struct Step {
    /// The PNG's name, without `.png`.
    name: &'static str,
    /// The window size for this shot, in logical pixels.
    size: (f32, f32),
    /// Puts the app on the screen to shoot.
    setup: fn(&mut App),
}

/// The screens the tour shoots, in order.
const TOUR: &[Step] = &[
    Step { name: "home", size: (1280.0, 820.0), setup: |app| app.go(Route::Home) },
    Step {
        name: "call",
        size: (1440.0, 900.0),
        setup: |app| {
            app.demo().start_call();
            app.go(Route::Person(Demo::robin()));
        },
    },
    Step {
        name: "video-call",
        size: (1440.0, 900.0),
        setup: |app| {
            app.demo().video_on();
            app.go(Route::Person(Demo::robin()));
        },
    },
    Step {
        name: "volume",
        size: (1280.0, 820.0),
        setup: |app| {
            app.go(Route::Home);
            let _ = app.send(Msg::Cursor(iced::Point::new(150.0, 262.0)));
            let theo = app.demo().model.people[1].id.clone();
            app.demo().act(Action::Volume(theo.clone(), -4.4));
            let _ = app.send(Msg::Menu(Some(theo)));
        },
    },
    Step { name: "settings", size: (1280.0, 820.0), setup: |app| app.go(Route::Settings(Section::Appearance)) },
    Step { name: "profile", size: (1280.0, 820.0), setup: |app| app.go(Route::Settings(Section::Profile)) },
    Step {
        name: "fine-tune",
        size: (1280.0, 900.0),
        setup: |app| {
            app.go(Route::Settings(Section::Voice));
            let _ = app.send(Msg::SettingsTab(2));
        },
    },
    Step {
        name: "my-profile",
        size: (1280.0, 820.0),
        setup: |app| {
            // The code placed on the banner, so the shot shows where it goes.
            let layout = app.demo().model.me.layout.with_qr(ui::QrPlace::Banner);
            app.demo().act(Action::SetProfileLayout(layout));
            app.go(Route::Profile(None));
        },
    },
    Step { name: "my-profile-edit", size: (1280.0, 820.0), setup: |app| app.go(Route::ProfileEdit) },
    Step { name: "their-profile", size: (1280.0, 820.0), setup: |app| app.go(Route::Profile(Some(Demo::robin()))) },
    Step {
        name: "chat",
        size: (1280.0, 820.0),
        setup: |app| {
            app.demo().act(Action::Hangup);
            app.go(Route::Person(Demo::robin()));
        },
    },
    Step { name: "voice", size: (1280.0, 820.0), setup: |app| app.go(Route::Settings(Section::Voice)) },
    Step { name: "account", size: (1280.0, 820.0), setup: |app| app.go(Route::Settings(Section::Account)) },
    Step { name: "community", size: (1280.0, 820.0), setup: |app| app.go(Route::Community(Demo::club(), 0)) },
    Step {
        name: "community-emoji",
        size: (1280.0, 820.0),
        setup: |app| {
            app.go(Route::Community(Demo::club(), 0));
            let _ = app.send(Msg::Dialog(Some(ui::Dialog::SpaceEmoji(Demo::club()))));
        },
    },
    Step {
        name: "community-notify",
        size: (1280.0, 820.0),
        setup: |app| {
            app.go(Route::Community(Demo::club(), 0));
            app.demo().act(Action::SpaceNotify { community: Demo::club(), channel: None, level: Some(ui::Notify::Mentions) });
            app.demo().act(Action::SpaceNotify { community: Demo::club(), channel: Some(1), level: Some(ui::Notify::All) });
            let _ = app.send(Msg::Dialog(Some(ui::Dialog::SpaceNotify(Demo::club()))));
        },
    },
    Step { name: "spaces", size: (1280.0, 820.0), setup: |app| {
            let _ = app.send(Msg::Dialog(None));
            app.go(Route::Spaces);
        } },
    Step {
        name: "invite",
        size: (1280.0, 820.0),
        setup: |app| {
            app.go(Route::Home);
            let _ = app.send(Msg::Dialog(Some(ui::Dialog::Invite)));
        },
    },
    Step {
        name: "phone",
        size: (400.0, 820.0),
        setup: |app| {
            let _ = app.send(Msg::Dialog(None));
            app.demo().as_phone(true);
            // A typical Android phone's status bar and gesture bar, so the
            // shots show the layout keeping clear of them.
            app.ui.set_insets(32.0, 24.0);
            app.go(Route::Home);
        },
    },
    Step { name: "phone-call", size: (400.0, 820.0), setup: |app| app.go(Route::Person(Demo::robin())) },
    Step { name: "phone-spaces", size: (400.0, 820.0), setup: |app| app.go(Route::Spaces) },
    Step { name: "phone-community", size: (400.0, 820.0), setup: |app| app.go(Route::Community(Demo::club(), 0)) },
    Step { name: "phone-profile", size: (400.0, 820.0), setup: |app| app.go(Route::Profile(None)) },
    Step { name: "phone-profile-edit", size: (400.0, 820.0), setup: |app| app.go(Route::ProfileEdit) },
    Step { name: "phone-their-profile", size: (400.0, 820.0), setup: |app| app.go(Route::Profile(Some(Demo::robin()))) },
    Step {
        name: "phone-chat",
        size: (400.0, 820.0),
        setup: |app| {
            app.demo().act(Action::Hangup);
            app.go(Route::Person(Demo::robin()));
        },
    },
    Step { name: "phone-settings", size: (400.0, 820.0), setup: |app| app.go(Route::SettingsMenu) },
    Step { name: "phone-settings-account", size: (400.0, 820.0), setup: |app| app.go(Route::Settings(Section::Account)) },
    Step {
        name: "light",
        size: (1280.0, 820.0),
        setup: |app| {
            app.demo().as_phone(false);
            app.ui.set_insets(0.0, 0.0);
            app.demo().act(Action::Theme("light".into()));
            app.go(Route::Person(Demo::robin()));
        },
    },
    Step {
        name: "reading-font",
        size: (1280.0, 820.0),
        setup: |app| {
            app.demo().act(Action::Theme("stoop".into()));
            app.demo().act(Action::Font("opendyslexic".into()));
            app.go(Route::Settings(Section::Appearance));
            // Two Tabs in: the focus ring on the first section in the list.
            let _ = app.send(Msg::Focus(1));
            let _ = app.send(Msg::Focus(1));
        },
    },
];

impl App {
    /// Starts the app in `mode`; a tour queues its first step.
    pub fn boot(mode: Mode) -> (Self, Task<Message>) {
        let (source, tour, updater) = match mode {
            Mode::Live(state) => {
                let mut engine = Box::new(Engine::new(state));
                let model = engine.model(None);
                // The setting lives beside the app's own; downloads go to a
                // folder of the updater's own under the system's temporary one.
                let updater = antiphon_client::config::config_dir().map(|dir| Updater::new(&dir, std::env::temp_dir().join("stoop-update")));
                (Source::Live { engine, model: Box::new(model) }, None, updater)
            }
            Mode::Demo => (Source::Demo(Box::new(Demo::new())), None, None),
            Mode::Tour(dir) => (Source::Demo(Box::new(Demo::new())), Some(dir), None),
        };
        let mut app = App { ui: Ui::new(), source, tour, system_dark: true, look: Default::default(), theme: theme::iced_theme(&theme::look()), epoch: Instant::now(), updater, installing: None };
        app.stamp_update();
        app.apply_look();
        let mut tasks = vec![iced::system::theme().map(Message::SystemMode)];
        if app.tour.is_some() {
            tasks.push(Task::done(Message::TourStep(0)));
        }
        (app, Task::batch(tasks))
    }

    /// The current model, whichever source it comes from.
    fn model(&self) -> &ui::Model {
        match &self.source {
            Source::Live { model, .. } => model,
            Source::Demo(d) => &d.model,
        }
    }

    /// The demo source; only the tour calls this.
    fn demo(&mut self) -> &mut Demo {
        match &mut self.source {
            Source::Demo(d) => d,
            Source::Live { .. } => unreachable!("the tour runs on demo data"),
        }
    }

    /// Feeds the interface a message and carries out what comes back.
    fn send(&mut self, msg: Msg) -> Task<Message> {
        let model = match &self.source {
            Source::Live { model, .. } => model,
            Source::Demo(d) => &d.model,
        };
        let mut actions = self.ui.update(msg, model);
        // Moving the typing cursor is for the window, not the engine.
        let mut tasks = Vec::new();
        actions.retain(|a| match a {
            Action::FocusInput(Some(id)) => {
                tasks.push(operation::focus(*id));
                false
            }
            Action::FocusInput(None) => {
                tasks.push(iced::advanced::widget::operate(iced::advanced::widget::operation::focusable::unfocus()));
                false
            }
            Action::RevealFocus(input) => {
                // Runs on the frame drawn after this update, where the
                // newly focused control carries its marker.
                tasks.push(iced::advanced::widget::operate(ui::a11y::reveal(*input)));
                false
            }
            _ => true,
        });
        tasks.push(self.run(actions));
        Task::batch(tasks)
    }

    /// Moves the interface to `route`.
    fn go(&mut self, route: Route) {
        let _ = self.send(Msg::Nav(route));
    }

    /// Carries out the interface's actions on the source, then refreshes.
    fn run(&mut self, actions: Vec<Action>) -> Task<Message> {
        let mut tasks = Vec::new();
        let opened = actions.iter().any(|a| matches!(a, Action::Open(_)));
        for action in actions {
            // Updates are the app's, not the engine's.
            let action = match self.update_action(action) {
                Ok(task) => {
                    tasks.push(task);
                    continue;
                }
                Err(action) => action,
            };
            match &mut self.source {
                Source::Live { engine, .. } => {
                    let open = self.ui.open_person().map(str::to_string);
                    engine.act(action, open.as_deref());
                }
                Source::Demo(d) => {
                    if let Action::Copy(text) = &action {
                        tasks.push(iced::clipboard::write(text.clone()));
                    }
                    d.act(action);
                }
            }
        }
        tasks.push(self.drain());
        if opened && !self.ui.narrow() {
            tasks.push(operation::focus(ui::COMPOSER));
        }
        self.refresh();
        Task::batch(tasks)
    }

    /// Carries the engine's results into the interface.
    fn drain(&mut self) -> Task<Message> {
        let Source::Live { engine, model } = &mut self.source else { return Task::none() };
        let outs = std::mem::take(&mut engine.out);
        let mut tasks = Vec::new();
        for out in outs {
            match out {
                Out::Toast(text, kind) => self.ui.toast(text, kind),
                Out::Copy(text) => tasks.push(iced::clipboard::write(text)),
                Out::Relaunch => {
                    // The toast saying so was queued just before; the new
                    // copy shows the restored account.
                    engine.shutdown();
                    crate::account::relaunch();
                    tasks.push(iced::exit());
                }
                Out::AddFailed(why) => self.ui.add_failed(why),
                Out::Show(route) => {
                    for a in self.ui.show(route) {
                        let open = self.ui.open_person().map(str::to_string);
                        engine.act(a, open.as_deref());
                    }
                }
                Out::Added(id) => {
                    for a in self.ui.added(id) {
                        let open = self.ui.open_person().map(str::to_string);
                        engine.act(a, open.as_deref());
                    }
                }
                Out::SpaceCreated(id) => {
                    for a in self.ui.space_created(id) {
                        let open = self.ui.open_person().map(str::to_string);
                        engine.act(a, open.as_deref());
                    }
                }
            }
        }
        let open = self.ui.open_person().map(str::to_string);
        engine.set_viewing(self.ui.open_channel());
        **model = engine.model(open.as_deref());
        if !engine.out.is_empty() {
            tasks.push(Task::done(Message::Tick(Instant::now())));
        }
        self.stamp_update();
        Task::batch(tasks)
    }

    /// Rebuilds the model from the engine and repaints if the look changed.
    fn refresh(&mut self) {
        let open = self.ui.open_person().map(str::to_string);
        if let Source::Live { engine, model } = &mut self.source {
            engine.set_viewing(self.ui.open_channel());
            **model = engine.model(open.as_deref());
        }
        self.stamp_update();
        self.apply_look();
    }

    /// Copies where the updater stands into the model, which the engine
    /// rebuilds without it.
    fn stamp_update(&mut self) {
        let Source::Live { model, .. } = &mut self.source else { return };
        let Some(u) = &self.updater else { return };
        let state = match (u.status(), &self.installing) {
            (_, Some(v)) => ui::UpdateState::Installing(v.clone()),
            (update::Status::Idle, _) => ui::UpdateState::Idle,
            (update::Status::Checking, _) => ui::UpdateState::Checking,
            (update::Status::UpToDate, _) => ui::UpdateState::UpToDate,
            (update::Status::Available(r), _) => ui::UpdateState::Available(r.version.to_string()),
            (update::Status::Downloading { release, done, total }, _) => {
                ui::UpdateState::Downloading(release.version.to_string(), total.filter(|t| *t > 0).map(|t| (done as f32 / t as f32).min(1.0)))
            }
            // Ready is handed over at once, so it shows as installing.
            (update::Status::Ready { release, .. }, _) => ui::UpdateState::Installing(release.version.to_string()),
            (update::Status::Failed(why), _) => ui::UpdateState::Failed(why),
        };
        model.update = ui::Update { available: u.target().can_check(), auto: u.auto(), state, banner: u.banner(), can_install: u.can_install() };
        // The About screen shows the full version, a hotfix's run number
        // included, which the crate version has no room for.
        model.settings.version = u.current().to_string();
    }

    /// Carries out an update action, or gives back any other action for the
    /// engine.
    fn update_action(&mut self, action: Action) -> Result<Task<Message>, Action> {
        let Some(u) = &mut self.updater else {
            // The demo has no updater; its buttons do nothing.
            return match action {
                Action::UpdateCheck | Action::UpdateInstall | Action::UpdateLater | Action::UpdatePage | Action::UpdateAuto(_) => Ok(Task::none()),
                other => Err(other),
            };
        };
        match action {
            Action::UpdateCheck => u.check(),
            Action::UpdateInstall if u.can_install() => u.start(),
            // Nothing this copy can install: the page has the files.
            Action::UpdateInstall | Action::UpdatePage => {
                if let Err(why) = update::install::open(u.page()) {
                    self.ui.toast(why, ToastKind::Err);
                }
            }
            Action::UpdateLater => u.later(),
            Action::UpdateAuto(on) => u.set_auto(on),
            other => return Err(other),
        }
        self.stamp_update();
        Ok(Task::none())
    }

    /// Installs a downloaded update, once the updater hands one over: closes
    /// down for an installer or a restart, or says what the system will do.
    fn install_update(&mut self, file: &Path) -> Task<Message> {
        let Some(u) = &mut self.updater else { return Task::none() };
        let version = u.status().release().map(|r| r.version.to_string()).unwrap_or_default();
        self.installing = Some(version.clone());
        let result = update::install::install(u.target(), file);
        let task = match result {
            Ok(update::After::Quit) => Task::done(Message::Close),
            Ok(update::After::Restart(path)) => match update::install::relaunch(&path) {
                Ok(()) => Task::done(Message::Close),
                Err(why) => {
                    // In place but not started: it starts next time.
                    self.ui.toast(format!("Stoop {version} is installed. {why}."), ToastKind::Warn);
                    Task::none()
                }
            },
            Ok(update::After::Handed(next)) => {
                self.installing = None;
                self.ui.toast(next, ToastKind::Info);
                Task::none()
            }
            Err(why) => {
                self.installing = None;
                u.failed(why.clone());
                self.ui.toast(why, ToastKind::Err);
                Task::none()
            }
        };
        self.stamp_update();
        task
    }

    /// Repaints the palette and switches the reading font when the theme,
    /// accent, font or system tone changed.
    fn apply_look(&mut self) {
        let s = &self.model().settings;
        // Compared by reference first: this runs on every tick, and the
        // look only rarely changes.
        let (t, a, c, f) = (&self.look.0, &self.look.1, &self.look.2, &self.look.3);
        if (&s.theme, &s.accent, &s.custom_accent, &s.font, self.system_dark) != (t, a, c, f, self.look.4) {
            let look = (s.theme.clone(), s.accent.clone(), s.custom_accent.clone(), s.font.clone(), self.system_dark);
            theme::set(theme::palette(&look.0, &look.1, &look.2, look.4));
            theme::set_font(&look.3);
            self.theme = theme::iced_theme(&theme::look());
            self.look = look;
        }
        let reduced = self.model().settings.reduced_motion;
        self.ui.set_reduced_motion(reduced);
    }

    /// Handles one message.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Ui(msg) => self.send(msg),
            Message::Tick(now) => {
                let model = match &self.source {
                    Source::Live { model, .. } => model,
                    Source::Demo(d) => &d.model,
                };
                let _ = self.ui.update(Msg::Frame(now), model);
                let open = self.ui.open_person().map(str::to_string);
                let t = self.epoch.elapsed().as_secs_f32();
                // A look that is due starts here; a finished download comes
                // back to be installed.
                let ready = self.updater.as_mut().and_then(Updater::tick);
                let task = match &mut self.source {
                    Source::Live { engine, .. } => {
                        engine.tick(open.as_deref());
                        let drained = self.drain();
                        match ready {
                            Some(file) => Task::batch([drained, self.install_update(&file)]),
                            None => drained,
                        }
                    }
                    Source::Demo(d) => {
                        d.tick(t);
                        Task::none()
                    }
                };
                self.apply_look();
                task
            }
            Message::Frame(now) => {
                let model = match &self.source {
                    Source::Live { model, .. } => model,
                    Source::Demo(d) => &d.model,
                };
                let _ = self.ui.update(Msg::Frame(now), model);
                Task::none()
            }
            Message::SystemMode(mode) => {
                self.system_dark = mode != iced::theme::Mode::Light;
                self.apply_look();
                Task::none()
            }
            Message::Escape => self.send(Msg::Back),
            Message::Opened(id, size) => {
                let _ = self.send(Msg::Resized(size));
                let attach = window::run(id, crate::a11y::install);
                if crate::a11y::START_HIDDEN {
                    // The adapter is in; now the window may be seen.
                    attach.then(move |_| window::set_mode(id, window::Mode::Windowed))
                } else {
                    attach.discard()
                }
            }
            Message::WindowFocus(on) => {
                crate::a11y::window_focused(on);
                Task::none()
            }
            Message::Find => operation::focus(ui::SEARCH),
            Message::Close => {
                if let Source::Live { engine, .. } = &mut self.source {
                    engine.shutdown();
                }
                iced::exit()
            }
            Message::TourStep(i) => {
                let Some(step) = TOUR.get(i) else { return iced::exit() };
                (step.setup)(self);
                let size = iced::Size::new(step.size.0, step.size.1);
                window::latest()
                    .and_then(move |id| window::resize(id, size))
                    .chain(Task::perform(tokio::time::sleep(Duration::from_millis(1200)), move |_| Message::TourShoot(i)))
            }
            Message::TourShoot(i) => window::latest().and_then(window::screenshot).map(move |shot| Message::TourShot(i, shot)),
            Message::TourShot(i, shot) => {
                if let (Some(dir), Some(step)) = (&self.tour, TOUR.get(i)) {
                    let path = dir.join(format!("{}.png", step.name));
                    match image::RgbaImage::from_raw(shot.size.width, shot.size.height, shot.rgba.to_vec()) {
                        Some(img) => match img.save(&path) {
                            Ok(()) => println!("saved {}", path.display()),
                            Err(e) => eprintln!("couldn't save {}: {e}", path.display()),
                        },
                        None => eprintln!("screenshot {} had the wrong size", step.name),
                    }
                }
                Task::done(Message::TourStep(i + 1))
            }
        }
    }

    /// Draws the interface.
    pub fn view(&self) -> Element<'_, Message> {
        let view = self.ui.view(self.model()).map(Message::Ui);
        // The controls registered themselves while the view was built;
        // pass that list on to a screen reader, if one is listening.
        crate::a11y::publish(&self.title());
        view
    }

    /// The iced theme built from the current palette (see `theme`).
    pub fn theme(&self) -> Theme {
        self.theme.clone()
    }

    /// The window title: who you are on a call with, if anyone.
    pub fn title(&self) -> String {
        let m = self.model();
        match &m.call.phase {
            ui::Phase::Connected => format!("{} · Stoop", m.call.peer_name),
            ui::Phase::Dialing => format!("Calling {} · Stoop", m.call.peer_name),
            _ => "Stoop".to_string(),
        }
    }

    /// The tick (faster during a call), window and key events, and frames while animating.
    pub fn subscription(&self) -> Subscription<Message> {
        let busy = match &self.source {
            Source::Live { engine, .. } => engine.busy(),
            Source::Demo(d) => d.model.call.active(),
        };
        let tick = iced::time::every(if busy { Duration::from_millis(50) } else { Duration::from_millis(400) }).map(Message::Tick);
        let mut subs = vec![
            tick,
            event::listen_with(on_event),
            window::close_requests().map(|_| Message::Close),
            crate::a11y::requests().map(Message::Ui),
        ];
        if self.ui.animating() {
            subs.push(window::frames().map(Message::Frame));
        }
        Subscription::batch(subs)
    }
}

/// Window and keyboard events the interface cares about.
///
/// Besides Escape and the find shortcut, these are the keyboard-only keys
/// (see `antiphon_ui::a11y`): Tab and Shift+Tab move focus, Enter and
/// Space press, the arrows step a slider or choice, and Shift+F10 or the
/// menu key open a right-click menu. Each acts only when no widget used
/// the key first, so typing in a box is never taken over.
fn on_event(event: Event, status: event::Status, window: window::Id) -> Option<Message> {
    use key::Named;
    let free = status == event::Status::Ignored;
    match event {
        Event::Mouse(iced::mouse::Event::CursorMoved { position }) => Some(Message::Ui(Msg::Cursor(position))),
        Event::Window(window::Event::Resized(size)) => Some(Message::Ui(Msg::Resized(size))),
        Event::Window(window::Event::Opened { size, .. }) => Some(Message::Opened(window, size)),
        Event::Window(window::Event::Focused) => Some(Message::WindowFocus(true)),
        Event::Window(window::Event::Unfocused) => Some(Message::WindowFocus(false)),
        Event::Keyboard(keyboard::Event::KeyPressed { key: keyboard::Key::Named(named), modifiers, .. }) if free && named != Named::Escape => {
            let msg = match named {
                Named::Tab if !modifiers.command() && !modifiers.alt() => Msg::Focus(if modifiers.shift() { -1 } else { 1 }),
                Named::Enter | Named::Space => Msg::Activate,
                Named::ArrowUp | Named::ArrowRight => Msg::Adjust(1),
                Named::ArrowDown | Named::ArrowLeft => Msg::Adjust(-1),
                Named::F10 if modifiers.shift() => Msg::ContextMenu,
                Named::ContextMenu => Msg::ContextMenu,
                _ => return None,
            };
            Some(Message::Ui(msg))
        }
        Event::Keyboard(keyboard::Event::KeyPressed { key: keyboard::Key::Character(c), .. }) if free && c.as_str() == " " => Some(Message::Ui(Msg::Activate)),
        Event::Keyboard(keyboard::Event::KeyPressed { key: keyboard::Key::Named(Named::Escape), .. }) => Some(Message::Escape),
        Event::Keyboard(keyboard::Event::KeyPressed { key: keyboard::Key::Character(c), modifiers, .. })
            if modifiers.command() && (c.as_str() == "k" || c.as_str() == "f") && status == event::Status::Ignored =>
        {
            Some(Message::Find)
        }
        _ => None,
    }
}
