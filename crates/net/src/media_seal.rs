//! Sealing group-call media end to end, so an SFU forwards frames it
//! cannot read (spec §3 "SFU relay forwarding encrypted frames" and
//! "Media through a relay -- SFrame").
//!
//! The SFU ([`crate::sfu`]) treats every payload as opaque bytes; what
//! makes that true is a [`MediaSealer`] on each member. The trait is
//! deliberately tiny -- seal my frame, open a frame from that member --
//! so the key schedule behind it can change without touching the
//! transport. [`SframeSealer`] is the implementation: RFC 9605 SFrame
//! (`antiphon_crypto::sframe`) under keys exported from the call's MLS
//! group, so a member removed from the group can't read the next epoch.

use std::collections::HashMap;

use antiphon_crypto::sframe::SframeContext;

use crate::sfu::MemberTag;

/// Seals this member's media and opens everyone else's.
///
/// Contract for implementations:
///
/// - `seal` output must be self-contained: whatever the receiver needs
///   besides its keys (a counter, an epoch) travels inside it. Frames go
///   over UDP, so they may arrive late, twice, or not at all.
/// - `open` returns `None` for anything it cannot authenticate -- a
///   forged or corrupted frame, an unknown sender, or a replay -- and
///   never panics on hostile input. `sender` is the member tag the SFU
///   routed the frame under; bind it into the authentication so one
///   member cannot pass a frame off as another's.
/// - Output stays under [`crate::sfu::MAX_MEDIA_PAYLOAD`] for an audio
///   frame.
pub trait MediaSealer: Send {
    /// `None` while there is no key to seal under yet.
    fn seal(&mut self, plain: &[u8]) -> Option<Vec<u8>>;
    /// Opens a frame the SFU routed under `sender`'s tag; `None` for anything
    /// that does not authenticate as that member's.
    fn open(&mut self, sender: MemberTag, sealed: &[u8]) -> Option<Vec<u8>>;
}

/// SFrame behind the SFU: the frame's KID names the sender's MLS leaf, and
/// [`MediaSealer::open`] checks that leaf is the one the room tag belongs
/// to, so a member can't replay its frames under someone else's tag.
pub struct SframeSealer {
    /// The SFrame keys and replay state for this call.
    ctx: SframeContext,
    /// Which MLS leaf each room tag belongs to, so a frame's KID can be checked
    /// against the tag it arrived under.
    leaves: HashMap<MemberTag, u32>,
}

impl SframeSealer {
    /// A sealer over `ctx`, with no senders known yet.
    pub fn new(ctx: SframeContext) -> Self {
        Self { ctx, leaves: HashMap::new() }
    }

    /// The SFrame context, to install a new epoch's key into.
    pub fn context(&mut self) -> &mut SframeContext {
        &mut self.ctx
    }

    /// Records which MLS leaf speaks under `sender`'s tag.
    pub fn set_sender(&mut self, sender: MemberTag, leaf: u32) {
        self.leaves.insert(sender, leaf);
    }

}

impl MediaSealer for SframeSealer {
    fn seal(&mut self, plain: &[u8]) -> Option<Vec<u8>> {
        self.ctx.epoch()?;
        Some(self.ctx.seal(plain))
    }

    fn open(&mut self, sender: MemberTag, sealed: &[u8]) -> Option<Vec<u8>> {
        let leaf = *self.leaves.get(&sender)?;
        self.ctx.open(leaf, sealed).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two sealers keyed into the same epoch, as leaves 0 and 1.
    fn pair() -> (SframeSealer, SframeSealer) {
        let key = [7u8; 16];
        let mut a = SframeContext::new(0);
        a.install_epoch(1, 0, &key);
        let mut b = SframeContext::new(1);
        b.install_epoch(1, 1, &key);
        let (mut a, mut b) = (SframeSealer::new(a), SframeSealer::new(b));
        a.set_sender([2; 16], 1);
        b.set_sender([1; 16], 0);
        (a, b)
    }

    #[test]
    fn frames_open_only_under_the_sender_s_own_tag() {
        let (mut a, mut b) = pair();
        let sealed = a.seal(b"audio").unwrap();
        assert_eq!(b.open([1; 16], &sealed).as_deref(), Some(&b"audio"[..]));
        // Replayed, or passed off under another tag: refused.
        assert!(b.open([1; 16], &sealed).is_none());
        let again = a.seal(b"more").unwrap();
        assert!(b.open([9; 16], &again).is_none());
    }

    #[test]
    fn nothing_seals_before_a_key_exists() {
        assert!(SframeSealer::new(SframeContext::new(0)).seal(b"x").is_none());
    }
}
