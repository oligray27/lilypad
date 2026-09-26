# LilyPad

A system tray companion for [FrogLog](https://froglog.co.uk). LilyPad notices when you start a game, times the session, and logs it to your FrogLog profile when you stop.

There are three builds from this repository, sharing the same core:

- **Windows**: a Tauri app.
- **Linux**: a native GTK4/libadwaita app.
- **Steam Gaming Mode** (Steam Deck, Bazzite): a [Decky Loader](https://decky.xyz) plugin with its own headless tracking engine. See [decky/README.md](decky/README.md).

## Install

Download from the [latest release](https://github.com/oligray27/lilypad/releases/latest).

### Windows

Run `LilyPad_<version>_x64-setup.exe`.

### Linux

| Package | For |
|---|---|
| `lilypad-gtk_<version>-1_amd64.deb` | Ubuntu 24.04+, Debian 13+ and derivatives: `sudo apt install ./lilypad-gtk_*.deb` |
| `lilypad-gtk-<version>-1.x86_64.rpm` | Fedora 40+ and derivatives: `sudo dnf install ./lilypad-gtk-*.rpm` |
| `LilyPad-x86_64.AppImage` | Anything else with glibc 2.39+: `chmod +x LilyPad-x86_64.AppImage` and run it |

Requires GTK 4.12+ and libadwaita 1.5+. The packages declare these; the AppImage bundles them.

- **KDE Plasma** shows the tray icon out of the box.
- **GNOME** needs the [AppIndicator and KStatusNotifierItem Support](https://extensions.gnome.org/extension/615/appindicator-support/) extension for any tray icon. Without it, LilyPad opens its window on every start instead; the ⋮ menu has everything the tray would.

LilyPad adds itself to your login items on first launch.

### Steam Gaming Mode (Steam Deck, Bazzite)

Needs [Decky Loader](https://decky.xyz). Download `LilyPad-<version>.zip`, enable Decky's developer mode, then go to Decky settings > Developer > Install Plugin from ZIP. LilyPad then appears in the Quick Access Menu. The plugin works on its own and doesn't need the desktop app.

If you also install the Linux desktop app, both share one login and history, and only one tracks at a time: the desktop app in Desktop Mode, the plugin in Gaming Mode.

### Updates

LilyPad checks for a new release shortly after it starts, then once a day. It never downloads or installs anything without you. Turn this on or off with **Check for LilyPad updates automatically** in Configure (Windows and Linux; on Linux it also covers Gaming Mode). The Windows installer asks the first time you install; after that, upgrades keep your answer. Checks are on unless you turn them off. When a new version is out:

- **Windows**: a notification, and **Update to (x.y.z)...** in the tray menu, which downloads the installer. Run it over the top of the installed version.
- **Linux**: a notification with a **Download** button, **Update to (x.y.z)…** in the tray menu, and the version link in the window header. They open the release page, where you pick your package.
- **Steam Gaming Mode**: a one-time toast, and an **Update available** section at the top of the plugin's panel. **Update now** hands the new version to Decky, which asks you to confirm and then installs it; your login and history are kept, and a game in progress keeps being tracked. If your Decky can't do that, **Open release page** has the zip to install the same way as the first.

You're notified once per release. The tray item and panel notice stay until you update.

#### Upgrading from 0.5.x on Linux

Your pending sessions and New Games are imported automatically the first time the new version starts. Older versions didn't record which account a session belonged to. Those sessions appear in **Pending Submissions** under *From an earlier LilyPad version*, where you choose **Assign to me** or **Discard**. The old files are left in place as a backup.

Going back to 0.5.x after upgrading is not supported: anything played since the upgrade is stored only in the new format.

## Using LilyPad

1. **Log in** with your FrogLog account. LilyPad then runs in the tray.
2. **Link games** under **Configure…**: pick a running or installed game's executable and the FrogLog entry it belongs to. Steam games you already have in FrogLog are linked automatically the first time you play them.
3. **Play.** LilyPad shows "Tracking Started" and your profile shows you as playing, if you've turned that on in Configure.
4. **When you stop**, the session is submitted automatically. For session-tracked and live-service games, a notification first offers **Add Notes** for a short time. If auto-submit is off, a window asks you to submit it or not record it.

Also:

- **New Games**: Steam and watched-folder games that aren't in your FrogLog yet are recorded anyway. **New Games** lets you create the entry, add the time to a game you already have, or dismiss it.
- **Pending Submissions**: a session that couldn't be sent (offline, logged out) waits here, and **Retry** sends it.
- **Stop Tracking Current Session** (tray): ends a session that was attributed to the wrong game. You then choose what to do with the time.
- **Crashes and restarts**: sessions are saved as they happen. If LilyPad or the PC stops mid-game, the session is recovered at the next start, counted up to the last point LilyPad knew the game was running.

### Where data lives

| OS | Folder |
|---|---|
| Windows | `%LOCALAPPDATA%\froglog-lilypad\` |
| Linux | `~/.local/share/froglog-lilypad/` |

`sessions.sqlite` holds sessions, pending submissions and New Games. `auth.json` holds your login, and `process-map-<account>.json` your game links.

### Known limitations on Linux

- **Flatpak Steam**: only games you have already linked in Configure are tracked. Unlinked games are not detected as New Games.
- **Window-title filters** (telling apart games that share one executable, e.g. `java`) only see X11/XWayland windows.
- **No notification service**: sessions auto-submit without the Add Notes option. Notes can be added afterwards on the FrogLog website.

## Development

### Windows (Tauri)

```powershell
npm install
npm run dev      # tauri dev
npm run build    # installer in target/release/bundle/nsis
```

### Linux (GTK)

```bash
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev   # or the Fedora equivalents
cargo run -p lilypad-gtk
```

The GTK build needs GTK 4.12+ and libadwaita 1.5+ development files. Its code can also be type-checked on Windows with `scripts/check-gtk-on-windows.ps1`.

### Tests

- `cargo test -p lilypad-core -p lilypad`: unit and integration tests.
- `scripts/test-all.ps1`: every automated check on Windows and, over SSH, on a Linux build host (see `docs/linux-baseline.md`).
- `docs/release-test-plan.md`: the manual pre-release checklist.

### Releasing

- **Windows**: `scripts/release.ps1` bumps the patch version, builds, commits, tags, pushes and creates the GitHub release. Use `-NoBump` to release the version already in the manifests.
- **Linux**: `scripts/release-linux-gtk.sh` builds the `.deb`, `.rpm` and AppImage into `target/release/bundle/linux-<version>/`, with `SHA256SUMS` and `BUILD-INFO.txt`. Build on the oldest distribution you support: the packages need at least the build machine's glibc.
- **Steam Gaming Mode**: build the engine (`cargo build --release --locked -p lilypad-engine`, in the same build container) and the frontend (`cd decky && npm ci && npm run build`), then `decky/build-plugin.sh` assembles `target/decky/LilyPad-<version>.zip`.
- **Update checks** (`crates/lilypad-core/src/updates.rs`) read GitHub's *latest release*, so a release only reaches existing installs once it is published: not a draft, not a pre-release. The tag must be `vX.Y.Z`, and the Windows tray item links to the asset whose name ends in `-setup.exe`, falling back to the release page if there isn't one. The Decky panel's **Update now** installs the `LilyPad-<version>.zip` asset, verified against its line in the release's `SHA256SUMS`. `release-linux-gtk.sh` only lists the Linux packages there, so add the zip's line (from `target/decky/LilyPad-<version>.zip.sha256`) before uploading; without it Decky installs the update unverified.

### App icon

Icons live in `src-tauri/icons/`, and both builds use them. To regenerate every size from one PNG (512×512 or larger):

```bash
npm run tauri icon path/to/icon.png
```
