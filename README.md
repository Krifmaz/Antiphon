# Antiphon

A direct, end-to-end encrypted voice app in Rust: two peers, no server in
the call path, a real multi-stage microphone processing chain, encrypted
text chat and file transfer over the same connection, and a native Slint
desktop UI with a CLI alongside it.

It is a working vertical slice of the [feature spec](docs/voip-app-feature-spec.pdf),
not the whole of it. The spec describes a multi-year platform -- relays,
DHT discovery, group calls over MLS, mobile clients, VPN integration. What
is built is the part that needs no infrastructure beyond two machines that
can reach each other, built so the rest layers on without a rewrite.
**[`FEATURES.md`](FEATURES.md) is the authoritative, line-by-line status
of every item in the spec**; this file is the overview.

## What's here

A Cargo workspace, one crate per concern:

| Crate | Spec section | What it does |
|---|---|---|
| [`crates/crypto`](crates/crypto) | §3 Connection & Trust | Persistent Ed25519 identity + signed X25519 prekey, authenticated triple-DH handshake, symmetric KDF ratchet with ChaCha20-Poly1305 (fresh key per message, reorder-tolerant), trust-on-first-use key pinning, 40-digit verification codes, 24-word recovery phrase, cross-signed device list, OS keystore sealing (DPAPI on Windows) |
| [`crates/protocol`](crates/protocol) | §3 | Binary wire framing binding encrypted sessions to UDP datagrams (`Handshake` / `Audio` / `Control` / `File`, each with its own AAD) |
| [`crates/net`](crates/net) | §2, §3, §5, §6 | UDP peer connection, STUN discovery + hole punching, NAT-PMP port mapping with renewal, adaptive jitter buffer, forward error correction, audio retransmit requests, file chunking with per-chunk hashes |
| [`crates/audio`](crates/audio) | §1 Audio Suite | The mic chain -- noise suppression, gate, high/low-pass, de-esser, parametric EQ (bell/shelf/notch), compressor, limiter, gain/auto-gain, pitch shift with formant preservation, reverb -- plus an NLMS echo canceller with delay estimation and double-talk detection, SOLA catch-up playback, and `cpal` device I/O |
| [`crates/app`](crates/app) | §1 Utility | `voip-cli`: flag-driven, or an arrow-key interactive menu with no flags to memorise |
| [`crates/gui`](crates/gui) | §4, §5, §7, §8, §9, §13 | `voip-gui`: the Slint desktop app -- freeform call-screen layout editor, full theming, contacts with QR invites, in-call chat with history/edit/delete/reactions, streamed file transfer with resume and rate control, presence, notification rules, moderation, data export, opt-in local crash reports |

Every crate's `lib.rs` doc comment states its scope against the spec.

## Where it stands

The short version of [`FEATURES.md`](FEATURES.md):

- **Real:** the 1:1 encrypted call path end to end, the whole §1 audio
  suite, lost-audio recovery (FEC, retransmit, adaptive jitter buffer,
  replay history, catch-up), file transfer with resume and send-rate
  control, chat persisted encrypted at rest with an outbox, QR / link
  invites, verification codes, key-change warnings, the UI editor and
  theming, presence, notification rules, data export, reproducible
  build configuration with a CI pipeline for all three desktop targets.
- **Reduced:** classical spectral noise suppression rather than an ML
  model; a symmetric ratchet without the DH step (a session is per call,
  so a compromised chain key is worth the rest of one call); keystore
  sealing on Windows only; Caps Lock cannot be a push-to-talk key
  (needs an OS-level hook, and is refused with the reason rather than
  misbehaving).
- **Not built:** anything that needs infrastructure the app does not
  have -- relays, the DHT, group calls / MLS / SFU, mailbox nodes and the
  tiered delivery routes, VPN integration, mobile clients, push
  notifications. Each is a project on its own and is listed as such.

## Building

Rust **1.97.0** (see [`BUILDING.md`](BUILDING.md) for why the version is
stated rather than enforced, and for how to verify a release binary).

```bash
cargo build --workspace
cargo test --workspace
```

**Linux** needs the development headers the audio and UI stacks link
against:

```bash
sudo apt-get install libasound2-dev libxkbcommon-dev libfontconfig1-dev libdbus-1-dev
```

**Windows** builds with either the MSVC or the GNU toolchain. **macOS**
needs nothing beyond Xcode's command-line tools. The
[CI workflow](.github/workflows/ci.yml) builds and tests all three from
the same commit.

Two tests are ignored by default because they need something the test
runner may not have: `discovers_a_public_address_from_a_real_server`
(internet access and a STUN server) and `a_large_file_streams_through`
(a 64 MB transfer, run on purpose when measuring throughput):

```bash
cargo test -p voip-gui large_file -- --ignored --nocapture
```

## Running it

### GUI

```bash
cargo run -p voip-gui
```

Opens a window and puts an icon in the system tray; closing the window
hides it there. An identity is generated silently on first run. Share your
QR code or `voip://` invite link from the Connect panel, or add a contact
by address; one side listens, the other calls. Settings -- the audio
chain, devices, keybinds, theme -- apply live to a running call and
persist across restarts. `--profile <name>` keeps a separate identity and
settings directory.

### CLI

```bash
cargo run -p voip-cli            # interactive menu
cargo run -p voip-cli -- listen --port 6001
cargo run -p voip-cli -- call --remote <A's IP>:6001 --port 6002
```

Hold Space to talk; Ctrl+C hangs up. `--no-ptt` keeps the mic open,
`devices` lists audio devices, `preset` exports the chain as an editable
JSON file, `identity restore` recovers an identity from its 24-word
phrase.

## Layout

```
crates/
  crypto/     identity, handshake, ratchet, trust, backup
  protocol/   wire framing
  net/        UDP transport, NAT traversal, recovery, file chunks
  audio/      DSP chain, echo cancellation, device I/O
  app/        voip-cli
  gui/        voip-gui (Slint UI) + build.rs compiling ui/app.slint
docs/         the feature spec
notes/        where work stopped, for picking up on another machine
.github/      CI: build + test on Linux/Windows/macOS; reproducible release builds
```
