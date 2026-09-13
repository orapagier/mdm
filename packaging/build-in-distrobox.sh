#!/usr/bin/env bash
#
# Build the redistributable packages against an old glibc, inside a distrobox.
#
# The same job as packaging/build-in-container.sh, against the same Ubuntu and
# with the same glibc floor, driven by distrobox instead of a throwaway
# docker/podman run.
#
# Both amortize the toolchain, so neither is the faster one: that script caches
# an image built from packaging/Containerfile.build, this one installs the
# toolchain inside a named box on first entry, behind a marker file. The
# difference is what you are left holding afterwards -- a box you can enter and
# poke at, which is worth something if you already live in distrobox and
# nothing at all if you do not.
#
#     ./packaging/build-in-distrobox.sh
#
# Needs distrobox, and podman or docker underneath it.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BOX="mdm-build-ubuntu2204"
IMAGE="ubuntu:22.04"              # glibc 2.35 -- see the note at the end
CACHE="$HOME/.cache/mdm-distrobox"
WORK="$CACHE/work"                # copy of the tree the box builds, home-shared
OUT="$REPO/target/release/bundle-compat"
MARKER="$CACHE/$BOX.deps-built"

say()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m error:\033[0m %s\n' "$*" >&2; exit 1; }

# ------------------------------------------------------------------ inner
#
# The half that runs inside the box, on the staged copy in $WORK. Same steps as
# build-in-container.sh --inner; only where the tree lives is different.
if [[ "${1:-}" == "--inner" ]]; then
  export RUSTUP_HOME=/usr/local/rustup \
         CARGO_HOME=/usr/local/cargo \
         PATH="/usr/local/cargo/bin:$PATH"
  cd "$WORK"

  cargo build --release --workspace

  triple="$(rustc -vV | sed -n 's/^host: //p')"
  mkdir -p src-tauri/binaries
  install -m755 target/release/mdm-host "src-tauri/binaries/mdm-host-$triple"

  rm -rf target/mdm-chrome && mkdir -p target/mdm-chrome
  ( cd extension && tar cf - --exclude=test --exclude=manifest.json . ) \
    | ( cd target/mdm-chrome && tar xf - )
  mv target/mdm-chrome/manifest.chrome.json target/mdm-chrome/manifest.json
  grep -q '"key"' target/mdm-chrome/manifest.json \
    || { echo "staged the Firefox manifest for Chromium" >&2; exit 1; }

  cargo tauri build --bundles deb,rpm --config src-tauri/tauri.bundle.linux.conf.json

  mkdir -p "$WORK/out"
  cp target/release/bundle/deb/*.deb target/release/bundle/rpm/*.rpm "$WORK/out/"
  chmod 0644 "$WORK"/out/* 2>/dev/null || true
  exit 0
fi

# ------------------------------------------------------------------ outer

command -v distrobox >/dev/null 2>&1 \
  || die "distrobox is not installed. Install it, or use
  ./packaging/build-in-container.sh, which needs only podman or docker."
(command -v podman >/dev/null 2>&1 || command -v docker >/dev/null 2>&1) \
  || die "distrobox runs on podman or docker, and neither is installed."

if ! distrobox list 2>/dev/null | grep -qw "$BOX"; then
  say "Creating the build box $BOX ($IMAGE)"
  distrobox create --yes --name "$BOX" --image "$IMAGE"
fi

# The image ships nothing but Ubuntu, so the toolchain goes in on the first
# entry: the apt deps Tauri v2's build scripts look for, then rustup (the
# distro's rustc is far too old for this workspace's edition), then tauri-cli
# baked in so a rebuild of the packages does not rebuild the tooling. A marker
# file keeps it to once -- distrobox shares the home directory, so the marker
# and the cargo-in-toolchain check are both visible from the host.
if [[ ! -f "$MARKER" ]] \
   || ! distrobox enter "$BOX" -- bash -c 'test -x /usr/local/cargo/bin/cargo' 2>/dev/null; then

  say "First entry: installing build deps, rust and tauri-cli in the box"
  say "(one long run: tauri-cli is a compile or two)"
  mkdir -p "$CACHE"
  cat > "$CACHE/setup.sh" <<'SETUP'
#!/usr/bin/env bash
set -euo pipefail
BOX="$1"
sudo bash -c '
  set -e
  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  apt-get install -y --no-install-recommends \
    build-essential curl ca-certificates pkg-config file \
    libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev \
    libayatana-appindicator3-dev librsvg2-dev patchelf
  rm -rf /var/lib/apt/lists/*
  export RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal --no-modify-path
  export PATH=/usr/local/cargo/bin:$PATH
  cargo install tauri-cli --locked --version "^2"
'
# The build itself runs as this user, not as root, and cargo keeps its
# registry and toolchain under $CARGO_HOME -- which the install above, running
# as root, created root-owned. Hand it over so the build can write there.
sudo chown -R "$(id -u):$(id -g)" /usr/local/cargo /usr/local/rustup
touch "$HOME/.cache/mdm-distrobox/$BOX.deps-built"
echo "toolchain installed in the box"
SETUP
  chmod +x "$CACHE/setup.sh"
  distrobox enter "$BOX" -- bash -eu "$CACHE/setup.sh" "$BOX"
fi

# Staged into a copy rather than built in the tree you are working in: the
# box shares the home directory, and the build has no business writing to the
# working tree any more than it would on the host.
say "Staging the tree for the box"
rm -rf "$WORK" && mkdir -p "$WORK"
tar -C "$REPO" --exclude=./target --exclude=./.git -cf - . \
  | tar -C "$WORK" -xf -

mkdir -p "$OUT"
rm -f "$OUT"/*.deb "$OUT"/*.rpm

say "Building against $IMAGE"
distrobox enter "$BOX" -- bash -eu -c '
  cd "$HOME/.cache/mdm-distrobox/work"
  bash packaging/build-in-distrobox.sh --inner
'

echo
say "Built"
for pkg in "$WORK"/out/*.rpm "$WORK"/out/*.deb; do
  [[ -f "$pkg" ]] || continue
  install -m644 "$pkg" "$OUT/"
  printf '  %s\n  %s\n' "$OUT/$(basename "$pkg")" "$(du -h "$pkg" | cut -f1)"
done

floor="$(distrobox enter "$BOX" -- bash -c 'ldd --version 2>/dev/null | head -1' \
         | grep -oP '\d+\.\d+$' || true)"
[[ "$floor" =~ ^[0-9]+\.[0-9]+$ ]] || floor="2.35"

cat <<FLOOR

Linked against glibc $floor, which stops being the binding constraint at that
age: every distribution carrying webkit2gtk-4.1 -- which Tauri v2 requires and
which is the real floor -- already has a glibc at least this old. Checked:
Ubuntu 22.04+, Debian 12+, Fedora 38+.

Older than that, nothing helps. Fedora 36 and openSUSE Leap 15.6 have the
glibc but no webkit2gtk-4.1, and RHEL 9 and its rebuilds ship only
webkit2gtk3, the libsoup2 build -- none of them can run this whatever it was
built against.

Both packages come from this one build. Tauri assembles the .rpm in Rust
rather than by shelling out to rpmbuild, so an Ubuntu image producing a
package Fedora installs is expected rather than a surprise.

The box is left in place for the next build. Tear it down with:

  distrobox rm -f $BOX
FLOOR