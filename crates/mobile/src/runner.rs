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
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    format: wgpu::TextureFormat,
    renderer: Renderer,
    viewport: Viewport,
    needs_configure: bool,
}

pub struct Runner {
    /// The app's private storage: identity, settings.
    data_dir: PathBuf,
    proxy: Arc<Mutex<EventLoopProxy<Wake>>>,
    app: Option<App>,
    graphics: Option<Graphics>,
    cache: user_interface::Cache,
    cursor: mouse::Cursor,
    events: Vec<Event>,
    fingers: usize,
    /// Status bar and gesture bar or keyboard, in physical pixels.
    insets: (f32, f32),
    keyboard_shown: bool,
    theme: Theme,
}

impl Runner {
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

    fn ensure_app(&mut self) {
        if self.app.is_some() {
            return;
        }
        let dir = self.data_dir.clone();
        let proxy = self.proxy.clone();
        let wake = Arc::new(move || {
            let _ = proxy.lock().unwrap().send_event(Wake);
        });
        match App::new(dir, wake) {
            Ok(app) => self.app = Some(app),
            Err(e) => log::error!("{e}"),
        }
    }

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
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: Wake) {
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
