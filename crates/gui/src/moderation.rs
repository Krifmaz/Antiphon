//! Blocking and reporting (spec §9: "moderation tools -- block, mute,
//! report, per-community ban lists (needed for app store review as much as
//! for users)").
//!
//! Blocking in a peer-to-peer app is different from blocking on a service,
//! and the difference matters:
//!
//! - There is **no server to enforce it**. A block is a rule your own
//!   machine applies to incoming connections. Someone can still send
//!   packets at you; what a block guarantees is that your app hangs up
//!   without ever showing you anything.
//! - It is keyed to an **identity**, not an address. Blocking an address
//!   blocks whoever happens to be there next, which is both too broad and
//!   trivially escaped by reconnecting from somewhere else.
//! - It **cannot be undone remotely** and involves nobody else. There is
//!   no appeal, because there is no authority -- which is the honest
//!   consequence of having no server.
//!
//! Reports are the awkward one, and the spec knows it: §12's fifth open
//! question is "moderation without content access -- you can't scan what
//! you can't read". Since nothing can read a call, a report cannot carry
//! evidence. What it can do is record, locally, that *you* judged someone,
//! so that a future community node has something to consult if you choose
//! to share it. Reports here therefore go nowhere by default. Anything
//! else would be claiming a moderation pipeline that does not exist.

use serde::{Deserialize, Serialize};

/// Someone the user has blocked.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BlockedIdentity {
    /// Hex Ed25519 identity key -- the person, not their address.
    pub identity_key: String,
    /// What they were called when blocked, so the list is readable.
    pub petname: String,
    pub blocked_at_secs: u64,
    /// The user's own note. Never sent anywhere.
    #[serde(default)]
    pub reason: String,
}

/// A locally recorded report.
///
/// Kept on this device. There is nowhere to send it: no server can read a
/// call, so no report can carry evidence of one.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Report {
    pub identity_key: String,
    pub petname: String,
    pub at_secs: u64,
    pub note: String,
}

/// Whether this identity is blocked.
pub fn is_blocked(blocked: &[BlockedIdentity], identity_key: &str) -> bool {
    blocked.iter().any(|b| b.identity_key.eq_ignore_ascii_case(identity_key))
}

/// Adds a block, replacing any existing entry for the same identity.
///
/// Blocking someone twice is not an error and must not produce two
/// entries -- the list is a set of people, not a log of decisions.
pub fn block(
    blocked: &mut Vec<BlockedIdentity>,
    identity_key: &str,
    petname: &str,
    reason: &str,
    now_secs: u64,
) {
    blocked.retain(|b| !b.identity_key.eq_ignore_ascii_case(identity_key));
    blocked.push(BlockedIdentity {
        identity_key: identity_key.to_lowercase(),
        petname: petname.to_string(),
        blocked_at_secs: now_secs,
        reason: reason.to_string(),
    });
}

/// Removes a block. Returns whether anything changed.
pub fn unblock(blocked: &mut Vec<BlockedIdentity>, identity_key: &str) -> bool {
    let before = blocked.len();
    blocked.retain(|b| !b.identity_key.eq_ignore_ascii_case(identity_key));
    blocked.len() != before
}

/// Records a report locally.
pub fn report(
    reports: &mut Vec<Report>,
    identity_key: &str,
    petname: &str,
    note: &str,
    now_secs: u64,
) {
    reports.push(Report {
        identity_key: identity_key.to_lowercase(),
        petname: petname.to_string(),
        at_secs: now_secs,
        note: note.to_string(),
    });
}

/// The plain-language explanation of what a block does here, for the UI.
///
/// Written to avoid the two ways this is usually oversold: that the person
/// is prevented from contacting you at all, and that someone else is
/// informed.
pub const BLOCK_EXPLANATION: &str = "They can no longer reach you: calls from them are \
    refused without showing you anything. This happens on your machine, so they are not \
    told, and there is no server involved. Blocking follows the person, not their address, \
    so it still works if they move.";

/// The honest explanation of what a report does.
pub const REPORT_EXPLANATION: &str = "Saved on this device only. Nothing can read your \
    calls -- not this app, not anyone -- so a report cannot carry evidence of what \
    happened. It is a note to yourself, and something you could hand to a community you \
    trust if you ever choose to.";

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const KEY_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn a_blocked_identity_is_recognised() {
        let mut blocked = Vec::new();
        block(&mut blocked, KEY_A, "Nuisance", "", 1);

        assert!(is_blocked(&blocked, KEY_A));
        assert!(!is_blocked(&blocked, KEY_B));
    }

    #[test]
    fn blocking_follows_the_person_not_their_address() {
        // Nothing about an address appears in the record, so reconnecting
        // from somewhere else changes nothing.
        let mut blocked = Vec::new();
        block(&mut blocked, KEY_A, "Nuisance", "", 1);

        let json = serde_json::to_string(&blocked).unwrap();
        assert!(!json.contains("127.0.0.1"));
        assert!(json.contains(KEY_A));
    }

    #[test]
    fn case_does_not_let_someone_slip_past_a_block() {
        let mut blocked = Vec::new();
        block(&mut blocked, &KEY_A.to_uppercase(), "Nuisance", "", 1);

        assert!(is_blocked(&blocked, KEY_A), "uppercase key evaded the block");
        assert!(is_blocked(&blocked, &KEY_A.to_uppercase()));
    }

    #[test]
    fn blocking_twice_does_not_produce_two_entries() {
        // The list is a set of people, not a log of decisions.
        let mut blocked = Vec::new();
        block(&mut blocked, KEY_A, "Nuisance", "first", 1);
        block(&mut blocked, KEY_A, "Nuisance", "second", 2);

        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].reason, "second");
    }

    #[test]
    fn unblocking_removes_the_entry_and_reports_whether_it_did() {
        let mut blocked = Vec::new();
        block(&mut blocked, KEY_A, "Nuisance", "", 1);

        assert!(unblock(&mut blocked, KEY_A));
        assert!(!is_blocked(&blocked, KEY_A));
        // Unblocking someone who was not blocked is a no-op, not an error.
        assert!(!unblock(&mut blocked, KEY_A));
    }

    #[test]
    fn an_empty_block_list_blocks_nobody() {
        assert!(!is_blocked(&[], KEY_A));
    }

    #[test]
    fn reports_accumulate_rather_than_replacing_each_other() {
        // Unlike blocks, two reports about the same person are two
        // separate incidents worth keeping.
        let mut reports = Vec::new();
        report(&mut reports, KEY_A, "Nuisance", "first thing", 1);
        report(&mut reports, KEY_A, "Nuisance", "second thing", 2);

        assert_eq!(reports.len(), 2);
    }

    #[test]
    fn the_explanations_do_not_overpromise() {
        // The two ways this is normally oversold: implying the person is
        // stopped entirely, and implying somebody was told.
        let block_text = BLOCK_EXPLANATION.to_lowercase();
        assert!(block_text.contains("your machine"), "should say where it is enforced");
        assert!(block_text.contains("not told"), "should say they are not notified");

        let report_text = REPORT_EXPLANATION.to_lowercase();
        assert!(report_text.contains("this device only"), "should say it goes nowhere");
        assert!(
            report_text.contains("cannot carry evidence"),
            "should be honest that nothing can read a call"
        );
    }

    #[test]
    fn blocked_entries_round_trip_through_json() {
        let mut blocked = Vec::new();
        block(&mut blocked, KEY_A, "Nuisance", "kept calling", 1_700_000_000);

        let json = serde_json::to_string(&blocked).unwrap();
        let back: Vec<BlockedIdentity> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, blocked);
    }
}
