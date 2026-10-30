//! A bot in a few lines, against the local API (spec §11 "Bots / API,
//! local-first"): it answers every message with the same words back.
//!
//! Run the app as the bot, with an identity of its own, then this:
//!
//! ```text
//! antiphon --profile echo --headless
//! cargo run -p antiphon --example echo_bot -- ~/.config/antiphon/profiles/echo/api.json
//! ```
//!
//! (The path is printed by `--headless`; it differs per system.) Add the
//! bot as a contact from your own app with its invite (`{"cmd":"me"}`
//! gives it) and message it.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

use serde_json::{json, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("give the path to api.json")?;
    let info: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let stream = TcpStream::connect(("127.0.0.1", info["port"].as_u64().ok_or("no port")? as u16))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let mut send = |v: Value| writeln!(writer, "{v}");

    send(json!({ "token": info["token"] }))?;
    send(json!({ "id": 1, "cmd": "me" }))?;
    send(json!({ "id": 2, "cmd": "subscribe" }))?;

    let mut line = String::new();
    while reader.read_line(&mut line)? > 0 {
        let msg: Value = serde_json::from_str(&line)?;
        line.clear();
        if msg["id"] == 1 {
            println!("I am {} -- invite: {}", msg["result"]["name"], msg["result"]["invite"]);
        }
        if msg["event"] == "message" {
            println!("{}: {}", msg["name"], msg["text"]);
            let reply = format!("You said: {}", msg["text"].as_str().unwrap_or_default());
            send(json!({ "id": 3, "cmd": "send", "to": msg["from"], "text": reply }))?;
        }
        if msg["ok"] == false {
            eprintln!("error: {}", msg["error"]);
        }
    }
    Ok(())
}
