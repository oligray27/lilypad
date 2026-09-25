# LilyPad for Decky

LilyPad in Steam Gaming Mode (Steam Deck, Bazzite and other gamescope sessions). It tracks the games you play and submits every session to FrogLog as soon as the game closes. Gaming Mode has no notes: each session is sent with one message you choose in the plugin's settings (blank for none). Notes can be added on the FrogLog website.

The Quick Access Menu has:

- what is being tracked, and stopping a session attributed to the wrong game (you then submit its time or don't record it)
- Pending Submissions: retry or delete
- New Games: add to FrogLog, add the time to a game you already have, log as a replay, or dismiss
- login, the session message, and "show what I'm playing" settings

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
