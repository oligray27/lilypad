# Linux parity baseline

Recorded 2026-09-25 against v0.6.0 / `cc801431714d50e8a8e58def5dbc674a3c82a4c2`.

## Release artifacts

GitHub release metadata identifies these as the latest published GTK upgrade baselines:

| Format | Release | Asset | Published SHA-256 |
|---|---|---|---|
| Debian | [v0.5.5](https://github.com/oligray27/lilypad/releases/tag/v0.5.5) | `lilypad-gtk_0.5.5-1_amd64.deb` | `bdd1b81e78c394f6fe9b93609ec046f51051faaab7f8ccd5a6803987dee62c94` |
| AppImage | [v0.5.5](https://github.com/oligray27/lilypad/releases/tag/v0.5.5) | `LilyPad-x86_64.AppImage` | `39d68b83589dfd18b9647fbaaf4104c53056ccf53a6f39d3f99be2133455d0c9` |
| RPM | [v0.5.0](https://github.com/oligray27/lilypad/releases/tag/v0.5.0) | `lilypad-gtk-0.5.0-1.x86_64.rpm` | `facac75d091a11926109e43353817e5350ae249bb106ff3c80f7167b656a5b6d` |

All three assets were downloaded on Bazzite into `/var/home/ogray/.local/share/lilypad-parity/baseline-assets` and their SHA-256 hashes matched the published values. The AppImage is saved as `LilyPad-0.5.5-x86_64.AppImage` to retain its version in the filename. They have not been installed into the desktop. v0.6.0 currently contains only the Windows installer.

## Validation status

- Windows Rust 1.95.0: `cargo test -p lilypad-core -p lilypad --locked --offline` passed, with 86 core and 7 Tauri unit tests.
- Two new isolated `linux_legacy_upgrade` integration tests passed on Windows and Linux. They exercise SQLite migration, not the GTK application.
- `.github/workflows/validate.yml` adds Ubuntu 24.04 core/GTK tests and release compilation, and Windows core/Tauri tests and compilation. It has not yet run on GitHub; no changes have been pushed.
- Existing Arch WSL cannot boot: `HCS_E_HYPERV_NOT_INSTALLED`. No Windows features or virtualisation settings were changed.
- SSH validation is available through `ogray@bazzite`, Bazzite `44.20260921.0`, KDE Wayland. The host has GTK 4.22.5 and libadwaita 1.9.4 runtime packages but no Rust/GTK development toolchain.
- A rootless Podman container named `lilypad-parity-build` provides Rust 1.95.0, GTK 4.18.6 and libadwaita 1.7.6. Its Debian Trixie image is `docker.io/library/rust:1.95.0-trixie` at digest `sha256:443dd9a3260cf23c22fc05051dd5661dd7b4028d3d25dbaffab6563b63c3539c`. Development dependencies are installed in the container, not layered into Bazzite.
- Linux core tests pass: 88 unit tests plus 2 migration integration tests. GTK's 15 model tests pass after the compatibility fix below.
- `cargo build -p lilypad-gtk --release --locked` passes in the container (initial release compilation: 2m 31s). The executable is `target/release/lilypad-gtk` in the remote workspace.
- The built executable passed startup and second-instance activation on the host's KDE Wayland session using an empty isolated profile. Application D-Bus introspection succeeded and the startup log contained only the normal monitor-start message. The smoke test terminated its own process afterwards; no real account, session queue or autostart configuration was used.
- Packaged runtime behaviour, supported distribution floor, full desktop workflows and deployed backend migration status remain unverified.

## First Linux compilation finding

The unmodified GTK source failed with `E0063`: `views/mappings.rs` constructed `ProcessMapping` without the newer `exe_path` field. The editor now sets `exe_path: None`, matching its basename-only input. Linux tests compile and pass after that fix. This does not implement the later planned process-path backfill.

The compiler also reports existing deprecated `ComboBoxText` usage. These warnings are retained for the GTK workflow phase; they are not compilation failures.

## Remote workspace and repeatable checks

The isolated workspace is `/var/home/ogray/.local/share/lilypad-parity/`:

- `source/`: source snapshot, including uncommitted migration fixtures and validation scripts.
- `target/`: container build output, mounted at `/workspace/target`.
- `baseline-assets/`: hash-verified published upgrade packages.

Run `podman exec -e CARGO_BUILD_JOBS=4 lilypad-parity-build bash scripts/validate-linux.sh` over SSH. The script records tool/library versions, runs core and GTK tests, and builds the release executable. It fails immediately when a stage fails, preserving logs under `target/validation/`. CI uses the same script. The container can be stopped between runs and restarted with `podman start lilypad-parity-build`.

`scripts/smoke-linux-desktop.sh` uses a fresh XDG profile, refuses to run alongside an existing LilyPad instance, checks application D-Bus registration/responsiveness and second-instance activation, then terminates its own test process. Its autostart entry is confined to the temporary profile. These are startup checks, not visual or logged-in session qualification.

The successful KDE run used:

```sh
cd /var/home/ogray/.local/share/lilypad-parity/source
XDG_RUNTIME_DIR=/run/user/1000 \
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
WAYLAND_DISPLAY=wayland-0 GDK_BACKEND=wayland \
bash scripts/smoke-linux-desktop.sh ../target/release/lilypad-gtk
```

Its retained profile is `source/target/validation/desktop-profile.IwBNEi`. Build logs and that smoke-test evidence were also copied to local `target/linux-baseline/` (ignored by Git). `gtk-before-fix.log` retains the original compilation failure. Shell syntax checks passed for both new scripts. The build container is stopped between validation sessions, retaining dependencies and build caches for the next phase.

## Toolchain findings

The manifests declare Rust 1.70 (core/Tauri) and 1.75 (GTK). Locally cached resolved `gtk4 0.11.4` and `glib 0.22.8` manifests declare Rust 1.92. Thus the GTK declaration does not describe a sufficient compiler for the locked dependency graph. CI pins Rust 1.95.0 to match the tested Windows compiler; do not advertise a verified minimum compiler until Linux qualification passes.

GTK feature flags require GTK 4.12 and libadwaita 1.5. Ubuntu 24.04 is the initial CI candidate, not yet a qualified minimum runtime. CI reports actual system library versions. Package dependency minimums will be adjusted only after the build/runtime check.

## Synthetic migration fixtures

`crates/lilypad-core/tests/fixtures/linux-legacy/` contains fabricated, credential-free data matching the v0.5.5 configuration schema inspected from git:

- An active native Linux session with a ten-minute checkpoint interval represented in its timestamps.
- A failed live-service submission with Unicode notes, spoiler flag and private visibility.
- Two dated sittings of an unmapped game with spaces in its executable name.
- A replay pointing to a completed game.
- An older aggregate-only entry omitting optional executable/replay/session fields.

`linux_legacy_upgrade.rs` copies these into temporary directories. It checks import, reopen, repeatability, untouched original files, explicit ownership and isolation between two synthetic accounts. A second test corrupts the last file to verify rollback of earlier imports and a successful subsequent retry. Neither test reads or changes the user's real LilyPad profile.

Additional missing-username authentication, disk-full, newer-schema, packaged upgrade and UI ownership scenarios remain in the parity plan; these fixtures do not establish full Phase 0 completion.

## Backend gate

The sibling backend contains `scripts/add_session_sync_ref_uniqueness.sql`, defining unique partial indexes on `(game_id, sync_ref)` for both session tables. File presence does not establish deployment. Before submission integration, inspect deployed index definitions/validity and exercise duplicate requests against a test account on staging. A 409 response is not universally an acknowledgement: the live-service route can report that a session is still being created.

No database migrations or API writes were performed during baseline preparation.

## First Linux run

Use an isolated source/build directory; do not run the application against an existing profile during compilation validation.

```sh
rustc --version
cargo --version
pkg-config --modversion gtk4 libadwaita-1
cargo test -p lilypad-core --locked
cargo test -p lilypad-gtk --locked
cargo build -p lilypad-gtk --release --locked
```

Retain compiler output and Linux-only test results. Only after compilation succeeds, run desktop smoke tests with isolated XDG data/config directories and test credentials. GTK/tray/notification qualification requires a real desktop session; passing these shell commands does not qualify desktop behaviour.
