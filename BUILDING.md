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

Built with **Rust 1.97.0**, which is what the CI pipeline pins and what
the reproducibility check below was run with. (1.98.0 also builds it; a
*release* must use the stated version, for the reason under "Why each
setting is there".)

On Linux the audio and UI stacks need their development headers first:

```bash
sudo apt-get install libasound2-dev libxkbcommon-dev libfontconfig1-dev libdbus-1-dev
```

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

## Android

The Android app is [`crates/android`](crates/android): Rust and iced, plus
four small Java classes in [`crates/android/java`](crates/android/java) for
what only Java can receive on Android -- an activity result, a foreground
service for calls in the background, and the UnifiedPush receiver. It is
part of the workspace but compiles to nothing on a desktop, so `cargo build`
above is unaffected.

The APK is built by [`crates/android/build-apk.sh`](crates/android/build-apk.sh),
which runs the SDK's own tools in order: `cargo build` per ABI with the
NDK's clang as linker, `aapt2` for the resources and the hand-written
[`AndroidManifest.xml`](crates/android/AndroidManifest.xml), `javac` and
`d8` for the Java, then `zip`, `zipalign` and `apksigner`. (It replaced
cargo-apk, which cannot package a service or a receiver.) You need the
Android SDK with `platforms;android-35` and a build-tools, an NDK, and a
JDK (17 is what CI uses):

```bash
rustup target add aarch64-linux-android x86_64-linux-android
export ANDROID_HOME=~/Android/Sdk ANDROID_NDK_ROOT=~/Android/Sdk/ndk/<version>
crates/android/build-apk.sh                    # release; target/apk/antiphon.apk
crates/android/build-apk.sh --debug            # quicker to build, slow to run
APK_TARGETS=aarch64-linux-android crates/android/build-apk.sh   # one ABI only
adb install -r target/apk/antiphon.apk
```

Without a keystore the script signs with the SDK's debug key
(`~/.android/debug.keystore`, made if missing). To sign with a real key set
`APK_KEYSTORE` and `APK_KEYSTORE_PASSWORD` (and `APK_KEY_ALIAS` if the
keystore holds more than one key). CI does this from the
`ANDROID_KEYSTORE_BASE64` and `ANDROID_KEYSTORE_PASSWORD` repository
secrets, or with a throwaway key when they are not set -- that APK installs,
but the next one will not install over it. The versionCode follows
cargo-apk's formula from the crate version, so these APKs install over the
ones cargo-apk built. File times inside the APK are set from
`SOURCE_DATE_EPOCH` (default: the last commit's time). Logs go to logcat
under the tag `antiphon`.

Push wake-ups use [UnifiedPush](https://unifiedpush.org): install a
distributor such as ntfy on the phone and the app registers with it. There
is no FCM build; see [`docs/mobile-push.md`](docs/mobile-push.md).

Audio needs nothing Android-specific: cpal's Android backend is AAudio,
through the `ndk` crate, so there is no C++ runtime (`libc++_shared.so`) to
ship. The script checks, and adds it if a dependency ever starts needing
it. cpal does not set AAudio's voice-communication input preset, so the
platform's own echo canceller is not guaranteed to engage; the app's own
echo canceller in `antiphon-audio` is what a call relies on.

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
| Rust 1.97.0 exactly | Different compilers emit different code from identical source |
| `codegen-units = 1` | Work is split across threads; symbol ordering depends on scheduling, so the same machine can produce different bytes twice |
| `--remap-path-prefix` | The build directory is baked into panic messages, so `C:\Users\Alice\...` differs from `/home/bob/...` — and the builder's username ships inside the binary |
| `SOURCE_DATE_EPOCH` | Build timestamps embed the moment of compilation |
| `/Brepro` (MSVC linker, via `.cargo/config.toml`) | link.exe writes the current time into the PE header and a fresh GUID into the debug directory; `SOURCE_DATE_EPOCH` does not reach it. Two otherwise identical builds differed at byte 257, the header timestamp, until this was set. Any `RUSTFLAGS` you set yourself replaces the config file's flags, so include `-Clink-arg=/Brepro` in them on MSVC |
| `strip = true` | On the Windows GNU toolchain, import-library symbol names embed a temporary build folder's full path — username and a random suffix — which path remapping does not reach |
| `--locked` | Dependency versions drift, so you are verifying different source than was released |

## The pipeline

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) is spec §10's "one
pipeline building every target from the same commit":

- **Every push and pull request:** build, test and clippy on Linux,
  Windows and macOS, with the toolchain pinned per platform — which is
  the right place to pin it, since a build matrix already knows its
  target triple.
- **Every `v*` tag:** the release build above, run **twice from clean on
  each platform**. The job compares the two binaries byte for byte and
  refuses to publish anything that differs, then uploads the binaries with
  a `SHA256SUMS` file. So reproducibility is not a property someone
  asserted once; it is checked on every release, and a regression in it
  fails the build rather than shipping.

## Cross-check

Run on 2026-10-17, Linux x86_64, Rust 1.97.0, following the procedure
above exactly: a release build, `cargo clean --release`, a second release
build, and a byte comparison.

```
5f88b5256928ba0804f6e136d7159b4693343a160149ed5fdd3820fa1de54684  first/voip-cli
bde2b5ac155831d3452c08c78354c4af640795ab172b590ad1f73a68d2d1ff35  first/voip-gui
5f88b5256928ba0804f6e136d7159b4693343a160149ed5fdd3820fa1de54684  second/voip-cli
bde2b5ac155831d3452c08c78354c4af640795ab172b590ad1f73a68d2d1ff35  second/voip-gui
```

Both binaries identical. The hashes are for commit 4866782, which
introduced this section and still built the binaries under their old
names; they will differ for any other commit, what matters is that two
builds of the same commit agreed, which had not been demonstrated before.

## What is not done yet

Being straight about the gap, because a half-finished verification story is
worse than an absent one — it invites people to believe a check happened
that did not:

- **Nothing is signed.** There is no release key, so there is currently no
  way to confirm a published hash came from the project rather than from
  whoever served you the download page. Reproducibility proves the binary
  matches *some* source; a signature is what ties the published hash to the
  people who made it. Both are needed.
- **One machine per platform, not two.** The pipeline's two builds run on
  the same runner image. That rules out the non-determinism this file is
  about (symbol ordering, timestamps, paths) but not a difference between
  two differently provisioned machines. A verifier building on their own
  machine and matching the published hash is the check that closes that,
  and it is the one this procedure exists to make possible.
- **Packaging.** The pipeline produces bare binaries, not the Flatpak /
  AppImage / deb / MSI / dmg spec §10 lists.

## Running the tests

```bash
cargo test --workspace
```

Two tests are ignored by default. `discovers_a_public_address_from_a_real_server`
needs internet access and a reachable STUN server, and would otherwise fail
offline or behind a firewall for reasons that have nothing to do with the
code. `a_large_file_streams_through` moves 64 MB over loopback and is for
measuring throughput on purpose:

```bash
cargo test --release -p antiphon large_file -- --ignored --nocapture
```

Use `--release` for that number: the dev profile optimises the crypto
primitives (see `Cargo.toml`) but the rest of the receive path is still
unoptimised, and a debug figure measures the compiler, not the protocol.
