#!/usr/bin/env bash
# Package Stoop's release binaries for the OS this runs on, the way Encore packages its desktop app:
#   Linux    Stoop-<v>-linux-x86_64.AppImage, stoop_<v>_amd64.deb, stoop-<v>-linux-x86_64.tar.gz (the plain
#            binaries in a stoop/ folder) and Stoop-<v>-linux-x86_64.flatpak (a single-file Flatpak bundle,
#            packaging/linux/io.github.krifmaz.stoop.yml)
#   macOS    Stoop-<v>-macos-aarch64.dmg: Stoop.app, signed ad hoc
#   Windows  Stoop-<v>-windows-x86_64.exe: the NSIS installer (packaging/windows/stoop.nsi), per user under
#            %LOCALAPPDATA%\Stoop, and Stoop-<v>-windows-x86_64.msi (packaging/windows/stoop.wxs), per user
#            under %LOCALAPPDATA%\Programs\Stoop, for deploying with Group Policy, Intune or winget; run this
#            from Git Bash
# The release workflow (.github/workflows/release.yml) runs it on binaries it has already built twice and
# compared; it works the same on your own machine.
#
#   packaging/package.sh --version 0.2.0 --bin target/release
#   --bin <dir>    the folder holding the desktop app and the CLI (APP_BIN and CLI_BIN, below)
#   --out <dir>    where the files go (default: out/ at the top of the repository)
#   --only <kinds> appimage,deb,tar,flatpak on Linux, nsis,msi on Windows. Without it every kind is made,
#                  except that the Flatpak and the MSI are skipped (with a note) when their tools are missing;
#                  named in --only, a missing tool is an error (the release workflow names them all)
#
# The binaries keep their crate names until the rename lands; the packages are called Stoop.
#   APP_BIN  the desktop app's binary name (default antiphon)
#   CLI_BIN  the command-line tool's (default antiphon-cli)
#
# Needs: Linux – dpkg-deb (dpkg-shlibdeps from dpkg-dev for the Depends line), curl (appimagetool is fetched
# once into ~/.cache/stoop-packaging unless APPIMAGETOOL names one), and for the Flatpak flatpak-builder with
# org.freedesktop.Platform and .Sdk installed (FLATPAK_RUNTIME, below); macOS – the Xcode command line tools; Windows – NSIS
# (makensis), Git Bash, and for the MSI the WiX Toolset 4 command line (dotnet tool install --global wix).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG="$ROOT/packaging"
APP_BIN="${APP_BIN:-antiphon}"
CLI_BIN="${CLI_BIN:-antiphon-cli}"

die() { echo "error: $*" >&2; exit 1; }
need_value() { [ "$2" -ge 2 ] || die "$1 needs a value (see --help)"; }

V=""; BIN=""; ONLY=""; OUT="$ROOT/out"
while [ $# -gt 0 ]; do
  case "$1" in
    --version) need_value "$1" $#; V="${2#v}"; shift 2 ;;
    --bin)     need_value "$1" $#; BIN="$2"; shift 2 ;;
    --only)    need_value "$1" $#; ONLY="$2"; shift 2 ;;
    --out)     need_value "$1" $#; OUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
    *)         die "unknown option $1 (see --help)" ;;
  esac
done
# a release is MAJOR.MINOR.PATCH, a hotfix has the run number as a fourth part (0.2.1.57); NUM, the first three,
# goes where a package wants X.Y.Z (Info.plist, the installer's file version)
[[ "$V" =~ ^[0-9]+\.[0-9]+\.[0-9]+(\.[0-9]+)?$ ]] || die "--version MAJOR.MINOR.PATCH[.N] is required (got '${V}')"
NUM=$(echo "$V" | cut -d. -f1-3)
[ -n "$BIN" ] || die "--bin <dir> is required"
BIN="$(cd "$BIN" && pwd)"

run() { echo "+ $*"; "$@"; }
want() { [ -z "$ONLY" ] || [[ ",$ONLY," == *",$1,"* ]]; }
named() { [[ ",$ONLY," == *",$1,"* ]]; } # asked for by name in --only, so a missing tool is an error
skip_or_die() { # <kind> <why>: an error when --only names <kind>, otherwise a note and carry on
  if named "$1"; then die "$2"; fi
  echo "== skipping the $1 package: $2"
}

case "$(uname -s)" in
  Linux*)  OS=linux; EXE="" ;;
  Darwin*) OS=macos; EXE="" ;;
  MINGW*|MSYS*|CYGWIN*) OS=windows; EXE=".exe" ;;
  *) die "unsupported system $(uname -s)" ;;
esac
case "$(uname -m)" in
  x86_64|amd64)  ARCH=x86_64; DEBARCH=amd64 ;;
  aarch64|arm64) ARCH=aarch64; DEBARCH=arm64 ;;
  *) die "unsupported machine $(uname -m)" ;;
esac

APP="$BIN/$APP_BIN$EXE"; CLI="$BIN/$CLI_BIN$EXE"
[ -f "$APP" ] || die "no desktop app at $APP"
[ -f "$CLI" ] || die "no command-line tool at $CLI"
echo "Stoop $V ($OS $ARCH) from $BIN"
mkdir -p "$OUT"; OUT="$(cd "$OUT" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# ---- Linux ---------------------------------------------------------------------------------------
icons_into() { # <dir>: hicolor icons named stoop.png
  local s
  for s in 32 64 128 256 512; do
    install -Dm644 "$PKG/icons/${s}x${s}.png" "$1/usr/share/icons/hicolor/${s}x${s}/apps/stoop.png"
  done
  install -Dm644 "$PKG/icons/stoop.svg" "$1/usr/share/icons/hicolor/scalable/apps/stoop.svg"
}

appimagetool_path() {
  if [ -n "${APPIMAGETOOL:-}" ]; then echo "$APPIMAGETOOL"; return; fi
  if command -v appimagetool >/dev/null; then command -v appimagetool; return; fi
  local cache="${XDG_CACHE_HOME:-$HOME/.cache}/stoop-packaging"
  local f="$cache/appimagetool-$ARCH.AppImage"
  local url="${APPIMAGETOOL_URL:-https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage}"
  if [ ! -x "$f" ]; then
    mkdir -p "$cache"
    echo "+ curl -fsSL -o $f $url" >&2
    curl -fsSL --retry 3 -o "$f.part" "$url" || die "could not download appimagetool from $url"
    chmod +x "$f.part" && mv "$f.part" "$f"
  fi
  echo "$f"
}

appimage() {
  local dir="$WORK/Stoop.AppDir" out="$OUT/Stoop-$V-linux-$ARCH.AppImage" tool
  echo "== AppImage → $out"
  install -Dm755 "$APP" "$dir/usr/bin/$APP_BIN"
  install -Dm755 "$CLI" "$dir/usr/bin/$CLI_BIN"
  sed "s/@APP_BIN@/$APP_BIN/g" "$PKG/linux/stoop.desktop" > "$WORK/stoop.desktop"
  install -Dm644 "$WORK/stoop.desktop" "$dir/usr/share/applications/stoop.desktop"
  icons_into "$dir"
  cp "$WORK/stoop.desktop" "$dir/stoop.desktop"
  cp "$PKG/icons/256x256.png" "$dir/stoop.png"
  ln -s stoop.png "$dir/.DirIcon"
  cat > "$dir/AppRun" <<EOF
#!/bin/sh
# The AppImage's entry point: Stoop from the mounted image.
HERE="\$(dirname "\$(readlink -f "\$0")")"
exec "\$HERE/usr/bin/$APP_BIN" "\$@"
EOF
  chmod 755 "$dir/AppRun"
  chmod -R u+rwX,go+rX,go-w "$dir"
  tool="$(appimagetool_path)"
  # APPIMAGE_EXTRACT_AND_RUN: the tool runs without FUSE (CI runners and containers have none)
  rm -f "$out"
  run env ARCH="$ARCH" APPIMAGE_EXTRACT_AND_RUN=1 "$tool" --no-appstream "$dir" "$out"
  [ -s "$out" ] || die "appimagetool made no $out"
  chmod 755 "$out"
}

deb_depends() { # <binary>: the shared libraries it links (dpkg-shlibdeps), plus what winit loads at run time
  local d deps=""
  if command -v dpkg-shlibdeps >/dev/null; then
    d="$(mktemp -d)"
    mkdir -p "$d/debian"
    printf 'Source: stoop\n\nPackage: stoop\nArchitecture: any\n' > "$d/debian/control"
    deps=$(cd "$d" && dpkg-shlibdeps -O --ignore-missing-info -e"$1" 2>/dev/null | sed -n 's/^shlibs:Depends=//p') || deps=""
    rm -rf "$d"
  fi
  [ -n "$deps" ] || deps="libc6 (>= 2.35), libgcc-s1, libasound2"
  # the keyboard libraries are opened with dlopen, so dpkg-shlibdeps cannot see them; without them the window
  # does not open
  echo "$deps, libxkbcommon0, libxkbcommon-x11-0"
}

deb() {
  local root="$WORK/deb" out="$OUT/stoop_${V}_${DEBARCH}.deb" size
  echo "== .deb → $out"
  command -v dpkg-deb >/dev/null || die "dpkg-deb not found (Debian/Ubuntu: it comes with dpkg)"
  install -Dm755 "$APP" "$root/usr/bin/$APP_BIN"
  install -Dm755 "$CLI" "$root/usr/bin/$CLI_BIN"
  sed "s/@APP_BIN@/$APP_BIN/g" "$PKG/linux/stoop.desktop" > "$WORK/stoop.desktop"
  install -Dm644 "$WORK/stoop.desktop" "$root/usr/share/applications/stoop.desktop"
  icons_into "$root"
  install -Dm644 "$ROOT/README.md" "$root/usr/share/doc/stoop/README.md"
  mkdir -p "$root/DEBIAN"
  chmod -R u+rwX,go+rX,go-w "$root"
  size=$(du -sk "$root/usr" | cut -f1)
  cat > "$root/DEBIAN/control" <<EOF
Package: stoop
Version: $V
Architecture: $DEBARCH
Maintainer: Stoop <Krifmaz@users.noreply.github.com>
Installed-Size: $size
Depends: $(deb_depends "$root/usr/bin/$APP_BIN")
Section: net
Priority: optional
Homepage: https://github.com/Krifmaz/Antiphon
Description: Stoop: direct, end-to-end encrypted voice calls and chat
 Two peers, no server in the call path: encrypted voice with a full
 microphone processing chain, text chat and file transfer over the same
 connection. Includes the desktop app and the command-line tool.
EOF
  # xz, not the zstd of newer dpkg: every Debian and Ubuntu still in use reads it
  run dpkg-deb --root-owner-group -Zxz --build "$root" "$out"
  dpkg-deb --info "$out" | sed -n '/Package:/,/Description:/p'
}

tarball() {
  local dir="$WORK/tar/stoop" out="$OUT/stoop-$V-linux-$ARCH.tar.gz"
  echo "== tarball → $out"
  install -Dm755 "$APP" "$dir/$APP_BIN"
  install -Dm755 "$CLI" "$dir/$CLI_BIN"
  install -Dm644 "$ROOT/README.md" "$dir/README.md"
  run tar -C "$WORK/tar" --owner=0 --group=0 -czf "$out" stoop
}

# The Flatpak: the same two binaries in org.freedesktop.Platform, built by flatpak-builder from the manifest
# and exported as a single-file bundle (flatpak install --user Stoop-<v>-linux-x86_64.flatpak). FLATPAK_RUNTIME
# names the runtime branch (default 25.08); it needs a glibc at least as new as the one the binaries were built
# against, which is why the release workflow picks the newest. The app id
# names the desktop file and icons, as Flatpak requires. FLATPAK_CHECK=1 then installs the bundle for this
# user and fails if the app is missing a shared library inside the runtime (the release workflow does this).
flatpak_bundle() {
  local id=io.github.krifmaz.stoop src="$WORK/flatpak" out="$OUT/Stoop-$V-linux-$ARCH.flatpak" s
  echo "== Flatpak → $out"
  command -v flatpak-builder >/dev/null || { skip_or_die flatpak "flatpak-builder not found"; return 0; }
  mkdir -p "$src/stage"
  install -Dm755 "$APP" "$src/stage/$APP_BIN"
  install -Dm755 "$CLI" "$src/stage/$CLI_BIN"
  sed -e "s/@APP_BIN@/$APP_BIN/g" -e "s/^Icon=stoop$/Icon=$id/" "$PKG/linux/stoop.desktop" > "$src/stage/$id.desktop"
  for s in 32 64 128 256 512; do cp "$PKG/icons/${s}x${s}.png" "$src/stage/icon-$s.png"; done
  cp "$PKG/icons/stoop.svg" "$src/stage/icon.svg"
  sed -e "s/@APP_BIN@/$APP_BIN/g" -e "s/@CLI_BIN@/$CLI_BIN/g" -e "s/@RUNTIME@/${FLATPAK_RUNTIME:-25.08}/g" \
    "$PKG/linux/$id.yml" > "$src/$id.yml"
  rm -f "$out"
  run flatpak-builder --user --force-clean --disable-rofiles-fuse --repo="$src/repo" "$src/build" "$src/$id.yml"
  run flatpak build-bundle "$src/repo" "$out" "$id"
  [ -s "$out" ] || die "flatpak build-bundle made no $out"
  if [ "${FLATPAK_CHECK:-}" = 1 ]; then
    run flatpak install --user -y --noninteractive --reinstall "$out"
    local libs missing dbus=()
    command -v dbus-run-session >/dev/null && dbus=(dbus-run-session --) # flatpak run wants a session bus
    libs=$("${dbus[@]}" flatpak run --command=ldd "$id" "/app/bin/$APP_BIN" "/app/bin/$CLI_BIN") \
      || die "could not run ldd inside the Flatpak"
    missing=$(grep 'not found' <<< "$libs" || true)
    [ -z "$missing" ] || die "the Flatpak is missing libraries the runtime does not have:"$'\n'"$missing"
    echo "every library the app links is in the runtime"
  fi
}

linux() {
  if want appimage; then appimage; fi
  if want deb; then deb; fi
  if want tar; then tarball; fi
  if want flatpak; then flatpak_bundle; fi
}

# ---- macOS ---------------------------------------------------------------------------------------
macos() {
  local app="$WORK/Stoop.app" stage="$WORK/dmg" icons="$WORK/stoop.iconset" s
  local dmg="$OUT/Stoop-$V-macos-$ARCH.dmg"
  echo "== Stoop.app → $dmg"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$icons"
  cp "$APP" "$app/Contents/MacOS/$APP_BIN"
  cp "$CLI" "$app/Contents/MacOS/$CLI_BIN"
  chmod 755 "$app/Contents/MacOS/"*
  sed -e "s/@VERSION@/$NUM/g" -e "s/@APP_BIN@/$APP_BIN/g" -e "s/@MIN_MACOS@/${MACOSX_DEPLOYMENT_TARGET:-11.0}/g" \
    "$PKG/macos/Info.plist" > "$app/Contents/Info.plist"
  printf 'APPL????' > "$app/Contents/PkgInfo"
  # the .icns from the PNGs: iconutil wants each size and its @2x
  sips -z 16 16 "$PKG/icons/32x32.png" --out "$icons/icon_16x16.png" >/dev/null
  cp "$PKG/icons/32x32.png"   "$icons/icon_16x16@2x.png"
  cp "$PKG/icons/32x32.png"   "$icons/icon_32x32.png"
  cp "$PKG/icons/64x64.png"   "$icons/icon_32x32@2x.png"
  cp "$PKG/icons/128x128.png" "$icons/icon_128x128.png"
  cp "$PKG/icons/256x256.png" "$icons/icon_128x128@2x.png"
  cp "$PKG/icons/256x256.png" "$icons/icon_256x256.png"
  cp "$PKG/icons/512x512.png" "$icons/icon_256x256@2x.png"
  cp "$PKG/icons/512x512.png" "$icons/icon_512x512.png"
  run iconutil -c icns -o "$app/Contents/Resources/stoop.icns" "$icons"
  plutil -lint "$app/Contents/Info.plist"
  echo "Signed ad hoc (the first start needs System Settings → Privacy & Security → Open Anyway)"
  run codesign --force --deep --sign - "$app"
  run codesign --verify --strict --verbose=2 "$app"
  mkdir -p "$stage"
  cp -R "$app" "$stage/"
  ln -s /Applications "$stage/Applications"
  rm -f "$dmg"
  run hdiutil create -volname "Stoop" -srcfolder "$stage" -ov -format UDZO "$dmg"
}

# ---- Windows -------------------------------------------------------------------------------------
winpath() { if command -v cygpath >/dev/null; then cygpath -w "$1"; else echo "$1"; fi; }

find_tool() { # <name> <path>…: on the PATH, else the last of the paths that exists
  local name="$1" f found=""; shift
  if command -v "$name" >/dev/null; then command -v "$name"; return; fi
  for f in "$@"; do [ -f "$f" ] && found="$f"; done
  [ -n "$found" ] || return 1
  echo "$found"
}

# The MSI (WiX Toolset 4): the same files, per user, for whoever deploys software with Group Policy, Intune or
# winget rather than by running an installer. The product version is $V; Windows Installer compares only the
# first three parts, so a hotfix (0.2.1.57) replaces the build of the same release before it.
msi() {
  local out="$OUT/Stoop-$V-windows-$ARCH.msi" wix
  echo "== MSI → $out"
  wix=$(find_tool wix "$HOME/.dotnet/tools/wix.exe") \
    || { skip_or_die msi "wix not found (dotnet tool install --global wix --version 4.0.6)"; return 0; }
  rm -f "$out"
  MSYS2_ARG_CONV_EXCL='*' run "$wix" build -arch x64 -nologo \
    -d "Version=$V" -d "AppExe=$(winpath "$APP")" -d "AppName=$APP_BIN.exe" \
    -d "CliExe=$(winpath "$CLI")" -d "CliName=$CLI_BIN.exe" \
    -d "Readme=$(winpath "$ROOT/README.md")" -d "Icon=$(winpath "$PKG/icons/stoop.ico")" \
    -o "$(winpath "$out")" "$(winpath "$PKG/windows/stoop.wxs")"
  [ -s "$out" ] || die "wix made no $out"
  rm -f "${out%.msi}.wixpdb"
}

windows() {
  if want msi; then msi; fi
  if want nsis; then nsis; fi
}

nsis() {
  local out="$OUT/Stoop-$V-windows-$ARCH.exe" makensis
  echo "== installer → $out"
  makensis=$(find_tool makensis "/c/Program Files (x86)/NSIS/makensis.exe" "/c/Program Files/NSIS/makensis.exe") \
    || die "makensis not found: install NSIS (https://nsis.sourceforge.io, or: choco install nsis)"
  rm -f "$out"
  # MSYS2_ARG_CONV_EXCL: Git Bash must not turn the -D values into paths
  MSYS2_ARG_CONV_EXCL='*' run "$makensis" -V2 "-DVERSION=$V" "-DNUMVER=$NUM" \
    "-DAPP_EXE=$(winpath "$APP")" "-DAPP_NAME=$APP_BIN.exe" \
    "-DCLI_EXE=$(winpath "$CLI")" "-DCLI_NAME=$CLI_BIN.exe" \
    "-DREADME=$(winpath "$ROOT/README.md")" "-DICON=$(winpath "$PKG/icons/stoop.ico")" \
    "-DOUTFILE=$(winpath "$out")" "$(winpath "$PKG/windows/stoop.nsi")"
  [ -s "$out" ] || die "makensis made no $out"
}

"$OS"
echo "== made"
ls -la "$OUT"
