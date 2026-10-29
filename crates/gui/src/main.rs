mod animated_image;
use antiphon_client::caps_lock;
use antiphon_client::connection_mode;
use antiphon_client::group;
use antiphon_client::config;
use antiphon_client::crash_report;
use antiphon_client::data_export;
use antiphon_client::file_integrity;
use antiphon_client::file_preview;
use antiphon_client::first_run;
use antiphon_client::history;
use antiphon_client::introduction;
use antiphon_client::key_capture;
use antiphon_client::link_preview;
use antiphon_client::moderation;
use antiphon_client::notify;
use antiphon_client::presence;
use antiphon_client::reachability;
use antiphon_client::reactions;
mod qr;
use antiphon_client::search;
use antiphon_client::soundboard;
use antiphon_client::voice_message;
use antiphon_client::theme_presets;
use antiphon_client::transfer_limits;
use antiphon_client::control;
use antiphon_client::delivery;
use antiphon_client::sharing;
use antiphon_client::vpn;
use antiphon_client::discovery;
use antiphon_client::file_transfer;
use antiphon_client::state;
use antiphon_client::wav;
use antiphon_client::community;
mod captions;
use antiphon_client::anonymity;
use antiphon_client::fallback_relay;
use antiphon_client::sfu_call;
mod network_ui;

use std::cell::RefCell;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use device_query::Keycode;
use slint::fontique_010::fontique;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use antiphon_audio::{
    list_input_devices, list_output_devices, BandType, ChainConfig, CompressorConfig, DeEsserConfig, EqBandConfig, GainConfig,
    HighPassConfig, LimiterConfig, LowPassConfig, NoiseGateConfig, PitchShiftConfig, ReverbConfig,
};

use control::{ControlMessage, Hello};
use state::{AppState, TransmitSettings, MODE_OPEN_MIC, MODE_PTT, MODE_VOICE_ACTIVATION};

slint::include_modules!();

const DEFAULT_DEVICE_LABEL: &str = "System default";

/// Reads `--profile <name>` / `--profile=<name>` off the command line.
///
/// Hand-parsed rather than pulling in an argument crate: the GUI takes
/// exactly one flag, and `clap` would be a dependency and a startup cost
/// for a five-line loop.
fn profile_from_args() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--profile" {
            return args.next();
        }
        if let Some(rest) = arg.strip_prefix("--profile=") {
            return Some(rest.to_string());
        }
    }
    None
}

fn main() -> anyhow::Result<()> {
    // Must happen before anything reads config or the identity key, since
    // it decides which directory both come from.
    let profile = config::set_profile(profile_from_args().as_deref());

    // Spec §13 crash reporting. Installed early so a panic during startup
    // is caught too, and gated on the stored setting -- opting out means
    // the hook writes nothing, not that it writes and withholds.
    {
        let enabled = config::load().crash_reporting;
        let dir = config::config_dir();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(dir) = &dir {
                let message = info.payload().downcast_ref::<&str>().map(|s| s.to_string())
                    .or_else(|| info.payload().downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panicked".to_string());
                let location = info
                    .location()
                    .map(|l| format!("{}:{}", l.file(), l.line()))
                    .unwrap_or_default();
                let report = crash_report::build(
                    &message,
                    &location,
                    env!("CARGO_PKG_VERSION"),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                );
                crash_report::save_if_enabled(dir, enabled, &report);
            }
            previous(info);
        }));
    }

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let handle = runtime.handle().clone();
    // The runtime must outlive `main` (callbacks keep spawning onto it via
    // `handle` until the event loop quits); leaking it is simpler and just
    // as correct as threading a shutdown sequence through every callback.
    std::mem::forget(runtime);

    let loaded = config::load();
    let ptt_key = Keycode::from_str(&loaded.ptt_key)
        .or_else(|_| Keycode::from_str(caps_lock::default_ptt_key()))
        .unwrap_or(Keycode::Space);
    // Validate rather than cast: a hand-edited config could hold any
    // integer, and an unrecognised mode should mean push-to-talk (the
    // option that transmits least) rather than something arbitrary.
    let transmit_mode = match loaded.transmit_mode {
        1 => MODE_VOICE_ACTIVATION,
        2 => MODE_OPEN_MIC,
        _ => MODE_PTT,
    };
    let transmit = Arc::new(TransmitSettings::new(ptt_key, transmit_mode, loaded.va_threshold));
    caps_lock::set_bound(ptt_key == Keycode::CapsLock && transmit_mode == MODE_PTT);
    let output_gain_db = Arc::new(Mutex::new(loaded.output_gain_db));

    // Spec §4: identity is a keypair this machine holds, generated on
    // first run and stable from then on. A failure here is fatal rather
    // than falling back to a throwaway key -- an app that silently became
    // a different person on every launch would break every contact's
    // key-change warning and make verification meaningless.
    let identity_dir = config::config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not locate a config directory for the identity key"))?;
    let identity = Arc::new(antiphon_crypto::store::load_or_create(
        &antiphon_crypto::store::identity_path(&identity_dir),
    )?);

    let peer_volume_db = Arc::new(Mutex::new(0.0f32));
    let peer_muted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let app_state = Arc::new(AppState {
        runtime: handle,
        call: Mutex::new(None),
        transmit,
        output_gain_db,
        loopback: Mutex::new(None),
        voice_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        replay_seconds: Arc::new(std::sync::atomic::AtomicU32::new(loaded.replay_seconds)),
        duck_db: Arc::new(std::sync::atomic::AtomicU32::new(loaded.duck_db)),
        peer_eq: Arc::new(Mutex::new(None)),
        echo_cancel: Arc::new(std::sync::atomic::AtomicBool::new(loaded.echo_cancel)),
        // Built once for the process, not once per call: the filter it
        // learns describes this room and these speakers, which do not
        // change between calls (spec §1's "survives device hot-swap").
        // 48 kHz is the rate the pipeline runs at.
        echo_canceller: Arc::new(Mutex::new(antiphon_audio::EchoCanceller::new(48_000))),
        peer_volume_db: peer_volume_db.clone(),
        peer_muted: peer_muted.clone(),
        punch_socket: Mutex::new(None),
        port_map: Mutex::new(None),
        identity,
        relay_addr: Mutex::new(None),
    });

    // Spec §8: collect mail waiting at your nodes, and leave queued
    // messages at contacts' nodes, in the background.
    delivery::set_identity(app_state.identity.clone());
    {
        let weak = Arc::downgrade(&app_state);
        delivery::start(&app_state.runtime, move || {
            let state = weak.upgrade()?;
            let call = state.call.lock().unwrap();
            call.as_ref().map(|a| a.peer_identity().identity_key)
        });
    }
    delivery::set_hosting(&app_state.runtime, loaded.hold_mail_for_devices, loaded.hold_mail_port);
    sharing::set_seeding(&app_state.runtime, sharing::seeds_from_here(&loaded), loaded.seed_port);
    apply_vpn(&app_state.runtime, &loaded);
    discovery::start(&app_state.runtime, app_state.identity.clone());

    let main_window = MainWindow::new()?;
    main_window.set_my_share_id(share_id(&app_state.identity.public().identity_key).into());
    main_window.set_eq_preset_names(string_vec_to_model(
        antiphon_audio::eq_preset_names().iter().map(|s| s.to_string()).collect(),
    ));
    refresh_invite(&main_window, &app_state);
    if let Some(name) = &profile {
        main_window.set_window_title(format!("Stoop -- {name}").into());
    }
    let tray = AppTrayIcon::new()?;

    // Font registration is in-memory only and doesn't survive a restart on
    // its own -- re-register a previously imported font now (before the UI
    // reads `font_family` off `loaded` below) so the saved family name
    // actually resolves to something instead of silently falling back.
    if !loaded.custom_font_path.is_empty() {
        if let Err(e) =
            std::fs::read(&loaded.custom_font_path).map_err(anyhow::Error::from).and_then(register_font_bytes)
        {
            eprintln!("Could not re-register imported font {}: {e}", loaded.custom_font_path);
        }
    }

    apply_config_to_ui(&main_window, &loaded);
    // Spec §1: what monitoring actually costs, buffer plus an estimate for
    // the sound card -- reporting only our own buffer understates it.
    main_window.set_monitor_latency_ms(state::monitor_added_latency_ms() as i32);
    apply_window_state(&main_window, &loaded.window, loaded.start_maximized);

    // Closing the window (the X button) hides it to the tray rather than
    // quitting -- the app only fully exits via the tray's Quit item. This
    // is what makes "the app lives in the tray" actually true rather than
    // just having an icon that happens to appear. Size/position/maximized
    // are captured here too, since this fires on every real-world "close".
    {
        let window_weak = main_window.as_weak();
        main_window.window().on_close_requested(move || {
            if let Some(app) = window_weak.upgrade() {
                save_current_config(&app);
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    wire_tray(&tray, &main_window, &app_state);
    wire_main_window(&main_window, &tray, &app_state);

    if loaded.server_mode {
        do_start_listen(&main_window, &main_window.as_weak(), &tray.as_weak(), &app_state);
    }

    // Live mic-level meter and PTT indicator: read shared state on a UI-thread
    // timer rather than pushing updates from the audio/network threads.
    let meter_window = main_window.as_weak();
    let meter_state = app_state.clone();
    let _meter_timer = Timer::default();
    let meter_app_state = app_state.clone();
    let meter_window2 = main_window.as_weak();
    // An escalated knock is the one thing this timer raises that belongs
    // in the tray as well as the window: it is asking for attention the
    // user may not currently be giving the app.
    let meter_tray = tray.as_weak();
    // Chat model rebuilds only when the log's revision counter moved --
    // `u32::MAX` forces one initial sync (and re-sync on a new call, whose
    // counter restarts at 0).
    let mut last_chat_rev: u32 = u32::MAX;
    // `u64::MAX` means "not started yet", so the first tick of a call
    // establishes a baseline instead of offering every clip cut before
    // anyone was looking.
    let mut clips_cut_last: u64 = u64::MAX;
    // The relay this call's peer said they listen at, once saved, so it is
    // written once per call rather than on every tick.
    let mut peer_relay_saved = String::new();
    let mut peer_routes_saved = String::new();
    let mut delivery_round_seen = 0u64;
    let mut sharing_seen = u64::MAX;
    let mut spaces_seen = u64::MAX;
    let mut vpn_seen = u64::MAX;
    let mut vpn_ticks = 0u32;
    let mut discovery_seen = 0u64;
    _meter_timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
        let Some(app) = meter_window.upgrade() else { return };
        // Spec §8: what the courier did since the last tick.
        let round = delivery::rounds();
        if round != delivery_round_seen {
            delivery_round_seen = round;
            let notices = delivery::take_notices();
            if !notices.is_empty() {
                app.set_delivery_notice(notices.join("\n").into());
            }
            apply_routes_to_ui(&app, &config::load());
        }
        // Spec §6: the VPN list on change, its status line once a second
        // (a handshake going stale changes it without any event).
        vpn_ticks = vpn_ticks.wrapping_add(1);
        let vpn_now = vpn::revision();
        if vpn_now != vpn_seen {
            vpn_seen = vpn_now;
            apply_vpn_to_ui(&app, &config::load());
        } else if vpn_ticks.is_multiple_of(20) {
            app.set_vpn_status(vpn::status_line().into());
        }
        // Spec §9: a message, an invite, a change to a group or community.
        let spaces_now = community::revision();
        if spaces_now != spaces_seen {
            spaces_seen = spaces_now;
            apply_spaces_to_ui(&app);
        }
        let sharing_now = sharing::revision();
        if sharing_now != sharing_seen {
            sharing_seen = sharing_now;
            apply_shared_files_to_ui(&app, &config::load());
        }
        // Spec §4: what the DHT side knows about reaching us, so a new
        // forwarded port, relay or STUN answer is republished at once.
        discovery::set_local(discovery::Local {
            listen_port: app.get_local_port().parse().unwrap_or(0),
            mapped_port: meter_state.port_map.lock().unwrap().as_ref().map(|h| h.current_external_port()),
            stun_public: Some(app.get_public_address().to_string()).filter(|a| !a.is_empty()),
            relay: app.get_relay_server().trim().to_string(),
        });
        let discovery_now = discovery::revision();
        if discovery_now != discovery_seen {
            discovery_seen = discovery_now;
            app.set_dht_status(discovery::status().into());
            apply_contacts_to_ui(&app, &config::load().contacts);
        }

        // Keyboard shortcuts, raised as flags by the poller thread (which
        // has no handle to the window). Handled here, before the call lock
        // is taken: `hang_up` locks `state.call` itself, so invoking it
        // while this timer still held that guard would deadlock.
        let t = &meter_state.transmit;
        if t.pending_mute.swap(false, Ordering::Relaxed) {
            app.invoke_toggle_mute();
        }
        if t.pending_deafen.swap(false, Ordering::Relaxed) {
            app.invoke_toggle_deafen();
        }
        if t.pending_replay.swap(false, Ordering::Relaxed) {
            app.invoke_save_replay();
        }
        if t.pending_hangup.swap(false, Ordering::Relaxed) {
            app.invoke_hang_up();
        }

        // With no call running the meter would otherwise sit dead, which
        // is exactly when someone is checking their mic with loopback on.
        if meter_state.call.lock().unwrap().is_none() {
            let level = meter_state
                .loopback
                .lock()
                .unwrap()
                .as_ref()
                .map(|l| l.level())
                .unwrap_or(0.0);
            app.set_mic_level(level.min(1.0));
        }

        let call = meter_state.call.lock().unwrap();
        if let Some(active) = call.as_ref() {
            let level = active.mic_level.lock().map(|l| *l).unwrap_or(0.0);
            app.set_mic_level(level.min(1.0));
            app.set_ptt_held(active.ptt_active.load(Ordering::Relaxed));
            if let Some(peer) = active.peer_profile.lock().unwrap().as_ref() {
                app.set_peer_display_name(peer.display_name.clone().into());
                app.set_peer_status_message(peer.status_message.clone().into());
                // No presence field means they said nothing, which shows
                // as nothing -- never as "offline", which would be a claim
                // about someone we are actively talking to.
                let shown = match peer.presence {
                    Some(v) => presence::PeerPresence::Known(presence::Presence::from_i32(v)),
                    None => presence::PeerPresence::Unknown,
                };
                app.set_peer_presence_label(shown.label().into());
                // Spec §3: remember where a contact can be reached through
                // a relay, so the next call to them can use it.
                if !peer.relay.is_empty() && peer.relay != peer_relay_saved {
                    peer_relay_saved = peer.relay.clone();
                    save_peer_relay(&active.peer_identity().identity_key, &peer.relay);
                }
                // Spec §8: their route list, so mail can be left for them
                // while they're offline. Checked against who they are.
                if !peer.routes.is_empty() && peer.routes != peer_routes_saved {
                    peer_routes_saved = peer.routes.clone();
                    save_peer_routes(&active.peer_identity().identity_key, &peer.routes);
                }
            }
            update_file_transfer_ui(&app, active, &meter_app_state, &meter_window2);

            // Spec §1 gain reduction meter. Read from the live chain, so
            // it reflects what the compressor is actually doing rather
            // than what the settings imply it would do.
            // A silent-call marker arrives just after the connection, so
            // the ring decision made at connect time is revisited once --
            // the flag can only lower what was already decided.
            if active.peer_wants_silent.swap(false, Ordering::Relaxed)
                && ring_outcome(&active.peer_identity().identity_key, true) != notify::Outcome::Ring
            {
                app.set_status_text("Connected (they knocked quietly)".into());
            }

            // Spec §9: a knock escalated to a ring, without the call being
            // restarted. The decision is remade here, on this machine,
            // from this machine's rules -- withdrawing the caller's own
            // silencing is the only thing the message can do.
            if active.peer_wants_ring.swap(false, Ordering::Relaxed) {
                let name = app.get_peer_display_name();
                let name = if name.is_empty() { "They".to_string() } else { name.to_string() };
                match ring_outcome(&active.peer_identity().identity_key, false) {
                    notify::Outcome::Ring => {
                        set_status(&app, &meter_tray, &format!("{name} needs you"));
                    }
                    notify::Outcome::Notify => {
                        app.set_status_text(format!("{name} needs you (silenced)").into());
                    }
                    // Their rules said never. Saying so is the point: an
                    // escalation that quietly did nothing would leave the
                    // caller waiting on an answer that is not coming.
                    notify::Outcome::Ignore => {
                        app.set_status_text(
                            format!("{name} needs you (notifications off for them)").into(),
                        );
                    }
                }
            }

            if let Ok(chain) = active.chain.lock() {
                app.set_comp_gain_reduction_db(chain.gain_reduction_db("compressor"));
                app.set_gate_attenuation_db(chain.gain_reduction_db("noise_gate"));
            }
            // Spec §1: how much echo is actually being removed. Shown
            // rather than assumed -- an echo canceller that is switched on
            // and doing nothing looks exactly like one that is working,
            // right up until the other person complains.
            if let Ok(aec) = active.echo_canceller.lock() {
                app.set_echo_erle_db(aec.erle_db());
            }

            // Spec §2 replay modes: Off / Ask me / Auto-play. What is
            // offered is a *clip* -- the seconds around a moment the
            // network damaged badly enough to be worth a second listen
            // (`replay_history.rs`) -- not every frame FEC quietly fixed,
            // which plays seamlessly with nothing to replay.
            {
                let cut = active.replay_clips_cut();
                if clips_cut_last != u64::MAX && cut > clips_cut_last {
                    match app.get_replay_mode() {
                        1 => {
                            app.set_replay_ask(true);
                            app.set_replay_notice("Their audio dropped out and was recovered.".into());
                        }
                        2 => {
                            app.set_replay_notice("Their audio dropped out -- replaying it now.".into());
                            app.invoke_play_replay_clip(0);
                        }
                        _ => {}
                    }
                }
                clips_cut_last = cut;
            }

            // Spec §2 replay history. Rebuilt only when the count moves:
            // the labels are stable, and replacing the model every tick
            // would reset the list under whoever is reading it.
            {
                let labels = active.replay_clip_labels();
                if labels.len() != app.get_replay_clips().row_count() {
                    let rows: Vec<SharedString> =
                        labels.into_iter().map(SharedString::from).collect();
                    app.set_replay_clips(ModelRc::new(VecModel::from(rows)));
                }
            }

            let (delivered, concealed, recovered) = {
                let jitter = active.jitter.lock().unwrap();
                let (d, c) = jitter.stats();
                (d, c, jitter.recovered())
            };
            let secs = active.started_at.elapsed().as_secs();
            let total = delivered + concealed;
            let loss_pct = if total > 0 { concealed as f64 * 100.0 / total as f64 } else { 0.0 };
            app.set_call_stats(
                format!(
                    "{:02}:{:02}:{:02} -- {delivered} received, {recovered} repaired, {concealed} concealed ({loss_pct:.1}% loss)",
                    secs / 3600,
                    (secs / 60) % 60,
                    secs % 60
                )
                .into(),
            );

            // Spec §4: an introduction touches the contact list, which
            // outlives the call, so it is applied here on the UI thread
            // rather than in the receive loop.
            if let Some(intro) = active.pending_introduction.lock().unwrap().take() {
                let introducer = {
                    let hex = encode_identity_key(&active.peer_identity().identity_key);
                    let config = config::load();
                    config
                        .contacts
                        .iter()
                        .find(|c| c.identity_key == hex)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| "someone on this call".to_string())
                };
                let message = apply_introduction(&intro, active, &introducer);
                app.set_introduction_message(message.into());
            }

            // Spec §3 group calls: act on introductions, keep the MLS
            // group keys current, notice who left, and keep the
            // participant line current.
            let group_busy = !active.group.inbox.lock().unwrap().is_empty()
                || ((active.group.people() >= 3 || active.group.keys.in_group()) && active.group.keys.poll_due());
            if group_busy {
                group_context(&app, &meter_app_state, active).poll(&meter_app_state.runtime);
            }
            for (_, name) in active.group.remove_departed() {
                *active.group.notice.lock().unwrap() = format!("{name} left the call.");
            }
            {
                let notice = std::mem::take(&mut *active.group.notice.lock().unwrap());
                if !notice.is_empty() {
                    app.set_contacts_message(notice.into());
                }
                let mut names = active.group.names().join(", ");
                // Spec §3: say when the group's audio is under an MLS
                // key, and which epoch, so a rekey on a join or leave is
                // visible rather than taken on trust.
                if let Some(epoch) = active.group.keys.epoch().filter(|_| active.group.people() >= 3) {
                    names.push_str(&format!(" · group key #{epoch} (MLS + SFrame)"));
                }
                let people = active.group.people() as i32;
                if app.get_group_people() != people || app.get_group_names().as_str() != names {
                    app.set_group_people(people);
                    app.set_group_names(names.into());
                }
            }

            let pending = active.pending_friend_request.lock().unwrap().clone();
            app.set_friend_request_from(pending.unwrap_or_default().into());
            // The peer accepted a request we sent -- save them now that
            // there's a name to save them under.
            if let Some((name, address)) = active.take_friend_accepted() {
                save_contact_if_new(&app, &meter_app_state, active, &name, &address);
            }

            let rev = active.chat_rev.load(Ordering::Relaxed);
            if rev != last_chat_rev {
                // Summarised once per rebuild rather than per message:
                // the alternative is scanning every reaction for every row.
                let reaction_summary: std::collections::HashMap<u64, String> = {
                    let reactions = active.reactions.lock().unwrap();
                    let mut map = std::collections::HashMap::new();
                    for r in reactions.iter() {
                        map.entry(r.message_id).or_insert_with(|| {
                            reactions::summarize(&reactions, r.message_id)
                        });
                    }
                    map
                };
                last_chat_rev = rev;
                let entries: Vec<ChatMessage> = active
                    .chat_log
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|e| ChatMessage {
                        id: e.id as i32,
                        from_me: e.from_me,
                        text: e.display_text().into(),
                        edited: e.edited,
                        deleted: e.deleted,
                        pinned: e.pinned,
                        links: link_preview::describe(&e.text).into(),
                        reactions: reaction_summary.get(&e.id).cloned().unwrap_or_default().into(),
                        // Spec §8. Only on our own messages: a delivery
                        // state on something *they* sent would be a claim
                        // about their device that this one cannot make.
                        delivery: if e.from_me {
                            active.delivery_status(e.id).wording().into()
                        } else {
                            SharedString::new()
                        },
                    })
                    .collect();
                app.set_chat_messages(ModelRc::new(VecModel::from(entries)));
                // Spec §8. Named rather than "them" so a queue noticed on
                // a shared screen still says who it is for.
                let name = app.get_peer_display_name();
                let name = if name.is_empty() { "them".to_string() } else { name.to_string() };
                app.set_outbox_summary(
                    active.outbox_summary(&name).map(SharedString::from).unwrap_or_default(),
                );
            }
        } else {
            app.set_mic_level(0.0);
            app.set_ptt_held(false);
            app.set_ft_offer_pending(false);
            app.set_ft_outgoing_active(false);
            app.set_ft_incoming_active(false);
            app.set_call_stats("".into());
            app.set_friend_request_from("".into());
            last_chat_rev = u32::MAX;
            // Reset the baseline so the next call does not offer clips
            // carried over from this one.
            clips_cut_last = u64::MAX;
            peer_relay_saved.clear();
            app.set_replay_notice("".into());
            app.set_replay_ask(false);
        }
    });

    // Spec §10: the table starts filled in with "not checked yet" rather
    // than empty, so the questions are visible before any answers are.
    apply_reachability(&main_window, &current_reachability(&main_window));
    main_window.set_call_mode(reachability::Mode::Direct.label().into());
    main_window.set_call_mode_explanation(reachability::Mode::Direct.explain().into());
    network_ui::install(&main_window, &app_state);

    main_window.show()?;
    tray.show()?;
    slint::run_event_loop()?;

    // Spec §10: hand the port back on the way out rather than leaving a
    // hole in the user's router until it expires. Waited on, briefly,
    // because the release is one UDP packet and the process is about to
    // end -- spawning it and exiting would send nothing at all.
    let closing = app_state.port_map.lock().unwrap().take();
    if let Some(handle) = closing {
        handle.close_before_exit(&app_state.runtime);
    }
    Ok(())
}

fn wire_tray(tray: &AppTrayIcon, main_window: &MainWindow, app_state: &Arc<AppState>) {
    let window_weak = main_window.as_weak();
    tray.on_show_window(move || {
        if let Some(w) = window_weak.upgrade() {
            let _ = w.show();
        }
    });

    let window_weak = main_window.as_weak();
    tray.on_quit_app(move || {
        if let Some(app) = window_weak.upgrade() {
            save_current_config(&app);
        }
        slint::quit_event_loop().ok();
    });

    let window_weak = main_window.as_weak();
    let state = app_state.clone();
    tray.on_toggle_mute(move || {
        if let Some(w) = window_weak.upgrade() {
            toggle_mute(&w, &state);
        }
    });
}

/// Starts listening on the window's saved port -- shared by the "Listen"
/// button, the server-mode checkbox (turning it on while not connected),
/// and auto-listen at launch when server mode was saved on.
fn do_start_listen(app: &MainWindow, window_weak: &slint::Weak<MainWindow>, tray_weak: &slint::Weak<AppTrayIcon>, state: &Arc<AppState>) {
    if app.get_connected() {
        return;
    }
    let port: u16 = app.get_local_port().parse().unwrap_or(6000);
    let config = read_chain_config(app);
    let (input, output) = selected_devices(app);
    let hello = local_hello(app);
    set_status(app, tray_weak, "Listening...");

    let window_weak2 = window_weak.clone();
    let tray_weak2 = tray_weak.clone();
    let state2 = state.clone();
    let identity = state.identity.clone();
    let relay = listen_relay(app, state);
    state.runtime.spawn(async move {
        // Spec §3: also reachable through our relay, when one is set. A
        // relay that won't resolve costs the relay path, not the listen.
        let relay = match relay {
            Some((address, tokens)) => match state::resolve(&address).await {
                Ok(addr) => {
                    *state2.relay_addr.lock().unwrap() = Some(addr);
                    Some((addr, tokens))
                }
                Err(_) => None,
            },
            None => {
                *state2.relay_addr.lock().unwrap() = None;
                None
            }
        };
        let result = state::establish_listen(port, identity, relay).await;
        finish_connect(window_weak2, tray_weak2, state2, result, config, input, output, hello);
    });
}

fn local_hello(app: &MainWindow) -> Hello {
    // Spec §7: invisible sends *nothing*, not a status reading "offline".
    // `None` is the wire form of "said nothing", which is exactly right.
    let chosen = presence::Presence::from_i32(app.get_my_presence());
    Hello {
        display_name: app.get_display_name().to_string(),
        status_message: app.get_status_message().to_string(),
        presence: chosen.should_publish().then(|| chosen.to_i32()),
        // Spec §9: whether this call is being started as a knock. Set by
        // the caller before connecting; a listener never knocks, since
        // answering is not something you do quietly.
        wants_silent: app.get_start_call_silently(),
        relay: network_ui::effective_relay(app),
        routes: delivery::my_record_hex(),
    }
}

/// Brings `contact` into the current call (spec §3 group calls), reaching
/// them the same way a call to them on its own would.
fn add_to_group_call(app: &MainWindow, state: &Arc<AppState>, contact: config::Contact) {
    let ctx = {
        let call = state.call.lock().unwrap();
        let Some(active) = call.as_ref() else { return };
        // Past the mesh the call moves to our relay's SFU, so only a
        // full SFU call, or a full mesh with no relay, turns anyone away.
        let has_relay = !network_ui::effective_relay(app).is_empty();
        if active.group.is_full() && (active.group.sfu_call().is_some() || !has_relay) {
            let most = if active.group.sfu_call().is_some() { sfu_call::MAX_PEOPLE } else { group::MAX_PEOPLE };
            app.set_contacts_message(format!("A group call holds up to {most} people.").into());
            return;
        }
        group_context(app, state, active)
    };
    let plan = match connection_mode::plan(&contact, &state.identity, now_secs()) {
        Ok(plan) => plan,
        Err(reason) => {
            app.set_contacts_message(reason.into());
            return;
        }
    };
    app.set_contacts_message(format!("Adding {}...", contact.name).into());
    let identity = state.identity.clone();
    let window_weak = app.as_weak();
    let name = contact.name.clone();
    state.runtime.spawn(async move {
        let connected = match plan {
            connection_mode::Plan::Direct { address } => match address.parse() {
                Ok(addr) => state::establish_call(addr, 0, identity).await.map(|p| (p, false)),
                Err(_) => Err(anyhow::anyhow!("{address} isn't an address")),
            },
            connection_mode::Plan::Relay { relay, token } => async {
                let addr = state::resolve(&relay).await?;
                state::establish_relayed_call(addr, token, 0, identity).await
            }
            .await
            .map(|p| (p, true)),
        };
        let message = match connected {
            Ok((peer, relayed)) => match ctx.add_person(peer, relayed).await {
                Ok(m) => m,
                Err(e) => e,
            },
            Err(e) => format!("Couldn't add {name}: {e}"),
        };
        slint::invoke_from_event_loop(move || {
            if let Some(app) = window_weak.upgrade() {
                app.set_contacts_message(message.into());
            }
        })
        .ok();
    });
}

/// What acting on group-call messages needs (spec §3).
fn group_context(app: &MainWindow, state: &Arc<AppState>, active: &state::ActiveCall) -> group::Context {
    let relay = network_ui::effective_relay(app);
    group::Context {
        group: active.group.clone(),
        primary: active.peer.clone(),
        identity: state.identity.clone(),
        hello: local_hello(app),
        relay: (!relay.is_empty()).then_some(relay),
        is_blocked: Arc::new(is_peer_blocked),
    }
}

/// Spec §4: a direct call to `contact` that looks them up in the DHT when
/// their saved address doesn't answer, then falls back to their relay
/// (only in automatic mode -- a contact set to Direct stays direct).
async fn call_with_lookup(
    contact: &config::Contact,
    saved: String,
    local_port: u16,
    identity: Arc<antiphon_crypto::IdentityKeyPair>,
    state: &Arc<AppState>,
    window_weak: &slint::Weak<MainWindow>,
    now: u64,
) -> anyhow::Result<antiphon_net::PeerConnection> {
    let attempt = |address: String| {
        let identity = identity.clone();
        async move {
            let remote = state::resolve(&address).await?;
            match tokio::time::timeout(discovery::DIRECT_TRY, state::establish_call(remote, local_port, identity)).await {
                Ok(result) => result,
                Err(_) => anyhow::bail!("{address} didn't answer"),
            }
        }
    };
    let say = |text: String| {
        let weak = window_weak.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                app.set_status_text(text.into());
            }
        })
        .ok();
    };
    let mut last = match attempt(saved.clone()).await {
        Ok(peer) => return Ok(peer),
        Err(e) => e,
    };
    say(format!("{} didn't answer at their saved address. Looking them up...", contact.name));
    if let Some(fresh) = discovery::find_contact_now(&contact.identity_key, &saved).await {
        say(format!("Found {} at a new address. Calling...", contact.name));
        let weak = window_weak.clone();
        let shown = fresh.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                app.set_remote_address(shown.into());
            }
        })
        .ok();
        match attempt(fresh).await {
            Ok(peer) => return Ok(peer),
            Err(e) => last = e,
        }
    }
    // The DHT may have brought a relay along; re-read the contact.
    let refreshed = config::load().contacts.into_iter().find(|c| c.identity_key == contact.identity_key);
    let contact = refreshed.as_ref().unwrap_or(contact);
    if connection_mode::Override::from_i32(contact.connection_mode) == connection_mode::Override::Auto {
        if let Some(connection_mode::Plan::Relay { relay, token }) = connection_mode::relay_only(contact, &identity, now) {
            say(format!("Trying {} through their relay...", contact.name));
            let addr = state::resolve(&relay).await?;
            *state.relay_addr.lock().unwrap() = Some(addr);
            return state::establish_relayed_call(addr, token, local_port, identity).await;
        }
    }
    Err(last)
}

/// Records the relay a saved contact listens at. Strangers are not saved.
/// Stores a contact's route list if it is theirs and newer than ours.
fn save_peer_routes(identity_key: &[u8; 32], encoded: &str) {
    let mut config = config::load();
    let key = encode_identity_key(identity_key);
    let Some(contact) = config.contacts.iter_mut().find(|c| c.identity_key == key) else { return };
    if contact.route_record == encoded {
        return;
    }
    let Some(record) = delivery::accept_record(encoded, identity_key, contact.route_sequence) else { return };
    contact.route_record = encoded.to_string();
    contact.route_sequence = Some(record.record.sequence);
    config::save(&config);
}

fn save_peer_relay(identity_key: &[u8; 32], relay: &str) {
    let mut config = config::load();
    let key = encode_identity_key(identity_key);
    if let Some(contact) = config.contacts.iter_mut().find(|c| c.identity_key == key) {
        if contact.relay != relay {
            contact.relay = relay.to_string();
            config::save(&config);
        }
    }
}

/// The secret behind the relay part of the invite link (spec §3), made on
/// first use and kept until the user asks for a new link.
fn relay_invite_secret() -> [u8; 32] {
    let mut config = config::load();
    if let Some(secret) = connection_mode::decode_hex32(&config.relay_invite_secret) {
        return secret;
    }
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).expect("OS randomness is unavailable");
    config.relay_invite_secret = connection_mode::encode_hex32(&secret);
    config::save(&config);
    secret
}

/// What to register at our relay while listening, if there is one.
fn listen_relay(
    app: &MainWindow,
    state: &Arc<AppState>,
) -> Option<(String, state::RelayTokens)> {
    let relay = network_ui::effective_relay(app);
    if relay.is_empty() {
        return None;
    }
    let identity = state.identity.clone();
    let secret = relay_invite_secret();
    Some((
        relay,
        Box::new(move || {
            connection_mode::listening_tokens(&config::load().contacts, &identity, Some(secret), now_secs())
        }),
    ))
}

fn wire_main_window(main_window: &MainWindow, tray: &AppTrayIcon, app_state: &Arc<AppState>) {
    wire_mic_test(main_window, app_state);
    {
        let window_weak = main_window.as_weak();
        let tray_weak = tray.as_weak();
        let state = app_state.clone();
        main_window.on_start_listen(move || {
            let Some(app) = window_weak.upgrade() else { return };
            do_start_listen(&app, &window_weak, &tray_weak, &state);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_server_mode_changed(move || {
            let Some(app) = window_weak.upgrade() else { return };
            // Entering/leaving server mode is a whole different app surface
            // (console vs. widget UI), so it's a save-and-relaunch, not an
            // in-place mode flip: the relaunched instance reads
            // `server_mode` from config at startup and auto-listens there.
            relaunch_app(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let tray_weak = tray.as_weak();
        let state = app_state.clone();
        main_window.on_start_call(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let remote_str = app.get_remote_address().to_string();
            let Ok(remote) = remote_str.parse() else {
                set_status(&app, &tray_weak, &format!("Invalid address: {remote_str}"));
                return;
            };
            let local_port: u16 = app.get_local_port().parse().unwrap_or(0);
            let config = read_chain_config(&app);
            let (input, output) = selected_devices(&app);
            let hello = local_hello(&app);
            set_status(&app, &tray_weak, "Calling...");
            *state.relay_addr.lock().unwrap() = None;

            let window_weak2 = window_weak.clone();
            let tray_weak2 = tray_weak.clone();
            let state2 = state.clone();
            let identity = state.identity.clone();
            state.runtime.spawn(async move {
                let result = state::establish_call(remote, local_port, identity).await;
                finish_connect(window_weak2, tray_weak2, state2, result, config, input, output, hello);
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        let tray_weak = tray.as_weak();
        let state = app_state.clone();
        main_window.on_hang_up(move || {
            let Some(app) = window_weak.upgrade() else { return };
            // Spec §5: keep an in-flight transfer so it can resume, before
            // dropping the call takes its state with it.
            if let Some(active) = state.call.lock().unwrap().as_ref() {
                file_transfer::save_partial_if_interrupted(&active.file_transfer);
                // Counted on hang-up rather than on connect: a call that
                // happened is what makes losing your identity cost
                // something.
                let mut config = config::load();
                config.calls_completed = config.calls_completed.saturating_add(1);
                config::save(&config);
                apply_onboarding(&app, &config);
            }
            if let Some(active) = state.call.lock().unwrap().as_ref() {
                let _guard = state.runtime.enter();
                active.group.announce_leaving(&active.peer);
            }
            *state.call.lock().unwrap() = None; // Drop tears the call down.
            // Spec §8: anything still queued for them can now go to their
            // mailbox, and mail held back during the call can come in.
            delivery::kick();
            file_transfer::set_call_active(false);
            app.set_connected(false);
            app.set_muted(false);
            app.set_deafened(false);
            app.set_monitor_enabled(false);
            app.set_peer_display_name("".into());
            app.set_verification_code("".into());
            app.set_peer_fingerprint("".into());
            app.set_peer_trust_message("".into());
            app.set_peer_key_changed(false);
            app.set_peer_verified(false);
            app.set_peer_status_message("".into());
            set_status(&app, &tray_weak, "Not connected");
        });
    }
    main_window.on_initial_of(|name| initial_of(&name).into());
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_toggle_mute(move || {
            if let Some(app) = window_weak.upgrade() {
                toggle_mute(&app, &state);
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_toggle_deafen(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                let now_deafened = !active.deafened.load(Ordering::Relaxed);
                active.deafened.store(now_deafened, Ordering::Relaxed);
                drop(call);
                app.set_deafened(now_deafened);
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_toggle_monitor(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                let now_on = !active.monitor_enabled.load(Ordering::Relaxed);
                active.monitor_enabled.store(now_on, Ordering::Relaxed);
                drop(call);
                app.set_monitor_enabled(now_on);
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §4: the invite is the primary way to add someone. It pairs
        // the permanent identity with a *current* address, so it has to be
        // rebuilt whenever that address changes -- hence an explicit
        // refresh rather than a value computed once at startup.
        main_window.on_refresh_invite(move || {
            let Some(app) = window_weak.upgrade() else { return };
            refresh_invite(&app, &state);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_add_from_invite(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let text = app.get_new_contact_invite().to_string();

            let invite = match antiphon_crypto::Invite::parse(&text) {
                Ok(invite) => invite,
                Err(_) => {
                    app.set_contacts_message(
                        "That doesn't look like an invite link. It should start with antiphon://".into(),
                    );
                    return;
                }
            };

            let identity_key = encode_identity_key(&invite.identity_key);
            let mut contacts = contacts_from_ui(&app);

            // Identity, not address, decides whether this is someone new --
            // the whole reason contacts are keyed by key.
            if let Some(existing) = contacts.iter_mut().find(|c| c.identity_key == identity_key) {
                let name = existing.name.clone();
                let new_secret =
                    invite.relay_secret.as_ref().map(connection_mode::encode_hex32).unwrap_or_default();
                let address_changed = !invite.address.is_empty() && existing.address != invite.address;
                let relay_changed = !invite.relay.is_empty()
                    && (existing.relay != invite.relay || existing.relay_secret != new_secret);
                if !address_changed && !relay_changed {
                    app.set_contacts_message(format!("{name} is already saved").into());
                    return;
                }
                if address_changed {
                    existing.address = invite.address.clone();
                }
                if relay_changed {
                    existing.relay = invite.relay.clone();
                    existing.relay_secret = new_secret;
                }
                apply_contacts_to_ui(&app, &contacts);
                save_current_config(&app);
                app.set_contacts_message(format!("Updated {name}'s details").into());
                app.set_new_contact_invite("".into());
                return;
            }

            // Pinned from the invite, but explicitly *not* verified: a link
            // can be forwarded or swapped in transit, and only a compared
            // verification code settles that.
            let name = if app.get_new_contact_name().is_empty() {
                format!("Contact {}", &identity_key[..8])
            } else {
                app.get_new_contact_name().to_string()
            };
            contacts.push(config::Contact {
                name: name.clone(),
                address: invite.address.clone(),
                is_server: false,
                identity_key,
                verified: false,
                first_seen_secs: now_secs(),
                volume_db: 0.0,
                muted: false,
                expire_after_secs: None,
                ring_rule: 0,
                eq_bands: Vec::new(),
                note: String::new(),
                // Spec §3: a stranger is reached through their relay, so
                // the relay part of the link is what makes them callable
                // without either side exposing an address.
                relay: invite.relay.clone(),
                relay_secret: invite.relay_secret.as_ref().map(connection_mode::encode_hex32).unwrap_or_default(),
                ..Default::default()
            });
            apply_contacts_to_ui(&app, &contacts);
            save_current_config(&app);
            app.set_new_contact_invite("".into());
            app.set_new_contact_name("".into());
            app.set_contacts_message(
                format!("Added {name}. Compare the code on your first call to verify them.").into(),
            );
        });
    }
    {
        let window_weak = main_window.as_weak();
        // Spec §13 "take everything with you, in an open format".
        main_window.on_export_all_data(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let Some(dest) = rfd::FileDialog::new()
                .set_title("Choose a folder to export into")
                .pick_folder()
            else {
                return;
            };
            let Some(dir) = config::config_dir() else { return };

            // History is decrypted on the way out -- an export you cannot
            // read is not an export. The key stays here.
            let backup_key = history::load_or_create_backup_key(&dir).ok();
            let decrypt = |identity_hex: &str| {
                let key = backup_key.as_ref()?;
                let log = history::load(&dir, identity_hex, key);
                (!log.is_empty()).then_some(log)
            };

            match data_export::export_all(&dir, &dest, decrypt) {
                Ok(summary) => {
                    let mut parts = vec![format!(
                        "Exported {} contacts, {} conversations ({} messages)",
                        summary.contacts, summary.conversations, summary.messages
                    )];
                    if summary.sound_clips > 0 {
                        parts.push(format!("{} clips", summary.sound_clips));
                    }
                    if summary.stored_files > 0 {
                        parts.push(format!("{} files", summary.stored_files));
                    }
                    parts.push(format!("to {}", dest.display()));
                    app.set_import_export_message(parts.join(", ").into());
                }
                Err(e) => app.set_import_export_message(format!("Export failed: {e}").into()),
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §13 voice messages. Recorded standalone and sent down the
        // existing file-transfer path, which already does chunking, hash
        // verification and repair -- a second transport would be all of
        // that again, tested less.
        main_window.on_start_voice_message(move || {
            let Some(app) = window_weak.upgrade() else { return };
            if app.get_voice_recording() {
                return;
            }
            app.set_voice_recording(true);
            app.set_voice_message_status(
                format!("Recording... (up to {}s)", voice_message::MAX_SECONDS).into(),
            );

            let stop = state.voice_stop.clone();
            stop.store(false, Ordering::Relaxed);
            let (input, _) = selected_devices(&app);
            let config = read_chain_config(&app);
            let window_weak2 = window_weak.clone();
            let state2 = state.clone();

            state.runtime.spawn_blocking(move || {
                let result =
                    state::record_until(input.as_deref(), stop, voice_message::MAX_SECONDS);
                slint::invoke_from_event_loop(move || {
                    let Some(app) = window_weak2.upgrade() else { return };
                    app.set_voice_recording(false);

                    let (sample_rate, mut samples) = match result {
                        Ok(v) => v,
                        Err(e) => {
                            app.set_voice_message_status(format!("Recording failed: {e}").into());
                            return;
                        }
                    };

                    // Same chain a live call uses, so a voice message
                    // sounds like you rather than like a raw microphone.
                    voice_message::process_recording(&mut samples, sample_rate, &config);
                    let trimmed = voice_message::trim_silence(&samples, 0.01).to_vec();
                    if trimmed.is_empty() {
                        app.set_voice_message_status("Nothing was recorded.".into());
                        return;
                    }

                    let seconds = voice_message::duration_secs(&trimmed, sample_rate);
                    let wav = voice_message::encode_wav(sample_rate, &trimmed);
                    let name = voice_message::file_name(now_secs());

                    let call = state2.call.lock().unwrap();
                    let Some(active) = call.as_ref() else {
                        app.set_voice_message_status("The call ended before it could send.".into());
                        return;
                    };
                    // A voice message has nothing to preview.
                    let sha256 = antiphon_net::file_transfer::sha256_hex(&wav);
                    active.send_file(
                        &state2.runtime,
                        name,
                        file_integrity::FileSource::Memory(wav),
                        sha256,
                        String::new(),
                    );
                    app.set_voice_message_status(format!("Sent a {seconds:.0}s voice message.").into());
                })
                .ok();
            });
        });
    }
    {
        let state = app_state.clone();
        main_window.on_stop_voice_message(move || {
            state.voice_stop.store(true, Ordering::Relaxed);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §13 soundboard. The file is copied into the app so the clip
        // keeps working after the original is moved or deleted.
        main_window.on_add_sound_clip(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let Some(picked) = rfd::FileDialog::new()
                .add_filter("WAV audio", &["wav"])
                .set_title("Add a soundboard clip")
                .pick_file()
            else {
                return;
            };

            // Validated before copying: no point keeping a file the
            // soundboard will refuse to play.
            if let Err(e) = soundboard::load_wav(&picked) {
                app.set_soundboard_message(format!("Couldn't add that: {e}").into());
                return;
            }
            let Some(dir) = config::config_dir() else { return };
            let clips = soundboard::clips_dir(&dir);
            if let Err(e) = std::fs::create_dir_all(&clips) {
                app.set_soundboard_message(format!("Couldn't add that: {e}").into());
                return;
            }

            let name = picked
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Clip".to_string());
            let dest = clips.join(format!("{}.wav", sanitize_file_stem(&name)));
            if let Err(e) = std::fs::copy(&picked, &dest) {
                app.set_soundboard_message(format!("Couldn't add that: {e}").into());
                return;
            }

            let mut config = config::load();
            config.sound_clips.push(soundboard::SoundClip {
                name: name.clone(),
                path: dest.to_string_lossy().to_string(),
                gain_db: 0.0,
            });
            config::save(&config);
            apply_sound_clips_to_ui(&app, &config.sound_clips);
            app.set_soundboard_message(format!("Added \"{name}\"").into());
            let _ = &state;
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_remove_sound_clip(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut config = config::load();
            let Ok(i) = usize::try_from(index) else { return };
            if i >= config.sound_clips.len() {
                return;
            }
            let removed = config.sound_clips.remove(i);
            // The copy in the app's own directory is ours to delete; the
            // user's original is untouched.
            let _ = std::fs::remove_file(&removed.path);
            config::save(&config);
            apply_sound_clips_to_ui(&app, &config.sound_clips);
            app.set_soundboard_message(format!("Removed \"{}\"", removed.name).into());
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_play_sound_clip(move |index, to_peer| {
            let Some(app) = window_weak.upgrade() else { return };
            let config = config::load();
            let Ok(i) = usize::try_from(index) else { return };
            let Some(clip) = config.sound_clips.get(i) else { return };

            let loaded = match soundboard::load_wav(std::path::Path::new(&clip.path)) {
                Ok(loaded) => loaded,
                Err(e) => {
                    app.set_soundboard_message(format!("Couldn't play that: {e}").into());
                    return;
                }
            };

            let call = state.call.lock().unwrap();
            if to_peer {
                let Some(active) = call.as_ref() else {
                    app.set_soundboard_message("Only during a call.".into());
                    return;
                };
                // Resampled to the *call's* rate, not the device default:
                // a hot-swap can leave those different, and a clip at the
                // wrong rate plays at the wrong pitch.
                let rate = active.sample_rate.load(Ordering::Relaxed);
                let mut samples = soundboard::resample(&loaded, rate);
                soundboard::apply_gain(&mut samples, clip.gain_db);
                active.soundboard_queue.lock().unwrap().extend(samples);
                app.set_soundboard_message(format!("Sent \"{}\"", clip.name).into());
            } else {
                drop(call);
                let (_, output) = selected_devices(&app);
                let mut samples = loaded.samples.clone();
                soundboard::apply_gain(&mut samples, clip.gain_db);
                match state::play_clip(output.as_deref(), loaded.sample_rate, &samples) {
                    Ok(handle) => {
                        // Held until the clip finishes: dropping the handle
                        // stops the stream mid-sound.
                        let seconds = samples.len() as f32 / loaded.sample_rate.max(1) as f32;
                        state.runtime.spawn(async move {
                            tokio::time::sleep(Duration::from_secs_f32(seconds + 0.2)).await;
                            drop(handle);
                        });
                        app.set_soundboard_message(format!("Playing \"{}\"", clip.name).into());
                    }
                    Err(e) => app.set_soundboard_message(format!("Couldn't play that: {e}").into()),
                }
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §7 presence. Pushed to the peer immediately, so a change
        // lands on the call in progress rather than only the next one.
        main_window.on_presence_changed(move |value| {
            let Some(app) = window_weak.upgrade() else { return };
            let chosen = presence::Presence::from_i32(value);
            app.set_my_presence(chosen.to_i32());
            app.set_presence_note(chosen.explain().into());
            save_current_config(&app);

            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                let peer = active.peer.clone();
                let hello = local_hello(&app);
                state.runtime.spawn(async move {
                    let _ = peer.send_control(&ControlMessage::Hello(hello).encode()).await;
                });
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        // Spec §7's dyslexia-friendly font. Resolved against what is
        // actually installed rather than named blindly: a button that
        // selects a font nobody has is a button that does nothing.
        main_window.on_pick_font_choice(move |_index| {
            let Some(app) = window_weak.upgrade() else { return };
            match pick_readable_font() {
                Some((family, note)) => {
                    app.set_font_family_choice(family.into());
                    app.set_font_choice_note(note.into());
                }
                None => app.set_font_choice_note(
                    "No high-legibility font was found on this system. Installing OpenDyslexic                      and choosing this again will use it."
                        .into(),
                ),
            }
            save_current_config(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        // Spec §7's theme gallery. Sets only the three values that define
        // a look -- accent, background, light/dark -- and deliberately
        // leaves wallpaper, font and opacity alone, since losing those to
        // a colour change would be a nasty surprise.
        main_window.on_apply_theme_preset(move |name| {
            let Some(app) = window_weak.upgrade() else { return };
            let Some(preset) = theme_presets::get(&name) else { return };

            if let Some(accent) = color_from_hex(preset.accent_hex) {
                set_accent_color(&app, accent);
            }
            if let Some(bg) = color_from_hex(preset.background_hex) {
                set_theme_bg_color(&app, bg);
            }
            app.set_theme_mode(preset.theme_mode);
            save_current_config(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        // Spec §7 "Save named arrangements".
        main_window.on_save_layout(move |name| {
            let Some(app) = window_weak.upgrade() else { return };
            let name = name.trim().to_string();
            if name.is_empty() {
                return;
            }
            let mut config = config::load();
            let layout = current_layout(&app, &name);
            // Saving over an existing name replaces it rather than adding a
            // duplicate: two entries reading the same would be impossible
            // to tell apart in the list.
            match config.saved_layouts.iter_mut().find(|l| l.name == name) {
                Some(existing) => *existing = layout,
                None => config.saved_layouts.push(layout),
            }
            config::save(&config);
            apply_layout_names(&app, &config.saved_layouts);
            app.set_new_layout_name("".into());
            app.set_layout_message(format!("Saved \"{name}\"").into());
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_load_layout(move |name| {
            let Some(app) = window_weak.upgrade() else { return };
            let config = config::load();
            let Some(layout) = config.saved_layouts.iter().find(|l| l.name == name.as_str()) else {
                return;
            };
            apply_layout(&app, layout);
            save_current_config(&app);
            app.set_layout_message(format!("Using \"{name}\"").into());
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_delete_layout(move |name| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut config = config::load();
            config.saved_layouts.retain(|l| l.name != name.as_str());
            config::save(&config);
            apply_layout_names(&app, &config.saved_layouts);
            app.set_layout_message(format!("Deleted \"{name}\"").into());
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §13 message lifecycle. All three refuse politely when there
        // is no call: the log only exists while one is up.
        main_window.on_edit_chat_message(move |id, text| {
            let Some(app) = window_weak.upgrade() else { return };
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                active.edit_chat(&state.runtime, id as u64, text.to_string());
            }
            drop(call);
            app.set_chat_input("".into());
        });
    }
    {
        let state = app_state.clone();
        main_window.on_delete_chat_message(move |id| {
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                active.delete_chat(&state.runtime, id as u64);
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §2: the Replay window is shared with the running call, so a
        // change takes effect on the audio already being buffered rather
        // than only on the next call.
        main_window.on_replay_settings_changed(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let seconds = app
                .get_replay_seconds()
                .clamp(state::REPLAY_SECONDS_MIN as i32, state::REPLAY_SECONDS_MAX as i32);
            app.set_replay_seconds(seconds);
            state.replay_seconds.store(seconds as u32, Ordering::Relaxed);
            if app.get_replay_mode() == 0 {
                app.set_replay_notice("".into());
            }
            save_current_config(&app);
        });
    }
    {
        let state = app_state.clone();
        main_window.on_react_to_message(move |id, emoji| {
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                active.react(&state.runtime, id as u64, emoji.to_string());
            }
        });
    }
    {
        let state = app_state.clone();
        main_window.on_pin_chat_message(move |id, pinned| {
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                active.set_chat_pinned(id as u64, pinned);
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §13 disappearing messages, per contact.
        main_window.on_chat_expire_changed(move |secs| {
            let Some(app) = window_weak.upgrade() else { return };
            let peer_key = {
                let call = state.call.lock().unwrap();
                call.as_ref().map(|c| encode_identity_key(&c.peer_identity().identity_key))
            };
            let Some(peer_key) = peer_key else { return };

            let mut config = config::load();
            if let Some(contact) = config.contacts.iter_mut().find(|c| c.identity_key == peer_key) {
                contact.expire_after_secs = (secs > 0).then_some(secs as u64);
                config::save(&config);
                // Takes effect for messages sent from here on. Already-sent
                // messages keep whatever expiry they were given, rather than
                // retroactively acquiring or losing one.
                app.set_chat_expire_secs(secs);
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §1 "Per-User Volume". Applied live to the playback loop and
        // saved against the contact, so it is still there next call.
        main_window.on_peer_audio_changed(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let volume = app.get_peer_volume_db();
            let muted = app.get_peer_muted();
            let bands = peer_tone_bands(
                app.get_peer_bass_db(),
                app.get_peer_voice_db(),
                app.get_peer_treble_db(),
            );

            *state.peer_volume_db.lock().unwrap() = volume;
            state.peer_muted.store(muted, Ordering::Relaxed);
            // Rebuilt rather than retuned so the change is audible on the
            // next frame -- dragging a tone slider mid-call is exactly when
            // you want to hear the result.
            *state.peer_eq.lock().unwrap() = peer_eq_chain(&bands);

            let peer_key = {
                let call = state.call.lock().unwrap();
                call.as_ref().map(|c| encode_identity_key(&c.peer_identity().identity_key))
            };
            let Some(peer_key) = peer_key else { return };

            let mut config = config::load();
            if let Some(contact) = config.contacts.iter_mut().find(|c| c.identity_key == peer_key) {
                contact.volume_db = volume;
                contact.muted = muted;
                contact.eq_bands = bands;
                config::save(&config);
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §9 "Search over your own history, indexed locally on your
        // device". Entirely local: it reads the same encrypted file the
        // call writes, and nothing is sent anywhere.
        main_window.on_search_history(move |query| {
            let Some(app) = window_weak.upgrade() else { return };
            let query = query.to_string();
            if query.trim().is_empty() {
                app.set_history_result("".into());
                return;
            }

            let sink = {
                let call = state.call.lock().unwrap();
                call.as_ref().and_then(|active| active.history.clone())
            };
            let Some(sink) = sink else {
                app.set_history_result(
                    "History is only kept for saved contacts.".into(),
                );
                return;
            };

            let log = sink.load_all();
            let hits = history::search(&log, &query);
            if hits.is_empty() {
                app.set_history_result(format!("No messages matching \"{query}\".").into());
                return;
            }
            // The most recent few, newest last, so the tail of the result
            // reads in the same order as the conversation itself.
            let preview: Vec<String> = hits
                .iter()
                .rev()
                .take(5)
                .rev()
                .map(|m| format!("{}{}", if m.from_me { "you: " } else { "them: " }, m.display_text()))
                .collect();
            app.set_history_result(
                format!("{} match(es)\n{}", hits.len(), preview.join("\n")).into(),
            );
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_forget_history(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            let contacts = contacts_from_ui(&app);
            let Some(contact) = contacts.get(index as usize) else { return };
            if contact.identity_key.is_empty() {
                app.set_contacts_message("No stored messages for that contact.".into());
                return;
            }
            let Some(dir) = config::config_dir() else { return };
            match history::forget(&dir, &contact.identity_key) {
                Ok(()) => app.set_contacts_message(
                    format!("Deleted stored messages with {}.", contact.name).into(),
                ),
                Err(e) => app.set_contacts_message(format!("Could not delete: {e}").into()),
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §3 "Backup & recovery". Rendered on demand rather than held
        // in a UI property from startup: the phrase is the whole identity
        // in plain text, so it should exist in the window for as long as
        // someone is actually looking at it and no longer.
        main_window.on_toggle_recovery_phrase(move || {
            let Some(app) = window_weak.upgrade() else { return };
            if app.get_recovery_phrase_shown() {
                app.set_recovery_phrase("".into());
                app.set_recovery_phrase_shown(false);
                return;
            }
            let phrase = antiphon_crypto::mnemonic::phrase_from_seed(&state.identity.master_seed());
            app.set_recovery_phrase(phrase.into());
            app.set_recovery_phrase_shown(true);

            // Seeing it is not proof it was written down -- nothing can be
            // -- but it is the difference between "never saw it" and
            // "chose not to", which is enough to stop nagging.
            let mut config = config::load();
            if !config.first_run.phrase_seen {
                config.first_run.phrase_seen = true;
                config::save(&config);
                apply_onboarding(&app, &config);
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §3 "optional Shamir split". Made fresh on each show, like
        // the phrase, so the shares exist in the window only while
        // someone is looking at them.
        main_window.on_toggle_recovery_shares(move || {
            let Some(app) = window_weak.upgrade() else { return };
            if app.get_recovery_shares_shown() {
                app.set_recovery_shares("".into());
                app.set_recovery_shares_shown(false);
                return;
            }
            let (need, of) = match app.get_recovery_split_choice() {
                1 => (3, 5),
                2 => (2, 2),
                _ => (2, 3),
            };
            let Ok(shares) = antiphon_crypto::shamir::split(&state.identity.master_seed(), need, of) else { return };
            let text = shares.iter().map(|s| s.to_text()).collect::<Vec<_>>().join("\n\n");
            app.set_recovery_shares(text.into());
            app.set_recovery_shares_shown(true);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_copy_share_id(move || {
            let Some(app) = window_weak.upgrade() else { return };
            // Slint has no clipboard API, so the honest fallback is to
            // select-and-copy from the field itself. Say so rather than
            // silently doing nothing.
            let id = app.get_my_share_id();
            app.set_import_export_message(format!("Your ID: {id}").into());
        });
    }
    {
        let window_weak = main_window.as_weak();
        // Settings search (spec §0). Filtering happens here rather than in
        // the UI because the section orders are already the thing the
        // rendering loops walk -- narrowing them is the whole feature.
        main_window.on_settings_search_changed(move |query| {
            let Some(app) = window_weak.upgrade() else { return };
            let saved = config::load();
            let query = query.to_string();

            app.set_audio_order(string_vec_to_model(search::filter(
                &config::sanitize_order(saved.audio_section_order.clone(), config::AUDIO_SECTION_IDS),
                search::Category::Audio,
                &query,
            )));
            app.set_appearance_order(string_vec_to_model(search::filter(
                &config::sanitize_order(
                    saved.appearance_section_order.clone(),
                    config::APPEARANCE_SECTION_IDS,
                ),
                search::Category::Appearance,
                &query,
            )));
            app.set_profile_order(string_vec_to_model(search::filter(
                &config::sanitize_order(saved.profile_section_order.clone(), config::PROFILE_SECTION_IDS),
                search::Category::Profile,
                &query,
            )));
            app.set_call_order(string_vec_to_model(search::filter(
                &config::sanitize_order(saved.call_section_order.clone(), config::CALL_SECTION_IDS),
                search::Category::Call,
                &query,
            )));
            app.set_show_audio_fixed_block(
                search::Category::Audio.has_always_visible_match(&query),
            );
            app.set_settings_search_summary(search::summary(&query).into());
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_settings_changed(move || {
            let Some(app) = window_weak.upgrade() else { return };
            apply_settings_change(&app, &state);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_eq_toggle(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            edit_eq_band(&app, index, |b| b.enabled = !b.enabled);
            apply_settings_change(&app, &state);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_eq_set_freq(move |index, text| {
            let Some(app) = window_weak.upgrade() else { return };
            if let Ok(v) = text.parse::<f32>() {
                edit_eq_band(&app, index, |b| b.center_hz = v.clamp(20.0, 20_000.0));
                apply_settings_change(&app, &state);
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_eq_set_gain(move |index, text| {
            let Some(app) = window_weak.upgrade() else { return };
            if let Ok(v) = text.parse::<f32>() {
                edit_eq_band(&app, index, |b| b.gain_db = v.clamp(-24.0, 24.0));
                apply_settings_change(&app, &state);
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_eq_set_q(move |index, text| {
            let Some(app) = window_weak.upgrade() else { return };
            if let Ok(v) = text.parse::<f32>() {
                edit_eq_band(&app, index, |b| b.q = v.clamp(0.1, 20.0));
                apply_settings_change(&app, &state);
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_eq_add_band(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let mut bands = eq_bands_from_ui(&app);
            bands.push(EqBandConfig {
                enabled: true,
                center_hz: 1000.0,
                gain_db: 0.0,
                q: 1.0,
                band_type: antiphon_audio::BandType::Bell,
            });
            apply_eq_bands_to_ui(&app, &bands);
            apply_settings_change(&app, &state);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_eq_set_type(move |index, band_type| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut bands = eq_bands_from_ui(&app);
            if let Ok(i) = usize::try_from(index) {
                if let Some(band) = bands.get_mut(i) {
                    band.band_type = antiphon_audio::BandType::from_i32(band_type);
                }
            }
            apply_eq_bands_to_ui(&app, &bands);
            apply_settings_change(&app, &state);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §1 starter presets. Replaces the band list wholesale rather
        // than merging: a preset the user cannot predict the result of is
        // worse than no preset, and everything stays editable afterwards.
        main_window.on_eq_load_preset(move |name| {
            let Some(app) = window_weak.upgrade() else { return };
            let Some(bands) = antiphon_audio::eq_preset(&name) else { return };
            apply_eq_bands_to_ui(&app, &bands);
            apply_settings_change(&app, &state);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_eq_remove_band(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut bands = eq_bands_from_ui(&app);
            if let Ok(i) = usize::try_from(index) {
                if i < bands.len() {
                    bands.remove(i);
                }
            }
            apply_eq_bands_to_ui(&app, &bands);
            apply_settings_change(&app, &state);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_eq_curve_drag(move |x_frac, y_frac| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut bands = eq_bands_from_ui(&app);
            let freq = eq_curve_x_to_freq(x_frac);
            let gain = eq_curve_y_to_db(y_frac);

            // No separate "which band is being dragged" state needed: each
            // tick just grabs whichever band is nearest the current mouse
            // position, and since the previous tick just moved that same
            // band to (almost) exactly where the mouse is now, it stays the
            // nearest one for as long as the drag continues smoothly.
            let index = nearest_band_within_reach(&bands, freq).unwrap_or_else(|| {
                bands.push(EqBandConfig {
                    enabled: true,
                    center_hz: freq,
                    gain_db: 0.0,
                    q: 1.0,
                    band_type: antiphon_audio::BandType::Bell,
                });
                bands.len() - 1
            });

            if let Some(band) = bands.get_mut(index) {
                band.center_hz = freq;
                band.gain_db = gain.clamp(-18.0, 18.0);
            }
            apply_eq_bands_to_ui(&app, &bands);
            apply_settings_change(&app, &state);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_device_changed(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let (input, output) = selected_devices(&app);
            save_current_config(&app);

            // Device Hot-Swap (spec §1 Utility): swap the running stream
            // live instead of waiting for the next call.
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                if let Err(e) = active.swap_input_device(input.as_deref()) {
                    set_status_only(&app, &format!("Couldn't switch microphone: {e}"));
                    return;
                }
                if let Err(e) = active.swap_output_device(output.as_deref()) {
                    set_status_only(&app, &format!("Couldn't switch output device: {e}"));
                }
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_profile_changed(move || {
            let Some(app) = window_weak.upgrade() else { return };
            update_display_initial(&app);
            save_current_config(&app);

            // Push the change to whoever's on the call now, rather than
            // leaving them with the name/status from when the call started.
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                let peer = active.peer.clone();
                let hello = local_hello(&app);
                state.runtime.spawn(async move {
                    let _ = peer.send_control(&ControlMessage::Hello(hello).encode()).await;
                });
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_accent_color_picker_changed(move || {
            let Some(app) = window_weak.upgrade() else { return };
            // Deliberately doesn't go through `set_accent_color` (which
            // re-derives hue/sat/val from the resulting RGB): the picker
            // itself just set hue/sat/val directly, and re-deriving them
            // from an RGB round-trip risks a float-precision nudge that'd
            // make the indicator visibly jitter mid-drag.
            let (r, g, b) = hsv_to_rgb(app.get_accent_hue(), app.get_accent_sat(), app.get_accent_val());
            let color = slint::Color::from_rgb_u8(r, g, b);
            app.set_accent_choice(color);
            app.set_accent_hex_display(color_to_hex(color).into());
            let (pr, pg, pb) = hsv_to_rgb(app.get_accent_hue(), 1.0, 1.0);
            app.set_accent_hue_preview(slint::Color::from_rgb_u8(pr, pg, pb));
            save_current_config(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_accent_hex_changed(move |text| {
            let Some(app) = window_weak.upgrade() else { return };
            // Invalid input (typo, incomplete paste mid-edit) just leaves
            // the accent as it was rather than snapping to some fallback --
            // the hex field itself gets reset to match on the next redraw
            // since its `text` is bound from `accent-hex-display`, which
            // only Rust writes to.
            if let Some(color) = color_from_hex(&text) {
                set_accent_color(&app, color);
                save_current_config(&app);
            } else {
                app.set_accent_hex_display(color_to_hex(app.get_accent_choice()).into());
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_theme_bg_color_picker_changed(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let (r, g, b) = hsv_to_rgb(app.get_theme_bg_hue(), app.get_theme_bg_sat(), app.get_theme_bg_val());
            let color = slint::Color::from_rgb_u8(r, g, b);
            app.set_theme_bg_color(color);
            app.set_theme_bg_hex_display(color_to_hex(color).into());
            let (pr, pg, pb) = hsv_to_rgb(app.get_theme_bg_hue(), 1.0, 1.0);
            app.set_theme_bg_hue_preview(slint::Color::from_rgb_u8(pr, pg, pb));
            save_current_config(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_theme_bg_hex_changed(move |text| {
            let Some(app) = window_weak.upgrade() else { return };
            if let Some(color) = color_from_hex(&text) {
                set_theme_bg_color(&app, color);
                save_current_config(&app);
            } else {
                app.set_theme_bg_hex_display(color_to_hex(app.get_theme_bg_color()).into());
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_import_theme_image(move || {
            if window_weak.upgrade().is_none() {
                return;
            }
            let window_weak2 = window_weak.clone();
            std::thread::spawn(move || {
                // Picking the file happens off the UI thread (it blocks);
                // decoding (a GIF's frames especially) is also blocking
                // work, so it happens here too, not after hopping back to
                // the event loop -- only setting the resulting image(s) on
                // `app` needs to run there.
                let Some(path) = rfd::FileDialog::new()
                    .set_title("Choose a background image")
                    .add_filter("Image files", &["png", "jpg", "jpeg", "bmp", "gif", "webp", "tiff", "svg"])
                    .pick_file()
                else {
                    return;
                };
                let raw = animated_image::load_raw(&path);
                slint::invoke_from_event_loop(move || {
                    let Some(app) = window_weak2.upgrade() else { return };
                    let result = raw.and_then(|raw| {
                        app.set_theme_bg_image_set(true);
                        app.set_theme_image_message("".into());
                        animated_image::apply_raw_loaded(
                            &app,
                            window_weak2.clone(),
                            raw,
                            animated_image::ImageSlot::ThemeBackground,
                            Rc::new(|app: &MainWindow, img| app.set_theme_bg_image(img)),
                        )
                    });
                    match result {
                        Ok(()) => {
                            let mut cfg = current_config(&app);
                            cfg.theme_image_path = path.to_string_lossy().to_string();
                            config::save(&cfg);
                        }
                        Err(e) => {
                            app.set_theme_image_message(format!("Couldn't load that image: {e}").into());
                        }
                    }
                })
                .ok();
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_send_file(move || {
            if window_weak.upgrade().is_none() {
                return;
            }
            let state2 = state.clone();
            let window_weak2 = window_weak.clone();
            std::thread::spawn(move || {
                let Some(path) = rfd::FileDialog::new().set_title("Send a file").pick_file() else { return };
                let file_name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".to_string());

                // Spec §5: hash it now, so the bytes that were verified
                // are the bytes that get sent. Reading again later would
                // reopen the window where the file changes underneath.
                // Hashing a large file takes a moment, and a click that
                // produces no response for several seconds reads as a
                // click that did not register.
                {
                    let weak = window_weak2.clone();
                    let name = file_name.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(app) = weak.upgrade() {
                            app.set_ft_send_error("".into());
                            app.set_ft_status_message(format!("Preparing {name}...").into());
                        }
                    })
                    .ok();
                }
                let (source, sha256) = match file_integrity::open_and_hash(&path) {
                    Ok(opened) => opened,
                    Err(reason) => {
                        let message = reason.explain(&file_name);
                        let weak = window_weak2.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(app) = weak.upgrade() {
                                app.set_ft_send_error(message.into());
                            }
                        })
                        .ok();
                        return;
                    }
                };
                // Spec §5: the *sender* makes the preview. A receiver that
                // decoded an image to show a thumbnail would be running a
                // decoder over a file it has not agreed to accept yet.
                let thumbnail = file_preview::generate(&path)
                    .map(|png| file_transfer::encode_b64(&png))
                    .unwrap_or_default();
                let with = {
                    let call = state2.call.lock().unwrap();
                    let Some(active) = call.as_ref() else { return };
                    active.send_file(&state2.runtime, file_name.clone(), source, sha256, thumbnail);
                    crate::delivery::hex(&active.peer_identity().identity_key)
                };
                // Spec §5: also seed and pin it, so it can be fetched
                // after the call. Sealing a big file takes a while; the
                // direct transfer is already under way meanwhile.
                if let Some(link) = sharing::share(&state2.runtime, &path, &file_name, &with) {
                    if let Some(active) = state2.call.lock().unwrap().as_ref() {
                        active.send_control(&state2.runtime, link);
                    }
                }
                let weak = window_weak2.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(app) = weak.upgrade() {
                        app.set_ft_status_message("".into());
                    }
                })
                .ok();
            });
        });
    }
    {
        let state = app_state.clone();
        let window_weak = main_window.as_weak();
        main_window.on_accept_file_offer(move || {
            let problem = {
                let call = state.call.lock().unwrap();
                call.as_ref().and_then(|active| active.accept_file_offer(&state.runtime))
            };
            if let (Some(why), Some(app)) = (problem, window_weak.upgrade()) {
                app.set_ft_status_message(why.into());
            }
        });
    }
    {
        let state = app_state.clone();
        main_window.on_reject_file_offer(move || {
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                active.reject_file_offer(&state.runtime);
            }
        });
    }
    {
        let state = app_state.clone();
        main_window.on_cancel_file_transfer(move || {
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                active.cancel_file_transfer(&state.runtime);
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_send_chat_message(move |text| {
            let Some(app) = window_weak.upgrade() else { return };
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                active.send_chat(&state.runtime, text.to_string());
                // Cleared here (not in .slint) so both the Enter-key and
                // Send-button paths clear it from one place.
                app.set_chat_input("".into());
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §10: ask the router for an inbound port, so being *called*
        // works and not only calling out. Separate from STUN on purpose --
        // STUN says what your address looks like, this is what makes it
        // reachable, and conflating them would make a failure of either
        // read as a failure of both.
        main_window.on_open_port_automatically(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let local_port: u16 = app.get_local_port().parse().unwrap_or(0);
            if local_port == 0 {
                app.set_nat_status("Set a port to listen on first.".into());
                return;
            }

            // Already open: this is the off switch.
            if let Some(handle) = state.port_map.lock().unwrap().take() {
                handle.close(&state.runtime);
                app.set_port_mapped(false);
                app.set_nat_status("Closed the port your router was forwarding.".into());
                apply_reachability(&app, &current_reachability(&app));
                return;
            }

            app.set_nat_busy(true);
            app.set_nat_status("Asking your router to open a port...".into());
            let window_weak2 = window_weak.clone();
            let state2 = state.clone();
            state.runtime.spawn(async move {
                let result = state::open_port_automatically(local_port).await;
                slint::invoke_from_event_loop(move || {
                    let Some(app) = window_weak2.upgrade() else { return };
                    app.set_nat_busy(false);
                    match result {
                        Ok(handle) => {
                            let external = handle.external_port;
                            app.set_port_mapped(true);
                            app.set_port_forward_refused(false);
                            // The external port is reported rather than
                            // assumed: a router is allowed to hand back a
                            // different number, and telling people the one
                            // that was asked for would send them to a port
                            // nobody is listening on.
                            app.set_nat_status(
                                if external == local_port {
                                    format!("Your router is forwarding port {external}. It is renewed automatically while the app is open.")
                                } else {
                                    format!("Your router forwarded port {external} instead of {local_port} -- share the address with :{external}.")
                                }
                                .into(),
                            );
                            *state2.port_map.lock().unwrap() = Some(handle);
                            apply_reachability(&app, &current_reachability(&app));
                        }
                        Err(e) => {
                            app.set_port_mapped(false);
                            // Asked and refused, which is a different fact
                            // from never having asked -- one is a router
                            // to look at, the other is a button nobody
                            // pressed.
                            app.set_port_forward_refused(true);
                            app.set_nat_status(format!("{e}").into());
                            apply_reachability(&app, &current_reachability(&app));
                        }
                    }
                })
                .ok();
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_discover_public_address(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let local_port: u16 = app.get_local_port().parse().unwrap_or(0);
            app.set_nat_busy(true);
            app.set_nat_status("Asking a STUN server...".into());

            let window_weak2 = window_weak.clone();
            let state2 = state.clone();
            state.runtime.spawn(async move {
                let result = state::discover_public_address(local_port).await;
                slint::invoke_from_event_loop(move || {
                    let Some(app) = window_weak2.upgrade() else { return };
                    app.set_nat_busy(false);
                    match result {
                        Ok((public, socket)) => {
                            // Hold the socket so Punch reuses it -- the
                            // mapping just discovered belongs to it alone.
                            *state2.punch_socket.lock().unwrap() = Some(socket);
                            app.set_public_address(public.to_string().into());
                            app.set_nat_status("Send that address to the other person.".into());
                            apply_reachability(&app, &current_reachability(&app));
                        }
                        Err(e) => {
                            app.set_public_address("".into());
                            app.set_nat_status(format!("Couldn't find it: {e}").into());
                            apply_reachability(&app, &current_reachability(&app));
                        }
                    }
                })
                .ok();
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        let tray_weak = tray.as_weak();
        let state = app_state.clone();
        main_window.on_start_punch(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let remote_str = app.get_remote_address().to_string();
            let Ok(remote) = remote_str.parse() else {
                app.set_nat_status(format!("Invalid address: {remote_str}").into());
                return;
            };
            let local_port: u16 = app.get_local_port().parse().unwrap_or(0);
            let existing = state.punch_socket.lock().unwrap().take();
            let chain = read_chain_config(&app);
            let (input, output) = selected_devices(&app);
            let hello = local_hello(&app);

            app.set_nat_busy(true);
            app.set_nat_status("Punching -- the other side has to be doing this too.".into());
            set_status(&app, &tray_weak, "Punching...");

            let window_weak2 = window_weak.clone();
            let tray_weak2 = tray_weak.clone();
            let state2 = state.clone();
            let identity = state.identity.clone();
            state.runtime.spawn(async move {
                let result = state::establish_punch(remote, local_port, existing, identity).await;
                let failed = result.as_ref().err().map(|e| e.to_string());
                let window_weak3 = window_weak2.clone();
                finish_connect(window_weak2, tray_weak2, state2, result, chain, input, output, hello);
                slint::invoke_from_event_loop(move || {
                    let Some(app) = window_weak3.upgrade() else { return };
                    app.set_nat_busy(false);
                    app.set_nat_status(match failed {
                        Some(e) => e,
                        None => "Connected.".to_string(),
                    }
                    .into());
                })
                .ok();
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_save_preset(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let name = app.get_new_preset_name().trim().to_string();
            if name.is_empty() {
                return;
            }
            let mut cfg = current_config(&app);
            let preset = config::VoicePreset { name: name.clone(), chain: read_chain_config(&app) };
            // Saving under an existing name overwrites it, which is what
            // "save" means everywhere else -- rather than silently making a
            // second preset with the same label.
            match cfg.voice_presets.iter_mut().find(|p| p.name == name) {
                Some(existing) => *existing = preset,
                None => cfg.voice_presets.push(preset),
            }
            apply_presets_to_ui(&app, &cfg.voice_presets);
            app.set_new_preset_name("".into());
            app.set_preset_message(format!("Saved \"{name}\"").into());
            config::save(&cfg);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_load_preset(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            let cfg = config::load();
            let Some(preset) = cfg.voice_presets.get(index as usize).cloned() else { return };
            apply_chain_to_ui(&app, &preset.chain);
            // Push it at a live call too, so loading a preset mid-call is
            // audible immediately rather than on the next one.
            apply_settings_change(&app, &state);
            app.set_preset_message(format!("Loaded \"{}\"", preset.name).into());
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_delete_preset(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut cfg = current_config(&app);
            let index = index as usize;
            if index >= cfg.voice_presets.len() {
                return;
            }
            let removed = cfg.voice_presets.remove(index);
            apply_presets_to_ui(&app, &cfg.voice_presets);
            app.set_preset_message(format!("Deleted \"{}\"", removed.name).into());
            config::save(&cfg);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_export_preset(move |index| {
            // Only a liveness guard -- the dialog thread uses its own weak
            // handle, so nothing here needs the upgraded window itself.
            if window_weak.upgrade().is_none() {
                return;
            }
            let cfg = config::load();
            let Some(preset) = cfg.voice_presets.get(index as usize).cloned() else { return };
            let window_weak2 = window_weak.clone();
            std::thread::spawn(move || {
                let Some(path) = rfd::FileDialog::new()
                    .set_title("Export voice preset")
                    .add_filter("Stoop preset", &["json"])
                    .set_file_name(format!("{}.json", preset.name))
                    .save_file()
                else {
                    return;
                };
                let message = match config::save_preset_to(&path, &preset) {
                    Ok(()) => format!("Exported \"{}\" to {}", preset.name, path.display()),
                    Err(e) => format!("Export failed: {e}"),
                };
                slint::invoke_from_event_loop(move || {
                    if let Some(app) = window_weak2.upgrade() {
                        app.set_preset_message(message.into());
                    }
                })
                .ok();
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_import_preset(move || {
            if window_weak.upgrade().is_none() {
                return;
            }
            let window_weak2 = window_weak.clone();
            std::thread::spawn(move || {
                let Some(path) = rfd::FileDialog::new()
                    .set_title("Import a voice preset")
                    .add_filter("Stoop preset", &["json"])
                    .pick_file()
                else {
                    return;
                };
                let result = config::load_preset_from(&path);
                slint::invoke_from_event_loop(move || {
                    let Some(app) = window_weak2.upgrade() else { return };
                    match result {
                        Ok(preset) => {
                            let mut cfg = current_config(&app);
                            let name = preset.name.clone();
                            match cfg.voice_presets.iter_mut().find(|p| p.name == name) {
                                Some(existing) => *existing = preset,
                                None => cfg.voice_presets.push(preset),
                            }
                            apply_presets_to_ui(&app, &cfg.voice_presets);
                            app.set_preset_message(format!("Imported \"{name}\" -- press Load to use it").into());
                            config::save(&cfg);
                        }
                        Err(e) => app.set_preset_message(format!("Couldn't read that preset: {e}").into()),
                    }
                })
                .ok();
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_add_contact(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let name = app.get_new_contact_name().trim().to_string();
            let address = app.get_new_contact_address().trim().to_string();
            if name.is_empty() || address.is_empty() {
                app.set_contacts_message("Name and address are both required".into());
                return;
            }
            let mut contacts = contacts_from_ui(&app);
            if contacts.iter().any(|c| c.address == address) {
                app.set_contacts_message(format!("{address} is already saved").into());
                return;
            }
            contacts.push(config::Contact {
                name: name.clone(),
                address,
                is_server: app.get_new_contact_is_server(),
                ..Default::default()
            });
            apply_contacts_to_ui(&app, &contacts);
            app.set_new_contact_name("".into());
            app.set_new_contact_address("".into());
            app.set_new_contact_is_server(false);
            app.set_contacts_message(format!("Saved {name}").into());
            save_current_config(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §4: the user confirming they compared the code out of band.
        // This is the only path that raises trust to Verified, and the only
        // one that accepts a changed key -- both require a human to have
        // actually checked, which is the entire point of the code.
        main_window.on_mark_peer_verified(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let Some((peer_key, remote_addr)) = ({
                let call = state.call.lock().unwrap();
                call.as_ref().map(|active| {
                    (active.peer_identity().identity_key, active.remote_addr())
                })
            }) else {
                return;
            };

            let mut config = config::load();
            let Some(contact) = config.contacts.iter_mut().find(|c| c.address == remote_addr)
            else {
                app.set_peer_trust_message(
                    "Save them as a contact first, then this can be remembered.".into(),
                );
                return;
            };

            let name = contact.name.clone();
            contact.identity_key = encode_identity_key(&peer_key);
            contact.verified = true;
            if contact.first_seen_secs == 0 {
                contact.first_seen_secs = now_secs();
            }
            config::save(&config);

            app.set_peer_verified(true);
            app.set_peer_key_changed(false);
            app.set_peer_trust_message(format!("Verified {name}.").into());
            apply_contacts_to_ui(&app, &config.contacts);
        });
    }
    {
        let window_weak = main_window.as_weak();
        // Spec §13 first-run. Dismissing advances whichever prompt is
        // showing, so "Got it" always means the thing in front of you.
        main_window.on_dismiss_onboarding(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let mut config = config::load();
            let prompt = current_prompt(&config);
            match prompt {
                first_run::Prompt::Welcome => config.first_run.welcomed = true,
                first_run::Prompt::SuggestBackup => {
                    config.first_run.backup_prompt_dismissed = true
                }
                first_run::Prompt::SuggestVerify => {
                    config.first_run.verify_prompt_dismissed = true
                }
                first_run::Prompt::None => return,
            }
            config::save(&config);
            apply_onboarding(&app, &config);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_block_peer(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let peer = {
                let call = state.call.lock().unwrap();
                call.as_ref().map(|c| c.peer_identity().identity_key)
            };
            let Some(peer) = peer else { return };
            let hex = encode_identity_key(&peer);

            let mut config = config::load();
            let petname = config
                .contacts
                .iter()
                .find(|c| c.identity_key == hex)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            moderation::block(&mut config.blocked, &hex, &petname, "", now_secs());
            config::save(&config);
            apply_blocked_to_ui(&app, &config.blocked);

            // Hang up immediately: leaving the call running would mean
            // "blocked" took effect only for the *next* call, which is not
            // what anyone pressing this button expects.
            *state.call.lock().unwrap() = None;
            app.set_connected(false);
            app.set_peer_blocked(true);
            app.set_moderation_message(
                "Blocked. They are not told, and calls from them will be refused.".into(),
            );
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_unblock_at(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut config = config::load();
            let Ok(i) = usize::try_from(index) else { return };
            let Some(entry) = config.blocked.get(i).cloned() else { return };

            moderation::unblock(&mut config.blocked, &entry.identity_key);
            config::save(&config);
            apply_blocked_to_ui(&app, &config.blocked);
            app.set_moderation_message(
                format!("Unblocked {}.", if entry.petname.is_empty() { "them" } else { &entry.petname })
                    .into(),
            );
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_report_peer(move |note| {
            let Some(app) = window_weak.upgrade() else { return };
            let peer = {
                let call = state.call.lock().unwrap();
                call.as_ref().map(|c| c.peer_identity().identity_key)
            };
            let Some(peer) = peer else { return };
            let hex = encode_identity_key(&peer);

            let mut config = config::load();
            let petname = config
                .contacts
                .iter()
                .find(|c| c.identity_key == hex)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            moderation::report(&mut config.reports, &hex, &petname, &note, now_secs());
            config::save(&config);
            app.set_moderation_message(
                "Saved on this device. Nothing was sent -- nothing can read a call, so a                  report cannot carry evidence of one."
                    .into(),
            );
        });
    }
    {
        let state = app_state.clone();
        main_window.on_send_silent_marker(move || {
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                active.announce_silent(&state.runtime);
            }
        });
    }
    {
        let state = app_state.clone();
        main_window.on_set_transfer_paused(move |paused| {
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                file_transfer::set_paused(&active.file_transfer, paused);
            }
        });
    }
    {
        let state = app_state.clone();
        main_window.on_escalate_call(move || {
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                active.escalate(&state.runtime);
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §4 contact introduction: the one discovery mechanism that
        // needs no infrastructure at all -- an existing encrypted channel
        // is the whole transport.
        main_window.on_introduce_contact(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            let config = config::load();
            let Ok(i) = usize::try_from(index) else { return };
            let Some(contact) = config.contacts.get(i) else { return };
            if contact.identity_key.is_empty() {
                app.set_introduction_message(
                    "That contact has no key pinned yet -- call them once first.".into(),
                );
                return;
            }

            let call = state.call.lock().unwrap();
            let Some(active) = call.as_ref() else { return };

            // Introducing someone to themselves proves nothing and is
            // almost certainly a misclick.
            let peer_hex = encode_identity_key(&active.peer_identity().identity_key);
            if peer_hex.eq_ignore_ascii_case(&contact.identity_key) {
                app.set_introduction_message(
                    format!("{} is the person you're talking to.", contact.name).into(),
                );
                return;
            }

            active.introduce(
                &state.runtime,
                contact.identity_key.clone(),
                contact.name.clone(),
                String::new(),
            );
            app.set_introduction_message(
                format!(
                    "Sent {}'s key. They'll still need to compare a code before trusting it.",
                    contact.name
                )
                .into(),
            );
        });
    }
    {
        let window_weak = main_window.as_weak();
        // Spec §9: this is the receiver deciding who may ring them, so it
        // is stored against the contact and read on every incoming call.
        main_window.on_set_contact_ring_rule(move |index, rule| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut config = config::load();
            let Ok(i) = usize::try_from(index) else { return };
            let Some(contact) = config.contacts.get_mut(i) else { return };

            // Round-tripped through the enum rather than stored raw, so an
            // out-of-range value from the UI cannot become a rule nothing
            // understands.
            contact.ring_rule = notify::RingRule::from_i32(rule).to_i32();
            let name = contact.name.clone();
            config::save(&config);
            apply_contacts_to_ui(&app, &config.contacts);
            app.set_contacts_message(
                match notify::RingRule::from_i32(rule) {
                    notify::RingRule::Always => format!("{name} can ring you."),
                    notify::RingRule::SilentOnly => format!("{name} will show up silently."),
                    notify::RingRule::Never => format!("{name} won't notify you at all."),
                }
                .into(),
            );
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_remove_contact(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut contacts = contacts_from_ui(&app);
            let index = index as usize;
            if index >= contacts.len() {
                return;
            }
            let removed = contacts.remove(index);
            apply_contacts_to_ui(&app, &contacts);
            app.set_contacts_message(format!("Removed {}", removed.name).into());
            save_current_config(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let tray_weak = tray.as_weak();
        let state = app_state.clone();
        main_window.on_call_contact(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            let contacts = config::load().contacts;
            let Some(contact) = usize::try_from(index).ok().and_then(|i| contacts.get(i)) else { return };
            // Spec §3: during a call, the same button adds them to it.
            if app.get_connected() {
                add_to_group_call(&app, &state, contact.clone());
                return;
            }
            // Spec §3: direct for known contacts, through a relay for
            // everyone else, unless this conversation says otherwise.
            match connection_mode::plan(contact, &state.identity, now_secs()) {
                Err(reason) => app.set_contacts_message(reason.into()),
                Ok(connection_mode::Plan::Direct { address }) if config::load().dht_enabled && !contact.identity_key.is_empty() => {
                    // Spec §4: with the DHT on, a saved address that
                    // doesn't answer is looked up before giving up, and
                    // then -- unless this contact is set to Direct -- their
                    // relay is tried.
                    *state.relay_addr.lock().unwrap() = None;
                    app.set_remote_address(address.clone().into());
                    let local_port: u16 = app.get_local_port().parse().unwrap_or(0);
                    let config = read_chain_config(&app);
                    let (input, output) = selected_devices(&app);
                    let hello = local_hello(&app);
                    set_status(&app, &tray_weak, &format!("Calling {}...", contact.name));
                    let (window_weak2, tray_weak2, state2) = (window_weak.clone(), tray_weak.clone(), state.clone());
                    let identity = state.identity.clone();
                    let contact = contact.clone();
                    let now = now_secs();
                    state.runtime.spawn(async move {
                        let result = call_with_lookup(&contact, address, local_port, identity, &state2, &window_weak2, now).await;
                        finish_connect(window_weak2, tray_weak2, state2, result, config, input, output, hello);
                    });
                }
                Ok(connection_mode::Plan::Direct { address }) => {
                    // A direct contact call goes down the exact same path
                    // as a typed one rather than duplicating it.
                    *state.relay_addr.lock().unwrap() = None;
                    app.set_remote_address(address.into());
                    app.invoke_start_call();
                }
                Ok(connection_mode::Plan::Relay { relay, token }) => {
                    let local_port: u16 = app.get_local_port().parse().unwrap_or(0);
                    let config = read_chain_config(&app);
                    let (input, output) = selected_devices(&app);
                    let hello = local_hello(&app);
                    set_status(&app, &tray_weak, &format!("Calling {} through a relay...", contact.name));
                    let (window_weak2, tray_weak2, state2) = (window_weak.clone(), tray_weak.clone(), state.clone());
                    let identity = state.identity.clone();
                    state.runtime.spawn(async move {
                        let result = async {
                            let addr = state::resolve(&relay).await?;
                            *state2.relay_addr.lock().unwrap() = Some(addr);
                            state::establish_relayed_call(addr, token, local_port, identity).await
                        }
                        .await;
                        finish_connect(window_weak2, tray_weak2, state2, result, config, input, output, hello);
                    });
                }
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_set_contact_message_route(move |index, mode| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut config = config::load();
            let Some(contact) = usize::try_from(index).ok().and_then(|i| config.contacts.get_mut(i)) else { return };
            contact.message_route = mode.clamp(0, 1);
            let message = if contact.message_route == 1 {
                format!("Messages to {} wait until you're both online. Nothing is left anywhere.", contact.name)
            } else {
                format!("Messages to {} use the routes they chose.", contact.name)
            };
            config::save(&config);
            apply_contacts_to_ui(&app, &config.contacts);
            app.set_contacts_message(message.into());
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_leave_message(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            let config = config::load();
            let Some(contact) = usize::try_from(index).ok().and_then(|i| config.contacts.get(i)) else { return };
            let text = app.get_leave_message_text().to_string();
            // On a call with them: it is just a chat message.
            let in_call = {
                let call = state.call.lock().unwrap();
                match call.as_ref() {
                    Some(active) if encode_identity_key(&active.peer_identity().identity_key) == contact.identity_key => {
                        active.send_chat(&state.runtime, text.clone());
                        true
                    }
                    _ => false,
                }
            };
            let result = if in_call { Ok(()) } else { delivery::leave_message(&contact.identity_key, &text, contact.expire_after_secs) };
            match result {
                Ok(()) => {
                    app.set_leave_message_text("".into());
                    let how = if in_call {
                        "Sent.".to_string()
                    } else if contact.message_route == 1 || config.strict_mode {
                        format!("Waiting for {} to come online.", contact.name)
                    } else {
                        format!("Queued for {}; it will be left at their mailbox if they have one, or sent when you're both online.", contact.name)
                    };
                    app.set_contacts_message(how.into());
                }
                Err(e) => app.set_contacts_message(e.into()),
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_route_toggled(move |index, on| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut config = config::load();
            let Some(r) = usize::try_from(index).ok().and_then(|i| config.routes.get_mut(i)) else { return };
            r.enabled = on;
            save_routes(&app, config);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_route_move(move |index, by| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut config = config::load();
            let Ok(from) = usize::try_from(index) else { return };
            let Ok(to) = usize::try_from(index + by) else { return };
            if from >= config.routes.len() || to >= config.routes.len() {
                return;
            }
            config.routes.swap(from, to);
            save_routes(&app, config);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_route_nodes_edited(move |index, text| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut config = config::load();
            let Some(r) = usize::try_from(index).ok().and_then(|i| config.routes.get_mut(i)) else { return };
            r.nodes = text.split([',', ' ', '\n']).map(str::trim).filter(|n| !n.is_empty()).map(String::from).collect();
            save_routes(&app, config);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_route_code_edited(move |index, text| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut config = config::load();
            let Some(r) = usize::try_from(index).ok().and_then(|i| config.routes.get_mut(i)) else { return };
            r.code = text.trim().to_string();
            save_routes(&app, config);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_delivery_settings_changed(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let config = current_config(&app);
            delivery::set_hosting(&state.runtime, config.hold_mail_for_devices, config.hold_mail_port);
            save_routes(&app, config);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_sharing_settings_changed(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let config = current_config(&app);
            sharing::set_seeding(&state.runtime, sharing::seeds_from_here(&config), config.seed_port);
            config::save(&config);
        });
    }
    {
        let state = app_state.clone();
        main_window.on_check_shared_files(move || sharing::check_all(&state.runtime));
    }
    wire_communities(main_window, app_state);
    captions::wire(main_window, app_state);
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_vpn_settings_changed(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let config = current_config(&app);
            config::save(&config);
            apply_vpn(&state.runtime, &config);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_import_vpn_config(move || {
            let window_weak = window_weak.clone();
            let runtime = state.runtime.clone();
            // The dialog blocks, so not on the UI thread.
            std::thread::spawn(move || {
                let Some(path) = rfd::FileDialog::new()
                    .set_title("Import a WireGuard config")
                    .add_filter("WireGuard config", &["conf"])
                    .pick_file()
                else {
                    return;
                };
                let message = match vpn::import(&path) {
                    Ok(entry) => {
                        // The first one imported becomes the active one.
                        let mut config = config::load();
                        if config.vpn_active.is_empty() || !vpn::load().iter().any(|c| c.id == config.vpn_active) {
                            config.vpn_active = entry.id.clone();
                            config::save(&config);
                            apply_vpn(&runtime, &config);
                        }
                        format!("Imported {}.", entry.name)
                    }
                    Err(e) => format!("Couldn't import that file: {e}"),
                };
                let _ = window_weak.upgrade_in_event_loop(move |app| app.set_vpn_message(message.into()));
            });
        });
    }
    {
        let state = app_state.clone();
        main_window.on_select_vpn_config(move |id| {
            let mut config = config::load();
            config.vpn_active = id.to_string();
            config::save(&config);
            apply_vpn(&state.runtime, &config);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_delete_vpn_config(move |id| {
            let message = match vpn::delete(&id) {
                Ok(()) => String::new(),
                Err(e) => format!("Couldn't delete it: {e}"),
            };
            let config = config::load();
            apply_vpn(&state.runtime, &config);
            if let Some(app) = window_weak.upgrade() {
                app.set_vpn_message(message.into());
            }
        });
    }
    {
        let state = app_state.clone();
        main_window.on_download_shared_file(move |file_id| {
            let state = state.clone();
            let file_id = file_id.to_string();
            let name = sharing::load().into_iter().find(|f| f.file_id == file_id).map(|f| f.name).unwrap_or_default();
            std::thread::spawn(move || {
                let Some(dest) = rfd::FileDialog::new().set_title("Save the shared file").set_file_name(&name).save_file() else {
                    return;
                };
                sharing::download(&state.runtime, &file_id, dest);
            });
        });
    }
    main_window.on_forget_shared_file(move |file_id| sharing::forget(&file_id));
    {
        let window_weak = main_window.as_weak();
        main_window.on_dht_settings_changed(move || {
            let Some(app) = window_weak.upgrade() else { return };
            config::save(&current_config(&app));
            if !app.get_dht_enabled() {
                app.set_dht_status("".into());
            }
            discovery::kick();
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_browse_directories(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let config = current_config(&app);
            config::save(&config);
            if config.directories.is_empty() {
                app.set_directory_status("Add a directory's address first.".into());
                return;
            }
            app.set_directory_status("Asking...".into());
            let weak = window_weak.clone();
            state.runtime.spawn(async move {
                let mut pins = config.directory_pins.clone();
                let (listed, problems) = discovery::browse(&config.directories, &mut pins).await;
                let mut saved = config::load();
                saved.directory_pins = pins;
                config::save(&saved);
                slint::invoke_from_event_loop(move || {
                    let Some(app) = weak.upgrade() else { return };
                    let rows: Vec<DirectoryEntry> = listed
                        .into_iter()
                        .map(|l| DirectoryEntry {
                            name: l.entry.name.into(),
                            description: l.entry.description.into(),
                            invite: l.entry.invite.into(),
                            directory: l.directory.into(),
                        })
                        .collect();
                    let summary = if rows.is_empty() { "No communities found.".to_string() } else { format!("{} communities.", rows.len()) };
                    let mut lines = vec![summary];
                    lines.extend(problems);
                    app.set_directory_status(lines.join("\n").into());
                    app.set_directory_entries(ModelRc::new(VecModel::from(rows)));
                })
                .ok();
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_join_directory_entry(move |name, invite| {
            let Some(app) = window_weak.upgrade() else { return };
            // The same path as pasting the link by hand: it is saved
            // unverified, like any invite.
            app.set_new_contact_name(name);
            app.set_new_contact_invite(invite);
            app.invoke_add_from_invite();
            app.set_directory_status(app.get_contacts_message());
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_find_nodes(move || {
            let Some(app) = window_weak.upgrade() else { return };
            app.set_found_nodes("Looking on your network...".into());
            let window_weak = window_weak.clone();
            state.runtime.spawn(async move {
                let found = delivery::discover(Duration::from_secs(2)).await;
                let text = if found.is_empty() {
                    "No mailbox nodes answered on your network.".to_string()
                } else {
                    format!(
                        "Found: {}. Paste one into a route above.",
                        found.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")
                    )
                };
                let _ = window_weak.upgrade_in_event_loop(move |app| app.set_found_nodes(text.into()));
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_set_contact_connection_mode(move |index, mode| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut config = config::load();
            let Some(contact) = usize::try_from(index).ok().and_then(|i| config.contacts.get_mut(i)) else { return };
            contact.connection_mode = match connection_mode::Override::from_i32(mode) {
                connection_mode::Override::Auto => 0,
                connection_mode::Override::Direct => 1,
                connection_mode::Override::Relay => 2,
            };
            let message = match connection_mode::Override::from_i32(mode) {
                connection_mode::Override::Auto => format!(
                    "{} is called {} automatically.",
                    contact.name,
                    if connection_mode::is_known(contact) { "direct" } else { "through a relay" }
                ),
                connection_mode::Override::Direct => format!(
                    "{} is always called direct -- fastest, but you see each other's IP address.",
                    contact.name
                ),
                connection_mode::Override::Relay => format!(
                    "{} is always called through a relay -- neither of you sees the other's IP address.",
                    contact.name
                ),
            };
            config::save(&config);
            apply_contacts_to_ui(&app, &config.contacts);
            app.set_contacts_message(message.into());
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_new_invite_secret(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let mut config = config::load();
            config.relay_invite_secret.clear();
            config::save(&config);
            refresh_invite(&app, &state);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_send_friend_request(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                active.send_friend_request(&state.runtime, app.get_display_name().to_string());
                app.set_contacts_message("Request sent".into());
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_accept_friend_request(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let call = state.call.lock().unwrap();
            let Some(active) = call.as_ref() else { return };
            let Some((name, address)) =
                active.accept_friend_request(&state.runtime, app.get_display_name().to_string())
            else {
                return;
            };
            app.set_friend_request_from("".into());
            save_contact_if_new(&app, &state, active, &name, &address);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_reject_friend_request(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let call = state.call.lock().unwrap();
            if let Some(active) = call.as_ref() {
                active.reject_friend_request(&state.runtime);
            }
            app.set_friend_request_from("".into());
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // The console's command echo/output lines. Owned here (not a
        // MainWindow property alone) so appending is a plain Vec push; the
        // model is rebuilt per command, which is nothing at command rates.
        let term_lines: Rc<RefCell<Vec<SharedString>>> = Rc::new(RefCell::new(Vec::new()));
        main_window.on_terminal_command(move |cmd| {
            let Some(app) = window_weak.upgrade() else { return };
            let cmd = cmd.trim().to_string();
            if cmd.is_empty() {
                return;
            }
            app.set_terminal_input("".into());
            let mut lines = term_lines.borrow_mut();
            lines.push(format!("> {cmd}").into());
            let (verb, rest) = cmd.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((cmd.as_str(), ""));
            match verb.to_ascii_lowercase().as_str() {
                "help" => {
                    for l in [
                        "commands:",
                        "  status         connection + quality readout",
                        "  say <message>  send a chat line to the peer",
                        "  mute           toggle mic mute",
                        "  deafen         toggle deafen",
                        "  normal         leave server mode (relaunches the full UI)",
                        "  quit           exit the app",
                    ] {
                        lines.push(l.into());
                    }
                }
                "status" => {
                    lines.push(app.get_status_text());
                    let stats = app.get_call_stats();
                    if !stats.is_empty() {
                        lines.push(stats);
                    }
                }
                "say" => {
                    if rest.is_empty() {
                        lines.push("usage: say <message>".into());
                    } else {
                        let call = state.call.lock().unwrap();
                        match call.as_ref() {
                            Some(active) => active.send_chat(&state.runtime, rest.to_string()),
                            None => lines.push("not connected".into()),
                        }
                    }
                }
                "mute" => {
                    app.invoke_toggle_mute();
                    lines.push(if app.get_muted() { "muted" } else { "unmuted" }.into());
                }
                "deafen" => {
                    app.invoke_toggle_deafen();
                    lines.push(if app.get_deafened() { "deafened" } else { "undeafened" }.into());
                }
                "normal" | "gui" => {
                    drop(lines);
                    app.set_server_mode(false);
                    relaunch_app(&app);
                    return;
                }
                "quit" => {
                    drop(lines);
                    save_current_config(&app);
                    let _ = slint::quit_event_loop();
                    return;
                }
                _ => lines.push("unknown command -- try help".into()),
            }
            while lines.len() > 200 {
                lines.remove(0);
            }
            let snapshot = lines.clone();
            drop(lines);
            app.set_terminal_lines(ModelRc::new(VecModel::from(snapshot)));
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §2 replay history. Playback goes through its own output
        // stream rather than the call's, so listening back does not
        // interrupt or mix into the live audio still arriving.
        main_window.on_play_replay_clip(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            let clip = {
                let call = state.call.lock().unwrap();
                call.as_ref().and_then(|active| active.replay_clip(index.max(0) as usize))
            };
            let Some((sample_rate, samples)) = clip else { return };
            let device = config::load().output_device;
            match state::play_clip(device.as_deref(), sample_rate, &samples) {
                // Held until playback finishes; dropping it cuts the sound
                // off immediately.
                Ok(handle) => {
                    let seconds = samples.len() as f32 / sample_rate.max(1) as f32;
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs_f32(seconds + 0.3));
                        drop(handle);
                    });
                    app.set_replay_message("".into());
                }
                Err(e) => app.set_replay_message(format!("Couldn't play that: {e}").into()),
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_clear_replay_clips(move || {
            let Some(app) = window_weak.upgrade() else { return };
            if let Some(active) = state.call.lock().unwrap().as_ref() {
                active.clear_replay_clips();
            }
            app.set_replay_clips(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_save_replay(move || {
            let Some(app) = window_weak.upgrade() else { return };
            // Snapshot on the UI thread (a memcpy of at most ~30s of
            // samples), then the blocking save dialog + file write happen
            // off it -- same pattern as settings export.
            let snapshot = { state.call.lock().unwrap().as_ref().map(|active| active.take_replay_snapshot()) };
            let Some((sample_rate, samples)) = snapshot else { return };
            if samples.is_empty() {
                app.set_replay_message("Nothing received yet".into());
                return;
            }
            let window_weak2 = window_weak.clone();
            std::thread::spawn(move || {
                let Some(path) = rfd::FileDialog::new()
                    .set_title("Save replay")
                    .add_filter("WAV audio", &["wav"])
                    .set_file_name("replay.wav")
                    .save_file()
                else {
                    return;
                };
                let seconds = samples.len() as f32 / sample_rate as f32;
                let message = match wav::write_wav_16bit_mono(&path, sample_rate, &samples) {
                    Ok(()) => format!("Saved {seconds:.0}s to {}", path.display()),
                    Err(e) => format!("Couldn't save replay: {e}"),
                };
                slint::invoke_from_event_loop(move || {
                    if let Some(app) = window_weak2.upgrade() {
                        app.set_replay_message(message.into());
                    }
                })
                .ok();
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_clear_theme_image(move || {
            let Some(app) = window_weak.upgrade() else { return };
            app.set_theme_bg_image_set(false);
            app.set_theme_bg_image(slint::Image::default());
            animated_image::stop_animation(&app, animated_image::ImageSlot::ThemeBackground);
            let mut cfg = current_config(&app);
            cfg.theme_image_path = String::new();
            config::save(&cfg);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_import_profile_avatar(move || {
            if window_weak.upgrade().is_none() {
                return;
            }
            let window_weak2 = window_weak.clone();
            std::thread::spawn(move || {
                let Some(path) = rfd::FileDialog::new()
                    .set_title("Choose a profile picture")
                    .add_filter("Image files", &["png", "jpg", "jpeg", "bmp", "gif", "webp", "tiff"])
                    .pick_file()
                else {
                    return;
                };
                let raw = animated_image::load_raw(&path);
                slint::invoke_from_event_loop(move || {
                    let Some(app) = window_weak2.upgrade() else { return };
                    let result = raw.and_then(|raw| {
                        app.set_profile_avatar_image_set(true);
                        app.set_profile_avatar_message("".into());
                        animated_image::apply_raw_loaded(
                            &app,
                            window_weak2.clone(),
                            raw,
                            animated_image::ImageSlot::ProfileAvatar,
                            Rc::new(|app: &MainWindow, img| app.set_profile_avatar_image(img)),
                        )
                    });
                    match result {
                        Ok(()) => {
                            let mut cfg = current_config(&app);
                            cfg.profile_avatar_path = path.to_string_lossy().to_string();
                            config::save(&cfg);
                        }
                        Err(e) => {
                            app.set_profile_avatar_message(format!("Couldn't load that image: {e}").into());
                        }
                    }
                })
                .ok();
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_clear_profile_avatar(move || {
            let Some(app) = window_weak.upgrade() else { return };
            app.set_profile_avatar_image_set(false);
            app.set_profile_avatar_image(slint::Image::default());
            animated_image::stop_animation(&app, animated_image::ImageSlot::ProfileAvatar);
            app.set_profile_avatar_message("".into());
            let mut cfg = current_config(&app);
            cfg.profile_avatar_path = String::new();
            config::save(&cfg);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_move_audio_section(move |id, dir| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut order = model_to_string_vec(app.get_audio_order());
            move_order_item(&mut order, &id, dir);
            app.set_audio_order(string_vec_to_model(order));
            save_current_config(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_move_appearance_section(move |id, dir| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut order = model_to_string_vec(app.get_appearance_order());
            move_order_item(&mut order, &id, dir);
            app.set_appearance_order(string_vec_to_model(order));
            save_current_config(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_move_profile_section(move |id, dir| {
            let Some(app) = window_weak.upgrade() else { return };
            let mut order = model_to_string_vec(app.get_profile_order());
            move_order_item(&mut order, &id, dir);
            app.set_profile_order(string_vec_to_model(order));
            save_current_config(&app);
        });
    }
    // Call tab panel drag/resize. `*_started` snapshots the panel's geometry
    // at press (before any movement); every `*_moved` reports the TOTAL
    // delta since that press, not an incremental one, so the snapshot has
    // to survive across calls -- hence the shared `RefCell`, reset by the
    // matching `*_finished`.
    let call_drag_start: Rc<RefCell<Option<(String, PanelGeom)>>> = Rc::new(RefCell::new(None));
    let call_resize_start: Rc<RefCell<Option<(String, PanelGeom)>>> = Rc::new(RefCell::new(None));
    {
        let window_weak = main_window.as_weak();
        let call_drag_start = call_drag_start.clone();
        main_window.on_call_panel_drag_started(move |id| {
            let Some(app) = window_weak.upgrade() else { return };
            *call_drag_start.borrow_mut() = Some((id.to_string(), get_panel_geom(&app, &id)));
        });
    }
    {
        let window_weak = main_window.as_weak();
        let call_drag_start = call_drag_start.clone();
        main_window.on_call_panel_drag_moved(move |id, dx, dy| {
            let Some(app) = window_weak.upgrade() else { return };
            let start = { call_drag_start.borrow().as_ref().filter(|(sid, _)| sid == id.as_str()).map(|(_, g)| *g) };
            let Some(start) = start else { return };
            let (x, y) = clamp_panel_move(start, dx, dy, app.get_call_canvas_width(), app.get_call_canvas_height());
            set_panel_pos(&app, &id, x, y);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let call_drag_start = call_drag_start.clone();
        main_window.on_call_panel_drag_finished(move |id| {
            let Some(app) = window_weak.upgrade() else { return };
            *call_drag_start.borrow_mut() = None;

            // The drag itself was free (no collision-avoidance -- see
            // `clamp_panel_move`'s doc comment), so the drop point can be
            // sitting on top of another panel. Resolve that now, animating
            // the correction rather than snapping instantly, so "settles
            // into place" actually reads as settling.
            let dropped = get_panel_geom(&app, &id);
            let others = other_visible_panel_geoms(&app, &id);
            let (x, y) = resolve_drop_position(dropped, &others, app.get_call_canvas_width(), app.get_call_canvas_height());
            if (x, y) != (dropped.x, dropped.y) {
                app.set_call_panel_snap_animating(true);
                set_panel_pos(&app, &id, x, y);
                app.set_call_panel_snap_animating(false);
            }

            save_current_config(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let call_resize_start = call_resize_start.clone();
        main_window.on_call_panel_resize_started(move |id| {
            let Some(app) = window_weak.upgrade() else { return };
            *call_resize_start.borrow_mut() = Some((id.to_string(), get_panel_geom(&app, &id)));
        });
    }
    {
        let window_weak = main_window.as_weak();
        let call_resize_start = call_resize_start.clone();
        main_window.on_call_panel_resize_moved(move |id, dw, dh| {
            let Some(app) = window_weak.upgrade() else { return };
            let start = { call_resize_start.borrow().as_ref().filter(|(sid, _)| sid == id.as_str()).map(|(_, g)| *g) };
            let Some(start) = start else { return };
            let others = other_visible_panel_geoms(&app, &id);
            let (w, h) = clamp_panel_resize(start, dw, dh, &others, app.get_call_canvas_width(), app.get_call_canvas_height());
            set_panel_size(&app, &id, w, h);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_call_panel_resize_finished(move |_id| {
            let Some(app) = window_weak.upgrade() else { return };
            *call_resize_start.borrow_mut() = None;
            save_current_config(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_export_settings(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let cfg = current_config(&app);
            let window_weak2 = window_weak.clone();
            std::thread::spawn(move || {
                let Some(path) = rfd::FileDialog::new()
                    .set_title("Export Stoop settings")
                    .add_filter("Stoop settings", &["json"])
                    .set_file_name("antiphon-settings.json")
                    .save_file()
                else {
                    return;
                };
                let message = match config::save_to(&path, &cfg) {
                    Ok(()) => format!("Exported to {}", path.display()),
                    Err(e) => format!("Export failed: {e}"),
                };
                slint::invoke_from_event_loop(move || {
                    if let Some(app) = window_weak2.upgrade() {
                        app.set_import_export_message(message.into());
                    }
                })
                .ok();
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_import_settings(move || {
            let window_weak2 = window_weak.clone();
            let state2 = state.clone();
            std::thread::spawn(move || {
                let Some(path) =
                    rfd::FileDialog::new().set_title("Import Stoop settings").add_filter("Stoop settings", &["json"]).pick_file()
                else {
                    return;
                };
                let result = config::load_from(&path);
                slint::invoke_from_event_loop(move || {
                    let Some(app) = window_weak2.upgrade() else { return };
                    match result {
                        Ok(cfg) => {
                            apply_config_to_ui(&app, &cfg);
                            sync_transmit_from_ui(&app, &state2);
                            save_current_config(&app);
                            app.set_import_export_message(format!("Imported {}", path.display()).into());
                        }
                        Err(e) => app.set_import_export_message(format!("Import failed: {e}").into()),
                    }
                })
                .ok();
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_import_font(move || {
            if window_weak.upgrade().is_none() {
                return;
            }
            let window_weak2 = window_weak.clone();
            std::thread::spawn(move || {
                // File pick + disk read happen off the UI thread (both can
                // block); the actual font registration below has to happen
                // back on it, since `shared_collection()` isn't thread-safe.
                let Some(path) =
                    rfd::FileDialog::new().set_title("Import font").add_filter("Font files", &["ttf", "otf", "ttc"]).pick_file()
                else {
                    return;
                };
                let data = std::fs::read(&path);
                slint::invoke_from_event_loop(move || {
                    let Some(app) = window_weak2.upgrade() else { return };
                    let data = match data {
                        Ok(d) => d,
                        Err(e) => {
                            app.set_font_import_message(format!("Import failed: {e}").into());
                            return;
                        }
                    };
                    match register_font_bytes(data) {
                        Ok(name) => {
                            app.set_font_family_choice(name.clone().into());
                            app.set_font_import_message(format!("Using imported font: {name}").into());
                            let mut cfg = current_config(&app);
                            cfg.custom_font_path = path.to_string_lossy().to_string();
                            config::save(&cfg);
                        }
                        Err(e) => app.set_font_import_message(format!("Import failed: {e}").into()),
                    }
                })
                .ok();
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_capture_binding(move |id| {
            let Some(app) = window_weak.upgrade() else { return };
            if !app.get_capturing_binding().is_empty() {
                return; // one capture at a time
            }
            app.set_capturing_binding(id.clone());

            let window_weak2 = window_weak.clone();
            let state2 = state.clone();
            state.runtime.spawn_blocking(move || {
                let key = state::capture_next_key(Duration::from_secs(6));
                slint::invoke_from_event_loop(move || {
                    let Some(app) = window_weak2.upgrade() else { return };
                    app.set_capturing_binding("".into());
                    let Some(key) = key else {
                        // Nothing arrived in six seconds. Saying so beats
                        // leaving the binding silently unchanged.
                        app.set_binding_warning(
                            "No key was detected. Another program or the system may have it.".into(),
                        );
                        return;
                    };
                    let captured = key;
                    let name = key.to_string();

                    // Spec §1: refuse keys that cannot work, with the
                    // reason. The failure this prevents is a binding that
                    // saves fine and then never transmits, which nobody
                    // blames on the key.
                    if id == "ptt" {
                        if let Err(reason) = key_capture::check(&name) {
                            app.set_binding_warning(reason.explain(&name).into());
                            return;
                        }
                    }

                    app.set_binding_warning("".into());
                    set_binding_name(&app, &id, &name);
                    sync_transmit_from_ui(&app, &state2);
                    save_current_config(&app);

                    // Spec §1: confirm the app can actually see the key it
                    // just bound. Some keys bind fine and are then
                    // swallowed by another program, and the resulting
                    // silence gets blamed on the microphone.
                    if id == "ptt" {
                        let window_weak3 = window_weak2.clone();
                        let name2 = name.clone();
                        state2.runtime.spawn_blocking(move || {
                            let seen = state::watch_for_key(captured, Duration::from_secs(3));
                            slint::invoke_from_event_loop(move || {
                                let Some(app) = window_weak3.upgrade() else { return };
                                if let Some(warning) = key_capture::warn_if_never_seen(&name2, seen)
                                {
                                    app.set_binding_warning(warning.into());
                                }
                            })
                            .ok();
                        });
                    }
                })
                .ok();
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        main_window.on_clear_binding(move |id| {
            let Some(app) = window_weak.upgrade() else { return };
            set_binding_name(&app, &id, "");
            sync_transmit_from_ui(&app, &state);
            save_current_config(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        let state = app_state.clone();
        // Spec §1 "Live Monitoring". Standalone rather than the call
        // pipeline's monitor tap, so it works with no call running --
        // which is the whole point of checking your mic before you dial.
        main_window.on_toggle_loopback(move || {
            let Some(app) = window_weak.upgrade() else { return };

            if !app.get_loopback_enabled() {
                *state.loopback.lock().unwrap() = None;
                app.set_loopback_message("".into());
                return;
            }

            if app.get_connected() {
                // One capture stream per device: the call already owns it.
                app.set_loopback_enabled(false);
                app.set_loopback_message(
                    "Not while you're on a call -- the call is already using your microphone.".into(),
                );
                return;
            }

            let (input, output) = selected_devices(&app);
            let config = read_chain_config(&app);
            match state::start_loopback(input.as_deref(), output.as_deref(), &config) {
                Ok(handle) => {
                    *state.loopback.lock().unwrap() = Some(handle);
                    app.set_loopback_message("Loopback running -- use headphones to avoid echo.".into());
                }
                Err(e) => {
                    app.set_loopback_enabled(false);
                    app.set_loopback_message(format!("Couldn't start loopback: {e}").into());
                }
            }
        });
    }
}

/// Spec §1 mic test: record five seconds, then play it back processed.
fn wire_mic_test(main_window: &MainWindow, app_state: &Arc<AppState>) {
    let window_weak = main_window.as_weak();
    let state = app_state.clone();
    main_window.on_mic_test(move || {
        let Some(app) = window_weak.upgrade() else { return };
        if app.get_connected() || app.get_loopback_enabled() {
            // One capture stream per device, and something already has it.
            app.set_mic_test_message("Not while a call or loopback is using your microphone.".into());
            return;
        }
        let (input, output) = selected_devices(&app);
        let config = read_chain_config(&app);
        app.set_mic_test_running(true);
        app.set_mic_test_message(
            format!("Recording for {} seconds -- say something.", state::MIC_TEST_SECONDS).into(),
        );
        let weak = window_weak.clone();
        state.runtime.spawn_blocking(move || {
            let result = state::record_mic_test(input.as_deref(), &config, state::MIC_TEST_SECONDS)
                .and_then(|(rate, samples)| {
                    let handle = state::play_clip(output.as_deref(), rate, &samples)?;
                    let weak = weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(app) = weak.upgrade() {
                            app.set_mic_test_message("Playing it back -- this is what others hear.".into());
                        }
                    })
                    .ok();
                    // Held until playback finishes; dropping it cuts the
                    // sound off.
                    std::thread::sleep(Duration::from_secs_f32(samples.len() as f32 / rate.max(1) as f32 + 0.3));
                    drop(handle);
                    Ok(())
                });
            slint::invoke_from_event_loop(move || {
                let Some(app) = weak.upgrade() else { return };
                app.set_mic_test_running(false);
                app.set_mic_test_message(match result {
                    Ok(()) => "".into(),
                    Err(e) => format!("Mic test failed: {e}").into(),
                });
            })
            .ok();
        });
    });
}

/// Sets the status line without touching the tray tooltip -- for transient,
/// local-only notices (a mic test failing) that don't reflect call state.
fn set_status_only(app: &MainWindow, text: &str) {
    app.set_status_text(text.into());
}

fn toggle_mute(app: &MainWindow, state: &Arc<AppState>) {
    let call = state.call.lock().unwrap();
    if let Some(active) = call.as_ref() {
        let now_muted = !active.muted.load(Ordering::Relaxed);
        active.muted.store(now_muted, Ordering::Relaxed);
        drop(call);
        app.set_muted(now_muted);
    }
}

/// Applies the whole current UI state to a running call (if any) and
/// persists it -- the common tail of every settings change, EQ edits
/// included, so a live call always reflects what's on screen.
fn apply_settings_change(app: &MainWindow, state: &Arc<AppState>) {
    let config = read_chain_config(app);
    let call = state.call.lock().unwrap();
    if let Some(active) = call.as_ref() {
        active.apply_chain_config(config);
    }
    drop(call);

    sync_transmit_from_ui(app, state);
    save_current_config(app);
}

/// Edits one band by rebuilding the whole `eq-bands` list from a plain
/// `Vec` and pushing it back via [`apply_eq_bands_to_ui`], rather than
/// mutating the live model in place. `ModelRc::as_any().downcast_ref` was
/// tried first and turned out fragile: if it ever guessed the concrete
/// model type wrong, `.expect()` would panic inside a Slint callback --
/// invisible in a windowed app with no attached console, so every EQ edit
/// would have silently no-op'd instead of erroring loudly. Rebuilding from
/// a `Vec` can't have that failure mode.
fn edit_eq_band(app: &MainWindow, index: i32, edit: impl FnOnce(&mut EqBandConfig)) {
    let mut bands = eq_bands_from_ui(app);
    let Some(band) = usize::try_from(index).ok().and_then(|i| bands.get_mut(i)) else { return };
    edit(band);
    apply_eq_bands_to_ui(app, &bands);
}

/// Sets the status line in the window and mirrors it into the tray tooltip,
/// so the current call state is visible even while the window is hidden.
fn set_status(app: &MainWindow, tray_weak: &slint::Weak<AppTrayIcon>, text: &str) {
    app.set_status_text(text.into());
    if let Some(tray) = tray_weak.upgrade() {
        tray.set_status_line(text.into());
    }
}

// Every handle the connect path produces, passed once; bundling them into a
// struct for a single call site would be shape without substance.
#[allow(clippy::too_many_arguments)]
fn finish_connect(
    window_weak: slint::Weak<MainWindow>,
    tray_weak: slint::Weak<AppTrayIcon>,
    state: Arc<AppState>,
    result: anyhow::Result<antiphon_net::PeerConnection>,
    config: ChainConfig,
    input_device: Option<String>,
    output_device: Option<String>,
    hello: Hello,
) {
    match result {
        Ok(peer) => {
            let transmit = state.transmit.clone();
            let output_gain_db = state.output_gain_db.clone();
            // Computed before the pipeline consumes `peer`, from the
            // identity the handshake actually verified rather than
            // anything the peer merely claimed about themselves.
            let code = antiphon_crypto::verification_code(
                &state.identity.public().identity_key,
                &peer.peer_identity().identity_key,
            );
            // Spec §4 trust-on-first-use. The handshake already proved the
            // peer holds this key; the question here is whether it is the
            // key we saw last time at this address.
            let peer_identity = peer.peer_identity();
            let remote_addr = peer.remote_addr().to_string();
            // Spec §3: a connection that came from our relay's address is
            // a relayed one, and the address is the relay's, not theirs.
            let anonymous = anonymity::is_circuit(peer.remote_addr());
            let relayed = anonymous || *state.relay_addr.lock().unwrap() == Some(peer.remote_addr());
            let mode = match (anonymous, relayed) {
                (true, _) => reachability::Mode::Anonymous,
                (false, true) => reachability::Mode::Relayed,
                (false, false) => reachability::Mode::Direct,
            };
            let trust = evaluate_peer_trust(&remote_addr, &peer_identity, relayed);
            // Spec §1: this person's remembered volume trim and mute, so a
            // quiet microphone stays fixed across calls rather than being
            // re-adjusted every time.
            let peer_audio = saved_peer_audio(&peer_identity.identity_key);
            // Spec §1: this contact's EQ, built once at connect. 48 kHz is
            // the preview rate used elsewhere -- the received stream's own
            // rate is whatever the far end captured at, which we do not
            // learn, and at audible frequencies the curve barely moves
            // between common rates.
            let peer_bands = saved_peer_eq_bands(&peer_identity.identity_key);
            *state.peer_eq.lock().unwrap() = peer_eq_chain(&peer_bands);
            let (bass, voice, treble) = peer_tone_positions(&peer_bands);
            let expire_secs = saved_expire_secs(&peer_identity.identity_key);
            *state.peer_volume_db.lock().unwrap() = peer_audio.0;
            state.peer_muted.store(peer_audio.1, Ordering::Relaxed);
            // History is only kept for saved contacts: nothing about a
            // stranger's conversation is written to disk unless the user
            // chose to keep them.
            // Spec §9: a block is enforced here, on the receiving machine,
            // before the call is shown or any state is set up. There is no
            // server to do it, so if this path does not hang up, nothing
            // does.
            if is_peer_blocked(&peer_identity.identity_key) {
                slint::invoke_from_event_loop(move || {
                    if let Some(app) = window_weak.upgrade() {
                        app.set_connected(false);
                        set_status(&app, &tray_weak, "Not connected");
                    }
                })
                .ok();
                return;
            }

            let history = history_sink_for(&peer_identity.identity_key);
            match state::start_pipeline(
                peer,
                config,
                input_device,
                output_device,
                transmit,
                output_gain_db,
                state.replay_seconds.clone(),
                state.duck_db.clone(),
                state.peer_eq.clone(),
                state.echo_cancel.clone(),
                state.echo_canceller.clone(),
                state.peer_volume_db.clone(),
                state.peer_muted.clone(),
                hello,
                history,
            ) {
                Ok(active) => {
                    // Spec §8: anything queued for this contact -- in this
                    // session or a previous one -- goes now that there is
                    // somewhere to send it.
                    active.flush_outbox(&state.runtime);
                    *state.call.lock().unwrap() = Some(active);
                    slint::invoke_from_event_loop(move || {
                        if let Some(app) = window_weak.upgrade() {
                            app.set_connected(true);
                            app.set_call_mode(mode.label().into());
                            app.set_call_mode_explanation(mode.explain().into());
                            // A new call has not knocked yet, whatever the
                            // last one did.
                            app.set_knocked(false);
                            app.set_verification_code(code.into());
                            apply_peer_trust(&app, &trust);
                            // Spec §5: transfers slow down while a call is
                            // up, so the file does not eat the audio.
                            file_transfer::set_call_active(true);
                            // Spec §9: the receiver's rules decide, here,
                            // on this machine. Nothing in the incoming
                            // call can raise the outcome.
                            match ring_outcome(&peer_identity.identity_key, false) {
                                notify::Outcome::Ring => {
                                    set_status(&app, &tray_weak, "Connected");
                                }
                                notify::Outcome::Notify => {
                                    set_status(&app, &tray_weak, "Connected (silenced)");
                                }
                                notify::Outcome::Ignore => {
                                    set_status(&app, &tray_weak, "Connected (notifications off)");
                                }
                            }
                            app.set_peer_volume_db(peer_audio.0);
                            app.set_peer_muted(peer_audio.1);
                            app.set_peer_bass_db(bass);
                            app.set_peer_voice_db(voice);
                            app.set_peer_treble_db(treble);
                            app.set_chat_expire_secs(expire_secs);
                            set_status(&app, &tray_weak, "Connected");
                        }
                    })
                    .ok();
                }
                Err(e) => {
                    slint::invoke_from_event_loop(move || {
                        if let Some(app) = window_weak.upgrade() {
                            set_status(&app, &tray_weak, &format!("Audio error: {e}"));
                        }
                    })
                    .ok();
                }
            }
        }
        Err(e) => {
            slint::invoke_from_event_loop(move || {
                if let Some(app) = window_weak.upgrade() {
                    set_status(&app, &tray_weak, &format!("Connection error: {e}"));
                }
            })
            .ok();
        }
    }
}

fn device_model(names: Vec<String>) -> ModelRc<SharedString> {
    let mut items = vec![SharedString::from(DEFAULT_DEVICE_LABEL)];
    items.extend(names.into_iter().map(SharedString::from));
    ModelRc::new(VecModel::from(items))
}

/// Maps the UI's device dropdown selection back to what `antiphon-audio` expects:
/// `None` picks the system default, matching the "System default" sentinel entry.
fn selected_devices(app: &MainWindow) -> (Option<String>, Option<String>) {
    let to_opt = |s: String| if s == DEFAULT_DEVICE_LABEL || s.is_empty() { None } else { Some(s) };
    (to_opt(app.get_selected_input_device().to_string()), to_opt(app.get_selected_output_device().to_string()))
}

/// Parses "#rrggbb" (`#` optional) into a `slint::Color`. `None` on
/// anything malformed -- callers decide what that should mean (fall back
/// to a default at startup; leave the current color alone if the user
/// just typed something bad into the hex field).
fn color_from_hex(hex: &str) -> Option<slint::Color> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let byte = |range: std::ops::Range<usize>| u8::from_str_radix(hex.get(range)?, 16).ok();
    Some(slint::Color::from_rgb_u8(byte(0..2)?, byte(2..4)?, byte(4..6)?))
}

fn color_to_hex(c: slint::Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.red(), c.green(), c.blue())
}

/// Standard HSV->RGB (h in 0..360, s/v in 0..1). Used both for the final
/// color the `ColorPicker` produces and for its SV square's pure-hue base
/// layer (`hue_preview`, called with s=1, v=1).
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let h = h.rem_euclid(360.0);
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r1, g1, b1) = match h as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (((r1 + m) * 255.0).round() as u8, ((g1 + m) * 255.0).round() as u8, ((b1 + m) * 255.0).round() as u8)
}

/// Inverse of `hsv_to_rgb`, used to seed the `ColorPicker`'s hue/sat/val
/// whenever the color changes from outside it -- hex field, a loaded
/// config, an imported settings file.
fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let h = if delta == 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let s = if max == 0.0 { 0.0 } else { delta / max };
    (h, s, max)
}

/// Pushes `color` into every UI element that displays the accent color --
/// the swatch (`accent-choice`, which the `.slint` side mirrors into
/// `Theme.accent`), the `ColorPicker`'s hue/sat/val (and its hue-only
/// preview swatch), and the hex text field -- so all of them stay in sync
/// regardless of which one triggered the change.
fn set_accent_color(app: &MainWindow, color: slint::Color) {
    app.set_accent_choice(color);
    app.set_accent_hex_display(color_to_hex(color).into());
    let (h, s, v) = rgb_to_hsv(color.red(), color.green(), color.blue());
    app.set_accent_hue(h);
    app.set_accent_sat(s);
    app.set_accent_val(v);
    let (pr, pg, pb) = hsv_to_rgb(h, 1.0, 1.0);
    app.set_accent_hue_preview(slint::Color::from_rgb_u8(pr, pg, pb));
}

/// Same swatch/picker/hex sync as `set_accent_color`, for Appearance >
/// Theme's "Custom" background color picker.
fn set_theme_bg_color(app: &MainWindow, color: slint::Color) {
    app.set_theme_bg_color(color);
    app.set_theme_bg_hex_display(color_to_hex(color).into());
    let (h, s, v) = rgb_to_hsv(color.red(), color.green(), color.blue());
    app.set_theme_bg_hue(h);
    app.set_theme_bg_sat(s);
    app.set_theme_bg_val(v);
    let (pr, pg, pb) = hsv_to_rgb(h, 1.0, 1.0);
    app.set_theme_bg_hue_preview(slint::Color::from_rgb_u8(pr, pg, pb));
}

/// Formats a byte count for the file transfer panel, e.g. "2.3 MB".
fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["KB", "MB", "GB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// Reads `active`'s file-transfer state into the UI's `ft-*` properties,
/// and hands a just-completed incoming transfer off to a save-file dialog
/// (off the UI thread, same pattern as every other native file dialog in
/// this app) -- called from the same 50ms timer that already polls
/// mic-level/PTT/peer-profile.
fn update_file_transfer_ui(app: &MainWindow, active: &state::ActiveCall, state: &Arc<AppState>, window_weak: &slint::Weak<MainWindow>) {
    let ready = {
        let mut ft = active.file_transfer.lock().unwrap();

        app.set_ft_offer_pending(ft.pending_offer.is_some());
        if let Some(offer) = ft.pending_offer.as_ref() {
            // A voice message is announced as one rather than as a WAV
            // with a timestamp in its name -- the receiver should know
            // they are being offered a clip to listen to, not a file.
            let label = if voice_message::is_voice_message(&offer.file_name) {
                "Voice message".to_string()
            } else {
                offer.file_name.clone()
            };
            app.set_ft_offer_name(label.into());
            app.set_ft_offer_size(human_size(offer.file_size).into());

            // Already bounded and PNG-checked on arrival, so decoding here
            // is over a known shape rather than whatever a peer sent.
            match decode_thumbnail(&offer.thumbnail_png) {
                Some(image) => app.set_ft_offer_thumbnail(image),
                None => app.set_ft_offer_thumbnail(slint::Image::default()),
            }

            // Spec §5: the offer arrived, so whoever sent it is here and
            // holding the file. The other states need the delivery routes
            // of §8 to be observable at all.
            app.set_ft_availability(file_preview::Availability::Available.describe().into());
        }

        app.set_ft_outgoing_active(ft.outgoing.is_some());
        if let Some(outgoing) = ft.outgoing.as_ref() {
            app.set_ft_outgoing_name(format!("{} ({})", outgoing.file_name, human_size(outgoing.file_size)).into());
            let progress = if outgoing.total_chunks == 0 { 0.0 } else { outgoing.chunks_sent as f32 / outgoing.total_chunks as f32 };
            app.set_ft_outgoing_progress(progress);
            app.set_ft_outgoing_error(outgoing.error.clone().unwrap_or_default().into());
            app.set_ft_outgoing_paused(outgoing.paused);
            if outgoing.done {
                app.set_ft_status_message("File sent and confirmed received.".into());
            }
        }

        app.set_ft_incoming_active(ft.incoming.is_some());
        if let Some(incoming) = ft.incoming.as_ref() {
            let note = if incoming.verifying {
                " -- checking it arrived intact".to_string()
            } else if incoming.resumed_chunks > 0 && !incoming.done {
                let pct = incoming.resumed_chunks as u64 * 100 / incoming.total_chunks.max(1) as u64;
                format!(" -- resumed, {pct}% was already here")
            } else {
                String::new()
            };
            app.set_ft_incoming_name(
                format!("{} ({}){note}", incoming.file_name, human_size(incoming.file_size)).into(),
            );
            let progress = if incoming.total_chunks == 0 { 0.0 } else { incoming.chunks_received as f32 / incoming.total_chunks as f32 };
            app.set_ft_incoming_progress(progress);
            app.set_ft_incoming_error(incoming.error.clone().unwrap_or_default().into());
        }

        ft.ready_to_save.take()
    };

    if let Some((file_name, stored)) = ready {
        let state2 = state.clone();
        let window_weak2 = window_weak.clone();
        std::thread::spawn(move || {
            // Whatever the user picks, the transfer itself is finished: the
            // file is verified and in the store, and the sender has already
            // been told. Clearing `incoming` here returns the panel to its
            // idle view instead of a 100% bar that never goes away.
            let clear = || {
                let call = state2.call.lock().unwrap();
                if let Some(active) = call.as_ref() {
                    active.file_transfer.lock().unwrap().incoming = None;
                }
            };
            let Some(path) = rfd::FileDialog::new().set_title("Save received file").set_file_name(&file_name).save_file() else {
                clear();
                return;
            };
            // A copy, not a move: the store keeps its own so the same file
            // offered again later needs no transfer at all (spec §5).
            let message = match std::fs::copy(&stored, &path) {
                Ok(_) => {
                    clear();
                    format!("Saved to {}", path.display())
                }
                Err(e) => format!("Received the file but couldn't save it: {e}"),
            };
            slint::invoke_from_event_loop(move || {
                if let Some(app) = window_weak2.upgrade() {
                    app.set_ft_status_message(message.into());
                }
            })
            .ok();
        });
    }
}

/// Pushes a saved `PanelRect` into a Call-tab panel's four length
/// properties. Takes the setters as closures rather than the panel's name
/// as a string, since each panel's `set_call_*_x/y/width/height` are
/// distinct generated methods with no common trait to call through.
fn apply_panel_rect(
    rect: config::PanelRect,
    set_x: impl FnOnce(f32),
    set_y: impl FnOnce(f32),
    set_width: impl FnOnce(f32),
    set_height: impl FnOnce(f32),
) {
    set_x(rect.x);
    set_y(rect.y);
    set_width(rect.width);
    set_height(rect.height);
}

/// Adds `(name, address)` to the saved contacts unless that address is
/// already there -- shared by both sides of the friend-request exchange,
/// which can otherwise both try to save the same peer.
/// Saves the person on the current call as a contact, after a friend
/// request. Keyed by the identity the handshake proved; over a relay the
/// address is the relay's, so none is stored and the relay is kept instead
/// (spec §3).
fn save_contact_if_new(app: &MainWindow, state: &AppState, active: &state::ActiveCall, name: &str, address: &str) {
    let peer = active.peer_identity();
    let key = encode_identity_key(&peer.identity_key);
    let relayed = state.relay_addr.lock().unwrap().is_some_and(|r| r.to_string() == address);
    let mut contacts = contacts_from_ui(app);
    if contacts.iter().any(|c| c.identity_key == key || (!relayed && c.address == address)) {
        app.set_contacts_message(format!("{name} is already saved").into());
        return;
    }
    let their_relay = active.peer_profile.lock().unwrap().as_ref().map(|h| h.relay.clone()).unwrap_or_default();
    contacts.push(config::Contact {
        name: name.to_string(),
        address: if relayed { String::new() } else { address.to_string() },
        is_server: false,
        identity_key: key,
        first_seen_secs: now_secs(),
        prekey: connection_mode::encode_hex32(&peer.prekey),
        relay: their_relay,
        ..Default::default()
    });
    apply_contacts_to_ui(app, &contacts);
    app.set_contacts_message(format!("Added {name}").into());
    save_current_config(app);
}

fn apply_presets_to_ui(app: &MainWindow, presets: &[config::VoicePreset]) {
    let entries: Vec<PresetEntry> = presets.iter().map(|p| PresetEntry { name: p.name.clone().into() }).collect();
    app.set_voice_presets(ModelRc::new(VecModel::from(entries)));
}

/// What the peer's key means for the contact saved at this address, and
/// the pinning side effect that follows from it.
struct PeerTrust {
    fingerprint: String,
    verified: bool,
    key_changed: bool,
    message: String,
}

/// Evaluates the peer's identity against whatever is pinned for their
/// address, pinning on first contact.
///
/// Runs off the UI thread, so it reads and writes the saved config
/// directly rather than the UI model. A contact that isn't saved at all is
/// not pinned -- there is nothing to warn about and nothing to remember
/// until the user decides to keep them.
fn evaluate_peer_trust(remote_addr: &str, peer: &antiphon_crypto::PublicIdentity, relayed: bool) -> PeerTrust {
    let fingerprint = antiphon_crypto::fingerprint(&peer.identity_key);
    let mut config = config::load();

    // By key first: a relayed call arrives from the relay's address, and
    // a contact who moved arrives from a new one. The address is only a
    // fallback for a contact saved by address and never called before --
    // and never for a relayed call, where it is the relay's.
    let key_hex = encode_identity_key(&peer.identity_key);
    let index = config.contacts.iter().position(|c| c.identity_key == key_hex).or_else(|| {
        (!relayed)
            .then(|| config.contacts.iter().position(|c| c.address == remote_addr))
            .flatten()
    });
    let Some(index) = index else {
        return PeerTrust {
            fingerprint,
            verified: false,
            key_changed: false,
            message: "Not a saved contact. Add them to remember this key.".to_string(),
        };
    };

    // The same key as pinned: keep its prekey on file (spec §3 -- it is
    // what makes them a known contact). Contacts pinned from an invite
    // link arrive here on their first call, never through FirstContact.
    let prekey_hex = connection_mode::encode_hex32(&peer.prekey);
    if config.contacts[index].identity_key == key_hex && config.contacts[index].prekey != prekey_hex {
        config.contacts[index].prekey = prekey_hex;
        config::save(&config);
    }

    let contact = &config.contacts[index];
    let pinned = decode_identity_key(&contact.identity_key).map(|identity_key| {
        antiphon_crypto::PinnedContact {
            identity_key,
            petname: contact.name.clone(),
            trust: if contact.verified {
                antiphon_crypto::TrustLevel::Verified
            } else {
                antiphon_crypto::TrustLevel::Unverified
            },
            first_seen_secs: contact.first_seen_secs,
        }
    });

    match antiphon_crypto::evaluate(pinned.as_ref(), peer) {
        antiphon_crypto::TrustDecision::FirstContact => {
            // Trust on first use: remember the key so a later change is
            // detectable. Nobody has checked it yet, and the message says so.
            let name = contact.name.clone();
            config.contacts[index].identity_key = encode_identity_key(&peer.identity_key);
            config.contacts[index].first_seen_secs = now_secs();
            // Spec §3: a completed call makes them a known contact, and
            // their prekey is what a pairwise relay token is derived from.
            config.contacts[index].prekey = connection_mode::encode_hex32(&peer.prekey);
            config::save(&config);
            PeerTrust {
                fingerprint,
                verified: false,
                key_changed: false,
                message: format!(
                    "Pinned {name}'s key. Compare the code with them to verify it."
                ),
            }
        }
        antiphon_crypto::TrustDecision::Unchanged { trust } => {
            let verified = trust == antiphon_crypto::TrustLevel::Verified;
            PeerTrust {
                fingerprint,
                verified,
                key_changed: false,
                message: if verified {
                    "Verified contact -- same key you checked before.".to_string()
                } else {
                    "Same key as last time, but never verified out of band.".to_string()
                },
            }
        }
        antiphon_crypto::TrustDecision::KeyChanged { was_verified, .. } => {
            // Deliberately not re-pinned. Silently accepting the new key
            // would erase the only evidence that anything happened.
            let name = contact.name.clone();
            PeerTrust {
                fingerprint,
                verified: false,
                key_changed: true,
                message: if was_verified {
                    format!(
                        "WARNING: {name}'s key changed, and you had verified the old one.                          Either they reinstalled without a recovery phrase, or someone is                          intercepting this call. Compare the code with them before trusting it."
                    )
                } else {
                    format!(
                        "{name}'s key changed since last time. Usually a reinstall without a                          recovery phrase -- compare the code with them to be sure."
                    )
                },
            }
        }
    }
}

fn apply_peer_trust(app: &MainWindow, trust: &PeerTrust) {
    app.set_peer_fingerprint(trust.fingerprint.clone().into());
    app.set_peer_verified(trust.verified);
    app.set_peer_key_changed(trust.key_changed);
    app.set_peer_trust_message(trust.message.clone().into());
}

/// Strips a clip name down to something safe as a filename.
fn sanitize_file_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == ' ')
        .take(48)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() { "clip".to_string() } else { trimmed.to_string() }
}

fn apply_sound_clips_to_ui(app: &MainWindow, clips: &[soundboard::SoundClip]) {
    app.set_sound_clip_names(string_vec_to_model(
        clips.iter().map(|c| c.name.clone()).collect(),
    ));
}

/// Snapshots the call screen's current arrangement (spec §7).
fn current_layout(app: &MainWindow, name: &str) -> config::NamedLayout {
    let rect = |x: f32, y: f32, w: f32, h: f32| config::PanelRect { x, y, width: w, height: h };
    config::NamedLayout {
        name: name.to_string(),
        connect_rect: rect(
            app.get_call_connect_x(),
            app.get_call_connect_y(),
            app.get_call_connect_width(),
            app.get_call_connect_height(),
        ),
        filetransfer_rect: rect(
            app.get_call_filetransfer_x(),
            app.get_call_filetransfer_y(),
            app.get_call_filetransfer_width(),
            app.get_call_filetransfer_height(),
        ),
        profile_rect: rect(
            app.get_call_profile_x(),
            app.get_call_profile_y(),
            app.get_call_profile_width(),
            app.get_call_profile_height(),
        ),
        chat_rect: rect(
            app.get_call_chat_x(),
            app.get_call_chat_y(),
            app.get_call_chat_width(),
            app.get_call_chat_height(),
        ),
        contacts_rect: rect(
            app.get_call_contacts_x(),
            app.get_call_contacts_y(),
            app.get_call_contacts_width(),
            app.get_call_contacts_height(),
        ),
        invite_rect: rect(
            app.get_call_invite_x(),
            app.get_call_invite_y(),
            app.get_call_invite_width(),
            app.get_call_invite_height(),
        ),
        connect_visible: app.get_call_panel_visible_connect(),
        filetransfer_visible: app.get_call_panel_visible_filetransfer(),
        chat_visible: app.get_call_panel_visible_chat(),
        contacts_visible: app.get_call_panel_visible_contacts(),
        invite_visible: app.get_call_panel_visible_invite(),
    }
}

/// Puts a saved arrangement back on screen.
fn apply_layout(app: &MainWindow, layout: &config::NamedLayout) {
    let put = |r: config::PanelRect,
               sx: &dyn Fn(f32),
               sy: &dyn Fn(f32),
               sw: &dyn Fn(f32),
               sh: &dyn Fn(f32)| {
        sx(r.x);
        sy(r.y);
        sw(r.width);
        sh(r.height);
    };
    put(
        layout.connect_rect,
        &|v| app.set_call_connect_x(v),
        &|v| app.set_call_connect_y(v),
        &|v| app.set_call_connect_width(v),
        &|v| app.set_call_connect_height(v),
    );
    put(
        layout.filetransfer_rect,
        &|v| app.set_call_filetransfer_x(v),
        &|v| app.set_call_filetransfer_y(v),
        &|v| app.set_call_filetransfer_width(v),
        &|v| app.set_call_filetransfer_height(v),
    );
    put(
        layout.invite_rect,
        &|v| app.set_call_invite_x(v),
        &|v| app.set_call_invite_y(v),
        &|v| app.set_call_invite_width(v),
        &|v| app.set_call_invite_height(v),
    );
    put(
        layout.profile_rect,
        &|v| app.set_call_profile_x(v),
        &|v| app.set_call_profile_y(v),
        &|v| app.set_call_profile_width(v),
        &|v| app.set_call_profile_height(v),
    );
    put(
        layout.chat_rect,
        &|v| app.set_call_chat_x(v),
        &|v| app.set_call_chat_y(v),
        &|v| app.set_call_chat_width(v),
        &|v| app.set_call_chat_height(v),
    );
    put(
        layout.contacts_rect,
        &|v| app.set_call_contacts_x(v),
        &|v| app.set_call_contacts_y(v),
        &|v| app.set_call_contacts_width(v),
        &|v| app.set_call_contacts_height(v),
    );
    app.set_call_panel_visible_connect(layout.connect_visible);
    app.set_call_panel_visible_filetransfer(layout.filetransfer_visible);
    app.set_call_panel_visible_invite(layout.invite_visible);
    app.set_call_panel_visible_chat(layout.chat_visible);
    app.set_call_panel_visible_contacts(layout.contacts_visible);
}

fn apply_layout_names(app: &MainWindow, layouts: &[config::NamedLayout]) {
    app.set_layout_names(string_vec_to_model(
        layouts.iter().map(|l| l.name.clone()).collect(),
    ));
}

/// Picks the most legible font actually present on this machine.
///
/// No font is bundled. Shipping one means licensing it for redistribution,
/// and the typefaces that help most -- OpenDyslexic in particular -- are
/// ones people install deliberately. The fallbacks are wide, open-countered
/// faces that ship with the major platforms, which is a real improvement
/// over a default like Segoe UI even though they are not dyslexia-specific.
fn pick_readable_font() -> Option<(String, String)> {
    // In preference order: purpose-built first, then generally legible.
    const CANDIDATES: &[(&str, &str)] = &[
        ("OpenDyslexic", "Using OpenDyslexic."),
        ("Comic Sans MS", "Using Comic Sans MS -- widely reported as easier to read."),
        ("Verdana", "Using Verdana -- wide letterforms, easier to tell apart."),
        ("Tahoma", "Using Tahoma -- wide letterforms, easier to tell apart."),
        ("DejaVu Sans", "Using DejaVu Sans -- wide letterforms, easier to tell apart."),
    ];

    // Asks the same font collection the renderer uses, so a name that
    // resolves here is one that will actually render.
    let mut collection = slint::fontique_010::shared_collection();
    for (family, note) in CANDIDATES {
        if collection.family_by_name(family).is_some() {
            return Some((family.to_string(), note.to_string()));
        }
    }
    None
}

/// Turns a received thumbnail into an image.
///
/// Returns `None` for anything that fails, which is an ordinary outcome:
/// a transfer without a preview works exactly as one with it.
fn decode_thumbnail(png: &[u8]) -> Option<slint::Image> {
    if !file_preview::accept_thumbnail(png) {
        return None;
    }
    let decoded = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?;
    let rgba = decoded.to_rgba8();
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(rgba.width(), rgba.height());
    buffer.make_mut_bytes().copy_from_slice(rgba.as_raw());
    Some(slint::Image::from_rgba8(buffer))
}

/// Records an introduced key as a contact -- unverified, with provenance.
///
/// Never marks it verified. Alice can be wrong, lying, or fooled herself,
/// and treating her word as proof is exactly what a verification code
/// exists to catch.
fn apply_introduction(
    intro: &introduction::Introduction,
    active: &state::ActiveCall,
    introducer: &str,
) -> String {
    let mut config = config::load();
    let known: Vec<String> = config.contacts.iter().map(|c| c.identity_key.clone()).collect();
    let blocked: Vec<String> = config.blocked.iter().map(|b| b.identity_key.clone()).collect();
    let introducer_hex = encode_identity_key(&active.peer_identity().identity_key);

    let outcome = introduction::evaluate(intro, &introducer_hex, &known, &blocked);
    if outcome == introduction::Outcome::Added {
        config.contacts.push(config::Contact {
            name: intro.petname.clone(),
            // No address: an introduction carries a key, not a location.
            // They become callable once they send an invite or call in.
            address: String::new(),
            is_server: false,
            identity_key: intro.identity_key.to_lowercase(),
            verified: false,
            first_seen_secs: now_secs(),
            volume_db: 0.0,
            muted: false,
            expire_after_secs: None,
            ring_rule: 0,
            eq_bands: Vec::new(),
            note: introduction::provenance_note(introducer),
            ..Default::default()
        });
        config::save(&config);
    }
    outcome.explain(introducer, &intro.petname)
}

/// Whether calls from this identity should be refused (spec §9).
fn is_peer_blocked(peer_identity_key: &[u8; 32]) -> bool {
    let hex = encode_identity_key(peer_identity_key);
    moderation::is_blocked(&config::load().blocked, &hex)
}

fn apply_blocked_to_ui(app: &MainWindow, blocked: &[moderation::BlockedIdentity]) {
    app.set_blocked_names(string_vec_to_model(
        blocked
            .iter()
            .map(|b| {
                if b.petname.is_empty() {
                    format!("({}...)", &b.identity_key[..8.min(b.identity_key.len())])
                } else {
                    b.petname.clone()
                }
            })
            .collect(),
    ));
}

/// Which getting-started prompt applies right now (spec §13).
fn current_prompt(config: &config::AppConfig) -> first_run::Prompt {
    first_run::next_prompt(
        config.first_run,
        config.contacts.len(),
        config.contacts.iter().filter(|c| c.verified).count(),
        config.calls_completed,
    )
}

fn apply_onboarding(app: &MainWindow, config: &config::AppConfig) {
    let prompt = current_prompt(config);
    app.set_onboard_title(first_run::title(prompt).into());
    app.set_onboard_body(first_run::body(prompt).into());
}

/// What an incoming call from this identity is allowed to do (spec §9).
///
/// Reads the receiver's own settings only. A contact's rule overrides the
/// per-community rules, which override the default; quiet hours can lower the result but never raise it.
fn ring_outcome(peer_identity_key: &[u8; 32], caller_wants_silent: bool) -> notify::Outcome {
    let hex = encode_identity_key(peer_identity_key);
    let config = config::load();
    let rule = config
        .contacts
        .iter()
        .find(|c| c.identity_key == hex)
        .map(|c| notify::RingRule::from_i32(c.ring_rule))
        // Not a contact: the rules you set on group chats and communities
        // you share with them, then your default.
        .or_else(|| community::Store::app().and_then(|s| notify::shared_rule(s.rules_for(peer_identity_key))))
        .unwrap_or(notify::RingRule::from_i32(config.default_ring_rule));

    notify::decide(
        rule,
        config.quiet_hours,
        notify::current_minute_of_day(),
        caller_wants_silent,
    )
}

/// Centre frequencies for the three per-contact tone controls (spec §1).
///
/// Chosen around the voice band rather than a hi-fi bass/treble split:
/// 200 Hz is where a close microphone gets boomy, 5 kHz is where a cheap
/// one gets harsh, and 1.5 kHz is where intelligibility lives.
const PEER_BASS_HZ: f32 = 200.0;
const PEER_VOICE_HZ: f32 = 1500.0;
const PEER_TREBLE_HZ: f32 = 5000.0;

/// The three tone controls as EQ bands.
///
/// A band at 0 dB is stored disabled rather than dropped, so the round
/// trip back into the sliders does not depend on band order.
fn peer_tone_bands(bass_db: f32, voice_db: f32, treble_db: f32) -> Vec<EqBandConfig> {
    [
        (BandType::LowShelf, PEER_BASS_HZ, 0.7, bass_db),
        (BandType::Bell, PEER_VOICE_HZ, 1.0, voice_db),
        (BandType::HighShelf, PEER_TREBLE_HZ, 0.7, treble_db),
    ]
    .into_iter()
    .map(|(band_type, center_hz, q, gain_db)| EqBandConfig {
        enabled: gain_db.abs() >= 0.5,
        center_hz,
        q,
        gain_db,
        band_type,
    })
    .collect()
}

/// The three tone-control positions recovered from stored bands.
///
/// Matched by centre frequency so a config written by an older build --
/// or hand-edited -- degrades to "no tone shaping" instead of loading
/// somebody else's curve into three sliders that cannot express it.
fn peer_tone_positions(bands: &[EqBandConfig]) -> (f32, f32, f32) {
    let at = |hz: f32| {
        bands
            .iter()
            .find(|b| (b.center_hz - hz).abs() < 1.0)
            .map(|b| b.gain_db)
            .unwrap_or(0.0)
    };
    (at(PEER_BASS_HZ), at(PEER_VOICE_HZ), at(PEER_TREBLE_HZ))
}

/// Those bands as a ready-to-run chain, or `None` if they do nothing.
///
/// Only the EQ bands: this is somebody else's voice, and running it
/// through a gate or a compressor tuned for *your* microphone would be
/// processing their audio with settings chosen for different input.
fn peer_eq_chain(bands: &[EqBandConfig]) -> Option<antiphon_audio::AudioChain> {
    if bands.iter().all(|b| !b.enabled) {
        return None;
    }
    let mut chain = ChainConfig::default();
    chain.noise_gate.enabled = false;
    chain.high_pass.enabled = false;
    chain.low_pass.enabled = false;
    chain.de_esser.enabled = false;
    chain.compressor.enabled = false;
    chain.limiter.enabled = false;
    chain.pitch_shift.enabled = false;
    chain.reverb.enabled = false;
    chain.gain = GainConfig::Fixed { enabled: false, gain_db: 0.0 };
    chain.eq_bands = bands.to_vec();
    Some(antiphon_audio::AudioChain::from_config(EQ_CURVE_PREVIEW_SAMPLE_RATE, &chain))
}

/// Pushes the network capability table into the UI (spec §10).
///
/// Rebuilt from one value rather than each row being set independently,
/// so the three answers can never disagree about the same facts.
fn apply_reachability(app: &MainWindow, state: &reachability::Reachability) {
    let rows: Vec<ReachabilityRow> = state
        .rows()
        .into_iter()
        .map(|(question, answer, meaning)| ReachabilityRow {
            question: question.into(),
            answer: answer.into(),
            meaning: meaning.into(),
        })
        .collect();
    app.set_reachability_rows(ModelRc::new(VecModel::from(rows)));
}

/// What the UI currently believes about reachability, read back out.
///
/// Kept in the UI properties rather than in `AppState` because every one
/// of these facts is already displayed there; a second copy is a second
/// thing to keep in step.
pub(crate) fn current_reachability(app: &MainWindow) -> reachability::Reachability {
    reachability::Reachability {
        public_address_known: !app.get_public_address().is_empty(),
        port_forwarded: app.get_port_mapped(),
        port_forward_refused: app.get_port_forward_refused(),
    }
}

/// This contact's stored tone bands.
fn saved_peer_eq_bands(peer_identity_key: &[u8; 32]) -> Vec<EqBandConfig> {
    let hex = encode_identity_key(peer_identity_key);
    config::load()
        .contacts
        .iter()
        .find(|c| c.identity_key == hex)
        .map(|c| c.eq_bands.clone())
        .unwrap_or_default()
}

/// This contact's remembered volume trim and mute, or neutral defaults
/// for someone not saved.
fn saved_peer_audio(peer_identity_key: &[u8; 32]) -> (f32, bool) {
    let hex = encode_identity_key(peer_identity_key);
    config::load()
        .contacts
        .iter()
        .find(|c| c.identity_key == hex)
        .map(|c| (c.volume_db, c.muted))
        .unwrap_or((0.0, false))
}

/// This contact's disappearing-message window in seconds, 0 for off.
fn saved_expire_secs(peer_identity_key: &[u8; 32]) -> i32 {
    let hex = encode_identity_key(peer_identity_key);
    config::load()
        .contacts
        .iter()
        .find(|c| c.identity_key == hex)
        .and_then(|c| c.expire_after_secs)
        .unwrap_or(0) as i32
}

/// A history sink for this peer, if they are a saved contact.
///
/// Returns `None` for anyone not in the contact list, which is what keeps
/// "nothing is stored about people you haven't chosen to keep" true rather
/// than a claim.
fn history_sink_for(peer_identity_key: &[u8; 32]) -> Option<Arc<history::HistorySink>> {
    let hex = encode_identity_key(peer_identity_key);
    let saved = config::load();
    let contact = saved.contacts.iter().find(|c| c.identity_key == hex)?;
    let expire_after_secs = contact.expire_after_secs;
    let dir = config::config_dir()?;
    match history::load_or_create_backup_key(&dir) {
        Ok(key) => {
            Some(Arc::new(history::HistorySink { dir, peer_hex: hex, key, expire_after_secs }))
        }
        Err(e) => {
            eprintln!("[history] disabled for this call: {e}");
            None
        }
    }
}

/// Rebuilds the invite link and its QR from the identity plus whatever
/// address is currently known.
///
/// The address is a bootstrap hint, so an invite without one is still
/// valid -- it names the right person and simply can't be dialled yet.
fn refresh_invite(app: &MainWindow, state: &std::sync::Arc<state::AppState>) {
    let address = {
        let public = app.get_public_address().to_string();
        if public.trim().is_empty() {
            String::new()
        } else {
            public.trim().to_string()
        }
    };

    let mut invite = antiphon_crypto::Invite::new(state.identity.public().identity_key, address.clone());
    // Spec §3: with a relay, the link also lets its holder ring you
    // through it without either of you seeing the other's address.
    let relay = network_ui::effective_relay(app);
    if !relay.is_empty() {
        invite.relay = relay;
        invite.relay_secret = Some(relay_invite_secret());
    }
    let link = invite.to_link();

    app.set_my_invite_has_address(!address.is_empty());
    app.set_my_invite_link(link.clone().into());
    match qr::render(&link, 400) {
        Some(image) => app.set_my_invite_qr(image),
        // A link too long to encode shouldn't blank the text form.
        None => app.set_my_invite_qr(slint::Image::default()),
    }
}

/// A shareable ID for an identity key (spec §4).
///
/// The key itself, rendered in groups so it can be read aloud or typed
/// without losing your place. Derived from the key rather than assigned,
/// so there is no registry to squat, nothing to run out of, and no server
/// that can take it away -- and no need for the unique-ID allocation a
/// username system would require.
fn share_id(identity_key: &[u8; 32]) -> String {
    let hex: String = identity_key.iter().map(|b| format!("{b:02x}")).collect();
    hex.as_bytes()
        .chunks(8)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join("-")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn apply_contacts_to_ui(app: &MainWindow, contacts: &[config::Contact]) {
    let entries: Vec<ContactEntry> = contacts
        .iter()
        .map(|c| ContactEntry {
            name: c.name.clone().into(),
            address: c.address.clone().into(),
            is_server: c.is_server,
            fingerprint: pinned_fingerprint(c).into(),
            verified: c.verified,
            // Only ever set transiently by a live key-change detection;
            // never persisted, so a warning can't outlive the situation
            // that produced it.
            key_changed: false,
            ring_rule: c.ring_rule,
            key: c.identity_key.clone().into(),
            connection_mode: c.connection_mode,
            via_relay: !c.relay.is_empty(),
            message_route: c.message_route,
            has_mailbox: delivery::decode_identity(&c.identity_key)
                .and_then(|k| delivery::accept_record(&c.route_record, &k, None))
                .is_some_and(|r| !delivery::mailboxes_in(&r).is_empty()),
        })
        .collect();
    app.set_contacts(ModelRc::new(VecModel::from(entries)));
}

/// Pushes the route list (spec §8) into the UI, with each node's last
/// known status.
fn apply_routes_to_ui(app: &MainWindow, config: &config::AppConfig) {
    let report = delivery::node_report();
    let entries: Vec<RouteEntry> = config
        .routes
        .iter()
        .map(|r| RouteEntry {
            title: r.kind.title().into(),
            description: r.kind.description().into(),
            enabled: r.enabled,
            is_direct: r.kind == delivery::RouteKind::Direct,
            nodes: r.nodes.join(", ").into(),
            code: r.code.clone().into(),
            status: r
                .nodes
                .iter()
                .filter_map(|n| report.iter().find(|(node, _)| node == n.trim()).map(|(node, s)| format!("{node}: {s}")))
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
        })
        .collect();
    app.set_routes(ModelRc::new(VecModel::from(entries)));
    app.set_strict_mode(config.strict_mode);
    app.set_route_fan_out(config.route_fan_out);
    app.set_hold_mail(config.hold_mail_for_devices);
    app.set_hold_mail_port(config.hold_mail_port as i32);
    app.set_captions_enabled(config.captions_enabled);
    app.set_captions_own_voice(config.captions_own_voice);
    app.set_seeding(config.seeding);
    app.set_anonymous_seeding(config.anonymous_seeding);
    app.set_seed_scope(config.seed_scope);
    app.set_seed_port(config.seed_port as i32);
    app.set_pin_nodes(config.pin_nodes.join(", ").into());
    app.set_pin_code(config.pin_code.clone().into());
    app.set_dht_enabled(config.dht_enabled);
    app.set_dht_bootstrap(config.dht_bootstrap.join(", ").into());
    app.set_directories(config.directories.join(", ").into());
}

/// Spec §6: hands the VPN settings to the router, starting or stopping
/// the tunnel as needed.
fn apply_vpn(runtime: &tokio::runtime::Handle, config: &config::AppConfig) {
    let routes = antiphon_net::vpn::Routes {
        voice: config.vpn_route_voice,
        files: config.vpn_route_files,
        messages: config.vpn_route_messages,
    };
    vpn::apply(runtime, config.vpn_enabled, &config.vpn_active, routes, config.vpn_kill_switch);
}

/// Spec §6: the VPN section -- stored configs (never their keys), which
/// is active, routing, and the status line.
fn apply_vpn_to_ui(app: &MainWindow, config: &config::AppConfig) {
    let entries: Vec<VpnConfigEntry> = vpn::load()
        .iter()
        .map(|c| VpnConfigEntry {
            id: c.id.clone().into(),
            name: c.name.clone().into(),
            detail: vpn::describe(c).into(),
            active: c.id == config.vpn_active,
        })
        .collect();
    app.set_vpn_configs(ModelRc::new(VecModel::from(entries)));
    app.set_vpn_enabled(config.vpn_enabled);
    app.set_vpn_route_voice(config.vpn_route_voice);
    app.set_vpn_route_files(config.vpn_route_files);
    app.set_vpn_route_messages(config.vpn_route_messages);
    app.set_vpn_kill_switch(config.vpn_kill_switch);
    app.set_vpn_status(vpn::status_line().into());
}

/// Spec §5: the shared-files list, with what's known about each.
fn apply_shared_files_to_ui(app: &MainWindow, config: &config::AppConfig) {
    let entries: Vec<SharedFileEntry> = sharing::load()
        .into_iter()
        .map(|f| {
            let community = f
                .with
                .strip_prefix("community:")
                .and_then(connection_mode::decode_hex32)
                .and_then(|id| community::Store::app()?.load(&antiphon_crypto::community::Key(id)))
                .map(|s| s.manifest().name.clone());
            let who = config
                .contacts
                .iter()
                .find(|c| c.identity_key.eq_ignore_ascii_case(&f.with))
                .map(|c| c.name.clone())
                .or(community)
                .unwrap_or_else(|| "someone not in your contacts".to_string());
            let holders = match f.holders.len() {
                0 => "no one else holds it".to_string(),
                1 => "1 holder".to_string(),
                n => format!("{n} holders"),
            };
            SharedFileEntry {
                name: f.name.clone().into(),
                detail: format!(
                    "{} · {} {} · {}",
                    human_size(f.size),
                    if f.mine { "shared with" } else { "from" },
                    who,
                    holders
                )
                .into(),
                status: sharing::status(&f.file_id).into(),
                file_id: f.file_id.into(),
                mine: f.mine,
            }
        })
        .collect();
    app.set_shared_files(ModelRc::new(VecModel::from(entries)));
    app.set_shared_files_notice(sharing::notice().into());
}

/// Saves a changed route list: bumps its sequence so contacts take the
/// new one over the old, and tells the courier.
fn save_routes(app: &MainWindow, mut config: config::AppConfig) {
    delivery::normalize(&mut config.routes);
    config.route_sequence = (config.route_sequence + 1).max(now_secs());
    config::save(&config);
    apply_routes_to_ui(app, &config);
    // Members of your groups who aren't your contacts learn the new list
    // from your card (spec §9), as contacts do from your next Hello.
    std::thread::spawn(community::refresh_cards);
    delivery::kick();
}

/// A contact's pinned key as a short fingerprint, or "" if nothing is
/// pinned yet.
fn pinned_fingerprint(contact: &config::Contact) -> String {
    match decode_identity_key(&contact.identity_key) {
        Some(key) => antiphon_crypto::fingerprint(&key),
        None => String::new(),
    }
}

/// Parses a hex identity key back into bytes. Returns `None` for the empty
/// string (nothing pinned) and for anything malformed -- a corrupt pin is
/// treated as "not pinned" rather than as a mismatch, so a hand-edited
/// config produces a fresh pin instead of a permanent scary warning.
fn decode_identity_key(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn encode_identity_key(key: &[u8; 32]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

fn contacts_from_ui(app: &MainWindow) -> Vec<config::Contact> {
    // The UI model doesn't carry the full key or the pin timestamp (it
    // only shows a fingerprint), so those are read back from the saved
    // config by address rather than reconstructed from the UI. Losing
    // them on a round-trip would silently unpin every contact.
    let saved = config::load();
    app.get_contacts()
        .iter()
        .map(|c| {
            let address = c.address.to_string();
            let key = c.key.to_string();
            // By key where there is one: two contacts reached only through
            // a relay both have an empty address.
            let previous = if key.is_empty() {
                saved.contacts.iter().find(|s| s.identity_key.is_empty() && s.address == address)
            } else {
                saved.contacts.iter().find(|s| s.identity_key == key)
            };
            config::Contact {
                name: c.name.to_string(),
                address,
                is_server: c.is_server,
                identity_key: previous.map(|p| p.identity_key.clone()).unwrap_or_default(),
                verified: c.verified,
                first_seen_secs: previous.map(|p| p.first_seen_secs).unwrap_or(0),
                // Per-person audio settings aren't shown in the contact
                // row, so they're carried from the saved config rather
                // than reconstructed from a UI that never had them.
                volume_db: previous.map(|p| p.volume_db).unwrap_or(0.0),
                muted: previous.is_some_and(|p| p.muted),
                expire_after_secs: previous.and_then(|p| p.expire_after_secs),
                ring_rule: c.ring_rule,
                // Not shown in the contact row, so carried from the saved
                // config rather than rebuilt from a UI that never had it.
                eq_bands: previous.map(|p| p.eq_bands.clone()).unwrap_or_default(),
                note: previous.map(|p| p.note.clone()).unwrap_or_default(),
                prekey: previous.map(|p| p.prekey.clone()).unwrap_or_default(),
                relay: previous.map(|p| p.relay.clone()).unwrap_or_default(),
                relay_secret: previous.map(|p| p.relay_secret.clone()).unwrap_or_default(),
                connection_mode: c.connection_mode,
                route_record: previous.map(|p| p.route_record.clone()).unwrap_or_default(),
                route_sequence: previous.and_then(|p| p.route_sequence),
                message_route: c.message_route,
                dht_sequence: previous.and_then(|p| p.dht_sequence),
            }
        })
        .collect()
}

/// A comma- or whitespace-separated list the user typed, as entries.
fn split_list(text: &str) -> Vec<String> {
    text.split(|c: char| c == ',' || c.is_whitespace()).map(str::trim).filter(|n| !n.is_empty()).map(String::from).collect()
}

fn string_vec_to_model(items: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(items.into_iter().map(SharedString::from).collect::<Vec<_>>()))
}

fn model_to_string_vec(model: ModelRc<SharedString>) -> Vec<String> {
    model.iter().map(|s| s.to_string()).collect()
}

/// Swaps `id` with its neighbor in `dir` (-1 up, 1 down) if that neighbor
/// exists; a no-op at either end rather than wrapping, matching the
/// disabled-at-the-boundary up/down buttons in the UI.
fn move_order_item(order: &mut [String], id: &str, dir: i32) {
    let Some(pos) = order.iter().position(|x| x == id) else { return };
    let new_pos = pos as i32 + dir;
    if new_pos < 0 || new_pos as usize >= order.len() {
        return;
    }
    order.swap(pos, new_pos as usize);
}

/// The seven Call-tab `FreeformPanel` ids -- "profile" is always visible
/// (see `is_panel_visible`), the rest can be hidden.
const CALL_PANEL_IDS: &[&str] =
    &["connect", "filetransfer", "profile", "chat", "contacts", "invite"];
// Wide enough that every panel's own content -- a label, a device picker,
// and a button side by side is the widest row any panel has -- stays
// visible left-to-right without needing horizontal scrolling, which is
// deliberately disabled (see `FreeformPanel`'s `ScrollView`; vertical
// scrolling for content that's too *tall* is fine, side-scrolling isn't).
const MIN_PANEL_WIDTH: f32 = 300.0;
const MIN_PANEL_HEIGHT: f32 = 100.0;
// Panels snap to this spacing while being dragged/resized (spec §7 "grid
// system") -- deliberately fine enough not to feel restrictive, coarse
// enough that alignment between panels is easy to hit on purpose.
const GRID_SIZE: f32 = 20.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct PanelGeom {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl PanelGeom {
    fn right(&self) -> f32 {
        self.x + self.w
    }
    fn bottom(&self) -> f32 {
        self.y + self.h
    }
    fn overlaps(&self, other: &PanelGeom) -> bool {
        self.x < other.right() && self.right() > other.x && self.y < other.bottom() && self.bottom() > other.y
    }
}

/// Rounds to the nearest multiple of `GRID_SIZE`.
fn snap_to_grid(value: f32) -> f32 {
    (value / GRID_SIZE).round() * GRID_SIZE
}

fn get_panel_geom(app: &MainWindow, id: &str) -> PanelGeom {
    match id {
        "connect" => {
            PanelGeom { x: app.get_call_connect_x(), y: app.get_call_connect_y(), w: app.get_call_connect_width(), h: app.get_call_connect_height() }
        }
        "invite" => PanelGeom {
            x: app.get_call_invite_x(),
            y: app.get_call_invite_y(),
            w: app.get_call_invite_width(),
            h: app.get_call_invite_height(),
        },
        "filetransfer" => PanelGeom {
            x: app.get_call_filetransfer_x(),
            y: app.get_call_filetransfer_y(),
            w: app.get_call_filetransfer_width(),
            h: app.get_call_filetransfer_height(),
        },
        "profile" => {
            PanelGeom { x: app.get_call_profile_x(), y: app.get_call_profile_y(), w: app.get_call_profile_width(), h: app.get_call_profile_height() }
        }
        "chat" => PanelGeom { x: app.get_call_chat_x(), y: app.get_call_chat_y(), w: app.get_call_chat_width(), h: app.get_call_chat_height() },
        "contacts" => {
            PanelGeom { x: app.get_call_contacts_x(), y: app.get_call_contacts_y(), w: app.get_call_contacts_width(), h: app.get_call_contacts_height() }
        }
        _ => PanelGeom { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
    }
}

fn set_panel_pos(app: &MainWindow, id: &str, x: f32, y: f32) {
    match id {
        "connect" => {
            app.set_call_connect_x(x);
            app.set_call_connect_y(y);
        }
        "invite" => {
            app.set_call_invite_x(x);
            app.set_call_invite_y(y);
        }
        "filetransfer" => {
            app.set_call_filetransfer_x(x);
            app.set_call_filetransfer_y(y);
        }
        "profile" => {
            app.set_call_profile_x(x);
            app.set_call_profile_y(y);
        }
        "chat" => {
            app.set_call_chat_x(x);
            app.set_call_chat_y(y);
        }
        "contacts" => {
            app.set_call_contacts_x(x);
            app.set_call_contacts_y(y);
        }
        _ => {}
    }
}

fn set_panel_size(app: &MainWindow, id: &str, w: f32, h: f32) {
    match id {
        "connect" => {
            app.set_call_connect_width(w);
            app.set_call_connect_height(h);
        }
        "invite" => {
            app.set_call_invite_width(w);
            app.set_call_invite_height(h);
        }
        "filetransfer" => {
            app.set_call_filetransfer_width(w);
            app.set_call_filetransfer_height(h);
        }
        "profile" => {
            app.set_call_profile_width(w);
            app.set_call_profile_height(h);
        }
        "chat" => {
            app.set_call_chat_width(w);
            app.set_call_chat_height(h);
        }
        "contacts" => {
            app.set_call_contacts_width(w);
            app.set_call_contacts_height(h);
        }
        _ => {}
    }
}

/// "profile" has no `call-panel-visible-profile` counterpart -- it's
/// always visible (see `FreeformPanel.removable` on the `.slint` side).
fn is_panel_visible(app: &MainWindow, id: &str) -> bool {
    match id {
        "connect" => app.get_call_panel_visible_connect(),
        "invite" => app.get_call_panel_visible_invite(),
        "filetransfer" => app.get_call_panel_visible_filetransfer(),
        "profile" => true,
        "chat" => app.get_call_panel_visible_chat(),
        "contacts" => app.get_call_panel_visible_contacts(),
        _ => false,
    }
}

/// Every *visible* panel's geometry except `exclude` -- a hidden panel
/// isn't there to collide with or make room around.
fn other_visible_panel_geoms(app: &MainWindow, exclude: &str) -> Vec<PanelGeom> {
    CALL_PANEL_IDS.iter().filter(|&&id| id != exclude && is_panel_visible(app, id)).map(|&id| get_panel_geom(app, id)).collect()
}

/// Slides `start` by `(dx, dy)`, snapped to `GRID_SIZE` and clamped to the
/// canvas -- deliberately *not* collision-checked against other panels
/// while the drag is live, so dragging tracks the pointer 1:1 the whole
/// gesture (see `resolve_drop_position` for what happens on release,
/// where overlap actually gets resolved).
// Snapping has to happen *before* the final clamp, not after: `0.0` and
// `MIN_PANEL_WIDTH` (a clean multiple of `GRID_SIZE`) are always
// grid-aligned, but the canvas-edge bound on the other side generally
// isn't -- snapping a value that's already been clamped to a
// non-grid-aligned bound can round it back out past that bound (e.g.
// snapping 90 with a 20px grid rounds up to 100, which would silently
// violate a 90px minimum). Clamping last guarantees the true bound always
// wins, even though that means the one value actually at the edge may not
// itself land on a grid line.
fn clamp_panel_move(start: PanelGeom, dx: f32, dy: f32, canvas_w: f32, canvas_h: f32) -> (f32, f32) {
    let x = snap_to_grid(start.x + dx).clamp(0.0, (canvas_w - start.w).max(0.0));
    let y = snap_to_grid(start.y + dy).clamp(0.0, (canvas_h - start.h).max(0.0));
    (x, y)
}

/// Same idea as `clamp_panel_move`, but growing/shrinking width/height from
/// the fixed top-left corner instead of moving -- and, unlike a live drag,
/// resize *is* continuously collision-clamped against `others`: growing a
/// panel is what would create a new overlap out of nothing (a plain move
/// only relocates the overlap, which `resolve_drop_position` cleans up
/// once, on release), so stopping it right at a neighbor's edge as it
/// happens is both correct and simpler than resolving it after the fact.
fn clamp_panel_resize(start: PanelGeom, dw: f32, dh: f32, others: &[PanelGeom], canvas_w: f32, canvas_h: f32) -> (f32, f32) {
    let y_overlaps_start = |o: &PanelGeom| start.y < o.bottom() && start.bottom() > o.y;
    let x_overlaps_start = |o: &PanelGeom| start.x < o.right() && start.right() > o.x;

    // The furthest right/down edge reachable before crossing into an
    // obstacle that overlaps in the other axis.
    let max_w = {
        let mut limit = canvas_w;
        for o in others.iter().filter(|o| y_overlaps_start(o)) {
            if o.x >= start.x {
                limit = limit.min(o.x);
            }
        }
        (limit - start.x).max(1.0)
    };
    let max_h = {
        let mut limit = canvas_h;
        for o in others.iter().filter(|o| x_overlaps_start(o)) {
            if o.y >= start.y {
                limit = limit.min(o.y);
            }
        }
        (limit - start.y).max(1.0)
    };

    // The lower bound is normally `MIN_PANEL_WIDTH`/`_HEIGHT`, but if an
    // obstacle sits closer than that, it wins: better to end up slightly
    // under the preferred minimum than to overlap. Snapped before
    // clamping, same reasoning as `clamp_panel_move`.
    let reach_w = snap_to_grid(start.w + dw).clamp(MIN_PANEL_WIDTH.min(max_w), max_w);
    let reach_h = snap_to_grid(start.h + dh).clamp(MIN_PANEL_HEIGHT.min(max_h), max_h);

    let collides = |w: f32, h: f32| {
        let candidate = PanelGeom { x: start.x, y: start.y, w, h };
        others.iter().any(|o| candidate.overlaps(o))
    };
    if !collides(reach_w, reach_h) {
        return (reach_w, reach_h);
    }
    if !collides(reach_w, start.h) {
        return (reach_w, start.h);
    }
    if !collides(start.w, reach_h) {
        return (start.w, reach_h);
    }
    (start.w, start.h)
}

/// Called once, on drop: if `dropped` overlaps any `others`, searches
/// outward on the grid from the drop point for the nearest free slot,
/// ring by ring (Chebyshev distance in grid cells) so the first ring that
/// contains any valid spot is searched fully before accepting one -- close
/// enough to "nearest by straight-line distance" without needing to search
/// the entire canvas up front. Falls back to the (clamped, still
/// overlapping) drop point itself if nothing opens up within the search
/// radius, rather than searching forever on a canvas that's simply full.
fn resolve_drop_position(dropped: PanelGeom, others: &[PanelGeom], canvas_w: f32, canvas_h: f32) -> (f32, f32) {
    let max_x = (canvas_w - dropped.w).max(0.0);
    let max_y = (canvas_h - dropped.h).max(0.0);
    let start_x = snap_to_grid(dropped.x.clamp(0.0, max_x));
    let start_y = snap_to_grid(dropped.y.clamp(0.0, max_y));

    let collides = |x: f32, y: f32| {
        let candidate = PanelGeom { x, y, w: dropped.w, h: dropped.h };
        others.iter().any(|o| candidate.overlaps(o))
    };

    if !collides(start_x, start_y) {
        return (start_x, start_y);
    }

    const MAX_RING: i32 = 40; // 40 * GRID_SIZE = 800px out in each direction
    let mut best: Option<(f32, f32, f32)> = None; // (x, y, distance_sq)
    for ring in 1..=MAX_RING {
        for gx in -ring..=ring {
            for gy in -ring..=ring {
                if gx.abs() != ring && gy.abs() != ring {
                    continue; // interior cells were already covered by an earlier, smaller ring
                }
                let cx = snap_to_grid((start_x + gx as f32 * GRID_SIZE).clamp(0.0, max_x));
                let cy = snap_to_grid((start_y + gy as f32 * GRID_SIZE).clamp(0.0, max_y));
                if collides(cx, cy) {
                    continue;
                }
                let (dx, dy) = (cx - dropped.x, cy - dropped.y);
                let dist_sq = dx * dx + dy * dy;
                if best.is_none_or(|(_, _, d)| dist_sq < d) {
                    best = Some((cx, cy, dist_sq));
                }
            }
        }
        if best.is_some() {
            break;
        }
    }
    best.map(|(x, y, _)| (x, y)).unwrap_or((start_x, start_y))
}

/// Registers a font file's bytes with Slint's (shared, process-wide) text
/// engine and returns the family name it was registered under -- read back
/// from the engine itself rather than parsed independently, since that's
/// the name `font-family`/`Window.default-font-family` need to actually
/// pick it up. Must run on the UI thread: `shared_collection()` reaches
/// into per-window state that isn't safe to touch from a background thread.
fn register_font_bytes(data: Vec<u8>) -> anyhow::Result<String> {
    let blob = fontique::Blob::new(std::sync::Arc::new(data));
    let mut collection = slint::fontique_010::shared_collection();
    let registered = collection.register_fonts(blob, None);
    let (family_id, _) =
        registered.into_iter().next().ok_or_else(|| anyhow::anyhow!("no fonts found in that file"))?;
    let name = collection
        .family_name(family_id)
        .ok_or_else(|| anyhow::anyhow!("font has no family name"))?
        .to_string();
    Ok(name)
}

/// Sets every collapsible Settings section to the same starting state, per
/// the General tab's "sections start out expanded/collapsed" preference.
/// Deliberately a blanket setting, not per-section memory -- see
/// `config::AppConfig::sections_expanded_default`'s doc comment.
fn apply_section_defaults(app: &MainWindow, expanded: bool) {
    app.set_sec_presets(expanded);
    app.set_sec_callcontrols(expanded);
    app.set_sec_gate(expanded);
    app.set_sec_hpf(expanded);
    app.set_sec_lpf(expanded);
    app.set_sec_deesser(expanded);
    app.set_sec_eq(expanded);
    app.set_sec_comp(expanded);
    app.set_sec_limiter(expanded);
    app.set_sec_pitch(expanded);
    app.set_sec_reverb(expanded);
    app.set_sec_profile(expanded);
    app.set_sec_about(expanded);
    app.set_sec_accent(expanded);
    app.set_sec_theme(expanded);
    app.set_sec_textmotion(expanded);
    app.set_sec_importexport(expanded);
    app.set_sec_callstart(expanded);
    app.set_sec_callparticipants(expanded);
    app.set_sec_calladvanced(expanded);
    app.set_sec_soundboard(expanded);
    app.set_sec_notifications(expanded);
    app.set_sec_transferlimits(expanded);
    app.set_sec_sharedfiles(expanded);
    app.set_sec_captions(expanded);
    app.set_sec_vpn(expanded);
    app.set_sec_discovery(expanded);
}

/// Computes the profile bar's avatar-circle letter from the current
/// display name -- Rust does the string-uppercase-first-char work since
/// `.slint` expressions can't.
/// First letter of a name, uppercased, for the letter avatars; "?" for an
/// empty name.
fn initial_of(name: &str) -> String {
    name.trim().chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".to_string())
}

fn update_display_initial(app: &MainWindow) {
    let name = app.get_display_name();
    let initial = name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "Y".to_string());
    app.set_display_initial(initial.into());
}

fn apply_config_to_ui(app: &MainWindow, config: &config::AppConfig) {
    app.set_display_name(config.profile.display_name.clone().into());
    app.set_status_message(config.profile.status_message.clone().into());
    update_display_initial(app);

    app.set_input_devices(device_model(list_input_devices()));
    app.set_output_devices(device_model(list_output_devices()));
    app.set_selected_input_device(config.input_device.clone().unwrap_or_else(|| DEFAULT_DEVICE_LABEL.to_string()).into());
    app.set_selected_output_device(config.output_device.clone().unwrap_or_else(|| DEFAULT_DEVICE_LABEL.to_string()).into());

    app.set_ptt_key_name(config.ptt_key.clone().into());
    app.set_transmit_mode(config.transmit_mode);
    app.set_mute_key_name(config.mute_key.clone().into());
    app.set_deafen_key_name(config.deafen_key.clone().into());
    app.set_hangup_key_name(config.hangup_key.clone().into());
    app.set_replay_key_name(config.replay_key.clone().into());
    app.set_va_threshold(config.va_threshold);

    app.set_theme_mode(config.theme_mode);
    app.set_text_scale(config.text_scale);
    app.set_reduced_motion(config.reduced_motion);
    app.set_server_mode(config.server_mode);
    app.set_start_maximized(config.start_maximized);
    app.set_output_gain_db(config.output_gain_db);
    app.set_sections_expanded_default(config.sections_expanded_default);
    apply_section_defaults(app, config.sections_expanded_default);
    let accent = color_from_hex(&config.accent_hex).unwrap_or(slint::Color::from_rgb_u8(0x2d, 0x7d, 0x5a));
    set_accent_color(app, accent);
    app.set_font_family_choice(config.font_family.clone().into());

    let theme_bg = color_from_hex(&config.theme_bg_hex).unwrap_or(slint::Color::from_rgb_u8(0x20, 0x20, 0x20));
    set_theme_bg_color(app, theme_bg);
    app.set_theme_image_opacity(config.theme_image_opacity);
    app.set_theme_opacity(config.theme_opacity);
    app.set_theme_image_fit(config.theme_image_fit);
    app.set_theme_image_rotation(config.theme_image_rotation);
    app.set_theme_image_crop_left(config.theme_image_crop_left);
    app.set_theme_image_crop_top(config.theme_image_crop_top);
    app.set_theme_image_crop_right(config.theme_image_crop_right);
    app.set_theme_image_crop_bottom(config.theme_image_crop_bottom);
    app.set_avatar_image_fit(config.avatar_image_fit);
    app.set_avatar_image_rotation(config.avatar_image_rotation);
    app.set_avatar_image_crop_left(config.avatar_image_crop_left);
    app.set_avatar_image_crop_top(config.avatar_image_crop_top);
    app.set_avatar_image_crop_right(config.avatar_image_crop_right);
    app.set_avatar_image_crop_bottom(config.avatar_image_crop_bottom);
    app.set_sidebar_width(config.sidebar_width);
    apply_layout_names(app, &config.saved_layouts);
    apply_sound_clips_to_ui(app, &config.sound_clips);
    app.set_reaction_choices(string_vec_to_model(
        reactions::AVAILABLE.iter().map(|e| e.to_string()).collect(),
    ));
    apply_onboarding(app, config);
    apply_blocked_to_ui(app, &config.blocked);
    app.set_block_explanation(moderation::BLOCK_EXPLANATION.into());
    app.set_report_explanation(moderation::REPORT_EXPLANATION.into());
    let gallery: Vec<ThemePresetEntry> = theme_presets::PRESETS
        .iter()
        .map(|p| ThemePresetEntry {
            name: p.name.into(),
            description: p.description.into(),
            accent: color_from_hex(p.accent_hex).unwrap_or_default(),
            background: color_from_hex(p.background_hex).unwrap_or_default(),
        })
        .collect();
    app.set_theme_presets(ModelRc::new(VecModel::from(gallery)));
    apply_panel_rect(config.call_connect_rect, |x| app.set_call_connect_x(x), |y| app.set_call_connect_y(y), |w| app.set_call_connect_width(w), |h| app.set_call_connect_height(h));
    apply_panel_rect(
        config.call_filetransfer_rect,
        |x| app.set_call_filetransfer_x(x),
        |y| app.set_call_filetransfer_y(y),
        |w| app.set_call_filetransfer_width(w),
        |h| app.set_call_filetransfer_height(h),
    );
    apply_panel_rect(
        config.call_invite_rect,
        |x| app.set_call_invite_x(x),
        |y| app.set_call_invite_y(y),
        |w| app.set_call_invite_width(w),
        |h| app.set_call_invite_height(h),
    );
    apply_panel_rect(
        config.call_profile_rect,
        |x| app.set_call_profile_x(x),
        |y| app.set_call_profile_y(y),
        |w| app.set_call_profile_width(w),
        |h| app.set_call_profile_height(h),
    );
    apply_panel_rect(
        config.call_chat_rect,
        |x| app.set_call_chat_x(x),
        |y| app.set_call_chat_y(y),
        |w| app.set_call_chat_width(w),
        |h| app.set_call_chat_height(h),
    );
    apply_panel_rect(
        config.call_contacts_rect,
        |x| app.set_call_contacts_x(x),
        |y| app.set_call_contacts_y(y),
        |w| app.set_call_contacts_width(w),
        |h| app.set_call_contacts_height(h),
    );
    app.set_call_panel_visible_contacts(config.call_contacts_visible);
    apply_contacts_to_ui(app, &config.contacts);
    apply_presets_to_ui(app, &config.voice_presets);
    app.set_call_panel_visible_chat(config.call_chat_visible);
    app.set_call_panel_visible_connect(config.call_connect_visible);
    app.set_call_panel_visible_filetransfer(config.call_filetransfer_visible);
    app.set_call_panel_visible_invite(config.call_invite_visible);
    app.set_audio_order(string_vec_to_model(config::sanitize_order(
        config.audio_section_order.clone(),
        config::AUDIO_SECTION_IDS,
    )));
    app.set_appearance_order(string_vec_to_model(config::sanitize_order(
        config.appearance_section_order.clone(),
        config::APPEARANCE_SECTION_IDS,
    )));
    app.set_profile_order(string_vec_to_model(config::sanitize_order(
        config.profile_section_order.clone(),
        config::PROFILE_SECTION_IDS,
    )));
    app.set_call_order(string_vec_to_model(config::sanitize_order(
        config.call_section_order.clone(),
        config::CALL_SECTION_IDS,
    )));
    app.set_ip_direct_calling(config.ip_direct_calling);
    app.set_relay_server(config.relay_server.clone().into());
    app.set_fallback_relays_text(config.fallback_relays.join("\n").into());
    app.set_use_project_relays(config.use_project_relays);
    app.set_anonymity_mode(config.anonymity_mode);
    app.set_anonymity_hops_text(config.anonymity_hops.join("\n").into());
    app.set_ptt_toggle(config.ptt_toggle);
    app.set_mute_hold(config.mute_hold);
    app.set_deafen_hold(config.deafen_hold);
    app.set_crash_reporting(config.crash_reporting);
    app.set_replay_seconds(config.replay_seconds as i32);
    app.set_replay_mode(config.replay_mode);
    app.set_outbox_give_up_hours(config.outbox_give_up_hours as i32);
    apply_routes_to_ui(app, config);
    app.set_silent_call_mode(config.silent_call_mode);
    app.set_start_call_silently(config.start_call_silently);
    app.set_echo_cancel(config.echo_cancel);
    app.set_duck_db(config.duck_db as i32);
    let chosen = presence::Presence::from_i32(config.presence);
    app.set_my_presence(chosen.to_i32());
    app.set_presence_note(chosen.explain().into());
    app.set_upload_kbps(config.transfer_limits.upload_kbps as i32);
    app.set_throttle_during_calls(config.transfer_limits.throttle_during_calls);
    app.set_in_call_kbps(config.transfer_limits.in_call_kbps as i32);
    app.set_transfer_limit_note(config.transfer_limits.describe(false).into());
    app.set_default_ring_rule(config.default_ring_rule);
    app.set_quiet_hours_enabled(config.quiet_hours.enabled);
    app.set_quiet_hours_start(config.quiet_hours.start_minute as i32);
    app.set_quiet_hours_end(config.quiet_hours.end_minute as i32);
    app.set_deesser_attack(config.chain.de_esser.attack_ms);
    app.set_deesser_release(config.chain.de_esser.release_ms);
    app.set_ptt_release_ms(config.ptt_release_ms);
    app.set_start_with_screen_share(config.start_with_screen_share);
    app.set_start_with_camera(config.start_with_camera);
    app.set_mute_others_by_default(config.mute_others_by_default);
    app.set_mute_self_on_join(config.mute_self_on_join);
    if config.theme_image_path.is_empty() {
        app.set_theme_bg_image_set(false);
        animated_image::stop_animation(app, animated_image::ImageSlot::ThemeBackground);
    } else {
        let path = std::path::Path::new(&config.theme_image_path);
        // Startup runs on the UI thread already, so `load_raw` and
        // `apply_raw_loaded` can run back-to-back here with no thread hop
        // -- unlike the runtime import handlers, which pick the file (and
        // therefore decode it) off-thread.
        let result = animated_image::load_raw(path).and_then(|raw| {
            app.set_theme_bg_image_set(true);
            animated_image::apply_raw_loaded(
                app,
                app.as_weak(),
                raw,
                animated_image::ImageSlot::ThemeBackground,
                Rc::new(|app: &MainWindow, img| app.set_theme_bg_image(img)),
            )
        });
        if let Err(e) = result {
            eprintln!("Could not reload theme background image {}: {e}", config.theme_image_path);
            app.set_theme_bg_image_set(false);
        }
    }

    if config.profile_avatar_path.is_empty() {
        app.set_profile_avatar_image_set(false);
        animated_image::stop_animation(app, animated_image::ImageSlot::ProfileAvatar);
    } else {
        let path = std::path::Path::new(&config.profile_avatar_path);
        let result = animated_image::load_raw(path).and_then(|raw| {
            app.set_profile_avatar_image_set(true);
            animated_image::apply_raw_loaded(
                app,
                app.as_weak(),
                raw,
                animated_image::ImageSlot::ProfileAvatar,
                Rc::new(|app: &MainWindow, img| app.set_profile_avatar_image(img)),
            )
        });
        if let Err(e) = result {
            eprintln!("Could not reload profile avatar {}: {e}", config.profile_avatar_path);
            app.set_profile_avatar_image_set(false);
        }
    }
    app.set_profile_widget_show_avatar(config.profile_widget_show_avatar);
    app.set_profile_widget_show_name(config.profile_widget_show_name);
    app.set_profile_widget_show_status(config.profile_widget_show_status);

    apply_chain_to_ui(app, &config.chain);
}

/// Pushes one audio chain into every DSP control in the UI. Split out of
/// `apply_config_to_ui` so loading a voice preset -- which replaces the
/// chain and nothing else -- goes through exactly the same path as loading
/// the saved config, instead of a parallel copy that could drift.
fn apply_chain_to_ui(app: &MainWindow, chain: &ChainConfig) {
    let c = chain;
    app.set_ns_enabled(c.noise_suppress.enabled);
    app.set_ns_strength(c.noise_suppress.strength);
    app.set_ns_neural(c.noise_suppress.neural);
    app.set_gate_enabled(c.noise_gate.enabled);
    app.set_gate_threshold(c.noise_gate.threshold_db);
    app.set_gate_attack(c.noise_gate.attack_ms);
    app.set_gate_hold(c.noise_gate.hold_ms);
    app.set_gate_release(c.noise_gate.release_ms);
    app.set_hpf_enabled(c.high_pass.enabled);
    app.set_hpf_cutoff(c.high_pass.cutoff_hz);
    app.set_lpf_enabled(c.low_pass.enabled);
    app.set_lpf_cutoff(c.low_pass.cutoff_hz);
    app.set_deesser_enabled(c.de_esser.enabled);
    app.set_deesser_threshold(c.de_esser.threshold_db);
    app.set_deesser_ratio(c.de_esser.ratio);
    app.set_deesser_split(c.de_esser.split_hz);
    app.set_comp_enabled(c.compressor.enabled);
    app.set_comp_threshold(c.compressor.threshold_db);
    app.set_comp_ratio(c.compressor.ratio);
    app.set_limiter_enabled(c.limiter.enabled);
    app.set_limiter_ceiling(c.limiter.ceiling_db);
    match &c.gain {
        GainConfig::Auto { target_db, .. } => {
            app.set_gain_auto(true);
            app.set_gain_target(*target_db);
        }
        GainConfig::Fixed { gain_db, .. } => {
            app.set_gain_auto(false);
            app.set_gain_target(*gain_db);
        }
    }
    apply_eq_bands_to_ui(app, &c.eq_bands);

    app.set_pitch_enabled(c.pitch_shift.enabled);
    app.set_pitch_semitones(c.pitch_shift.semitones);
    app.set_pitch_preserve_formants(c.pitch_shift.preserve_formants);
    app.set_reverb_enabled(c.reverb.enabled);
    app.set_reverb_room_size(c.reverb.room_size);
    app.set_reverb_damping(c.reverb.damping);
    app.set_reverb_wet(c.reverb.wet_dry);
}

fn eq_band_to_ui(b: &EqBandConfig) -> EqBand {
    EqBand {
        enabled: b.enabled,
        freq: b.center_hz,
        gain: b.gain_db,
        q: b.q,
        band_type: b.band_type.to_i32(),
    }
}

fn eq_band_from_ui(b: &EqBand) -> EqBandConfig {
    EqBandConfig {
        enabled: b.enabled,
        center_hz: b.freq,
        gain_db: b.gain,
        q: b.q,
        band_type: antiphon_audio::BandType::from_i32(b.band_type),
    }
}

/// Bands are a real add/remove-able list now (spec §1 "Multi-band" --
/// previously a fixed three slots), backed by a `VecModel` so rows can be
/// pushed/removed and the UI just reacts.
fn apply_eq_bands_to_ui(app: &MainWindow, bands: &[EqBandConfig]) {
    let rows: Vec<EqBand> = bands.iter().map(eq_band_to_ui).collect();
    app.set_eq_bands(ModelRc::new(VecModel::from(rows)));
    recompute_eq_curve(app);
}

fn eq_bands_from_ui(app: &MainWindow) -> Vec<EqBandConfig> {
    app.get_eq_bands().iter().map(|b| eq_band_from_ui(&b)).collect()
}

/// Reference sample rate for the *preview* curve only -- the real chain
/// uses whatever the active device's native rate is. The curve's shape at
/// audible frequencies barely moves between common rates (44.1k/48k/etc.),
/// so a fixed reference is an honest, simple stand-in rather than needing
/// a live device rate before any call has started.
const EQ_CURVE_PREVIEW_SAMPLE_RATE: u32 = 48_000;
const EQ_CURVE_POINTS: usize = 128;
const EQ_CURVE_MIN_HZ: f32 = 20.0;
const EQ_CURVE_MAX_HZ: f32 = 20_000.0;
const EQ_CURVE_DB_RANGE: f32 = 18.0;
const EQ_CURVE_VIEWBOX_W: f32 = 300.0;
const EQ_CURVE_VIEWBOX_H: f32 = 100.0;

/// Recomputes the EQ curve graph from the current bands and pushes it to
/// the UI as an SVG path string -- the "funny wave thing". Cascaded biquads
/// sum in dB, so this doesn't need to run any audio through anything, just
/// evaluate each enabled band's closed-form magnitude response and add.
fn recompute_eq_curve(app: &MainWindow) {
    let bands = eq_bands_from_ui(app);
    let freqs = antiphon_audio::log_spaced_frequencies(EQ_CURVE_MIN_HZ, EQ_CURVE_MAX_HZ, EQ_CURVE_POINTS);
    let curve_db = antiphon_audio::eq_curve_db(&bands, EQ_CURVE_PREVIEW_SAMPLE_RATE, &freqs);

    let mut path = String::new();
    for (i, (&f, &db)) in freqs.iter().zip(curve_db.iter()).enumerate() {
        let x = EQ_CURVE_VIEWBOX_W * (f / EQ_CURVE_MIN_HZ).ln() / (EQ_CURVE_MAX_HZ / EQ_CURVE_MIN_HZ).ln();
        let y = (EQ_CURVE_VIEWBOX_H / 2.0 - (db / EQ_CURVE_DB_RANGE) * (EQ_CURVE_VIEWBOX_H / 2.0)).clamp(0.0, EQ_CURVE_VIEWBOX_H);
        path.push_str(if i == 0 { "M " } else { "L " });
        path.push_str(&format!("{x:.1},{y:.1} "));
    }
    app.set_eq_curve_svg(path.into());
}

/// Inverse of the x-axis mapping in [`recompute_eq_curve`]: a horizontal
/// drag fraction (0 = left edge, 1 = right edge) back to Hz.
fn eq_curve_x_to_freq(x_frac: f32) -> f32 {
    (EQ_CURVE_MIN_HZ * (EQ_CURVE_MAX_HZ / EQ_CURVE_MIN_HZ).powf(x_frac.clamp(0.0, 1.0))).clamp(EQ_CURVE_MIN_HZ, EQ_CURVE_MAX_HZ)
}

/// Inverse of the y-axis mapping: a vertical drag fraction (0 = top,
/// 1 = bottom) back to dB.
fn eq_curve_y_to_db(y_frac: f32) -> f32 {
    EQ_CURVE_DB_RANGE - 2.0 * EQ_CURVE_DB_RANGE * y_frac.clamp(0.0, 1.0)
}

/// The band whose center is closest to `freq` in log space, if any band is
/// within about a third of an octave -- close enough that grabbing "near"
/// a point on the curve reliably grabs *that* point instead of drifting to
/// a farther-off band, or (if nothing is that close) creating a new one.
fn nearest_band_within_reach(bands: &[EqBandConfig], freq: f32) -> Option<usize> {
    const MAX_LOG_DISTANCE: f32 = 0.4; // ln(freq ratio); ~0.4 is a bit under an octave
    bands
        .iter()
        .enumerate()
        .map(|(i, b)| (i, (b.center_hz.ln() - freq.ln()).abs()))
        .filter(|&(_, dist)| dist <= MAX_LOG_DISTANCE)
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(i, _)| i)
}

fn current_config(app: &MainWindow) -> config::AppConfig {
    let (input, output) = selected_devices(app);
    // `custom_font_path` has no corresponding UI property (importing a font
    // sets it directly, see `on_import_font`) -- carry the previously saved
    // value forward so every other settings change doesn't silently wipe it.
    let previous = config::load();
    config::AppConfig {
        // The iced app's own settings; this app only carries them forward.
        stoop: previous.stoop.clone(),
        profile: config::ProfileData {
            display_name: app.get_display_name().to_string(),
            status_message: app.get_status_message().to_string(),
        },
        chain: read_chain_config(app),
        input_device: input,
        output_device: output,
        ptt_key: app.get_ptt_key_name().to_string(),
        transmit_mode: app.get_transmit_mode(),
        mute_key: app.get_mute_key_name().to_string(),
        deafen_key: app.get_deafen_key_name().to_string(),
        hangup_key: app.get_hangup_key_name().to_string(),
        replay_key: app.get_replay_key_name().to_string(),
        va_threshold: app.get_va_threshold(),
        theme_mode: app.get_theme_mode(),
        text_scale: app.get_text_scale(),
        reduced_motion: app.get_reduced_motion(),
        server_mode: app.get_server_mode(),
        start_maximized: app.get_start_maximized(),
        window: capture_window_state(app, &previous.window),
        output_gain_db: app.get_output_gain_db(),
        sections_expanded_default: app.get_sections_expanded_default(),
        accent_hex: color_to_hex(app.get_accent_choice()),
        font_family: app.get_font_family_choice().to_string(),
        custom_font_path: previous.custom_font_path,
        theme_bg_hex: color_to_hex(app.get_theme_bg_color()),
        // No corresponding UI property, same reasoning as `custom_font_path`
        // above -- `on_import_theme_image`/`on_clear_theme_image` set it directly.
        theme_image_path: previous.theme_image_path,
        theme_image_opacity: app.get_theme_image_opacity(),
        theme_opacity: app.get_theme_opacity(),
        audio_section_order: if app.get_settings_search().trim().is_empty() {
            model_to_string_vec(app.get_audio_order())
        } else {
            previous.audio_section_order.clone()
        },
        appearance_section_order: if app.get_settings_search().trim().is_empty() {
            model_to_string_vec(app.get_appearance_order())
        } else {
            previous.appearance_section_order.clone()
        },
        profile_section_order: if app.get_settings_search().trim().is_empty() {
            model_to_string_vec(app.get_profile_order())
        } else {
            previous.profile_section_order.clone()
        },
        // Section orders are read back from the saved config, not the UI:
        // while a settings search is active the UI models hold only the
        // filtered subset, and saving that would permanently delete every
        // section the search happened to hide.
        call_section_order: previous.call_section_order.clone(),
        ip_direct_calling: app.get_ip_direct_calling(),
        relay_server: app.get_relay_server().trim().to_string(),
        relay_invite_secret: previous.relay_invite_secret.clone(),
        fallback_relays: fallback_relay::parse_list(&app.get_fallback_relays_text()),
        use_project_relays: app.get_use_project_relays(),
        anonymity_mode: app.get_anonymity_mode(),
        anonymity_hops: fallback_relay::parse_list(&app.get_anonymity_hops_text()),
        routes: previous.routes.clone(),
        route_sequence: previous.route_sequence,
        route_fan_out: app.get_route_fan_out(),
        strict_mode: app.get_strict_mode(),
        hold_mail_for_devices: app.get_hold_mail(),
        hold_mail_port: u16::try_from(app.get_hold_mail_port()).unwrap_or(3480),
        captions_enabled: app.get_captions_enabled(),
        captions_own_voice: app.get_captions_own_voice(),
        seeding: app.get_seeding(),
        anonymous_seeding: app.get_anonymous_seeding(),
        seed_scope: app.get_seed_scope().clamp(0, 2),
        seed_port: previous.seed_port,
        pin_nodes: app
            .get_pin_nodes()
            .split(',')
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect(),
        pin_code: app.get_pin_code().trim().to_string(),
        vpn_enabled: app.get_vpn_enabled(),
        vpn_active: previous.vpn_active.clone(),
        vpn_route_voice: app.get_vpn_route_voice(),
        vpn_route_files: app.get_vpn_route_files(),
        vpn_route_messages: app.get_vpn_route_messages(),
        vpn_kill_switch: app.get_vpn_kill_switch(),
        dht_enabled: app.get_dht_enabled(),
        dht_bootstrap: split_list(&app.get_dht_bootstrap()),
        directories: split_list(&app.get_directories()),
        directory_pins: previous.directory_pins.clone(),
        ptt_toggle: app.get_ptt_toggle(),
        mute_hold: app.get_mute_hold(),
        deafen_hold: app.get_deafen_hold(),
        ptt_release_ms: app.get_ptt_release_ms(),
        start_with_screen_share: app.get_start_with_screen_share(),
        start_with_camera: app.get_start_with_camera(),
        mute_others_by_default: app.get_mute_others_by_default(),
        mute_self_on_join: app.get_mute_self_on_join(),
        sidebar_width: app.get_sidebar_width(),
        theme_image_fit: app.get_theme_image_fit(),
        theme_image_rotation: app.get_theme_image_rotation(),
        theme_image_crop_left: app.get_theme_image_crop_left(),
        theme_image_crop_top: app.get_theme_image_crop_top(),
        theme_image_crop_right: app.get_theme_image_crop_right(),
        theme_image_crop_bottom: app.get_theme_image_crop_bottom(),
        avatar_image_fit: app.get_avatar_image_fit(),
        avatar_image_rotation: app.get_avatar_image_rotation(),
        avatar_image_crop_left: app.get_avatar_image_crop_left(),
        avatar_image_crop_top: app.get_avatar_image_crop_top(),
        avatar_image_crop_right: app.get_avatar_image_crop_right(),
        avatar_image_crop_bottom: app.get_avatar_image_crop_bottom(),
        // Saved arrangements aren't represented in the UI beyond their
        // names, so they're carried from the stored config rather than
        // reconstructed -- rebuilding them from the window would flatten
        // every saved layout into whatever is on screen right now.
        saved_layouts: previous.saved_layouts.clone(),
        // Same reasoning as saved layouts: the UI shows clip names, not
        // their paths and gains, so these come from the stored config.
        sound_clips: previous.sound_clips.clone(),
        transfer_limits: transfer_limits::TransferLimits {
            upload_kbps: app.get_upload_kbps().max(0) as u32,
            throttle_during_calls: app.get_throttle_during_calls(),
            in_call_kbps: app.get_in_call_kbps().max(0) as u32,
        },
        replay_seconds: app.get_replay_seconds().max(0) as u32,
        replay_mode: app.get_replay_mode(),
        outbox_give_up_hours: app.get_outbox_give_up_hours().max(0) as u32,
        silent_call_mode: app.get_silent_call_mode(),
        start_call_silently: app.get_start_call_silently(),
        echo_cancel: app.get_echo_cancel(),
        duck_db: app.get_duck_db().max(0) as u32,
        presence: app.get_my_presence(),
        crash_reporting: app.get_crash_reporting(),
        // Onboarding progress isn't a UI-editable setting; it advances
        // through its own callbacks, so it rides along from the stored
        // config rather than being rebuilt from the window.
        // Blocks and reports are managed through their own callbacks,
        // never rebuilt from the window -- the UI shows names, not keys.
        blocked: previous.blocked.clone(),
        reports: previous.reports.clone(),
        first_run: previous.first_run,
        calls_completed: previous.calls_completed,
        default_ring_rule: app.get_default_ring_rule(),
        quiet_hours: crate::notify::QuietHours {
            enabled: app.get_quiet_hours_enabled(),
            start_minute: app.get_quiet_hours_start() as u32,
            end_minute: app.get_quiet_hours_end() as u32,
        },
        call_connect_rect: config::PanelRect {
            x: app.get_call_connect_x(),
            y: app.get_call_connect_y(),
            width: app.get_call_connect_width(),
            height: app.get_call_connect_height(),
        },
        call_filetransfer_rect: config::PanelRect {
            x: app.get_call_filetransfer_x(),
            y: app.get_call_filetransfer_y(),
            width: app.get_call_filetransfer_width(),
            height: app.get_call_filetransfer_height(),
        },
        call_profile_rect: config::PanelRect {
            x: app.get_call_profile_x(),
            y: app.get_call_profile_y(),
            width: app.get_call_profile_width(),
            height: app.get_call_profile_height(),
        },
        call_chat_rect: config::PanelRect {
            x: app.get_call_chat_x(),
            y: app.get_call_chat_y(),
            width: app.get_call_chat_width(),
            height: app.get_call_chat_height(),
        },
        call_chat_visible: app.get_call_panel_visible_chat(),
        call_contacts_rect: config::PanelRect {
            x: app.get_call_contacts_x(),
            y: app.get_call_contacts_y(),
            width: app.get_call_contacts_width(),
            height: app.get_call_contacts_height(),
        },
        call_contacts_visible: app.get_call_panel_visible_contacts(),
        contacts: contacts_from_ui(app),
        // Presets have no single UI property holding their chains -- the
        // list model only carries names -- so they're carried forward from
        // the saved config and only rewritten by the preset handlers.
        voice_presets: previous.voice_presets,
        call_connect_visible: app.get_call_panel_visible_connect(),
        call_filetransfer_visible: app.get_call_panel_visible_filetransfer(),
        call_invite_rect: config::PanelRect {
            x: app.get_call_invite_x(),
            y: app.get_call_invite_y(),
            width: app.get_call_invite_width(),
            height: app.get_call_invite_height(),
        },
        call_invite_visible: app.get_call_panel_visible_invite(),
        // No corresponding UI property, same reasoning as `custom_font_path`
        // above -- `on_import_profile_avatar`/`on_clear_profile_avatar` set it directly.
        profile_avatar_path: previous.profile_avatar_path,
        profile_widget_show_avatar: app.get_profile_widget_show_avatar(),
        profile_widget_show_name: app.get_profile_widget_show_name(),
        profile_widget_show_status: app.get_profile_widget_show_status(),
    }
}

fn save_current_config(app: &MainWindow) {
    let before = config::load().profile;
    let now = current_config(app);
    let renamed = before.display_name != now.profile.display_name || before.status_message != now.profile.status_message;
    config::save(&now);
    // Spaces where you kept your profile's name or status show the new
    // one (spec §7); refresh_cards skips any where nothing changed.
    if renamed {
        std::thread::spawn(community::refresh_cards);
    }
}

/// Saves settings, spawns a fresh instance of this same executable, and
/// quits this one -- how server mode switches between the widget UI and
/// the console. The two surfaces differ enough that a clean restart into
/// the right one beats morphing in place, and it exercises the same
/// startup path a real reboot-into-server-mode would. The old process's
/// sockets close on exit before the new instance gets far enough to
/// listen, so the port isn't contested.
fn relaunch_app(app: &MainWindow) {
    save_current_config(app);
    match std::env::current_exe() {
        Ok(exe) => {
            if let Err(e) = std::process::Command::new(exe).spawn() {
                eprintln!("Could not relaunch: {e}");
            }
        }
        Err(e) => eprintln!("Could not determine own executable to relaunch: {e}"),
    }
    let _ = slint::quit_event_loop();
}

/// Reads the window's current geometry off the live `slint::Window`, not a
/// `.slint` property -- position/size/maximized aren't exposed that way,
/// they're read and set directly through `ComponentHandle::window()`.
///
/// A maximized window's reported size/position is the maximized extent,
/// not the restored one -- saving that would make "un-maximize" always
/// land back at full-screen size. So while maximized, `previous`'s
/// geometry (the last time it was saved un-maximized) is kept as-is;
/// only the `maximized` flag itself updates.
fn capture_window_state(app: &MainWindow, previous: &config::WindowState) -> config::WindowState {
    let window = app.window();
    let maximized = window.is_maximized();
    if maximized {
        config::WindowState { maximized: true, ..previous.clone() }
    } else {
        let size = window.size();
        let position = window.position();
        config::WindowState {
            width: Some(size.width),
            height: Some(size.height),
            x: Some(position.x),
            y: Some(position.y),
            maximized: false,
        }
    }
}

/// Applies remembered geometry before the window is shown. `start_maximized`
/// wins over a remembered non-maximized size; a remembered maximized state
/// also wins (both mean "open maximized"), but either way the restored
/// size/position are set first so un-maximizing later lands somewhere sane.
fn apply_window_state(app: &MainWindow, saved: &config::WindowState, start_maximized: bool) {
    let window = app.window();
    if let (Some(w), Some(h)) = (saved.width, saved.height) {
        window.set_size(slint::PhysicalSize::new(w, h));
    }
    if let (Some(x), Some(y)) = (saved.x, saved.y) {
        window.set_position(slint::PhysicalPosition::new(x, y));
    }
    if start_maximized || saved.maximized {
        window.set_maximized(true);
    }
}

/// Pushes the transmit-mode UI properties (PTT key, voice-activation mode
/// and its sensitivity) into the shared [`TransmitSettings`], so a change
/// applies to a running call immediately rather than only on the next one.
fn sync_transmit_from_ui(app: &MainWindow, state: &AppState) {
    let mode = match app.get_transmit_mode() {
        1 => MODE_VOICE_ACTIVATION,
        2 => MODE_OPEN_MIC,
        _ => MODE_PTT,
    };
    state.transmit.transmit_mode.store(mode, Ordering::Relaxed);
    *state.transmit.va_threshold.lock().unwrap() = app.get_va_threshold();
    state.transmit.ptt_toggle.store(app.get_ptt_toggle(), Ordering::Relaxed);
    state.transmit.mute_hold.store(app.get_mute_hold(), Ordering::Relaxed);
    state.transmit.deafen_hold.store(app.get_deafen_hold(), Ordering::Relaxed);
    state.transmit.ptt_release_ms.store(app.get_ptt_release_ms() as u32, Ordering::Relaxed);
    state.duck_db.store(app.get_duck_db().max(0) as u32, Ordering::Relaxed);
    // Spec §1: switching echo cancellation on mid-call applies to that
    // call. Turning it *off* resets the filter -- leaving a converged
    // filter sitting idle means the next time it is switched on it is
    // describing an echo path from whenever it was last used, which may
    // be a different room entirely.
    let echo_on = app.get_echo_cancel();
    if !echo_on && state.echo_cancel.swap(false, Ordering::Relaxed) {
        state.echo_canceller.lock().unwrap().reset();
    } else {
        state.echo_cancel.store(echo_on, Ordering::Relaxed);
    }
    // Leaving toggle mode with the latch still on would keep the mic open
    // with no key held and no obvious way to tell why.
    if !app.get_ptt_toggle() {
        state.transmit.ptt_latched.store(false, Ordering::Relaxed);
    }
    if let Ok(key) = Keycode::from_str(&app.get_ptt_key_name()) {
        *state.transmit.ptt_key.lock().unwrap() = key;
    }
    caps_lock::set_bound(
        *state.transmit.ptt_key.lock().unwrap() == Keycode::CapsLock && mode == MODE_PTT,
    );
    // An unparseable or empty name means "unbound", which is a normal
    // state for every shortcut except push-to-talk.
    let parse = |name: slint::SharedString| Keycode::from_str(&name).ok();
    *state.transmit.mute_key.lock().unwrap() = parse(app.get_mute_key_name());
    *state.transmit.deafen_key.lock().unwrap() = parse(app.get_deafen_key_name());
    *state.transmit.hangup_key.lock().unwrap() = parse(app.get_hangup_key_name());
    *state.transmit.replay_key.lock().unwrap() = parse(app.get_replay_key_name());
    *state.output_gain_db.lock().unwrap() = app.get_output_gain_db();
}

/// Writes a captured key into whichever binding the user was rebinding.
/// One function rather than five callbacks, keyed by the same id string
/// the `.slint` side passes to `capture-binding`.
fn set_binding_name(app: &MainWindow, id: &str, name: &str) {
    match id {
        "ptt" => app.set_ptt_key_name(name.into()),
        "mute" => app.set_mute_key_name(name.into()),
        "deafen" => app.set_deafen_key_name(name.into()),
        "hangup" => app.set_hangup_key_name(name.into()),
        "replay" => app.set_replay_key_name(name.into()),
        _ => {}
    }
}

fn read_chain_config(app: &MainWindow) -> ChainConfig {
    ChainConfig {
        name: "gui".to_string(),
        noise_suppress: antiphon_audio::NoiseSuppressConfig {
            enabled: app.get_ns_enabled(),
            strength: app.get_ns_strength(),
            neural: app.get_ns_neural(),
        },
        noise_gate: NoiseGateConfig {
            enabled: app.get_gate_enabled(),
            threshold_db: app.get_gate_threshold(),
            attack_ms: app.get_gate_attack(),
            hold_ms: app.get_gate_hold(),
            release_ms: app.get_gate_release(),
        },
        high_pass: HighPassConfig { enabled: app.get_hpf_enabled(), cutoff_hz: app.get_hpf_cutoff() },
        low_pass: LowPassConfig { enabled: app.get_lpf_enabled(), cutoff_hz: app.get_lpf_cutoff() },
        de_esser: DeEsserConfig {
            enabled: app.get_deesser_enabled(),
            split_hz: app.get_deesser_split(),
            threshold_db: app.get_deesser_threshold(),
            ratio: app.get_deesser_ratio(),
            attack_ms: app.get_deesser_attack(),
            release_ms: app.get_deesser_release(),
        },
        eq_bands: eq_bands_from_ui(app),
        compressor: CompressorConfig {
            enabled: app.get_comp_enabled(),
            threshold_db: app.get_comp_threshold(),
            ratio: app.get_comp_ratio(),
            attack_ms: 8.0,
            release_ms: 120.0,
            makeup_gain_db: 4.0,
        },
        limiter: LimiterConfig { enabled: app.get_limiter_enabled(), ceiling_db: app.get_limiter_ceiling(), release_ms: 60.0 },
        gain: if app.get_gain_auto() {
            GainConfig::Auto { enabled: true, target_db: app.get_gain_target(), min_gain_db: -12.0, max_gain_db: 24.0, adapt_time_ms: 500.0 }
        } else {
            GainConfig::Fixed { enabled: true, gain_db: app.get_gain_target() }
        },
        pitch_shift: PitchShiftConfig {
            enabled: app.get_pitch_enabled(),
            semitones: app.get_pitch_semitones(),
            preserve_formants: app.get_pitch_preserve_formants(),
        },
        reverb: ReverbConfig {
            enabled: app.get_reverb_enabled(),
            room_size: app.get_reverb_room_size(),
            damping: app.get_reverb_damping(),
            wet_dry: app.get_reverb_wet(),
        },
    }
}

// ---- Spec §9: group chats and communities -----------------------------------

fn selected_space(app: &MainWindow) -> Option<antiphon_crypto::community::Key> {
    connection_mode::decode_hex32(&app.get_selected_space()).map(antiphon_crypto::community::Key)
}

/// Contacts who could be invited: saved with a pinned key and a prekey,
/// so an invite can be sealed to them. The same order every time, so a
/// combo box index maps back.
fn invitable_contacts(config: &config::AppConfig) -> Vec<config::Contact> {
    config
        .contacts
        .iter()
        .filter(|c| connection_mode::decode_hex32(&c.identity_key).is_some() && connection_mode::decode_hex32(&c.prekey).is_some())
        .cloned()
        .collect()
}

fn ago(at_secs: u64) -> String {
    let d = now_secs().saturating_sub(at_secs);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86_399 => format!("{} h ago", d / 3600),
        _ => format!("{} d ago", d / 86_400),
    }
}

/// Runs an action on the selected space and shows how it went.
fn space_action(app: &MainWindow, f: impl FnOnce(&community::Store, &antiphon_crypto::community::Key) -> Result<(), String>) {
    let (Some(store), Some(id)) = (community::Store::app(), selected_space(app)) else { return };
    match f(&store, &id) {
        Ok(()) => app.set_space_status("".into()),
        Err(e) => app.set_space_status(e.into()),
    }
    apply_spaces_to_ui(app);
}

/// Runs a manifest change on the selected space.
fn space_update(app: &MainWindow, change: impl FnOnce(&mut antiphon_crypto::community::Manifest)) {
    space_action(app, |store, id| {
        store.with_space(id, |space, identity| Ok(((), space.update(identity, change)?, vec![community::Event::Changed])))
    });
}

fn apply_spaces_to_ui(app: &MainWindow) {
    use antiphon_crypto::community::{perm, Kind};
    let Some(store) = community::Store::app() else { return };
    let config = config::load();
    let mut spaces: Vec<community::Space> = store.ids().iter().filter_map(|id| store.load(id)).collect();
    spaces.sort_by_key(|s| s.manifest().name.to_lowercase());
    let selected = selected_space(app);
    let viewing = app.get_current_tab() == 2;
    let mut total_unread = 0;
    let mut entries = Vec::new();
    for s in &spaces {
        let m = s.manifest();
        let unread = if viewing && Some(s.id()) == selected {
            store.mark_read(&s.id());
            0
        } else {
            store.unread(&s.id())
        };
        total_unread += unread;
        let kind = if m.kind == Kind::Group { "Group" } else { "Community" };
        entries.push(SpaceEntry {
            id: delivery::hex(&s.id().0).into(),
            name: m.name.clone().into(),
            detail: format!("{kind} · {} members", m.members.len()).into(),
            unread: unread as i32,
        });
    }
    app.set_spaces(ModelRc::new(VecModel::from(entries)));
    app.set_spaces_unread(total_unread as i32);
    let invites: Vec<SpaceInviteEntry> = store
        .invites()
        .into_iter()
        .map(|i| {
            let from = config
                .contacts
                .iter()
                .find(|c| c.identity_key.eq_ignore_ascii_case(&delivery::hex(&i.from.0)))
                .map(|c| c.name.clone())
                .unwrap_or_else(|| format!("{} (not a contact; a member of something you're in)", i.card.name));
            let kind = if i.kind == Kind::Group { "Group chat" } else { "Community" };
            SpaceInviteEntry { name: i.name.into(), detail: format!("{kind} · invited by {from} · {}", ago(i.at_secs)).into() }
        })
        .collect();
    app.set_space_invites(ModelRc::new(VecModel::from(invites)));

    let Some(space) = selected.and_then(|id| spaces.iter().find(|s| s.id() == id)) else {
        app.set_selected_space("".into());
        app.set_space_messages(ModelRc::default());
        app.set_space_members(ModelRc::default());
        return;
    };
    let m = space.manifest();
    let community = m.kind == Kind::Community;
    let me = space.me();
    let joined = space.joining.is_none() && !space.removed;
    app.set_space_title(m.name.clone().into());
    let subtitle = if space.joining.is_some() {
        "Waiting to be let in: whoever invited you adds you when their app next collects mail or you're next on a call.".to_string()
    } else if space.removed {
        "You're no longer in this.".to_string()
    } else {
        format!(
            "{} · {} members · group key {} · {} waiting to send",
            if community { "Community" } else { "Group chat" },
            m.members.len(),
            space.epoch().unwrap_or(0),
            store.queued()
        )
    };
    app.set_space_subtitle(subtitle.into());
    app.set_space_is_community(community);
    app.set_space_separate(space.persona);
    app.set_space_look_defaults(config.profile.display_name.clone().into());
    app.set_space_removed(space.removed);
    let channel_index = (app.get_space_channel_index().max(0) as usize).min(m.channels.len() - 1);
    app.set_space_channel_names(ModelRc::new(VecModel::from(
        m.channels.iter().map(|c| SharedString::from(format!("#{}", c.name))).collect::<Vec<_>>(),
    )));
    app.set_space_channel_index(channel_index as i32);
    let channel = m.channels[channel_index].id;

    let log = history::load(&store.dir, &community::channel_log(&space.id(), channel), &store.key);
    let root: Option<u64> = app.get_space_thread_root().parse().ok();
    let to_entry = |msg: &history::StoredMessage, replies: i32| {
        let sender = msg.sender.as_deref().and_then(connection_mode::decode_hex32).unwrap_or(me);
        SpaceMessage {
            id: msg.id.to_string().into(),
            sender: if sender == me { "You".into() } else { space.name_of(&sender).into() },
            sender_key: delivery::hex(&sender).into(),
            text: msg.display_text().into(),
            time: ago(msg.at_secs).into(),
            mine: sender == me,
            deleted: msg.deleted,
            replies,
            can_delete: joined && (sender == me || m.can(&me, perm::MANAGE_MESSAGES, Some(&sender))),
            can_edit: joined && sender == me && !msg.deleted && !msg.text.starts_with("[file] "),
            plain: msg.text.clone().into(),
        }
    };
    let shown: Vec<SpaceMessage> = match root {
        Some(r) => log.iter().filter(|x| x.id == r || x.reply_to == Some(r)).map(|x| to_entry(x, 0)).collect(),
        None => log
            .iter()
            // A reply whose root isn't here (expired, or from before we
            // joined) is shown in the channel rather than lost.
            .filter(|x| x.reply_to.is_none_or(|r| !log.iter().any(|y| y.id == r)))
            .map(|x| to_entry(x, log.iter().filter(|y| y.reply_to == Some(x.id)).count() as i32))
            .collect(),
    };
    app.set_space_messages(ModelRc::new(VecModel::from(shown)));

    let members: Vec<SpaceMember> = m
        .members
        .iter()
        .map(|x| {
            let k = x.key.0;
            let owner = m.is_owner(&k);
            let card = space.card(&k);
            let avatar = card.and_then(|c| c.avatar.as_deref()).and_then(avatar_image);
            SpaceMember {
                key: delivery::hex(&k).into(),
                name: space.name_of(&k).into(),
                status: card.map(|c| c.status.clone()).unwrap_or_default().into(),
                has_avatar: avatar.is_some(),
                avatar: avatar.unwrap_or_default(),
                role: if owner { "Owner".into() } else { m.roles.get(x.role as usize).map(|r| r.name.clone()).unwrap_or_default().into() },
                role_index: x.role as i32,
                me: k == me,
                can_kick: joined && k != me && m.can(&me, perm::KICK, Some(&k)),
                can_ban: joined && k != me && m.can(&me, perm::BAN, Some(&k)),
                can_role: joined && community && k != me && !owner && m.can(&me, perm::MANAGE_ROLES, Some(&k)),
            }
        })
        .collect();
    app.set_space_members(ModelRc::new(VecModel::from(members)));
    app.set_space_role_names(ModelRc::new(VecModel::from(m.roles.iter().map(|r| SharedString::from(r.name.as_str())).collect::<Vec<_>>())));
    let bans: Vec<SpaceBan> = m
        .bans
        .iter()
        .map(|b| SpaceBan {
            key: delivery::hex(&b.key.0).into(),
            detail: format!(
                "{}{} -- by {}",
                space.name_of(&b.key.0),
                if b.reason.is_empty() { String::new() } else { format!(": {}", b.reason) },
                space.name_of(&b.by.0)
            )
            .into(),
        })
        .collect();
    app.set_space_bans(ModelRc::new(VecModel::from(bans)));
    let names: Vec<SharedString> =
        invitable_contacts(&config).iter().filter(|c| !m.is_member(&connection_mode::decode_hex32(&c.identity_key).unwrap_or_default())).map(|c| c.name.as_str().into()).collect();
    app.set_space_contact_names(ModelRc::new(VecModel::from(names)));
    app.set_space_can_post(joined && m.can(&me, perm::POST, None));
    app.set_space_can_invite(joined && m.can(&me, perm::INVITE, None));
    app.set_space_can_ban(joined && m.can(&me, perm::BAN, None));
    app.set_space_can_manage(joined && m.can(&me, perm::MANAGE_COMMUNITY, None));
    app.set_space_can_manage_channels(joined && m.can(&me, perm::MANAGE_CHANNELS, None));
    app.set_space_server_hosted(m.server_hosted_files);
    app.set_space_ring_rule(store.ring_rule(&space.id()).map_or(0, |r| r.to_i32() + 1));
    let first = |v: &[antiphon_crypto::community::NodeRef]| v.first().cloned().unwrap_or_default();
    let (mailbox, files) = (first(&m.nodes.mailbox), first(&m.nodes.files));
    app.set_space_mailbox_node(mailbox.addr.into());
    app.set_space_mailbox_code(mailbox.code.into());
    app.set_space_files_node(files.addr.into());
    app.set_space_files_code(files.code.into());
    app.set_space_dht_node(m.nodes.dht.first().cloned().unwrap_or_default().into());
}

/// A picture from a card (base64 JPEG), for the member list.
fn avatar_image(b64: &str) -> Option<slint::Image> {
    if b64.is_empty() || b64.len() > community::MAX_AVATAR {
        return None;
    }
    let bytes = file_transfer::decode_b64(b64)?;
    let rgba = image::load_from_memory(&bytes).ok()?.thumbnail(64, 64).to_rgba8();
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height());
    Some(slint::Image::from_rgba8(buffer))
}

/// A picked image as a card picture: a 48 px square JPEG, base64, made
/// smaller until it fits [`community::MAX_AVATAR`].
fn avatar_from_file(path: &std::path::Path) -> Result<String, String> {
    let img = image::open(path).map_err(|e| format!("couldn't read that picture: {e}"))?;
    let square = img.resize_to_fill(48, 48, image::imageops::FilterType::Lanczos3).to_rgb8();
    for quality in [80u8, 65, 50, 35] {
        let mut out = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
        encoder.encode_image(&square).map_err(|e| e.to_string())?;
        let b64 = file_transfer::encode_b64(&out);
        if b64.len() <= community::MAX_AVATAR {
            return Ok(b64);
        }
    }
    Err("that picture won't shrink small enough".into())
}

/// The selected space's "How I appear here" fields, from what's saved.
/// Only on selecting or saving, so a redraw never wipes what's being typed.
fn load_look_to_ui(app: &MainWindow) {
    let (Some(store), Some(id)) = (community::Store::app(), selected_space(app)) else { return };
    let look = store.appearance(&id);
    app.set_space_look_name(look.name.into());
    app.set_space_look_status(look.status.into());
    app.set_space_look_avatar(avatar_image(&look.avatar).unwrap_or_default());
    app.set_space_look_avatar_data(look.avatar.into());
}

fn wire_communities(main_window: &MainWindow, app_state: &Arc<AppState>) {
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_save_look(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let look = community::Appearance {
                name: app.get_space_look_name().trim().to_string(),
                status: app.get_space_look_status().trim().to_string(),
                avatar: app.get_space_look_avatar_data().to_string(),
            };
            space_action(&app, |store, id| store.set_appearance(id, look));
            if app.get_space_status().is_empty() {
                app.set_space_status("Saved. Members see it when they next collect their mail.".into());
            }
            load_look_to_ui(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_clear_look_avatar(move || {
            let Some(app) = window_weak.upgrade() else { return };
            app.set_space_look_avatar_data("".into());
            app.set_space_look_avatar(slint::Image::default());
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_pick_look_avatar(move || {
            let window_weak = window_weak.clone();
            // The dialog blocks, so not on the UI thread.
            std::thread::spawn(move || {
                let Some(path) = rfd::FileDialog::new().set_title("A picture for this space").add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp", "bmp"]).pick_file() else { return };
                let result = avatar_from_file(&path);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(app) = window_weak.upgrade() else { return };
                    match result {
                        Ok(b64) => {
                            app.set_space_look_avatar(avatar_image(&b64).unwrap_or_default());
                            app.set_space_look_avatar_data(b64.into());
                            app.set_space_status("Save to send it.".into());
                        }
                        Err(e) => app.set_space_status(e.into()),
                    }
                });
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_open_communities(move || {
            let Some(app) = window_weak.upgrade() else { return };
            apply_spaces_to_ui(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_refresh_space(move || {
            let Some(app) = window_weak.upgrade() else { return };
            apply_spaces_to_ui(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_select_space(move |id| {
            let Some(app) = window_weak.upgrade() else { return };
            app.set_selected_space(id);
            app.set_space_thread_root("".into());
            app.set_space_channel_index(0);
            app.set_space_status("".into());
            load_look_to_ui(&app);
            apply_spaces_to_ui(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_select_space_channel(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            app.set_space_channel_index(index);
            app.set_space_thread_root("".into());
            apply_spaces_to_ui(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_create_space(move |name, is_community| {
            let Some(app) = window_weak.upgrade() else { return };
            let Some(store) = community::Store::app() else { return };
            let kind = if is_community { antiphon_crypto::community::Kind::Community } else { antiphon_crypto::community::Kind::Group };
            match store.create(kind, &name, app.get_space_new_separate()) {
                Ok(id) => {
                    app.set_space_new_name("".into());
                    app.set_selected_space(delivery::hex(&id.0).into());
                    app.set_space_status("".into());
                }
                Err(e) => app.set_space_status(e.into()),
            }
            apply_spaces_to_ui(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_accept_space_invite(move |index, separate| {
            let Some(app) = window_weak.upgrade() else { return };
            let Some(store) = community::Store::app() else { return };
            match store.accept_invite(index.max(0) as usize, separate) {
                Ok(id) => {
                    app.set_selected_space(delivery::hex(&id.0).into());
                    app.set_space_status("Accepted. You'll be let in when they next collect their mail.".into());
                }
                Err(e) => app.set_space_status(e.into()),
            }
            apply_spaces_to_ui(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_decline_space_invite(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            if let Some(store) = community::Store::app() {
                store.decline_invite(index.max(0) as usize);
            }
            apply_spaces_to_ui(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_send(move |text| {
            let Some(app) = window_weak.upgrade() else { return };
            let channel_index = app.get_space_channel_index().max(0) as usize;
            let reply_to: Option<u64> = app.get_space_thread_root().parse().ok();
            let mut sent = false;
            space_action(&app, |store, id| {
                store.with_space(id, |space, _| {
                    let channel = space.manifest().channels.get(channel_index).map(|c| c.id).unwrap_or(0);
                    let (outs, event) = space.post(channel, &text, reply_to)?;
                    Ok(((), outs, vec![event]))
                })?;
                sent = true;
                Ok(())
            });
            if sent {
                app.set_space_input("".into());
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_edit_message(move |id, text| {
            let Some(app) = window_weak.upgrade() else { return };
            let Ok(id) = id.parse::<u64>() else { return };
            let mut done = false;
            space_action(&app, |store, sid| {
                store.with_space(sid, |space, identity| {
                    let me = identity.public().identity_key;
                    let outs = space.edit(id, &text)?;
                    Ok(((), outs, vec![community::Event::Edited { id, sender: me, text: text.trim().to_string() }]))
                })?;
                done = true;
                Ok(())
            });
            if done {
                app.set_space_editing("".into());
                app.set_space_input("".into());
            }
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_delete_message(move |id, sender| {
            let Some(app) = window_weak.upgrade() else { return };
            let (Ok(id), Some(author)) = (id.parse::<u64>(), connection_mode::decode_hex32(&sender)) else { return };
            space_action(&app, |store, sid| {
                store.with_space(sid, |space, _| Ok(((), space.delete(id, author)?, vec![community::Event::Deleted { id, author }])))
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_invite(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            let config = config::load();
            space_action(&app, |store, id| {
                let space = store.load(id).ok_or("couldn't open it")?;
                let candidates: Vec<config::Contact> = invitable_contacts(&config)
                    .into_iter()
                    .filter(|c| !space.manifest().is_member(&connection_mode::decode_hex32(&c.identity_key).unwrap_or_default()))
                    .collect();
                let contact = candidates.get(index.max(0) as usize).ok_or("pick a contact")?;
                let key = connection_mode::decode_hex32(&contact.identity_key).ok_or("no key for them")?;
                store.with_space(id, |space, identity| Ok(((), space.invite(identity, key, &contact.name)?, vec![community::Event::Changed])))
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_kick(move |key| {
            let Some(app) = window_weak.upgrade() else { return };
            let Some(key) = connection_mode::decode_hex32(&key) else { return };
            space_action(&app, |store, id| store.with_space(id, |space, identity| Ok(((), space.kick(identity, key)?, vec![]))));
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_ban(move |key, reason| {
            let Some(app) = window_weak.upgrade() else { return };
            let Some(key) = connection_mode::decode_hex32(&key) else { return };
            space_action(&app, |store, id| store.with_space(id, |space, identity| Ok(((), space.ban(identity, key, &reason)?, vec![]))));
            app.set_space_ban_reason("".into());
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_unban(move |key| {
            let Some(app) = window_weak.upgrade() else { return };
            let Some(key) = connection_mode::decode_hex32(&key) else { return };
            space_update(&app, |m| m.bans.retain(|b| b.key.0 != key));
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_set_role(move |key, role| {
            let Some(app) = window_weak.upgrade() else { return };
            let Some(key) = connection_mode::decode_hex32(&key) else { return };
            space_update(&app, |m| {
                if let Some(x) = m.members.iter_mut().find(|x| x.key.0 == key) {
                    x.role = role.max(0) as u16;
                }
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_add_channel(move |name| {
            let Some(app) = window_weak.upgrade() else { return };
            let name: String = name.trim().trim_start_matches('#').chars().take(80).collect();
            if name.is_empty() {
                return;
            }
            space_update(&app, |m| {
                let id = m.channels.iter().map(|c| c.id).max().unwrap_or(0) + 1;
                m.channels.push(antiphon_crypto::community::Channel { id, name, topic: String::new() });
            });
            app.set_space_new_channel("".into());
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_remove_channel(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let index = app.get_space_channel_index().max(0) as usize;
            space_update(&app, |m| {
                if m.channels.len() > 1 && index < m.channels.len() {
                    m.channels.remove(index);
                }
            });
            app.set_space_channel_index(0);
            apply_spaces_to_ui(&app);
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_save_settings(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let node = |addr: SharedString, code: SharedString| {
                let addr = addr.trim().to_string();
                (!addr.is_empty()).then(|| antiphon_crypto::community::NodeRef { addr, code: code.trim().to_string() })
            };
            let mailbox = node(app.get_space_mailbox_node(), app.get_space_mailbox_code());
            let files = node(app.get_space_files_node(), app.get_space_files_code());
            let dht = app.get_space_dht_node().trim().to_string();
            let hosted = app.get_space_server_hosted();
            space_update(&app, |m| {
                m.nodes.mailbox = mailbox.into_iter().collect();
                m.nodes.files = files.into_iter().collect();
                m.nodes.dht = if dht.is_empty() { Vec::new() } else { vec![dht] };
                m.server_hosted_files = hosted;
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_set_ring_rule(move |index| {
            let Some(app) = window_weak.upgrade() else { return };
            let rule = (index > 0).then(|| notify::RingRule::from_i32(index - 1));
            space_action(&app, |store, id| {
                store.set_ring_rule(id, rule);
                Ok(())
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_use_nodes(move || {
            let Some(app) = window_weak.upgrade() else { return };
            let Some(m) = community::Store::app().zip(selected_space(&app)).and_then(|(s, id)| s.load(&id)).map(|s| s.manifest().clone()) else { return };
            let mut config = config::load();
            let Some(tier) = config.routes.iter_mut().find(|r| r.kind == delivery::RouteKind::CommunityNode) else { return };
            let Some(node) = m.nodes.mailbox.first() else { return };
            if !tier.code.is_empty() && tier.code != node.code && !tier.nodes.is_empty() {
                app.set_space_status("Your Community node route already uses another community's code; edit it under Call → Delivery routes.".into());
                return;
            }
            if !tier.nodes.contains(&node.addr) {
                tier.nodes.push(node.addr.clone());
            }
            tier.code = node.code.clone();
            tier.enabled = true;
            for d in &m.nodes.dht {
                if !config.dht_bootstrap.contains(d) {
                    config.dht_bootstrap.push(d.clone());
                }
            }
            save_routes(&app, config);
            app.set_space_status(format!("{} now holds your mail too; your contacts and fellow members get your new route list.", node.addr).into());
        });
    }
    {
        let window_weak = main_window.as_weak();
        let runtime = app_state.runtime.clone();
        main_window.on_space_share_file(move || {
            let window_weak = window_weak.clone();
            let runtime = runtime.clone();
            let Some(app) = window_weak.upgrade() else { return };
            let Some(id) = selected_space(&app) else { return };
            let channel_index = app.get_space_channel_index().max(0) as usize;
            // The dialog and the sealing block, so not on the UI thread.
            std::thread::spawn(move || {
                let Some(path) = rfd::FileDialog::new().set_title("Share a file here").pick_file() else { return };
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
                let result = (|| {
                    let store = community::Store::app().ok_or("not ready")?;
                    let manifest = store.load(&id).ok_or("couldn't open it")?.manifest().clone();
                    let (name, size, file_id, key, holders) = sharing::share_in_community(&runtime, &path, &name, &manifest)?;
                    store.with_space(&id, |space, _| {
                        let channel = space.manifest().channels.get(channel_index).map(|c| c.id).unwrap_or(0);
                        let at_secs = now_secs();
                        let mut b = [0u8; 8];
                        getrandom::fill(&mut b).map_err(|e| e.to_string())?;
                        let mid = u64::from_le_bytes(b) >> 1;
                        let post = community::Post::File { id: mid, channel, name: name.clone(), size, file_id: file_id.clone(), key: key.clone(), holders: holders.clone(), at_secs };
                        let outs = space.share_file(post)?;
                        Ok(((), outs, vec![community::Event::File { channel, id: mid, sender: space.me(), name, size, file_id, key, holders, at_secs }]))
                    })
                })();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(app) = window_weak.upgrade() else { return };
                    match result {
                        Ok(()) => app.set_space_status("Shared. It's under Shared files too.".into()),
                        Err(e) => app.set_space_status(SharedString::from(e)),
                    }
                    apply_spaces_to_ui(&app);
                });
            });
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_leave(move || {
            let Some(app) = window_weak.upgrade() else { return };
            space_action(&app, |store, id| store.with_space(id, |space, identity| Ok(((), space.leave(identity)?, vec![]))));
        });
    }
    {
        let window_weak = main_window.as_weak();
        main_window.on_space_forget(move || {
            let Some(app) = window_weak.upgrade() else { return };
            if let (Some(store), Some(id)) = (community::Store::app(), selected_space(&app)) {
                store.forget(&id);
            }
            app.set_selected_space("".into());
            apply_spaces_to_ui(&app);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_tone_controls_round_trip_through_bands() {
        let bands = peer_tone_bands(4.0, -3.0, 6.0);
        assert_eq!(peer_tone_positions(&bands), (4.0, -3.0, 6.0));
    }

    #[test]
    fn flat_peer_tone_controls_run_no_processing_at_all() {
        // Somebody who never touches these sliders must get their audio
        // back untouched -- not passed through three biquads sitting at
        // unity, which costs work and rounds every sample for nothing.
        assert!(peer_eq_chain(&peer_tone_bands(0.0, 0.0, 0.0)).is_none());
    }

    #[test]
    fn a_nudge_too_small_to_hear_does_not_switch_the_eq_on() {
        // Sliders land on fractional values; a 0.2 dB band is not a
        // setting, it is a slip, and it should not start processing.
        assert!(peer_eq_chain(&peer_tone_bands(0.2, -0.1, 0.0)).is_none());
        assert!(peer_eq_chain(&peer_tone_bands(0.0, 0.0, 1.0)).is_some());
    }

    #[test]
    fn an_unrecognised_stored_curve_reads_as_flat() {
        // Bands from a hand-edited config cannot be expressed by three
        // sliders. Showing 0 is honest about what the sliders control;
        // showing someone else's gain values would not be.
        let alien = vec![EqBandConfig {
            enabled: true,
            center_hz: 8000.0,
            q: 1.0,
            gain_db: 9.0,
            band_type: BandType::Bell,
        }];
        assert_eq!(peer_tone_positions(&alien), (0.0, 0.0, 0.0));
    }

    #[test]
    fn a_peer_eq_never_touches_their_level() {
        // The volume trim is a separate control. If the EQ also moved the
        // overall level the two would fight, and the trim would stop
        // meaning what its label says.
        let mut chain = peer_eq_chain(&peer_tone_bands(0.0, 0.0, 8.0))
            .expect("a treble boost is a real setting");
        // Silence in, silence out: no gain stage, no makeup, no offset.
        let mut samples = vec![0.0f32; 480];
        chain.process(&mut samples);
        assert!(samples.iter().all(|s| s.abs() < 1e-6), "silence gained something");
    }

    #[test]
    fn hex_round_trips_through_color() {
        let color = color_from_hex("#2d7d5a").expect("valid hex should parse");
        assert_eq!(color.red(), 0x2d);
        assert_eq!(color.green(), 0x7d);
        assert_eq!(color.blue(), 0x5a);
        assert_eq!(color_to_hex(color), "#2d7d5a");
    }

    #[test]
    fn hex_without_hash_prefix_still_parses() {
        assert!(color_from_hex("2d7d5a").is_some());
    }

    #[test]
    fn malformed_hex_is_rejected_not_guessed_at() {
        assert!(color_from_hex("").is_none());
        assert!(color_from_hex("#fff").is_none(), "3-digit shorthand isn't supported, shouldn't silently misparse");
        assert!(color_from_hex("#gggggg").is_none());
        assert!(color_from_hex("#2d7d5a00").is_none(), "8 digits (with alpha) isn't the format this field expects");
    }

    #[test]
    fn hsv_matches_known_primary_colors() {
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), (255, 0, 0));
        assert_eq!(hsv_to_rgb(120.0, 1.0, 1.0), (0, 255, 0));
        assert_eq!(hsv_to_rgb(240.0, 1.0, 1.0), (0, 0, 255));
        assert_eq!(hsv_to_rgb(0.0, 0.0, 0.0), (0, 0, 0));
        assert_eq!(hsv_to_rgb(0.0, 0.0, 1.0), (255, 255, 255));
    }

    #[test]
    fn rgb_round_trips_through_hsv() {
        for (r, g, b) in [(45u8, 125, 90), (255, 0, 0), (10, 10, 10), (0, 0, 0), (255, 255, 255)] {
            let (h, s, v) = rgb_to_hsv(r, g, b);
            let (r2, g2, b2) = hsv_to_rgb(h, s, v);
            assert!((r as i16 - r2 as i16).abs() <= 1, "r: {r} vs {r2}");
            assert!((g as i16 - g2 as i16).abs() <= 1, "g: {g} vs {g2}");
            assert!((b as i16 - b2 as i16).abs() <= 1, "b: {b} vs {b2}");
        }
    }

    #[test]
    fn move_order_item_swaps_with_neighbor_and_stops_at_the_ends() {
        let mut order = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        move_order_item(&mut order, "b", -1);
        assert_eq!(order, vec!["b", "a", "c"]);
        move_order_item(&mut order, "b", 1);
        assert_eq!(order, vec!["a", "b", "c"]);
        move_order_item(&mut order, "a", -1); // already first: no-op, not a wrap-around
        assert_eq!(order, vec!["a", "b", "c"]);
        move_order_item(&mut order, "c", 1); // already last: no-op
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn snap_to_grid_rounds_to_the_nearest_multiple() {
        assert_eq!(snap_to_grid(0.0), 0.0);
        assert_eq!(snap_to_grid(9.0), 0.0);
        assert_eq!(snap_to_grid(11.0), 20.0);
        assert_eq!(snap_to_grid(29.0), 20.0);
        assert_eq!(snap_to_grid(31.0), 40.0);
    }

    #[test]
    fn clamp_panel_move_snaps_a_clear_move_to_the_grid() {
        let start = PanelGeom { x: 0.0, y: 0.0, w: 100.0, h: 50.0 };
        let (x, y) = clamp_panel_move(start, 41.0, 9.0, 1000.0, 1000.0);
        assert_eq!((x, y), (40.0, 0.0));
    }

    #[test]
    fn clamp_panel_move_stops_at_the_canvas_edge() {
        let start = PanelGeom { x: 0.0, y: 0.0, w: 100.0, h: 50.0 };
        let (x, _) = clamp_panel_move(start, -50.0, 0.0, 1000.0, 1000.0);
        assert_eq!(x, 0.0, "can't be dragged off the left edge of the canvas");
    }

    #[test]
    fn clamp_panel_move_allows_overlapping_another_panel() {
        // The grid is an alignment aid, not a collision system -- panels
        // are free to end up on top of each other (e.g. while rearranging,
        // or deliberately stacking one behind another).
        let start = PanelGeom { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let (x, y) = clamp_panel_move(start, 40.0, 40.0, 1000.0, 1000.0);
        assert_eq!((x, y), (40.0, 40.0));
    }

    #[test]
    fn clamp_panel_resize_snaps_to_the_grid_and_respects_minimum_size() {
        let start = PanelGeom { x: 0.0, y: 0.0, w: MIN_PANEL_WIDTH, h: MIN_PANEL_HEIGHT };
        let (w, h) = clamp_panel_resize(start, 41.0, -1000.0, &[], 1000.0, 1000.0);
        assert_eq!(w, MIN_PANEL_WIDTH + 40.0);
        assert_eq!(h, MIN_PANEL_HEIGHT, "can't shrink below the minimum");
    }

    #[test]
    fn clamp_panel_resize_stops_at_the_canvas_edge() {
        let start = PanelGeom { x: 500.0, y: 0.0, w: MIN_PANEL_WIDTH, h: MIN_PANEL_HEIGHT };
        let (w, _) = clamp_panel_resize(start, 1000.0, 0.0, &[], 1000.0, 1000.0);
        assert_eq!(w, 500.0, "can't grow past the right edge of the canvas");
    }

    #[test]
    fn clamp_panel_resize_yields_to_a_cramped_canvas_edge_below_minimum() {
        // With less than `MIN_PANEL_WIDTH` of room left on the canvas, the
        // actual available space wins rather than the clamp panicking
        // (Rust's `f32::clamp` requires min <= max) or growing past the edge.
        let start = PanelGeom { x: 950.0, y: 0.0, w: MIN_PANEL_WIDTH, h: MIN_PANEL_HEIGHT };
        let (w, _) = clamp_panel_resize(start, 1000.0, 0.0, &[], 1000.0, 1000.0);
        assert_eq!(w, 50.0);
    }

    #[test]
    fn clamp_panel_resize_stops_at_a_neighboring_panel() {
        let start = PanelGeom { x: 0.0, y: 0.0, w: MIN_PANEL_WIDTH, h: MIN_PANEL_HEIGHT };
        let neighbor = PanelGeom { x: 350.0, y: 0.0, w: 300.0, h: MIN_PANEL_HEIGHT };
        let (w, h) = clamp_panel_resize(start, 200.0, 0.0, &[neighbor], 1000.0, 1000.0);
        assert_eq!(w, 350.0, "should stop flush against the neighbor's left edge, not tunnel through");
        assert_eq!(h, MIN_PANEL_HEIGHT);
    }

    #[test]
    fn resolve_drop_position_returns_the_same_spot_when_nothing_blocks_it() {
        let dropped = PanelGeom { x: 40.0, y: 40.0, w: 100.0, h: 100.0 };
        let (x, y) = resolve_drop_position(dropped, &[], 1000.0, 1000.0);
        assert_eq!((x, y), (40.0, 40.0));
    }

    #[test]
    fn resolve_drop_position_finds_a_nearby_non_overlapping_slot() {
        // Dropped directly on top of an identically-sized obstacle -- the
        // worst case for "how far does it have to search".
        let dropped = PanelGeom { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let obstacle = PanelGeom { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let (x, y) = resolve_drop_position(dropped, &[obstacle], 1000.0, 1000.0);
        let result = PanelGeom { x, y, w: dropped.w, h: dropped.h };
        assert!(!result.overlaps(&obstacle), "result {result:?} still overlaps {obstacle:?}");
        let dist = ((x - dropped.x).powi(2) + (y - dropped.y).powi(2)).sqrt();
        assert!(dist <= 200.0, "resolved position {result:?} is farther from the drop point than expected: {dist}px");
    }

    #[test]
    fn resolve_drop_position_never_returns_an_overlapping_spot_when_room_exists() {
        let dropped = PanelGeom { x: 100.0, y: 100.0, w: 60.0, h: 60.0 };
        let obstacle = PanelGeom { x: 90.0, y: 90.0, w: 80.0, h: 80.0 };
        let (x, y) = resolve_drop_position(dropped, &[obstacle], 1000.0, 1000.0);
        let result = PanelGeom { x, y, w: dropped.w, h: dropped.h };
        assert!(!result.overlaps(&obstacle));
    }

    #[test]
    fn resolve_drop_position_ignores_panels_that_dont_actually_overlap() {
        let dropped = PanelGeom { x: 200.0, y: 200.0, w: 100.0, h: 100.0 };
        let far_away = PanelGeom { x: 800.0, y: 800.0, w: 100.0, h: 100.0 };
        let (x, y) = resolve_drop_position(dropped, &[far_away], 1000.0, 1000.0);
        assert_eq!((x, y), (200.0, 200.0));
    }
}
