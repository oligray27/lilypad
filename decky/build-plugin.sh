#!/usr/bin/env bash
# Assembles the LilyPad Decky plugin zip: target/decky/LilyPad-<version>.zip, containing
# LilyPad/{plugin.json,package.json,main.py,README.md,dist/{index.js,assets/},bin/lilypad-engine}.
#
# Usage: bash decky/build-plugin.sh [path/to/lilypad-engine]
#
# Needs, already built:
#   - the engine for Linux x86_64: `cargo build --release --locked -p lilypad-engine`, built on the
#     oldest distribution you support (it needs at least the build machine's glibc), e.g. in the
#     release build container;
#   - the frontend: `cd decky && npm ci && npm run build` (any OS with Node; dist/ is portable).
# Plus python3, for writing the zip with the engine marked executable.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DECKY="$ROOT/decky"
ENGINE="${1:-$ROOT/target/release/lilypad-engine}"

VERSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$DECKY/package.json")"
ENGINE_VERSION="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' "$ROOT/crates/lilypad-engine/Cargo.toml" | tr -d '\r' | head -1)"
[ "$VERSION" = "$ENGINE_VERSION" ] || { echo "decky/package.json is $VERSION but lilypad-engine is $ENGINE_VERSION" >&2; exit 1; }
[ -x "$ENGINE" ] || { echo "engine not found at $ENGINE (build it first)" >&2; exit 1; }
[ -f "$DECKY/dist/index.js" ] || { echo "frontend not built: run npm ci && npm run build in decky/" >&2; exit 1; }

OUT="$ROOT/target/decky"
STAGE="$OUT/LilyPad"
rm -rf "$STAGE"
mkdir -p "$STAGE/bin"
cp "$DECKY/plugin.json" "$DECKY/package.json" "$DECKY/main.py" "$DECKY/README.md" "$STAGE/"
# index.js plus dist/assets (the icon), which Decky serves at /plugins/LilyPad/assets/.
cp -r "$DECKY/dist" "$STAGE/dist"
rm -f "$STAGE"/dist/*.map
cp "$ENGINE" "$STAGE/bin/lilypad-engine"
chmod 755 "$STAGE/bin/lilypad-engine"

ZIP="$OUT/LilyPad-$VERSION.zip"
rm -f "$ZIP"
python3 - "$OUT" "$ZIP" <<'PY'
import os, sys, zipfile
root, zip_path = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(zip_path, "w", zipfile.ZIP_DEFLATED) as z:
    for folder, _, files in os.walk(os.path.join(root, "LilyPad")):
        for name in sorted(files):
            full = os.path.join(folder, name)
            info = zipfile.ZipInfo.from_file(full, os.path.relpath(full, root))
            info.compress_type = zipfile.ZIP_DEFLATED
            with open(full, "rb") as f:
                z.writestr(info, f.read())
PY
( cd "$OUT" && sha256sum "$(basename "$ZIP")" > "$(basename "$ZIP").sha256" )
echo "Built $ZIP"
python3 -c 'import sys,zipfile; [print(f"  {i.filename}  {oct(i.external_attr >> 16)}") for i in zipfile.ZipFile(sys.argv[1]).infolist()]' "$ZIP"
