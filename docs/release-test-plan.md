# LilyPad release test plan

Run before every release. Part 1 is automated; Part 2 is what only a person at the machine can check. Tick items as you go. A failure is anything that doesn't match **Expect**.

## Part 1 — automated (about 3 minutes)

From `lilypad/` on the Windows machine:

```powershell
./scripts/test-all.ps1
```

It runs, and prints PASS/FAIL/SKIP for each:

| Check | What it proves |
|---|---|
| Windows core + Tauri tests | Session store, submission/retry, New Games resolution, process matching, recovery rules (~130 tests) |
| Windows real-process tests | A real running program is detected, recorded, ended with its real duration; crash recovery resumes a running game and closes a stopped one; a reused pid is not mistaken for the original |
| Windows Tauri build | The Windows app compiles |
| GTK type-check on Windows | The Linux app's code type- and borrow-checks |
| Linux tests + release build | The same suites on Linux, plus GTK tests and a 25 s no-notification-daemon auto-submit test |
| Linux desktop smoke (×3) | Starts on KDE and hands a second launch to the running copy; upgrades v0.5.5 data (5 records migrated, originals untouched); runs with no tray and no notification daemon |

- [ ] `test-all.ps1` ends with no FAIL. The real-session smoke test is skipped while your own LilyPad is running on bazzite; close it first for a full run.

Once the release packages are built (`scripts/release-linux-gtk.sh`, in the build container), on bazzite:

```bash
bash scripts/test-linux-packages.sh <linux-<version> package dir> <previous-release dir>
```

- [ ] All package checks pass: fresh `.deb` install on Ubuntu 24.04 and Debian 13, fresh `.rpm` install on Fedora 41, and upgrades over the previous `.deb` and `.rpm`.
- [ ] The AppImage passes `scripts/smoke-linux-desktop.sh <AppImage>` (with `DISPLAY` and `XAUTHORITY` set: it runs through XWayland), and its autostart entry points at the AppImage file itself.

## Part 2 — manual

### Setup

- [ ] The backend is deployed and `add_games_client_ref.sql` has been run.
- [ ] Use the **test account**, never a real one.
- [ ] Start LilyPad on bazzite from Konsole, keeping every run's log:
  `RUST_LOG=lilypad_gtk=info,lilypad_core=info <path-to-lilypad-gtk> 2>&1 | tee -a ~/lilypad-test.log`
- [ ] For a clean start, quit LilyPad and delete `~/.local/share/froglog-lilypad/` first.
- [ ] After each section, `python3 scripts/show-ledger.py` should show nothing stuck `active` (unless a game is running) and no duplicates (two records with the same start and game).

### A. Account

- [ ] **Log in.** *Expect:* window moves to About; tray shows Configure, About, Logout.
- [ ] **Log out and in as a second account** with a session pending on the first. *Expect:* the first account's pending session is not listed; logging back in shows it again.
- [ ] **Header menu (⋮)**: every item opens the right page; Log Out logs out; Quit quits.

### B. Native Linux game

- [ ] **Launch a mapped native game.** *Expect:* "Tracking Started" notification within a few seconds; tray icon and tooltip show "Now Tracking".
- [ ] **Online presence**: turn "share now playing" on mid-game. *Expect:* you appear as playing on the website within ~2 minutes.
- [ ] **Quit after at least a minute, auto-submit on.** *Expect:* popup shows for about 5 s, then the session appears on the website with the right length.
- [ ] **Add Notes** from the notification. *Expect:* session window opens; notes, spoiler and hide-from-public are saved on the website.
- [ ] **Auto-submit off.** *Expect:* session window opens on exit. "Do not record session" discards it: nothing on the website, nothing in Pending.
- [ ] **Close the session window without choosing.** *Expect:* the session appears in Pending Submissions; Retry submits it.

### C. Proton game

- [ ] **Mapped Proton game with a short `.exe` name.** *Expect:* tracked for its full length.
- [ ] **Mapped Proton game whose `.exe` name is 16+ characters.** *Expect:* still detected (Linux shortens the name) and tracked for its full length.
- [ ] **Unmapped Proton game not in your library.** *Expect:* on exit, "Session Recorded … isn't in your FrogLog yet"; one New Games entry with the real length; **one** ledger record, not two.

### D. New Games

- [ ] **Create new** (IGDB search). *Expect:* game created on the website with the session and a Steam link; the next launch is tracked as a mapped game.
- [ ] **Map to existing.** *Expect:* sessions added to that game; it becomes "In Progress" if it was finished.
- [ ] **Finished game relaunched → Replay.** *Expect:* a second entry marked as a replay; the old one is untouched.
- [ ] **Dismiss.** *Expect:* entry gone; nothing created.
- [ ] **Interrupted resolve**: disconnect the network, press Create, reconnect, press Create again. *Expect:* exactly one game on the website, each session once.

### E. Failures and retries

- [ ] **Offline end of session**: disconnect, finish a game, reconnect. *Expect:* "Session Queued"; Pending shows it with a readable reason. Retry while still offline shows "Submitting…" and then "Retry failed" on its own line under the reason, and the row keeps its layout. Retry once back online submits it once.
- [ ] **Deleted game**: delete a mapped game on the website, then play it. *Expect:* the session queues with "no longer exists"; the next launch re-resolves (New Games or the new entry) rather than failing again.
- [ ] **Library refresh failure**: disconnect the network for over 5 minutes, then launch an owned but unmapped game. *Expect:* it is not filed as a New Game (the last good library copy is kept); the log says "could not refresh the library".

### F. Crash recovery and force-stop

- [ ] **Kill LilyPad mid-game, restart with the game still running.** *Expect:* tracking resumes on the same session; on exit it is submitted once, with the full length.
- [ ] **Kill LilyPad mid-game, close the game, then restart LilyPad.** *Expect:* the session is submitted, credited only up to the last check-in (within ~2 minutes of when LilyPad died), not up to the restart.
- [ ] **Stop Tracking, then immediately launch a different game.** *Expect:* the stopped game's popup appears; the second game is tracked normally and is not ended when the first game finally closes.
- [ ] **Stop Tracking, then relaunch the same game after closing it.** *Expect:* tracked as a new session.

### G. Installs and detection

- [ ] **Install a game while LilyPad runs, launch it straight away.** *Expect:* detected within ~10 s of the install finishing.
- [ ] **Non-Steam game in a watched directory.** *Expect:* detected; resolving it creates a "PC (Non-Steam)" entry with no Steam link.
- [ ] **Excluded app** (e.g. Wallpaper Engine). *Expect:* never tracked.

### H. Desktop integration

- [ ] **Relaunch LilyPad while running.** *Expect:* the existing window comes forward; no second tray icon.
- [ ] **Close the window.** *Expect:* LilyPad keeps running in the tray.
- [ ] **Reboot.** *Expect:* LilyPad starts at login.
- [ ] **Light and dark theme, 150% scaling.** *Expect:* everything readable; nothing clipped.
- [ ] *(Optional)* **GNOME without the AppIndicator extension.** *Expect:* window opens on every start; Quit and Log Out work from the ⋮ menu.

### I. Upgrade from an older Linux version

- [ ] Install v0.5.5 (`.deb` or AppImage), use it (one failed submission, one New Games entry, one game running when you quit), then install the new version. *Expect:*
  - Pending Submissions lists them under "From an earlier LilyPad version".
  - **Assign to me** moves each into your queue; the interrupted one appears with its length up to the last check-in.
  - **Discard** removes one.
  - The old JSON files are still in `~/.local/share/froglog-lilypad/`.

### J. Windows regression

Windows shares the session code, and several fixes changed it (stale waiter after force-stop, deleted-game detection, New Games resolution, saving submissions before sending).

- [ ] Track a game, auto-submit it, and check the website.
- [ ] Stop Tracking, then immediately launch another game (as in section F).
- [ ] Kill LilyPad mid-game and restart with the game still running (as in section F).
- [ ] Resolve a New Games entry (Create new) and check a single game is created.
- [ ] Retry a queued session from Pending Submissions.

### Known limitations (not failures)

- Flatpak Steam: only games you have already mapped are tracked.
- On KDE the auto-submit popup disappears after 5 s, but the session still submits at 25 s.
- With no notification daemon, sessions auto-submit without an Add Notes option; notes can be added on the website.
