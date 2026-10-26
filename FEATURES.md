# Feature Checklist — VoIP App Feature Spec v0.3

Every feature from [`docs/voip-app-feature-spec.pdf`](docs/voip-app-feature-spec.pdf), section by section, audited against
the code in this repo, last updated 2026-10-17. `cargo test --workspace` and `cargo clippy --workspace --all-targets`
are clean on Linux and Windows.

**Legend**

| Mark | Meaning |
|---|---|
| `[x]` | **Done** — implemented, wired into the UI or CLI, and covered by the code linked beside it |
| `[~]` | **Partial** — a real but reduced version exists; the gap is stated on the line |
| `[ ]` | **Not started** — no code for it |

**Score: 116 done · 42 partial · 79 not started** (237 checklist lines in §0–§13, counted by their mark; the 8 items under *Not in the spec, but built* are extra)

**§0 Core Principles: 5 of 6 done.** The sixth needs infrastructure, not code in this repo.

The shape of it: **§1 Audio Suite and the 1:1 encrypted-call path are largely real.**
Everything that needs infrastructure the app doesn't have yet — relays, DHT, MLS
groups, mailbox nodes, mobile — is not started. The foundation the spec calls for
exists; the fallback infrastructure it calls "fallback, not foundation" does not.

---

## 0. Core Principles

- [x] **User choice** — every automatic behavior has a manual override
  *Auto-gain vs. fixed gain, PTT vs. voice activation, every chain stage toggles independently. The three stages whose parameters were hardcoded — the gate's attack/hold/release, the de-esser's ratio and split frequency, and the low-pass filter, which had no UI at all — are now exposed ([`app.slint`](crates/gui/ui/app.slint), [`read_chain_config`](crates/gui/src/main.rs)).*
- [x] **Encrypted end to end, always** — X25519 + ChaCha20-Poly1305, no plaintext path ([`crates/crypto`](crates/crypto))
- [x] **Explain everything** — no feature ships without a one-sentence description in the UI
  *Every settings section and every DSP parameter carries a plain-language sentence behind a hoverable circled-i, through one shared [`HelpIcon`](crates/gui/ui/app.slint) component (41 call sites). The sentence is also the accessible label, so a screen reader reads it without hovering.*
- [x] **Your data is yours** — decryption is on-device; there is no server in the call path at all ([`crates/net/src/peer.rs`](crates/net/src/peer.rs))
- [x] **Lightweight** — native Rust + Slint, no browser engine ([`Cargo.toml`](Cargo.toml))
- [~] **No central server required** — true for calls; with no relay or DHT, first contact needs a manually exchanged address. **Cannot be closed without building §3 relays or §4 discovery** — it is an infrastructure gap, not a UI one

---

## 1. Audio Suite

### Cleanup stage

- [x] **Noise Gate** — threshold, attack, hold, release, all four exposed in the UI ([`stages/noise_gate.rs`](crates/audio/src/stages/noise_gate.rs))
- [x] **Live meter showing what's passing through the gate** — the gate reports its own attenuation through the same `Stage::gain_reduction_db` channel the compressor uses ([`noise_gate.rs`](crates/audio/src/stages/noise_gate.rs))
- [~] **Noise Suppression** ([`stages/noise_suppress.rs`](crates/audio/src/stages/noise_suppress.rs)) — spectral subtraction with a strength slider, placed **before** the gate so the gate judges your voice rather than your fan. **Not the RNNoise-class model the spec names**: this is classical STFT-domain suppression with minimum-statistics noise tracking, so it removes what is steadily *there* — a fan, a hiss, an air conditioner, including underneath speech, which is the gap a gate cannot close — and will not remove a dog barking. Measured across the slider: 1.8 dB of broadband noise removed at 10%, 23 dB at 100%, while a modulated voice keeps 85–100% of its energy. Radix-2 FFT written in-crate rather than adding a dependency to an audio path for a textbook butterfly
- [x] **Warning at high suppression strength** — shown next to the slider, and only once past the strength that actually causes it, with the reason: speech and background share frequencies, so removing more of one removes more of the other
- [x] **High-Pass Filter** — adjustable cutoff, default ~80 Hz ([`stages/biquad.rs`](crates/audio/src/stages/biquad.rs))
- [x] **Low-Pass Filter** — adjustable cutoff, now reachable from the UI ([`stages/biquad.rs`](crates/audio/src/stages/biquad.rs))
- [~] **Echo Cancellation** ([`echo_cancel.rs`](crates/audio/src/echo_cancel.rs)) — a real NLMS adaptive filter, not the ducking workaround: it removes the echo instead of hiding it, so it still works while *both* people are talking, which is when an echo is most disruptive. Three parts, all of which are needed — a filter alone does nothing: bulk **delay estimation** by envelope cross-correlation (the echo arrives hundreds of ms late, and a 21 ms filter can only cover the tail it spans), the **NLMS filter** itself, and **double-talk detection** so your own voice does not wreck a converged filter. It **survives device hot-swap without re-converging** — a new output device has a new buffer, but the room is the same, so the delay is re-searched and the filter kept. Reaches ~15 dB of echo removal on synthetic paths; the meter shows what it is actually achieving, since a canceller doing nothing looks identical to one that is working until somebody complains. **On by default**, as the spec asks, with the manual off switch for headphone users who want the least processing. **Not done:** no residual/non-linear suppressor, so a speaker driven into distortion leaves something behind
- [x] **AEC survives device hot-swap without re-converging** — `device_changed()` drops the now-stale far-end history and re-searches the delay, but **keeps the learned filter**. Relearning would mean several seconds of audible echo every time somebody swapped headphones, and changing headphones does not change the room
- [x] **De-esser** — split-band, with threshold, ratio, split frequency, attack and release all exposed ([`stages/de_esser.rs`](crates/audio/src/stages/de_esser.rs))

### Tone stage

- [x] **Parametric EQ** — multi-band, per-band Hz/dB/Q ([`stages/peaking_eq.rs`](crates/audio/src/stages/peaking_eq.rs))
- [x] Draggable response curve, add/remove bands freely ([`eq_curve_db`](crates/audio/src/chain.rs), `on_eq_curve_drag`)
- [x] **Per-band type** — bell, low shelf, high shelf and notch, selectable per band ([`BandType`](crates/audio/src/stages/peaking_eq.rs)). All four are RBJ cookbook biquads sharing one filter, so the curve graph draws them with no changes
- [x] **Starter EQ presets** — Warm, Clear, Broadcast, Thin Mic Rescue ([`eq_preset`](crates/audio/src/chain.rs)), deliberately gentle (nothing beyond 6 dB) and fully editable after loading
- [x] **Compressor** — threshold, ratio, attack, release, makeup gain ([`stages/compressor.rs`](crates/audio/src/stages/compressor.rs))
- [x] **Gain reduction meter** — live, read from the running chain rather than inferred from settings ([`AudioChain::gain_reduction_db`](crates/audio/src/chain.rs))
- [x] **Limiter** — hard ceiling, release ([`stages/limiter.rs`](crates/audio/src/stages/limiter.rs))
- [x] **Gain / Auto Gain** — fixed level or automatic ([`stages/gain.rs`](crates/audio/src/stages/gain.rs))

### Creative stage

- [x] **Pitch Shift** — semitone control ([`stages/pitch_shift.rs`](crates/audio/src/stages/pitch_shift.rs))
- [x] Formant-preserving option ([`stages/formant.rs`](crates/audio/src/stages/formant.rs))
- [x] **Reverb** — room size, damping, wet/dry mix ([`stages/reverb.rs`](crates/audio/src/stages/reverb.rs))

### Voice Presets / Chains

- [x] Save the whole chain as a named preset ([`VoicePreset`](crates/gui/src/config.rs))
- [x] Export a preset to a file ([`on_export_preset`](crates/gui/src/main.rs))
- [x] Import someone else's preset ([`on_import_preset`](crates/gui/src/main.rs))
- [x] Each stage toggles independently ([`AudioChain::set_enabled`](crates/audio/src/chain.rs))
- [x] CLI preset management — `antiphon-cli preset` ([`crates/app/src/main.rs`](crates/app/src/main.rs))

### Utility

- [x] **Push-to-Talk** — held, not toggled ([`crates/app/src/ptt.rs`](crates/app/src/ptt.rs), [`state.rs`](crates/gui/src/state.rs))
- [x] PTT default key **Caps Lock** — the default on a fresh install wherever it can be held (Windows, X11); Space on macOS and Wayland, where it cannot ([`caps_lock.rs`](crates/gui/src/caps_lock.rs)). Existing configs keep the key they already have
- [x] **Caps Lock as a momentary key** ([`caps_lock.rs`](crates/gui/src/caps_lock.rs)) — on Windows a `WH_KEYBOARD_LL` hook swallows Caps Lock while it is push-to-talk, so the lock never flips, and reports held/released itself; on X11 the key goes through and the lock is cleared again on release with `XkbLockModifiers`. Only while Caps Lock is the PTT key *and* PTT is the mode — otherwise the key is untouched. macOS (HID access) and Wayland (portal) are still refused at bind time with the reason
- [x] **Detect key-capture failure** ([`key_capture.rs`](crates/gui/src/key_capture.rs)) — keys that can't work are refused *with the reason and an alternative*, and after binding, the app watches for the key actually arriving and warns if it never does. That's the exact failure the spec names: a binding that saves fine, never transmits, and gets blamed on the microphone
- [x] **Voice Activation** with sensitivity slider ([`va_threshold`](crates/gui/src/config.rs))
- [x] Live meter showing the threshold against your actual voice ([`main.rs` meter timer](crates/gui/src/main.rs))
- [x] Any input rebindable via the config menu ([`on_capture_binding`](crates/gui/src/main.rs), Keybinds tab)
- [x] **PTT, mute and deafen each switchable between hold and toggle** — PTT latches on press when toggled; mute and deafen toggle by default and can each be switched to *hold* (muted / deafened only while the key is down), Settings > Keybinds
- [x] **PTT release delay slider**, 0-1000 ms, so the tail of your last word isn't cut off
- [x] **Mute** and **deafen**, each independently bindable ([`on_toggle_mute`, `on_toggle_deafen`](crates/gui/src/main.rs))
- [x] Global hotkeys work while the app is unfocused — `device_query` polling ([`state.rs`](crates/gui/src/state.rs))
- [x] **Live Monitoring** — hear your own mic ([`monitor_sink`](crates/gui/src/state.rs), `on_toggle_monitor`)
- [x] **Readout of how much delay you're adding** — buffer plus an estimate for the sound card, shown as approximate ([`monitor_added_latency_ms`](crates/gui/src/state.rs)). Reporting only our own buffer would understate it, and cpal can't report device latency portably — so it says "about", which is the honest form
- [x] **Mic Test & Record Preview** — records five seconds, runs it through the full chain and plays it back, so you hear exactly what others hear ([`record_mic_test`](crates/gui/src/state.rs)). Nothing is kept. The **live loopback** toggle stays alongside it
- [x] **Device Hot-Swap** — change mic or output mid-call without dropping ([`ActiveCall::swap_output`](crates/gui/src/state.rs), `on_device_changed`)
- [ ] **Plugin Host** — LV2 / LADSPA / VST *(spec §11 defers to v1.5+)*

### Receive side (per person)

- [x] **Per-User Volume** — a per-person trim and mute on top of the master level, saved against the contact so a quiet microphone stays fixed across calls. Applied live to the playback loop; muting is local and the other side is not told
- [x] **Per-User EQ** — three tone controls (bass 200 Hz, voice 1.5 kHz, treble 5 kHz) applied to *their* audio on your end only, remembered against the contact so a boomy or harsh microphone stays fixed for every future call. Deliberately not a per-contact parametric editor: the problem is "their mic sounds bad", which three knobs answer. Flat sliders run no processing at all rather than three biquads sitting at unity, and the chain carries no gain stage so it can't fight the volume trim next to it
- [~] **Ducking** — the other side is lowered while your microphone is open, ramped rather than switched so it doesn't click at the start of every word. Not the group-call version the spec pictures, but it solves the same problem in the case that exists: without headphones, what you hear from them is picked up by your mic and sent back, and this breaks that echo loop

---

## 2. Lost Audio Recovery

- [x] **Forward error correction** — every frame carries the previous one decimated and quantised to 8 bits, about a twelfth the size ([`net/src/fec.rs`](crates/net/src/fec.rs)). A single lost frame is repaired with no round trip and no added latency; the jitter buffer reports it as `Recovered` rather than concealed
- [x] **Retransmit request** — for files (`FileMissing`) and now for audio (`AudioMissing`). The receiver asks only for gaps it is genuinely stuck behind, once each, and skips any gap a backup copy already covers ([`take_retransmit_requests`](crates/net/src/jitter.rs))
- [x] **Adaptive jitter buffer** — reorders and conceals, grows immediately on a concealment and shrinks after a run of clean deliveries, floored at 10 ms and capped at the configured ceiling ([`net/src/jitter.rs`](crates/net/src/jitter.rs))
- [~] **Replay** — the gap is filled by repeating the last good frame (concealment), not by recovering the lost audio ([`PopResult::Concealed`](crates/net/src/jitter.rs))

### Replay — user-facing

- [~] Recovered audio played back to you — what exists is a bindable **"save the last N seconds of received audio to a .wav"** ring buffer (`take_replay_snapshot`, `on_save_replay`). Useful, but a different feature than the spec's
- [x] **Replay modes: Off / Ask me / Auto-play** — when the network damages a moment badly enough to cut a replay clip, *Auto-play* replays it immediately, *Ask me* shows a "Replay it" button on the call screen, *Off* just keeps it in the history list. Small losses FEC repaired seamlessly are never offered, since there is nothing to replay
- [x] **Replay history panel** ([`replay_history.rs`](crates/gui/src/replay_history.rs)) — the 1.5 s before and 0.8 s after each damaged moment, kept as up to 8 clips with a Play button each. A clip is cut *after* the damage settles rather than when it starts, because a sentence cut off at the moment it broke is not something you can make sense of; a burst of loss with a good frame in the middle is one clip, not three; and a single repaired frame is 20 ms FEC almost certainly fixed perfectly, so it gets no clip at all. The tail length is derived from the settle window rather than set separately — the two drifting apart would mean asking for audio that was never recorded. In memory only, cleared with the call: writing clips to disk would turn a repair aid into a call recording
- [x] **Catch-up playback** ([`audio/src/time_compress.rs`](crates/audio/src/time_compress.rs)) — after a stall the jitter buffer holds a backlog, and playing it at normal speed means you hear every word but stay permanently behind. Once the backlog passes what the buffer would hold anyway, playback speeds up in proportion to how far behind you are, capped at 1.15x — past that the rhythm of speech is audibly wrong and the fix is more noticeable than the problem. SOLA rather than resampling: resampling to 1.1x also raises the pitch about 160 cents. Speed 1.0 is a true passthrough, so a healthy call pays nothing
- [x] **User-configurable buffer window**, 10-60 s, shared with the running call so a change applies to audio already being buffered ([`replay_cap`](crates/gui/src/state.rs)). Capped because the ring is uncompressed f32 in memory — ~192 KB a second, so an unbounded window would quietly cost a gigabyte
- [~] **Ephemeral by design, and says so** — the ring is in memory, rolling, discarded when the call ends, and never written to disk unless you press Save. It is **not encrypted in RAM**, and the setting's own help text now says exactly that rather than leaving it implied. Encrypting it is still not done: a live audio ring is decrypted to be played microseconds later, so encrypting it defends against very little while reading as a stronger promise than it is — which is why the disclosure came first

---

## 3. Connection & Trust Model

### Who you connect to directly

- [x] **Known contacts → Direct P2P** ([`PeerConnection`](crates/net/src/peer.rs))
- [x] NAT traversal — STUN discovery plus UDP hole punching ([`stun.rs`](crates/net/src/stun.rs), [`PeerConnection::punch`](crates/net/src/peer.rs))
- [ ] **Relay (default for everyone else)** — no relay exists; every call is direct
- [ ] **Anonymity mode** — multi-hop relay routing *(spec §11 defers to v1.5+)*
- [ ] Mode chosen automatically by contact status, manually overridable per conversation
- [x] **Active mode visible in the call UI with a one-line explanation** — "Connection: Direct", plus the line the spec asks for: connected straight to them with nothing in between, *which also means you can each see the other's IP address*. Both halves, because saying only the first would be selling it rather than explaining it. Modelled as an enum with one variant rather than a hardcoded string: the point of showing the mode is that it will one day differ

### Group calls

- [ ] Up to ~5 people — direct mesh, no infrastructure *(1:1 only today)*
- [ ] Beyond that — SFU relay forwarding encrypted frames *(spec §11 defers)*
- [ ] Community owners can self-host their own SFU
- [ ] Project-hosted relays as fallback

### Encryption

- [x] 1:1 forward secrecy — symmetric KDF ratchet, a fresh key per message ([`crates/crypto/src/ratchet.rs`](crates/crypto/src/ratchet.rs))
- [x] AEAD with AAD binding, replay rejection, tamper rejection ([`session.rs`](crates/crypto/src/session.rs))
- [x] Out-of-order UDP tolerated via a bounded skipped-key cache ([`ratchet.rs`](crates/crypto/src/ratchet.rs))
- [x] **Authenticated handshake** — interactive triple-DH binding the session to both long-term identities, so a session proves *who*, not just "whoever is at this address" ([`handshake.rs`](crates/crypto/src/handshake.rs))
- [~] **Double Ratchet** — the symmetric KDF chains and the authenticated root are there; the **DH ratchet step is not**. What that actually costs, stated precisely rather than as a blanket "no post-compromise security": a session here is established per *connection* — every call runs a fresh authenticated triple-DH ([`peer.rs`](crates/net/src/peer.rs)) — so a stolen chain key is worth the remainder of one call, not the conversation. The DH step would bound that by minutes instead of by call length. It becomes genuinely necessary when §8's offline delivery lands, because a session that outlives a connection is exactly the case the symmetric chains alone cannot recover from — which is why it is listed here rather than deferred out
- [ ] **MLS (RFC 9420)** for groups
- [ ] **SFrame** for media through a relay
- [~] **Multi-device** — cross-signing is built ([`device.rs`](crates/crypto/src/device.rs)): devices are signed into a sequenced list by the master key, revoked by omission. No transport or UI for approving a device yet
- [~] **Backup & recovery** — 24-word recovery phrase, shown in Settings > Profile and restorable via `antiphon-cli identity restore` ([`mnemonic.rs`](crates/crypto/src/mnemonic.rs)), plus a backup key sealed per-device ([`backup.rs`](crates/crypto/src/backup.rs)) so restoring an identity and restoring history stay separate powers. No Shamir split, and no message history to back up yet
- [x] **Persistent identity keys** — Ed25519 identity plus a signed X25519 prekey, generated on first run and stable across restarts ([`identity.rs`](crates/crypto/src/identity.rs), [`store.rs`](crates/crypto/src/store.rs))
- [x] **Identity is never server-scoped** — no address or host appears in an identity; where to reach someone is a separate signed record ([`route.rs`](crates/crypto/src/route.rs))
- [~] **Identity key storage** ([`keystore.rs`](crates/crypto/src/keystore.rs)) — on Windows the seed is sealed with DPAPI under the logged-in user, so the file is no longer readable bytes and neither another account nor a copy taken to another machine can unseal it. An existing plaintext file is upgraded in place the first time it is read, verified to keep the same identity. A sealed file that will not unseal is **refused, never replaced**: minting a fresh identity there would change who you are to every contact *and* throw away a key the recovery phrase could still have restored. The **backup key** — which decrypts every message you have ever sent — is sealed and migrated the same way ([`load_or_create_backup_key`](crates/gui/src/history.rs)). **Not done:** Keychain and libsecret — on Linux and macOS both are still written plain, and the code says so rather than pretending. Sealing is user-scoped either way, so it stops another account but not code running as you

### What a relay operator can see — and the commitment

- [ ] Blinded session tokens, no persistent user identifiers at the relay
- [ ] No connection logs retained
- [ ] The relay never holds a key to anything
- [ ] All of the above stated publicly and plainly, not buried in a privacy policy

*(All four unstarted because there is no relay.)*

---

## 4. Finding People (Discovery & Identity)

### Reaching people

- [x] **QR code / invite link** — `antiphon://<id>@<address>` links (links from before the rename, `voip://`, are still accepted), generated with a scannable QR ([`invite.rs`](crates/crypto/src/invite.rs), [`qr.rs`](crates/gui/src/qr.rs)) and pasted straight into contacts. Carries the permanent identity plus a bootstrap address, so first contact works with no directory, no relay and nobody listed anywhere
- [x] **QR reachable from the call screen** — as well as the profile settings page, it is a panel in the main view, toggled from the panel menu
- [x] **QR position user-configurable** — a full `FreeformPanel` like the others: drag by its title, resize from its corner, hide from the panel menu, saved per layout. **Off by default**, unlike every other panel: a code that identifies you should be on screen because you put it there, not because it happened to be on while somebody was sharing their screen. A layout saved before this panel existed loads with it hidden rather than dropped on top of an arrangement someone already made
- [x] Manual address exchange — add a contact by `host:port` ([`on_add_contact`](crates/gui/src/main.rs), [`Contact`](crates/gui/src/config.rs))
- [x] Public address discovery, so you have something to share (`on_discover_public_address` → STUN)
- [ ] **DHT** distributed lookup, so contacts find you after your IP changes
- [ ] DHT records encrypted and signed
- [ ] Blinded lookups, so nodes can't tell who you're searching for
- [ ] Sybil / eclipse resistance *(spec makes this a hard requirement before DHT ships)*
- [ ] **Opt-in directories** for public communities only
- [ ] No global username search by default *(vacuously true — there is no search)*

### Knowing it's really them

- [x] **Verification code** — a 40-digit code derived from both identities, shown in-call and printed by the CLI ([`verification.rs`](crates/crypto/src/verification.rs))
- [x] **Contact introduction** ([`introduction.rs`](crates/gui/src/introduction.rs)) — the one discovery mechanism that needs no infrastructure: an existing encrypted channel *is* the transport. An introduced contact is recorded with its provenance and stays **unverified** — treating a friend's word as proof would let one compromised contact launder keys into everyone's address book, which is exactly what the verification code exists to catch. Self-introductions and re-introductions of blocked people are refused
- [x] **Trust-on-first-use** with a warning if a contact's key ever changes ([`contact.rs`](crates/crypto/src/contact.rs), `evaluate_peer_trust` in [`main.rs`](crates/gui/src/main.rs)). The key is pinned on first call, a change is surfaced loudly and **never re-pinned silently**, and only the user confirming a compared code accepts a new one
- [x] Friend requests ([`control.rs`](crates/gui/src/control.rs)) ride an authenticated session, so they prove which identity sent them, and the contact now stores the peer's **identity key** alongside the address

### Infrastructure position

- [x] If project infrastructure disappears, existing contacts keep working *(true today by construction — there is none)*
- [ ] Bootstrap nodes, fallback relays, optional directories

---

## 5. File Transfer

- [x] **Peer-to-peer by default, nothing uploaded to a server** ([`net/src/file_transfer.rs`](crates/net/src/file_transfer.rs), [`gui/src/file_transfer.rs`](crates/gui/src/file_transfer.rs))
- [x] Offer / accept / reject flow ([`ControlMessage::FileOffer`](crates/gui/src/control.rs))
- [x] Either side can cancel an in-progress transfer (`FileCancel`)
- [x] Chunked, whole transfer hash-verified end to end (SHA-256)
- [x] **Missing-chunk repair** — receiver lists gaps, sender resends just those (`FileMissing`). Each request is now **capped at 256 chunks** so it always fits in one 4 KB control datagram: before, a badly damaged transfer produced a request too large to send, so the transfers that most needed repair were the ones that never got it. While chunks are still arriving only true gaps are requested, not the tail the sender hasn't reached yet. Covered by a real-socket test that drops every third chunk and still gets the file byte-for-byte
- [x] Encrypted in transit — file frames ride the same AEAD session as audio ([`FrameType::File`](crates/protocol/src/frame.rs))
- [~] Progress UI ([`update_file_transfer_ui`](crates/gui/src/main.rs)) — progress, errors, **pause/resume**, and now two states that previously looked like hangs: *"checking it arrived intact"* while a large file is hash-verified, and *"resumed, N% was already here"* so a bar starting at 60% explains itself. **Not done:** a queue, since only one transfer runs at a time by design
- [x] **Content-addressed** — completed files stored under their SHA-256 ([`content_store.rs`](crates/gui/src/content_store.rs)). An offer whose hash is already held skips the transfer entirely and goes straight to the save dialog
- [x] **Each chunk hashed and verified** — every chunk carries an 8-byte SHA-256 prefix of its own body, checked in [`parse_chunk`](crates/net/src/file_transfer.rs) before it is stored. A damaged chunk is rejected on arrival and re-requested by the ordinary missing-chunk path, instead of failing the whole-file hash at the very end with nothing to say which chunk was wrong. Eight bytes and not thirty-two because this catches *corruption*: forgery is already covered a layer down, since every chunk arrives inside the session AEAD. The whole-file hash is still checked at the end
- [x] **Transfers resume after disconnect** — the partial file on disk *is* the reassembly buffer ([`DiskReassembly`](crates/gui/src/content_store.rs)), so resuming is just reopening it; there is no separate snapshot to keep in step. The index is saved every ~1 MB and data is flushed before the index, so a crash costs at most a megabyte of re-fetching and never an index claiming bytes that aren't there. Partials from earlier builds still resume. Cancelling your own download discards its partial; the *sender* cancelling keeps it, in case they offer it again
- [ ] **Per-file key delivered inside the encrypted message**, so any peer can hold an unreadable copy
- [ ] **Multi-peer download** — pull chunks from several peers at once
- [x] **No size limit** — streamed at both ends: the sender reads each chunk from disk as it goes ([`FileSource`](crates/gui/src/file_integrity.rs)) and the receiver writes each one straight to its offset. Verified with a 64 MB transfer over real sockets. Hashing and verification stream too, and run on blocking threads so they never stall call audio. Streaming reopens a window the in-memory version didn't have — bytes are read after they were hashed — so every read checks the file's size and modified time and **stops with an explanation** if it changed, rather than sending content that doesn't match its own address. The receiver now confirms as soon as the file is verified and stored, not after the user picks a save location (which used to make the sender report failure if you took 20 s choosing a folder). Throughput is governed by the send-rate control in the next item
- [x] **Send-rate control** ([`pacer.rs`](crates/gui/src/pacer.rs)) — AIMD pacing driven by the receiver's repair requests: slow-start until the first loss, then additive increase and a 30% cut on loss above a 2% noise floor, **at most one cut per 600 ms** — a burst is reported across several requests, and cutting for each report punished one event three times over. Repairs are served from the same paced loop as new chunks, so pause and the upload cap apply to them, and the receiver re-requests a chunk no more than once a second instead of repeating its whole list every round. Deadline-based rather than per-chunk sleeps, which also fixes the cap undershooting on Windows' coarse timers. Two receiver-side causes of loss found with the kernel's `RcvbufErrors` counter and fixed: the periodic index fsync ran inside the receive callback (now a blocking-thread job from a snapshot, same ordering guarantee), and the ~200 KB default socket buffer held fifty chunks (now 1 MB, `RECEIVE_BUFFER` in [`peer.rs`](crates/net/src/peer.rs)). The earlier "4 MB buffers made it worse" result was with the blast sender; with pacing in front, a bigger buffer is just fewer drops. **Measured, 64 MB over loopback, same machine:** before, 1.2 MB/s with 74 repair rounds and 1,160 datagrams dropped at the socket; after, 2.7 MB/s with 4 repair rounds and 47 dropped in a debug build, and **16.6 MB/s with 6 repair rounds in release** (previously ~1 MB/s in release, because the blast sender lost more the faster it went). The remaining debug-build ceiling is unoptimised decrypt per chunk, not loss

### Availability

- [ ] **Seeding** — keep helping share a file after you download it, off by default
- [ ] **Seeding scope** — this conversation / this community / contacts / anyone, enforced at the protocol level
- [ ] **Anonymous seeding** through relays
- [ ] **Pinning** — ask a device or server you trust to hold a copy
- [~] **Availability status** — the three states exist with wording that says whether waiting will help ([`Availability`](crates/gui/src/file_preview.rs)), and an offer shows Available. The other two need §8's delivery routes before they're observable

### Server-hosted mode (opt-in)

- [~] A `server_mode` toggle exists, but it means "auto-listen for incoming calls" — **not** the spec's hosted file storage
- [ ] Per-community toggle, off by default
- [ ] Max file size, total storage quota, per-user quota
- [ ] Retention: keep forever / expire after N days / expire when unopened
- [ ] Stored encrypted — the server holds bytes it cannot read
- [ ] Admin view of usage, manual purge

### Details that will bite if skipped

- [ ] Mobile file access — Android persisted URI permissions, iOS security-scoped bookmarks
- [x] **Moved or deleted files** — a file is read and hashed at the moment it is picked ([`file_integrity.rs`](crates/gui/src/file_integrity.rs)), so anything already gone is reported before the offer rather than dying halfway with no explanation. Reading and hashing happen in one step on purpose: any gap between checking and reading is a window for the file to change, and with content addressing the hash *is* the file's identity
- [ ] Background transfers — Android foreground service, iOS resume-on-open
- [x] **Thumbnails generated by the sender, sent inline** ([`file_preview.rs`](crates/gui/src/file_preview.rs)) — a 64px PNG capped at 2 KB so it fits in the offer datagram. Sender-generated for the same reason link previews aren't fetched: a receiver rendering a preview is running an image decoder over a file it hasn't agreed to accept. The size limit and PNG magic are re-checked on arrival, since the sender chose those bytes
- [~] **Bandwidth controls** — an overall upload cap and an automatic throttle while a call is running, paced per chunk rather than in bursts ([`transfer_limits.rs`](crates/gui/src/transfer_limits.rs)). Throttling during calls is *on by default*: an unthrottled transfer makes a call sound broken, and people blame the call rather than the file. A user's own lower cap is never raised by the in-call rule. **Not done:** queue, Wi-Fi-only (which needs mobile); pause/resume is done, see the progress UI above

---

## 6. Network Privacy Tools

- [ ] **Bring your own VPN** — import a WireGuard configuration file
- [ ] Userspace WireGuard inside the app, not a system-level VPN
- [ ] Import, store, and switch between multiple configs
- [ ] Per-mode routing — file transfers through the VPN while voice stays direct, or the reverse
- [~] **Key storage in the OS keystore** — done on Windows (DPAPI), still plaintext on Linux and macOS; see §6 above. Never in theme exports or backup blobs is already true and structural: the key lives in its own file, so settings export — which serializes `config.json` — cannot sweep it up
- [ ] Kill switch — app traffic stops rather than falling back to your real connection
- [x] **Automatic port negotiation** ([`port_map.rs`](crates/net/src/port_map.rs)) — a NAT-PMP (RFC 6886) client, with the renewal loop that makes it mean anything: mappings expire by design, so a one-shot request would leave you reachable for an hour and then quietly not, with nothing on screen changing. Renewed at half the lifetime as the RFC prescribes, so a lost renewal still gets a second chance. An hour, not a day, so a hole left by a crash closes on its own. The **external port is read from the reply, never assumed** — routers are allowed to hand back a different number, and reporting the requested one would send people to a port nobody is listening on. Released when you switch it off and again on exit. Every refusal names a next step, because a failure the user cannot act on is noise. NAT-PMP only: UPnP-IGD is SOAP over HTTP with a long history of being exploitable, and the small protocol is the right one to add first
- [~] Handles port changes on reconnect without user action — the renewal loop keeps trying through a router reboot and picks the mapping back up, backing off so a router that is simply gone is not hammered. **Not done:** noticing that the router handed back a *different* external port on renewal and telling anyone — that needs somewhere to publish a changed address to, which is §4's DHT
- [x] **The port-forwarding capability table** ([`reachability.rs`](crates/gui/src/reachability.rs)) — three rows in the Connect panel answering the questions people actually have: can I call out (always yes), is my public address known, and can anyone call me. The rule it turns on: **"not checked yet" is never reported as "no"** — one is a router to look at, the other is a button nobody pressed, and collapsing them sends people to debug a network that is fine. A refusal says what still works (call out, forward by hand, both press Punch) rather than overstating the problem
- [~] **Reachability status** — the capability table above answers "can I be reached", combining the STUN and NAT-PMP results. Still no Direct / Relayed / blocked line, because there is no relay to fall back to; what exists instead is an explicit **Connection: Direct** line in the call UI

---

## 7. Interface & Customization

- [x] **Layout editor** — move, resize, add, and remove panels on the call screen ([`FreeformPanel`](crates/gui/ui/app.slint), `on_call_panel_drag_*` / `on_call_panel_resize_*`)
- [x] Per-panel visibility toggles ([`call_*_visible`](crates/gui/src/config.rs))
- [x] Reorder settings sections by dragging ([`on_move_audio_section`, `on_move_appearance_section`, `on_move_profile_section`](crates/gui/src/main.rs))
- [x] **Save named arrangements** — the call screen's panel positions, sizes and visibility saved under a name and switched between ([`NamedLayout`](crates/gui/src/config.rs)). Deliberately excludes colours, fonts and audio, so trying a compact layout doesn't also change your theme between them
- [~] **Profile layout** — the profile widget's avatar / name / status can each be hidden and the panel repositioned, but there is no full profile-screen editor and no QR code to place
- [x] **Scale** independent of OS scaling (`text_scale`)
- [x] **Fonts** — family and size, including importing your own ([`on_import_font`](crates/gui/src/main.rs))
- [~] **Dyslexia-friendly font option** — an "Easy reading" choice that resolves against fonts actually installed (OpenDyslexic first, then wide-countered fallbacks), asking the same font collection the renderer uses so it can never select something that won't render. **Not bundled:** shipping a font means licensing it for redistribution, and the typeface that helps most is one people install deliberately
- [x] **Color themes** — full palette control via HSV picker and hex entry ([`ColorPicker`](crates/gui/ui/app.slint))
- [x] Light / dark / custom theme mode (`theme_mode`)
- [x] Custom background image or animated GIF with fit, rotation, crop, and opacity ([`animated_image.rs`](crates/gui/src/animated_image.rs))
- [x] **Export & import** — one file carries layout, colors, fonts, and profile arrangement ([`on_export_settings` / `on_import_settings`](crates/gui/src/main.rs))
- [x] **Theme files are declarative data only, no scripting, ever** — plain JSON through `serde` ([`config.rs`](crates/gui/src/config.rs))
- [x] A good default plus a reset path (`on_defaults`)
- [x] **Theme preset gallery** — six complete schemes with live swatches ([`theme_presets.rs`](crates/gui/src/theme_presets.rs)), each tested to clear WCAG 3:1 accent-on-background contrast so no entry looks broken. Sets accent, background and light/dark only — wallpaper, font and opacity are left alone
- [x] **Statuses** — custom status text ([`ProfileData`](crates/gui/src/config.rs))
- [x] **Presence** — online / away / busy / invisible, published only to saved contacts ([`presence.rs`](crates/gui/src/presence.rs)). **Invisible sends nothing at all**, not a status reading "offline" — an app that transmits something while claiming invisibility has quietly sold the user out. There is deliberately no "last seen", asserted structurally by a test: what goes on the wire is a state with nowhere for a timestamp to hide
- [ ] Per-community identity
- [~] **Accessibility — screen reader** — `accessible-role` / `accessible-label` / `accessible-value` are set on the custom slider, toggle, and tab components, but coverage is partial and untested with a real screen reader
- [ ] On-device live captions
- [~] **Keyboard-only navigation** — every custom control (toggle, tab, sidebar button, section header, slider) now takes focus, activates on Space/Enter and draws a visible focus ring; sliders respond to arrows plus Home/End. A control built from a `TouchArea` is invisible to the keyboard without this, which is why each needed its own `FocusScope`. **Not verified** by an actual keyboard-only pass through the whole app
- [x] **Reduced motion** setting, honored by the custom components (`reduced_motion`)
- [x] System tray icon with show / quit ([`on_show_window`, `on_quit_app`](crates/gui/src/main.rs))

---

## 8. Message Delivery

- [~] **Text chat, persisted** — encrypted over the control channel in call ([`ControlMessage::Chat`](crates/gui/src/control.rs)) and stored per-identity, encrypted at rest under the backup key ([`history.rs`](crates/gui/src/history.rs)), so a conversation reopens where it left off. Messages still only *move* while both are online — none of the delivery model below is built
- [~] **Outbox** ([`outbox.rs`](crates/gui/src/outbox.rs)) — a per-contact queue, encrypted at rest under the same key as history, flushed the moment that contact is reachable again (including across restarts). A message leaves the queue **only when the transport confirms it went** — before this, `send_control`'s error was discarded and a message handed to a dying connection was shown in the sender's own log as though it arrived, which is the one failure a messaging app must not have. Full offline *composition* still needs a contact-selection view that does not exist yet, so today this catches sends that fail rather than letting you write to someone who was never online
- [ ] **Multi-device catch-up** — a desktop holds messages while your phone sleeps, then hands them over
- [ ] **Delivery Routes** — a single ordered list, walked top to bottom
- [x] Route 1: Direct — the only route that needs no infrastructure, and the only one built
- [ ] Route 2: My other devices
- [ ] Route 3: A node you trust
- [ ] Route 4: Community node
- [ ] Route 5: Project relay
- [ ] Per-route on / off and drag-to-reorder
- [ ] Node picker — add by address, QR, or auto-detect on the local network
- [ ] Live per-node status: reachable, response time, last used, storage used, storage cap
- [ ] Remove or replace a node with waiting messages re-routed, not lost
- [ ] **Redundancy setting** — first available vs. fan out
- [ ] **Strict mode** — every route off except Direct, nothing stored anywhere ever
- [ ] **Per-contact route overrides**
- [ ] **Server-side opt-in** — a self-hosted node chooses whether it relays or holds mail, off by default
- [ ] Operator controls: storage caps, per-label rate limits, expiry, own-community-only
- [ ] Writes accepted only to labels registered with that node, so strangers can't fill a public node
- [ ] Operator dashboard with no ability to see contents or identities
- [~] **The recipient's published route list decides where mail is deposited**, not the sender — the signed, sequenced record is implemented and rollback-resistant ([`route.rs`](crates/crypto/src/route.rs)); nothing publishes or routes on it yet
- [x] **Sender controls only the outbox** — a "keep trying" window from forever (the default) to 14 days, under Call → Undelivered messages. Forever by default because a message you meant to send is still one you meant to send a week later; when the window does run out, the messages dropped are reported rather than disappearing quietly. Where mail may be *deposited* stays the recipient's decision, not the sender's
- [x] **Delivery status wording** — the spec's exact three, shown against a message only while it is still queued: labelling every delivered line "Delivered" is noise, and noise is what stops people noticing the one that says otherwise. Three states and not two, because "sent" and "delivered" being one word is how an app claims something arrived when it only left

---

## 9. Chat & Communities

### Starting a call

- [x] Start a 1:1 call to a contact ([`on_start_call`, `on_call_contact`](crates/gui/src/main.rs))
- [x] Listen for an incoming call (`on_start_listen`, plus `server_mode` auto-listen)
- [x] Hang up, bindable to a key (`on_hang_up`, `hangup_key`)
- [x] **Silent call, including from the outset** — `Hello.wants_silent` carries the intent with the *first* thing sent, so the other end knows before it decides rather than being corrected afterwards; [`ControlMessage::SilentCall`](crates/gui/src/control.rs) still turns a call already in progress into a knock. Both are *requests* that reach the one place that can only lower the receiver's outcome — there is deliberately no field or message that raises one. A greeting from a peer predating the field reads as "did not knock", since defaulting the other way would silence calls from anyone on an older build. Note that "ringing" today is a status line and a tray notification; there is no audible ring yet
- [x] **Shift+click, its own control, and an invertible default** — all three the spec asks for, with the priorities reversed. The visible "Knock instead of ringing" switch is always there; leaving it on *is* inverting the default. Shift+click on Call is opt-in (Settings → Call → Advanced) and **flips the switch for one call and puts it back**, so a modifier can never quietly change a standing preference. Shift+click is not the default here because a courtesy nobody can find is not a courtesy
- [ ] Persistent "Voice active · 3 people" bar in the channel with a join button
- [x] **Escalate a silent call to a ringing one** — a "Ring them after all" button replaces Knock once you have knocked, sending [`ControlMessage::EscalateCall`](crates/gui/src/control.rs). The connection, keys and audio all continue; only the notification decision is revisited. It withdraws the *caller's own* silencing and nothing more — the receiver's rules are re-read on their machine and still decide, so a contact set never to ring still does not. When their rules refuse, the caller is told, since an escalation that quietly did nothing would leave them waiting on an answer that is not coming
- [x] **Silent call in 1:1 as a knock** — its own button rather than a hidden Shift modifier, since a courtesy nobody can find is not a courtesy
- [~] **Receiver settings always win** — per-contact ring rules (always / silent only / never) plus a default and a do-not-disturb schedule ([`notify.rs`](crates/gui/src/notify.rs)). Per-*community* settings wait on communities existing
- [x] **A caller can never force a ring through do-not-disturb** — the decision is made on the receiving machine from the receiver's own settings, and every path through `notify::decide` either honours the rule or lowers the outcome. There is deliberately no input that raises it

### Communities

- [ ] Group chats up to ~100, no server required
- [ ] Communities with channels, roles, permissions *(spec §11 defers full scale)*
- [ ] Threads and replies
- [x] **Search over your own history**, indexed locally on your device ([`history.rs`](crates/gui/src/history.rs)) — substring search over the current conversation, plus a per-contact Forget
- [~] **Moderation — block and report** ([`moderation.rs`](crates/gui/src/moderation.rs)). A block is keyed to the *identity*, so it follows the person rather than an address they can change, and it is enforced on the receiving machine before the call is shown — there is no server to do it. Reports stay on the device: nothing can read a call, so a report cannot carry evidence of one, and the UI says exactly that. Mute is per-contact (§1). **Not done:** per-community ban lists, which wait on communities
- [ ] Bots / API, local-first *(spec §11 defers)*

---

## 10. Platform Strategy

- [x] **Shared Rust core** — audio, crypto, net, protocol, and state split into separate crates ([`Cargo.toml`](Cargo.toml))
- [x] **Slint declarative UI** for desktop ([`crates/gui/ui/app.slint`](crates/gui/ui/app.slint))
- [~] Slint markup doubling as the theme format — theming is JSON config, not the markup itself
- [~] **Desktop UI (Linux, Windows, macOS)** — one Slint UI. Builds, passes the full test suite and is clippy-clean on **Linux** and **Windows**; the [CI pipeline](.github/workflows/ci.yml) builds and tests all three on every push. **Not done:** the window has been opened by hand only on Windows — the Linux and macOS GUI have been compiled and tested, not sat in front of
- [ ] **Mobile UI (Android, iOS)** — Compose / SwiftUI calling the core over FFI
- [~] **Audio backends** — `cpal` abstracts WASAPI / CoreAudio / ALSA, but PipeWire and AAudio are not specifically addressed
- [ ] **Push notifications** — FCM, APNs / PushKit, persistent socket on desktop
- [ ] **Packaging** — Flatpak / AppImage / deb, APK, MSI, dmg. The pipeline below produces bare binaries with checksums, not installers
- [~] **One release pipeline building all targets from the same commit** ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) — every push builds, tests and lints Linux, Windows and macOS; a `v*` tag builds release binaries for all three, **twice from clean each**, and refuses to publish any that differ byte for byte, then uploads them with `SHA256SUMS`. Desktop only: there are no mobile targets to build
- [ ] Ship order Linux + Android first — *the current build is Windows-first, a deliberate divergence worth deciding on explicitly*

---

## 11. Deferred (v1.5+) — the spec's own list, tracked for completeness

- [ ] Video calls
- [ ] Screen share with audio
- [ ] SFU relay for large calls
- [ ] Communities at full scale
- [ ] Plugin host
- [ ] Bots / API
- [ ] Anonymity mode (multi-hop)

---

## 12. Open Questions — still unanswered, and now load-bearing

1. [~] **Identity model** — usernames, keypairs only, or both? What's the recovery story when someone loses every device?
   *Answered in practice by what is built: identity is the keypair ([`identity.rs`](crates/crypto/src/identity.rs)), recoverable from a 24-word phrase ([`mnemonic.rs`](crates/crypto/src/mnemonic.rs)); the handshake is authenticated against it, which is what §4 verification, §3 cross-signing and authenticated friend requests rest on. Still open: whether there are usernames at all (nothing here needs one), and what happens to someone who loses every device **and** the phrase — today, nothing can be done, and the first-run flow says so by asking for a backup only after a real call*
2. [ ] **Relay funding** — donations, community-run, or volunteer?
3. [ ] **Push on iOS** — confirm the encrypted-payload design before committing to iOS
4. [ ] **Federation** — can two self-hosted communities talk to each other, or are they islands?
5. [ ] **Moderation without content access** — what are the actual tools, and what's the answer when a store reviewer asks?

---

## 13. Proposed — Not Yet Confirmed

- [~] **Notification controls** — per-contact rules, a global default and a quiet-hours window that correctly handles crossing midnight ([`notify.rs`](crates/gui/src/notify.rs)). Times are UTC, stated in the UI, since a timezone database is a large dependency. Per-community and per-channel wait on communities
- [~] **Message lifecycle** — edit, delete and pin are done ([`history.rs`](crates/gui/src/history.rs), [`ChatEdit`/`ChatDelete`](crates/gui/src/control.rs)), with per-contact disappearing messages (1 hour / 1 day / 1 week) pruned on every read so an expiry survives the app being closed. Edits and deletes only ever touch messages their own sender wrote; pinning is local. **Not done:** saved / note-to-self, which needs a conversation with yourself and so waits on multi-device
- [x] **Link previews — nothing is ever fetched** ([`link_preview.rs`](crates/gui/src/link_preview.rs)). The spec names the trap and this takes the side it points at: links are parsed locally and the message shows where each one actually goes, including unmasking `https://real-looking.com@evil.example` and flagging plain HTTP. A fetched preview would hand the sender your IP before you decided whether to trust them
- [~] **Reactions and soundboard** — reactions are a fixed set of eight, toggled by tapping, and **deleted with the message they point at** ([`reactions.rs`](crates/gui/src/reactions.rs)): leaving thumbs-up under "(deleted)" would partly undo the deletion. Emoji arriving from a peer are checked against that set, or the reaction row becomes somewhere to put arbitrary text. Soundboard takes 16-bit WAV clips up to 15 s with separate Send and Preview buttons, mixed in *after* the mic chain ([`soundboard.rs`](crates/gui/src/soundboard.rs)). **Custom emoji deliberately not done:** they mean rendering an image someone else chose, with nobody accepting anything first
- [~] **Voice messages** — recorded through the same mic chain a call uses, silence-trimmed, capped at 2 minutes, and sent down the existing file-transfer path so they inherit its chunking, hash verification and repair ([`voice_message.rs`](crates/gui/src/voice_message.rs)). The receiver is shown "Voice message", not a timestamped WAV. **Not async yet:** with no mailbox or relay, nothing can hold one for a peer who is offline
- [~] **Reproducible builds — configured, documented and checked** ([`BUILDING.md`](BUILDING.md)): single codegen unit, path remapping, `SOURCE_DATE_EPOCH` and `--locked`, with a table explaining what each one prevents. A `rust-toolchain.toml` was tried and removed — pinning a channel resolves against rustup's default *host triple*, which silently moved one machine from the GNU target to MSVC and broke linking; the toolchain is pinned per target in the pipeline instead, where the build matrix already knows its triple. **Proven on one machine:** two release builds of the same commit, with `cargo clean` between, produced byte-identical `antiphon` and `antiphon-cli` (hashes recorded in `BUILDING.md`). The release job repeats that cross-check on every tag and fails rather than publish a binary that differs. **Not done, and said so:** nothing is signed, and both builds of the check run on the same kind of machine — a verifier matching the hash on their own is what closes that
- [x] **First-run experience** ([`first_run.rs`](crates/gui/src/first_run.rs)) — the identity is generated silently before the first screen, and the opening message is *it already worked*, not homework. Backup is suggested only after a real call with a real contact, since asking someone to write down 24 words to protect an empty account teaches them prompts are noise. A test asserts the wording contains no jargon
- [x] **Data export in an open format** — settings, contacts, decrypted message history, soundboard clips and received files, written as plain JSON and WAV in a directory tree ([`data_export.rs`](crates/gui/src/data_export.rs)). The identity and backup keys are deliberately excluded and the manifest says why, since a key in a folder people email around is how identities leak. The UI warns that the exported folder is unencrypted
- [ ] Mobile battery & data controls
- [x] **Crash reporting, opt-in only, no identifiers** ([`crash_report.rs`](crates/gui/src/crash_report.rs)). Off means *nothing is written*, not written-and-withheld. Reports carry the panic message and source location with usernames, home directories, keys and addresses scrubbed, and the timestamp blunted to the hour so it can't be used as a correlation handle. Nothing is ever transmitted — there is no endpoint

---

## Not in the spec, but built

Recorded so this checklist isn't mistaken for the full inventory:

- [x] **Settings search** — every section indexed by title plus the words people actually reach for ("sibilance" finds the de-esser), with a summary pointing at other categories ([`search.rs`](crates/gui/src/search.rs))
- [x] **Call settings tab** — start-of-call defaults, participant defaults, and an Advanced section holding **IP direct calling**, off by default so a raw-address call is a deliberate choice
- [x] **Multiple identities per machine** — `--profile <name>` gives each its own identity key and settings directory, in both binaries ([`config.rs`](crates/gui/src/config.rs), [`main.rs`](crates/app/src/main.rs)). Not an account system: no server, no login, just one machine holding more than one keypair. The default profile keeps the original paths, so an existing install is untouched
- [x] **CLI client** (`antiphon-cli`) with an arrow-key interactive menu, so there are no flags to memorize ([`crates/app/src/interactive.rs`](crates/app/src/interactive.rs))
- [x] **In-app terminal command box** (`on_terminal_command`)
- [x] **Window state persistence** — size, position, maximized ([`WindowState`](crates/gui/src/config.rs))
- [x] **Profile avatar** with import, fit, rotation, crop, and animated-GIF support
- [x] **Save received audio to .wav** — bindable, ring-buffered ([`wav.rs`](crates/gui/src/wav.rs))
