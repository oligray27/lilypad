#!/usr/bin/env bash
# Usage: bash scripts/release.sh [--no-bump] [--appimage-only]
#   --no-bump        Skip version increment and use the current version as-is.
#                     Use this when adding another platform's build to an already-tagged
#                     release (e.g. this release was tagged from Linux, and you're now
#                     running this script's Windows-flavored steps elsewhere) — see below.
#   --appimage-only  Linux only: skip .deb/.rpm and only build/upload the .AppImage
#                     (see scripts/release-linux-gtk.sh). Ignored on other platforms.
#
# Bumps the version everywhere, commits, tags, pushes, builds this platform's
# installers, and creates (or adds to) the GitHub release for that tag.
#
# Platform-specific build step:
#   - Linux:  builds crates/lilypad-gtk (native GTK4/libadwaita app) and packages
#             .deb/.rpm/.AppImage via scripts/release-linux-gtk.sh. The old
#             Tauri/webview Linux build is NOT built here anymore — the native
#             GTK app replaced it as of the 2026-07-07 rewrite.
#   - other:  runs `npm run build` (Tauri) and collects the NSIS .exe, same as
#             before. This is what a Windows machine should still use.
#
# Multi-platform releases: this repo has no single machine that can build every
# platform, so a release is assembled incrementally. Run this once (bumping the
# version) on whichever machine you're on first; the resulting tag/release gets
# that platform's artifacts. Then run it again with --no-bump on each other
# platform (e.g. release.ps1 -NoBump on Windows) — it detects the release
# already exists and uploads its artifacts to it instead of trying to create a
# duplicate.
#
# Requires: node, cargo, git, gh (GitHub CLI); on Linux also cargo-deb and
# cargo-generate-rpm (cargo install cargo-deb cargo-generate-rpm).

set -e

NO_BUMP=0
APPIMAGE_ONLY=0
for arg in "$@"; do
  [ "$arg" = "--no-bump" ] && NO_BUMP=1
  [ "$arg" = "--appimage-only" ] && APPIMAGE_ONLY=1
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

  # --- Update Cargo.toml version fields (first `version = "..."` in [package]) ---
  for CARGO_TOML in src-tauri/Cargo.toml crates/lilypad-core/Cargo.toml crates/lilypad-gtk/Cargo.toml; do
    node -e "
      const fs = require('fs');
      const f = '$CARGO_TOML';
      let content = fs.readFileSync(f, 'utf8');
      content = content.replace(/^version = \"[^\"]+\"/m, 'version = \"$NEW\"');
      fs.writeFileSync(f, content);
    "
  done
fi

# --- Build (platform-specific) ---
ASSETS=()
if [ "$(uname -s)" = "Linux" ]; then
  if [ "$APPIMAGE_ONLY" -eq 1 ]; then
    echo "Building Linux native GTK app (.AppImage only)..."
    bash scripts/release-linux-gtk.sh --appimage-only
  else
    echo "Building Linux native GTK app (.deb/.rpm/.AppImage)..."
    bash scripts/release-linux-gtk.sh
  fi
  BUNDLE_DIR="target/release/bundle"
  APPIMAGE=$(ls -t "$BUNDLE_DIR"/appimage/*.AppImage 2>/dev/null | head -1)
  [ -n "$APPIMAGE" ] && ASSETS+=("$APPIMAGE")
  if [ "$APPIMAGE_ONLY" -eq 0 ]; then
    # Only collected on a full build -- with --appimage-only, a stale .deb/.rpm from an
    # earlier run would otherwise get silently re-uploaded even though this run never
    # touched them.
    DEB=$(ls -t "$BUNDLE_DIR"/deb/*.deb 2>/dev/null | head -1)
    RPM=$(ls -t "$BUNDLE_DIR"/rpm/*.rpm 2>/dev/null | head -1)
    [ -n "$DEB" ] && ASSETS+=("$DEB")
    [ -n "$RPM" ] && ASSETS+=("$RPM")
  fi
else
  echo "Building Tauri Windows app..."
  npm run build
  NSIS_EXE=$(ls -t src-tauri/target/release/bundle/nsis/*.exe 2>/dev/null | head -1)
  [ -n "$NSIS_EXE" ] && ASSETS+=("$NSIS_EXE")
fi

if [ "$NO_BUMP" -eq 0 ]; then
  # --- Commit version bump ---
  git add package.json src-tauri/tauri.conf.json src-tauri/Cargo.toml \
    crates/lilypad-core/Cargo.toml crates/lilypad-gtk/Cargo.toml Cargo.lock
  git commit -m "chore: release v$NEW"

  # --- Tag ---
  git tag "v$NEW"

  # --- Push ---
  git push origin main
  git push origin "v$NEW"
fi

# --- Create GitHub release, or add to it if it already exists (multi-platform workflow) ---
if [ ${#ASSETS[@]} -eq 0 ]; then
  echo "Warning: no installer artifacts found for this platform"
fi

if gh release view "v$NEW" >/dev/null 2>&1; then
  if [ ${#ASSETS[@]} -eq 0 ]; then
    echo "Release v$NEW already exists; nothing to upload from this platform."
  else
    echo "Release v$NEW already exists — uploading: ${ASSETS[*]}"
    gh release upload "v$NEW" "${ASSETS[@]}" --clobber
  fi
else
  if [ ${#ASSETS[@]} -eq 0 ]; then
    echo "Creating release v$NEW without assets"
    gh release create "v$NEW" --title "LilyPad v$NEW" --generate-notes
  else
    echo "Creating release v$NEW with: ${ASSETS[*]}"
    gh release create "v$NEW" "${ASSETS[@]}" --title "LilyPad v$NEW" --generate-notes
  fi
fi

echo "Done — v$NEW released."
