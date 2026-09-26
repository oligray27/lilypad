# LilyPad for Decky

LilyPad in Steam Gaming Mode (Steam Deck, Bazzite and other gamescope sessions). It tracks the games you play and logs them to FrogLog:

- **Auto-submit on** (the default): every session is submitted as soon as the game closes, with a message you choose in the plugin's settings (blank for none).
- **Auto-submit off**: when a game closes, a dialog asks you to submit the session, with notes, spoiler and visibility for session-tracked and live-service games, or not record it. A session you close the dialog on waits under *Sessions to submit* in the Quick Access Menu.

The Quick Access Menu has:

- what is being tracked and for how long, and stopping a session attributed to the wrong game (the same dialog then asks what to do with the time)
- Pending Submissions: retry or delete
- New Games: add to FrogLog, map to a game you already have, log as a replay, or dismiss
- login, auto-submit, the session message, and "Mirror online presence to FrogLog" settings

Toasts tell you when tracking starts, when a session has been submitted, and when a game isn't in your FrogLog yet.

The desktop app's auto-submit settings don't apply in Gaming Mode, and the plugin leaves them unchanged.

## How it works

The plugin bundles `bin/lilypad-engine`, LilyPad's headless tracking engine. It runs the same code as the LilyPad desktop app for Linux, against the same data folder (`~/.local/share/froglog-lilypad`): the same login, game links and session history.

The desktop app is optional. If it is installed too, only one of the two tracks at a time: the desktop app takes over while it is running in Desktop Mode, and the engine resumes when you return to Gaming Mode. A session in progress carries across the switch.

Games in your Steam library that are already in FrogLog are linked automatically the first time you play them, and anything else is offered under New Games. Linking an executable to a game by hand needs the desktop app.

## Install

From a release zip, with Decky's developer options enabled: Decky settings > Developer > Install Plugin from ZIP.

## Build

See `build-plugin.sh`.
