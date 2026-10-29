//! Automatic call bandwidth, the client's half (spec §2): reports go out,
//! reports come in, and the voice and video senders are tuned from them.
//!
//! About once a second each side sends every person it hears a
//! [`ControlMessage::LinkReport`]: how much of their voice it lost since
//! the last one, plus the timestamps for a round trip. The round trip is
//! measured the way RTCP does it, with no shared clock: a report carries
//! our own send time (`sent_ms`), the other side echoes the last one it
//! got back with how long it held it, and the difference is the time spent
//! on the wire both ways.
//!
//! What to send is decided by [`antiphon_net::rate::RateController`];
//! this module only feeds it and applies its [`Plan`]. One [`LinkQuality`]
//! per call, on the call's [`crate::group::Group`], so the primary
//! connection, mesh legs and the encoder all share it.
//!
//! **Low data.** A single switch for every call ([`set_low_data`]): read
//! on every plan, so flipping it mid-call takes effect within a packet.
//! The desktop sets it from the user's setting; the phone from its data
//! saver while on mobile data.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use antiphon_net::rate::{Plan, RateController};
use antiphon_net::voice::{LossCounter, VoiceSender};

use crate::control::ControlMessage;

/// How often each side reports, in ticks of the call's 10 ms playback
/// loop.
pub const REPORT_EVERY_TICKS: u32 = 100;

/// "Low data" for every call on this device.
static LOW_DATA: AtomicBool = AtomicBool::new(false);

/// Turns "Low data" on or off for every call, including one running now.
pub fn set_low_data(on: bool) {
    LOW_DATA.store(on, Ordering::Relaxed);
}

/// Whether "Low data" is on.
pub fn low_data() -> bool {
    LOW_DATA.load(Ordering::Relaxed)
}

/// What we know about one person we hear.
#[derive(Default)]
struct Source {
    /// Their voice's loss count, read for our reports to them.
    loss: Option<Arc<LossCounter>>,
    /// The `sent_ms` of their last report, and when it arrived, echoed
    /// in our next report so they can time the round trip.
    heard: Option<(u32, Instant)>,
}

/// One call's link measurements and the controller they drive.
pub struct LinkQuality {
    /// Zero for `sent_ms`: only ever compared with itself.
    epoch: Instant,
    controller: Mutex<RateController>,
    sources: Mutex<HashMap<u64, Source>>,
}

impl Default for LinkQuality {
    fn default() -> Self {
        Self { epoch: Instant::now(), controller: Mutex::new(RateController::new()), sources: Mutex::default() }
    }
}

impl LinkQuality {
    fn now_ms(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    /// Starts counting loss on `source`'s voice (0 for the primary
    /// connection, a leg's id + 1 for the rest).
    pub fn watch(&self, source: u64, loss: Arc<LossCounter>) {
        self.sources.lock().unwrap().entry(source).or_default().loss = Some(loss);
    }

    /// Forgets `source`, e.g. when they leave, so their old reports stop
    /// holding everyone else's quality down.
    pub fn forget(&self, source: u64) {
        self.sources.lock().unwrap().remove(&source);
        self.controller.lock().unwrap().forget(source);
    }

    /// Our report for `source`: what we lost of their voice since the
    /// last one, and the echo for their round trip.
    pub fn report(&self, source: u64) -> ControlMessage {
        let now = Instant::now();
        let sources = self.sources.lock().unwrap();
        let s = sources.get(&source);
        let loss_pct = s.and_then(|s| s.loss.as_ref()).and_then(|l| l.take_percent());
        let (echo_ms, held_ms) = match s.and_then(|s| s.heard) {
            Some((sent, at)) => (Some(sent), now.saturating_duration_since(at).as_millis() as u32),
            None => (None, 0),
        };
        ControlMessage::LinkReport { loss_pct, sent_ms: self.now_ms(), echo_ms, held_ms }
    }

    /// Takes `source`'s report on how our voice reached them.
    pub fn on_report(&self, source: u64, loss_pct: Option<u8>, sent_ms: u32, echo_ms: Option<u32>, held_ms: u32) {
        let now = Instant::now();
        self.sources.lock().unwrap().entry(source).or_default().heard = Some((sent_ms, now));
        // Our own send time, echoed: now minus that is the round trip
        // plus however long they sat on it, which they told us.
        let rtt_ms = echo_ms.map(|echo| self.now_ms().saturating_sub(echo).saturating_sub(held_ms));
        // No loss figure means they heard none of our voice -- we may not
        // be talking. Not a loss; the round trip still counts.
        self.controller.lock().unwrap().on_report(source, loss_pct.unwrap_or(0), rtt_ms, now);
    }

    /// Takes a loss figure with no timing, as reports through a group
    /// relay's room carry (they go to everyone, so there is no one echo
    /// to time a round trip with).
    pub fn on_loss(&self, source: u64, loss_pct: u8) {
        self.controller.lock().unwrap().on_report(source, loss_pct, None, Instant::now());
    }

    /// What to send now, with "Low data" applied.
    pub fn plan(&self) -> Plan {
        let mut c = self.controller.lock().unwrap();
        c.set_low_data(low_data());
        c.plan()
    }

    /// Brings a voice encoder in line with the plan: its tier, and how
    /// much FEC to spend for the loss being reported.
    pub fn tune(&self, sender: &mut VoiceSender) {
        let plan = self.plan();
        sender.set_tier(plan.voice_tier);
        sender.set_expected_loss(self.controller.lock().unwrap().expected_loss());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_fields(m: ControlMessage) -> (Option<u8>, u32, Option<u32>, u32) {
        match m {
            ControlMessage::LinkReport { loss_pct, sent_ms, echo_ms, held_ms } => (loss_pct, sent_ms, echo_ms, held_ms),
            other => panic!("not a report: {other:?}"),
        }
    }

    #[test]
    fn a_report_round_trip_measures_the_time_on_the_wire() {
        let alice = LinkQuality::default();
        let bob = LinkQuality::default();
        // Alice reports to Bob; Bob holds it 30 ms, then reports back
        // with the echo; the "wire" adds nothing here.
        let (loss, sent, echo, held) = report_fields(alice.report(0));
        assert_eq!((loss, echo, held), (None, None, 0), "nothing heard yet");
        bob.on_report(0, loss, sent, echo, held);
        std::thread::sleep(std::time::Duration::from_millis(30));
        let (loss, sent, echo, held) = report_fields(bob.report(0));
        assert!(held >= 30);
        assert!(echo.is_some());
        alice.on_report(0, loss, sent, echo, held);
        // Alice's RTT is ~0 (the 30 ms was Bob's holding time), so a
        // clean report: the plan stays at the top.
        assert_eq!(alice.plan(), antiphon_net::rate::LADDER[0]);
    }

    #[test]
    fn reported_loss_lowers_the_encoder() {
        let link = LinkQuality::default();
        link.on_report(0, Some(30), 0, None, 0);
        let mut sender = VoiceSender::new(48_000);
        link.tune(&mut sender);
        assert!(sender.tier() > 0 || link.plan().video_percent < 100);
        assert_eq!(link.plan(), antiphon_net::rate::LADDER[2]);
    }

    #[test]
    fn low_data_applies_to_a_call_already_running() {
        let link = LinkQuality::default();
        assert_eq!(link.plan().video_percent, 100);
        set_low_data(true);
        let plan = link.plan();
        set_low_data(false);
        assert_eq!(plan, antiphon_net::rate::LADDER[antiphon_net::rate::LOW_DATA_STEP]);
    }
}
