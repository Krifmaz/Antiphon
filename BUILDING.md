# Building and verifying

Spec §13 puts this plainly: *"for a privacy app this is trust
infrastructure, not a nicety. Users should be able to verify the binary
matches the published source."*

An unverifiable binary asks people to trust that whoever built it did not
add anything. For an app whose entire pitch is that nobody can read your
calls, that is the one place trust should not be required — the point of a
reproducible build is that you do not have to take anyone's word for it.

---

## Building

```bash
cargo build --release
```

Built with **Rust 1.98.0**.

There is deliberately **no `rust-toolchain.toml`**, and the reason is worth
recording because adding one looks obviously correct:

A `rust-toolchain.toml` naming a channel resolves against rustup's *default
host triple*. On Windows that is usually `x86_64-pc-windows-msvc`, so the
pin silently switches a developer whose working toolchain is
`x86_64-pc-windows-gnu` onto MSVC — and their build stops linking. Pinning
the full triple instead would hardcode one platform into a project meant to
build on three.

So the version is a documented requirement rather than an enforced one.
A release build must use the stated version: rustc's output changes between
releases, and two people on different compilers cannot produce the same
bytes no matter what else they do.

## Verifying a release binary

The goal is that you can build the published source yourself and get a
file identical to the one that was distributed.

```bash
git checkout <the tagged commit>

# Same directory layout, same timestamps, same paths baked into the binary.
export SOURCE_DATE_EPOCH=$(git log -1 --pretty=%ct)
export RUSTFLAGS="--remap-path-prefix=$PWD=/build --remap-path-prefix=$CARGO_HOME=/cargo"

cargo build --release --locked
sha256sum target/release/antiphon
```

**On Windows, or anywhere the path contains a space,** that `RUSTFLAGS`
line silently does the wrong thing. Cargo splits `RUSTFLAGS` on spaces, so
`C:\Users\Some Name\...` arrives as two broken arguments; and under Git Bash
`$PWD` is `/c/Users/...` while the compiler sees `C:\Users\...`, so the
prefix never matches and the path ships in the binary anyway. Use
`CARGO_ENCODED_RUSTFLAGS`, which separates flags with `\x1f` rather than
spaces, and pass Windows-form paths. This is how the published test builds
are made:

```bash
export SOURCE_DATE_EPOCH=$(git log -1 --pretty=%ct)
SRC="$(cygpath -w "$PWD")"
CH="$(cygpath -w "${CARGO_HOME:-$HOME/.cargo}")"
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=${SRC}=/build"$'\x1f'"--remap-path-prefix=${CH}=/cargo"

cargo build --release --locked
sha256sum target/release/antiphon.exe target/release/antiphon-cli.exe
```

Afterwards, searching the binary for your own username is a quick check
that the remap took effect.

`--locked` matters: without it, cargo is free to pick newer dependency
versions than `Cargo.lock` records, and you would be building different
source than the release did.

Compare the hash against the one published with the release. If it matches,
the binary is the source — no trust required.

## Why each setting is there

| Setting | Without it |
|---|---|
| Rust 1.98.0 exactly | Different compilers emit different code from identical source |
| `codegen-units = 1` | Work is split across threads; symbol ordering depends on scheduling, so the same machine can produce different bytes twice |
| `--remap-path-prefix` | The build directory is baked into panic messages, so `C:\Users\Alice\...` differs from `/home/bob/...` — and the builder's username ships inside the binary |
| `SOURCE_DATE_EPOCH` | Build timestamps embed the moment of compilation |
| `strip = true` | On the Windows GNU toolchain, import-library symbol names embed a temporary build folder's full path — username and a random suffix — which path remapping does not reach |
| `--locked` | Dependency versions drift, so you are verifying different source than was released |

## What is not done yet

Being straight about the gap, because a half-finished verification story is
worse than an absent one — it invites people to believe a check happened
that did not:

- **Nothing is signed.** There is no release key, so there is currently no
  way to confirm a published hash came from the project rather than from
  whoever served you the download page. Reproducibility proves the binary
  matches *some* source; a signature is what ties the published hash to the
  people who made it. Both are needed.
- **Reproducibility is configured, not yet proven.** The settings above are
  the known requirements, but no two-machine cross-check has actually been
  run to confirm the output is byte-identical. Until that is done, treat
  this as the intended procedure rather than a demonstrated property.
- **No release pipeline.** Spec §10 wants one pipeline building every
  target from the same commit. Builds are currently local and Windows-only.
- **The toolchain version is not enforced**, for the reason above. A real
  release pipeline would pin the full triple per platform, which is the
  right place to solve this — a build matrix already knows its target.

## Running the tests

```bash
cargo test --workspace
```

One test is ignored by default (`discovers_a_public_address_from_a_real_server`)
because it needs internet access and a reachable STUN server; it would
otherwise fail offline or behind a firewall for reasons that have nothing
to do with the code.
