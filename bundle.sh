#!/usr/bin/env bash
#
# Build the redistributable packages: one .rpm and one .deb, each carrying the
# app, the native host and the extension.
#
# This installs nothing. It is the other half of what used to be
# `install.sh --bundle`, split out because the two are opposite jobs and
# reading them as one was confusing:
#
#   ./install.sh   sets up *this* machine, for this user, without root
#   ./bundle.sh    produces packages, and installs none of them
#
# A package built here links against this machine's glibc, which is the right
# answer for a package you then install here and the wrong one for a package
# you hand to somebody else -- see the note this prints at the end, and
# packaging/build-in-container.sh for the portable build.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=packaging/common.sh
source "$REPO/packaging/common.sh"

BUNDLES="deb,rpm"
while (( $# )); do
  case "$1" in
    --rpm)  BUNDLES="rpm" ;;
    --deb)  BUNDLES="deb" ;;
    -h|--help)
      cat <<'USAGE'
Usage: bundle.sh [--rpm | --deb]

Builds the redistributable packages. Installs nothing.

               target/release/bundle/rpm/*.rpm   Fedora, RHEL, openSUSE
               target/release/bundle/deb/*.deb   Debian, Ubuntu, Mint

Each is one self-contained file carrying the app, the native host and the
extension, and registers itself with Firefox and the Chromium browsers as it
installs.

  --rpm      build only the .rpm
  --deb      build only the .deb

Needs the Tauri CLI:  cargo install tauri-cli --locked

Packages built here run only on distributions at least as new as this one. For
packages to hand to other people, use ./packaging/build-in-container.sh
instead, which builds against an old glibc.
USAGE
      exit 0 ;;
    *) die "unknown option: $1 (try --help)" ;;
  esac
  shift
done

cd "$REPO"

# Asked of cargo, not of PATH. `cargo install` puts the CLI in $CARGO_HOME/bin,
# and cargo searches that directory for `cargo-<subcommand>` whether or not it
# is on PATH -- so a machine can have a perfectly good `cargo tauri`, which is
# what the build below actually runs, and no `cargo-tauri` for `command -v` to
# find. Looking for the wrong one of those is how a freshly installed CLI still
# came back as "the Tauri CLI was not found", with a successful `cargo install`
# sitting in between.
cargo tauri --version >/dev/null 2>&1 \
  || die "the Tauri CLI was not found. Install it with: cargo install tauri-cli --locked"

say "Building (release)"
cargo build --release --workspace

HOST_BIN="$TARGET_DIR/release/mdm-host"
[[ -x "$HOST_BIN" ]] || die "build did not produce $HOST_BIN"

# Tauri copies a sidecar by looking for `<name>-<target triple>` and installs it
# beside the app as `<name>`. The triple comes from the toolchain rather than
# being assumed: a machine building for aarch64 would otherwise ship an x86 host
# binary that silently never starts.
triple="$(rustc -vV | sed -n 's/^host: //p')"
[[ -n "$triple" ]] || die "could not read the host triple from rustc"
mkdir -p "$REPO/src-tauri/binaries"
install -m755 "$HOST_BIN" "$REPO/src-tauri/binaries/mdm-host-$triple"

# The overlay names target/mdm-chrome as a resource, so the packages carry
# whatever is staged there -- staged here rather than assumed, because a folder
# left over from an older checkout looks exactly like a current one.
stage_extension

# The packages carry the *signed* extension. Without it they would ship an
# add-on Firefox refuses, which is worse than shipping none.
find_signed_xpi
[[ "$signed" == yes ]] || warn "packaging/mdm-firefox-signed.xpi is missing or unsigned, so the
  packages will carry an add-on Firefox refuses. Sign the package at
  addons.mozilla.org and save it there, then build again."

say "Building the redistributable packages"

# What "this run produced" is measured against, so a package left over from an
# older build is not reported as though it had just been made.
started="$(mktemp)"
trap 'rm -f "$started"' EXIT

# The overlay carries the sidecar, the extension and the registration scripts.
# They are deliberately not in tauri.conf.json: `externalBin` is checked on
# *every* build, and the binary it names is produced by that same build, so
# putting it in the base config makes a plain `cargo build` fail on any tree
# where it has not been staged -- which is every clean checkout.
#
# Which is also why this file is not called tauri.linux.conf.json. That name is
# reserved: Tauri merges tauri.<platform>.conf.json into every build for that
# platform automatically, so calling it that would put `externalBin` back into
# every build through the back door and break the clean checkout exactly as if
# it had been written into the base config. The Windows overlay beside it,
# tauri.bundle.windows.conf.json, is named the same way and for the same reason
# -- the pair is deliberate, and neither may lose the `bundle.` in front.
( cd "$REPO" && cargo tauri build --bundles "$BUNDLES" \
    --config src-tauri/tauri.bundle.linux.conf.json ) || die "cargo tauri build failed"

echo
built=no
# Only the kinds that were asked for, and only the files this build touched.
# Globbing both directories unconditionally reported the .deb from some earlier
# run as an output of a `--rpm` build, which is a package nobody just made and
# may be several versions old.
IFS=',' read -ra kinds <<< "$BUNDLES"
for kind in "${kinds[@]}"; do
  for pkg in "$TARGET_DIR"/release/bundle/"$kind"/*."$kind"; do
    [[ -f "$pkg" && ! "$pkg" -ot "$started" ]] || continue
    built=yes
    printf '  %s\n  %s\n' "$pkg" "$(du -h "$pkg" | cut -f1)"
  done
done
[[ "$built" == yes ]] || die "the bundler reported success but produced no packages"

# The extension, on its own, beside the packages.
#
# It is inside each package already, and that covers everyone who installs one.
# It does not cover the rest: an AppImage carries no post-install script, the
# Windows installer is a separate build, and somebody on a distribution neither
# package fits still needs the add-on. All of them can be sent to a release
# asset, and none of them can be sent to a path inside an .rpm -- so the two
# files are copied out here, ready to upload with the packages they were built
# alongside. The signed .xpi where there is one, because an unsigned package is
# one Firefox refuses to install.
DIST="$TARGET_DIR/release/bundle"
if [[ "$signed" == yes ]]; then
  install -m644 "$SIGNED_XPI" "$DIST/mdm-firefox.xpi"
else
  # Named for what it is. An unsigned .xpi uploaded under the ordinary name is
  # a release asset that fails at the last step, in Firefox, with nothing to
  # say the file was never going to work.
  install -m644 "$XPI" "$DIST/mdm-firefox-unsigned.xpi"
fi
install -m644 "$TARGET_DIR/mdm-chrome.zip" "$DIST/mdm-chrome.zip"
for ext in "$DIST"/mdm-firefox*.xpi "$DIST/mdm-chrome.zip"; do
  [[ -f "$ext" ]] || continue
  printf '  %s\n  %s\n' "$ext" "$(du -h "$ext" | cut -f1)"
done

cat <<'PACKAGES'

Each is one file and needs nothing beside it. Installing one puts the app, the
native host and the extension on the machine and registers the host with
Firefox and with the Chromium browsers — Chrome, Brave, Chromium, Edge and
Vivaldi — for every account on it:

  sudo dnf install ./My*.rpm     (Fedora, RHEL, openSUSE)
  sudo apt install ./My*.deb     (Debian, Ubuntu, Mint)

Installing over a package of the same version is a reinstall rather than an
upgrade, and the package managers say "nothing to do" instead:

  sudo dnf reinstall ./My*.rpm
  sudo apt install --reinstall ./My*.deb

A Firefox or Chrome installed from Flatpak or Snap reads its manifests from
inside its own sandbox and will not see the system ones, so those two need
install.sh rather than a package. yt-dlp is not carried either: the app fetches
it on first run and keeps it current.

The two extension files beside them are the same add-on the packages carry,
loose. Upload them with a release: someone who installs a package finds the
extension behind the app's "Browser extension" button and never needs these,
and everybody else does -- an AppImage runs no install script, and a
distribution neither package fits has no other way to get the add-on.
PACKAGES

# The one thing about these packages that is not visible in them. glibc has no
# forward compatibility, so a package built here runs only on distributions at
# least as new as this one -- and the failure is silent: it installs without
# complaint and then does nothing when clicked.
#
# Read with awk rather than `head -1 | grep`: head closes the pipe after the
# first line, ldd takes a SIGPIPE writing its second, and under `set -o
# pipefail` that 141 is the status of the whole assignment -- which `set -e`
# then treats as a failure and exits on. Silently, and only sometimes, because
# whether ldd has finished writing by then is a race: measured here, it killed
# the script five times out of eight, after the packages had been built and
# before this last paragraph could say anything about them.
glibc="$(ldd --version 2>/dev/null | awk 'NR == 1 { print $NF }')" || glibc=""
[[ "$glibc" =~ ^[0-9]+\.[0-9]+$ ]] || glibc=""
cat <<GLIBC
Built against this machine's glibc${glibc:+ ($glibc)}, so they will not start on
anything older. For packages to hand to other people, build them against an old
glibc instead, which costs a container and nothing else:

  ./packaging/build-in-container.sh

GLIBC
