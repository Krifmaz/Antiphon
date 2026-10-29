#!/usr/bin/env bash
# Write a release version into the workspace, so `cargo build --locked` builds the app as that version
# (the About screen and --version show env!("CARGO_PKG_VERSION")).
#
#   packaging/set-version.sh 0.2.0
#
# Changes, and only changes, the version lines a release needs:
#   Cargo.toml          [workspace.package] version
#   crates/*/Cargo.toml the version requirement on a path dependency to another workspace crate
#   Cargo.lock          the version of every workspace crate
# The release workflow runs it before building; to rebuild a release yourself (BUILDING.md), run it with
# the release's version after checking out its tag. A version equal to the current one changes nothing.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
V="${1:-}"; V="${V#v}"
[[ "$V" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "usage: $0 MAJOR.MINOR.PATCH" >&2; exit 2; }

OLD=$(awk '/^\[workspace\.package\]/ { s = 1; next } /^\[/ { s = 0 } s && /^version *= *"/ { gsub(/.*= *"|".*/, ""); print; exit }' "$ROOT/Cargo.toml")
[ -n "$OLD" ] || { echo "no [workspace.package] version in Cargo.toml" >&2; exit 1; }
[ "$OLD" != "$V" ] || { echo "already $V"; exit 0; }

# the names of the workspace's own crates, from their manifests
NAMES=$(for m in "$ROOT"/crates/*/Cargo.toml; do sed -n 's/^name *= *"\([^"]*\)".*/\1/p' "$m" | head -1; done)

awk -v v="$V" '
  /^\[workspace\.package\]/ { s = 1 } /^\[/ && !/^\[workspace\.package\]/ { s = 0 }
  s && /^version *= *"/ { print "version = \"" v "\""; next } { print }' "$ROOT/Cargo.toml" > "$ROOT/Cargo.toml.new"
mv "$ROOT/Cargo.toml.new" "$ROOT/Cargo.toml"

for m in "$ROOT"/crates/*/Cargo.toml; do
  for n in $NAMES; do
    sed -i.bak -E "s/^($n *= *\{ *version *= *\")[^\"]*(\", *path)/\1$V\2/" "$m"
  done
  rm -f "$m.bak"
done

awk -v v="$V" -v names="$NAMES" '
  BEGIN { split(names, a, /[ \n]+/); for (i in a) own["name = \"" a[i] "\""] = 1 }
  hit && /^version = "/ { print "version = \"" v "\""; hit = 0; next }
  { hit = ($0 in own); print }' "$ROOT/Cargo.lock" > "$ROOT/Cargo.lock.new"
mv "$ROOT/Cargo.lock.new" "$ROOT/Cargo.lock"

echo "version $OLD -> $V"
