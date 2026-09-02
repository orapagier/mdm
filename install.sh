#!/usr/bin/env bash
#
# Build and install MDM (My Download Manager) for the current user. Nothing here needs root; the
# system packages it depends on are checked for, not installed.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${HOME}/.local/bin"
APP_DIR="${HOME}/.local/share/applications"
ICON_DIR="${HOME}/.local/share/icons/hicolor"
EXT_ID="mdm@ramlej.local"
HOST_NAME="io.mdm.host"

say()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m warning:\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m error:\033[0m %s\n' "$*" >&2; exit 1; }

# ------------------------------------------------------------ distribution

# Which family this is, so every "install it with" line below names a command
# that exists here. ID_LIKE is what makes derivatives work without listing
# every one of them: Mint says "ubuntu debian", Nobara says "fedora".
detect_family() {
  local id="" like="" token
  if [[ -r /etc/os-release ]]; then
    id="$(. /etc/os-release 2>/dev/null && printf '%s' "${ID:-}")"
    like="$(. /etc/os-release 2>/dev/null && printf '%s' "${ID_LIKE:-}")"
  fi
  for token in $id $like; do
    case "$token" in
      debian|ubuntu|linuxmint|pop|elementary|zorin|kali|raspbian|devuan|neon|deepin|mx)
        echo debian; return ;;
      fedora|rhel|centos|rocky|almalinux|ol|nobara|bazzite) echo fedora; return ;;
      arch|manjaro|endeavouros|cachyos|garuda|artix)        echo arch;   return ;;
      opensuse*|suse|sles|sled)                             echo suse;   return ;;
      alpine|postmarketos)                                  echo alpine; return ;;
    esac
  done
  # A stripped image may have no useful os-release; the package manager on
  # PATH is the better answer by then anyway.
  for token in apt-get:debian dnf:fedora pacman:arch zypper:suse apk:alpine; do
    command -v "${token%%:*}" >/dev/null && { echo "${token##*:}"; return; }
  done
  echo unknown
}
FAMILY="$(detect_family)"

# Generic name -> what this family calls it. Several names per entry is normal:
# Debian splits the Rust toolchain in two, Fedora spells the compilers out.
pkg() {
  case "$1" in
    aria2|yt-dlp|ffmpeg|nodejs|zip|zenity) echo "$1" ;;
    rust)
      case "$FAMILY" in
        debian) echo "rustc cargo" ;; fedora|suse) echo "rust cargo" ;;
        arch)   echo "rust"        ;; alpine)      echo "rust cargo" ;;
        *)      echo "rust cargo"  ;;
      esac ;;
    buildtools)
      case "$FAMILY" in
        debian) echo "build-essential" ;; arch)   echo "base-devel" ;;
        alpine) echo "build-base"      ;; *)      echo "gcc gcc-c++ make" ;;
      esac ;;
    pkgconfig)
      case "$FAMILY" in
        debian|suse) echo "pkg-config"         ;; fedora) echo "pkgconf-pkg-config" ;;
        arch|alpine) echo "pkgconf"            ;; *)      echo "pkg-config" ;;
      esac ;;
    webkit)
      case "$FAMILY" in
        debian) echo "libwebkit2gtk-4.1-dev" ;; fedora) echo "webkit2gtk4.1-devel" ;;
        arch)   echo "webkit2gtk-4.1"        ;; suse)   echo "webkit2gtk3-devel"   ;;
        alpine) echo "webkit2gtk-dev"        ;; *)      echo "webkit2gtk-4.1"      ;;
      esac ;;
    dbus)
      case "$FAMILY" in
        debian) echo "libdbus-1-dev" ;; fedora) echo "dbus-devel" ;;
        arch)   echo "dbus"          ;; suse)   echo "dbus-1-devel" ;;
        alpine) echo "dbus-dev"      ;; *)      echo "dbus-devel" ;;
      esac ;;
    notify)
      case "$FAMILY" in
        debian) echo "libnotify-bin"   ;; suse) echo "libnotify-tools" ;;
        *)      echo "libnotify"       ;;
      esac ;;
    *) echo "$1" ;;
  esac
}

# The command that installs the given packages here.
install_cmd() {
  case "$FAMILY" in
    debian) echo "sudo apt install $*"     ;;
    fedora) echo "sudo dnf install $*"     ;;
    arch)   echo "sudo pacman -S $*"       ;;
    suse)   echo "sudo zypper install $*"  ;;
    alpine) echo "sudo apk add $*"         ;;
    *)      echo "install with your package manager: $*" ;;
  esac
}

# Install line for a list of *generic* names.
install_line() {
  local names=() n
  for n in "$@"; do names+=($(pkg "$n")); done
  install_cmd "${names[@]}"
}

# ---------------------------------------------------------------- deps

say "Checking dependencies (${FAMILY} family)"

# Required to build and install at all. The build ones are here rather than
# left to cargo because a missing -dev package surfaces as a pkg-config error
# from a build script three hundred lines into the output.
need=()
missing=()
require() { # require <command> <generic package>
  command -v "$1" >/dev/null || { missing+=("$1"); need+=("$2"); }
}
require cargo      rust
require aria2c     aria2
require zip        zip           # the extension is shipped as a zipped .xpi
require pkg-config pkgconfig
command -v cc >/dev/null || command -v gcc >/dev/null || {
  missing+=("a C compiler"); need+=(buildtools); }

# WebKitGTK is what Tauri renders in, and libdbus is linked by the desktop
# integration; both are found through pkg-config, so ask it directly rather
# than guessing at package names that may already be satisfied another way.
if command -v pkg-config >/dev/null; then
  pkg-config --exists webkit2gtk-4.1 || { missing+=("webkit2gtk-4.1"); need+=(webkit); }
  pkg-config --exists dbus-1         || { missing+=("dbus-1");         need+=(dbus);   }
else
  # Unverifiable without pkg-config, and both are hard requirements, so name
  # them: installing one that is already present costs nothing.
  need+=(webkit dbus)
fi

if (( ${#missing[@]} || ${#need[@]} )); then
  listed="$(printf '%s, ' "${missing[@]:-development headers}")"
  die "missing build dependencies: ${listed%, }
  Install them with:
    $(install_line "${need[@]}")"
fi

# Debian 12 and Ubuntu 22.04 freeze a Rust far older than this workspace needs,
# and cargo's own error for that ("feature edition2024 is required") reads like
# a bug in the source rather than an old toolchain.
MSRV="$(sed -n 's/^rust-version *= *"\([0-9.]*\)".*/\1/p' "$REPO/Cargo.toml" | head -1)"
have="$(cargo --version | awk '{print $2}' | sed 's/[^0-9.].*//')"
if [[ -n "$MSRV" && "$have" != "$MSRV" ]] &&
   [[ "$(printf '%s\n%s\n' "$have" "$MSRV" | sort -V | head -1)" == "$have" ]]; then
  die "cargo $have is too old — this needs $MSRV or newer.
  Distribution packages are often years behind; install the toolchain with:
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi

for cmd in yt-dlp ffmpeg; do
  command -v "$cmd" >/dev/null || \
    warn "$cmd not found — video extraction will be unavailable. Install it with:
    $(install_line "$cmd")"
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
    $(install_line nodejs)        (deno, bun and quickjs also work)"
  fi
fi

command -v notify-send >/dev/null || \
  warn "notify-send not found — desktop notifications disabled. Install it with:
    $(install_line notify)"

# The folder chooser shells out to one of these; without either, Settings can
# still be typed into, but the Browse button has nothing to open.
command -v zenity >/dev/null || command -v kdialog >/dev/null || \
  warn "neither zenity nor kdialog found — the folder picker will not open. Install one with:
    $(install_line zenity)"

# ---------------------------------------------------------------- build

say "Building (release)"
cd "$REPO"
cargo build --release --workspace

APP_BIN="$REPO/target/release/mdm"
HOST_BIN="$REPO/target/release/mdm-host"
[[ -x "$APP_BIN"  ]] || die "build did not produce $APP_BIN"
[[ -x "$HOST_BIN" ]] || die "build did not produce $HOST_BIN"

# ---------------------------------------------------------------- install

# --------------------------------------------- migrate a pre-rename install

# This app was called "ldm" until the rename to "mdm". Left alone, a machine
# that has the old one keeps a launcher entry and a native-messaging manifest
# both pointing at binaries that no longer exist, and — worse — its settings,
# queues and download history sit in a directory the new name never looks in.
say "Checking for a pre-rename (ldm) install"

# Never move a database out from under a process that has it open: renames do
# not disturb an open descriptor, so the running app would go on writing to
# files nothing can find again after it exits.
if pgrep -x ldm >/dev/null 2>&1 || pgrep -x mdm >/dev/null 2>&1; then
  die "MDM is running — quit it first, then re-run this script.
  Its database is open, and moving it now would strand everything written
  since the app started."
fi

migrated=
for base in "${XDG_CONFIG_HOME:-$HOME/.config}" \
            "${XDG_DATA_HOME:-$HOME/.local/share}" \
            "${XDG_CACHE_HOME:-$HOME/.cache}"; do
  if [[ -d "$base/ldm" && ! -e "$base/mdm" ]]; then
    mv "$base/ldm" "$base/mdm"
    say "  kept your data: $base/ldm -> $base/mdm"
    migrated=yes
  elif [[ -d "$base/ldm" ]]; then
    warn "both $base/ldm and $base/mdm exist — leaving the old one untouched,
  move anything you still want across by hand"
  fi
done
# The database is named after the app too, so moving the directory is not
# enough — and it is three files, not one. SQLite finds a write-ahead log by
# appending "-wal" to the database's own path, so a db renamed without its
# sidecars silently abandons every transaction still sitting in that log.
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/mdm"
for suffix in "" "-wal" "-shm"; do
  if [[ -f "$data_dir/ldm.db$suffix" && ! -e "$data_dir/mdm.db$suffix" ]]; then
    mv "$data_dir/ldm.db$suffix" "$data_dir/mdm.db$suffix"
  fi
done

# Old install artefacts. Only ever files this script itself wrote.
for stale in "$BIN_DIR/ldm" "$BIN_DIR/ldm-host" "$APP_DIR/io.ldm.app.desktop"; do
  [[ -e "$stale" ]] && { rm -f "$stale"; migrated=yes; }
done
for size in 16 24 32 48 64 128 256 512; do
  rm -f "$ICON_DIR/${size}x${size}/apps/io.ldm.app.png" \
        "$ICON_DIR/${size}x${size}/apps/ldm.png"
done

say "Installing binaries to $BIN_DIR"
mkdir -p "$BIN_DIR"
install -m755 "$APP_BIN"  "$BIN_DIR/mdm"
install -m755 "$HOST_BIN" "$BIN_DIR/mdm-host"

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
  install -m644 "$REPO/extension/icons/mdm-${size}.png" "$dir/io.mdm.app.png"
  install -m644 "$REPO/extension/icons/mdm-${size}.png" "$dir/mdm.png"
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
cat > "$APP_DIR/io.mdm.app.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=MDM
GenericName=My Download Manager
Comment=My Download Manager — accelerated downloads with browser capture
Exec=$BIN_DIR/mdm %u
Icon=io.mdm.app
Terminal=false
Categories=Network;FileTransfer;
Keywords=mdm;my download manager;download;manager;downloader;aria2;idm;video;
StartupWMClass=io.mdm.app
MimeType=x-scheme-handler/mdm;
DESKTOP
command -v update-desktop-database >/dev/null && \
  update-desktop-database -q "$APP_DIR" 2>/dev/null || true

# ------------------------------------------------- native messaging host

# Firefox looks for the manifest under whichever tree the *package* was built
# with, and one machine can have several: Debian's firefox-esr and Mozilla's
# own .deb read ~/.mozilla, Ubuntu ships Firefox as a snap that reads
# ~/snap/firefox, and a Flatpak reads ~/.var/app. Write to every tree that
# exists — an unused manifest is inert, a missing one breaks the extension.
say "Registering the native messaging host for Firefox"
NM_DIRS=("${HOME}/.mozilla/native-messaging-hosts")

# The per-package trees are asked about two ways, because the directory only
# appears once Firefox has been run at least once: a snap installed this
# morning and not yet opened would otherwise be missed, and the manifest is
# read at browser startup, not written by it.
snap_firefox=no
flatpak_firefox=no
[[ -d "$HOME/snap/firefox" ]] && snap_firefox=yes
[[ -d "$HOME/.var/app/org.mozilla.firefox" ]] && flatpak_firefox=yes
if command -v snap >/dev/null && snap list firefox >/dev/null 2>&1; then
  snap_firefox=yes
fi
if command -v flatpak >/dev/null && flatpak info org.mozilla.firefox >/dev/null 2>&1; then
  flatpak_firefox=yes
fi

[[ "$snap_firefox" == yes ]] && \
  NM_DIRS+=("${HOME}/snap/firefox/common/.mozilla/native-messaging-hosts")
[[ "$flatpak_firefox" == yes ]] && \
  NM_DIRS+=("${HOME}/.var/app/org.mozilla.firefox/.mozilla/native-messaging-hosts")

manifest_lines=""
for nm_dir in "${NM_DIRS[@]}"; do
  mkdir -p "$nm_dir"
  # Pre-rename manifest, if any: it names a host binary that is now gone.
  rm -f "$nm_dir/io.ldm.host.json"
  manifest_lines+="  Host manifest $nm_dir/${HOST_NAME}.json"$'\n'
  cat > "$nm_dir/${HOST_NAME}.json" <<MANIFEST
{
  "name": "${HOST_NAME}",
  "description": "My Download Manager native host",
  "path": "${BIN_DIR}/mdm-host",
  "type": "stdio",
  "allowed_extensions": ["${EXT_ID}"]
}
MANIFEST
done

# Both sandboxed builds can read the manifest above but still have to be let
# out to the host binary, and each is let out a different way.
if [[ "$flatpak_firefox" == yes ]]; then
  warn "Flatpak Firefox detected. Its sandbox cannot reach $BIN_DIR until you run:
    flatpak override --user --filesystem=home org.mozilla.firefox"
fi
if [[ "$snap_firefox" == yes ]]; then
  warn "Snap Firefox detected (the default on Ubuntu). Its manifest was written to
    ${HOME}/snap/firefox/common/.mozilla/native-messaging-hosts/
  A snap may only launch hosts from your home directory, which $BIN_DIR is.
  If the extension still reports the host as unavailable, install Firefox from
  Mozilla's apt repository (a .deb, not a snap) and re-run this script."
fi

# ---------------------------------------------------------------- extension

say "Packaging the extension"
XPI="$REPO/target/mdm-firefox.xpi"
# The test directory is developer-only; shipping it would put dead code in
# front of AMO reviewers and bloat the package.
( cd "$REPO/extension" && rm -f "$XPI" && zip -qr "$XPI" . -x '*.DS_Store' 'test/*' )

cat <<DONE

$(say "Installed")

  App           $BIN_DIR/mdm
  Native host   $BIN_DIR/mdm-host
${manifest_lines}  Extension     $XPI

${migrated:+If you had the older "ldm" build, its binaries, launcher entry and host
manifest have been removed and your settings and history moved across. The
extension id changed too, so remove the old temporary add-on before loading
this one.

}Load the extension in Firefox:

  1. Open  about:debugging#/runtime/this-firefox
  2. Click "Load Temporary Add-on…"
  3. Select  $REPO/extension/manifest.json

  Temporary add-ons are removed when Firefox restarts. To install it
  permanently, Firefox requires a signed package: submit $XPI to
  addons.mozilla.org (self-distribution signing is free and unlisted),
  or use Firefox Developer Edition with xpinstall.signatures.required=false.

Then start the app:  mdm
The extension launches it automatically on the first captured download.
DONE
