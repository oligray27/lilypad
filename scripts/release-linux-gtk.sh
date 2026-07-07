#!/usr/bin/env bash
# Builds .deb, .rpm, and .AppImage packages for the native GTK4/libadwaita
# Linux build (crates/lilypad-gtk) — the Windows/Tauri build has its own
# release.sh/release.ps1 and is not touched by this script.
#
# Usage: bash scripts/release-linux-gtk.sh
#
# Requires: cargo, cargo-deb (cargo install cargo-deb), cargo-generate-rpm
# (cargo install cargo-generate-rpm), and for the AppImage step: linuxdeploy
# + linuxdeploy-plugin-gtk (downloaded to a cache dir on first run).
#
# Does NOT bump versions, commit, tag, or push — this only produces local
# package artifacts. See scripts/release.sh for the Windows/Tauri release flow.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$SCRIPT_DIR/.."
CRATE="$ROOT/crates/lilypad-gtk"
BUNDLE_DIR="$ROOT/target/release/bundle"
CACHE_DIR="${LINUXDEPLOY_CACHE_DIR:-$HOME/.cache/lilypad-linuxdeploy}"

cd "$ROOT"

echo "== Building release binary =="
cargo build --release -p lilypad-gtk

echo "== Building .deb =="
cargo deb -p lilypad-gtk --no-build
mkdir -p "$BUNDLE_DIR/deb"
mv target/debian/*.deb "$BUNDLE_DIR/deb/"

echo "== Building .rpm =="
(cd "$CRATE" && cargo generate-rpm --target-dir "$ROOT/target" -o "$BUNDLE_DIR/rpm/")

echo "== Building .AppImage =="
mkdir -p "$CACHE_DIR"
LINUXDEPLOY="$CACHE_DIR/linuxdeploy-x86_64.AppImage"
LINUXDEPLOY_GTK="$CACHE_DIR/linuxdeploy-plugin-gtk.sh"
if [ ! -x "$LINUXDEPLOY" ]; then
  curl -L -o "$LINUXDEPLOY" https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
  chmod +x "$LINUXDEPLOY"
fi
if [ ! -x "$LINUXDEPLOY_GTK" ]; then
  curl -L -o "$LINUXDEPLOY_GTK" https://raw.githubusercontent.com/tauri-apps/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh
  chmod +x "$LINUXDEPLOY_GTK"
fi

APPDIR="$ROOT/target/release/AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/128x128/apps"
cp target/release/lilypad-gtk "$APPDIR/usr/bin/"
cp "$CRATE/data/uk.co.froglog.lilypad.desktop" "$APPDIR/usr/share/applications/"
cp src-tauri/icons/128x128.png "$APPDIR/usr/share/icons/hicolor/128x128/apps/uk.co.froglog.lilypad.png"
cp src-tauri/icons/128x128_nowplaying.png "$APPDIR/usr/share/icons/hicolor/128x128/apps/uk.co.froglog.lilypad-tracking.png"

mkdir -p "$BUNDLE_DIR/appimage"
(
  cd "$BUNDLE_DIR/appimage"
  PATH="$CACHE_DIR:$PATH" NO_STRIP=1 "$LINUXDEPLOY" \
    --appimage-extract-and-run \
    --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/lilypad-gtk" \
    --desktop-file "$APPDIR/usr/share/applications/uk.co.froglog.lilypad.desktop" \
    --icon-file "$APPDIR/usr/share/icons/hicolor/128x128/apps/uk.co.froglog.lilypad.png" \
    --plugin gtk \
    --output appimage
)

echo ""
echo "Done. Artifacts:"
find "$BUNDLE_DIR" -maxdepth 2 -iname "*.deb" -o -iname "*.rpm" -o -iname "*.AppImage"
