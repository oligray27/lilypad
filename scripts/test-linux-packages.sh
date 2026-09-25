#!/usr/bin/env bash
# Installs the built Linux packages into clean containers and checks they work as packages:
# dependencies resolve, the binary has no missing libraries, desktop file and icons are in
# place, and each installs cleanly as an upgrade over the previous published package.
#
# Usage (on a Linux host with podman):
#   bash scripts/test-linux-packages.sh <package-dir> [<previous-release-dir>]
# <package-dir> is release-linux-gtk.sh's output (target/release/bundle/linux-<version>).
# <previous-release-dir>, if given, holds the last published .deb/.rpm to upgrade from.
#
# Headless: this cannot show the GUI. The desktop smoke test covers startup.
set -uo pipefail

PKG_DIR="$(realpath "$1")"
PREV_DIR="$(realpath "${2:-/nonexistent}" 2>/dev/null || true)"
DEB="$(ls "$PKG_DIR"/*.deb)"; RPM="$(ls "$PKG_DIR"/*.rpm)"
failures=0

check() {  # check <name> <image> <script>
  local name="$1" image="$2" script="$3"
  echo "=== $name ($image)"
  if podman run --rm -v "$PKG_DIR:/pkg:ro,z" ${PREV_DIR:+-v "$PREV_DIR:/prev:ro,z"} "$image" bash -euc "$script"; then
    echo "PASS $name"
  else
    echo "FAIL $name"; failures=$((failures + 1))
  fi
}

# Shared post-install assertions.
VERIFY='
  test -x /usr/bin/lilypad-gtk
  if ldd /usr/bin/lilypad-gtk | grep "not found"; then echo "missing libraries"; exit 1; fi
  test -f /usr/share/applications/uk.co.froglog.lilypad.desktop
  test -f /usr/share/icons/hicolor/128x128/apps/uk.co.froglog.lilypad.png
  test -f /usr/share/icons/hicolor/128x128/apps/uk.co.froglog.lilypad-tracking.png
'

DEB_INSTALL='export DEBIAN_FRONTEND=noninteractive; apt-get -qq update >/dev/null'

for image in docker.io/library/ubuntu:24.04 docker.io/library/debian:trixie; do
  check ".deb fresh install" "$image" "$DEB_INSTALL; apt-get -qq install -y /pkg/$(basename "$DEB") >/dev/null; $VERIFY"
done
check ".rpm fresh install" docker.io/library/fedora:41 "dnf -q install -y /pkg/$(basename "$RPM") >/dev/null; $VERIFY"

if [ -d "$PREV_DIR" ]; then
  OLD_DEB="$(ls "$PREV_DIR"/*.deb 2>/dev/null | head -1)"
  OLD_RPM="$(ls "$PREV_DIR"/*.rpm 2>/dev/null | head -1)"
  if [ -n "$OLD_DEB" ]; then
    check ".deb upgrade from $(basename "$OLD_DEB")" docker.io/library/ubuntu:24.04 "
      $DEB_INSTALL; apt-get -qq install -y /prev/$(basename "$OLD_DEB") >/dev/null
      apt-get -qq install -y /pkg/$(basename "$DEB") >/dev/null
      dpkg-query -W -f='\${Version}\n' lilypad-gtk | grep -q '^$(basename "$DEB" | cut -d_ -f2)$'
      $VERIFY"
  fi
  if [ -n "$OLD_RPM" ]; then
    # --nodeps for the old package only: rpms before 0.6.1 required Debian-only sonames (their
    # dependencies were computed on a Debian build machine) and cannot be installed normally on
    # Fedora at all. The upgrade transaction itself is what this checks.
    check ".rpm upgrade from $(basename "$OLD_RPM")" docker.io/library/fedora:41 "
      dnf -q install -y gtk4 libadwaita >/dev/null
      rpm -i --nodeps /prev/$(basename "$OLD_RPM")
      dnf -q install -y /pkg/$(basename "$RPM") >/dev/null
      rpm -q lilypad-gtk | grep -q '$(basename "$RPM" .x86_64.rpm)'
      $VERIFY"
  fi
fi

echo
[ "$failures" -eq 0 ] && echo "All package checks passed." || echo "$failures package check(s) failed."
exit "$failures"
