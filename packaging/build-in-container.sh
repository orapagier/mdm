#!/usr/bin/env bash
#
# Build the redistributable packages against an old glibc.
#
# `./bundle.sh` builds them against *this* machine's glibc, which is
# the right answer for a package you install here and the wrong one for a
# package you hand to somebody else. glibc has no forward compatibility: a
# binary linked on Fedora 44 wants GLIBC_2.39, and on Ubuntu 22.04 that
# package installs without a word of complaint and then does nothing at all
# when clicked — the worst way for this to fail.
#
# So the packages meant for other people are built in a container old enough
# to cover the distributions still in support. Both come out of one build:
# Tauri assembles the .rpm in Rust rather than by shelling out to rpmbuild, so
# an Ubuntu image can produce a package Fedora installs.
#
#     ./packaging/build-in-container.sh
#
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="mdm-build:ubuntu2204"
OUT="$REPO/target/release/bundle-compat"

say()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m error:\033[0m %s\n' "$*" >&2; exit 1; }

# ------------------------------------------------------------------ inner
#
# The half that runs inside the container, on a copy of the tree. Same staging
# as bundle.sh does, because the bundler wants the same things
# wherever it runs.
if [[ "${1:-}" == "--inner" ]]; then
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
  cp target/release/bundle/deb/*.deb target/release/bundle/rpm/*.rpm /out/
  chmod 0644 /out/* || true
  exit 0
fi

# ------------------------------------------------------------------ outer

ENGINE=""
for candidate in podman docker; do
  command -v "$candidate" >/dev/null && { ENGINE="$candidate"; break; }
done
[[ -n "$ENGINE" ]] || die "neither podman nor docker is installed, and the whole
  point of this script is to build somewhere other than here. Install one, or
  use ./bundle.sh to build against this machine's glibc and accept
  that the packages will only run on distributions at least this new."

say "Preparing the toolchain image (cached after the first build)"
"$ENGINE" build -t "$IMAGE" -f "$REPO/packaging/Containerfile.build" "$REPO/packaging"

# Staged into a copy rather than bind-mounted: mounting the working tree would
# need an SELinux relabel of it on the distributions that enforce one, and the
# build has no business writing to the tree you are working in either.
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
tar -C "$REPO" --exclude=./target --exclude=./.git -cf - . | tar -C "$STAGE" -xf -

mkdir -p "$OUT"
rm -f "$OUT"/*.deb "$OUT"/*.rpm

say "Building against $(basename "$IMAGE" | tr ':' ' ')"
"$ENGINE" run --rm \
  -v "$STAGE:/src:ro,z" \
  -v "$OUT:/out:z" \
  "$IMAGE" bash -eu -c 'cp -a /src/. /work/ && cd /work \
     && bash packaging/build-in-container.sh --inner'

echo
say "Built"
for pkg in "$OUT"/*.rpm "$OUT"/*.deb; do
  [[ -f "$pkg" ]] || continue
  printf '  %s\n  %s\n' "$pkg" "$(du -h "$pkg" | cut -f1)"
done

floor="$("$ENGINE" run --rm "$IMAGE" bash -c 'ldd --version | head -1 | grep -oP "\d+\.\d+$"')"
cat <<FLOOR

Linked against glibc $floor, which stops being the binding constraint at that
age: every distribution carrying webkit2gtk-4.1 — which Tauri v2 requires and
which is the real floor — already has a glibc at least this old. Checked:
Ubuntu 22.04+, Debian 12+, Fedora 38+.

Older than that, nothing helps. Fedora 36 and openSUSE Leap 15.6 have the
glibc but no webkit2gtk-4.1, and RHEL 9 and its rebuilds ship only
webkit2gtk3, the libsoup2 build — none of them can run this whatever it was
built against.

Both packages come from this one build. Tauri assembles the .rpm in Rust
rather than by shelling out to rpmbuild, so an Ubuntu image producing a
package Fedora installs is expected rather than a surprise.
FLOOR
