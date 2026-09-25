#!/usr/bin/env bash
# Run inside a desktop session (or forward its display and session-bus environment over SSH).
# Uses an empty profile; does not log in, submit sessions, or change the real autostart entry.
# Set LILYPAD_SMOKE_SEED to a directory of legacy data files (e.g. the synthetic fixtures in
# crates/lilypad-core/tests/fixtures/linux-legacy) to start from an upgraded profile instead;
# the script then also requires the startup log to report their migration.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
binary="$(realpath "${1:-target/release/lilypad-gtk}")"
app_id=uk.co.froglog.lilypad
command -v gdbus >/dev/null
test -x "$binary"

owner=$(gdbus call --session --dest org.freedesktop.DBus \
  --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.NameHasOwner "$app_id")
if [[ "$owner" != '(false,)' ]]; then
  echo 'An existing LilyPad instance owns the application ID; close it before this isolated smoke test.' >&2
  exit 1
fi

mkdir -p target/validation
profile=$(mktemp -d "$(pwd)/target/validation/desktop-profile.XXXXXX")
export XDG_DATA_HOME="$profile/data"
export XDG_CONFIG_HOME="$profile/config"
export XDG_CACHE_HOME="$profile/cache"
mkdir -p "$XDG_DATA_HOME" "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME"
seed="${LILYPAD_SMOKE_SEED:-}"
if [[ -n "$seed" ]]; then
  mkdir -p "$XDG_DATA_HOME/froglog-lilypad"
  cp "$seed"/*.json "$XDG_DATA_HOME/froglog-lilypad/"
fi
export RUST_LOG=lilypad_gtk=info,lilypad_core=info
"$binary" >"$profile/startup.log" 2>&1 &
app_pid=$!
cleanup() {
  if kill -0 "$app_pid" 2>/dev/null; then
    kill "$app_pid"
  fi
  wait "$app_pid" 2>/dev/null || true
}
trap cleanup EXIT

ready=false
for ((attempt=0; attempt<20; attempt++)); do
  kill -0 "$app_pid"
  owner=$(gdbus call --session --dest org.freedesktop.DBus \
    --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.NameHasOwner "$app_id")
  if [[ "$owner" == '(true,)' ]]; then
    ready=true
    break
  fi
  sleep 0.5
done
if [[ "$ready" != true ]]; then
  cat "$profile/startup.log" >&2
  exit 1
fi

# Require an actual responding GTK application, not just a process that has not exited yet.
gdbus introspect --session --dest "$app_id" --object-path /uk/co/froglog/lilypad \
  >"$profile/application-dbus.txt"
timeout 10 "$binary" >"$profile/second-launch.log" 2>&1
kill -0 "$app_pid"
sleep 3
kill -0 "$app_pid"
test -f "$XDG_CONFIG_HOME/autostart/$app_id.desktop"
ps -p "$app_pid" -o pid=,etime=,rss=,comm= >"$profile/process.txt"
if [[ -n "$seed" ]]; then
  grep -q 'migrated [0-9]* record(s) from the legacy JSON queues' "$profile/startup.log"
  # Originals are a backup, never moved or rewritten.
  for f in "$seed"/*.json; do
    cmp -s "$f" "$XDG_DATA_HOME/froglog-lilypad/$(basename "$f")"
  done
fi
printf 'Startup and second-instance activation passed. Isolated profile/logs: %s\n' "$profile"
cat "$profile/startup.log"
