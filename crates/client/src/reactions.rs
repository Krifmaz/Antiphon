//! Reactions (spec §13: "reactions, custom emoji, soundboard").
//!
//! A reaction is a message about a message, which makes it the smallest
//! feature here with a real design question attached: what happens when
//! the message it points at is edited, deleted, or was never received?
//!
//! The answer taken here is that a reaction is **attached to a message id
//! and nothing else**. If the message is gone, so is the reaction -- it is
//! not promoted to a floating comment, and it does not resurrect a
//! tombstone. Someone who deleted a message deleted the thing people were
//! reacting to, and leaving a row of thumbs-up hanging under "(deleted)"
//! would undo that.
//!
//! Custom emoji are deliberately **not** implemented. They would mean
//! receiving and rendering an image chosen by someone else -- the same
//! decoder-on-untrusted-input problem as link previews and thumbnails --
//! and unlike a thumbnail, a custom emoji is shown without anyone
//! accepting anything first. The standard set covers the actual use, which
//! is reacting without interrupting.

use serde::{Deserialize, Serialize};

/// The reactions offered.
///
/// A short, fixed list rather than a picker over every emoji: a long list
/// turns a one-tap acknowledgement into a search, and the point of a
/// reaction is that it is faster than typing.
pub const AVAILABLE: &[&str] = &["👍", "👎", "❤️", "😂", "😮", "😢", "🎉", "✅"];

/// One person's reaction to one message.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Reaction {
    /// The message reacted to.
    pub message_id: u64,
    /// Which reaction, as one of [`AVAILABLE`].
    pub emoji: String,
    /// True when it was us.
    pub from_me: bool,
}

/// Whether `emoji` is one this app offers.
///
/// Checked on receipt: the emoji arrives from a peer, and rendering
/// arbitrary text as a "reaction" is how a reaction row becomes a place to
/// put a paragraph nobody asked for.
pub fn is_allowed(emoji: &str) -> bool {
    AVAILABLE.contains(&emoji)
}

/// Adds or removes a reaction, returning whether the list changed.
///
/// Tapping the same reaction twice removes it -- the near-universal
/// convention, and the only one that does not need a separate "un-react"
/// affordance.
pub fn toggle(reactions: &mut Vec<Reaction>, message_id: u64, emoji: &str, from_me: bool) -> bool {
    if !is_allowed(emoji) {
        return false;
    }
    let existing = reactions
        .iter()
        .position(|r| r.message_id == message_id && r.emoji == emoji && r.from_me == from_me);

    match existing {
        Some(at) => {
            reactions.remove(at);
        }
        None => reactions.push(Reaction {
            message_id,
            emoji: emoji.to_string(),
            from_me,
        }),
    }
    true
}

/// Drops every reaction pointing at a message that no longer exists.
///
/// Called after a delete. A reaction is about a message, so without the
/// message it is about nothing -- and leaving thumbs-up under "(deleted)"
/// would partly undo the deletion.
pub fn forget_message(reactions: &mut Vec<Reaction>, message_id: u64) -> usize {
    let before = reactions.len();
    reactions.retain(|r| r.message_id != message_id);
    before - reactions.len()
}

/// A compact summary for one message, in the order of [`AVAILABLE`] so it
/// does not reshuffle as people react.
pub fn summarize(reactions: &[Reaction], message_id: u64) -> String {
    let mut parts = Vec::new();
    for emoji in AVAILABLE {
        let count = reactions
            .iter()
            .filter(|r| r.message_id == message_id && r.emoji == *emoji)
            .count();
        if count == 1 {
            parts.push((*emoji).to_string());
        } else if count > 1 {
            parts.push(format!("{emoji}{count}"));
        }
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reaction_can_be_added_and_taken_back() {
        let mut reactions = Vec::new();

        assert!(toggle(&mut reactions, 1, "👍", true));
        assert_eq!(reactions.len(), 1);

        // Tapping again removes it -- no separate un-react button needed.
        assert!(toggle(&mut reactions, 1, "👍", true));
        assert!(reactions.is_empty());
    }

    #[test]
    fn both_people_can_react_with_the_same_thing() {
        let mut reactions = Vec::new();
        toggle(&mut reactions, 1, "👍", true);
        toggle(&mut reactions, 1, "👍", false);

        assert_eq!(reactions.len(), 2);
        assert_eq!(summarize(&reactions, 1), "👍2");
    }

    #[test]
    fn an_emoji_this_app_does_not_offer_is_refused() {
        // The emoji comes from a peer. Without this, the reaction row is
        // somewhere to put arbitrary text.
        let mut reactions = Vec::new();
        assert!(!toggle(&mut reactions, 1, "a whole sentence, actually", true));
        assert!(!toggle(&mut reactions, 1, "🦆", true));
        assert!(reactions.is_empty());
    }

    #[test]
    fn deleting_a_message_takes_its_reactions_with_it() {
        // Leaving thumbs-up under "(deleted)" would partly undo the
        // deletion the sender asked for.
        let mut reactions = Vec::new();
        toggle(&mut reactions, 1, "👍", true);
        toggle(&mut reactions, 1, "😂", false);
        toggle(&mut reactions, 2, "👍", true);

        assert_eq!(forget_message(&mut reactions, 1), 2);
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].message_id, 2);
    }

    #[test]
    fn forgetting_a_message_with_no_reactions_is_a_no_op() {
        let mut reactions = vec![Reaction { message_id: 1, emoji: "👍".into(), from_me: true }];
        assert_eq!(forget_message(&mut reactions, 99), 0);
        assert_eq!(reactions.len(), 1);
    }

    #[test]
    fn a_summary_keeps_a_stable_order() {
        // Ordered by the fixed list, not by arrival, so the row does not
        // reshuffle under someone's cursor as reactions come in.
        let mut reactions = Vec::new();
        toggle(&mut reactions, 1, "🎉", true);
        toggle(&mut reactions, 1, "👍", false);

        let first = summarize(&reactions, 1);
        assert!(first.find('👍') < first.find('🎉'), "{first}");

        // Adding another must not reorder what was there.
        toggle(&mut reactions, 1, "❤️", true);
        let second = summarize(&reactions, 1);
        assert!(second.find('👍') < second.find('🎉'), "{second}");
    }

    #[test]
    fn a_message_with_no_reactions_summarises_to_nothing() {
        // So the caller hides the row rather than showing an empty one.
        assert!(summarize(&[], 1).is_empty());
        let reactions = vec![Reaction { message_id: 2, emoji: "👍".into(), from_me: true }];
        assert!(summarize(&reactions, 1).is_empty());
    }

    #[test]
    fn a_single_reaction_shows_without_a_count() {
        let reactions = vec![Reaction { message_id: 1, emoji: "👍".into(), from_me: true }];
        assert_eq!(summarize(&reactions, 1), "👍");
    }

    #[test]
    fn every_offered_reaction_is_accepted_by_the_receiver_check() {
        for emoji in AVAILABLE {
            assert!(is_allowed(emoji), "{emoji} is offered but would be refused on receipt");
        }
    }

    #[test]
    fn the_list_is_short_enough_to_be_faster_than_typing() {
        // A picker over every emoji turns a one-tap acknowledgement into a
        // search, which defeats the point.
        assert!(AVAILABLE.len() <= 10, "the reaction list has grown to {}", AVAILABLE.len());
    }

    #[test]
    fn reactions_round_trip_through_json() {
        let reactions = vec![
            Reaction { message_id: 1, emoji: "👍".into(), from_me: true },
            Reaction { message_id: 1, emoji: "😂".into(), from_me: false },
        ];
        let json = serde_json::to_string(&reactions).unwrap();
        assert_eq!(serde_json::from_str::<Vec<Reaction>>(&json).unwrap(), reactions);
    }
}
