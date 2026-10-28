# Next steps

Where work stopped, so it can be picked up from any machine. The full
feature-by-feature status is in [`FEATURES.md`](../FEATURES.md).

## Done: send-rate control for file transfers

The AIMD pacer ([`crates/client/src/pacer.rs`](../crates/client/src/pacer.rs))
is wired in. Repairs are served from the same paced loop as new chunks, so
pause and the upload cap apply to them; the receiver re-requests a missing
chunk no more than once a second; the sender cuts its rate at most once per
600 ms recovery window, because one burst of loss is reported across
several requests and used to be punished several times.

Two receiver-side causes of loss were found and fixed along the way, both
measured with the kernel's `RcvbufErrors` counter rather than guessed at:

- The periodic index save (`sync_data` every ~1 MB) ran inside the UDP
  receive callback. An fsync stalls the loop long enough to overflow the
  socket buffer. It now runs on a blocking thread from a snapshot, so the
  index still never claims bytes that were not synced first.
- The default ~200 KB receive buffer holds about fifty chunks.
  `PeerConnection` now asks for 1 MB (`RECEIVE_BUFFER` in `peer.rs`).

The earlier 4 MB-buffer experiment that "did not finish" was with the
blast-then-repair sender; with pacing in front of it, a larger buffer is
simply fewer drops.

**Debug builds are not a throughput measurement.** Unoptimised ChaCha20 and
SHA-256 capped a debug build's loopback transfer at ~3 MB/s regardless of
pacing, which is what made the pacer look broken when it was correctly
discovering a receiver that could not keep up. The crypto primitives are
now built optimised in the dev profile (see `Cargo.toml`); measure with
`--release` when the number matters:

```
cargo test --release -p antiphon large_file -- --ignored --nocapture
```

## Next: the relay

Self-hostable, with the choice of running it as part of the client or as a
Docker container. The goal is Matrix-style federation *without*
server-scoped identity: identity stays the keypair, and a relay is somewhere
to be reached, never who you are. The relay unlocks most of what is still
unstarted in `FEATURES.md`: §3 relay fallback, §8 offline delivery routes,
and the DH ratchet step that offline delivery makes necessary.

## Smaller items that need no infrastructure

- Keychain (macOS) and libsecret (Linux) sealing for the identity seed --
  `crates/crypto/src/keystore.rs` is Windows-only and says so.
- An actual keyboard-only pass and a screen-reader pass through the GUI.
- Launching the GUI on Linux and macOS by hand: the workspace builds and
  the full test suite passes on Linux, but the window has only been opened
  on Windows.
