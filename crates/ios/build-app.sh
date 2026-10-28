#!/usr/bin/env bash
# Builds Antiphon.app for the iOS simulator or for devices, from cargo
# alone: no Xcode project. Needs a Mac with Xcode's command line tools
# (the iOS SDK, for linking) and the Rust targets:
#
#   rustup target add aarch64-apple-ios aarch64-apple-ios-sim
#   crates/ios/build-app.sh sim      # -> target/aarch64-apple-ios-sim/release/Antiphon.app
#   crates/ios/build-app.sh device   # -> target/aarch64-apple-ios/release/Antiphon.app
#
# A simulator build is signed ad hoc, which is all the simulator asks for.
# A device build is unsigned unless signing details are given, because
# installing on an iPhone needs an Apple developer account's certificate
# and a provisioning profile naming org.antiphon.app (with the Push
# Notifications capability, for PushKit). Either:
#
#   IOS_CERTIFICATE_P12        base64 of the .p12 (certificate and key)
#   IOS_CERTIFICATE_PASSWORD   its password
#   IOS_PROVISIONING_PROFILE   base64 of the .mobileprovision
#
# (what CI uses, from repository secrets), or, on a Mac whose keychain
# already holds the certificate:
#
#   IOS_SIGN_IDENTITY              e.g. "Apple Development: Name (TEAMID)"
#   IOS_PROVISIONING_PROFILE_PATH  path to the .mobileprovision
#
# A signed device build also produces Antiphon.ipa beside the .app.

set -euo pipefail

kind="${1:-sim}"
case "$kind" in
  sim) target=aarch64-apple-ios-sim; platform=iPhoneSimulator ;;
  device) target=aarch64-apple-ios; platform=iPhoneOS ;;
  *) echo "usage: $0 [sim|device]" >&2; exit 2 ;;
esac

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
cd "$root"

# Matches MinimumOSVersion in Info.plist. Without it rustc links for its
# own default, and the linker warns about every object built for another.
export IPHONEOS_DEPLOYMENT_TARGET=15.0

cargo build -p antiphon-ios --release --locked --target "$target"

# The workspace version for people, the commit count for iOS, which wants
# a number that only goes up.
version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' Cargo.toml)"
build="$(git rev-list --count HEAD 2>/dev/null || echo 1)"
platform_lower="$(echo "$platform" | tr '[:upper:]' '[:lower:]')"

out="$root/target/$target/release"
app="$out/Antiphon.app"
rm -rf "$app" "$out/Antiphon.ipa"
mkdir -p "$app"
cp "$out/antiphon-ios" "$app/antiphon-ios"
sed -e "s/@VERSION@/$version/" -e "s/@BUILD@/$build/" \
    -e "s/@PLATFORM@/$platform/" -e "s/@PLATFORM_LOWER@/$platform_lower/" \
    "$here/Info.plist" > "$app/Info.plist"
cp "$here"/res/AppIcon*.png "$app/"
# iOS reads a binary plist faster; plutil also catches a malformed one
# here rather than as a refusal to install.
if command -v plutil >/dev/null; then
  plutil -convert binary1 "$app/Info.plist"
fi
printf 'APPL????' > "$app/PkgInfo"

sign_with() {
  local identity="$1" profile="$2" work
  work="$(mktemp -d)"
  cp "$profile" "$app/embedded.mobileprovision"
  # The entitlements are the profile's own (application identifier, team,
  # aps-environment), so they cannot disagree with it.
  security cms -D -i "$profile" > "$work/profile.plist"
  /usr/libexec/PlistBuddy -x -c 'Print :Entitlements' "$work/profile.plist" > "$work/entitlements.plist"
  codesign --force --timestamp=none --sign "$identity" --entitlements "$work/entitlements.plist" "$app"
  rm -rf "$work"
  (cd "$out" && rm -rf Payload && mkdir Payload && cp -R Antiphon.app Payload/ && zip -qry Antiphon.ipa Payload && rm -rf Payload)
  echo "signed: $app and $out/Antiphon.ipa"
}

if [ "$kind" = device ] && [ -n "${IOS_CERTIFICATE_P12:-}" ] && [ -n "${IOS_PROVISIONING_PROFILE:-}" ]; then
  # A throwaway keychain, so CI never touches the login keychain and the
  # key is gone when this script ends.
  tmp="$(mktemp -d)"
  keychain="$tmp/antiphon-signing.keychain-db"
  kc_pass="$(openssl rand -hex 16)"
  trap 'security delete-keychain "$keychain" 2>/dev/null || true; rm -rf "$tmp"' EXIT
  echo "$IOS_CERTIFICATE_P12" | base64 --decode > "$tmp/cert.p12"
  echo "$IOS_PROVISIONING_PROFILE" | base64 --decode > "$tmp/profile.mobileprovision"
  security create-keychain -p "$kc_pass" "$keychain"
  security set-keychain-settings -lut 3600 "$keychain"
  security unlock-keychain -p "$kc_pass" "$keychain"
  security import "$tmp/cert.p12" -k "$keychain" -P "${IOS_CERTIFICATE_PASSWORD:-}" -T /usr/bin/codesign
  security set-key-partition-list -S apple-tool:,apple: -s -k "$kc_pass" "$keychain" > /dev/null
  security list-keychains -d user -s "$keychain" $(security list-keychains -d user | tr -d '"')
  identity="$(security find-identity -v -p codesigning "$keychain" | awk 'NR==1 {print $2}')"
  sign_with "$identity" "$tmp/profile.mobileprovision"
elif [ "$kind" = device ] && [ -n "${IOS_SIGN_IDENTITY:-}" ] && [ -n "${IOS_PROVISIONING_PROFILE_PATH:-}" ]; then
  sign_with "$IOS_SIGN_IDENTITY" "$IOS_PROVISIONING_PROFILE_PATH"
elif [ "$kind" = sim ] && command -v codesign >/dev/null; then
  codesign --force --timestamp=none --sign - "$app"
  echo "built: $app (ad hoc signed, for the simulator)"
else
  echo "built: $app (unsigned; see BUILDING.md to sign it for a device)"
fi
