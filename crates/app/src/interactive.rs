//! The no-flags-required front door: an arrow-key menu that walks a user
//! through listening for or placing a call without them ever typing a
//! command. Triggered automatically when `antiphon-cli` is run with no
//! subcommand -- the CLI equivalent of the GUI's Call tab, for people who'd
//! rather stay in a terminal (or don't have a desktop session at all).
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use inquire::{Confirm, Select, Text};
use tokio::net::UdpSocket;

use antiphon_audio::{list_input_devices, list_output_devices, ChainConfig};
use antiphon_net::{AppSocket, PeerConnection};

use crate::{run_call, PTT_KEY_CHOICES};

const MAIN_MENU: &[&str] =
    &["Wait for a call (listen)", "Call someone", "List audio devices", "Save a settings preset", "Exit"];

pub async fn run() -> Result<()> {
    println!("Stoop -- interactive mode (run with a subcommand, e.g. `antiphon-cli listen --help`, to skip this menu)\n");

    loop {
        let choice = Select::new("What would you like to do?", MAIN_MENU.to_vec()).prompt()?;
        match choice {
            "Wait for a call (listen)" => {
                if let Err(e) = do_listen().await {
                    println!("Error: {e:?}\n");
                }
            }
            "Call someone" => {
                if let Err(e) = do_call().await {
                    println!("Error: {e:?}\n");
                }
            }
            "List audio devices" => {
                println!("Input devices:");
                for d in list_input_devices() {
                    println!("  {d}");
                }
                println!("Output devices:");
                for d in list_output_devices() {
                    println!("  {d}");
                }
                println!();
            }
            "Save a settings preset" => {
                let path = Text::new("Save preset to:").with_default("preset.json").prompt()?;
                let json = serde_json::to_string_pretty(&ChainConfig::default_preset())?;
                std::fs::write(&path, json)?;
                println!("Wrote default preset to {path}\n");
            }
            _ => return Ok(()),
        }
    }
}

async fn do_listen() -> Result<()> {
    let port: u16 = Text::new("Port to listen on:").with_default("5000").prompt()?.parse()?;
    let (ptt_key, no_ptt) = ask_ptt()?;
    let preset = ask_preset()?;
    let (input_device, output_device) = ask_devices()?;

    let socket = Arc::new(AppSocket::from(UdpSocket::bind(("0.0.0.0", port)).await?));
    println!("Listening on {}", socket.local_addr()?);
    let identity = crate::load_identity()?;
    let peer = PeerConnection::listen(socket, &identity).await?;
    println!("Connected to {}", peer.remote_addr());
    crate::announce_peer(&identity, &peer.peer_identity());
    run_call(peer, preset, &ptt_key, no_ptt, input_device, output_device).await
}

async fn do_call() -> Result<()> {
    let remote: SocketAddr = Text::new("Address to call (ip:port):")
        .with_validator(|s: &str| {
            if s.parse::<SocketAddr>().is_ok() {
                Ok(inquire::validator::Validation::Valid)
            } else {
                Ok(inquire::validator::Validation::Invalid("not a valid ip:port, e.g. 127.0.0.1:6001".into()))
            }
        })
        .prompt()?
        .parse()?;
    let port: u16 = Text::new("Local port (0 for any free port):").with_default("0").prompt()?.parse()?;
    let (ptt_key, no_ptt) = ask_ptt()?;
    let preset = ask_preset()?;
    let (input_device, output_device) = ask_devices()?;

    let socket = Arc::new(AppSocket::from(UdpSocket::bind(("0.0.0.0", port)).await?));
    println!("Calling {remote} from {}...", socket.local_addr()?);
    let identity = crate::load_identity()?;
    let peer = PeerConnection::connect(socket, remote, &identity).await?;
    println!("Connected to {}", peer.remote_addr());
    crate::announce_peer(&identity, &peer.peer_identity());
    run_call(peer, preset, &ptt_key, no_ptt, input_device, output_device).await
}

const SYSTEM_DEFAULT: &str = "System default";

/// Prompts for a mic and output device, defaulting both to the system
/// default without asking further if there's nothing else to pick from --
/// matches the GUI's device picker (spec §1 "Device Hot-Swap").
fn ask_devices() -> Result<(Option<String>, Option<String>)> {
    let pick = |kind: &str, devices: Vec<String>| -> Result<Option<String>> {
        if devices.is_empty() {
            return Ok(None);
        }
        let mut options = vec![SYSTEM_DEFAULT.to_string()];
        options.extend(devices);
        let choice = Select::new(&format!("{kind} device:"), options).prompt()?;
        Ok(if choice == SYSTEM_DEFAULT { None } else { Some(choice) })
    };
    Ok((pick("Microphone", list_input_devices())?, pick("Output", list_output_devices())?))
}

fn ask_ptt() -> Result<(String, bool)> {
    if !Confirm::new("Use push-to-talk?").with_default(true).prompt()? {
        return Ok(("space".to_string(), true));
    }
    let mut options: Vec<&str> = PTT_KEY_CHOICES.to_vec();
    options.push("Custom...");
    let choice = Select::new("Push-to-talk key:", options).prompt()?;
    let key = if choice == "Custom..." { Text::new("Key name:").prompt()? } else { choice.to_string() };
    Ok((key, false))
}

fn ask_preset() -> Result<Option<String>> {
    if !Confirm::new("Load a custom audio preset file?").with_default(false).prompt()? {
        return Ok(None);
    }
    Ok(Some(Text::new("Preset file path:").prompt()?))
}
