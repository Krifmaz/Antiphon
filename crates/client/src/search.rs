//! Settings search (spec §0 "Explain everything" -- a setting nobody can
//! find is as broken as one nobody can understand).
//!
//! Every collapsible settings section is indexed by its visible title plus
//! a few keywords for the words people actually reach for that aren't in
//! the title -- "sibilance" for the de-esser, "rumble" for the high-pass.
//! Searching filters each category's section order down to the matches,
//! which means the existing `for section-id in <category>-order` loops in
//! the UI do the hiding for free, with no per-section conditionals.

/// Which settings category a section lives in. The numbers match
/// `settings-category` in the UI.
/// `General` and `Keybinds` have no reorderable sections to index, so
/// nothing constructs them -- they exist so the discriminants line up with
/// `settings-category` in the UI, and so a future indexed section in
/// either one has an obvious home.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    General = 0,
    Appearance = 1,
    Audio = 2,
    Keybinds = 3,
    Profile = 4,
    Call = 5,
}

impl Category {
    /// Whether a query matches anything that is always on screen in this
    /// category, so the caller knows to keep that fixed block visible
    /// while a search is active.
    pub fn has_always_visible_match(self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        INDEX
            .iter()
            .any(|e| e.category == self && e.always_visible && matches(e, &query))
    }

    pub fn label(self) -> &'static str {
        match self {
            Category::General => "General",
            Category::Appearance => "Appearance",
            Category::Audio => "Audio",
            Category::Keybinds => "Keybinds",
            Category::Profile => "Profile",
            Category::Call => "Call",
        }
    }
}

/// One searchable settings section.
pub struct Entry {
    pub category: Category,
    pub id: &'static str,
    pub title: &'static str,
    /// Words that should find this section but don't appear in its title.
    pub keywords: &'static str,
    /// True for UI that is always on screen in its category rather than
    /// living in a collapsible, reorderable section -- the mic level meter
    /// and mic test block at the top of the Audio tab. Such an entry can
    /// still be *found* (the summary points at its category) but there is
    /// no section id to filter down to.
    pub always_visible: bool,
}

pub const INDEX: &[Entry] = &[
    // Audio
    Entry { category: Category::Audio, id: "presets", title: "Voice presets", keywords: "preset chain save export import share", always_visible: false },
    Entry { category: Category::Audio, id: "miclevel", title: "Mic level", keywords: "meter input volume gain loudness", always_visible: true },
    Entry { category: Category::Audio, id: "loopback", title: "Live loopback", keywords: "mic test monitor hear myself sidetone check echo preview", always_visible: true },
    Entry { category: Category::Audio, id: "callcontrols", title: "Transmit and monitoring", keywords: "push to talk ptt voice activation open mic monitor sidetone hear myself", always_visible: false },
    Entry { category: Category::Audio, id: "aec", title: "Echo Cancellation", keywords: "echo cancel feedback speakers headphones hear myself aec loop", always_visible: false },
    Entry { category: Category::Audio, id: "denoise", title: "Noise Suppression", keywords: "background fan hiss hum air conditioner denoise suppress strength thin", always_visible: false },
    Entry { category: Category::Audio, id: "gate", title: "Noise Gate", keywords: "background noise keyboard fan chatter threshold attack hold release", always_visible: false },
    Entry { category: Category::Audio, id: "hpf", title: "High-pass filter", keywords: "rumble low bass footsteps desk bump breath cutoff", always_visible: false },
    Entry { category: Category::Audio, id: "lpf", title: "Low-pass filter", keywords: "hiss high treble sharp static cheap mic cutoff", always_visible: false },
    Entry { category: Category::Audio, id: "deesser", title: "De-esser", keywords: "sibilance harsh s sounds t sounds lisp", always_visible: false },
    Entry { category: Category::Audio, id: "eq", title: "Parametric EQ", keywords: "equalizer tone bands frequency boominess clarity curve", always_visible: false },
    Entry { category: Category::Audio, id: "comp", title: "Compressor", keywords: "dynamics even volume quiet loud ratio threshold makeup", always_visible: false },
    Entry { category: Category::Audio, id: "limiter", title: "Limiter", keywords: "ceiling peak loud protection clipping", always_visible: false },
    Entry { category: Category::Audio, id: "pitch", title: "Pitch shift", keywords: "voice changer semitones formant chipmunk deep", always_visible: false },
    Entry { category: Category::Audio, id: "reverb", title: "Reverb", keywords: "room space echo hall damping wet dry", always_visible: false },
    // Appearance
    Entry { category: Category::Appearance, id: "accent", title: "Accent color", keywords: "colour highlight theme hue palette", always_visible: false },
    Entry { category: Category::Appearance, id: "theme", title: "Theme", keywords: "dark light background wallpaper image colour opacity transparent", always_visible: false },
    Entry { category: Category::Appearance, id: "textmotion", title: "Text, font and motion", keywords: "font size scale dyslexia animation reduced motion accessibility", always_visible: false },
    Entry { category: Category::Appearance, id: "importexport", title: "Import and export settings", keywords: "backup restore share config file", always_visible: false },
    // Profile
    Entry { category: Category::Profile, id: "profile", title: "Your profile", keywords: "name status avatar picture display identity id key fingerprint recovery phrase", always_visible: false },
    Entry { category: Category::Profile, id: "about", title: "About", keywords: "version licence credits", always_visible: false },
    // Call
    Entry { category: Category::Call, id: "callstart", title: "Starting a call", keywords: "screen share camera video mute join defaults", always_visible: false },
    Entry { category: Category::Call, id: "notifications", title: "Ringing and quiet hours", keywords: "notification ring silent do not disturb dnd schedule night sound alert", always_visible: false },
    Entry { category: Category::Call, id: "soundboard", title: "Soundboard", keywords: "sound clips wav effects meme play audio", always_visible: false },
    Entry { category: Category::Call, id: "transferlimits", title: "File transfer speed", keywords: "bandwidth upload cap throttle limit slow data", always_visible: false },
    Entry { category: Category::Call, id: "calladvanced", title: "Knock instead of ringing", keywords: "silent knock quiet shift click ring notify attention", always_visible: false },
    Entry { category: Category::Call, id: "discovery", title: "Finding contacts", keywords: "dht discovery bootstrap lookup address changed moved ip directory community directories find people", always_visible: false },
    Entry { category: Category::Call, id: "captions", title: "Live captions", keywords: "captions subtitles transcribe transcript speech to text deaf hard of hearing accessibility whisper", always_visible: false },
    Entry { category: Category::Call, id: "sharedfiles", title: "Shared files", keywords: "seed seeding pin pinning share download files swarm torrent holders available offline", always_visible: false },
    Entry { category: Category::Call, id: "vpn", title: "VPN", keywords: "vpn wireguard tunnel privacy ip address hide kill switch route mullvad proton conf", always_visible: false },
    Entry { category: Category::Call, id: "outbox", title: "Undelivered messages", keywords: "outbox queue offline retry give up pending unsent delivery waiting", always_visible: false },
    Entry { category: Category::Call, id: "callparticipants", title: "Participants", keywords: "mute others volume per user disable people", always_visible: false },
    Entry { category: Category::Call, id: "calladvanced", title: "Advanced", keywords: "ip direct calling address host port relay privacy", always_visible: false },
];

/// Section ids in `category` matching `query`, in `order`'s order.
///
/// An empty or whitespace-only query matches everything, so the caller can
/// use the same path for "searching" and "not searching".
pub fn filter(order: &[String], category: Category, query: &str) -> Vec<String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return order.to_vec();
    }
    order
        .iter()
        .filter(|id| {
            INDEX
                .iter()
                .find(|e| e.category == category && e.id == id.as_str())
                .is_some_and(|e| matches(e, &query))
        })
        .cloned()
        .collect()
}

/// Every term in the query must appear somewhere in the entry, so adding
/// words narrows rather than widens -- "noise gate" shouldn't return every
/// section that mentions noise.
fn matches(entry: &Entry, query: &str) -> bool {
    let haystack = format!("{} {}", entry.title.to_lowercase(), entry.keywords);
    query.split_whitespace().all(|term| haystack.contains(term))
}

/// A one-line summary of where the matches are, for when a search hides
/// everything in the category the user happens to be looking at.
pub fn summary(query: &str) -> String {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return String::new();
    }
    let mut counts: Vec<(Category, usize)> = Vec::new();
    for entry in INDEX.iter().filter(|e| matches(e, &query)) {
        match counts.iter_mut().find(|(c, _)| *c == entry.category) {
            Some((_, n)) => *n += 1,
            None => counts.push((entry.category, 1)),
        }
    }
    if counts.is_empty() {
        return "No settings match that.".to_string();
    }
    let parts: Vec<String> = counts.iter().map(|(c, n)| format!("{n} in {}", c.label())).collect();
    format!("Matches: {}", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(ids: &[&str]) -> Vec<String> {
        ids.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn an_empty_query_matches_everything() {
        let all = order(&["gate", "hpf", "eq"]);
        assert_eq!(filter(&all, Category::Audio, ""), all);
        assert_eq!(filter(&all, Category::Audio, "   "), all);
    }

    #[test]
    fn a_title_word_finds_its_section() {
        let all = order(&["gate", "hpf", "eq"]);
        assert_eq!(filter(&all, Category::Audio, "gate"), order(&["gate"]));
    }

    #[test]
    fn a_keyword_finds_a_section_whose_title_does_not_contain_it() {
        // The point of keywords: nobody searches "de-esser" for the
        // problem they actually have.
        let all = order(&["gate", "deesser", "hpf"]);
        assert_eq!(filter(&all, Category::Audio, "sibilance"), order(&["deesser"]));
        assert_eq!(filter(&all, Category::Audio, "rumble"), order(&["hpf"]));
    }

    #[test]
    fn search_is_case_insensitive() {
        let all = order(&["gate"]);
        assert_eq!(filter(&all, Category::Audio, "GATE"), order(&["gate"]));
    }

    #[test]
    fn extra_terms_narrow_rather_than_widen() {
        let all = order(&["gate", "hpf", "lpf"]);
        // "cutoff" alone hits both filters; adding "hiss" should leave one.
        assert_eq!(filter(&all, Category::Audio, "cutoff").len(), 2);
        assert_eq!(filter(&all, Category::Audio, "cutoff hiss"), order(&["lpf"]));
    }

    #[test]
    fn filtering_preserves_the_users_section_order() {
        let all = order(&["eq", "gate", "comp"]);
        // Not the index's order -- the order the user arranged.
        assert_eq!(filter(&all, Category::Audio, "threshold"), order(&["gate", "comp"]));
    }

    #[test]
    fn a_query_matching_nothing_yields_nothing() {
        let all = order(&["gate", "hpf"]);
        assert!(filter(&all, Category::Audio, "wireguard").is_empty());
    }

    #[test]
    fn the_summary_points_at_the_categories_holding_matches() {
        // A user searching from the Audio tab needs to be told the match
        // is somewhere else, or the search just looks broken.
        let found = summary("ip direct");
        assert!(found.contains("Call"), "got {found:?}");
    }

    #[test]
    fn the_summary_says_so_when_nothing_matches() {
        assert_eq!(summary("nonsensequery"), "No settings match that.");
    }

    #[test]
    fn the_summary_is_empty_when_not_searching() {
        assert!(summary("").is_empty());
    }

    #[test]
    fn every_indexed_section_id_is_a_real_section() {
        // Guards against an index entry drifting away from the canonical
        // id lists, which would make a section permanently unfindable.
        for entry in INDEX {
            if entry.always_visible {
                continue;
            }
            let known = match entry.category {
                Category::Audio => crate::config::AUDIO_SECTION_IDS,
                Category::Appearance => crate::config::APPEARANCE_SECTION_IDS,
                Category::Profile => crate::config::PROFILE_SECTION_IDS,
                Category::Call => crate::config::CALL_SECTION_IDS,
                Category::General | Category::Keybinds => continue,
            };
            assert!(
                known.contains(&entry.id),
                "search index has unknown section {:?} in {:?}",
                entry.id,
                entry.category
            );
        }
    }

    #[test]
    fn every_real_section_is_findable() {
        for (ids, category) in [
            (crate::config::AUDIO_SECTION_IDS, Category::Audio),
            (crate::config::APPEARANCE_SECTION_IDS, Category::Appearance),
            (crate::config::PROFILE_SECTION_IDS, Category::Profile),
            (crate::config::CALL_SECTION_IDS, Category::Call),
        ] {
            for id in ids {
                assert!(
                    INDEX.iter().any(|e| e.category == category && e.id == *id && !e.always_visible),
                    "section {id:?} in {category:?} is not in the search index"
                );
            }
        }
    }
}
