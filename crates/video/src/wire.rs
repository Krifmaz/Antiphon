//! Video datagrams: how one encoded patch is cut into packets and put back.
//!
//! Every datagram carries the same small header, so any one of them tells
//! the receiver which stream, which frame, the picture's size, and where it
//! sits among its patch's parts:
//!
//! ```text
//! [stream u8][frame u32][patch u8][patches u8][part u8][parts u8][width u16][height u16][bytes...]
//! ```
//!
//! All big-endian. A patch, once its parts are together, is
//! `[x u16][y u16][w u16][h u16][jpeg...]`: a rectangle of the picture and
//! the JPEG that fills it. A datagram with `patches == 0` carries no patch
//! and says the stream has stopped, so the picture goes away at once
//! instead of freezing until it times out.

use std::collections::HashMap;

use crate::Stream;

/// Bytes of header in front of every datagram's share of a patch.
pub const HEADER_LEN: usize = 1 + 4 + 1 + 1 + 1 + 1 + 2 + 2;

/// Most patch bytes in one datagram. With the header, the session's
/// ratchet header and tag, and the frame header, a datagram stays well
/// under 1,280 bytes -- the smallest MTU IPv6 promises -- so video is
/// never fragmented by the network, where one lost fragment loses all.
pub const MAX_PART: usize = 1100;

/// The most parts one patch can be cut into (the part count is a byte).
/// About 280 KB: far above any band at the qualities the encoder uses.
pub const MAX_PARTS: usize = 255;

/// The fixed fields every datagram starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Which video this belongs to.
    pub stream: Stream,
    /// The sender's frame number, counting up; wraps after four billion.
    pub frame: u32,
    /// This patch's index within its frame.
    pub patch: u8,
    /// Patches in the frame; `0` means the stream stopped.
    pub patches: u8,
    /// This datagram's index within its patch.
    pub part: u8,
    /// Datagrams the patch was cut into.
    pub parts: u8,
    /// The whole picture's width, in pixels.
    pub width: u16,
    /// The whole picture's height, in pixels.
    pub height: u16,
}

impl Header {
    /// Appends the header's bytes to `out`.
    fn write(&self, out: &mut Vec<u8>) {
        out.push(self.stream.to_byte());
        out.extend_from_slice(&self.frame.to_be_bytes());
        out.extend_from_slice(&[self.patch, self.patches, self.part, self.parts]);
        out.extend_from_slice(&self.width.to_be_bytes());
        out.extend_from_slice(&self.height.to_be_bytes());
    }

    /// Splits a datagram into its header and its share of the patch, or
    /// `None` when it is too short, names an unknown stream, or claims a
    /// part past its own count.
    pub fn read(bytes: &[u8]) -> Option<(Header, &[u8])> {
        if bytes.len() < HEADER_LEN {
            return None;
        }
        let header = Header {
            stream: Stream::from_byte(bytes[0])?,
            frame: u32::from_be_bytes(bytes[1..5].try_into().ok()?),
            patch: bytes[5],
            patches: bytes[6],
            part: bytes[7],
            parts: bytes[8],
            width: u16::from_be_bytes(bytes[9..11].try_into().ok()?),
            height: u16::from_be_bytes(bytes[11..13].try_into().ok()?),
        };
        let stopped = header.patches == 0;
        if !stopped && (header.parts == 0 || header.part >= header.parts || header.patch >= header.patches) {
            return None;
        }
        Some((header, &bytes[HEADER_LEN..]))
    }
}

/// A rectangle of the picture and the JPEG that fills it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch {
    /// Left edge, in pixels.
    pub x: u16,
    /// Top edge, in pixels.
    pub y: u16,
    /// Width, in pixels.
    pub w: u16,
    /// Height, in pixels.
    pub h: u16,
    /// The JPEG for the rectangle.
    pub jpeg: Vec<u8>,
}

impl Patch {
    /// The patch as bytes, before it is cut into parts.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.jpeg.len());
        for v in [self.x, self.y, self.w, self.h] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&self.jpeg);
        out
    }

    /// Reads a reassembled patch, or `None` if it is too short to hold
    /// even the rectangle.
    pub fn from_bytes(bytes: &[u8]) -> Option<Patch> {
        if bytes.len() < 8 {
            return None;
        }
        let at = |i: usize| u16::from_be_bytes([bytes[i], bytes[i + 1]]);
        Some(Patch { x: at(0), y: at(2), w: at(4), h: at(6), jpeg: bytes[8..].to_vec() })
    }
}

/// Cuts one patch into datagrams. `header`'s `part` and `parts` are filled
/// in here. A patch too big for [`MAX_PARTS`] datagrams yields nothing --
/// the encoder never makes one, and the band's next refresh covers it.
pub fn split(mut header: Header, patch: &[u8]) -> Vec<Vec<u8>> {
    let parts = patch.len().div_ceil(MAX_PART).max(1);
    if parts > MAX_PARTS {
        return Vec::new();
    }
    header.parts = parts as u8;
    (0..parts)
        .map(|i| {
            header.part = i as u8;
            let piece = &patch[i * MAX_PART..((i + 1) * MAX_PART).min(patch.len())];
            let mut out = Vec::with_capacity(HEADER_LEN + piece.len());
            header.write(&mut out);
            out.extend_from_slice(piece);
            out
        })
        .collect()
}

/// The datagram that says a stream stopped.
pub fn stopped(stream: Stream, frame: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    Header { stream, frame, patch: 0, patches: 0, part: 0, parts: 0, width: 0, height: 0 }.write(&mut out);
    out
}

/// One patch's parts as they arrive.
struct Pending {
    /// Each part, once it has come in.
    parts: Vec<Option<Vec<u8>>>,
    /// How many of `parts` are still missing.
    missing: usize,
}

/// Puts patches back together from datagrams arriving in any order.
///
/// A patch whose parts do not all arrive is dropped once its frame is
/// [`Self::KEEP_FRAMES`] behind the newest seen: by then a newer picture
/// of the same place has come, or will with the band's refresh.
#[derive(Default)]
pub struct Reassembler {
    /// Patches still missing parts, by stream, frame and patch index.
    pending: HashMap<(Stream, u32, u8), Pending>,
    /// The newest frame seen on each stream.
    newest: HashMap<Stream, u32>,
}

impl Reassembler {
    /// Frames behind the newest that a patch may still complete in.
    pub const KEEP_FRAMES: u32 = 8;

    /// A reassembler with nothing pending.
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes one datagram's share of a patch. Returns the whole patch's
    /// bytes when this was its last missing part.
    pub fn push(&mut self, header: Header, bytes: &[u8]) -> Option<Vec<u8>> {
        let newest = self.newest.entry(header.stream).or_insert(header.frame);
        // Wrapping comparison: a frame "after" the newest is up to half
        // the counter's range ahead, so a wrap past u32::MAX still counts
        // as moving forward.
        if header.frame.wrapping_sub(*newest) < u32::MAX / 2 {
            *newest = header.frame;
        } else if newest.wrapping_sub(header.frame) > Self::KEEP_FRAMES {
            return None; // too late to matter
        }
        let newest = *newest;
        self.pending.retain(|(s, f, _), _| *s != header.stream || newest.wrapping_sub(*f) <= Self::KEEP_FRAMES);

        if header.parts == 1 {
            return Some(bytes.to_vec());
        }
        let key = (header.stream, header.frame, header.patch);
        let entry = self.pending.entry(key).or_insert_with(|| Pending { parts: vec![None; header.parts as usize], missing: header.parts as usize });
        // A datagram that disagrees with its siblings about the part
        // count is not part of this patch.
        let slot = entry.parts.get_mut(header.part as usize)?;
        if slot.is_none() {
            *slot = Some(bytes.to_vec());
            entry.missing -= 1;
        }
        if entry.missing > 0 {
            return None;
        }
        let done = self.pending.remove(&key)?;
        Some(done.parts.into_iter().flatten().flatten().collect())
    }

    /// Forgets everything pending on `stream`, when it stops.
    pub fn clear(&mut self, stream: Stream) {
        self.pending.retain(|(s, _, _), _| *s != stream);
        self.newest.remove(&stream);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(frame: u32) -> Header {
        Header { stream: Stream::Screen, frame, patch: 0, patches: 1, part: 0, parts: 0, width: 640, height: 360 }
    }

    #[test]
    fn a_patch_survives_the_round_trip_in_any_order() {
        let patch: Vec<u8> = (0..5000u32).map(|i| (i * 7) as u8).collect();
        let mut datagrams = split(header(3), &patch);
        assert_eq!(datagrams.len(), 5);
        assert!(datagrams.iter().all(|d| d.len() <= HEADER_LEN + MAX_PART));
        datagrams.reverse();
        let mut r = Reassembler::new();
        let mut out = None;
        for d in &datagrams {
            let (h, bytes) = Header::read(d).unwrap();
            assert_eq!((h.width, h.height), (640, 360));
            out = r.push(h, bytes).or(out);
        }
        assert_eq!(out.unwrap(), patch);
    }

    #[test]
    fn a_lost_part_loses_only_its_patch_and_is_forgotten() {
        let mut r = Reassembler::new();
        let first = split(header(1), &[1; 3000]);
        let (h, b) = Header::read(&first[0]).unwrap();
        assert!(r.push(h, b).is_none());
        // Many frames later the stale half-patch is gone.
        let later = split(header(1 + Reassembler::KEEP_FRAMES + 1), &[2; 10]);
        let (h, b) = Header::read(&later[0]).unwrap();
        assert_eq!(r.push(h, b).unwrap(), vec![2; 10]);
        assert!(r.pending.is_empty());
    }

    #[test]
    fn nonsense_is_refused() {
        assert!(Header::read(&[0; 3]).is_none());
        let mut bad = split(header(1), &[0; 10]).remove(0);
        bad[7] = 5; // part 5 of 1
        assert!(Header::read(&bad).is_none());
        bad[0] = 9; // unknown stream
        assert!(Header::read(&bad).is_none());
    }

    #[test]
    fn the_stop_datagram_reads_back() {
        let datagram = stopped(Stream::Camera, 9);
        let (h, rest) = Header::read(&datagram).unwrap();
        assert_eq!((h.stream, h.patches), (Stream::Camera, 0));
        assert!(rest.is_empty());
    }

    #[test]
    fn patches_round_trip() {
        let p = Patch { x: 64, y: 128, w: 320, h: 64, jpeg: vec![1, 2, 3] };
        assert_eq!(Patch::from_bytes(&p.to_bytes()).unwrap(), p);
    }
}
