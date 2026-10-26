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
| [`crates/crypto`](crates/crypto) | §3 Connection & Trust | Persistent Ed25519 identity + signed X25519 prekey, authenticated triple-DH handshake, Double Ratchet with ChaCha20-Poly1305 (fresh key per message, DH steps every few seconds, reorder-tolerant), trust-on-first-use key pinning, 40-digit verification codes, 24-word recovery phrase, cross-signed device list, OS keystore sealing (DPAPI on Windows) |
| [`crates/protocol`](crates/protocol) | §3 | Binary wire framing binding encrypted sessions to UDP datagrams (`Handshake` / `Audio` / `Control` / `File`, each with its own AAD) |
| [`crates/net`](crates/net) | §2, §3, §5, §6 | UDP peer connection, STUN discovery + hole punching, NAT-PMP port mapping with renewal, adaptive jitter buffer, forward error correction, audio retransmit requests, file chunking with per-chunk hashes |
| [`crates/audio`](crates/audio) | §1 Audio Suite | The mic chain -- noise suppression, gate, high/low-pass, de-esser, parametric EQ (bell/shelf/notch), compressor, limiter, gain/auto-gain, pitch shift with formant preservation, reverb -- plus an NLMS echo canceller with delay estimation and double-talk detection, SOLA catch-up playback, and `cpal` device I/O |
| [`crates/app`](crates/app) | §1 Utility | `antiphon-cli`: flag-driven, or an arrow-key interactive menu with no flags to memorise |
| [`crates/relay`](crates/relay) | §3 Relay, §8 mailbox | `antiphon-relay`: a self-hostable relay. Pairs two sockets on a blinded, daily-rotating token and forwards sealed frames byte for byte -- no keys, no logs, nothing kept past expiry. With `--mailbox`, also holds sealed mail for registered inboxes; with `--files`, sealed copies of pinned files |
| [`crates/gui`](crates/gui) | §4, §5, §7, §8, §9, §13 | `antiphon`: the Slint desktop app -- freeform call-screen layout editor, full theming, contacts with QR invites, in-call chat with history/edit/delete/reactions, streamed file transfer with resume and rate control, presence, notification rules, moderation, data export, opt-in local crash reports |

Every crate's `lib.rs` doc comment states its scope against the spec.

## Where it stands

The short version of [`FEATURES.md`](FEATURES.md):

- **Real:** the encrypted call path end to end, 1:1 or a mesh of up to
  five, direct or through a self-hosted relay, the whole §1 audio
  suite, lost-audio recovery (FEC, retransmit, adaptive jitter buffer,
  replay history, catch-up), file transfer with resume and send-rate
  control, shared files that outlive the call (per-file keys, seeding,
  pinning, multi-peer download), chat persisted encrypted at rest with
  an outbox and offline delivery through mailbox nodes on your own
  route list, QR / link
  invites, verification codes, key-change warnings, the UI editor and
  theming, presence, notification rules, data export, reproducible
  build configuration with a CI pipeline for all three desktop targets.
- **Reduced:** Caps Lock as push-to-talk works on Windows and X11 but
  not yet on macOS or Wayland; no project-run mailbox node ships as a
  default.
- **Not built:** anything that needs infrastructure the app does not
  have yet -- the DHT, large group calls / MLS / SFU, VPN integration,
  mobile clients, push
  notifications. Each is a project on its own and is listed as such.

## Installing

Releases carry one archive per platform with `antiphon` and `antiphon-cli`
inside, plus `SHA256SUMS.txt`. Each release binary is built twice from a
clean tree and published only when both builds match byte for byte.

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
cargo test -p antiphon large_file -- --ignored --nocapture
```

## Running it

### GUI

```bash
cargo run -p antiphon
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
cargo run -p antiphon-cli            # interactive menu
cargo run -p antiphon-cli -- listen --port 6001
cargo run -p antiphon-cli -- call --remote <A's IP>:6001 --port 6002
```

Hold Space to talk; Ctrl+C hangs up. `--no-ptt` keeps the mic open,
`devices` lists audio devices, `preset` exports the chain as an editable
JSON file, `identity restore` recovers an identity from its 24-word
phrase.

### Relay

```bash
cargo run -p antiphon-relay -- --listen 0.0.0.0:3479
# or: docker build -f crates/relay/Dockerfile -t antiphon-relay . && docker run -p 3479:3479/udp antiphon-relay
```

Put its `host:port` in Settings > Call > Relay server. While listening,
the app registers there, and anyone without a direct route -- strangers
by default, any contact you set to Relay -- reaches you through it
without either side seeing the other's IP address. Invite links then
carry the relay too.

To also hold sealed mail for people who are offline, add `--mailbox`
(off by default). `--mailbox-code <code>` limits it to your own
community; `--mailbox-storage-mb`, `--mailbox-inbox-kb`,
`--mailbox-rate` and `--mailbox-expiry-days` set the limits, and
`--stats-every 60` prints totals. Then add its address to a route
under Settings > Call > Message delivery.

To host files people pin there, add `--files` (off by default). It
only ever holds sealed chunks it cannot read. `--files-code <code>`
limits uploads to your community; `--files-max-mb`, `--files-quota-gb`
and `--files-per-user-mb` set the limits; `--files-expire-days N` or
`--files-expire-unopened-days N` set retention (default: keep);
`--files-list` and `--files-purge <id|all>` are the admin view. Then
add its address under Settings > Call > Shared files > Pin nodes.

## Layout

```
crates/
  crypto/     identity, handshake, ratchet, trust, backup
  protocol/   wire framing
  net/        UDP transport, NAT traversal, recovery, file chunks
  audio/      DSP chain, echo cancellation, device I/O
  app/        antiphon-cli
  relay/      antiphon-relay, self-hostable
  gui/        antiphon (Slint UI) + build.rs compiling ui/app.slint
docs/         the feature spec
notes/        where work stopped, for picking up on another machine
.github/      CI: build + test on Linux/Windows/macOS; reproducible release builds
```
