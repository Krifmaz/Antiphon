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
    Live(Arc<AppState>),
    Demo,
    /// Demo data, walked through each screen, saved as PNGs in this folder.
    Tour(PathBuf),
}

enum Source {
    Live { engine: Box<Engine>, model: Box<ui::Model> },
    Demo(Box<Demo>),
}

pub struct App {
    ui: Ui,
    source: Source,
    tour: Option<PathBuf>,
    system_dark: bool,
    look: (String, String, bool),
    epoch: Instant,
}

#[derive(Debug, Clone)]
pub enum Message {
    Ui(Msg),
    Tick(Instant),
    Frame(Instant),
    SystemMode(iced::theme::Mode),
    Escape,
    Find,
    Close,
    TourStep(usize),
    TourShoot(usize),
    TourShot(usize, window::Screenshot),
}

/// One screen of the tour: a name, a window size, and how to get there.
struct Step {
    name: &'static str,
    size: (f32, f32),
    setup: fn(&mut App),
}

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
    Step { name: "settings", size: (1280.0, 820.0), setup: |app| app.go(Route::Settings(Section::Appearance)) },
    Step { name: "voice", size: (1280.0, 820.0), setup: |app| app.go(Route::Settings(Section::Voice)) },
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
            app.go(Route::Home);
        },
    },
    Step { name: "phone-call", size: (400.0, 820.0), setup: |app| app.go(Route::Person(Demo::robin())) },
    Step {
        name: "light",
        size: (1280.0, 820.0),
        setup: |app| {
            app.demo().act(Action::Theme("light".into()));
            app.go(Route::Person(Demo::robin()));
        },
    },
];

impl App {
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

    fn model(&self) -> &ui::Model {
        match &self.source {
            Source::Live { model, .. } => model,
            Source::Demo(d) => &d.model,
        }
    }

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
        let actions = self.ui.update(msg, model);
        self.run(actions)
    }

    fn go(&mut self, route: Route) {
        let _ = self.send(Msg::Nav(route));
    }

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
            }
        }
        let open = self.ui.open_person().map(str::to_string);
        **model = engine.model(open.as_deref());
        if !engine.out.is_empty() {
            tasks.push(Task::done(Message::Tick(Instant::now())));
        }
        Task::batch(tasks)
    }

    fn refresh(&mut self) {
        let open = self.ui.open_person().map(str::to_string);
        if let Source::Live { engine, model } = &mut self.source {
            **model = engine.model(open.as_deref());
        }
        self.apply_look();
    }

    /// Repaints the palette when the theme, accent or system tone changed.
    fn apply_look(&mut self) {
        let s = &self.model().settings;
        let look = (s.theme.clone(), s.accent.clone(), self.system_dark);
        if look != self.look {
            theme::set(theme::palette(&look.0, &look.1, "", look.2));
            self.look = look;
        }
        let reduced = self.model().settings.reduced_motion;
        self.ui.set_reduced_motion(reduced);
    }

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

    pub fn view(&self) -> Element<'_, Message> {
        self.ui.view(self.model()).map(Message::Ui)
    }

    pub fn theme(&self) -> Theme {
        theme::iced_theme(&theme::look())
    }

    pub fn title(&self) -> String {
        let m = self.model();
        match &m.call.phase {
            ui::Phase::Connected => format!("{} · Stoop", m.call.peer_name),
            ui::Phase::Dialing => format!("Calling {} · Stoop", m.call.peer_name),
            _ => "Stoop".to_string(),
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let busy = match &self.source {
            Source::Live { engine, .. } => engine.busy(),
            Source::Demo(d) => d.model.call.active(),
        };
        let tick = iced::time::every(if busy { Duration::from_millis(50) } else { Duration::from_millis(400) }).map(Message::Tick);
        let mut subs = vec![tick, event::listen_with(on_event), window::close_requests().map(|_| Message::Close)];
        if self.ui.animating() {
            subs.push(window::frames().map(Message::Frame));
        }
        Subscription::batch(subs)
    }
}

/// Window and keyboard events the interface cares about.
fn on_event(event: Event, status: event::Status, _window: window::Id) -> Option<Message> {
    match event {
        Event::Window(window::Event::Resized(size)) => Some(Message::Ui(Msg::Resized(size))),
        Event::Window(window::Event::Opened { size, .. }) => Some(Message::Ui(Msg::Resized(size))),
        Event::Keyboard(keyboard::Event::KeyPressed { key: keyboard::Key::Named(key::Named::Escape), .. }) => Some(Message::Escape),
        Event::Keyboard(keyboard::Event::KeyPressed { key: keyboard::Key::Character(c), modifiers, .. })
            if modifiers.command() && (c.as_str() == "k" || c.as_str() == "f") && status == event::Status::Ignored =>
        {
            Some(Message::Find)
        }
        _ => None,
    }
}
