//! Spec §11 "Bots / API, local-first": a local API for your own scripts
//! and bots, served by the app on this computer and nowhere else.
//!
//! **Off by default.** Settings > Advanced > *Local API* turns it on (or
//! `--api`). The app then listens on `127.0.0.1` only, on a port chosen by
//! the system, and writes the port and a fresh random token to `api.json`
//! in its config folder, readable by this user alone. A script reads that
//! file, connects, and must send the token first; without it the
//! connection is closed. Nothing is reachable from the network, and
//! nothing goes through any server: a bot is a program on your machine
//! acting as you (or as a separate identity, run with
//! `--profile <name> --headless`).
//!
//! **Protocol.** One JSON object per line, both ways.
//!
//! ```text
//! -> {"token":"..."}                               first line, always
//! <- {"ok":true,"result":{"api":1}}
//! -> {"id":1,"cmd":"contacts"}
//! <- {"id":1,"ok":true,"result":[{"id":"…","name":"Sam","online":true,…}]}
//! -> {"id":2,"cmd":"send","to":"…","text":"hello"}
//! <- {"id":2,"ok":true,"result":null}
//! -> {"id":3,"cmd":"subscribe"}
//! <- {"id":3,"ok":true,"result":null}
//! <- {"event":"message","from":"…","name":"Sam","text":"hi back"}
//! ```
//!
//! Commands are answered by the engine ([`crate::engine::Engine`]), which
//! does exactly what the same button would: `me`, `contacts`,
//! `messages {with, limit}`, `send {to, text}`, `call {to}`, `hangup`,
//! `mute {on}`, `status`, `door {open}`, `subscribe`. Errors come back as
//! `{"id":…,"ok":false,"error":"…"}`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

/// The protocol's version, in the greeting.
pub const VERSION: u32 = 1;

/// The longest line a client may send, so a stuck or hostile script
/// can't make the app buffer without end.
const MAX_LINE: usize = 64 * 1024;

/// One command from a client, for the engine to answer.
pub struct Request {
    /// The command object as sent.
    pub body: Value,
    /// Where the answer goes.
    pub reply: oneshot::Sender<Result<Value, String>>,
    /// The client's event stream, for `subscribe`.
    pub events: mpsc::UnboundedSender<Value>,
}

/// The running server: the engine drains [`Self::requests`] each tick.
pub struct Api {
    pub requests: mpsc::UnboundedReceiver<Request>,
    /// Where the port and token were written; removed when the API stops.
    info: PathBuf,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for Api {
    fn drop(&mut self) {
        self.accept.abort();
        let _ = std::fs::remove_file(&self.info);
    }
}

/// A random token, as hex.
fn new_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS randomness is unavailable");
    antiphon_crypto::hex::encode(&bytes)
}

/// Writes `api.json` so only this user can read it.
fn write_info(path: &Path, port: u16, token: &str) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(&json!({ "host": "127.0.0.1", "port": port, "token": token, "api": VERSION }))?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        file.write_all(text.as_bytes())
    }
    #[cfg(not(unix))]
    {
        // The config folder is already private to the user on Windows.
        std::fs::write(path, text)
    }
}

/// Starts listening on a free local port, writing its details into
/// `config_dir/api.json`; connections are served on `runtime`.
pub fn start(config_dir: &Path, runtime: &tokio::runtime::Handle) -> std::io::Result<Api> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let token = new_token();
    let info = config_dir.join("api.json");
    write_info(&info, port, &token)?;
    let (tx, requests) = mpsc::unbounded_channel();
    let accept = runtime.spawn(async move {
        let Ok(listener) = TcpListener::from_std(listener) else { return };
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(serve(stream, token.clone(), tx.clone()));
        }
    });
    Ok(Api { requests, info, accept })
}

/// Reads one line, refusing one longer than [`MAX_LINE`].
async fn read_line(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Option<String> {
    let mut line = String::new();
    let mut limited = (&mut *reader).take(MAX_LINE as u64);
    use tokio::io::AsyncReadExt;
    match limited.read_line(&mut line).await {
        Ok(0) | Err(_) => None,
        Ok(_) if !line.ends_with('\n') => None,
        Ok(_) => Some(line),
    }
}

/// Compares in constant time, so the token can't be guessed byte by byte
/// from how long a refusal takes.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// One client: the token, then commands until it hangs up.
async fn serve(stream: TcpStream, token: String, engine: mpsc::UnboundedSender<Request>) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let authorised = match read_line(&mut reader).await.and_then(|l| serde_json::from_str::<Value>(&l).ok()) {
        Some(hello) => hello.get("token").and_then(Value::as_str).is_some_and(|t| same(t, &token)),
        None => false,
    };
    if !authorised {
        let _ = write.write_all(b"{\"ok\":false,\"error\":\"bad token\"}\n").await;
        return;
    }
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<Value>();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    // One writer, so replies and events never interleave mid-line.
    let writer = tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                Some(v) = out_rx.recv() => v,
                Some(v) = events_rx.recv() => v,
                else => break,
            };
            let mut line = next.to_string();
            line.push('\n');
            if write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let _ = out_tx.send(json!({ "ok": true, "result": { "api": VERSION } }));
    while let Some(line) = read_line(&mut reader).await {
        let body: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = out_tx.send(json!({ "ok": false, "error": format!("not JSON: {e}") }));
                continue;
            }
        };
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        let (reply, answer) = oneshot::channel();
        if engine.send(Request { body, reply, events: events_tx.clone() }).is_err() {
            break;
        }
        let out = match answer.await {
            Ok(Ok(result)) => json!({ "id": id, "ok": true, "result": result }),
            Ok(Err(error)) => json!({ "id": id, "ok": false, "error": error }),
            Err(_) => break,
        };
        if out_tx.send(out).is_err() {
            break;
        }
    }
    drop(out_tx);
    drop(events_tx);
    let _ = writer.await;
}

/// The command name and a string argument, for the engine.
pub fn arg<'a>(body: &'a Value, name: &str) -> Result<&'a str, String> {
    body.get(name).and_then(Value::as_str).ok_or_else(|| format!("'{name}' is missing"))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn connect(dir: &Path) -> (BufReader<tokio::net::tcp::OwnedReadHalf>, tokio::net::tcp::OwnedWriteHalf, String) {
        let info: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("api.json")).unwrap()).unwrap();
        let stream = TcpStream::connect(("127.0.0.1", info["port"].as_u64().unwrap() as u16)).await.unwrap();
        let (r, w) = stream.into_split();
        (BufReader::new(r), w, info["token"].as_str().unwrap().to_string())
    }

    async fn line(r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Value {
        serde_json::from_str(&read_line(r).await.expect("a line")).unwrap()
    }

    #[tokio::test]
    async fn a_client_needs_the_token_and_gets_answers_and_events() {
        let dir = std::env::temp_dir().join(format!("antiphon-api-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut api = start(&dir, &tokio::runtime::Handle::current()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join("api.json")).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // A wrong token is refused and the connection closed.
        let (mut r, mut w, _) = connect(&dir).await;
        w.write_all(b"{\"token\":\"nope\"}\n").await.unwrap();
        assert_eq!(line(&mut r).await["ok"], false);
        assert!(read_line(&mut r).await.is_none());

        // The right one: a greeting, then a command answered by "the
        // engine" (this test), then an event on the same connection.
        let (mut r, mut w, token) = connect(&dir).await;
        w.write_all(format!("{{\"token\":\"{token}\"}}\n").as_bytes()).await.unwrap();
        assert_eq!(line(&mut r).await["result"]["api"], VERSION);
        w.write_all(b"{\"id\":7,\"cmd\":\"echo\",\"text\":\"hi\"}\n").await.unwrap();
        let req = api.requests.recv().await.unwrap();
        assert_eq!(arg(&req.body, "cmd"), Ok("echo"));
        assert_eq!(arg(&req.body, "nope"), Err("'nope' is missing".to_string()));
        req.events.send(json!({ "event": "message", "text": "later" })).unwrap();
        req.reply.send(Ok(json!(arg(&req.body, "text").unwrap()))).unwrap();
        let first = line(&mut r).await;
        let second = line(&mut r).await;
        let (reply, event) = if first.get("id").is_some() { (first, second) } else { (second, first) };
        assert_eq!(reply, json!({ "id": 7, "ok": true, "result": "hi" }));
        assert_eq!(event["text"], "later");

        // Garbage is answered, not fatal.
        w.write_all(b"{oops\n").await.unwrap();
        assert_eq!(line(&mut r).await["ok"], false);

        drop(api);
        assert!(!dir.join("api.json").exists(), "the details go when the API stops");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tokens_are_long_and_compared_exactly() {
        let a = new_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, new_token());
        assert!(same(&a, &a.clone()));
        assert!(!same(&a, &a[..63]));
    }
}
