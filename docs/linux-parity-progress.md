# Linux parity implementation progress

## 2026-09-25 — Phase 1: shared submission, retry and interception

This is the first Phase 1 implementation slice. It follows the [Linux baseline](linux-baseline.md) and does **not** complete the ledger/account migration or the entire parity plan.

### Changes

- Moved the Tauri auto-submit policy into `lilypad-core::auto_submit`. The existing five timer/action tests moved with it; three additional tests cover failed presentation, stalled presentation and interception through a presentation adapter. Clock-based core tests use paused Tokio time rather than short wall-clock races.
- GTK now uses the same 25-second interception window. Notification creation and response handling are asynchronous and driven alongside the shared deadline. An unavailable notification service does not short-circuit the window; a stalled creation/action future is dropped when the deadline resolves. Notification closing has a one-second bound. The separate New Games notification wrapper remains unchanged.
- Extracted `submit_play_session`, retry policy and failure explanations into `lilypad-core::submission`. Tauri and GTK automatic/manual/retry flows use the shared rules. GTK no longer calls `update_game_hours` for regular mappings: it promotes them to session tracking through the existing API client, preserving pre-tracked hours, then submits a session.
- The retry service only attempts Live Service target recovery after a 404 for a non-live entry. It retains notes, privacy settings, date, hours and any supplied submission key. Recovery requires a unique exact title match; ambiguous matches are left pending instead of choosing the first entry.
- Corrected the HTTP 409 classification to `Conflict`. The backend source returns an existing session as HTTP success; 409 can mean the session is still being created. Tauri no longer acknowledges these unconfirmed failures. They remain pending for retry.
- Added narrow `SessionApi`/`RetryApi` boundaries and eight deterministic service tests. These cover routing, payload preservation, promotion failure, ambiguous retry, conflicts, moved-game recovery, non-404 failures and ambiguous targets.
- Added a GTK notification-adapter test under a private D-Bus session without a notification daemon. It initially caught a real runtime panic: D-Bus selected Tokio I/O while only the timer driver was enabled. The GTK runtime now enables its I/O driver as well. The test passes and waits the full 25 seconds.

### Validation

- Windows: 102 core unit tests, 2 migration integration tests, and 2 remaining Tauri tests passed. The original Tauri interception tests now run in core on both operating systems.
- `cargo build -p lilypad --locked --offline` passed on Windows.
- Linux: 104 core unit tests, 2 migration integration tests, 15 GTK model tests, and the isolated notification-unavailable test passed.
- Linux release build passed; KDE Wayland startup and second-instance activation passed with a fresh isolated profile (`desktop-profile.SSuWBO`).
- Existing GTK deprecation/dead-field warnings remain; no new compiler errors.
- No real account submissions, production migrations, releases, commits or pushes were performed.

### Scope still open

GTK's active session and both queues still use the legacy JSON writers. The shared service accepts the caller's `sync_ref`; GTK's initial submissions did not previously carry a durable key, so this slice does not invent a different key only when retrying those old entries. Full idempotency requires the planned ledger cutover and verification of the deployed backend indexes.

Ledger access/status, record-based frontend events, ownership resolution, account lifecycle, and New Games creation/resolution orchestration still need to be shared/integrated. Existing New Games key-namespace and partial-creation issues are not changed by this slice. Windows' explicit known-disabled-notification fast path also remains; the shared timer applies when interception is attempted.

The next coherent storage change must switch all GTK legacy readers/writers together before importing their files. Do not enable a one-time import while old JSON writers are active. No migration was enabled in this slice.

## 2026-09-25 — Ledger cutover prerequisite: atomic New Games transfer

Inspection of the existing Windows ledger flow found that unmapped completion and recovery settled the tracking record separately from adding hours to New Games. A failure between those writes could lose the session or leave ambiguous credit. This prerequisite is complete; the GTK storage cutover remains open.

- Added `SessionLedger::complete_unmapped`, which accumulates the New Games entry and settles its source in one immediate SQLite transaction. Repeated completion cannot add the same source twice, including after reopening the database.
- The source record supplies account, executable, game target, replay metadata and original play date. Normal completion uses recorded timestamps; recovery stops at the last durable checkpoint and excludes downtime.
- Updated Windows normal completion and recovery to use this shared operation, removing the separate finish/credit/dismiss sequence. A write failure retains the active source for recovery.
- Raw unmapped records no longer appear as retryable mapped sessions. Older pending unmapped records are retained but are not automatically transferred: the previous multi-write sequence cannot prove whether their hours were already credited. A review/reconciliation flow remains necessary for those records.
- Added four regression tests covering reopen/repeat completion and owner isolation, recovery with unknown ownership, rollback after aggregate insertion when source settlement fails, and preservation of ambiguous older records. Updated the queue-separation fixture to use a real mapped target.

Windows validation: 106 core unit tests, 2 migration integration tests and 2 Tauri tests passed. Linux validation: 108 core unit tests, 2 migration integration tests, 15 GTK model tests and the isolated notification-unavailable test passed; the GTK release build passed with existing warnings. Logs are saved locally under `target/linux-phase1`. No GTK legacy import has been enabled; all GTK storage readers/writers still need to move together, followed by ownership UI and account-switch checks.

## 2026-09-25 — GTK account cache lifecycle

GTK now routes login/logout through `AppState::change_account`. Each successful change replaces the process map, clears the library and installed-game caches, and invalidates background work from every earlier login. Installed-game scanning depends on account-specific watched directories and exclusions, so it needs the same guard as the remote library cache. Login refreshes both immediately instead of waiting for the five-minute timer. Failed credential saves are surfaced rather than silently reporting success.

Refresh publication holds the authentication read lock while checking its generation and replacing the cache. Logout holds the corresponding write lock while invalidating and clearing it. This prevents a late result from account A overwriting account B's library, including A → B → A and same-account re-login. Existing good library data is still retained if a request fails within the same login.

The monitor previously kept temporary library read guards alive across auto-link/replay callbacks. Those guards now end before entering frontend code, avoiding lock inversion with account changes. A core regression test checks that the auto-link callback can acquire the library write lock.

Five GTK model tests cover logout, a deliberately delayed old-account refresh, returning to the same account, same-account re-login, and detection preference replacement. These tests use synthetic credentials and data; they do not contact FrogLog or write a real profile.

This is account **cache** isolation, not completion of account-owned session storage. Active sessions, delayed session popups, queue ownership/adoption, and in-flight New Games resolution still require the coordinated GTK ledger cutover. GTK legacy imports remain disabled.

Validation: Linux passed 109 core unit tests, 2 migration integration tests, 20 GTK model tests, and the isolated notification-unavailable test. The GTK release build passed with the existing warnings. KDE Wayland startup and second-instance activation passed using isolated profile `desktop-profile.EAwpEw`. The new monitor regression also passed on Windows. Logs are under `target/linux-phase1`; no production account was used.

## 2026-09-25 — Account-owned retry queue (completed after interruption)

The previous session was cut off mid-slice by a usage limit. Its patches had applied; they are now verified. `SessionLedger` gained `pending_sessions`, `pending_session` and `dismiss_owned`: queue rows are read for a known owner, re-read at action time, and reconstructed from the recorded play date rather than today's. Tauri's retry and discard commands use them, taking credentials and owner from one snapshot. Five ledger tests cover date/ID stability, payload preservation, cross-account and unowned rejection, missing timestamps and legacy rows. Windows: 112 core, 2 migration and 2 Tauri tests passed.

## 2026-09-25 — Phase 2: GTK ledger cutover

GTK no longer reads or writes any legacy JSON queue. The switch was made in one change, as the plan requires: every GTK caller of `load/save_pending_sessions`, `load/record/remove_pending_game_submission(s)` and `session_persistence` (13 call sites across `app.rs`, `session_flow.rs`, `monitor_glue.rs`, `resolve.rs`, `tray.rs`, `views/pending.rs`, `views/new_games.rs`, `views/session.rs`, `views/mappings.rs`) was replaced. The one-time import of all three legacy files is enabled in the same change.

### Shared core: `session_store`

Added `lilypad-core::session_store`, the frontend-neutral layer the Tauri build had grown inline in `lib.rs`: `SessionStore` (ledger + last-error reporting, `None` meaning unknown rather than empty), `open_ledger_with_import`, `find_original_process`, begin/complete/queue/acknowledge/discard, New Games, unowned-record listing, adoption and startup recovery. `submission` gained `remote_reference` and `log_each_pending_session`. Tauri now uses the shared `open_ledger_with_import`, `find_original_process`, `remote_reference` and `log_each_pending_session`; its duplicate copies were removed. Tauri's `AppState`/`with_ledger` were deliberately left as they are to keep this change's Windows surface small; moving Tauri onto `SessionStore` is a follow-up.

### GTK behaviour now

- **Startup:** the primary instance opens the ledger in `build_window` (a second launch only forwards activation) and imports `active-session.json`, `pending-sessions.json` and `pending-game-submissions.json` transactionally. An open/import failure is logged, shown as a notification, and every queue view says "unavailable" rather than "empty". Recovery then runs before the monitor: a still-running original process (pid + OS start time) resumes its record with its own exit waiter; others are closed at their last checkpoint and go through the normal end path; interrupted unmapped sessions are credited into New Games.
- **Mapped sessions:** the record is inserted on the monitor thread before the start event is sent, and completed on the monitor thread before the end event. Force-stop completes the record synchronously; the process's later exit is swallowed and releases the block. The ledger heartbeat replaces `session_persistence`'s (checkpoint independent of presence; presence independent of the store).
- **Unmapped sessions:** recorded at start and checkpointed (replacing the former no-op callback), completed atomically into New Games via `complete_unmapped`. If the start could not be stored, the session is credited directly; if nothing could be stored, the user gets a "Session Not Saved" notification instead of a "Go to New Games" prompt.
- **Submission:** automatic, popup and retry paths send the record ID as `sync_ref`, acknowledge the record on success and attach the failed payload to it on failure. "Saved to Pending Submissions" is only claimed when the write succeeded.
- **Account ownership:** a session belongs to the account logged in when it started. If a different account is logged in when it ends, it is neither auto-submitted nor offered in a popup; it stays pending for its owner. The popup re-checks ownership at submit time.
- **Session popup:** a queued failure is terminal (Submit stays disabled, the other button becomes Close). "Do not record session" discards the record; closing the window without choosing leaves it pending.
- **Pending Submissions:** account-filtered ledger rows; Retry re-reads the row, submits with its key and acknowledges; Delete discards. A new section lists records imported from pre-ledger versions (no owner) with **Assign to me** / **Discard**. Assigning an interrupted legacy session closes it at its last checkpoint into the retry queue. Nothing is auto-assigned.
- **New Games:** account-filtered ledger rows; dismiss/resolve settle the ledger entry. Resolution now uses stable `newgame:<appid>#<index>` keys (it previously sent none), so a partial failure can be retried without duplicating uploaded sessions. If the server work succeeds but the local settle fails, the user is told to dismiss rather than re-resolve.
- **Tray / Configure counts:** from the ledger, including unowned imports. An unreadable store shows "unavailable", not zero.
- **Installed games:** fingerprint-gated rescan every 10 seconds (Windows cadence), library refresh still every 5 minutes. Account changes reset the fingerprint.

### Validation

- Windows: 118 core unit tests (6 new `session_store` tests: queue/acknowledge-once, orphan queueing, unavailable-is-unknown, adopting an interrupted legacy record, resume-vs-close recovery, unmapped recovery), 2 migration and 2 Tauri tests passed; Tauri builds.
- GTK: **type- and borrow-checked on Windows only**, via the new `scripts/check-gtk-on-windows.ps1` (skips pkg-config for a `cargo check`). The only errors are the two known Linux-only notify-rust APIs in `notify.rs`. A planted use-after-move confirmed borrow checking still runs. Four new GTK tests (normal completion, force-stop swallowing, owner-only submission, logged-out tracking) are written but **not yet run**.
- Linux (Bazzite, once reachable): `validate-linux.sh` passed — 120 core unit tests, 2 migration integration tests, 24 GTK tests (the 4 new ones included), the isolated notification-unavailable test (full 25 s), and the release build. KDE Wayland startup and second-instance activation passed with a fresh profile (`desktop-profile.dPFf4s`); the log shows the store opening in the isolated profile before the monitor starts.
- Legacy upgrade smoke: `smoke-linux-desktop.sh` gained an optional `LILYPAD_SMOKE_SEED` directory. Seeded with the synthetic v0.5.5 fixtures, the real application migrated all 5 records at startup, left the original JSON files byte-identical, and passed the same startup/second-instance checks (`desktop-profile.acpvqj`).
- **Not yet done:** no logged-in desktop session, real game, real account, or installed-package upgrade (`.deb`/`.rpm`/AppImage from v0.5.5) has been exercised. The ownership view and session popup have not been clicked through.

### Still open

- Run the Linux validation above, then an upgrade test from the v0.5.5 packages with populated legacy files.
- Recovery runs only at startup for the account logged in then; another account's interrupted records wait for a restart while it is logged in.
- Username backfill for pre-v0.4.4 logins (Tauri's `backfill_username`) is not ported; such logins get a token-hash identity.
- Mapping executable-path backfill and Proton identity validation (Phase 3) are not done.
- Tauri still has its own `with_ledger`/`AppState` plumbing rather than `SessionStore`.

## 2026-09-25 — Phase 3: stale waiters and Linux process identity

Phase 3 items 1–4 (durable starts, heartbeats, completion before presentation, recovery before detection) and item 8 (10-second install fingerprint) landed with the Phase 2 cutover above. This slice covers items 5–7 and 9. All are found by reading code and tested with unit tests; **none has been exercised against a real game yet**.

### Stale waiters (item 5), both frontends

Force-stopping game X, launching game Y, then X finally exiting did three wrong things, on Windows as well as Linux:

- The shared monitor's end handler cleared `current_session` unconditionally, dropping Y, so the next scan started Y a **second** time. It now clears only the exiting process's own session (`monitor::clear_session_for`).
- Both frontends' end paths took whatever ledger id was active and completed it, closing **Y's** record while Y was still running. The active id is now a `session_store::ActiveRecord`, tied to its process: a waiter takes only its own process's id, and none at all after a force-stop.
- The recovery (resumed-session) waiters in both frontends had the same unconditional clears; they now use the same guarded helpers.

### Linux process identity (items 6–7)

- **Exit detection by start time.** "Is our process still running" is decided by pid plus OS start time, not by name. By reading the code, an unmapped Proton game was reported as exited on its first check: it is tracked through Proton's `waitforexitandrun` wrapper (reported as `proton`, exe `python3`), while the waiter compared against the game's `.exe` name. This fix also makes pid reuse impossible to mistake for the tracked process. Name matching remains only as a fallback when the start time cannot be read.
- **No relaunch adoption by a shared runtime.** A session identified by `python3`/`wine*`/`proton` never adopts a "successor". Otherwise, after a game exits, any unrelated Python process could be taken as its relaunch and keep the session open.
- **Auto-linked Proton sessions are named after the game.** When an already-owned game is found through Proton's wrapper, the session was named `python3.x`, so a force-stop blocked every other `python3.x` process but not the game's own Wine process, which was then tracked again on the next scan. The session now uses the mapping's `.exe` name.
- **Truncated names.** Linux reports process names cut to 15 bytes, and Wine names the process after the `.exe`, so `HotlineMiami.exe` appears as `HotlineMiami.ex` and never matched its mapping. The scanner now accepts a 15-byte name that is the prefix of exactly one mapped name (ambiguous prefixes match nothing), and the exit waiter recognises the truncated form.
- **Executable-path backfill, Proton-safe.** GTK now pins a mapping to the executable it is seen running as (the Tauri hook's equivalent), via the shared `config::backfill_mapping_exe_path`, which Tauri now uses too. A Wine/Proton host binary is never recorded, and is ignored when matching even if an older build recorded one. Recording `wine64-preloader` would identify nothing, and after a Proton upgrade the old binary, still installed, would make the mapping match nothing at all. Proton games therefore stay name-matched, which is the pre-existing behaviour.

### Overlap policy (item 9)

Unchanged and already documented on `run_poll_loop`: one mapped session at a time, with further mapped games picked up when it ends; several unmapped sessions may run concurrently.

### Validation

- Windows: 122 core unit tests (4 new), 2 migration, 2 Tauri tests passed; Tauri builds. GTK type-check: only the 2 known Linux-only notify.rs errors.
- Linux (Bazzite): 125 core unit tests (including the Linux-only truncated-name test), 2 migration, 25 GTK tests (1 new: a force-stopped game's late exit leaves the next game's record alone), notification-unavailable test, release build, and both KDE smoke tests (fresh profile and legacy-seeded, 5 records migrated) passed.

### Still open for Phase 3's exit gate

## 2026-09-25 — First real-desktop test run (Bazzite, KDE Wayland, test account)

The user ran the release build from Konsole and played four sessions: two native Steam games (Brotato, Dead Cells) and one Proton game (About Fishing). All five resulting ledger records ended settled (acknowledged or dismissed); nothing was left active or pending.

What the log confirms:
- Native sessions start, checkpoint, end and auto-submit against their record (`session:2081`, `session:2083` acknowledged). Native mappings are pinned to their real executable (`.../Dead Cells/deadcells`).
- **The truncated-name fix works on a real Proton game.** `About Fishing.exe` (17 bytes, reported by Linux as `About Fishing.e`) matched its mapping, tracked for its full two minutes, and was correctly *not* pinned to the Wine binary.
- Presence follows the setting mid-session: enabling it during Brotato took effect on the next heartbeat.
- A New Games entry was created and resolved end-to-end (`session:14226`, 1 session logged with its sync key); discarding from the popup dismissed the record.

Fixes made from it:
- **Deleted game treated as a network blip (both frontends).** Brotato auto-linked to game 13072, which had been deleted on the website since the last library refresh. `get_game_raw` reported the miss as a bare `Game not found`, which `ApiFailure::classify` read as transient, so the dead-mapping cleanup never ran, Tauri's `drop_dead_mapping` included. It now reports `404: Game <id> not found`. GTK gained the same cleanup (`config::remove_dead_mapping` plus a library refresh), which it previously lacked.
- **Auto-submit notification duration.** KDE shows a notification for exactly the requested timeout, so it stayed up for the whole 25-second window. It now requests 5 seconds; expiry reports it closed, which submits, as dismissal does. The 25-second window remains the upper bound if the daemon never reports back. To confirm on the next run: a session is submitted roughly 5 seconds after the notification appears.
- The "could not be waited on (not supported on this platform)" line was logged for every normal Linux exit. Off Windows it now logs just "exited".

Validation after the fixes: Windows 122 core, 2 migration and 2 Tauri tests; Linux 125 core, 2 migration and 25 GTK tests, the notification test and the release build all passed.

## 2026-09-25 — Second real-desktop test run

Fifteen more ledger records, all settled. Covered: repeated New Games create/resolve cycles for native and Proton games (`session:14227`–`14230`), mapped sessions after resolution, and **LilyPad killed mid-game**: on restart the interrupted Brotato record was resumed against the still-running process (pid + start time), its exit was observed, and it was submitted (`session:2092`).

Found and fixed:
- **One Proton play recorded twice.** At 13:34:05 the same About Fishing launch produced an unmapped record (Proton's wrapper, to New Games) and a mapped record (the game's Wine process), both 169 s. The existing guard only fired when the mapped game resolved by appid; this mapping pointed at a deleted game, and a mapping to an entry with no Steam link would slip past the same way, crediting the play twice. `maybe_start_unmapped_tracking` now declines when any mapping covers the matched executable, respecting pinned paths. New regression test.
- **Notification timing on KDE, still to confirm.** The recovered Brotato session was submitted 25 s after it exited, not ~5 s, which suggests Plasma does not report an expired notification as closed (it moves it to history). The popup is shorter, but the decision still waits for the full window.

Log capture: `tee` without `-a` overwrote the log on each launch, so only the last run's log survived; the ledger was used for the rest.

Flatpak Steam could not be tested on this machine. By reading the code, it is expected to track mapped games (matched by executable name) but not detect unmapped or already-owned games. The sandbox reports library paths in `libraryfolders.vdf`, and `/proc/<pid>/exe`, as seen inside the sandbox, not the host paths the installed-games scan uses. Also, `find_steam_root` stops at the first Steam install it finds, so a machine with both native and Flatpak Steam only scans one.

Real sessions on Bazzite are still required: a native Steam game, a Proton game (short and 16+ character `.exe` names), a Flatpak Steam launch, a watched non-Steam directory, force-stop followed immediately by another launch, a game running before LilyPad starts, and a LilyPad kill mid-session. Two things in particular to confirm: what a Proton game's process tree actually reports (comm/exe of the Wine process and of the wrapper), and that an unmapped Proton session now lasts as long as the game.
