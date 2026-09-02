#!/usr/bin/env bash
#
# Build and install MDM (My Download Manager) for the current user. Nothing here needs root; the
# system packages it depends on are checked for, not installed.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${HOME}/.local/bin"
APP_DIR="${HOME}/.local/share/applications"
ICON_DIR="${HOME}/.local/share/icons/hicolor"
NM_DIR="${HOME}/.mozilla/native-messaging-hosts"
EXT_ID="ldm@ramlej.local"
HOST_NAME="io.ldm.host"

say()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m warning:\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m error:\033[0m %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- deps

say "Checking dependencies"
missing=()
for cmd in aria2c cargo; do
  command -v "$cmd" >/dev/null || missing+=("$cmd")
done
if (( ${#missing[@]} )); then
  die "missing required commands: ${missing[*]}
  Install them with:
    sudo dnf install aria2 rust cargo"
fi

for cmd in yt-dlp ffmpeg; do
  command -v "$cmd" >/dev/null || warn "$cmd not found — video extraction will be unavailable"
done

# YouTube obfuscates its stream URLs behind a JavaScript challenge. Without a
# runtime to solve it, yt-dlp drops every format and reports "The page needs to
# be reloaded" — which sends people reloading a page that was never at fault.
if command -v yt-dlp >/dev/null; then
  js_runtime=""
  for cmd in deno node qjs bun; do
    command -v "$cmd" >/dev/null && { js_runtime="$cmd"; break; }
  done
  if [[ -z "$js_runtime" ]]; then
    warn "no JavaScript runtime found — YouTube downloads will fail with
  \"The page needs to be reloaded\". Install one with:
    sudo dnf install nodejs        (deno, bun and quickjs also work)"
  fi
fi
command -v notify-send >/dev/null || warn "notify-send not found — desktop notifications disabled"

# ---------------------------------------------------------------- build

say "Building (release)"
cd "$REPO"
cargo build --release --workspace

APP_BIN="$REPO/target/release/ldm"
HOST_BIN="$REPO/target/release/ldm-host"
[[ -x "$APP_BIN"  ]] || die "build did not produce $APP_BIN"
[[ -x "$HOST_BIN" ]] || die "build did not produce $HOST_BIN"

# ---------------------------------------------------------------- install

say "Installing binaries to $BIN_DIR"
mkdir -p "$BIN_DIR"
install -m755 "$APP_BIN"  "$BIN_DIR/ldm"
install -m755 "$HOST_BIN" "$BIN_DIR/ldm-host"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "$BIN_DIR is not on your PATH; add it to ~/.bashrc" ;;
esac

# Under both names on purpose: desktop entries name their icon explicitly,
# while some shells instead guess it from the window's app id.
say "Installing icons"
for size in 16 24 32 48 64 128 256 512; do
  dir="$ICON_DIR/${size}x${size}/apps"
  mkdir -p "$dir"
  install -m644 "$REPO/extension/icons/ldm-${size}.png" "$dir/io.ldm.app.png"
  install -m644 "$REPO/extension/icons/ldm-${size}.png" "$dir/ldm.png"
done
command -v gtk-update-icon-cache >/dev/null && \
  gtk-update-icon-cache -qtf "$ICON_DIR" 2>/dev/null || true

say "Installing desktop entry"
mkdir -p "$APP_DIR"
# Name is the short one on purpose: it is what the launcher shows and what
# gets typed to find it. The long name lives in GenericName, and Keywords make
# the app findable by what it does as well as by what it is called.
#
# StartupWMClass must equal the window's GTK app id — the "identifier" from
# tauri.conf.json, which "enableGTKAppId" is what actually applies. Without
# that pair agreeing, the desktop cannot tell which entry the window belongs to
# and shows a generic placeholder in the taskbar instead of the icon above.
cat > "$APP_DIR/io.ldm.app.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=MDM
GenericName=My Download Manager
Comment=My Download Manager — accelerated downloads with browser capture
Exec=$BIN_DIR/ldm %u
Icon=io.ldm.app
Terminal=false
Categories=Network;FileTransfer;
Keywords=mdm;my download manager;download;manager;downloader;aria2;idm;video;
StartupWMClass=io.ldm.app
MimeType=x-scheme-handler/ldm;
DESKTOP
command -v update-desktop-database >/dev/null && \
  update-desktop-database -q "$APP_DIR" 2>/dev/null || true

# ------------------------------------------------- native messaging host

say "Registering the native messaging host for Firefox"
mkdir -p "$NM_DIR"
cat > "$NM_DIR/${HOST_NAME}.json" <<MANIFEST
{
  "name": "${HOST_NAME}",
  "description": "My Download Manager native host",
  "path": "${BIN_DIR}/ldm-host",
  "type": "stdio",
  "allowed_extensions": ["${EXT_ID}"]
}
MANIFEST

# Flatpak Firefox reads a different tree and cannot execute host binaries from
# ~/.local/bin without an override, so point it out rather than failing later.
if [[ -d "$HOME/.var/app/org.mozilla.firefox" ]]; then
  warn "Flatpak Firefox detected. It sandboxes native messaging; run:
    flatpak override --user --filesystem=home org.mozilla.firefox
  and copy ${NM_DIR}/${HOST_NAME}.json into
    ~/.var/app/org.mozilla.firefox/.mozilla/native-messaging-hosts/"
fi

# ---------------------------------------------------------------- extension

say "Packaging the extension"
XPI="$REPO/target/ldm-firefox.xpi"
# The test directory is developer-only; shipping it would put dead code in
# front of AMO reviewers and bloat the package.
( cd "$REPO/extension" && rm -f "$XPI" && zip -qr "$XPI" . -x '*.DS_Store' 'test/*' )

cat <<DONE

$(say "Installed")

  App           $BIN_DIR/ldm
  Native host   $BIN_DIR/ldm-host
  Host manifest $NM_DIR/${HOST_NAME}.json
  Extension     $XPI

Load the extension in Firefox:

  1. Open  about:debugging#/runtime/this-firefox
  2. Click "Load Temporary Add-on…"
  3. Select  $REPO/extension/manifest.json

  Temporary add-ons are removed when Firefox restarts. To install it
  permanently, Firefox requires a signed package: submit $XPI to
  addons.mozilla.org (self-distribution signing is free and unlisted),
  or use Firefox Developer Edition with xpinstall.signatures.required=false.

Then start the app:  ldm
The extension launches it automatically on the first captured download.
DONE
