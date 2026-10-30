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
//! **Any emoji can be a reaction**, not only the quick ones offered on
//! every message: the quick row is there because one tap beats a search,
//! not because other emoji are dangerous. What arrives from a peer is
//! still checked, so a reaction is always *an emoji* and never a sentence:
//!
//! - a Unicode emoji, one at a time ([`is_unicode_emoji`]): a pictograph
//!   with its skin tone and presentation marks, a ZWJ family, a flag or a
//!   keycap -- nothing else, and at most [`MAX_EMOJI_BYTES`] long;
//! - a community's custom emoji, written `:name:` ([`custom_name`]), which
//!   only counts where that community has an emoji by that name. The
//!   picture comes from the community's signed manifest, so a member is
//!   only ever shown what the people they joined under picked -- the same
//!   rule that lets custom emoji appear in messages at all. A
//!   conversation with one person has no community behind it, so there
//!   only Unicode emoji are taken ([`is_allowed`]).

use serde::{Deserialize, Serialize};

/// The quick reactions: the first ones offered, and the first in a
/// message's row of reactions.
///
/// A short, fixed list, so the common case stays one tap; anything else
/// is a tap further away in the full picker.
pub const AVAILABLE: &[&str] = &["👍", "👎", "❤️", "😂", "😮", "😢", "🎉", "✅"];

/// The longest a Unicode emoji reaction may be, in bytes. The longest
/// emoji in use (a ZWJ family with skin tones, or a subdivision flag) is
/// about 35 bytes; this leaves room for a few more marks, and no room for
/// a paragraph.
pub const MAX_EMOJI_BYTES: usize = 48;

/// The longest a custom emoji's name may be, matching the community
/// manifest's own limit (`antiphon_crypto::community::MAX_EMOJI_NAME`).
pub const MAX_CUSTOM_NAME: usize = 24;

/// One person's reaction to one message.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Reaction {
    /// The message reacted to.
    pub message_id: u64,
    /// Which reaction: a Unicode emoji, or a custom one as `:name:`.
    pub emoji: String,
    /// True when it was us.
    pub from_me: bool,
}

/// Whether `emoji` may be a reaction in a conversation with one person:
/// any single Unicode emoji.
///
/// Checked on receipt: the emoji arrives from a peer, and rendering
/// arbitrary text as a "reaction" is how a reaction row becomes a place to
/// put a paragraph nobody asked for. Custom emoji belong to a community
/// and are checked there, with [`is_allowed_in`].
pub fn is_allowed(emoji: &str) -> bool {
    is_unicode_emoji(emoji)
}

/// Whether `emoji` may be a reaction in a community whose custom emoji
/// are called `custom`: any single Unicode emoji, or `:name:` for one of
/// those names.
pub fn is_allowed_in<'a>(emoji: &str, mut custom: impl Iterator<Item = &'a str>) -> bool {
    match custom_name(emoji) {
        Some(name) => custom.any(|c| c == name),
        None => is_unicode_emoji(emoji),
    }
}

/// The name in a custom emoji reaction written `:name:`, if `emoji` is
/// shaped like one: lowercase letters, digits and underscores, 2 to
/// [`MAX_CUSTOM_NAME`] long -- the rule a community's manifest enforces on
/// the names it holds. Whether the name exists is the caller's question.
pub fn custom_name(emoji: &str) -> Option<&str> {
    let name = emoji.strip_prefix(':')?.strip_suffix(':')?;
    let valid = (2..=MAX_CUSTOM_NAME).contains(&name.len())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    valid.then_some(name)
}

/// Whether `s` is exactly one Unicode emoji.
///
/// Accepted shapes, which between them cover the emoji keyboards offer:
///
/// - a flag: two regional indicators (`🇫🇮`);
/// - a keycap: `0`-`9`, `#` or `*`, an optional U+FE0F, then U+20E3 (`1️⃣`);
/// - one or more pictographs joined by zero-width joiners (`👩‍👩‍👧`), each
///   followed only by marks that change how it looks: a presentation
///   selector, a skin tone, or the tag characters of a subdivision flag
///   (`🏴󠁧󠁢󠁳󠁣󠁴󠁿`).
///
/// Not a full Unicode emoji table, which would need a dependency in both
/// apps for the sake of rejecting a few unassigned code points; the ranges
/// below are the blocks emoji are allocated from, so nothing that is not
/// a symbol gets through.
pub fn is_unicode_emoji(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_EMOJI_BYTES {
        return false;
    }
    let chars: Vec<char> = s.chars().collect();
    // A flag is a pair of regional indicators and nothing else.
    if chars.iter().any(|c| is_regional_indicator(*c)) {
        return chars.len() == 2 && chars.iter().all(|c| is_regional_indicator(*c));
    }
    // A keycap: the key, perhaps the emoji-style selector, then the cap.
    if let Some(&first) = chars.first() {
        if first.is_ascii_digit() || first == '#' || first == '*' {
            return matches!(chars[1..], ['\u{20E3}'] | ['\u{FE0F}', '\u{20E3}']);
        }
    }
    // One pictograph, or several joined into one picture by ZWJs.
    s.split('\u{200D}').all(|part| {
        let mut it = part.chars();
        match it.next() {
            Some(base) if is_pictograph(base) => it.all(is_modifier),
            _ => false,
        }
    })
}

/// A regional indicator letter: two make a country's flag.
fn is_regional_indicator(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

/// A mark that changes how the pictograph before it looks without being
/// a picture of its own: text or emoji presentation (U+FE0E, U+FE0F), a
/// skin tone (U+1F3FB-U+1F3FF), or a tag character (U+E0020-U+E007F,
/// spelling a subdivision flag such as Scotland's).
fn is_modifier(c: char) -> bool {
    matches!(c, '\u{FE0E}' | '\u{FE0F}' | '\u{1F3FB}'..='\u{1F3FF}' | '\u{E0020}'..='\u{E007F}')
}

/// A code point emoji are drawn from: the pictograph blocks above
/// U+1F000, and the older symbols in the Basic Multilingual Plane that
/// keyboards offer as emoji (©, ‼, ⌚, ☀, ✅, ⬆, 〰 and their kind).
/// Skin tones and regional indicators are excluded: alone, they are not
/// a picture.
fn is_pictograph(c: char) -> bool {
    if is_regional_indicator(c) || ('\u{1F3FB}'..='\u{1F3FF}').contains(&c) {
        return false;
    }
    matches!(
        c,
        '\u{00A9}'
            | '\u{00AE}'
            | '\u{203C}'
            | '\u{2049}'
            | '\u{2122}'
            | '\u{2139}'
            | '\u{2194}'..='\u{2199}'
            | '\u{21A9}'..='\u{21AA}'
            | '\u{231A}'..='\u{231B}'
            | '\u{2328}'
            | '\u{23CF}'
            | '\u{23E9}'..='\u{23F3}'
            | '\u{23F8}'..='\u{23FA}'
            | '\u{24C2}'
            | '\u{25AA}'..='\u{25AB}'
            | '\u{25B6}'
            | '\u{25C0}'
            | '\u{25FB}'..='\u{25FE}'
            | '\u{2600}'..='\u{27BF}'
            | '\u{2934}'..='\u{2935}'
            | '\u{2B05}'..='\u{2B07}'
            | '\u{2B1B}'..='\u{2B1C}'
            | '\u{2B50}'
            | '\u{2B55}'
            | '\u{3030}'
            | '\u{303D}'
            | '\u{3297}'
            | '\u{3299}'
            | '\u{1F000}'..='\u{1FAFF}'
    )
}

/// Adds or removes a reaction, returning whether the list changed.
///
/// Tapping the same reaction twice removes it -- the near-universal
/// convention, and the only one that does not need a separate "un-react"
/// affordance. Only Unicode emoji are taken (see [`is_allowed`]); a
/// community's reactions are checked against its emoji before they get
/// here.
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

/// Each emoji on one message with how many people used it, in a stable
/// order (see [`tally`]).
pub fn counts(reactions: &[Reaction], message_id: u64) -> Vec<(String, u32)> {
    tally(reactions.iter().filter(|r| r.message_id == message_id).map(|r| r.emoji.as_str()))
}

/// Counts a message's reactions, given in the order they were made, into
/// (emoji, how many) in a stable order: the quick reactions in
/// [`AVAILABLE`]'s order first, then any others in the order they were
/// first used. A row that keeps its order does not reshuffle under
/// someone's pointer as reactions arrive. Shared by conversations and
/// community channels, which store reactions differently.
pub fn tally<'a>(emoji: impl IntoIterator<Item = &'a str>) -> Vec<(String, u32)> {
    let mut out: Vec<(String, u32)> = AVAILABLE.iter().map(|e| (e.to_string(), 0)).collect();
    for e in emoji {
        match out.iter_mut().find(|(seen, _)| seen == e) {
            Some((_, n)) => *n += 1,
            None => out.push((e.to_string(), 1)),
        }
    }
    out.retain(|(_, n)| *n > 0);
    out
}

/// A compact summary for one message, in the order of [`counts`].
pub fn summarize(reactions: &[Reaction], message_id: u64) -> String {
    counts(reactions, message_id)
        .into_iter()
        .map(|(emoji, n)| if n == 1 { emoji } else { format!("{emoji}{n}") })
        .collect::<Vec<_>>()
        .join(" ")
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
    fn text_is_refused_as_a_reaction() {
        // The emoji comes from a peer. Without this, the reaction row is
        // somewhere to put arbitrary text.
        let mut reactions = Vec::new();
        for bad in ["a whole sentence, actually", "", "a", "1", "👍 👍", "👍👍", "👍a", "🇫", "🇫🇮🇸🇪", "🏻", "\u{200D}"] {
            assert!(!toggle(&mut reactions, 1, bad, true), "{bad:?} was taken");
        }
        assert!(!toggle(&mut reactions, 1, &"😀".repeat(20), true), "too long");
        assert!(reactions.is_empty());
    }

    #[test]
    fn any_single_emoji_is_a_reaction() {
        // Not only the quick eight: a duck, a skin tone, a family joined
        // with ZWJs, a flag, a keycap and a subdivision flag.
        for good in ["🦆", "👍🏽", "👩‍👩‍👧‍👦", "🇫🇮", "1️⃣", "#⃣", "🏴󠁧󠁢󠁳󠁣󠁴󠁿", "☀️", "✅", "🫠", "❤️‍🔥"] {
            assert!(is_unicode_emoji(good), "{good:?} was refused");
        }
    }

    #[test]
    fn custom_emoji_count_only_where_the_community_has_them() {
        let names = ["party", "cat_wave"];
        assert!(is_allowed_in(":party:", names.iter().copied()));
        assert!(is_allowed_in("🦆", names.iter().copied()), "Unicode emoji work in a community too");
        assert!(!is_allowed_in(":nope:", names.iter().copied()), "not one of this community's");
        assert!(!is_allowed_in(":Party:", names.iter().copied()), "names are lowercase");
        // A conversation with one person has no community to draw from.
        assert!(!is_allowed(":party:"));
        assert_eq!(custom_name(":cat_wave:"), Some("cat_wave"));
        assert_eq!(custom_name(":a:"), None, "too short");
        assert_eq!(custom_name("party"), None);
    }

    #[test]
    fn other_emoji_follow_the_quick_ones_in_arrival_order() {
        let mut reactions = Vec::new();
        toggle(&mut reactions, 1, "🦆", false);
        toggle(&mut reactions, 1, "🫠", true);
        toggle(&mut reactions, 1, "👍", true);
        toggle(&mut reactions, 1, "🦆", true);
        assert_eq!(counts(&reactions, 1), [("👍".to_string(), 1), ("🦆".to_string(), 2), ("🫠".to_string(), 1)]);
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
