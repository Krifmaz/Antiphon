//! First run (spec §13: "with no account, setup means generating a
//! keypair. If that reads as confusing or scary, users are lost in the
//! first 60 seconds").
//!
//! The spec identifies the failure mode exactly, so this is built around
//! avoiding it rather than around explaining cryptography.
//!
//! The temptation with an accountless app is to open with the thing that
//! is *different* -- "your identity is a keypair, here are 24 words, write
//! them down" -- which lands as homework before the person has any reason
//! to care. Someone who has not made a call yet has nothing to lose, so a
//! backup prompt is asking them to protect something they do not value
//! yet.
//!
//! So the order here is deliberately: **it already worked** → what that
//! means → and only later, once there is something worth keeping, back it
//! up. The identity is generated silently before the first screen is ever
//! drawn, because the honest framing is that there was nothing to set up.
//!
//! This module owns the *decision* of what to show and when, which is the
//! part worth testing. The wording lives here too so it can be read in one
//! place rather than being scattered through the UI.

use serde::{Deserialize, Serialize};

/// What the app should be telling a new user right now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prompt {
    /// Nothing. Either they are past this or there is nothing to say.
    None,
    /// First launch: reassure that setup already happened.
    Welcome,
    /// They have a contact but have never verified anyone.
    SuggestVerify,
    /// They have something worth losing and no backup.
    SuggestBackup,
}

/// What the app knows about how far along someone is.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct FirstRunState {
    /// The welcome has been shown and dismissed.
    #[serde(default)]
    pub welcomed: bool,
    /// The recovery phrase has been revealed at least once.
    ///
    /// Not proof it was written down -- nothing can be -- but it is the
    /// difference between "never saw it" and "chose not to".
    #[serde(default)]
    pub phrase_seen: bool,
    /// The backup suggestion has been dismissed, so it stops asking.
    #[serde(default)]
    pub backup_prompt_dismissed: bool,
    /// The verification suggestion has been dismissed.
    #[serde(default)]
    pub verify_prompt_dismissed: bool,
}

/// Decides what to show.
///
/// `contacts` and `verified_contacts` are what make the later prompts
/// earn their place: nothing is suggested until the user has something the
/// suggestion would actually protect.
pub fn next_prompt(
    state: FirstRunState,
    contacts: usize,
    verified_contacts: usize,
    calls_completed: u32,
) -> Prompt {
    if !state.welcomed {
        return Prompt::Welcome;
    }

    // Only once there is something to lose. Asking someone to write down
    // 24 words before they have a single contact is asking them to protect
    // an empty account, and it teaches them the prompt is noise.
    if !state.phrase_seen && !state.backup_prompt_dismissed && contacts > 0 && calls_completed > 0 {
        return Prompt::SuggestBackup;
    }

    // Verification matters once there is somebody to verify *against*, and
    // it is worth raising before the conversation is old enough that a
    // swapped key would go unnoticed.
    if contacts > 0 && verified_contacts == 0 && !state.verify_prompt_dismissed {
        return Prompt::SuggestVerify;
    }

    Prompt::None
}

/// The heading for a prompt.
pub fn title(prompt: Prompt) -> &'static str {
    match prompt {
        Prompt::None => "",
        Prompt::Welcome => "You're ready to go",
        Prompt::SuggestVerify => "Check it's really them",
        Prompt::SuggestBackup => "Save a way back in",
    }
}

/// The body text.
///
/// Written to be read by someone who does not know or care what a keypair
/// is. No jargon, and specifically no reassurance-by-cryptography -- "your
/// keys are secured with X25519" tells a worried person nothing except
/// that there is something to worry about.
pub fn body(prompt: Prompt) -> &'static str {
    match prompt {
        Prompt::None => "",
        Prompt::Welcome => {
            "There's no account to make and nothing to sign up for -- this app already \
             set itself up. Share your invite link with someone and you can call them. \
             Everything stays on your machine."
        }
        Prompt::SuggestVerify => {
            "On a call, you'll each see the same short code. Read it aloud once and \
             check it matches. If it does, nobody is listening in -- and you never have \
             to check again."
        }
        Prompt::SuggestBackup => {
            "You've got contacts now. If this machine dies, twenty-four words are what \
             bring you back as the same person -- otherwise your contacts see a stranger. \
             It takes a minute, in Settings > Profile."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRESH: FirstRunState = FirstRunState {
        welcomed: false,
        phrase_seen: false,
        backup_prompt_dismissed: false,
        verify_prompt_dismissed: false,
    };

    fn welcomed() -> FirstRunState {
        FirstRunState { welcomed: true, ..FRESH }
    }

    #[test]
    fn a_brand_new_install_is_welcomed_first() {
        assert_eq!(next_prompt(FRESH, 0, 0, 0), Prompt::Welcome);
    }

    #[test]
    fn nothing_is_asked_of_someone_who_has_not_used_the_app_yet() {
        // The whole point: no homework before there is a reason to care.
        // No contacts, no calls -- so no backup nag and no verify nag.
        assert_eq!(next_prompt(welcomed(), 0, 0, 0), Prompt::None);
    }

    #[test]
    fn backup_is_suggested_only_once_there_is_something_to_lose() {
        // A contact but no call yet: still nothing worth protecting.
        assert_ne!(next_prompt(welcomed(), 1, 0, 0), Prompt::SuggestBackup);
        // A contact and a completed call: now it means something.
        assert_eq!(next_prompt(welcomed(), 1, 0, 1), Prompt::SuggestBackup);
    }

    #[test]
    fn backup_stops_being_suggested_once_the_phrase_has_been_seen() {
        let state = FirstRunState { welcomed: true, phrase_seen: true, ..FRESH };
        assert_ne!(next_prompt(state, 1, 0, 1), Prompt::SuggestBackup);
    }

    #[test]
    fn a_dismissed_backup_prompt_stays_dismissed() {
        // Nagging someone who already said no is how prompts get ignored
        // wholesale, including the ones that matter.
        let state = FirstRunState { welcomed: true, backup_prompt_dismissed: true, ..FRESH };
        assert_ne!(next_prompt(state, 5, 0, 10), Prompt::SuggestBackup);
    }

    #[test]
    fn verification_is_suggested_once_there_is_someone_to_verify() {
        let state = FirstRunState { welcomed: true, phrase_seen: true, ..FRESH };
        assert_eq!(next_prompt(state, 1, 0, 1), Prompt::SuggestVerify);
    }

    #[test]
    fn verification_stops_being_suggested_after_the_first_one() {
        let state = FirstRunState { welcomed: true, phrase_seen: true, ..FRESH };
        assert_eq!(next_prompt(state, 3, 1, 5), Prompt::None);
    }

    #[test]
    fn a_dismissed_verify_prompt_stays_dismissed() {
        let state = FirstRunState {
            welcomed: true,
            phrase_seen: true,
            verify_prompt_dismissed: true,
            ..FRESH
        };
        assert_eq!(next_prompt(state, 3, 0, 5), Prompt::None);
    }

    #[test]
    fn the_welcome_comes_before_anything_else() {
        // Even for someone who somehow has contacts already, the first
        // thing shown is the reassurance, not a task.
        assert_eq!(next_prompt(FRESH, 5, 0, 5), Prompt::Welcome);
    }

    #[test]
    fn every_prompt_has_something_to_say() {
        for prompt in [Prompt::Welcome, Prompt::SuggestVerify, Prompt::SuggestBackup] {
            assert!(!title(prompt).is_empty(), "{prompt:?} has no title");
            assert!(!body(prompt).is_empty(), "{prompt:?} has no body");
        }
        assert!(title(Prompt::None).is_empty());
        assert!(body(Prompt::None).is_empty());
    }

    #[test]
    fn the_wording_avoids_jargon() {
        // Someone lost in the first sixty seconds is not reassured by
        // being told which curve their keys are on.
        let jargon = [
            "keypair", "key pair", "public key", "private key", "X25519", "Ed25519",
            "cryptographic", "encryption", "hash",
        ];
        for prompt in [Prompt::Welcome, Prompt::SuggestVerify, Prompt::SuggestBackup] {
            let text = format!("{} {}", title(prompt), body(prompt)).to_lowercase();
            for word in jargon {
                assert!(
                    !text.contains(&word.to_lowercase()),
                    "{prompt:?} uses the word {word:?}: {text}"
                );
            }
        }
    }

    #[test]
    fn state_round_trips_through_json() {
        let state = FirstRunState {
            welcomed: true,
            phrase_seen: true,
            backup_prompt_dismissed: false,
            verify_prompt_dismissed: true,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<FirstRunState>(&json).unwrap(), state);
    }

    #[test]
    fn a_config_without_first_run_state_defaults_to_a_fresh_install() {
        // An existing user upgrading should not be treated as brand new
        // and re-welcomed... but defaulting to "not welcomed" is the safe
        // direction: a stray welcome is harmless, a skipped one is not.
        let state: FirstRunState = serde_json::from_str("{}").unwrap();
        assert_eq!(state, FRESH);
    }
}
