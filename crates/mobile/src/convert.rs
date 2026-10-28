//! winit's Android events, translated into iced's.
//!
//! `iced_winit::conversion` does this on desktop but does not compile for
//! Android, so this is the part of it a phone needs: touch, the soft
//! keyboard, and the window itself. A touch also moves iced's cursor,
//! because iced's widgets decide what a tap hit from the cursor position.

use iced_core::keyboard::{self, key, Key, Location, Modifiers};
use iced_core::{mouse, touch, window, Event, Point, Size};
use winit::event::{ElementState, TouchPhase, WindowEvent};
use winit::keyboard::{Key as WKey, NamedKey};

/// Converts one winit event, updating `cursor` from touches. `scale` turns
/// physical pixels into iced's logical ones.
pub fn window_event(event: &WindowEvent, scale: f64, cursor: &mut mouse::Cursor) -> Option<Event> {
    match event {
        WindowEvent::Resized(size) => {
            let logical = size.to_logical::<f32>(scale);
            Some(Event::Window(window::Event::Resized(Size::new(logical.width, logical.height))))
        }
        WindowEvent::Focused(true) => Some(Event::Window(window::Event::Focused)),
        WindowEvent::Focused(false) => Some(Event::Window(window::Event::Unfocused)),
        WindowEvent::Touch(t) => {
            let logical = t.location.to_logical::<f32>(scale);
            let position = Point::new(logical.x, logical.y);
            *cursor = mouse::Cursor::Available(position);
            let id = touch::Finger(t.id);
            Some(Event::Touch(match t.phase {
                TouchPhase::Started => touch::Event::FingerPressed { id, position },
                TouchPhase::Moved => touch::Event::FingerMoved { id, position },
                TouchPhase::Ended => touch::Event::FingerLifted { id, position },
                TouchPhase::Cancelled => touch::Event::FingerLost { id, position },
            }))
        }
        WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
            let key = convert_key(&event.logical_key);
            let physical_key = key::Physical::Unidentified(key::NativeCode::Unidentified);
            Some(Event::Keyboard(match event.state {
                ElementState::Pressed => keyboard::Event::KeyPressed {
                    modified_key: key.clone(),
                    key,
                    physical_key,
                    location: Location::Standard,
                    modifiers: Modifiers::default(),
                    text: event.text.as_ref().map(|t| iced_core::SmolStr::new(t.as_str())),
                    repeat: event.repeat,
                },
                ElementState::Released => keyboard::Event::KeyReleased {
                    modified_key: key.clone(),
                    key,
                    physical_key,
                    location: Location::Standard,
                    modifiers: Modifiers::default(),
                },
            }))
        }
        _ => None,
    }
}

/// The keys a text field acts on. Anything else that types a character
/// arrives as that character; the rest is ignored, since nothing in this
/// app has a keyboard shortcut.
fn convert_key(key: &WKey) -> Key {
    match key {
        WKey::Character(c) => Key::Character(iced_core::SmolStr::new(c.as_str())),
        WKey::Named(named) => match named {
            NamedKey::Enter => Key::Named(key::Named::Enter),
            NamedKey::Backspace => Key::Named(key::Named::Backspace),
            NamedKey::Delete => Key::Named(key::Named::Delete),
            NamedKey::ArrowLeft => Key::Named(key::Named::ArrowLeft),
            NamedKey::ArrowRight => Key::Named(key::Named::ArrowRight),
            NamedKey::ArrowUp => Key::Named(key::Named::ArrowUp),
            NamedKey::ArrowDown => Key::Named(key::Named::ArrowDown),
            NamedKey::Home => Key::Named(key::Named::Home),
            NamedKey::End => Key::Named(key::Named::End),
            NamedKey::Tab => Key::Named(key::Named::Tab),
            NamedKey::Escape => Key::Named(key::Named::Escape),
            NamedKey::Space => Key::Named(key::Named::Space),
            _ => Key::Unidentified,
        },
        _ => Key::Unidentified,
    }
}
