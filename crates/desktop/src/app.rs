//! The iced application: the shared interface on top of the engine (or on
//! top of made-up data, for `--demo` and `--screenshot-tour`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use antiphon_client::state::AppState;
use antiphon_ui::{self as ui, theme, Action, Msg, Route, Section, Ui};
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
    /// When the app started, to animate the demo's meters.
    epoch: Instant,
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
    Step { name: "phone-settings", size: (1280.0, 820.0), setup: |app| app.go(Route::Settings(Section::Phone)) },
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
        let (source, tour) = match mode {
            Mode::Live(state) => {
                let mut engine = Box::new(Engine::new(state));
                let model = engine.model(None);
                (Source::Live { engine, model: Box::new(model) }, None)
            }
            Mode::Demo => (Source::Demo(Box::new(Demo::new())), None),
            Mode::Tour(dir) => (Source::Demo(Box::new(Demo::new())), Some(dir)),
        };
        let mut app = App { ui: Ui::new(), source, tour, system_dark: true, look: Default::default(), epoch: Instant::now() };
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
        Task::batch(tasks)
    }

    /// Rebuilds the model from the engine and repaints if the look changed.
    fn refresh(&mut self) {
        let open = self.ui.open_person().map(str::to_string);
        if let Source::Live { engine, model } = &mut self.source {
            engine.set_viewing(self.ui.open_channel());
            **model = engine.model(open.as_deref());
        }
        self.apply_look();
    }

    /// Repaints the palette and switches the reading font when the theme,
    /// accent, font or system tone changed.
    fn apply_look(&mut self) {
        let s = &self.model().settings;
        let look = (s.theme.clone(), s.accent.clone(), s.custom_accent.clone(), s.font.clone(), self.system_dark);
        if look != self.look {
            theme::set(theme::palette(&look.0, &look.1, &look.2, look.4));
            theme::set_font(&look.3);
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
                let task = match &mut self.source {
                    Source::Live { engine, .. } => {
                        engine.tick(open.as_deref());
                        self.drain()
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

    /// The iced theme built from the current palette.
    pub fn theme(&self) -> Theme {
        theme::iced_theme(&theme::look())
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
        Event::Keyboard(keyboard::Event::KeyPressed { key: keyboard::Key::Named(key::Named::Escape), .. }) => Some(Message::Escape),
        Event::Keyboard(keyboard::Event::KeyPressed { key: keyboard::Key::Character(c), modifiers, .. })
            if modifiers.command() && (c.as_str() == "k" || c.as_str() == "f") && status == event::Status::Ignored =>
        {
            Some(Message::Find)
        }
        _ => None,
    }
}
