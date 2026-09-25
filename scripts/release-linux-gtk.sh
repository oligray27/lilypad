#!/usr/bin/env bash
# Builds .deb, .rpm, and .AppImage packages for the native GTK4/libadwaita
# Linux build (crates/lilypad-gtk) — the Windows/Tauri build has its own
# release.sh/release.ps1 and is not touched by this script.
#
# Usage: bash scripts/release-linux-gtk.sh [--appimage-only]
#   --appimage-only  Skip the .deb/.rpm steps and build just the .AppImage.
#
# Output: target/release/bundle/linux-<version>/ holding the packages, SHA256SUMS and
# BUILD-INFO.txt (commit, toolchain, library and tool versions). The directory is recreated
# on every run, so a failed build can never leave an older package to be uploaded by mistake,
# and the script fails unless every expected package was produced.
#
# Requires: cargo, cargo-deb (cargo install cargo-deb), cargo-generate-rpm
# (cargo install cargo-generate-rpm), dpkg-dev (for the .deb's automatic library
# dependencies), and for the AppImage step: linuxdeploy + linuxdeploy-plugin-gtk
# (downloaded to a cache dir on first run; their hashes are recorded in BUILD-INFO.txt).
#
# Build on the oldest distribution you intend to support: the packages require at least the
# glibc of the build machine. BUILD-INFO.txt records the highest glibc version the binary uses.
#
# Does NOT bump versions, commit, tag, or push — this only produces local
# package artifacts. See scripts/release.sh for the Windows/Tauri release flow.

set -euo pipefail

APPIMAGE_ONLY=0
for arg in "$@"; do
  [ "$arg" = "--appimage-only" ] && APPIMAGE_ONLY=1
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CRATE="$ROOT/crates/lilypad-gtk"
CACHE_DIR="${LINUXDEPLOY_CACHE_DIR:-$HOME/.cache/lilypad-linuxdeploy}"

cd "$ROOT"

# `tr -d '\r'`: a checkout made on Windows has CRLF line endings, which would otherwise leave a
# carriage return in the version and in every path built from it.
VERSION="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' "$CRATE/Cargo.toml" | tr -d '\r' | head -1)"
[ -n "$VERSION" ] || { echo "could not read the version from $CRATE/Cargo.toml" >&2; exit 1; }
# Everything shipped as a text file must have LF endings: see .gitattributes for what CRLF
# breaks. Checked here too, because a source tree copied from a Windows working copy bypasses it.
CRLF_FILES="$(grep -l $'\r' "$CRATE"/data/*.desktop "$CRATE"/debian/* 2>/dev/null || true)"
if [ -n "$CRLF_FILES" ]; then
  echo "These packaged files have Windows (CRLF) line endings; convert them to LF first:" >&2
  echo "$CRLF_FILES" >&2
  exit 1
fi

OUT="$ROOT/target/release/bundle/linux-$VERSION"
rm -rf "$OUT"
mkdir -p "$OUT"
echo "== LilyPad $VERSION -> $OUT =="

echo "== Building release binary =="
cargo build --release --locked -p lilypad-gtk

if [ "$APPIMAGE_ONLY" -eq 0 ]; then
  echo "== Building .deb =="
  cargo deb -p lilypad-gtk --no-build --output "$OUT/"

  echo "== Building .rpm =="
  (cd "$CRATE" && cargo generate-rpm --target-dir "$ROOT/target" -o "$OUT/")
else
  echo "== Skipping .deb/.rpm (--appimage-only) =="
fi

echo "== Building .AppImage =="
# Pinned, not "continuous"/"master": an unpinned tool changes under a release without any change
# here (the continuous build of 2026-08 fails to find this app's icon), and the version is part
# of the cached file name so an older download is never reused by mistake.
LINUXDEPLOY_TAG="1-alpha-20251107-1"
LINUXDEPLOY_GTK_COMMIT="7a3fbc31a9e5075073ff8790f26effbac5f84453"
mkdir -p "$CACHE_DIR"
LINUXDEPLOY="$CACHE_DIR/linuxdeploy-$LINUXDEPLOY_TAG-x86_64.AppImage"
LINUXDEPLOY_GTK_DIR="$CACHE_DIR/plugin-gtk-$LINUXDEPLOY_GTK_COMMIT"
LINUXDEPLOY_GTK="$LINUXDEPLOY_GTK_DIR/linuxdeploy-plugin-gtk.sh"
if [ ! -x "$LINUXDEPLOY" ]; then
  curl -fL -o "$LINUXDEPLOY" "https://github.com/linuxdeploy/linuxdeploy/releases/download/$LINUXDEPLOY_TAG/linuxdeploy-x86_64.AppImage"
  chmod +x "$LINUXDEPLOY"
fi
# Mainline plugin (NOT the tauri-apps fork, which hardcodes `DEPLOY_GTK_VERSION=3` for
# webkit2gtk/GTK3 apps and would silently mis-bundle this GTK4/libadwaita build, plus checks the
# legacy `gtk-theme` gsetting instead of `color-scheme`/the appearance portal for dark mode).
# linuxdeploy finds plugins on PATH by this exact file name, hence a directory per commit.
if [ ! -x "$LINUXDEPLOY_GTK" ]; then
  mkdir -p "$LINUXDEPLOY_GTK_DIR"
  curl -fL -o "$LINUXDEPLOY_GTK" "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/$LINUXDEPLOY_GTK_COMMIT/linuxdeploy-plugin-gtk.sh"
  chmod +x "$LINUXDEPLOY_GTK"
fi

APPDIR="$ROOT/target/release/AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/128x128/apps"
cp target/release/lilypad-gtk "$APPDIR/usr/bin/"
cp "$CRATE/data/uk.co.froglog.lilypad.desktop" "$APPDIR/usr/share/applications/"
cp src-tauri/icons/128x128.png "$APPDIR/usr/share/icons/hicolor/128x128/apps/uk.co.froglog.lilypad.png"
cp src-tauri/icons/128x128_nowplaying.png "$APPDIR/usr/share/icons/hicolor/128x128/apps/uk.co.froglog.lilypad-tracking.png"

(
  cd "$OUT"
  PATH="$LINUXDEPLOY_GTK_DIR:$PATH" NO_STRIP=1 "$LINUXDEPLOY" \
    --appimage-extract-and-run \
    --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/lilypad-gtk" \
    --desktop-file "$APPDIR/usr/share/applications/uk.co.froglog.lilypad.desktop" \
    --icon-file "$APPDIR/usr/share/icons/hicolor/128x128/apps/uk.co.froglog.lilypad.png" \
    --plugin gtk \
    --output appimage
)

echo "== Checking artifacts =="
expect() {
  local found
  found="$(find "$OUT" -maxdepth 1 -name "$1" | wc -l)"
  [ "$found" -eq 1 ] || { echo "expected exactly one $1 in $OUT, found $found" >&2; exit 1; }
}
expect '*.AppImage'
if [ "$APPIMAGE_ONLY" -eq 0 ]; then
  expect "*_${VERSION}-*.deb"
  expect "*-${VERSION}-*.rpm"
fi

GLIBC_MAX="$(objdump -T target/release/lilypad-gtk | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)"
{
  echo "LilyPad (GTK) $VERSION"
  # LILYPAD_COMMIT lets a build from an exported source tree (no .git) still say what it is.
  if [ -n "${LILYPAD_COMMIT:-}" ]; then
    echo "commit: $LILYPAD_COMMIT"
  elif git rev-parse HEAD >/dev/null 2>&1; then
    echo "commit: $(git rev-parse HEAD)$(git diff --quiet HEAD -- || echo ' + uncommitted changes')"
  else
    echo "commit: unknown (not a git checkout; set LILYPAD_COMMIT)"
  fi
  echo "built: $(date -u +%Y-%m-%dT%H:%M:%SZ) on $(. /etc/os-release && echo "$PRETTY_NAME")"
  echo "rustc: $(rustc --version)"
  echo "gtk4: $(pkg-config --modversion gtk4), libadwaita: $(pkg-config --modversion libadwaita-1)"
  echo "highest glibc symbol required: $GLIBC_MAX"
  echo "linuxdeploy $LINUXDEPLOY_TAG sha256: $(sha256sum "$LINUXDEPLOY" | cut -d' ' -f1)"
  echo "linuxdeploy-plugin-gtk $LINUXDEPLOY_GTK_COMMIT sha256: $(sha256sum "$LINUXDEPLOY_GTK" | cut -d' ' -f1)"
} > "$OUT/BUILD-INFO.txt"
(cd "$OUT" && sha256sum *.deb *.rpm *.AppImage 2>/dev/null > SHA256SUMS)

echo ""
echo "Done. $OUT:"
ls -l "$OUT"
cat "$OUT/BUILD-INFO.txt"
