#!/usr/bin/env bash
#
# Remove the MDM install that install.sh made.
#
#   ./uninstall.sh           remove the app, keep downloads and settings
#   ./uninstall.sh --purge   remove those too
#
# Only ever an install.sh install. A machine set up from the .rpm or .deb is
# owned by its package manager and is removed with that instead -- this script
# says so rather than reaching into /usr, and the check is not cosmetic: the
# two installs write native messaging manifests to different places, and a
# remover that deleted both would leave the packaged install unable to reach
# its own host binary.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=packaging/common.sh
source "$REPO/packaging/common.sh"

BIN_DIR="${HOME}/.local/bin"
APP_DIR="${HOME}/.local/share/applications"
ICON_DIR="${HOME}/.local/share/icons/hicolor"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/mdm"

PURGE=no
while (( $# )); do
  case "$1" in
    --purge) PURGE=yes ;;
    -h|--help)
      cat <<'USAGE'
Usage: uninstall.sh [--purge]

Removes what install.sh installed: the binaries in ~/.local/bin, the launcher
entry, the icons, and the native messaging manifests that point at them.

  --purge    also delete ~/.local/share/mdm — the downloads database,
             settings.toml, and the yt-dlp and QuickJS binaries the app
             fetched for itself. Files you have already downloaded are
             somewhere else entirely and are never touched.

Installed from the .rpm or .deb instead? That one belongs to your package
manager:

  sudo dnf remove my-download-manager
  sudo apt remove my-download-manager
USAGE
      exit 0 ;;
    *) die "unknown option: $1 (try --help)" ;;
  esac
  shift
done

removed=()
kept=()

# ------------------------------------------------------------------ packaged

# Said before anything is deleted, because it changes what the answer is. A
# packaged install is the far more likely thing to be sitting here when this
# script finds nothing of its own to remove, and "uninstalled" over a machine
# that still has MDM on it is the wrong thing to print.
packaged=
for query in "rpm -q my-download-manager" "dpkg-query -W -f=x my-download-manager"; do
  $query >/dev/null 2>&1 && { packaged=yes; break; }
done

# ----------------------------------------------------------------- processes

# The app has to be stopped before its binary is removed, and the host has to go
# with it: a running browser restarts the host the moment it dies, and the host
# launches the app again beside itself. stop.sh knows that order and the reasons
# for it, so it is called rather than reimplemented.
#
# Not fatal if it fails. A browser holding the add-on open will start MDM again
# whatever this script does, and the files still need to come off -- what is
# left running is a binary that has already been deleted, which exits with the
# browser.
if [[ -x "$REPO/stop.sh" ]]; then
  say "Stopping the app"
  "$REPO/stop.sh" --quiet || warn "MDM did not stop cleanly. The files below still
  come off; close your browser if it starts MDM again afterwards."
fi

# ------------------------------------------------------------------ binaries

# Named individually rather than by pattern: ~/.local/bin is the user's own
# directory and holds other people's programs.
say "Removing the binaries"
for bin in mdm mdm-host; do
  if [[ -e "$BIN_DIR/$bin" ]]; then
    rm -f "$BIN_DIR/$bin"
    removed+=("$BIN_DIR/$bin")
  fi
done
# The name this app had before the rename. install.sh clears these on its way
# past; a machine that never ran it again still has them.
rm -f "$BIN_DIR/ldm" "$BIN_DIR/ldm-host"

# ------------------------------------------------------------- desktop entry

say "Removing the launcher entry and icons"
# "My Download Manager.desktop" is what install.sh writes now -- it takes the
# packaged entry's name so the two shadow each other rather than both showing;
# see the note there. mdm.desktop is what it wrote before that, and the other two
# before the app id settled, and a machine that has not re-run it still has one.
for entry in "$APP_DIR/My Download Manager.desktop" "$APP_DIR/mdm.desktop" \
             "$APP_DIR/io.mdm.app.desktop" "$APP_DIR/io.ldm.app.desktop"; do
  if [[ -e "$entry" ]]; then
    rm -f "$entry"
    removed+=("$entry")
  fi
done
command -v update-desktop-database >/dev/null && \
  update-desktop-database -q "$APP_DIR" 2>/dev/null || true

# Exactly the sizes and the two names install.sh writes.
icons=0
for size in 16 24 32 48 64 128 256 512; do
  for name in io.mdm.app mdm io.ldm.app ldm; do
    icon="$ICON_DIR/${size}x${size}/apps/${name}.png"
    [[ -e "$icon" ]] && { rm -f "$icon"; icons=$(( icons + 1 )); }
  done
done
(( icons )) && removed+=("$icons icon files under $ICON_DIR")
command -v gtk-update-icon-cache >/dev/null && \
  gtk-update-icon-cache -qtf "$ICON_DIR" 2>/dev/null || true

# ------------------------------------------------- native messaging manifests

# Only the manifests that point back into BIN_DIR.
#
# This is the whole reason this script does not simply delete every
# io.mdm.host.json it can find. A packaged install writes its own manifests
# --- system-wide, under /etc, naming /usr/bin/mdm-host --- and a user may have
# hand-written one besides. Both are somebody else's file describing a host
# binary this script is not removing, and deleting them would break an install
# that is still perfectly good. The manifest's own `path` says which is which.
say "Removing the native messaging manifests"
while read -r nm_dir; do
  manifest="$nm_dir/${HOST_NAME}.json"
  [[ -f "$manifest" ]] || continue
  # -F, so a home directory with a regex character in its name cannot make this
  # match a manifest belonging to someone else. The path appears in the file
  # only as the value of "path", whatever the spacing around the colon.
  if grep -qF "\"${BIN_DIR}/mdm-host\"" "$manifest"; then
    rm -f "$manifest"
    removed+=("$manifest")
  else
    kept+=("$manifest (points somewhere other than $BIN_DIR)")
  fi
  # The pre-rename host, which nothing else could ever own.
  rm -f "$nm_dir/io.ldm.host.json"
done < <(firefox_manifest_dirs; chromium_manifest_dirs)

# ----------------------------------------------------------------- extension

# The signed package install.sh put aside so a re-install or a repaired profile
# could go back to it. It goes whether or not the data is being kept: it is an
# install artefact, not something the user made.
if [[ -e "$DATA_DIR/mdm-firefox.xpi" ]]; then
  rm -f "$DATA_DIR/mdm-firefox.xpi"
  removed+=("$DATA_DIR/mdm-firefox.xpi")
fi

# The Chromium copy, for the same reason. It matters more than the .xpi does:
# Firefox copies an add-on into the profile as it installs it, so removing this
# file takes nothing away from a running browser -- but Chromium reads an
# unpacked extension's folder every time it starts. So this one is still in use
# by whatever browser it was loaded into, and leaving it would leave an
# extension that outlives the app it talks to.
if [[ -d "$DATA_DIR/mdm-chrome" ]]; then
  rm -rf "$DATA_DIR/mdm-chrome"
  removed+=("$DATA_DIR/mdm-chrome")
  warn "if you loaded the extension into a Chromium browser, remove it there too
  — it was loaded from the folder just deleted, and the browser will report it
  as missing or corrupted until you do."
fi

# ----------------------------------------------------------------- user data

if [[ "$PURGE" == yes ]]; then
  if [[ -d "$DATA_DIR" ]]; then
    say "Removing $DATA_DIR"
    rm -rf "$DATA_DIR"
    removed+=("$DATA_DIR")
  fi
  # Settings and cache live under their own roots, not inside DATA_DIR; the
  # webview keeps its own store beside them, named after the bundle identifier
  # from tauri.conf.json rather than after the app. Both identifiers this app
  # has had are listed: it holds nothing but WebKit caches, so an old one is
  # only ever litter, and after the rename off `io.mdm.app` nothing else would
  # ever look at it again.
  for dir in "${XDG_CONFIG_HOME:-$HOME/.config}/mdm" \
             "${XDG_CACHE_HOME:-$HOME/.cache}/mdm" \
             "${XDG_DATA_HOME:-$HOME/.local/share}/io.github.orapagier.mdm" \
             "${XDG_DATA_HOME:-$HOME/.local/share}/io.mdm.app"; do
    if [[ -d "$dir" ]]; then
      rm -rf "$dir"
      removed+=("$dir")
    fi
  done
fi

# The runtime directory holds only the socket, which means nothing once the app
# is gone; left behind it suggests MDM is still up.
#
# Only when this script actually removed an install, though. The socket is not
# owned by whoever installed the binary -- a packaged MDM running right now is
# listening on that same path, and taking it out from under a program this
# script is deliberately not removing would break it for no reason at all.
if (( ${#removed[@]} )); then
  runtime="${XDG_RUNTIME_DIR:+$XDG_RUNTIME_DIR/mdm}"
  rm -rf "${runtime:-${TMPDIR:-/tmp}/mdm-$(id -u)}" 2>/dev/null || true
fi

# ------------------------------------------------------------------- report

echo
if (( ${#removed[@]} )); then
  say "Uninstalled"
  echo
  for item in "${removed[@]}"; do echo "  $item"; done
else
  say "Nothing of install.sh's was here"
  echo
  echo "  No binaries in $BIN_DIR, no launcher entry, no manifests pointing there."
fi

if (( ${#kept[@]} )); then
  echo
  echo "  Left alone, because they name a host binary this script did not install:"
  for item in "${kept[@]}"; do echo "    $item"; done
fi

echo
if [[ "$PURGE" == yes ]]; then
  echo "  Settings and the downloads database are gone too."
else
  echo "  Kept: $DATA_DIR"
  echo "        the downloads database, and the yt-dlp and QuickJS binaries the"
  echo "        app fetched for itself. Settings are in"
  echo "        ${XDG_CONFIG_HOME:-$HOME/.config}/mdm. Re-run with --purge to"
  echo "        remove both."
fi
echo
echo "  Files already downloaded are untouched, wherever you saved them."

if [[ -n "$packaged" ]]; then
  echo
  warn "MDM is also installed as a system package, which this script does not touch.
  That install is still in place and still works. To remove it as well:
    sudo dnf remove my-download-manager     (Fedora, RHEL, openSUSE)
    sudo apt remove my-download-manager     (Debian, Ubuntu, Mint)"
fi

cat <<'EXTENSION'

The browser extension is not removed by this script — a browser only lets you
remove an extension from inside it:

  Firefox   about:addons -> Extensions -> My Download Manager -> Remove
  Chromium  chrome://extensions -> My Download Manager -> Remove
            (edge://extensions, brave://extensions — same page)
EXTENSION
