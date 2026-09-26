## LilyPad 0.6.2

Notes are back in Steam Gaming Mode, deleted games no longer leave sessions stuck in Pending Submissions, and every version now shows how long the current session has run.

### Steam Gaming Mode (Decky plugin)

- **Auto-submit switch.** On (the default), sessions submit as soon as a game closes, as before. Off, a **LilyPad: Session Ended** dialog opens when a game closes: add notes, mark spoilers or hide the session from public, then **Submit to FrogLog** or **Do not record session**. Closing the dialog keeps the session under *Sessions to submit* in the Quick Access Menu.
- **Session clock.** The panel shows **Now Tracking: *game*** with the session's length (HH:MM:SS) underneath.
- **Stop tracking** opens the same dialog straight away, and a stopped session is no longer listed in Pending Submissions as well.
- **New Games** uses the desktop app's wording (**Map to Existing**, **Log Hours**), and sessions logged into a game created from New Games are noted "Session logged from LilyPad via SteamOS".
- The presence switch is now **Mirror online presence to FrogLog**.

### Windows and Linux

- **Session length in the tray.** The tray tooltip and menu show the session's length so far, e.g. "Now Tracking: Hades (1h 23m)", updated every 30 seconds.

### All versions

- **Deleted games go to New Games.** If a game linked in LilyPad was deleted on the website, LilyPad now notices when the game starts, drops the link, and records the session under New Games instead of failing to submit it every time. A session already tracked against a deleted game also moves to New Games when it fails to submit, or when you press **Retry** on it in Pending Submissions (Linux and Gaming Mode; on Windows, the next launch is recorded under New Games). Sessions LilyPad can't place stay in Pending Submissions.
- **Shorter error messages.** A session that couldn't be sent now says "Error submitting session, check connection." instead of showing the full request address, and a problem on FrogLog's side says so.

Packages: Windows installer; `.deb` (Ubuntu 24.04+, Debian 13+), `.rpm` (Fedora 40+) and an AppImage for other distributions with glibc 2.39+; `LilyPad-0.6.2.zip` for Decky Loader (Decky settings > Developer > Install Plugin from ZIP).

### Known limitations

- Linux with Flatpak Steam: only games you have already linked in Configure are tracked.
- Without a notification service on Linux, sessions submit automatically with no chance to add notes; notes can be added afterwards on the FrogLog website.
- Sessions already in Pending Submissions keep their old, longer error message until retried.
