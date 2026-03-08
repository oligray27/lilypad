#!/usr/bin/env bash
# Usage: bash scripts/release.sh
# Bumps the patch version, builds for production, commits, tags, pushes, and creates a GitHub release.
# Requires: node, cargo/tauri, git, gh (GitHub CLI)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$SCRIPT_DIR/.."

cd "$ROOT"

# --- Read current version ---
CURRENT=$(node -e "console.log(JSON.parse(require('fs').readFileSync('src-tauri/tauri.conf.json','utf8')).version)")

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

# --- Build ---
echo "Building..."
npm run build

# --- Commit version bump ---
git add package.json src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "chore: release v$NEW"

# --- Tag ---
git tag "v$NEW"

# --- Push ---
git push origin main
git push origin "v$NEW"

# --- Collect installer artifacts (latest only) ---
ASSETS=()
NSIS_EXE=$(ls -t src-tauri/target/release/bundle/nsis/*.exe 2>/dev/null | head -1)
[ -n "$NSIS_EXE" ] && ASSETS+=("$NSIS_EXE")

# --- Create GitHub release ---
if [ ${#ASSETS[@]} -eq 0 ]; then
  echo "Warning: no installer artifacts found, creating release without assets"
  gh release create "v$NEW" --title "LilyPad v$NEW" --generate-notes
else
  echo "Creating release with: ${ASSETS[*]}"
  gh release create "v$NEW" "${ASSETS[@]}" --title "LilyPad v$NEW" --generate-notes
fi

echo "Done — v$NEW released."
