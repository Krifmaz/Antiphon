mod interactive;
mod ptt;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use device_query::{DeviceQuery, DeviceState};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use antiphon_audio::{list_input_devices, list_output_devices, start_capture, start_playback, AudioChain, ChainConfig};
use antiphon_crypto::{verification_code, IdentityKeyPair};
use antiphon_net::{AppSocket, JitterBuffer, PeerConnection, PopResult};

/// Loads this machine's long-term identity, generating one on first run
/// (spec §4). Shared by every call path so both the CLI and the
/// interactive menu speak as the same person.
pub(crate) fn load_identity() -> Result<IdentityKeyPair> {
    let dir = dirs_config_dir().context("could not locate a config directory for the identity key")?;
    let path = antiphon_crypto::store::identity_path(&dir);
    antiphon_crypto::store::load_or_create(&path)
        .with_context(|| format!("could not load the identity key at {}", path.display()))
}

#[derive(Subcommand)]
enum IdentityAction {
    /// Print this profile's identity key and an invite link.
    Show,
    /// Print the 24-word recovery phrase for this profile.
    ShowPhrase,
    /// Restore an identity from its 24-word recovery phrase.
    Restore {
        /// The 24 words, quoted.
        phrase: String,
        /// Replace an existing, different identity.
        #[arg(long)]
        force: bool,
    },
    /// Split the recovery phrase into shares for trusted friends or
    /// devices; any `--need` of them rebuild it, fewer reveal nothing.
    Split {
        #[arg(long, default_value_t = 2)]
        need: u8,
        #[arg(long, default_value_t = 3)]
        of: u8,
    },
    /// Restore an identity from recovery shares, each one quoted.
    RestoreShares {
        shares: Vec<String>,
        /// Replace an existing, different identity.
        #[arg(long)]
        force: bool,
    },
}

/// Saves a restored identity, refusing to silently replace a different one.
fn restore_identity(seed: [u8; 32], force: bool) -> Result<()> {
    let restored = antiphon_crypto::IdentityKeyPair::from_master_seed(seed);

    let dir = dirs_config_dir().ok_or_else(|| anyhow::anyhow!("no config directory"))?;
    let path = antiphon_crypto::store::identity_path(&dir);

    // Overwriting an identity makes you a different person to
    // every contact who pinned the old key, so it takes an
    // explicit --force rather than happening because someone
    // pasted a phrase to see what it did.
    if path.exists() && !force {
        let existing = antiphon_crypto::store::load_or_create(&path)?;
        if existing.public().identity_key == restored.public().identity_key {
            println!("That already matches this profile's identity. Nothing to do.");
            return Ok(());
        }
        anyhow::bail!(
            "{} already holds a different identity ({}).
             Restoring would replace it, and every contact who pinned the old key
             would see a key-change warning. Re-run with --force if that is what you want,
             or use --profile <name> to restore into a separate profile instead.",
            path.display(),
            hex(&existing.public().identity_key)
        );
    }

    antiphon_crypto::store::save(&path, &restored)?;
    println!("Restored identity {}", hex(&restored.public().identity_key));
    println!("Saved to {}", path.display());
    Ok(())
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The active profile for this process, or `None` for the default. Set
/// once from `--profile` before any identity access.
static PROFILE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

pub(crate) fn set_profile(requested: Option<&str>) {
    let cleaned = requested.and_then(|name| {
        // Becomes a directory name, so strip anything that could climb
        // out of the profiles directory rather than trusting it.
        let cleaned: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if cleaned.is_empty() { None } else { Some(cleaned) }
    });
    let _ = PROFILE.set(cleaned);
}

/// `dirs` isn't a dependency of this crate, and pulling it in for one call
/// isn't worth it -- the platform config directory is two environment
/// variables.
///
/// The default profile keeps the original path exactly, matching the GUI,
/// so both binaries find the same identity when no profile is named.
fn dirs_config_dir() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(std::path::PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")));
    let mut dir = antiphon_crypto::store::app_data_dir(&base?);
    if let Some(Some(name)) = PROFILE.get() {
        dir.push("profiles");
        dir.push(name);
    }
    Some(dir)
}

/// Prints the code both sides compare out of band (spec §4
/// "Verification code").
pub(crate) fn announce_peer(identity: &IdentityKeyPair, peer: &antiphon_crypto::PublicIdentity) {
    println!(
        "Verification code (compare this with them out loud): {}",
        verification_code(&identity.public().identity_key, &peer.identity_key)
    );
}

/// How long the jitter buffer waits for a specific missing frame before
/// giving up and concealing it (spec §2 "Adaptive jitter buffer"). Fixed
/// rather than actually adaptive -- see `antiphon_net::JitterBuffer`'s doc comment.
const JITTER_MAX_WAIT: Duration = Duration::from_millis(60);

/// A minimal two-party encrypted voice call: the vertical slice of the full
/// spec (direct P2P only, no relay/DHT/groups) that proves the core loop --
/// handshake, per-message AEAD encryption, the mic processing chain, and
/// push-to-talk -- actually works end to end.
/// Key names [`ptt::parse_key`] accepts, surfaced as menu choices in
/// interactive mode instead of making users guess a string.
pub(crate) const PTT_KEY_CHOICES: &[&str] = &["space", "capslock", "tab", "ctrl", "rctrl", "shift", "rshift", "alt", "ralt", "grave"];

#[derive(Parser)]
#[command(name = "antiphon-cli")]
struct Cli {
    /// Use a separate identity and settings directory, so one machine can
    /// hold more than one identity (e.g. to call yourself while testing).
    /// Not an account -- there is no server and nothing to log into.
    #[arg(long, global = true)]
    profile: Option<String>,

    /// Omit entirely to get an interactive menu instead of flags.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show or restore this profile's identity (spec §3 "Backup & recovery").
    Identity {
        #[command(subcommand)]
        action: IdentityAction,
    },
    /// Wait for an incoming call.
    Listen {
        #[arg(long, default_value_t = 5000)]
        port: u16,
        #[arg(long)]
        preset: Option<String>,
        #[arg(long, default_value = "space")]
        ptt_key: String,
        /// Disable push-to-talk; the mic is always live (still processed through the chain).
        #[arg(long)]
        no_ptt: bool,
        /// Microphone name from `antiphon-cli devices`; omit for the system default.
        #[arg(long)]
        input_device: Option<String>,
        /// Output device name from `antiphon-cli devices`; omit for the system default.
        #[arg(long)]
        output_device: Option<String>,
    },
    /// Call a peer directly at `ip:port`.
    Call {
        #[arg(long)]
        remote: SocketAddr,
        #[arg(long, default_value_t = 0)]
        port: u16,
        #[arg(long)]
        preset: Option<String>,
        #[arg(long, default_value = "space")]
        ptt_key: String,
        #[arg(long)]
        no_ptt: bool,
        #[arg(long)]
        input_device: Option<String>,
        #[arg(long)]
        output_device: Option<String>,
    },
    /// List available audio input/output devices.
    Devices,
    /// Print (or save) the default audio chain preset as JSON, so it can be
    /// edited and passed back in via `--preset` (spec §1 "Voice Presets").
    Preset {
        #[arg(long)]
        out: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Before any identity access, since it decides which directory the
    // key is read from.
    set_profile(cli.profile.as_deref());
    let Some(command) = cli.command else {
        return interactive::run().await;
    };
    match command {
        Command::Identity { action } => match action {
            IdentityAction::Show => {
                let identity = load_identity()?;
                println!("Identity: {}", hex(&identity.public().identity_key));
                println!("Invite:   {}", antiphon_crypto::Invite::new(identity.public().identity_key, "").to_link());
            }
            IdentityAction::ShowPhrase => {
                let identity = load_identity()?;
                println!("Write these 24 words down and keep them somewhere safe.");
                println!("Anyone who reads them becomes you, and there is no server that");
                println!("can reset your identity if you lose them.
");
                println!("{}", antiphon_crypto::mnemonic::phrase_from_seed(&identity.master_seed()));
            }
            IdentityAction::Restore { phrase, force } => {
                let seed = antiphon_crypto::mnemonic::seed_from_phrase(&phrase)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                restore_identity(seed, force)?;
            }
            IdentityAction::Split { need, of } => {
                let identity = load_identity()?;
                let shares = antiphon_crypto::shamir::split(&identity.master_seed(), need, of)
                    .map_err(|_| anyhow::anyhow!("--need must be at least 2 and no more than --of"))?;
                println!("Give each share to a different person or device. Any {need} of these {of}");
                println!("rebuild your identity; fewer reveal nothing about it.\n");
                for share in shares {
                    println!("{}\n", share.to_text());
                }
            }
            IdentityAction::RestoreShares { shares, force } => {
                let parsed = shares
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        antiphon_crypto::shamir::Share::parse(s)
                            .map_err(|e| anyhow::anyhow!("share {}: {e}", i + 1))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let seed = antiphon_crypto::shamir::combine(&parsed).map_err(|e| anyhow::anyhow!("{e}"))?;
                restore_identity(seed, force)?;
            }
        },
        Command::Devices => {
            println!("Input devices:");
            for d in list_input_devices() {
                println!("  {d}");
            }
            println!("Output devices:");
            for d in list_output_devices() {
                println!("  {d}");
            }
        }
        Command::Preset { out } => {
            let config = ChainConfig::default_preset();
            let json = serde_json::to_string_pretty(&config)?;
            match out {
                Some(path) => {
                    std::fs::write(&path, &json).with_context(|| format!("writing preset to {path}"))?;
                    println!("Wrote default preset to {path}");
                }
                None => println!("{json}"),
            }
        }
        Command::Listen { port, preset, ptt_key, no_ptt, input_device, output_device } => {
            let socket = Arc::new(AppSocket::from(UdpSocket::bind(("0.0.0.0", port)).await?));
            println!("Listening on {}", socket.local_addr()?);
            let identity = load_identity()?;
            let peer = PeerConnection::listen(socket, &identity).await?;
            println!("Connected to {}", peer.remote_addr());
            announce_peer(&identity, &peer.peer_identity());
            run_call(peer, preset, &ptt_key, no_ptt, input_device, output_device).await?;
        }
        Command::Call { remote, port, preset, ptt_key, no_ptt, input_device, output_device } => {
            let socket = Arc::new(AppSocket::from(UdpSocket::bind(("0.0.0.0", port)).await?));
            println!("Calling {remote} from {}...", socket.local_addr()?);
            let identity = load_identity()?;
            let peer = PeerConnection::connect(socket, remote, &identity).await?;
            println!("Connected to {}", peer.remote_addr());
            announce_peer(&identity, &peer.peer_identity());
            run_call(peer, preset, &ptt_key, no_ptt, input_device, output_device).await?;
        }
    }
    Ok(())
}

pub(crate) fn load_config(preset: Option<String>) -> Result<ChainConfig> {
    match preset {
        Some(path) => {
            let text = std::fs::read_to_string(&path).with_context(|| format!("reading preset {path}"))?;
            serde_json::from_str(&text).with_context(|| format!("parsing preset {path}"))
        }
        None => Ok(ChainConfig::default_preset()),
    }
}

pub(crate) async fn run_call(
    peer: PeerConnection,
    preset: Option<String>,
    ptt_key: &str,
    no_ptt: bool,
    input_device: Option<String>,
    output_device: Option<String>,
) -> Result<()> {
    let peer = Arc::new(peer);
    let config = load_config(preset)?;

    // Push-to-talk gate: a background thread polls physical key state and
    // flips this flag. `no_ptt` pins it permanently open (spec's "voice
    // activation" mode would flip it based on mic level instead -- not yet
    // implemented here, see README).
    let ptt_active = Arc::new(AtomicBool::new(no_ptt));
    if no_ptt {
        println!("Push-to-talk disabled -- mic is always live.");
    } else {
        let key = ptt::parse_key(ptt_key)?;
        println!("Push-to-talk: hold '{ptt_key}' to transmit.");
        let flag = ptt_active.clone();
        std::thread::spawn(move || {
            let device_state = DeviceState::new();
            loop {
                let pressed = device_state.get_keys().contains(&key);
                flag.store(pressed, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<f32>>();
    let capture = start_capture(input_device.as_deref(), move |samples: &[f32]| {
        let _ = tx.send(samples.to_vec());
    })?;
    let sample_rate = capture.sample_rate;
    let mut chain = AudioChain::from_config(sample_rate, &config);
    println!("Mic chain @ {sample_rate} Hz: {:?}", chain.stage_names());

    let send_peer = peer.clone();
    let send_ptt = ptt_active.clone();
    let send_task = tokio::spawn(async move {
        // The last frame transmitted, carried as a backup copy on the next
        // one (spec section 2 forward error correction).
        let mut previous_frame: Option<Vec<f32>> = None;
        // Application frame sequence, independent of the crypto counter --
        // see `antiphon_net::fec` for why they must not be the same number.
        let mut sequence: u32 = 0;
        while let Some(mut samples) = rx.recv().await {
            chain.process(&mut samples);
            if send_ptt.load(Ordering::Relaxed) {
                let bytes = antiphon_net::fec::encode(sequence, &samples, previous_frame.as_deref());
                previous_frame = Some(samples.clone());
                sequence = sequence.wrapping_add(1);
                if let Err(e) = send_peer.send_audio(&bytes).await {
                    eprintln!("[net] send error: {e}");
                }
            } else {
                previous_frame = None;
            }
        }
    });

    let (playback_handle, mut sink) = start_playback(output_device.as_deref(), 0.5)?;

    // Frames land here in *arrival* order (UDP doesn't guarantee delivery
    // order); the playback task below drains them in *sequence* order,
    // reordering short-range jitter and concealing gaps that never fill in
    // (spec §2 "Adaptive jitter buffer" + "Replay").
    let jitter = Arc::new(Mutex::new(JitterBuffer::new(JITTER_MAX_WAIT)));

    let recv_peer = peer.clone();
    let recv_jitter = jitter.clone();
    let recv_task = tokio::spawn(async move {
        let result = recv_peer
            .recv_loop(move |msg| match msg {
                antiphon_net::Received::Audio { counter: _, plaintext } => {
                    if let Some(decoded) = antiphon_net::fec::decode(&plaintext) {
                        recv_jitter.lock().unwrap().push_with_backup(
                            decoded.sequence as u64,
                            decoded.primary,
                            decoded.backup,
                        );
                    }
                }
                // The CLI has no profile to show and no file transfer UI;
                // a peer's control messages (e.g. the GUI's display
                // name/status, file-transfer offers) and file chunks are
                // just dropped.
                antiphon_net::Received::Control(_) => {}
                antiphon_net::Received::File(_) => {}
            })
            .await;
        if let Err(e) = result {
            eprintln!("[net] receive loop ended: {e}");
        }
    });

    let playback_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        loop {
            tick.tick().await;
            loop {
                let popped = jitter.lock().unwrap().pop_ready();
                match popped {
                    PopResult::Frame(samples)
                    | PopResult::Recovered(samples)
                    | PopResult::Concealed(samples) => sink.push(&samples),
                    PopResult::Empty => break,
                }
            }
        }
    });

    println!("Call active. Press Ctrl+C to hang up.");
    tokio::signal::ctrl_c().await?;
    println!("Hanging up.");

    send_task.abort();
    recv_task.abort();
    playback_task.abort();
    drop(capture);
    drop(playback_handle);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every choice offered by the interactive menu's PTT key picker must
    /// actually be accepted by `ptt::parse_key` -- otherwise picking it from
    /// the menu would fail where typing the equivalent `--ptt-key` flag
    /// would have worked, which is exactly backwards for a menu meant to be
    /// easier than the flags.
    #[test]
    fn every_ptt_key_choice_parses() {
        for choice in PTT_KEY_CHOICES {
            assert!(ptt::parse_key(choice).is_ok(), "PTT_KEY_CHOICES entry '{choice}' doesn't parse");
        }
    }
}
