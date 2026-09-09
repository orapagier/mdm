#!/bin/sh
#
# Registration for a packaged install: the Linux counterpart of installer.nsh.
#
# The .deb and .rpm are meant to be handed to someone as one file, so the
# browser side has to be set up by the package rather than by a script the
# person was also supposed to have. That is what this does — it writes the
# native messaging manifests that let the extension reach mdm-host, which is
# the difference between an app that captures downloads and an app that sits
# there.
#
# It runs as root, so it writes the *system* manifest directories rather than
# one user's home: a package installed for the machine should work for every
# account on it, and root has no business writing into somebody's ~ anyway.
#
# Nothing here is allowed to fail the installation. A browser that is not
# installed, a directory that cannot be created — none of that is a reason for
# the package manager to roll back an app that is otherwise fine, so every
# step is best-effort and the script always exits 0.

set -u

HOST_NAME="io.mdm.host"
EXT_ID="mdm@ramlej.local"
CHROME_ID="pegdlonllkokelfmdafooihklghlkimh"

# Where the bundler put the native host. /usr/bin is where a sidecar goes in
# both a .deb and an .rpm — the bundler strips the target triple off the staged
# name and drops it beside the app. The rest are fallbacks: the layout is the
# bundler's to change between versions, and a manifest naming a path that does
# not exist is an extension that fails with nothing said anywhere about why.
HOST=""
for candidate in \
  "/usr/bin/mdm-host" \
  "/usr/lib/My Download Manager/mdm-host" \
  "/usr/lib/my-download-manager/mdm-host" \
  "/usr/lib/mdm/mdm-host" \
  "/usr/libexec/mdm/mdm-host"
do
  if [ -x "$candidate" ]; then HOST="$candidate"; break; fi
done

if [ -z "$HOST" ]; then
  echo "mdm: the native host was not found, so the browser extension will not" >&2
  echo "     be able to reach the app. Reinstall the package, or run" >&2
  echo "     install.sh from the source tree to register it by hand." >&2
  exit 0
fi

# Firefox and Chromium want the same facts in two spellings: Firefox names the
# add-on that may connect in `allowed_extensions`, Chromium names an extension
# *origin* in `allowed_origins`. Getting these the wrong way round leaves a
# manifest a browser reads, accepts and then ignores, so they are written
# separately rather than with one template and a substitution.
firefox_manifest() {
  cat <<JSON
{
  "name": "$HOST_NAME",
  "description": "My Download Manager native host",
  "path": "$HOST",
  "type": "stdio",
  "allowed_extensions": ["$EXT_ID"]
}
JSON
}

chromium_manifest() {
  cat <<JSON
{
  "name": "$HOST_NAME",
  "description": "My Download Manager native host",
  "path": "$HOST",
  "type": "stdio",
  "allowed_origins": ["chrome-extension://$CHROME_ID/"]
}
JSON
}

# Written to every directory whether or not that browser is installed. An
# unused manifest is inert; a missing one is an extension that cannot talk to
# anything — and a browser installed next week finds this already in place.
install_manifest() {  # install_manifest <dir> <writer>
  dir="$1"
  writer="$2"
  mkdir -p "$dir" 2>/dev/null || return 0
  if "$writer" > "$dir/$HOST_NAME.json" 2>/dev/null; then
    chmod 0644 "$dir/$HOST_NAME.json" 2>/dev/null || true
  fi
  return 0
}

# Firefox reads these two. Both are listed because the split is per
# distribution rather than per machine: Fedora's package looks in lib64,
# Debian's in lib, and writing both costs a file.
for dir in \
  /usr/lib/mozilla/native-messaging-hosts \
  /usr/lib64/mozilla/native-messaging-hosts
do
  install_manifest "$dir" firefox_manifest
done

# The Chromium family. Brave is not missing from this list — its binary reads
# /etc/opt/chrome and /etc/chromium and has no directory of its own, so the
# first two entries are what register it.
for dir in \
  /etc/opt/chrome/native-messaging-hosts \
  /etc/chromium/native-messaging-hosts \
  /etc/opt/edge/native-messaging-hosts \
  /etc/opt/vivaldi/native-messaging-hosts
do
  install_manifest "$dir" chromium_manifest
done

# Where this package put the extension, said out loud.
#
# The package carries both browsers' copies, and a package manager prints
# nothing about its own payload -- so somebody installing a release build got a
# working app, a registered native host, and no indication that the half doing
# the capturing was already on the machine. The app says the same thing in
# Settings, but only after it has been opened, and the first thing anyone does
# is look for the extension.
EXT_DIR=""
for candidate in \
  "/usr/lib/My Download Manager" \
  "/usr/lib/my-download-manager" \
  "/usr/lib/mdm"
do
  if [ -f "$candidate/mdm-firefox.xpi" ] || [ -d "$candidate/mdm-chrome" ]; then
    EXT_DIR="$candidate"
    break
  fi
done

if [ -n "$EXT_DIR" ]; then
  echo "mdm: the browser extension is installed with the app --"
  if [ -f "$EXT_DIR/mdm-firefox.xpi" ]; then
    echo "     Firefox:  open file://$EXT_DIR/mdm-firefox.xpi and click Add"
  fi
  if [ -d "$EXT_DIR/mdm-chrome" ]; then
    echo "     Chromium: chrome://extensions, Developer mode, Load unpacked,"
    echo "               then pick $EXT_DIR/mdm-chrome"
  fi
  echo "     Or press \"Browser extension\" in the app, which opens each one"
  echo "     in the right browser for you."
fi

# The launcher and its icon, so the app appears in the menu without a logout.
# Both are optional tools; a desktop that lacks them indexes on its own.
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database -q /usr/share/applications 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -qtf /usr/share/icons/hicolor 2>/dev/null || true
fi

exit 0
