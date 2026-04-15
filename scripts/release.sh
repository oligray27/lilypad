#!/usr/bin/env bash
# Usage: bash scripts/release.sh [--no-bump]
#   --no-bump  Skip version increment and use the current version as-is.
# Builds for production, commits (unless --no-bump), tags, pushes, and creates a GitHub release.
# Requires: node, cargo/tauri, git, gh (GitHub CLI)

set -e

NO_BUMP=0
for arg in "$@"; do
  [ "$arg" = "--no-bump" ] && NO_BUMP=1
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$SCRIPT_DIR/.."

cd "$ROOT"

# --- Read current version ---
CURRENT=$(node -e "console.log(JSON.parse(require('fs').readFileSync('src-tauri/tauri.conf.json','utf8')).version)")

if [ "$NO_BUMP" -eq 1 ]; then
  NEW="$CURRENT"
  echo "Releasing $CURRENT (no version bump)"
else
  # --- Bump patch ---
  NEW=$(node -e "const v='$CURRENT'.split('.').map(Number); v[2]++; console.log(v.join('.'))")
  echo "Releasing $CURRENT → $NEW"

  # --- Update tauri.conf.json ---
  node -e "
    const fs = require('fs');
    const f = 'src-tauri/tauri.conf.json';
    const c = JSON.parse(fs.readFileSync(f, 'utf8'));
    c.version = '$NEW';
    fs.writeFileSync(f, JSON.stringify(c, null, 2) + '\n');
  "

  # --- Update package.json ---
  node -e "
    const fs = require('fs');
    const f = 'package.json';
    const c = JSON.parse(fs.readFileSync(f, 'utf8'));
    c.version = '$NEW';
    fs.writeFileSync(f, JSON.stringify(c, null, 2) + '\n');
  "

  # --- Update Cargo.toml (first version = "..." in [package]) ---
  node -e "
    const fs = require('fs');
    const f = 'src-tauri/Cargo.toml';
    let content = fs.readFileSync(f, 'utf8');
    content = content.replace(/^version = \"[^\"]+\"/m, 'version = \"$NEW\"');
    fs.writeFileSync(f, content);
  "
fi

# --- Ensure appimagetool is available (needed for post-processing AppImage) ---
APPIMAGETOOL="$HOME/.cache/tauri/appimagetool-x86_64.AppImage"
if [ ! -f "$APPIMAGETOOL" ]; then
  echo "Downloading appimagetool..."
  curl -L -o "$APPIMAGETOOL" \
    "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage"
  chmod +x "$APPIMAGETOOL"
fi

# --- Build ---
echo "Building..."
# Clean stale AppDir — linuxdeploy's gtk plugin fails if it already exists from a prior run
rm -rf src-tauri/target/release/bundle/appimage/*.AppDir
# NO_STRIP=1: linuxdeploy's bundled strip is too old for modern Arch/Arch-based .relr.dyn sections
# APPIMAGE_EXTRACT_AND_RUN=1: run linuxdeploy without FUSE mount (more reliable)
NO_STRIP=1 APPIMAGE_EXTRACT_AND_RUN=1 npm run build

# --- Post-process AppImage: remove bundled GTK/webkit2gtk so the app uses system libs ---
# Bundled versions don't support Wayland fractional scaling (e.g. KDE 150%); system libs do.
APPIMAGE=$(ls -t src-tauri/target/release/bundle/appimage/*.AppImage 2>/dev/null | head -1)
if [ -n "$APPIMAGE" ]; then
  echo "Post-processing AppImage to use system GTK/webkit2gtk..."
  WORKDIR=$(mktemp -d)
  cd "$WORKDIR" && APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGE" --appimage-extract > /dev/null 2>&1
  rm -f  squashfs-root/usr/lib/libwebkit2gtk-4.1.so.0
  rm -f  squashfs-root/usr/lib/libjavascriptcoregtk-4.1.so.0
  rm -f  squashfs-root/usr/lib/libgtk-3.so.0
  rm -f  squashfs-root/usr/lib/libgdk-3.so.0
  rm -rf squashfs-root/usr/lib/webkit2gtk-4.1
  rm -rf squashfs-root/usr/lib/gtk-3.0
  # Remove forced X11 backend — it breaks Wayland fractional scaling (e.g. KDE 150%)
  sed -i '/GDK_BACKEND=x11/d' squashfs-root/apprun-hooks/linuxdeploy-plugin-gtk.sh
  ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGETOOL" squashfs-root "$APPIMAGE" > /dev/null 2>&1
  cd "$ROOT"
  rm -rf "$WORKDIR"
  echo "AppImage post-processed."
fi

if [ "$NO_BUMP" -eq 0 ]; then
  # --- Commit version bump ---
  git add package.json src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/Cargo.lock \
    index.html dist/index.html dist/src/main.js dist/src/main.css src/main.js src/main.css \
    packages/ .gitignore README.md
  git commit -m "chore: release v$NEW"
fi

# --- Tag (skip if already exists, e.g. secondary-platform build) ---
if ! git tag "v$NEW" 2>/dev/null; then
  echo "Tag v$NEW already exists, skipping tag creation"
fi

# --- Push ---
git push origin main
git push origin "v$NEW" 2>/dev/null || echo "Tag v$NEW already on remote"

# --- Collect installer artifacts (latest only) ---
ASSETS=()
NSIS_EXE=$(ls -t src-tauri/target/release/bundle/nsis/*.exe 2>/dev/null | head -1)
[ -n "$NSIS_EXE" ] && ASSETS+=("$NSIS_EXE")
[ -n "$APPIMAGE" ] && ASSETS+=("$APPIMAGE")
DEB=$(ls -t src-tauri/target/release/bundle/deb/*.deb 2>/dev/null | head -1)
[ -n "$DEB" ] && ASSETS+=("$DEB")

# --- Create or update GitHub release ---
if [ ${#ASSETS[@]} -eq 0 ]; then
  echo "Warning: no installer artifacts found"
elif gh release view "v$NEW" &>/dev/null; then
  # Release already exists (secondary platform build) — just upload artifacts
  echo "Release v$NEW already exists, uploading: ${ASSETS[*]}"
  gh release upload "v$NEW" "${ASSETS[@]}" --clobber
else
  echo "Creating release with: ${ASSETS[*]}"
  gh release create "v$NEW" "${ASSETS[@]}" --title "LilyPad v$NEW" --generate-notes
fi

echo "Done — v$NEW released."
