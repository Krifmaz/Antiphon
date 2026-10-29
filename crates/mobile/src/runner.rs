//! The winit application: owns the window, the wgpu surface and the iced
//! `UserInterface`, and feeds [`App`] its events. Follows iced's own
//! `integration` example, plus what phones add -- the window comes and
//! goes as the app is backgrounded, the soft keyboard must be asked for,
//! and the system bars draw over the edges.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced_core::{clipboard, mouse, renderer, window as core_window, Event, Pixels, Size, Theme};
use iced_graphics::{Shell, Viewport};
use iced_runtime::user_interface::{self, UserInterface};
use iced_wgpu::{wgpu, Engine};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::convert;
use crate::platform;
use crate::ui::{App, Renderer, FONT};

/// Sent by the call engine when something it owns changed.
#[derive(Debug, Clone, Copy)]
pub struct Wake;

/// Redraw rate while a level meter is moving.
const ANIMATION_FRAME: Duration = Duration::from_millis(50);
/// How often to re-read the insets while the keyboard is up, since it
/// slides in over several frames and reports no event when it settles.
const KEYBOARD_POLL: Duration = Duration::from_millis(200);

/// Android: Vulkan first, OpenGL ES if it fails, since older phones ship
/// Vulkan drivers that enumerate fine and then cannot create a device.
#[cfg(target_os = "android")]
const GRAPHICS_BACKENDS: [wgpu::Backends; 2] = [wgpu::Backends::VULKAN, wgpu::Backends::GL];
/// iOS has Metal and nothing else worth trying.
#[cfg(target_os = "ios")]
const GRAPHICS_BACKENDS: [wgpu::Backends; 1] = [wgpu::Backends::METAL];

/// iced's clipboard, answered by the platform's, so a long press in a
/// text field can paste an invite link copied from another app.
struct Clipboard;

impl clipboard::Clipboard for Clipboard {
    fn read(&self, _kind: clipboard::Kind) -> Option<String> {
        platform::read_clipboard()
    }

    fn write(&mut self, _kind: clipboard::Kind, contents: String) {
        platform::copy_to_clipboard(&contents);
    }
}

/// Everything that only exists while the activity has a window.
struct Graphics {
    /// The window the surface draws into; shared with the surface, which
    /// must not outlive it.
    window: Arc<Window>,
    /// The wgpu surface; `'static` because it holds its own `Arc` of the
    /// window.
    surface: wgpu::Surface<'static>,
    /// The GPU device the renderer and surface were made with.
    device: wgpu::Device,
    /// The surface's texture format: an sRGB one when offered, so colours
    /// match iced's palette.
    format: wgpu::TextureFormat,
    /// iced's wgpu renderer, with the bundled font loaded.
    renderer: Renderer,
    /// The window's size in physical pixels and its scale factor, which
    /// together give iced its logical size.
    viewport: Viewport,
    /// Set when the window was resized or the surface was lost, so the next
    /// redraw reconfigures the surface before drawing.
    needs_configure: bool,
}

/// The winit application each platform shell runs: it creates the
/// [`App`] on first resume, builds the graphics whenever the system
/// hands over a window, and turns winit events into iced ones.
///
/// Everything here runs on the event-loop thread. Other threads reach it
/// only by sending a [`Wake`] through the proxy.
pub struct Runner {
    /// The app's private storage: identity, settings.
    data_dir: PathBuf,
    /// How the call engine and the platform wake the event loop. Behind a
    /// mutex because the wake closures must be `Sync` and a proxy is not on
    /// every platform.
    proxy: Arc<Mutex<EventLoopProxy<Wake>>>,
    /// The interface and everything it owns. Created on the first resume
    /// and kept for the life of the process, so a call survives the window
    /// going away while the app is in the background.
    app: Option<App>,
    /// Present only while the system has given the app a window.
    graphics: Option<Graphics>,
    /// iced's layout cache, carried between frames.
    cache: user_interface::Cache,
    /// Where the last touch was, in logical pixels: iced decides what a tap
    /// hit from the cursor position.
    cursor: mouse::Cursor,
    /// Converted events not yet run through the interface.
    events: Vec<Event>,
    /// How many fingers are down, so a gesture ends only when the last one
    /// lifts.
    fingers: usize,
    /// Status bar and gesture bar or keyboard, in physical pixels.
    insets: (f32, f32),
    /// Whether the soft keyboard has been asked for, so it is requested or
    /// dismissed only when a text field gains or loses focus.
    keyboard_shown: bool,
    /// The palette the interface is drawn with.
    theme: Theme,
}

impl Runner {
    /// Called by the platform shell with the app's private storage and a
    /// proxy for its event loop. Nothing else happens until the first
    /// resume, when the app and the window are created.
    pub fn new(data_dir: PathBuf, proxy: EventLoopProxy<Wake>) -> Self {
        Self {
            data_dir,
            proxy: Arc::new(Mutex::new(proxy)),
            app: None,
            graphics: None,
            cache: user_interface::Cache::new(),
            cursor: mouse::Cursor::Unavailable,
            events: Vec::new(),
            fingers: 0,
            insets: (0.0, 0.0),
            keyboard_shown: false,
            theme: Theme::Dark,
        }
    }

    /// Creates the [`App`] on first use and gives the platform its waker.
    /// A failure to start (an unreadable identity, say) is logged and leaves
    /// the app absent, so the window stays blank rather than crashing.
    fn ensure_app(&mut self) {
        if self.app.is_some() {
            return;
        }
        let dir = self.data_dir.clone();
        let platform_proxy = self.proxy.clone();
        platform::set_waker(move || {
            let _ = platform_proxy.lock().unwrap().send_event(Wake);
        });
        let proxy = self.proxy.clone();
        let wake = Arc::new(move || {
            let _ = proxy.lock().unwrap().send_event(Wake);
        });
        match App::new(dir, wake) {
            Ok(app) => self.app = Some(app),
            Err(e) => log::error!("{e}"),
        }
    }

    /// Creates the window and the wgpu surface, device and renderer for it,
    /// trying each backend in [`GRAPHICS_BACKENDS`] in turn. The error
    /// names the last backend's failure.
    fn create_graphics(&mut self, event_loop: &ActiveEventLoop) -> Result<Graphics, String> {
        let window = Arc::new(
            event_loop
                .create_window(Window::default_attributes().with_title("Antiphon"))
                .map_err(|e| format!("could not create the window: {e}"))?,
        );

        let mut last_error = String::from("no graphics backend");
        for backends in GRAPHICS_BACKENDS {
            let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends, ..Default::default() });
            let surface = match instance.create_surface(window.clone()) {
                Ok(surface) => surface,
                Err(e) => {
                    last_error = format!("{backends:?}: could not create a surface: {e}");
                    continue;
                }
            };
            let adapter = match futures::executor::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })) {
                Ok(adapter) => adapter,
                Err(e) => {
                    last_error = format!("{backends:?}: no adapter: {e}");
                    continue;
                }
            };
            let device = futures::executor::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("antiphon"),
                required_features: wgpu::Features::empty(),
                // Whatever this GPU offers. wgpu's defaults assume a
                // desktop and are refused by many phone GPUs.
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
            }));
            let (device, queue) = match device {
                Ok(pair) => pair,
                Err(e) => {
                    last_error = format!("{backends:?}: no device: {e}");
                    continue;
                }
            };
            log::info!("rendering with {:?}", adapter.get_info());

            let capabilities = surface.get_capabilities(&adapter);
            let format = capabilities
                .formats
                .iter()
                .copied()
                .find(wgpu::TextureFormat::is_srgb)
                .or_else(|| capabilities.formats.first().copied())
                .ok_or("the surface supports no formats")?;

            let engine = Engine::new(&adapter, device.clone(), queue.clone(), format, None, Shell::headless());
            let renderer = Renderer::new(engine, FONT, Pixels::from(17));
            let size = window.inner_size();
            let viewport = Viewport::with_physical_size(Size::new(size.width, size.height), window.scale_factor() as f32);
            return Ok(Graphics { window, surface, device, format, renderer, viewport, needs_configure: true });
        }
        Err(last_error)
    }

    /// The window's scale factor (physical pixels per logical pixel), or 1
    /// when there is no window.
    fn scale(&self) -> f64 {
        self.graphics.as_ref().map(|g| g.window.scale_factor()).unwrap_or(1.0)
    }

    /// Runs queued events through the interface and applies the messages
    /// they produce.
    fn process_events(&mut self) {
        let (Some(app), Some(graphics)) = (self.app.as_mut(), self.graphics.as_mut()) else {
            self.events.clear();
            return;
        };
        if self.events.is_empty() {
            return;
        }
        let scale = graphics.viewport.scale_factor();
        let (top, bottom) = (self.insets.0 / scale, self.insets.1 / scale);
        let mut interface = UserInterface::build(
            app.view(top, bottom),
            graphics.viewport.logical_size(),
            std::mem::take(&mut self.cache),
            &mut graphics.renderer,
        );
        let mut messages = Vec::new();
        let _ = interface.update(&self.events, self.cursor, &mut graphics.renderer, &mut Clipboard, &mut messages);
        self.events.clear();
        self.cache = interface.into_cache();
        for message in messages {
            app.update(message);
        }
        graphics.window.request_redraw();
    }

    /// Draws one frame. Also re-reads the system insets, and raises or
    /// dismisses the soft keyboard to match whether a text field has focus.
    fn redraw(&mut self) {
        let (Some(app), Some(graphics)) = (self.app.as_mut(), self.graphics.as_mut()) else {
            return;
        };
        self.insets = platform::system_insets();

        let size = graphics.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        if graphics.needs_configure {
            graphics.viewport =
                Viewport::with_physical_size(Size::new(size.width, size.height), graphics.window.scale_factor() as f32);
            graphics.surface.configure(
                &graphics.device,
                &wgpu::SurfaceConfiguration {
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    format: graphics.format,
                    width: size.width,
                    height: size.height,
                    present_mode: wgpu::PresentMode::AutoVsync,
                    alpha_mode: wgpu::CompositeAlphaMode::Auto,
                    view_formats: vec![],
                    desired_maximum_frame_latency: 2,
                },
            );
            graphics.needs_configure = false;
        }

        let frame = match graphics.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                graphics.needs_configure = true;
                graphics.window.request_redraw();
                return;
            }
            Err(e) => {
                log::warn!("skipping a frame: {e}");
                graphics.window.request_redraw();
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        let scale = graphics.viewport.scale_factor();
        let (top, bottom) = (self.insets.0 / scale, self.insets.1 / scale);
        let mut interface = UserInterface::build(
            app.view(top, bottom),
            graphics.viewport.logical_size(),
            std::mem::take(&mut self.cache),
            &mut graphics.renderer,
        );
        let (state, _) = interface.update(
            &[Event::Window(core_window::Event::RedrawRequested(Instant::now()))],
            self.cursor,
            &mut graphics.renderer,
            &mut Clipboard,
            &mut Vec::new(),
        );

        // A focused text field asks for an input method; on a phone that
        // means raising the soft keyboard, which winit does on request.
        if let user_interface::State::Updated { input_method, .. } = &state {
            let wants_keyboard = matches!(input_method, iced_core::InputMethod::Enabled { .. });
            if wants_keyboard != self.keyboard_shown {
                graphics.window.set_ime_allowed(wants_keyboard);
                self.keyboard_shown = wants_keyboard;
            }
        }

        let palette = self.theme.palette();
        interface.draw(&mut graphics.renderer, &self.theme, &renderer::Style { text_color: palette.text }, self.cursor);
        self.cache = interface.into_cache();
        graphics.renderer.present(Some(palette.background), frame.texture.format(), &view, &graphics.viewport);
        frame.present();
    }
}

impl ApplicationHandler<Wake> for Runner {
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            if let Some(graphics) = &self.graphics {
                graphics.window.request_redraw();
            }
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.ensure_app();
        if let Some(app) = self.app.as_mut() {
            app.resumed();
        }
        if self.graphics.is_none() {
            match self.create_graphics(event_loop) {
                Ok(graphics) => {
                    graphics.window.request_redraw();
                    self.graphics = Some(graphics);
                    // Widget state from a previous window is meaningless
                    // against a new renderer.
                    self.cache = user_interface::Cache::new();
                    self.keyboard_shown = false;
                }
                Err(e) => log::error!("{e}"),
            }
        }
    }

    /// The window is gone (app backgrounded). The surface must go with it;
    /// the app, and any call in progress, carry on.
    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        self.graphics = None;
        self.fingers = 0;
        if let Some(app) = self.app.as_mut() {
            app.touch_ended();
            app.suspended();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: Wake) {
        if let Some(app) = self.app.as_mut() {
            for event in platform::take_events() {
                app.platform_event(event);
            }
        }
        if let Some(graphics) = &self.graphics {
            graphics.window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match &event {
            WindowEvent::RedrawRequested => {
                self.redraw();
                return;
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(graphics) = self.graphics.as_mut() {
                    graphics.needs_configure = true;
                    graphics.window.request_redraw();
                }
            }
            WindowEvent::Touch(touch) => match touch.phase {
                TouchPhase::Started => self.fingers += 1,
                TouchPhase::Ended | TouchPhase::Cancelled => {
                    self.fingers = self.fingers.saturating_sub(1);
                    if self.fingers == 0 {
                        if let Some(app) = self.app.as_mut() {
                            app.touch_ended();
                        }
                    }
                }
                TouchPhase::Moved => {}
            },
            WindowEvent::CloseRequested => {
                event_loop.exit();
                return;
            }
            _ => {}
        }

        let scale = self.scale();
        if let Some(event) = convert::window_event(&event, scale, &mut self.cursor) {
            self.events.push(event);
        }
        self.process_events();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Every wake ends here, including the call engine's and a
        // transfer's while the window is gone, so this is where what the
        // call delivered is taken in -- in the background too, where a
        // message still has to reach the history and a notification.
        if let Some(app) = self.app.as_mut() {
            if app.poll() {
                if let Some(graphics) = &self.graphics {
                    graphics.window.request_redraw();
                }
            }
        }
        let animating = self.app.as_ref().is_some_and(App::is_animating);
        let flow = if self.graphics.is_none() {
            ControlFlow::Wait
        } else if animating {
            ControlFlow::WaitUntil(Instant::now() + ANIMATION_FRAME)
        } else if self.keyboard_shown {
            ControlFlow::WaitUntil(Instant::now() + KEYBOARD_POLL)
        } else {
            ControlFlow::Wait
        };
        event_loop.set_control_flow(flow);
    }
}
