//! Kinetic scrolling: a swipe keeps the list moving after the finger
//! lifts, and it slows to a stop the way a phone's own lists do.
//!
//! iced's scrollables follow a dragging finger but stop dead when it
//! lifts. This watches the finger, and on a quick vertical swipe keeps
//! going with the finger's speed, handing the runner a scroll step each
//! frame (which it sends to the interface as a wheel scroll at the place
//! the finger left) while friction bleeds the speed away. A new touch
//! catches the list where it is.
//!
//! Plain arithmetic on positions and times, so it is tested on any
//! machine; the runner owns the events.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How far back the finger's last movements count towards its speed. A
/// swipe is judged by how it ended, not how it began.
const WINDOW: Duration = Duration::from_millis(90);

/// Slowest lift, in logical pixels a second, that starts a fling. Below
/// it the finger was placing the list, not throwing it.
const MIN_START: f32 = 250.0;

/// Fastest a fling starts, so a frantic swipe does not send a long list
/// off to its end in one go.
const MAX_START: f32 = 7000.0;

/// Below this the list is as good as stopped, and the fling ends.
const MIN_KEEP: f32 = 18.0;

/// How long the speed takes to fall to about a third (1/e): a little
/// over a second for a full-speed swipe to settle, close to Android's
/// default list friction.
const TAU: f32 = 0.33;

/// How far the finger must travel for its lift to count as a swipe; less
/// is a tap, which must never scroll.
const SLOP: f32 = 12.0;

/// A point in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    /// Across.
    pub x: f32,
    /// Down.
    pub y: f32,
}

/// The finger's recent path and, once thrown, the moving list.
#[derive(Debug, Default)]
pub struct Fling {
    /// Recent finger positions while it is down, oldest first.
    samples: VecDeque<(Instant, Point)>,
    /// Where the finger went down, to tell a tap from a swipe.
    start: Option<Point>,
    /// Where the fling scrolls: the place the finger left.
    at: Option<Point>,
    /// The fling's speed, logical pixels a second, down positive. Zero
    /// when not flinging.
    velocity: f32,
    /// When the last step was taken.
    last: Option<Instant>,
}

impl Fling {
    /// A fling at rest.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the list is still moving, so the runner keeps drawing.
    pub fn active(&self) -> bool {
        self.velocity != 0.0
    }

    /// A finger went down: stop any fling where it is, and start
    /// watching this one.
    pub fn pressed(&mut self, now: Instant, p: Point) {
        self.velocity = 0.0;
        self.samples.clear();
        self.samples.push_back((now, p));
        self.start = Some(p);
    }

    /// The finger moved.
    pub fn moved(&mut self, now: Instant, p: Point) {
        self.samples.push_back((now, p));
        while self.samples.front().is_some_and(|(t, _)| now.duration_since(*t) > WINDOW) && self.samples.len() > 2 {
            self.samples.pop_front();
        }
    }

    /// A second finger came down: a pinch or a two-finger gesture is not
    /// a swipe, so nothing is thrown when they lift.
    pub fn cancel(&mut self) {
        self.velocity = 0.0;
        self.samples.clear();
        self.start = None;
    }

    /// The finger lifted. A quick, mostly vertical swipe starts a fling.
    pub fn lifted(&mut self, now: Instant, p: Point) {
        self.moved(now, p);
        let start = self.start.take();
        let samples = std::mem::take(&mut self.samples);
        let (Some(start), Some((t0, p0))) = (start, samples.front().copied()) else { return };
        if (p.y - start.y).abs() < SLOP {
            return;
        }
        let dt = now.duration_since(t0).as_secs_f32();
        // A finger that stopped before lifting threw nothing.
        if dt <= 0.0 || now.duration_since(samples.iter().rev().nth(1).map(|s| s.0).unwrap_or(t0)) > WINDOW {
            return;
        }
        let (vx, vy) = ((p.x - p0.x) / dt, (p.y - p0.y) / dt);
        // Sideways swipes are for sliders and switches, never a scroll.
        if vy.abs() < MIN_START || vy.abs() < vx.abs() * 1.5 {
            return;
        }
        self.velocity = vy.clamp(-MAX_START, MAX_START);
        self.at = Some(p);
        self.last = Some(now);
    }

    /// Moves the fling on to `now`: where to scroll and by how much (in
    /// logical pixels, down positive, as the finger would have moved), or
    /// `None` when there is nothing to do.
    pub fn step(&mut self, now: Instant) -> Option<(Point, f32)> {
        if !self.active() {
            return None;
        }
        let last = self.last.replace(now)?;
        let dt = now.duration_since(last).as_secs_f32().min(0.1);
        // Exact for exponential friction, whatever the frame rate.
        let decay = (-dt / TAU).exp();
        let distance = self.velocity * TAU * (1.0 - decay);
        self.velocity *= decay;
        if self.velocity.abs() < MIN_KEEP {
            self.velocity = 0.0;
        }
        Some((self.at?, distance))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: f32) -> Point {
        Point { x: 100.0, y }
    }

    /// A swipe of `pixels` over `ms`, in ten steps, lifted at `t0 + ms`.
    fn swipe(f: &mut Fling, t0: Instant, pixels: f32, ms: u64) -> Instant {
        f.pressed(t0, at(500.0));
        for i in 1..=10 {
            f.moved(t0 + Duration::from_millis(ms * i / 10), at(500.0 + pixels * i as f32 / 10.0));
        }
        let end = t0 + Duration::from_millis(ms);
        f.lifted(end, at(500.0 + pixels));
        end
    }

    #[test]
    fn a_quick_swipe_keeps_going_and_slows_to_a_stop() {
        let mut f = Fling::new();
        let mut t = swipe(&mut f, Instant::now(), -300.0, 100);
        assert!(f.active());
        let (mut total, mut last_step) = (0.0, f32::MAX);
        let mut frames = 0;
        loop {
            t += Duration::from_millis(16);
            let Some((p, d)) = f.step(t) else { break };
            assert_eq!(p, at(200.0), "scrolls where the finger left");
            assert!(d <= 0.0, "keeps the swipe's direction");
            assert!(d.abs() <= last_step + 0.001, "never speeds up");
            last_step = d.abs();
            total += d;
            frames += 1;
        }
        assert!(total < -500.0, "travels well past the swipe: {total}");
        assert!((40..400).contains(&frames), "settles in a second or few: {frames} frames");
        assert!(!f.active());
    }

    #[test]
    fn a_tap_or_a_slow_drag_does_not_fling() {
        let mut f = Fling::new();
        swipe(&mut f, Instant::now(), -5.0, 60);
        assert!(!f.active(), "a tap");
        swipe(&mut f, Instant::now(), -80.0, 1500);
        assert!(!f.active(), "a slow, placing drag");
    }

    #[test]
    fn a_finger_that_stops_before_lifting_throws_nothing() {
        let mut f = Fling::new();
        let t0 = Instant::now();
        let end = swipe(&mut f, t0, -300.0, 100);
        f.pressed(end, at(500.0));
        f.moved(end + Duration::from_millis(50), at(300.0));
        f.lifted(end + Duration::from_millis(400), at(300.0));
        assert!(!f.active());
    }

    #[test]
    fn a_sideways_swipe_is_left_to_sliders() {
        let mut f = Fling::new();
        let t0 = Instant::now();
        f.pressed(t0, Point { x: 0.0, y: 500.0 });
        f.moved(t0 + Duration::from_millis(50), Point { x: 200.0, y: 480.0 });
        f.lifted(t0 + Duration::from_millis(100), Point { x: 400.0, y: 470.0 });
        assert!(!f.active());
    }

    #[test]
    fn a_new_touch_catches_the_list() {
        let mut f = Fling::new();
        let t = swipe(&mut f, Instant::now(), 400.0, 80);
        assert!(f.active());
        f.pressed(t + Duration::from_millis(100), at(300.0));
        assert!(!f.active());
        assert!(f.step(t + Duration::from_millis(116)).is_none());
    }
}
