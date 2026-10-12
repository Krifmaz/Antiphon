# Next steps

Where work stopped, so it can be picked up from any machine. The full
feature-by-feature status is in [`FEATURES.md`](../FEATURES.md).

## In progress: send-rate control for file transfers

File transfers stream from disk to disk with no size limit, verified with a
64 MB transfer over real sockets. Throughput is the open problem.

**Measured:** about 2.6 MB/s on loopback. Roughly half the chunks of a large
transfer are lost on the first pass, and recovering them took 35 repair
rounds. A release build was *slower* (1.0 MB/s), which is how loss rather
than CPU was identified as the cause: the sender blasts with no rate control
and outruns the receiver's socket buffer.

**What exists:**

- [`crates/gui/src/pacer.rs`](../crates/gui/src/pacer.rs) — an AIMD rate
  controller with deadline-based pacing. **Not yet in the module tree**, so
  it has never been compiled and its tests have never run. First step: add
  `mod pacer;` to `crates/gui/src/main.rs` and get it building.
- [`pacer_wiring.py`](pacer_wiring.py) — the unapplied patch that wires it
  in. Repairs move into the paced send loop (so pause and the upload cap
  apply to them), the separate resend task and `ResendSource` go away, the
  repair interval drops from 2 s to 250 ms with per-chunk re-request
  suppression, and `TransferLimits::delay_after_chunk` becomes
  `limit_bytes_per_sec`. The tests in `transfer_limits.rs` still call
  `delay_after_chunk` and will need rewriting afterwards.

**Measure with:**

```
cargo test -p antiphon large_file -- --ignored --nocapture
```

**Tried and reverted:** 4 MB UDP socket buffers via `socket2` in
`crates/net/src/peer.rs`. With them the 64 MB test did not finish in 600 s
(versus 70 s without). Cause not yet understood; worth revisiting once the
pacer is in, since it changes how fast the sender fills those buffers.

## After that: the relay

Self-hostable, with the choice of running it as part of the client or as a
Docker container. The goal is Matrix-style federation *without*
server-scoped identity: identity stays the keypair, and a relay is somewhere
to be reached, never who you are.
