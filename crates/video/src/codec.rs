//! The tile codec: [`Encoder`] turns frames into datagrams, [`Decoder`]
//! turns datagrams back into pictures.
//!
//! The picture is cut into bands [`BAND`] pixels tall and each band into
//! cells [`CELL`] pixels wide. For every frame the encoder finds the cells
//! that changed since what the receiver last got, and sends the run of
//! them in each band as one JPEG patch. On top of that it sends one whole
//! band each frame, round and round ("rolling refresh"): there are no
//! keyframe requests to lose and no keyframe bursts to clog the link, and
//! any patch lost on the way is replaced within one trip round the bands.
//!
//! The bytes sent are held to a budget with a token bucket, and the JPEG
//! quality follows how hard the budget is being pressed: lower while a lot
//! moves, back up when the picture settles.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::time::{Duration, Instant};

use jpeg_encoder::{ColorType, Encoder as Jpeg, SamplingFactor};
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::wire::{self, Header, Patch, Reassembler};
use crate::{scale, Frame, Stream};

/// Rows in a band: the unit of refresh and the tallest a patch gets.
pub const BAND: u32 = 64;

/// Columns in a cell: the unit of change detection across a band.
pub const CELL: u32 = 64;

/// How a stream is encoded: a camera and a screen want opposite things.
#[derive(Clone, Copy, Debug)]
struct Profile {
    /// The longest side a frame is sent at.
    max_side: u32,
    /// JPEG quality when nothing is pressing the budget.
    best: u8,
    /// The lowest the quality is let fall.
    worst: u8,
    /// Bytes per second the stream may use.
    budget: usize,
    /// Frames per second the caller is expected to offer.
    fps: u32,
    /// Full-resolution colour (4:4:4) instead of quarter (4:2:0). Coloured
    /// text smears at quarter colour; faces do not notice.
    full_colour: bool,
    /// Any changed pixel counts. A camera's sensor noise changes every
    /// pixel every frame, so a camera instead counts a cell as changed
    /// when its average difference passes a threshold.
    exact: bool,
}

impl Profile {
    /// 640 pixels, 15 frames a second, about 2 Mbit/s at most.
    const CAMERA: Profile = Profile { max_side: 640, best: 72, worst: 30, budget: 250_000, fps: 15, full_colour: false, exact: false };
    /// 1920 pixels, 8 frames a second, about 4 Mbit/s at most -- far less
    /// while the screen is still, which is most of the time.
    const SCREEN: Profile = Profile { max_side: 1920, best: 85, worst: 45, budget: 500_000, fps: 8, full_colour: true, exact: true };
}

/// Mean difference per sampled channel above which a camera cell counts as
/// changed. Sensor noise sits around 2 to 4.
const CAMERA_THRESHOLD: u32 = 7;

/// Turns frames from one source into datagrams for one stream.
pub struct Encoder {
    /// Which stream the datagrams are for.
    stream: Stream,
    /// How it is encoded.
    profile: Profile,
    /// The pixels the receiver last got for each place: what changes are
    /// measured against. `None` until the first frame.
    reference: Option<Frame>,
    /// The next frame number.
    frame: u32,
    /// The band refreshed whole by the next frame.
    refresh: u32,
    /// Send every band of the next frame.
    keyframe: bool,
    /// The JPEG quality now.
    quality: u8,
    /// The token bucket: bytes that may be sent before a frame is skipped.
    /// Negative while a big frame is being paid off.
    tokens: i64,
    /// Percent of the profile's budget allowed now: lowered by the call's
    /// automatic bandwidth (spec §2) when the link is struggling.
    budget_percent: u8,
}

impl Encoder {
    /// An encoder tuned for a camera.
    pub fn camera() -> Self {
        Self::new(Stream::Camera, Profile::CAMERA)
    }

    /// An encoder tuned for a shared screen.
    pub fn screen() -> Self {
        Self::new(Stream::Screen, Profile::SCREEN)
    }

    /// An encoder for `stream` with `profile`.
    fn new(stream: Stream, profile: Profile) -> Self {
        Self { stream, profile, reference: None, frame: 0, refresh: 0, keyframe: true, quality: profile.best, tokens: profile.budget as i64, budget_percent: 100 }
    }

    /// The stream this encoder sends.
    pub fn stream(&self) -> Stream {
        self.stream
    }

    /// Frames per second worth offering: more are mostly skipped, fewer
    /// look slow.
    pub fn fps(&self) -> u32 {
        self.profile.fps
    }

    /// Uses `percent` of the stream's usual byte budget (1 to 100), so a
    /// struggling link carries less video and more of it is left for the
    /// voice. Quality and frame skipping follow the smaller budget the
    /// same way they follow the full one.
    pub fn set_budget_percent(&mut self, percent: u8) {
        self.budget_percent = percent.clamp(1, 100);
    }

    /// Bytes per second allowed now.
    fn budget(&self) -> usize {
        self.profile.budget * self.budget_percent as usize / 100
    }

    /// The JPEG quality in use now (for a readout).
    pub fn quality(&self) -> u8 {
        self.quality
    }

    /// Sends every band with the next frame: for a receiver that has just
    /// joined, so it does not wait a trip round the refresh.
    pub fn request_keyframe(&mut self) {
        self.keyframe = true;
    }

    /// Encodes one frame into datagrams, ready to seal and send. Empty
    /// when nothing changed and no band was due, or when the budget asks
    /// for this frame to be skipped (what changed is then sent with the
    /// next one, since the reference is left as it was).
    pub fn encode(&mut self, frame: &Frame) -> Vec<Vec<u8>> {
        if !frame.is_valid() {
            return Vec::new();
        }
        // The bucket fills by a frame's share of the budget each frame,
        // and holds at most a second's worth.
        let p = self.profile;
        let budget = self.budget();
        self.tokens = (self.tokens + (budget / p.fps as usize) as i64).min(budget as i64);
        if self.tokens < 0 {
            return Vec::new();
        }

        let frame = scale::fit(frame, p.max_side);
        let (w, h) = (frame.width, frame.height);
        let resized = self.reference.as_ref().is_none_or(|r| (r.width, r.height) != (w, h));
        if resized {
            self.keyframe = true;
            self.reference = Some(Frame::solid(w, h, [0, 0, 0, 255]));
        }
        let bands = h.div_ceil(BAND);
        self.refresh %= bands;
        let keyframe = std::mem::take(&mut self.keyframe);

        // Which part of each band to send.
        let mut rects = Vec::new();
        for band in 0..bands {
            let y = band * BAND;
            let bh = BAND.min(h - y);
            let span = if keyframe || band == self.refresh { Some((0, w)) } else { self.changed_span(&frame, y, bh) };
            if let Some((x0, x1)) = span {
                rects.push((x0, y, x1 - x0, bh));
            }
        }
        self.refresh = (self.refresh + 1) % bands;

        // A patch too big for its datagrams is left out and the quality
        // drops hard; its place stays changed against the reference, so
        // it goes again with the next frame.
        // The frame may spend what is in the bucket and no more: bands
        // past that stay changed against the reference and go next time,
        // so a burst of movement is spread over frames, not sent at once.
        let mut oversized = false;
        let mut sent_rects = Vec::new();
        let mut patches = Vec::new();
        let mut spent = 0usize;
        for &(x, y, rw, rh) in &rects {
            match self.patch(&frame, x, y, rw, rh) {
                Some(bytes) if bytes.len() <= wire::MAX_PART * wire::MAX_PARTS => {
                    if spent + bytes.len() > self.tokens as usize && !patches.is_empty() {
                        break;
                    }
                    spent += bytes.len();
                    sent_rects.push((x, y, rw, rh));
                    patches.push(bytes);
                }
                Some(_) => oversized = true,
                None => {}
            }
        }
        if oversized {
            self.quality = self.quality.saturating_sub(10).max(p.worst);
        }
        if patches.is_empty() {
            return Vec::new();
        }

        // Remember what the receiver now has.
        let reference = self.reference.as_mut().expect("set above");
        for &(x, y, rw, rh) in &sent_rects {
            for row in y..y + rh {
                let at = ((row * w + x) * 4) as usize;
                let len = (rw * 4) as usize;
                reference.rgba[at..at + len].copy_from_slice(&frame.rgba[at..at + len]);
            }
        }

        let header = Header {
            stream: self.stream,
            frame: self.frame,
            patch: 0,
            patches: patches.len() as u8,
            part: 0,
            parts: 0,
            width: w as u16,
            height: h as u16,
        };
        self.frame = self.frame.wrapping_add(1);
        let datagrams: Vec<Vec<u8>> = patches
            .iter()
            .enumerate()
            .flat_map(|(i, bytes)| wire::split(Header { patch: i as u8, ..header }, bytes))
            .collect();

        let sent: usize = datagrams.iter().map(Vec::len).sum();
        self.tokens -= sent as i64;
        self.adapt(sent, keyframe || resized);
        datagrams
    }

    /// The datagram telling the receiver this stream has stopped.
    pub fn stop(&self) -> Vec<u8> {
        wire::stopped(self.stream, self.frame)
    }

    /// Moves the quality after a frame of `sent` bytes. A keyframe is
    /// expected to be big and moves nothing.
    fn adapt(&mut self, sent: usize, keyframe: bool) {
        if keyframe {
            return;
        }
        let p = self.profile;
        let share = self.budget() / p.fps as usize;
        if sent > share + share / 4 {
            self.quality = self.quality.saturating_sub(4).max(p.worst);
        } else if sent < share / 2 && self.tokens > 0 {
            self.quality = (self.quality + 2).min(p.best);
        }
    }

    /// The columns of the band starting at row `y` that changed since the
    /// reference, as a cell-aligned run `(x0, x1)`, or `None` if none did.
    fn changed_span(&self, frame: &Frame, y: u32, bh: u32) -> Option<(u32, u32)> {
        let reference = self.reference.as_ref()?;
        let w = frame.width;
        let cells = w.div_ceil(CELL);
        let changed = |cell: u32| {
            let x0 = cell * CELL;
            let cw = CELL.min(w - x0);
            if self.profile.exact {
                (y..y + bh).any(|row| {
                    let at = ((row * w + x0) * 4) as usize;
                    let len = (cw * 4) as usize;
                    frame.rgba[at..at + len] != reference.rgba[at..at + len]
                })
            } else {
                // Every fourth pixel of every fourth row: plenty to see a
                // face move, a sixteenth of the work.
                let (mut sum, mut n) = (0u32, 0u32);
                for row in (y..y + bh).step_by(4) {
                    for col in (x0..x0 + cw).step_by(4) {
                        let at = ((row * w + col) * 4) as usize;
                        for c in 0..3 {
                            sum += frame.rgba[at + c].abs_diff(reference.rgba[at + c]) as u32;
                        }
                        n += 3;
                    }
                }
                n > 0 && sum / n > CAMERA_THRESHOLD
            }
        };
        let first = (0..cells).find(|&c| changed(c))?;
        let last = (first..cells).rev().find(|&c| changed(c)).unwrap_or(first);
        Some((first * CELL, ((last + 1) * CELL).min(w)))
    }

    /// One rectangle of `frame` as patch bytes, or `None` if the JPEG
    /// encoder refuses it (it cannot, for the sizes made here).
    fn patch(&self, frame: &Frame, x: u32, y: u32, w: u32, h: u32) -> Option<Vec<u8>> {
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for row in y..y + h {
            let at = ((row * frame.width + x) * 4) as usize;
            for px in frame.rgba[at..at + (w * 4) as usize].chunks_exact(4) {
                rgb.extend_from_slice(&px[..3]);
            }
        }
        let mut jpeg = Vec::new();
        let mut enc = Jpeg::new(&mut jpeg, self.quality);
        enc.set_sampling_factor(if self.profile.full_colour { SamplingFactor::F_1_1 } else { SamplingFactor::F_2_2 });
        enc.encode(&rgb, w as u16, h as u16, ColorType::Rgb).ok()?;
        Some(Patch { x: x as u16, y: y as u16, w: w as u16, h: h as u16, jpeg }.to_bytes())
    }
}

/// A picture received from the other side, ready to draw.
#[derive(Clone, Debug)]
pub struct Picture {
    /// Which of their videos it is.
    pub stream: Stream,
    /// Pixels across.
    pub width: u32,
    /// Pixels down.
    pub height: u32,
    /// RGBA pixels, rows top to bottom.
    pub rgba: Vec<u8>,
    /// Counts up every time the picture changes, so a caller can skip
    /// redrawing one it already has.
    pub version: u64,
}

/// One stream's picture as it is being painted.
struct Canvas {
    /// The picture so far.
    frame: Frame,
    /// Bumped on every patch painted.
    version: u64,
    /// Whether anything has been painted yet: an all-blank canvas is not
    /// shown, so a video does not appear as a black box first.
    painted: bool,
    /// When a datagram for it last came in.
    heard: Instant,
}

/// Turns the other side's datagrams back into pictures, one per stream.
pub struct Decoder {
    /// Each stream's picture.
    canvases: BTreeMap<Stream, Canvas>,
    /// Patches still being put together.
    reassembler: Reassembler,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// How long a stream may go quiet before its picture is taken away:
    /// longer than any refresh trip, shorter than a confused stare.
    pub const QUIET: Duration = Duration::from_secs(3);

    /// A decoder showing nothing.
    pub fn new() -> Self {
        Self { canvases: BTreeMap::new(), reassembler: Reassembler::new() }
    }

    /// Takes one datagram. Returns true when a picture changed (painted,
    /// resized, or stopped).
    pub fn push(&mut self, datagram: &[u8]) -> bool {
        let Some((header, bytes)) = Header::read(datagram) else { return false };
        if header.patches == 0 {
            self.reassembler.clear(header.stream);
            return self.canvases.remove(&header.stream).is_some();
        }
        let (w, h) = (header.width as u32, header.height as u32);
        if w == 0 || h == 0 {
            return false;
        }
        let canvas = self.canvases.entry(header.stream).or_insert_with(|| Canvas {
            frame: Frame::solid(w, h, [17, 17, 20, 255]),
            version: 0,
            painted: false,
            heard: Instant::now(),
        });
        canvas.heard = Instant::now();
        let mut changed = (canvas.frame.width, canvas.frame.height) != (w, h);
        if changed {
            canvas.frame = Frame::solid(w, h, [17, 17, 20, 255]);
            canvas.painted = false;
            canvas.version += 1;
        }
        let Some(whole) = self.reassembler.push(header, bytes) else { return changed };
        let Some(patch) = Patch::from_bytes(&whole) else { return changed };
        if paint(&mut canvas.frame, &patch) {
            canvas.painted = true;
            canvas.version += 1;
            changed = true;
        }
        changed
    }

    /// Drops streams nothing has been heard on for [`Self::QUIET`].
    /// Returns true if one went.
    pub fn expire(&mut self) -> bool {
        let before = self.canvases.len();
        let gone: Vec<Stream> = self.canvases.iter().filter(|(_, c)| c.heard.elapsed() > Self::QUIET).map(|(s, _)| *s).collect();
        for s in gone {
            self.canvases.remove(&s);
            self.reassembler.clear(s);
        }
        self.canvases.len() != before
    }

    /// The streams showing a picture, with each one's version and size:
    /// cheap, for deciding whether [`Self::picture`] is worth calling.
    pub fn versions(&self) -> Vec<(Stream, u64)> {
        self.canvases.iter().filter(|(_, c)| c.painted).map(|(s, c)| (*s, c.version)).collect()
    }

    /// A copy of `stream`'s picture, if it is showing one.
    pub fn picture(&self, stream: Stream) -> Option<Picture> {
        let c = self.canvases.get(&stream).filter(|c| c.painted)?;
        Some(Picture { stream, width: c.frame.width, height: c.frame.height, rgba: c.frame.rgba.clone(), version: c.version })
    }

    /// Forgets every picture, when a call ends.
    pub fn clear(&mut self) {
        for s in self.canvases.keys().copied() {
            self.reassembler.clear(s);
        }
        self.canvases.clear();
    }
}

/// Decodes `patch` and paints it onto `canvas`. False when the JPEG does
/// not decode, or does not fit where it says it goes -- a damaged or
/// hostile patch is dropped, never painted past the canvas.
fn paint(canvas: &mut Frame, patch: &Patch) -> bool {
    let (x, y, w, h) = (patch.x as u32, patch.y as u32, patch.w as u32, patch.h as u32);
    if w == 0 || h == 0 || x + w > canvas.width || y + h > canvas.height {
        return false;
    }
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut dec = JpegDecoder::new_with_options(Cursor::new(&patch.jpeg[..]), options);
    let Ok(pixels) = dec.decode() else { return false };
    if dec.dimensions() != Some((w as usize, h as usize)) || pixels.len() != (w * h * 4) as usize {
        return false;
    }
    for row in 0..h {
        let dst = (((y + row) * canvas.width + x) * 4) as usize;
        let src = (row * w * 4) as usize;
        canvas.rgba[dst..dst + (w * 4) as usize].copy_from_slice(&pixels[src..src + (w * 4) as usize]);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test picture: a gradient with a square that can be moved.
    fn scene(w: u32, h: u32, square_x: u32) -> Frame {
        let mut f = Frame::solid(w, h, [0, 0, 0, 255]);
        for y in 0..h {
            for x in 0..w {
                let at = ((y * w + x) * 4) as usize;
                let inside = (square_x..square_x + 40).contains(&x) && (20..60).contains(&y);
                let v = if inside { [250, 250, 250] } else { [(x * 255 / w) as u8, (y * 255 / h) as u8, 90] };
                f.rgba[at..at + 3].copy_from_slice(&v);
            }
        }
        f
    }

    fn feed(dec: &mut Decoder, datagrams: &[Vec<u8>]) {
        for d in datagrams {
            dec.push(d);
        }
    }

    fn close(a: &[u8], b: &[u8]) -> bool {
        let diff: u64 = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u64).sum();
        diff / (a.len() as u64) < 6
    }

    #[test]
    fn a_screen_arrives_looking_like_itself() {
        let mut enc = Encoder::screen();
        let mut dec = Decoder::new();
        let f = scene(320, 200, 10);
        feed(&mut dec, &enc.encode(&f));
        let pic = dec.picture(Stream::Screen).expect("a picture");
        assert_eq!((pic.width, pic.height), (320, 200));
        assert!(close(&pic.rgba, &f.rgba));
    }

    #[test]
    fn a_still_screen_sends_only_the_refresh_band() {
        let mut enc = Encoder::screen();
        let f = scene(320, 256, 10);
        let first: usize = enc.encode(&f).iter().map(Vec::len).sum();
        let second = enc.encode(&f);
        let mut dec_headers: Vec<_> = second.iter().map(|d| Header::read(d).unwrap().0).collect();
        dec_headers.dedup_by_key(|h| h.patch);
        assert_eq!(dec_headers.len(), 1, "one band, the refresh");
        assert!(second.iter().map(Vec::len).sum::<usize>() < first / 2);
    }

    #[test]
    fn a_moved_square_is_sent_and_lands() {
        let mut enc = Encoder::screen();
        let mut dec = Decoder::new();
        feed(&mut dec, &enc.encode(&scene(320, 256, 10)));
        let moved = scene(320, 256, 200);
        feed(&mut dec, &enc.encode(&moved));
        let pic = dec.picture(Stream::Screen).unwrap();
        assert!(close(&pic.rgba, &moved.rgba));
    }

    #[test]
    fn a_lost_patch_heals_within_one_trip_round_the_bands() {
        let mut enc = Encoder::screen();
        let mut dec = Decoder::new();
        // The first frame is lost entirely.
        let f = scene(256, 256, 30);
        let _ = enc.encode(&f);
        for _ in 0..256 / BAND {
            feed(&mut dec, &enc.encode(&f));
        }
        assert!(close(&dec.picture(Stream::Screen).unwrap().rgba, &f.rgba));
    }

    #[test]
    fn a_camera_ignores_noise_but_not_movement() {
        let mut enc = Encoder::camera();
        let f = scene(320, 192, 10);
        let _ = enc.encode(&f);
        let mut noisy = f.clone();
        for (i, b) in noisy.rgba.iter_mut().enumerate() {
            if i % 4 != 3 {
                *b = b.saturating_add((i % 3) as u8);
            }
        }
        let still = enc.encode(&noisy);
        let patches = |d: &[Vec<u8>]| d.iter().map(|d| Header::read(d).unwrap().0.patches).max().unwrap_or(0);
        assert_eq!(patches(&still), 1, "only the refresh band");
        let moved = enc.encode(&scene(320, 192, 220));
        assert!(patches(&moved) >= 2);
    }

    #[test]
    fn big_sources_are_sent_smaller() {
        let mut enc = Encoder::camera();
        let mut dec = Decoder::new();
        feed(&mut dec, &enc.encode(&scene(1280, 720, 10)));
        let pic = dec.picture(Stream::Camera).unwrap();
        assert_eq!((pic.width, pic.height), (640, 360));
    }

    #[test]
    fn stopping_takes_the_picture_away() {
        let mut enc = Encoder::camera();
        let mut dec = Decoder::new();
        feed(&mut dec, &enc.encode(&scene(160, 120, 0)));
        assert_eq!(dec.versions().len(), 1);
        assert!(dec.push(&enc.stop()));
        assert!(dec.versions().is_empty());
    }

    #[test]
    fn a_patch_claiming_to_sit_outside_the_picture_is_dropped() {
        let mut canvas = Frame::solid(64, 64, [0, 0, 0, 255]);
        let enc = Encoder::screen();
        let mut bytes = enc.patch(&scene(64, 64, 0), 0, 0, 64, 64).unwrap();
        bytes[0..2].copy_from_slice(&32u16.to_be_bytes()); // x = 32: runs off the edge
        assert!(!paint(&mut canvas, &Patch::from_bytes(&bytes).unwrap()));
    }

    #[test]
    fn a_smaller_budget_sends_less() {
        // Noise, so every frame presses the budget.
        let mut seed = 7u32;
        let frames: Vec<Frame> = (0..Profile::CAMERA.fps * 3)
            .map(|_| {
                let mut f = Frame::solid(640, 480, [0, 0, 0, 255]);
                for b in &mut f.rgba {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    *b = (seed >> 24) as u8;
                }
                f
            })
            .collect();
        let total = |percent: u8| {
            let mut e = Encoder::camera();
            e.set_budget_percent(percent);
            frames.iter().map(|f| e.encode(f).iter().map(Vec::len).sum::<usize>()).sum::<usize>()
        };
        let (full, quarter) = (total(100), total(25));
        assert!(quarter * 2 < full, "{quarter} bytes at 25% vs {full} at 100%");
    }

    #[test]
    fn the_budget_skips_frames_rather_than_flooding() {
        let mut enc = Encoder::screen();
        let mut total = 0usize;
        // Noise is the worst case: nothing compresses, everything changes.
        let mut seed = 1u32;
        for _ in 0..(Profile::SCREEN.fps * 3) {
            let mut f = Frame::solid(1920, 1080, [0, 0, 0, 255]);
            for b in &mut f.rgba {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *b = (seed >> 24) as u8;
            }
            total += enc.encode(&f).iter().map(Vec::len).sum::<usize>();
        }
        // Three seconds, plus the bucket's one second of slack and one
        // frame that may overdraw it.
        assert!(total < Profile::SCREEN.budget * 5, "{total} bytes");
        assert_eq!(enc.quality(), Profile::SCREEN.worst);
    }
}
