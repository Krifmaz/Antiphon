//! How your own profile page is arranged: which cards it shows, in what
//! order, and where (if anywhere) your invite's QR code sits on it.
//!
//! The arrangement is yours alone. It changes the profile page this
//! device draws for you (the one you might hold up for a friend to scan
//! or share on a screen), not the card your contacts receive, so nothing
//! about it is ever sent.
//!
//! It is saved as two plain values, so the config files of both apps
//! keep it without depending on this crate:
//!
//! - the cards as a list of ids in order, a hidden one written with a
//!   leading `-` (`["about", "-door", "invite", "qr", "fingerprint"]`);
//! - the QR code's place as one id (`""`, `"banner"`, `"name"`, `"card"`).
//!
//! Reading either back is forgiving: unknown ids are dropped, repeats
//! keep their first place, and a card missing from the list (one added
//! by a newer version, say) is shown at the end, so an old or edited
//! file always gives a whole layout.

/// One card on your profile page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProfileBlock {
    /// Your About line.
    About,
    /// Whether your door is open, with its switch.
    Door,
    /// Your invite link, with the button that shows it.
    Invite,
    /// Your invite as a QR code. Shown only while [`QrPlace::Card`] is
    /// chosen; its place in the order is where that card goes.
    Qr,
    /// Your key's fingerprint, for reading aloud.
    Fingerprint,
}

impl ProfileBlock {
    /// Every card, in the order a new profile shows them.
    pub const ALL: [ProfileBlock; 5] = [ProfileBlock::About, ProfileBlock::Door, ProfileBlock::Invite, ProfileBlock::Qr, ProfileBlock::Fingerprint];

    /// The id the card is saved under.
    pub fn id(self) -> &'static str {
        match self {
            ProfileBlock::About => "about",
            ProfileBlock::Door => "door",
            ProfileBlock::Invite => "invite",
            ProfileBlock::Qr => "qr",
            ProfileBlock::Fingerprint => "fingerprint",
        }
    }

    /// The card with this saved id, if there is one.
    pub fn from_id(id: &str) -> Option<ProfileBlock> {
        ProfileBlock::ALL.into_iter().find(|b| b.id() == id)
    }

    /// The card's heading, as the page and the editor show it.
    pub fn title(self) -> &'static str {
        match self {
            ProfileBlock::About => "About me",
            ProfileBlock::Door => "Your door",
            ProfileBlock::Invite => "Your invite",
            ProfileBlock::Qr => "QR code",
            ProfileBlock::Fingerprint => "Fingerprint",
        }
    }
}

/// Where your invite's QR code is drawn on your profile page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum QrPlace {
    /// Nowhere. The default: a code that identifies you should be on
    /// screen because you put it there, not because a profile happened
    /// to be open while you shared your screen.
    #[default]
    Off,
    /// Small, in the banner's corner.
    Banner,
    /// Beside your name, under the banner.
    Name,
    /// As a card of its own, wherever [`ProfileBlock::Qr`] is in the order.
    Card,
}

impl QrPlace {
    /// Every place, in the order the editor offers them.
    pub const ALL: [QrPlace; 4] = [QrPlace::Off, QrPlace::Banner, QrPlace::Name, QrPlace::Card];

    /// The id the place is saved under; empty for [`QrPlace::Off`].
    pub fn id(self) -> &'static str {
        match self {
            QrPlace::Off => "",
            QrPlace::Banner => "banner",
            QrPlace::Name => "name",
            QrPlace::Card => "card",
        }
    }

    /// The place with this saved id; anything unknown is [`QrPlace::Off`],
    /// the safe reading.
    pub fn from_id(id: &str) -> QrPlace {
        QrPlace::ALL.into_iter().find(|q| q.id() == id).unwrap_or_default()
    }

    /// What the editor calls it.
    pub fn label(self) -> &'static str {
        match self {
            QrPlace::Off => "Hidden",
            QrPlace::Banner => "On the banner",
            QrPlace::Name => "By my name",
            QrPlace::Card => "As a card",
        }
    }
}

/// Your profile page's arrangement: its cards in order, which are hidden,
/// and where the QR code goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileLayout {
    /// Every card exactly once, in the order the page shows them, each
    /// with whether it is shown.
    pub blocks: Vec<(ProfileBlock, bool)>,
    /// Where the QR code is drawn.
    pub qr: QrPlace,
}

impl Default for ProfileLayout {
    fn default() -> Self {
        Self { blocks: ProfileBlock::ALL.into_iter().map(|b| (b, true)).collect(), qr: QrPlace::Off }
    }
}

impl ProfileLayout {
    /// Reads a layout back from its saved form (see the module notes):
    /// `blocks` the card ids in order, `-` before a hidden one; `qr` the
    /// QR code's place.
    pub fn from_saved(blocks: &[String], qr: &str) -> Self {
        let mut out: Vec<(ProfileBlock, bool)> = Vec::with_capacity(ProfileBlock::ALL.len());
        for entry in blocks {
            let (id, shown) = match entry.strip_prefix('-') {
                Some(id) => (id, false),
                None => (entry.as_str(), true),
            };
            // Unknown ids are dropped and repeats keep their first place.
            if let Some(block) = ProfileBlock::from_id(id) {
                if !out.iter().any(|(b, _)| *b == block) {
                    out.push((block, shown));
                }
            }
        }
        // A card the saved list doesn't name still belongs on the page.
        for block in ProfileBlock::ALL {
            if !out.iter().any(|(b, _)| *b == block) {
                out.push((block, true));
            }
        }
        Self { blocks: out, qr: QrPlace::from_id(qr) }
    }

    /// The cards' saved form: ids in order, `-` before a hidden one.
    pub fn saved_blocks(&self) -> Vec<String> {
        self.blocks.iter().map(|(b, shown)| if *shown { b.id().to_string() } else { format!("-{}", b.id()) }).collect()
    }

    /// The cards the page draws, in order. The QR card is in it only when
    /// the code is placed as a card (and then always, since choosing that
    /// place is how it is shown).
    pub fn visible(&self) -> impl Iterator<Item = ProfileBlock> + '_ {
        self.blocks.iter().filter(move |(b, shown)| if *b == ProfileBlock::Qr { self.qr == QrPlace::Card } else { *shown }).map(|(b, _)| *b)
    }

    /// This layout with `block` moved one place earlier (`up`) or later;
    /// unchanged at either end.
    pub fn moved(&self, block: ProfileBlock, up: bool) -> Self {
        let mut next = self.clone();
        if let Some(i) = next.blocks.iter().position(|(b, _)| *b == block) {
            let j = if up { i.checked_sub(1) } else { Some(i + 1).filter(|j| *j < next.blocks.len()) };
            if let Some(j) = j {
                next.blocks.swap(i, j);
            }
        }
        next
    }

    /// This layout with `block` shown or hidden.
    pub fn with_shown(&self, block: ProfileBlock, shown: bool) -> Self {
        let mut next = self.clone();
        if let Some(entry) = next.blocks.iter_mut().find(|(b, _)| *b == block) {
            entry.1 = shown;
        }
        next
    }

    /// This layout with the QR code at `qr`.
    pub fn with_qr(&self, qr: QrPlace) -> Self {
        Self { qr, ..self.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layout_survives_being_saved_and_read_back() {
        let layout = ProfileLayout::default().moved(ProfileBlock::Fingerprint, true).with_shown(ProfileBlock::Door, false).with_qr(QrPlace::Banner);
        let back = ProfileLayout::from_saved(&layout.saved_blocks(), layout.qr.id());
        assert_eq!(back, layout);
    }

    #[test]
    fn nothing_saved_reads_as_the_default() {
        assert_eq!(ProfileLayout::from_saved(&[], ""), ProfileLayout::default());
    }

    #[test]
    fn unknown_repeated_and_missing_cards_still_give_a_whole_layout() {
        let saved: Vec<String> = ["fingerprint", "banana", "-about", "fingerprint"].iter().map(ToString::to_string).collect();
        let layout = ProfileLayout::from_saved(&saved, "somewhere-new");
        assert_eq!(layout.blocks.len(), ProfileBlock::ALL.len());
        assert_eq!(layout.blocks[0], (ProfileBlock::Fingerprint, true));
        assert_eq!(layout.blocks[1], (ProfileBlock::About, false));
        assert_eq!(layout.qr, QrPlace::Off, "an unknown place is the safe one");
    }

    #[test]
    fn the_qr_card_shows_only_when_the_code_is_placed_as_a_card() {
        let layout = ProfileLayout::default();
        assert!(!layout.visible().any(|b| b == ProfileBlock::Qr));
        assert!(!layout.with_qr(QrPlace::Banner).visible().any(|b| b == ProfileBlock::Qr));
        assert!(layout.with_qr(QrPlace::Card).visible().any(|b| b == ProfileBlock::Qr));
    }

    #[test]
    fn moving_stops_at_the_ends() {
        let layout = ProfileLayout::default();
        assert_eq!(layout.moved(ProfileBlock::About, true), layout);
        assert_eq!(layout.moved(ProfileBlock::Fingerprint, false), layout);
        assert_eq!(layout.moved(ProfileBlock::Door, true).blocks[0].0, ProfileBlock::Door);
    }

    #[test]
    fn a_hidden_card_is_left_off_the_page() {
        let layout = ProfileLayout::default().with_shown(ProfileBlock::Door, false);
        assert!(!layout.visible().any(|b| b == ProfileBlock::Door));
    }
}
