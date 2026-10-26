#!/usr/bin/env bash
# Builds the Antiphon APK from source: the Rust library for each ABI, the
# few Java classes around it, the resources and the manifest, then aligns
# and signs it.
#
# This replaces `cargo apk build`. cargo-apk 0.10 writes its own manifest
# and packages no Java, and the app now needs a foreground service, a
# broadcast receiver and an activity subclass (see java/ and
# AndroidManifest.xml). Each step below is the SDK's own tool, in the order
# Gradle would run them, with nothing hidden.
#
# Usage: crates/android/build-apk.sh [--debug] [--out PATH]
#
#   --debug     an unoptimised Rust build (faster to build, slow to run)
#   --out PATH  where the signed APK goes (default target/apk/antiphon.apk)
#
# Environment:
#   ANDROID_HOME          the SDK (ANDROID_SDK_ROOT also works), with
#                         platforms;android-$TARGET_SDK and build-tools
#   ANDROID_NDK_ROOT      an NDK (ANDROID_NDK_HOME or ANDROID_NDK_LATEST_HOME
#                         also work)
#   BUILD_TOOLS_VERSION   which build-tools to use; default the newest installed
#   APK_TARGETS           Rust targets to include; default arm64 and x86_64
#   APK_KEYSTORE          keystore to sign with, and APK_KEYSTORE_PASSWORD
#                         its password; APK_KEY_ALIAS only if it holds more
#                         than one key. Without them the SDK's debug key is
#                         used (made if missing): that APK installs, but a
#                         release-signed one will not install over it.
#   SOURCE_DATE_EPOCH     timestamps written into the APK; default the last
#                         commit's time, so two builds of a commit match
#   CARGO_ENCODED_RUSTFLAGS / RUSTFLAGS   passed on, with the linker flags
#                         below added

set -euo pipefail

MIN_SDK=26
TARGET_SDK=35
PACKAGE=antiphon-android
LIB_NAME=libantiphon_android.so

die() {
    echo "build-apk: $*" >&2
    exit 1
}

PROFILE=release
OUT=
while [ $# -gt 0 ]; do
    case "$1" in
        --debug) PROFILE=debug ;;
        --out)
            [ $# -ge 2 ] || die "--out needs a path"
            OUT="$2"
            shift
            ;;
        -h | --help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
    shift
done

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
# Resolved before any cd, so a relative --out means relative to where the
# script was run from.
OUT="${OUT:-$ROOT/target/apk/antiphon.apk}"
mkdir -p "$(dirname "$OUT")"
OUT="$(cd "$(dirname "$OUT")" && pwd)/$(basename "$OUT")"

# --- Tools -----------------------------------------------------------------

SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
[ -n "$SDK" ] && [ -d "$SDK" ] || die "set ANDROID_HOME to the Android SDK"
NDK="${ANDROID_NDK_ROOT:-${ANDROID_NDK_HOME:-${ANDROID_NDK_LATEST_HOME:-}}}"
[ -n "$NDK" ] && [ -d "$NDK" ] || die "set ANDROID_NDK_ROOT to an Android NDK"

ANDROID_JAR="$SDK/platforms/android-$TARGET_SDK/android.jar"
[ -f "$ANDROID_JAR" ] || die "no $ANDROID_JAR; install it with: sdkmanager \"platforms;android-$TARGET_SDK\""

if [ -z "${BUILD_TOOLS_VERSION:-}" ]; then
    BUILD_TOOLS_VERSION="$(ls "$SDK/build-tools" 2>/dev/null | sort -V | tail -n 1 || true)"
fi
BUILD_TOOLS="$SDK/build-tools/${BUILD_TOOLS_VERSION:-none}"
[ -x "$BUILD_TOOLS/aapt2" ] || die "no build-tools in $SDK/build-tools; install with: sdkmanager \"build-tools;$TARGET_SDK.0.0\""
for tool in aapt2 d8 zipalign apksigner; do
    [ -x "$BUILD_TOOLS/$tool" ] || die "$BUILD_TOOLS has no $tool"
done
for tool in cargo javac jar keytool zip; do
    command -v "$tool" > /dev/null || die "$tool is not on PATH"
done

case "$(uname -s)" in
    Linux) HOST_TAG=linux-x86_64 ;;
    Darwin) HOST_TAG=darwin-x86_64 ;; # the NDK ships one, universal, under this name
    *) die "unsupported build host $(uname -s); build on Linux or macOS" ;;
esac
NDK_BIN="$NDK/toolchains/llvm/prebuilt/$HOST_TAG/bin"
NDK_SYSROOT_LIB="$NDK/toolchains/llvm/prebuilt/$HOST_TAG/sysroot/usr/lib"
[ -d "$NDK_BIN" ] || die "$NDK does not look like an NDK (no $NDK_BIN)"

# --- Version ---------------------------------------------------------------

# The crate's version, e.g. "0.1.0" from "path+file:///...#antiphon-android@0.1.0".
PKGID="$(cd "$ROOT" && cargo pkgid -p "$PACKAGE")"
VERSION="${PKGID##*[#@]}"
IFS=.- read -r MAJOR MINOR PATCH _ <<< "$VERSION"
for part in "$MAJOR" "$MINOR" "$PATCH"; do
    case "$part" in '' | *[!0-9]*) die "cannot read a version number from $PKGID" ;; esac
done
# cargo-apk's formula, kept so that this APK installs over the ones it
# built: Android refuses an update whose versionCode goes down.
VERSION_CODE=$(((1 << 24) | (MAJOR << 16) | (MINOR << 8) | PATCH))

if [ -z "${SOURCE_DATE_EPOCH:-}" ]; then
    SOURCE_DATE_EPOCH="$(git -C "$ROOT" log -1 --pretty=%ct 2>/dev/null || echo 315532800)"
fi
export SOURCE_DATE_EPOCH

BUILD="$ROOT/target/apk/build"
rm -rf "$BUILD"
mkdir -p "$BUILD/stage" "$BUILD/classes" "$BUILD/gen" "$BUILD/dex"

echo "build-apk: Antiphon $VERSION (versionCode $VERSION_CODE), $PROFILE, build-tools $BUILD_TOOLS_VERSION"

# --- Rust ------------------------------------------------------------------

# The flags cargo would otherwise take from RUSTFLAGS. Setting
# CARGO_ENCODED_RUSTFLAGS below makes cargo ignore RUSTFLAGS, so a plain
# RUSTFLAGS is carried over (split on spaces, as cargo does).
SEP=$'\x1f'
BASE_FLAGS="${CARGO_ENCODED_RUSTFLAGS:-}"
if [ -z "$BASE_FLAGS" ] && [ -n "${RUSTFLAGS:-}" ]; then
    read -r -a words <<< "$RUSTFLAGS"
    BASE_FLAGS="$(IFS="$SEP"; echo "${words[*]}")"
fi
unset RUSTFLAGS
# 16 KB pages: Android 15 phones may use them, and a library laid out for
# 4 KB pages will not load on one.
EXTRA_FLAGS="-Clink-arg=-Wl,-z,max-page-size=16384"
export CARGO_ENCODED_RUSTFLAGS="${BASE_FLAGS:+$BASE_FLAGS$SEP}$EXTRA_FLAGS"

CARGO_PROFILE_ARGS=()
[ "$PROFILE" = release ] && CARGO_PROFILE_ARGS=(--release)

read -r -a TARGETS <<< "${APK_TARGETS:-aarch64-linux-android x86_64-linux-android}"
for target in "${TARGETS[@]}"; do
    case "$target" in
        aarch64-linux-android) abi=arm64-v8a ;;
        armv7-linux-androideabi) abi=armeabi-v7a ;;
        x86_64-linux-android) abi=x86_64 ;;
        i686-linux-android) abi=x86 ;;
        *) die "unknown Android target $target" ;;
    esac
    # The NDK's compiler wrappers are named for the clang triple, which
    # for 32-bit ARM differs from Rust's.
    clang_triple="${target/armv7-linux-androideabi/armv7a-linux-androideabi}"
    clang="$NDK_BIN/$clang_triple$MIN_SDK-clang"
    [ -x "$clang" ] || die "the NDK has no $clang"

    # The linker for rustc, and the compilers for any build script that
    # uses the cc crate. Cargo's variable takes the target upper-cased with
    # underscores; cc's takes it as written.
    env_target="$(echo "$target" | tr 'a-z-' 'A-Z_')"
    echo "build-apk: cargo build --target $target"
    (
        cd "$ROOT"
        env \
            "CARGO_TARGET_${env_target}_LINKER=$clang" \
            "CC_$target=$clang" \
            "CXX_$target=$clang++" \
            "AR_$target=$NDK_BIN/llvm-ar" \
            cargo build --locked -p "$PACKAGE" --target "$target" ${CARGO_PROFILE_ARGS[@]+"${CARGO_PROFILE_ARGS[@]}"}
    )

    lib="$ROOT/target/$target/$PROFILE/$LIB_NAME"
    [ -f "$lib" ] || die "cargo did not produce $lib"
    mkdir -p "$BUILD/stage/lib/$abi"
    cp "$lib" "$BUILD/stage/lib/$abi/"

    # The C++ runtime is in the NDK but not on the phone, so it ships in
    # the APK if anything links it. Nothing does today -- cpal talks to
    # AAudio through the ndk crate, in Rust -- but a dependency could
    # start to.
    # Read into a variable first: with pipefail, grep -q stopping early
    # could fail the pipe while finding the match.
    dynamic="$("$NDK_BIN/llvm-readelf" --dynamic "$lib")"
    if grep -q 'libc++_shared\.so' <<< "$dynamic"; then
        cxx_triple="${target/armv7-linux-androideabi/arm-linux-androideabi}"
        cp "$NDK_SYSROOT_LIB/$cxx_triple/libc++_shared.so" "$BUILD/stage/lib/$abi/"
        echo "build-apk: $abi needs libc++_shared.so; included"
    fi
done

# --- Resources and manifest ------------------------------------------------

echo "build-apk: aapt2"
"$BUILD_TOOLS/aapt2" compile --dir "$HERE/res" -o "$BUILD/res.zip"
"$BUILD_TOOLS/aapt2" link \
    -o "$BUILD/base.apk" \
    -I "$ANDROID_JAR" \
    --manifest "$HERE/AndroidManifest.xml" \
    --min-sdk-version "$MIN_SDK" \
    --target-sdk-version "$TARGET_SDK" \
    --version-code "$VERSION_CODE" \
    --version-name "$VERSION" \
    --java "$BUILD/gen" \
    "$BUILD/res.zip"

# --- Java ------------------------------------------------------------------

echo "build-apk: javac, d8"
# Java 8 language level against android.jar as the boot class path, so
# only the platform's own classes are visible; d8 then turns the class
# files into Android's dex format, desugaring lambdas for MIN_SDK.
# Quoted one per line, so the argument file survives a space in the path.
find "$HERE/java" "$BUILD/gen" -name '*.java' | LC_ALL=C sort | sed 's/.*/"&"/' > "$BUILD/sources.txt"
javac -source 8 -target 8 -Xlint:-options -encoding UTF-8 \
    -bootclasspath "$ANDROID_JAR" \
    -d "$BUILD/classes" \
    @"$BUILD/sources.txt"
jar cf "$BUILD/classes.jar" -C "$BUILD/classes" .
D8_MODE=--release
[ "$PROFILE" = debug ] && D8_MODE=--debug
"$BUILD_TOOLS/d8" "$D8_MODE" \
    --min-api "$MIN_SDK" \
    --lib "$ANDROID_JAR" \
    --output "$BUILD/dex" \
    "$BUILD/classes.jar"
cp "$BUILD/dex/classes.dex" "$BUILD/stage/"

# --- Package, align, sign --------------------------------------------------

echo "build-apk: package"
cp "$BUILD/base.apk" "$BUILD/unaligned.apk"
# Fixed timestamps, a fixed order and no extra attributes, so the archive
# is the same bytes every time.
# GNU touch; elsewhere the build still works, it just is not reproducible.
find "$BUILD/stage" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} + 2> /dev/null ||
    echo "build-apk: could not fix file times; this APK will not be byte-for-byte reproducible" >&2
(
    cd "$BUILD/stage"
    find . -type f | sed 's|^\./||' | LC_ALL=C sort | TZ=UTC zip -X -D -q -@ "$BUILD/unaligned.apk"
)
"$BUILD_TOOLS/zipalign" -f -p 4 "$BUILD/unaligned.apk" "$BUILD/aligned.apk"

KEYSTORE="${APK_KEYSTORE:-}"
if [ -n "$KEYSTORE" ]; then
    [ -f "$KEYSTORE" ] || die "APK_KEYSTORE is set but $KEYSTORE does not exist"
    [ -n "${APK_KEYSTORE_PASSWORD:-}" ] || die "APK_KEYSTORE is set but APK_KEYSTORE_PASSWORD is not"
else
    # The SDK's conventional debug key, which Android Studio would make.
    KEYSTORE="$HOME/.android/debug.keystore"
    export APK_KEYSTORE_PASSWORD=android
    if [ ! -f "$KEYSTORE" ]; then
        mkdir -p "$(dirname "$KEYSTORE")"
        keytool -genkeypair -noprompt -keystore "$KEYSTORE" -storepass android -keypass android \
            -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Android Debug,O=Android,C=US"
    fi
    echo "build-apk: no APK_KEYSTORE; signing with the debug key $KEYSTORE" >&2
fi
SIGN_ARGS=(--ks "$KEYSTORE" --ks-pass env:APK_KEYSTORE_PASSWORD)
[ -n "${APK_KEY_ALIAS:-}" ] && SIGN_ARGS+=(--ks-key-alias "$APK_KEY_ALIAS")

echo "build-apk: apksigner"
"$BUILD_TOOLS/apksigner" sign "${SIGN_ARGS[@]}" --out "$BUILD/signed.apk" "$BUILD/aligned.apk"
"$BUILD_TOOLS/apksigner" verify "$BUILD/signed.apk"
cp "$BUILD/signed.apk" "$OUT"
echo "build-apk: $OUT"
