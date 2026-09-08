#!/bin/sh
#
# Undoes what postinstall.sh registered.
#
# The manifests live outside the package's own file list — they are written at
# install time rather than shipped — so nothing removes them automatically and
# they would otherwise be left behind pointing at a binary that is gone. A
# browser reading one of those spawns nothing and says nothing about why.

set -u

HOST_NAME="io.mdm.host"

# Both package managers reuse this script for an *upgrade*: rpm runs the old
# package's removal after the new one is unpacked, and dpkg does the same. The
# argument is how they say which this is — rpm passes the number of copies
# that will remain, dpkg passes a word. Deleting the manifests on an upgrade
# would leave the freshly installed app unregistered, which is exactly the
# failure this script exists to prevent.
case "${1:-}" in
  0|remove|purge) ;;
  *) exit 0 ;;
esac

for dir in \
  /usr/lib/mozilla/native-messaging-hosts \
  /usr/lib64/mozilla/native-messaging-hosts \
  /etc/opt/chrome/native-messaging-hosts \
  /etc/chromium/native-messaging-hosts \
  /etc/opt/edge/native-messaging-hosts \
  /etc/opt/vivaldi/native-messaging-hosts
do
  rm -f "$dir/$HOST_NAME.json" 2>/dev/null || true
done

# Deliberately left alone: ~/.local/share/mdm, which holds mdm.db. An
# uninstall is not a request to lose a download history — the same choice the
# Windows uninstaller makes.

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database -q /usr/share/applications 2>/dev/null || true
fi

exit 0
