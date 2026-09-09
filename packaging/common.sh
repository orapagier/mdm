# shellcheck shell=bash
#
# The parts install.sh and bundle.sh both need.
#
# The two scripts do genuinely different jobs -- one sets this machine up, the
# other produces packages for other machines -- but they build the extension
# the same way, because it is the same extension. Sourced rather than copied so
# that changing how it is staged changes it for both; a copy in each would go
# on working after the other one changed, which is the failure that is only
# noticed in a package somebody else installed.
#
# Nothing here runs on its own: it defines, and the caller calls.

say()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m warning:\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m error:\033[0m %s\n' "$*" >&2; exit 1; }

# Chromium derives an extension's id from its public key, so pinning the key in
# manifest.chrome.json pins the id -- which is what lets the native messaging
# manifest name it before the extension has ever been installed.
CHROME_ID="pegdlonllkokelfmdafooihklghlkimh"
EXT_ID="mdm@ramlej.local"
HOST_NAME="io.mdm.host"

# Where the build lands. Cargo honours CARGO_TARGET_DIR over the default, and
# so must anything that goes looking for what it produced.
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}"

# Both browsers' copies of the extension, built out of the one source tree.
#
# Sets XPI and CHROME_DIR for the caller.
stage_extension() {
  say "Packaging the extension"
  XPI="$TARGET_DIR/mdm-firefox.xpi"
  # The test directory is developer-only; shipping it would put dead code in
  # front of AMO reviewers and bloat the package. The Chromium manifest and its
  # service worker go the same way: Firefox reads neither.
  ( cd "$REPO/extension" && rm -f "$XPI" \
    && zip -qr "$XPI" . -x '*.DS_Store' 'test/*' 'manifest.chrome.json' 'src/sw.js' )

  # The Chromium build: same source, one file swapped. Left unpacked as well as
  # zipped because Chrome and Edge load an *unzipped folder* in developer mode,
  # which is how this gets used before a store listing exists, while the zip is
  # what the store dashboards want uploaded.
  say "Packaging the extension for Chrome and Edge"
  CHROME_DIR="$TARGET_DIR/mdm-chrome"
  rm -rf "$CHROME_DIR"
  mkdir -p "$CHROME_DIR"
  ( cd "$REPO/extension" && tar cf - --exclude=test --exclude=manifest.json . ) \
    | ( cd "$CHROME_DIR" && tar xf - )
  mv "$CHROME_DIR/manifest.chrome.json" "$CHROME_DIR/manifest.json"
  # Staging this folder wrong is silent and expensive, so it is checked rather
  # than assumed. The folder still loads with the Firefox manifest in it; what
  # breaks is downstream and mute -- Chromium derives the id from the path when
  # no key is pinned, the native host allows exactly one id and refuses every
  # other, and the popup then sits on "Checking..." for good with nothing said
  # anywhere about why. Checked here rather than trusted because the zip below,
  # and any package built from this folder, carry whatever it finds.
  [[ -f "$CHROME_DIR/manifest.json" ]] \
    || die "staging $CHROME_DIR produced no manifest.json"
  grep -q '"key"' "$CHROME_DIR/manifest.json" \
    || die "$CHROME_DIR/manifest.json carries no pinned key, so it is the Firefox
  manifest rather than the Chromium one. A browser would derive an id from the
  path and the native host would refuse it."
  ( cd "$CHROME_DIR" && rm -f "$TARGET_DIR/mdm-chrome.zip" \
    && zip -qr "$TARGET_DIR/mdm-chrome.zip" . -x '*.DS_Store' )
}

# Is there a signed Firefox package, and where?
#
# Firefox installs nothing Mozilla has not signed, and signing happens at AMO
# rather than here. So the package `stage_extension` builds is the one to
# submit, and the one that comes back signed is the one to install: leave it at
# packaging/mdm-firefox-signed.xpi (or point MDM_XPI at it) and both scripts
# find it here.
#
# The signature is also what tells the two apart. A signed .xpi carries
# META-INF/mozilla.rsa, and a zip keeps its member names uncompressed, so the
# name can be found in the file without unpacking it -- no unzip to require.
#
# Sets SIGNED_XPI and `signed`.
find_signed_xpi() {
  SIGNED_XPI="${MDM_XPI:-$REPO/packaging/mdm-firefox-signed.xpi}"
  signed=no
  if LC_ALL=C grep -qsa 'META-INF/mozilla\.rsa' "$SIGNED_XPI"; then
    signed=yes
  elif [[ -n "${MDM_XPI:-}" ]]; then
    warn "MDM_XPI=$SIGNED_XPI is missing or carries no signature. Firefox would
  refuse it."
  fi
}

# Every directory a Chromium browser reads native messaging manifests from, for
# this user. One per browser and per packaging; a Flatpak reads its own tree
# inside the sandbox rather than the one beside it.
#
# Shared because install.sh writes exactly this list and uninstall.sh has to
# find exactly the same files again -- a manifest missed by the remover is one
# naming a host binary that is no longer there, which a browser reports as a
# connection failure rather than as "not installed".
chromium_manifest_dirs() {
  cat <<DIRS
${HOME}/.config/google-chrome/NativeMessagingHosts
${HOME}/.config/chromium/NativeMessagingHosts
${HOME}/.config/microsoft-edge/NativeMessagingHosts
${HOME}/.config/BraveSoftware/Brave-Browser/NativeMessagingHosts
${HOME}/.config/vivaldi/NativeMessagingHosts
${HOME}/.var/app/com.google.Chrome/config/google-chrome/NativeMessagingHosts
${HOME}/.var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser/NativeMessagingHosts
DIRS
}

# The Firefox equivalent. Every tree that could exist rather than only the ones
# that do: install.sh narrows this by what it finds, uninstall.sh does not need
# to, since it only ever deletes a file that is already there.
firefox_manifest_dirs() {
  cat <<DIRS
${HOME}/.mozilla/native-messaging-hosts
${HOME}/snap/firefox/common/.mozilla/native-messaging-hosts
${HOME}/.var/app/org.mozilla.firefox/.mozilla/native-messaging-hosts
DIRS
}
