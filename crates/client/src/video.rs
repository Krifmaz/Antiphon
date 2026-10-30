//! A call's video: the pictures we send (camera, screen) and the ones we
//! receive.
//!
//! Sending runs one thread per stream. Each takes the newest frame from
//! its [`FrameSource`] at the encoder's pace, encodes it, and seals and
//! sends the datagrams on the call's connection -- the same session as
//! the voice, so video is end-to-end encrypted exactly as audio is.
//! Receiving is a [`Decoder`] fed from the call's receive loop.
//!
//! Shared by the desktop and the phone apps; where frames come from is
//! the app's business (`crate::video_capture` on desktops).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use antiphon_net::PeerConnection;
use antiphon_video::{Decoder, Encoder, Frame, Picture, Stream};

/// Somewhere frames come from: a camera, a screen, a phone's camera feed.
pub trait FrameSource: Send {
    /// The newest frame and its number, if one has arrived. The number
    /// tells a new frame from one already sent.
    fn latest(&mut self) -> Numbered;

    /// Why the source stopped by itself, if it did (unplugged, or the
    /// system's own "stop sharing" pressed).
    fn failed(&self) -> Option<String> {
        None
    }
}

/// A frame and its number, once there is one.
pub type Numbered = Option<(u64, Arc<Frame>)>;

/// The longest side of a local preview. Small: it is drawn as a tile, and
/// a full-size screen copied to the GPU every frame would be waste.
const PREVIEW_SIDE: u32 = 480;

/// One stream being sent.
struct Sender {
    /// Asks the thread to stop.
    stop: Arc<AtomicBool>,
    /// The thread, joined when the stream stops.
    thread: Option<JoinHandle<()>>,
    /// What we are sending, small and (for the camera) mirrored: the
    /// "this is what they see" tile. Numbered, so a redraw can tell a
    /// new one.
    preview: Arc<Mutex<Numbered>>,
    /// Why sending stopped by itself, once it has.
    failed: Arc<Mutex<Option<String>>>,
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Everything video on one call.
pub struct CallVideo {
    /// The other side's pictures.
    remote: Mutex<Decoder>,
    /// Our streams being sent.
    senders: Mutex<HashMap<Stream, Sender>>,
    /// Percent of each stream's byte budget to use, from the call's
    /// automatic bandwidth (spec §2); 0 pauses sending. Shared with every
    /// sender thread.
    share: Arc<AtomicU8>,
    /// Everyone else in a mesh group call, who gets the same datagrams as
    /// the primary connection. Shared with every sender thread.
    extra: Arc<Mutex<Vec<Arc<PeerConnection>>>>,
    /// Their pictures, by group leg id.
    legs: Mutex<HashMap<u64, Decoder>>,
    /// Once a group call moves to a group relay's room, datagrams go here
    /// instead (sealed and sent once, for everyone, by the room) and no
    /// longer to the primary connection or the legs.
    room: Arc<Mutex<Option<Room>>>,
    /// Asks every sender for a whole picture with its next frame.
    keyframe: Arc<AtomicBool>,
}

/// Where a group relay's room takes our video datagrams.
pub type Room = tokio::sync::mpsc::UnboundedSender<Vec<u8>>;

impl Default for CallVideo {
    fn default() -> Self {
        Self {
            remote: Mutex::default(),
            senders: Mutex::default(),
            share: Arc::new(AtomicU8::new(100)),
            extra: Arc::default(),
            legs: Mutex::default(),
            room: Arc::default(),
            keyframe: Arc::default(),
        }
    }
}

impl CallVideo {
    /// Video for a call with nothing on yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets how much of its usual budget video may use, in percent: video
    /// gives way first when the link struggles, and at 0 it pauses (the
    /// other side's tile goes away) until the link recovers.
    pub fn set_share(&self, percent: u8) {
        self.share.store(percent.min(100), Ordering::Relaxed);
    }

    /// Sets who else in a group call our video goes to, besides the
    /// primary connection (spec §3 group calls). Called as people come
    /// and go; a newcomer gets a whole picture straight away.
    pub fn set_group_peers(&self, peers: Vec<Arc<PeerConnection>>) {
        *self.extra.lock().unwrap() = peers;
    }

    /// Sends our video into a group relay's room from now on (spec §3
    /// past the mesh), or stops doing so with `None`.
    pub fn set_room(&self, room: Option<Room>) {
        *self.room.lock().unwrap() = room;
        self.request_keyframe();
    }

    /// Has every stream send a whole picture with its next frame, for
    /// someone who has just arrived.
    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    /// Takes one video datagram from group leg `leg`.
    pub fn receive_from(&self, leg: u64, datagram: &[u8]) {
        self.legs.lock().unwrap().entry(leg).or_default().push(datagram);
    }

    /// The group's pictures now showing, as `(leg, stream, version)`.
    /// Pictures that went quiet are dropped, and so are people with none.
    pub fn leg_versions(&self) -> Vec<(u64, Stream, u64)> {
        let mut legs = self.legs.lock().unwrap();
        let mut out = Vec::new();
        legs.retain(|id, d| {
            d.expire();
            let v = d.versions();
            out.extend(v.iter().map(|(s, n)| (*id, *s, *n)));
            !v.is_empty()
        });
        out.sort_by_key(|(id, s, _)| (*id, *s == Stream::Screen));
        out
    }

    /// A copy of one of group leg `leg`'s pictures.
    pub fn leg_picture(&self, leg: u64, stream: Stream) -> Option<Picture> {
        self.legs.lock().unwrap().get(&leg)?.picture(stream)
    }

    /// Takes one video datagram from the receive loop.
    pub fn receive(&self, datagram: &[u8]) {
        self.remote.lock().unwrap().push(datagram);
    }

    /// The other side's pictures now showing, with each one's version:
    /// cheap enough to ask every frame. Pictures that went quiet are
    /// dropped here.
    pub fn remote_versions(&self) -> Vec<(Stream, u64)> {
        let mut d = self.remote.lock().unwrap();
        d.expire();
        d.versions()
    }

    /// A copy of one of the other side's pictures.
    pub fn remote_picture(&self, stream: Stream) -> Option<Picture> {
        self.remote.lock().unwrap().picture(stream)
    }

    /// Whether we are sending `stream`.
    pub fn sending(&self, stream: Stream) -> bool {
        self.senders.lock().unwrap().contains_key(&stream)
    }

    /// Our preview of `stream`, if we are sending it.
    pub fn preview(&self, stream: Stream) -> Numbered {
        self.senders.lock().unwrap().get(&stream)?.preview.lock().unwrap().clone()
    }

    /// Streams that stopped by themselves since last asked, and why. The
    /// stream is taken off the list, so the button shows it off.
    pub fn take_failures(&self) -> Vec<(Stream, String)> {
        let mut senders = self.senders.lock().unwrap();
        let failed: Vec<(Stream, String)> = senders
            .iter()
            .filter_map(|(s, sender)| sender.failed.lock().unwrap().clone().map(|why| (*s, why)))
            .collect();
        for (s, _) in &failed {
            senders.remove(s);
        }
        failed
    }

    /// Starts sending `stream` from `source` to `peer`, replacing whatever
    /// was sending it before.
    pub fn start(&self, stream: Stream, mut source: Box<dyn FrameSource>, peer: Arc<PeerConnection>, runtime: tokio::runtime::Handle) {
        let stop = Arc::new(AtomicBool::new(false));
        let preview = Arc::new(Mutex::new(None));
        let failed = Arc::new(Mutex::new(None));
        let (t_stop, t_preview, t_failed) = (stop.clone(), preview.clone(), failed.clone());
        let t_share = self.share.clone();
        let t_extra = self.extra.clone();
        let (t_room, t_keyframe) = (self.room.clone(), self.keyframe.clone());
        let thread = std::thread::Builder::new()
            .name(format!("{stream:?} sender"))
            .spawn(move || {
                let mut encoder = match stream {
                    Stream::Camera => Encoder::camera(),
                    Stream::Screen => Encoder::screen(),
                };
                let period = Duration::from_secs(1) / encoder.fps();
                let mut last = None;
                let mut paused = false;
                // How many group legs got the last frame: one more means a
                // newcomer, who needs a whole picture rather than patches.
                let mut sent_to = 0;
                while !t_stop.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    if let Some(why) = source.failed() {
                        *t_failed.lock().unwrap() = Some(why);
                        break;
                    }
                    // Spec §3 group calls: the same datagrams to everyone
                    // in the mesh, or once into the group relay's room.
                    let room = t_room.lock().unwrap().clone();
                    let extra: Vec<Arc<PeerConnection>> =
                        if room.is_some() { Vec::new() } else { t_extra.lock().unwrap().clone() };
                    if extra.len() > sent_to || t_keyframe.swap(false, Ordering::Relaxed) {
                        encoder.request_keyframe();
                    }
                    sent_to = extra.len();
                    let send_all = |datagrams: &[Vec<u8>]| match &room {
                        Some(room) => {
                            for d in datagrams {
                                let _ = room.send(d.clone());
                            }
                        }
                        None => runtime.block_on(async {
                            for to in std::iter::once(&peer).chain(&extra) {
                                for d in datagrams {
                                    let _ = to.send_video(d).await;
                                }
                            }
                        }),
                    };
                    // Spec §2 automatic bandwidth: pausing tells the other
                    // side once, as stopping does; resuming starts with a
                    // whole picture, since theirs was cleared.
                    let share = t_share.load(Ordering::Relaxed);
                    if share == 0 {
                        if !paused {
                            paused = true;
                            let stop = encoder.stop();
                            send_all(&[stop.clone(), stop.clone(), stop]);
                        }
                    } else if paused {
                        paused = false;
                        encoder.request_keyframe();
                    }
                    // Every person in a mesh is another upload of the same
                    // datagrams, so the budget is split between them.
                    encoder.set_budget_percent((share as usize / (1 + extra.len())).max(1) as u8);
                    if let Some((n, frame)) = source.latest().filter(|(n, _)| Some(*n) != last) {
                        last = Some(n);
                        *t_preview.lock().unwrap() = Some((n, Arc::new(preview_of(&frame, stream))));
                        let datagrams = if paused { Vec::new() } else { encoder.encode(&frame) };
                        if !datagrams.is_empty() {
                            send_all(&datagrams);
                        }
                    }
                    std::thread::sleep(period.saturating_sub(started.elapsed()));
                }
                // Said a few times: it is one datagram, and a lost one
                // would leave our last picture frozen on their side until
                // it times out.
                let stop = encoder.stop();
                match t_room.lock().unwrap().clone() {
                    Some(room) => {
                        for _ in 0..3 {
                            let _ = room.send(stop.clone());
                        }
                    }
                    None => {
                        let extra: Vec<Arc<PeerConnection>> = t_extra.lock().unwrap().clone();
                        runtime.block_on(async {
                            for to in std::iter::once(&peer).chain(&extra) {
                                for _ in 0..3 {
                                    let _ = to.send_video(&stop).await;
                                }
                            }
                        });
                    }
                }
            });
        let Ok(thread) = thread else { return };
        let sender = Sender { stop, thread: Some(thread), preview, failed };
        // The old sender (if any) is dropped outside the lock: dropping
        // joins its thread, which can take a frame's time.
        let old = self.senders.lock().unwrap().insert(stream, sender);
        drop(old);
    }

    /// Stops sending `stream`. Joins its thread, so the camera is let go
    /// by the time this returns.
    pub fn stop(&self, stream: Stream) {
        let old = self.senders.lock().unwrap().remove(&stream);
        drop(old);
    }

    /// Stops everything, when the call ends.
    pub fn stop_all(&self) {
        let old: Vec<Sender> = self.senders.lock().unwrap().drain().map(|(_, s)| s).collect();
        drop(old);
        self.remote.lock().unwrap().clear();
        self.legs.lock().unwrap().clear();
    }
}

/// The small copy of a frame shown as our own tile: shrunk, and for the
/// camera flipped left to right, since people expect to see themselves
/// as in a mirror. (The other side sees it the right way round.)
fn preview_of(frame: &Frame, stream: Stream) -> Frame {
    let mut small = antiphon_video::fit(frame, PREVIEW_SIDE);
    if stream == Stream::Camera {
        let w = small.width as usize * 4;
        for row in small.rgba.chunks_exact_mut(w) {
            for x in 0..row.len() / 8 {
                let (a, b) = (x * 4, w - 4 - x * 4);
                for c in 0..4 {
                    row.swap(a + c, b + c);
                }
            }
        }
    }
    small
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_camera_preview_is_mirrored_and_small() {
        let mut f = Frame::solid(960, 2, [0, 0, 0, 255]);
        // A red dot top left, two by two so it survives the halving.
        for at in [0, 4, 960 * 4, 960 * 4 + 4] {
            f.rgba[at] = 255;
        }
        let p = preview_of(&f, Stream::Camera);
        assert_eq!((p.width, p.height), (480, 1));
        let last = (p.width as usize - 1) * 4;
        assert!(p.rgba[last] > 100, "the dot moved to the right edge");
        let s = preview_of(&f, Stream::Screen);
        assert!(s.rgba[0] > 100, "a screen is not mirrored");
    }

    /// A source that always offers the same frame, numbered once.
    struct Still(Arc<Frame>);

    impl FrameSource for Still {
        fn latest(&mut self) -> Numbered {
            Some((0, self.0.clone()))
        }
    }

    /// The whole path on loopback: a shared screen goes out through the
    /// encoder and a real encrypted connection, and comes out the other
    /// side's decoder as the same picture; stopping takes it away there.
    #[test]
    fn a_shared_screen_crosses_a_real_call() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (caller, callee) = runtime.block_on(async {
            let socket = |s: tokio::net::UdpSocket| Arc::new(antiphon_net::AppSocket::from(s));
            let listener = socket(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let addr = listener.local_addr().unwrap();
            let (alice, bob) = (antiphon_crypto::IdentityKeyPair::generate(), antiphon_crypto::IdentityKeyPair::generate());
            let listen = tokio::spawn(async move { PeerConnection::listen(listener, &bob).await.unwrap() });
            let dialer = socket(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let caller = PeerConnection::connect(dialer, addr, &alice).await.unwrap();
            (Arc::new(caller), Arc::new(listen.await.unwrap()))
        });

        let theirs = Arc::new(CallVideo::new());
        let on_receive = theirs.clone();
        runtime.spawn(callee.recv_loop(move |m| {
            if let antiphon_net::Received::Video(bytes) = m {
                on_receive.receive(&bytes);
            }
        }));

        let mut frame = Frame::solid(800, 450, [40, 120, 200, 255]);
        frame.rgba[..800 * 4 * 10].iter_mut().skip(1).step_by(4).for_each(|g| *g = 250);
        let mine = CallVideo::new();
        mine.start(Stream::Screen, Box::new(Still(Arc::new(frame))), caller, runtime.handle().clone());
        assert!(mine.sending(Stream::Screen));

        // Bands land one patch at a time; wait for the one holding row 200.
        let at = (200 * 800 + 400) * 4;
        let deadline = Instant::now() + Duration::from_secs(10);
        let picture = loop {
            if let Some(p) = theirs.remote_picture(Stream::Screen).filter(|p| p.rgba.len() > at && p.rgba[at + 2] > 170) {
                break p;
            }
            assert!(Instant::now() < deadline, "no picture arrived");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!((picture.width, picture.height), (800, 450));
        assert!(picture.rgba[at] < 80, "the colour survived: {:?}", &picture.rgba[at..at + 4]);
        assert!(mine.preview(Stream::Screen).is_some());

        mine.stop(Stream::Screen);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !theirs.remote_versions().is_empty() {
            assert!(Instant::now() < deadline, "the picture stayed after sharing stopped");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
