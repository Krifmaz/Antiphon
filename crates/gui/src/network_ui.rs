//! The always-visible connection line and the relay actually in use
//! (spec §6 "Reachability status", §3 "Project-hosted relays exist as
//! fallback").
//!
//! Kept out of `main.rs` so the start-up checks and the fallback choice
//! live in one place: what is shown in the header and which relay the
//! call code uses are worked out together and can never disagree.

use std::sync::Arc;

use slint::ComponentHandle;

use crate::fallback_relay;
use crate::reachability::{self, Status, StatusInputs};
use crate::state::AppState;
use crate::MainWindow;

/// The relay calls and listening use: the first of yours and the
/// fallbacks that answered when the network was last checked, or yours
/// unchecked if none did. Empty for none.
pub fn effective_relay(app: &MainWindow) -> String {
    let active = app.get_active_relay().trim().to_string();
    if !active.is_empty() {
        return active;
    }
    app.get_relay_server().trim().to_string()
}

/// Wires the header line's re-check and runs the first check.
pub fn install(app: &MainWindow, state: &Arc<AppState>) {
    let weak = app.as_weak();
    let st = state.clone();
    app.on_recheck_network(move || {
        if let Some(app) = weak.upgrade() {
            recheck(&app, &st);
        }
    });
    recheck(app, state);
}

/// Runs the checks off the UI thread: the VPN route, two STUN answers,
/// and -- only with no relay of your own -- which fallback is up.
pub fn recheck(app: &MainWindow, state: &Arc<AppState>) {
    app.set_network_status(Status::Checking.label().into());
    app.set_network_status_explanation(Status::Checking.explain().into());
    app.set_network_status_ok(true);

    let own = app.get_relay_server().trim().to_string();
    let fallbacks = fallback_relay::parse_list(&app.get_fallback_relays_text());
    let use_project = app.get_use_project_relays();
    let reach = crate::current_reachability(app);
    let weak = app.as_weak();
    state.runtime.spawn(async move {
        let vpn_active = tokio::task::spawn_blocking(reachability::default_route_via_vpn)
            .await
            .ok()
            .flatten()
            .unwrap_or(false);
        let symmetric_nat = match tokio::net::UdpSocket::bind(("0.0.0.0", 0)).await {
            Ok(socket) => antiphon_net::stun::discover_mapping(&socket)
                .await
                .ok()
                .map(|(_, m)| m == antiphon_net::stun::NatMapping::EndpointDependent),
            Err(_) => None,
        };
        let candidates = fallback_relay::candidates(&own, &fallbacks, use_project);
        let active = fallback_relay::first_live(&candidates).await;

        let inputs = StatusInputs {
            reach: reachability::Reachability {
                public_address_known: reach.public_address_known || symmetric_nat.is_some(),
                ..reach
            },
            symmetric_nat,
            vpn_active,
            relay_available: active.is_some() || !own.is_empty(),
        };
        let status = Status::from_inputs(&inputs).settled();
        slint::invoke_from_event_loop(move || {
            let Some(app) = weak.upgrade() else { return };
            app.set_active_relay(active.unwrap_or_default().into());
            app.set_network_status(status.label().into());
            app.set_network_status_explanation(status.explain().into());
            app.set_network_status_ok(status.is_ok());
        })
        .ok();
    });
}
