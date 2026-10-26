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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Direct,
    Relayed,
    /// Through several relays, each knowing only the step before and after.
    Anonymous,
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Direct => "Direct",
            Mode::Relayed => "Relayed",
            Mode::Anonymous => "Anonymous",
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
            Mode::Anonymous => {
                "Going through several relays, each one only knowing the step before and \
                 after it -- the strongest privacy available, at the cost of noticeable delay."
            }
        }
    }
}

/// The always-visible reachability line (spec §6: "always visible, never
/// discovered through failure: Direct · Relayed — VPN doesn't allow
/// incoming connections · Relayed — network blocks direct connections").
///
/// Worked out *before* anyone calls, from checks the app runs on its own at
/// start-up, so the first time someone learns their network blocks direct
/// connections is not a call that failed to connect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// The start-up checks have not finished.
    Checking,
    /// They finished without an answer: no STUN server replied, so the
    /// network may be offline or blocking UDP.
    Unchecked,
    /// People can reach you straight, and you them.
    Direct,
    /// A VPN is carrying your traffic, and VPNs don't forward incoming
    /// connections, so calls go through a relay.
    RelayedVpn,
    /// Your network blocks direct connections (a symmetric NAT, or a
    /// router that refused to open a port), so calls go through a relay.
    RelayedBlocked,
    /// Direct connections are blocked *and* there is no relay to fall back
    /// on. Calls out to people who can be reached still work.
    Blocked,
}

/// What [`Status`] is worked out from.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StatusInputs {
    pub reach: Reachability,
    /// `Some(true)` for an endpoint-dependent (symmetric) NAT, from asking
    /// two STUN servers; `None` while unknown.
    pub symmetric_nat: Option<bool>,
    /// The default route goes through a VPN interface.
    pub vpn_active: bool,
    /// A relay is set, or a fallback relay answered.
    pub relay_available: bool,
}

impl Status {
    pub fn from_inputs(i: &StatusInputs) -> Status {
        let blocked_reason = if i.vpn_active {
            Some(Status::RelayedVpn)
        } else if i.symmetric_nat == Some(true) {
            Some(Status::RelayedBlocked)
        } else if i.symmetric_nat.is_none() && !i.reach.public_address_known {
            // Nothing answered yet: saying "Direct" here would be the very
            // optimism the spec forbids.
            return Status::Checking;
        } else {
            None
        };
        match blocked_reason {
            None => Status::Direct,
            Some(_) if !i.relay_available => Status::Blocked,
            Some(reason) => reason,
        }
    }

    /// The line itself, in the spec's words.
    pub fn label(&self) -> &'static str {
        match self {
            Status::Checking => "Checking your connection...",
            Status::Unchecked => "Couldn't check -- click to try again",
            Status::Direct => "Direct",
            Status::RelayedVpn => "Relayed — VPN doesn't allow incoming connections",
            Status::RelayedBlocked => "Relayed — network blocks direct connections",
            Status::Blocked => "Blocked — network blocks direct connections and no relay is set",
        }
    }

    /// One sentence on what it means and what to do about it.
    pub fn explain(&self) -> &'static str {
        match self {
            Status::Checking => "Asking two STUN servers how your network maps connections.",
            Status::Unchecked => {
                "No STUN server answered, so whether people can reach you is unknown. You may \
                 be offline, or your network may block UDP."
            }
            Status::Direct => {
                "Calls connect straight to the other person. Contacts you haven't verified \
                 still go through a relay unless you say otherwise."
            }
            Status::RelayedVpn => {
                "Your VPN carries everything, and VPNs don't pass incoming connections, so \
                 calls go through a relay. The relay can't read or hear anything."
            }
            Status::RelayedBlocked => {
                "Your network gives each destination a different port, so direct connections \
                 can't be set up. Calls go through a relay, which can't read or hear anything."
            }
            Status::Blocked => {
                "Direct connections can't be set up from this network. Add a relay in \
                 Settings > Call so people can reach you."
            }
        }
    }

    /// What a finished check shows: never "still checking".
    pub fn settled(self) -> Status {
        if self == Status::Checking { Status::Unchecked } else { self }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Status::Direct | Status::RelayedVpn | Status::RelayedBlocked)
    }
}

/// Whether this machine's default route goes through a VPN interface.
///
/// Read from the routing table where that's a file (Linux) and from
/// `route` where it's a command (macOS); `None` where neither is
/// available, which callers treat as "no VPN" rather than guessing.
pub fn default_route_via_vpn() -> Option<bool> {
    default_route_interface().map(|name| is_vpn_interface(&name))
}

fn default_route_interface() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let table = std::fs::read_to_string("/proc/net/route").ok()?;
        return default_interface_from_proc_route(&table);
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("route").args(["-n", "get", "default"]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        return text
            .lines()
            .find_map(|l| l.trim().strip_prefix("interface:").map(|s| s.trim().to_string()));
    }
    #[allow(unreachable_code)]
    None
}

/// The interface of the `00000000` destination with the lowest metric.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn default_interface_from_proc_route(table: &str) -> Option<String> {
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() > 6 && f[1] == "00000000").then(|| (f[6].parse::<u32>().unwrap_or(u32::MAX), f[0].to_string()))
        })
        .min()
        .map(|(_, name)| name)
}

/// WireGuard, OpenVPN/tun, macOS utun, PPP and IPsec interfaces.
pub fn is_vpn_interface(name: &str) -> bool {
    ["wg", "tun", "tap", "utun", "ppp", "ipsec", "nordlynx", "proton", "mullvad"]
        .iter()
        .any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(symmetric: Option<bool>, vpn: bool, relay: bool) -> StatusInputs {
        StatusInputs {
            reach: Reachability { public_address_known: symmetric.is_some(), ..Default::default() },
            symmetric_nat: symmetric,
            vpn_active: vpn,
            relay_available: relay,
        }
    }

    #[test]
    fn the_status_line_uses_the_spec_wording() {
        assert_eq!(Status::from_inputs(&inputs(Some(false), false, false)).label(), "Direct");
        assert_eq!(
            Status::from_inputs(&inputs(Some(false), true, true)).label(),
            "Relayed — VPN doesn't allow incoming connections"
        );
        assert_eq!(
            Status::from_inputs(&inputs(Some(true), false, true)).label(),
            "Relayed — network blocks direct connections"
        );
    }

    #[test]
    fn nothing_known_is_checking_not_direct() {
        assert_eq!(Status::from_inputs(&inputs(None, false, true)), Status::Checking);
    }

    #[test]
    fn blocked_with_no_relay_says_so_and_points_at_settings() {
        let s = Status::from_inputs(&inputs(Some(true), false, false));
        assert_eq!(s, Status::Blocked);
        assert!(!s.is_ok());
        assert!(s.explain().contains("Settings"));
    }

    #[test]
    fn a_vpn_is_relayed_even_before_stun_answers() {
        assert_eq!(Status::from_inputs(&inputs(None, true, true)), Status::RelayedVpn);
    }

    #[test]
    fn the_default_route_is_read_from_the_routing_table() {
        let table = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
                     eth0\t00000000\t0102A8C0\t0003\t0\t0\t600\t00000000\n\
                     wg0\t00000000\t00000000\t0001\t0\t0\t50\t00000000\n\
                     eth0\t0002A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\n";
        assert_eq!(default_interface_from_proc_route(table).as_deref(), Some("wg0"));
        assert!(is_vpn_interface("wg0"));
        assert!(!is_vpn_interface("eth0"));
    }

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
