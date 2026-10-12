//! Recovery phrases (spec §3 "Backup & recovery", §12 open question 1).
//!
//! An identity here is a keypair the user holds, and there is no server to
//! ask for a password reset. That is the point -- nobody can lock you out,
//! and the same fact means nobody can let you back in. The only thing that
//! can restore an identity is a backup made in advance.
//!
//! This module is that backup in its most portable form: the 32-byte
//! master seed rendered as 24 BIP-39 words. Standard wordlist, standard
//! checksum, so a phrase can be written on paper, read aloud, or typed
//! into a different implementation.
//!
//! What a phrase does **not** cover, and what the UI must not imply it
//! does:
//!
//! - **Message history.** Restoring an identity gets you *who you are*,
//!   not what you said. History lives in an encrypted backup whose key is
//!   held by your existing devices (see [`crate::backup`]), and by design
//!   a phrase alone cannot open it -- that is the "you need another
//!   verified device" property.
//! - **Contacts' trust.** Contacts pinned your identity key; recovering
//!   the same key means they see no change at all. Recovering *without* a
//!   phrase means a new key and a key-change warning.
//!
//! Anyone holding the phrase is the user, completely. It is the one string
//! in the app that must never be logged, exported with settings, or sent
//! anywhere.

use bip39::Mnemonic;

use crate::error::CryptoError;

/// Renders a master seed as a 24-word recovery phrase.
pub fn phrase_from_seed(master_seed: &[u8; 32]) -> String {
    // 256 bits of entropy + an 8-bit checksum = 24 words. `from_entropy`
    // only fails on a length the standard doesn't define, and 32 bytes is
    // defined.
    Mnemonic::from_entropy(master_seed)
        .expect("32 bytes is a valid BIP-39 entropy length")
        .to_string()
}

/// Parses a recovery phrase back into a master seed.
///
/// Accepts any capitalisation and any run of whitespace between words,
/// because this gets typed by hand from paper. Everything else -- an
/// unknown word, the wrong number of words, a transposition that breaks
/// the checksum -- is rejected.
///
/// The checksum is what makes a typo an error rather than a silent
/// recovery of the wrong identity, which would look like "my account is
/// empty" instead of "you typed word 9 wrong".
pub fn seed_from_phrase(phrase: &str) -> Result<[u8; 32], CryptoError> {
    let normalized = phrase.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();

    let mnemonic = Mnemonic::parse_normalized(&normalized)
        .map_err(|_| CryptoError::InvalidRecoveryPhrase)?;

    let (entropy, len) = mnemonic.to_entropy_array();
    if len != 32 {
        // A valid 12-word phrase parses fine but only carries 16 bytes,
        // which is not an identity of ours.
        return Err(CryptoError::InvalidRecoveryPhrase);
    }

    let mut seed = [0u8; 32];
    seed.copy_from_slice(&entropy[..32]);
    Ok(seed)
}

/// How many words a valid phrase has. Exposed so the UI can say so before
/// the user finishes typing.
pub const PHRASE_WORDS: usize = 24;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::IdentityKeyPair;

    #[test]
    fn a_phrase_round_trips_back_to_the_same_seed() {
        let id = IdentityKeyPair::generate();
        let phrase = phrase_from_seed(&id.master_seed());

        assert_eq!(seed_from_phrase(&phrase).unwrap(), id.master_seed());
    }

    #[test]
    fn a_phrase_restores_the_same_public_identity() {
        let original = IdentityKeyPair::generate();
        let phrase = phrase_from_seed(&original.master_seed());

        let restored = IdentityKeyPair::from_master_seed(seed_from_phrase(&phrase).unwrap());

        // The whole promise of a recovery phrase: contacts who pinned the
        // old key see no change at all.
        assert_eq!(restored.public(), original.public());
    }

    #[test]
    fn a_phrase_is_twenty_four_words() {
        let id = IdentityKeyPair::generate();
        let phrase = phrase_from_seed(&id.master_seed());

        assert_eq!(phrase.split_whitespace().count(), PHRASE_WORDS);
    }

    #[test]
    fn messy_whitespace_and_capitalisation_are_tolerated() {
        let id = IdentityKeyPair::generate();
        let phrase = phrase_from_seed(&id.master_seed());
        let messy = format!("  {}  ", phrase.replace(' ', "\n   ").to_uppercase());

        // People type these off paper. Being strict about spacing would
        // fail recoveries that are actually correct.
        assert_eq!(seed_from_phrase(&messy).unwrap(), id.master_seed());
    }

    #[test]
    fn a_single_wrong_word_is_caught_by_the_checksum() {
        let id = IdentityKeyPair::generate();
        let phrase = phrase_from_seed(&id.master_seed());

        let mut words: Vec<&str> = phrase.split_whitespace().collect();
        // "zoo" is in the wordlist, so this is a valid word in an invalid
        // position -- exactly the mistake the checksum exists to catch,
        // and one that must not silently recover a different identity.
        let replacement = if words[8] == "zoo" { "zone" } else { "zoo" };
        words[8] = replacement;

        assert!(matches!(
            seed_from_phrase(&words.join(" ")),
            Err(CryptoError::InvalidRecoveryPhrase)
        ));
    }

    #[test]
    fn a_word_outside_the_list_is_rejected() {
        let id = IdentityKeyPair::generate();
        let phrase = phrase_from_seed(&id.master_seed());
        let mut words: Vec<&str> = phrase.split_whitespace().collect();
        words[0] = "notarealbip39word";

        assert!(seed_from_phrase(&words.join(" ")).is_err());
    }

    #[test]
    fn a_short_phrase_is_rejected_even_when_it_is_itself_valid() {
        // A real, checksum-valid 12-word phrase. It carries 128 bits, not
        // the 256 an identity needs, so accepting it would mean deriving
        // an identity from half the entropy the user thinks they have.
        let twelve = "legal winner thank year wave sausage worth useful legal winner thank yellow";
        assert!(Mnemonic::parse_normalized(twelve).is_ok(), "fixture should be a valid phrase");

        assert!(matches!(
            seed_from_phrase(twelve),
            Err(CryptoError::InvalidRecoveryPhrase)
        ));
    }

    #[test]
    fn empty_input_is_an_error_not_a_panic() {
        assert!(seed_from_phrase("").is_err());
        assert!(seed_from_phrase("   ").is_err());
    }

    #[test]
    fn different_identities_produce_different_phrases() {
        let a = phrase_from_seed(&IdentityKeyPair::generate().master_seed());
        let b = phrase_from_seed(&IdentityKeyPair::generate().master_seed());

        assert_ne!(a, b);
    }
}
