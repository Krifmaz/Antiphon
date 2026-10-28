//! What this machine's network can and cannot do (spec §10's
//! port-forwarding capability table, and §3's "active mode visible in the
//! call UI with a one-line explanation").
//!
//! Two questions people actually have, which the app previously answered
//! only in fragments scattered across a status line:
//!
//! - **Can I call out?** Almost always yes. An outbound connection is
//!   what every NAT is built to allow.
//! - **Can anyone call me?** That depends on the router, and it is the
//!   question a peer-to-peer app with no relay lives or dies on.
//!
//! The rule followed throughout: never say "unknown" where "not checked
//! yet" is the truth. They lead to different actions -- one is a network
//! problem, the other is a button nobody has pressed -- and collapsing
//! them into one word sends people to debug a router that is fine.

/// Whether a check has been run, and what it found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Check {
    /// Nobody has asked yet.
    NotChecked,
    Yes,
    No,
}

/// Everything known about how reachable this machine is.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Reachability {
    /// A STUN server answered with a public address.
    pub public_address_known: bool,
    /// The router agreed to forward a port inward.
    pub port_forwarded: bool,
    /// The router was asked and refused, as opposed to never asked.
    pub port_forward_refused: bool,
}

impl Reachability {
    /// Whether people can start a call *to* you.
    pub fn can_be_called(&self) -> Check {
        if self.port_forwarded {
            return Check::Yes;
        }
        if self.port_forward_refused {
            // A refusal is not proof nobody can reach you: hole punching
            // still works when both sides connect at once, and the port
            // may already be forwarded by hand. So this is "not by
            // itself", not "no".
            return Check::No;
        }
        Check::NotChecked
    }

    /// The capability table, as rows of (question, answer, what it means).
    ///
    /// Returned as data rather than rendered here so the wording lives in
    /// one place and can be tested, rather than being spread through a
    /// `.slint` file where nothing can check it.
    pub fn rows(&self) -> Vec<(&'static str, String, String)> {
        vec![
            (
                "Calling out",
                "Works".to_string(),
                "Connections you start are what every home router already allows."
                    .to_string(),
            ),
            (
                "Your public address",
                match self.public_address_known {
                    true => "Found".to_string(),
                    false => "Not checked".to_string(),
                },
                match self.public_address_known {
                    true => "This is the address to give someone so they can reach you."
                        .to_string(),
                    false => "Press Find to ask what the outside world sees you as."
                        .to_string(),
                },
            ),
            (
                "Being called",
                match self.can_be_called() {
                    Check::Yes => "Works".to_string(),
                    Check::No => "Needs a hand".to_string(),
                    Check::NotChecked => "Not checked".to_string(),
                },
                match self.can_be_called() {
                    Check::Yes => {
                        "Your router is forwarding a port inward and renewing it while the \
                         app is open."
                            .to_string()
                    }
                    Check::No => {
                        "Your router would not open a port automatically. You can still \
                         call out, still be called if you forward a port by hand, and \
                         still connect if you both press Punch at the same time."
                            .to_string()
                    }
                    Check::NotChecked => {
                        "Press \"Open my port on the router\" to find out.".to_string()
                    }
                },
            ),
        ]
    }
}

/// How a call is currently carried (spec §3).
///
/// Anonymity mode (multi-hop) is deferred by the spec itself (§11), so it
/// has no variant until it exists.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Direct,
    Relayed,
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Direct => "Direct",
            Mode::Relayed => "Relayed",
        }
    }

    /// The one line the spec asks for.
    pub fn explain(&self) -> &'static str {
        match self {
            Mode::Direct => {
                "Connected straight to them, with nothing in between -- which also means \
                 you can each see the other's IP address."
            }
            Mode::Relayed => {
                "Going through a relay that passes along encrypted data -- neither of you \
                 learns the other's IP address, and the relay can't read or hear anything. \
                 Slightly slower."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_checked_yet_is_not_the_same_as_a_problem() {
        // The distinction the whole module turns on: one is a router to
        // debug, the other is a button nobody pressed.
        let fresh = Reachability::default();
        assert_eq!(fresh.can_be_called(), Check::NotChecked);
        let refused = Reachability { port_forward_refused: true, ..Default::default() };
        assert_eq!(refused.can_be_called(), Check::No);
    }

    #[test]
    fn a_forwarded_port_means_people_can_call_you() {
        let mapped = Reachability { port_forwarded: true, ..Default::default() };
        assert_eq!(mapped.can_be_called(), Check::Yes);
    }

    #[test]
    fn a_refusal_still_leaves_the_ways_that_do_work() {
        // "No" here must not read as "you cannot be reached", because
        // hole punching and a hand-forwarded port both still work. A
        // status line that overstates the problem sends people to
        // reconfigure a router that was fine.
        let refused = Reachability { port_forward_refused: true, ..Default::default() };
        let (_, answer, meaning) = refused.rows().into_iter().nth(2).unwrap();
        assert_eq!(answer, "Needs a hand");
        assert!(meaning.contains("call out"), "{meaning}");
        assert!(meaning.contains("by hand"), "{meaning}");
        assert!(meaning.contains("Punch"), "{meaning}");
    }

    #[test]
    fn every_row_says_what_to_do_or_what_it_means() {
        for state in [
            Reachability::default(),
            Reachability { public_address_known: true, ..Default::default() },
            Reachability { port_forwarded: true, public_address_known: true, ..Default::default() },
            Reachability { port_forward_refused: true, ..Default::default() },
        ] {
            for (question, answer, meaning) in state.rows() {
                assert!(!answer.is_empty(), "{question} has no answer");
                assert!(meaning.len() > 30, "{question} explains nothing: {meaning}");
            }
        }
    }

    #[test]
    fn calling_out_is_never_reported_as_a_problem() {
        // It is the one thing that works regardless, and hedging it would
        // make the whole table read as broken.
        for state in [
            Reachability::default(),
            Reachability { port_forward_refused: true, ..Default::default() },
        ] {
            assert_eq!(state.rows()[0].1, "Works");
        }
    }

    #[test]
    fn an_unchecked_row_points_at_the_button_that_checks_it() {
        let fresh = Reachability::default();
        assert!(fresh.rows()[1].2.contains("Find"), "{}", fresh.rows()[1].2);
        assert!(fresh.rows()[2].2.contains("Open my port"), "{}", fresh.rows()[2].2);
    }

    #[test]
    fn the_active_mode_explains_what_it_costs_as_well_as_what_it_is() {
        // A direct connection is the fastest and most private option, and
        // it also shows both sides each other's address. Saying only the
        // first half would be selling it rather than explaining it.
        let explanation = Mode::Direct.explain();
        assert!(explanation.contains("nothing in between"), "{explanation}");
        assert!(explanation.contains("IP address"), "{explanation}");
        assert_eq!(Mode::Direct.label(), "Direct");
    }
}
