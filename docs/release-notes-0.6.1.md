## LilyPad 0.6.1

The Linux version catches up with Windows: sessions are saved as they happen, recovered after a crash, kept separate per account, and never sent twice. Both versions also get fixes for force-stopping, deleted games and adding New Games.

### Linux

This is the first Linux release since 0.5.5.

- **Sessions survive crashes.** A game's session is saved the moment it starts and checked in while it runs. If LilyPad or the PC stops mid-game, the session is recovered at the next start, counted up to the last check-in rather than including the downtime. If the game is still running, tracking simply continues.
- **Nothing is sent twice.** Every session carries its own ID, so retrying after a dropped connection can't log it again.
- **Accounts are kept apart.** A session belongs to the account that was logged in when it started. Switching accounts never submits or shows another account's sessions.
- **Proton games:**
  - Games with long `.exe` names (16+ characters) are now recognised.
  - Proton games not yet in your FrogLog are recorded for their real length.
  - One play is no longer recorded twice.
- **Add Notes.** The auto-submit notification shows for a few seconds with an **Add Notes** button, and the session waits 25 seconds before it is submitted.
- **Newly installed games** are recognised within about 10 seconds instead of up to 5 minutes.
- **No tray?** On GNOME without the AppIndicator extension, the window opens on start, and a new ⋮ menu has Configure, Pending Submissions, New Games, Log Out and Quit.
- **AppImage starts at login correctly.** It previously pointed at a temporary path that vanished after a reboot.
- **Upgrading from 0.5.x:** your pending sessions and New Games are imported automatically. Sessions from before accounts were recorded appear under *From an earlier LilyPad version* in Pending Submissions: choose **Assign to me** or **Discard**.

Packages: `.deb` (Ubuntu 24.04+, Debian 13+), `.rpm` (Fedora 40+), and an AppImage for other distributions with glibc 2.39+. Requires GTK 4.12+ and libadwaita 1.5+.

- **The `.rpm` now installs on Fedora.** Earlier `.rpm`s listed library names that only exist on Debian, so `dnf` refused them.

### Windows and Linux

- **Force-stop fix:** stopping tracking on one game and then starting another no longer ends the second game's session when the first finally closes.
- **Deleted games:** if a linked game was deleted on the website, LilyPad now drops the stale link and works the game out afresh on the next launch, instead of failing every session.
- **Adding New Games is safe to retry:**
  - A dropped connection or failure part-way through no longer creates a duplicate game.
  - If you add the same game from New Games a second time, those sessions are no longer silently skipped.
  - When FrogLog says you already have the game, the sessions are added to that entry instead of the add failing.
- **Your notes are kept:** notes and privacy choices are saved before submitting, so a crash mid-submit keeps them for the retry.
- **Only confirmed sessions count as sent:** a session is marked as submitted only once FrogLog confirms it.

### Known limitations

- Linux with Flatpak Steam: only games you have already linked in Configure are tracked.
- Without a notification service, sessions submit automatically with no chance to add notes; notes can be added afterwards on the FrogLog website.
- Going back to 0.5.x on Linux after upgrading is not supported.
