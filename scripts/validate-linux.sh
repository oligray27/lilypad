#!/usr/bin/env bash
# Requires Rust, GTK4/libadwaita development packages and dbus-run-session.
# Builds/tests only: does not launch LilyPad or read the user's application profile.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
mkdir -p target/validation
{
  rustc --version
  cargo --version
  pkg-config --modversion gtk4 libadwaita-1
} | tee target/validation/environment.log

cargo test -p lilypad-core --locked 2>&1 | tee target/validation/core.log
cargo test -p lilypad-gtk --locked 2>&1 | tee target/validation/gtk.log
dbus-run-session -- cargo test -p lilypad-gtk --locked \
  notify::tests::no_notification_daemon_preserves_the_interception_window \
  -- --exact --ignored 2>&1 | tee target/validation/notification-unavailable.log
cargo build -p lilypad-gtk --release --locked 2>&1 | tee target/validation/release.log
