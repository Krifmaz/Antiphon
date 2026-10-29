//! Camera and screen video for calls.
//!
//! Three pieces, each usable on its own:
//!
//! - [`Encoder`] and [`Decoder`]: a small tile codec. A picture is cut into
//!   bands; only the parts of a band that changed since the last frame are
//!   sent, each as a JPEG patch, and one band per frame is resent whole so
//!   a lost packet heals by itself within a second or two. That suits both
//!   kinds of video a call carries: a screen is mostly still and needs
//!   sharp text, and a face in front of a camera changes in one region.
//! - [`wire`]: how a patch is cut into datagrams small enough for any path
//!   and put back together. Each datagram stands alone: a lost one costs
//!   one patch until its band is refreshed, never the whole picture.
//! - `capture` (the `capture` feature, desktops only): the camera and the
//!   screen as a stream of [`Frame`]s.
//!
//! Everything here is plain Rust, so the same codec builds for every
//! platform the app ships on and a desktop and a phone always understand
//! each other. Encryption is not this crate's job: each datagram goes out
//! sealed by the call's session, like audio.

mod codec;
mod scale;
pub mod wire;

#[cfg(feature = "capture")]
pub mod capture;

pub use codec::{Decoder, Encoder, Picture};
pub use scale::fit;

/// Which of a person's videos a packet belongs to. Each side can send one
/// of each at once: their face and their screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stream {
    /// A camera: small, fast, forgiving about detail.
    Camera,
    /// A shared screen: larger, slower, sharp.
    Screen,
}

impl Stream {
    /// The stream's byte on the wire.
    pub(crate) fn to_byte(self) -> u8 {
        match self {
            Stream::Camera => 0,
            Stream::Screen => 1,
        }
    }

    /// The stream a wire byte names, or `None` for one this version does
    /// not know (a newer peer's new kind of video is ignored, not misread).
    pub(crate) fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Stream::Camera),
            1 => Some(Stream::Screen),
            _ => None,
        }
    }
}

/// One uncompressed picture: RGBA, rows top to bottom, no padding between
/// rows, so `rgba.len() == width * height * 4`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Pixels across.
    pub width: u32,
    /// Pixels down.
    pub height: u32,
    /// The pixels, four bytes each.
    pub rgba: Vec<u8>,
}

impl Frame {
    /// A frame of one colour, for tests and placeholders.
    pub fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Self {
        Self { width, height, rgba: rgba.repeat((width * height) as usize) }
    }

    /// Whether the buffer really holds `width * height` pixels. A frame
    /// that does not is refused rather than read past its end.
    pub fn is_valid(&self) -> bool {
        self.width > 0 && self.height > 0 && self.rgba.len() == self.width as usize * self.height as usize * 4
    }
}
