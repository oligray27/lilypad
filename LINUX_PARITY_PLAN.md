# LilyPad Linux parity plan

Prepared 2026-09-25. Planning only; no application changes or deployments performed.

Implementation started with Phase 0 on 2026-09-25. See [the baseline record](docs/linux-baseline.md) for verified release artifacts, tests, CI preparation and outstanding Linux/environment checks. Phase 0 is not yet complete.

Phase 1 is in progress: submission/retry rules and the auto-submit timer are now shared and validated on Windows and Linux. Phase 2's GTK ledger cutover is done and passes Linux tests, build and KDE smoke tests (including a legacy-data upgrade). Phase 3's code work is done (stale-waiter fixes in both frontends, Proton-safe exit detection, truncated names, exe-path backfill) and unit-tested; its exit gate still needs real game sessions. See [implementation progress](docs/linux-parity-progress.md) for completed work and remaining migration boundaries.

## Target and recommendation

Bring the native GTK4/libadwaita Linux application up to the functional and reliability baseline of **Windows v0.6.0**, keeping the native Linux interface. Prioritise durable recording, account isolation and safe submission before UI polish.

The published [v0.6.0 release](https://github.com/oligray27/lilypad/releases/tag/v0.6.0), checked through the GitHub CLI, was published on 2026-09-17 and currently contains only `LilyPad_0.6.0_x64-setup.exe`. Local HEAD and the v0.6.0 tag both resolve to `cc801431714d50e8a8e58def5dbc674a3c82a4c2`. Although the GTK crate also declares version 0.6.0, that does not establish Linux feature parity or a published Linux build.

This assessment is based on source inspection. Linux compilation, desktop behaviour, the last distributed Linux artifact, and production database migration status have not been verified. Establish those baselines in Phase 0. `PLAN.md` and `IMPLEMENTATION_NOTES.md` contain historical updates that sometimes supersede earlier claims; current source is the implementation baseline.

The recommended approach is to extract the working Windows domain logic into `lilypad-core` in small, tested steps and connect GTK to it. Copying the large Tauri implementation into GTK would reproduce the current maintenance problem. Keep Windows notifications, GTK widgets, platform activation and tray code in their respective frontends.

## Observed parity gaps

Paths below are relative to this repository.

| Area | Windows v0.6.0 | Current GTK implementation | Work required |
|---|---|---|---|
| Durable session lifecycle | SQLite records, checkpoints, completion and recovery | `app.rs` and `session_flow.rs` use `session_persistence` and a legacy active-session file | Connect every lifecycle path to the ledger |
| Unmapped/replay crash recovery | Records unmapped starts and checkpoints | `monitor_glue.rs` explicitly supplies a no-op start callback | Persist before presentation; recover interrupted unmapped play |
| Pending sessions and New Games | Account-filtered ledger queries | Global JSON load/modify/save calls across views, tray and resolution | Replace all readers and writers together; migrate existing data |
| Submission safety | Ledger IDs used as `sync_ref`; session endpoint for regular games | Auto/manual/retry paths pass no key and retain `update_game_hours` | One shared submission service and error policy |
| New Games resolution | Deterministic session keys, ledger-backed queue | Separate resolver uploads without equivalent stable keys | Shared new/existing/replay resolution with resumable progress |
| Account lifecycle | Ledger ownership; cache cleared/staled on login/logout | Mapping file selected at login, but global queues; logout only resets auth | Capture session owner; clear/reload state; reject stale worker results |
| Library refresh failure | Retains good cache and exposes stale status | Retains good cache already, but no equivalent state/status interface | Reuse existing fix and add visible stale/error status |
| Newly installed games | Fingerprint-gated scan every 10 seconds | Full installed-games refresh every 300 seconds | Share scheduling and fingerprint invalidation |
| Process matching | Shared exact/path-aware matcher plus Tauri path backfill | Shared matcher, but no equivalent GTK mapping path-backfill hook | Port backfill while validating Proton identity |
| Auto-submit interception | Application-owned 25-second window | Own timeout already exists, approximately 5.5 seconds; notification failure submits immediately | Shared 25-second decision policy and bounded notification handling |
| Session popup | Ledger ID, explicit discard, terminal queued state | No durable ID; skip closes; failure queues then re-enables Submit | Record-based actions and one terminal result per attempt |
| Diagnostics | Storage status command and storage error reporting | No equivalent ledger status state | Visible storage, recovery, queue and cache status |
| Distribution | Published Windows installer | Local `.deb`, `.rpm`, AppImage build script | Linux build/test/release gates and upgrade validation |

Already present and worth retaining: native GTK views for mappings, pending submissions, New Games/replays, watched directories and exclusions; tray integration; autostart; GApplication activation guard; local heartbeat independent of online presence; shared polling, Steam discovery and Proton handling. This is not a frontend rewrite.

## Scope and success criteria

Parity means equivalent recording, recovery, submission, retry, mapping and settings outcomes. Native presentation differences are acceptable. Retain GTK widgets and compositor-placed windows. Do not make exact Windows popup placement or arbitrary native-Wayland title discovery a release requirement; the current Linux implementation uses X11/EWMH for title matching and needs a documented fallback when that signal is unavailable.

The first supported release should cover x86_64 Linux using the existing three artifact formats. Choose explicit minimum distributions in Phase 0 using actual build/runtime results. A new Flatpak package, ARM builds, Steam Deck gaming-mode integration and additional launcher integrations are separate projects. Testing games launched by Flatpak Steam is in scope because existing discovery already includes its data directory.

Release criteria:

- Every accepted mapped, unmapped and replay session has a durable identity before notifications or submission.
- A crash or ambiguous network result does not silently lose a session or cause a duplicate session on retry.
- Account changes cannot submit or display another account's owned work.
- Existing Linux data is preserved and its ownership can be resolved explicitly.
- Auto-submit works with notifications unavailable, suppressed or unresponsive.
- Core logic remains shared and Windows regression tests/builds remain green.
- Installed packages pass actual Linux upgrade, restart and desktop tests.

## Phase 0 — establish the baseline and Linux validation environment

**Purpose:** replace assumptions about the Linux release and toolchain with a reproducible starting point.

1. Find the most recent release containing GTK Linux assets; record tag, commit, package versions and checksums. Preserve that artifact for upgrade testing rather than assuming v0.5.15 is the Linux baseline.
2. Run the existing Linux app on a dedicated Linux VM or machine with a desktop session and test accounts. Capture its configuration/data layout, startup behaviour and representative logs.
3. Establish Linux CI for core tests, GTK tests/checks and a release binary build. Keep a Windows job for core/Tauri tests and compilation. No CI workflow directory is currently present in this checkout.
4. Confirm toolchain and dependency minimums from the lockfile and builds. GTK currently requests GTK 4.12/libadwaita 1.5 features; package metadata declares unversioned runtime requirements, and the declared Rust 1.75 minimum needs verification against resolved dependencies.
5. Create anonymised legacy fixtures: active session, failed sessions, accumulated New Games, replay, aggregate-only history, missing username, malformed files and more than one account.
6. Verify the deployed API contract and whether `backend/scripts/add_session_sync_ref_uniqueness.sql` is applied. Do not assume that an existing SQL file means production is migrated. Use staging for duplicate-request tests.
7. Capture a Windows v0.6.0 behaviour checklist and the Linux gaps above as tracked work items.

**Exit gate:** reproducible baseline builds, identified upgrade artifact, chosen supported runtime floor, and a documented backend prerequisite. Existing Linux failures are recorded before implementation begins.

## Phase 1 — share the session orchestration boundary

**Depends on:** Phase 0. **Purpose:** make parity maintainable without a risky wholesale refactor.

1. Extract narrowly scoped services from `src-tauri/src/lib.rs` for ledger access/status, session submission, retry classification and New Games resolution. Reuse `session_ledger.rs`, `ledger_session.rs`, `api.rs` and `duration.rs` rather than creating parallel types.
2. Define frontend-neutral events and commands carrying the session ID and captured account/server identity. GTK's current `MonitorEvent` and `SessionEndedData` lack these fields.
3. Make the durable operation happen in the service before emitting a GTK/Tauri presentation event. Sending a GTK channel event must not be the only record that a session started or ended.
4. Extract `src-tauri/src/auto_submit.rs` policy into core, with a controllable timer and notification adapter. Resolve Add Notes/dismissal/timeout exactly once for a specific session.
5. Keep blocking HTTP and storage operations away from GTK widget callbacks. Return typed outcomes such as submitted, pending, discarded, authentication required and storage unavailable.
6. Adapt Tauri to each extraction first and verify unchanged expected behaviour, then connect GTK. Avoid combining all platform changes into one large patch.
7. Introduce injectable storage, API and clock boundaries sufficient for failure tests. Do not build a general application framework.

**Exit gate:** both frontends can call the same domain services; Windows tests and build pass; GTK compiles with the new contracts. No remaining parity-critical rule exists only in a copied GTK helper.

## Phase 2 — migrate Linux persistence and account ownership

**Depends on:** Phase 1. **Purpose:** protect existing data before enabling the newer runtime.

1. Open the existing shared SQLite ledger location after single-instance ownership is established and before the monitor starts.
2. Replace GTK active-session persistence and all pending/New Games JSON reads and writes, including tray counts, resolution, dismissal and popup failure handling. Search the whole GTK crate for legacy helpers before enabling import.
3. Import `active-session.json`, `pending-sessions.json` and `pending-game-submissions.json` transactionally using the existing importer. Preserve original files and snapshots; never import once while legacy writers remain active.
4. Add an ownership-resolution view using the ledger's `adopt` operation. Legacy files contain no trustworthy owner: show enough detail for explicit assignment to the current account or dismissal. Do not auto-assign them merely because someone is logged in. This completes an existing Windows limitation as well as making the Linux upgrade usable.
5. Preserve timestamps, aggregate hours, notes, spoiler/public settings and replay targets. Do not invent per-session history for old aggregate-only records.
6. Capture the account/server at session start and use it throughout recovery and submission. On logout/login, clear or reload mappings, exclusions, watched-directory state, library cache and queue views consistently. Guard background refresh results with account identity or a generation token so a previous account's request cannot repopulate the new cache.
7. Handle missing legacy usernames deliberately; reuse the Windows backfill approach where appropriate. Do not silently copy mappings from another account. Existing mapping filenames use a user key without the server: audit this limitation before claiming full custom-server isolation, and migrate keys explicitly if that scope is supported.
8. Expose ledger-open, migration and write failures. Never show 'saved to pending' unless persistence succeeded. Define degraded operation explicitly instead of treating failed storage as an empty queue.
9. Document rollback: retained JSON is a historical backup, not a reverse migration. Once SQLite receives new play, downgrading to a JSON-only build is unsafe without export/restore procedures. Back up a consistent database, including committed WAL contents.

**Exit gate:** repeatable migration preserves every fixture; malformed input leaves no partial import; ownership can be resolved; all queue views are account-filtered; no GTK legacy writer remains. Migration and storage errors are visible and actionable.

## Phase 3 — durable lifecycle, recovery and Linux process identity

**Depends on:** Phase 2. **Purpose:** give Linux the recording guarantees behind the Windows release.

1. Persist mapped and unmapped starts immediately, including ledger ID, process PID/start time, executable identity and target. Replace the no-op unmapped-start callback.
2. Use `spawn_ledger_heartbeat` for both paths; checkpoints must remain independent of presence preferences. Carry the ID through normal completion, forced completion, recovery, popup and submission.
3. Complete records before presentation/network work. Settle unmapped lifecycle records and credit their New Games history with an idempotent or transactional operation so a crash between those steps cannot lose or double-credit play.
4. Recover all interrupted records before normal detection starts. Resume only a verified original process; bound ended sessions by confirmed checkpoints. For unmapped sessions, preserve the established close/re-detect policy without overlapping credit.
5. Guard stale waiters by session identity, not executable name. Exercise force-stop followed immediately by another launch; old completion must neither clear the new session nor submit the old one again. Cancellable waiters are desirable shared hardening, not something the historical Windows notes prove complete.
6. Validate Linux identity carefully: native binaries, Proton's actual `waitforexitandrun` wrapper, renamed/truncated process names, Wine host executable versus game executable, executable paths with spaces, moved libraries and identical basenames in different installs. Do not persist `python3` or `wine64` as a universal game identity.
7. Add GTK mapping executable-path backfill equivalent to Tauri's hook, using the correct game identity for Proton. Preserve legitimate moved-install fallback without adopting an unrelated same-named game.
8. Reuse the shared install-location fingerprint at the Windows 10-second cadence and keep the library refresh schedule separate. Refresh promptly after watched-directory/exclusion changes and preserve the existing good-cache-on-failure behaviour.
9. Preserve and document the current overlap policy: one mapped session can block further detection; multiple unmapped sessions can exist. Full concurrent mapped tracking is beyond strict v0.6.0 parity.

**Exit gate:** deterministic lifecycle tests plus real native/Proton sessions pass; startup detection, crash recovery, PID reuse, launcher handoff, forced stop and newly installed game discovery behave as specified. Log session IDs and actual identity/detection decisions.

## Phase 4 — submission, retries and New Games resolution

**Depends on:** Phases 1–3 and verified staging API support.

1. Route manual, automatic and retry submissions through one service. Replace GTK regular-game `update_game_hours` calls with session-tracking promotion and session submission, preserving pre-tracked hours.
2. Persist the submission payload and stable `sync_ref` before sending; reuse it on every retry. Retain the remote acknowledgement/reference in the ledger.
3. Reuse API error classification. Authentication errors require reauthentication; transient failures retain work; validation/not-found failures need explicit handling. Only perform orphaned mapping/live-service recovery for the appropriate not-found case, not every network failure.
4. Confirm the actual duplicate-response contract. Do not acknowledge every HTTP 409 indiscriminately: the current backend also has a conflict response for a session still being created. Mark success only when duplicate ownership/success is established.
5. Share new-game, existing-game and replay resolution, including status repair, session tracking, process relinking and cache refresh. Preserve individual session dates and settings.
6. Persist the created remote game/replay ID and per-session progress. Windows notes explicitly retain a duplicate-game risk when creation succeeds and later upload fails; address it in shared code for a reliable Linux rollout rather than copying it.
7. Separately handle a lost response to game creation itself: persisting an ID after a successful response cannot solve that ambiguity. Confirm an idempotent backend creation operation or provide a deliberate reconciliation flow before attempting another creation.
8. Make accumulated session keys stable across retries and independent of mutable array ordering or today's date. Ensure a new batch for the same app cannot reuse a previous batch's keys.
9. Keep pending sessions and New Games disjoint; retry/dismiss the selected durable record and update counts from the ledger.

**Exit gate:** simulate success with a lost response, local acknowledgement failure, partial multi-session upload, game creation followed by upload failure, 401, 404, 409, 429 and offline recovery. Exactly one session exists remotely for each key, and all outstanding work remains discoverable.

## Phase 5 — GTK workflows and desktop integration

**Depends on:** stable services from Phases 2–4. UI preparation can occur earlier, but must not revive legacy writes.

1. Bring the existing GTK screens onto durable service outcomes: active tracking, recovery, pending sessions, New Games/replays, mappings, watched directories and exclusions. Keep native layout and navigation.
2. Add storage/recovery/stale-library status and legacy ownership actions. Distinguish an empty queue from an unavailable queue.
3. Use the shared 25-second interception policy. Notification failure must still allow the full interception interval; Add Notes wins simultaneous dismissal; late actions cannot reopen an acknowledged session.
4. Provide an in-app route to intercept pending auto-submission when desktop notifications are unavailable. Close or retire notification handles where possible and prevent indefinite accumulation of blocked action-waiter threads.
5. Keep Submit disabled while in flight. A successfully queued failure is terminal for that popup; subsequent retry occurs against the same record. Explicit 'Do not record' discards; closing the window without a choice retains pending work.
6. Ensure tray badges, menu counts and open views update after submission, resolution, discard, recovery and account changes. Retain the single-instance activation behaviour.
7. Test tray unavailable/startup failure and launching the app again: the user must still be able to reach the window and quit. Test GNOME and KDE, notification actions, focus behaviour, keyboard navigation, scaling and light/dark appearance.
8. Audit autostart for packaged and AppImage installs. `autostart.rs` currently writes `current_exe()` directly: verify AppImage does not save an ephemeral mount path; quote desktop Exec paths correctly and test paths containing spaces. Make failures visible.

**Exit gate:** all parity workflows pass on the chosen Linux desktops, with notifications and tray support independently disabled. No GTK widget access occurs from worker threads; no duplicate submission is possible through repeated UI actions.

## Phase 6 — packaging, qualification and release

**Depends on:** all earlier exit gates. Package smoke builds should begin in Phase 0.

1. Make `scripts/release-linux-gtk.sh` reproducible: use the committed lockfile, pin/check downloaded packaging tools, fail on missing artifacts and isolate outputs by version/architecture. Avoid accidentally uploading stale wildcard matches.
2. Verify `.deb` and `.rpm` dependency metadata against actual GTK/libadwaita/system-library requirements and the chosen minimum distributions. Test AppImage on clean hosts for runtime libraries, icons, schemas, certificates, theme and desktop integration.
3. Align package, crate, displayed and release versions. Build a new coordinated release from one reviewed commit rather than silently replacing the published Windows v0.6.0 baseline with changed code.
4. Add explicit Linux asset publication to the release process; the current GTK script only builds local artifacts. Publish checksums and clear installation/update instructions. There is no requirement here to add an automatic updater.
5. Replace the outdated Linux README instructions, which primarily describe Tauri/WebKit dependencies, with separate Windows/Tauri and Linux/GTK setup, build, install, data migration and troubleshooting instructions.
6. Run a prerelease with representative Linux users and retain diagnostic logs from complete play/restart/retry cycles. Compare CPU, memory and background work against the Phase 0 baseline; choose acceptable thresholds from measured results.
7. Run the qualification matrix below, then publish production artifacts and release notes describing supported environments, migration and known platform limitations. Prepare a recovery path that preserves the ledger.

**Exit gate:** clean install and upgrade work for all advertised formats; Windows regression qualification passes; no unresolved data-loss, cross-account submission or duplicate-session defects; artifacts correspond to the tested commit.

## Qualification matrix

Use automated shared-service tests for combinatorial failures, plus targeted real desktop/game tests. A headless build does not validate tray or notification integration.

| Dimension | Required cases |
|---|---|
| Desktop | GNOME Wayland and KDE Wayland; one supported X11 session; tray unavailable |
| Game source | Native Steam, Proton Steam, Flatpak Steam launch, watched non-Steam directory |
| Identity | Same basename/different path, PID reuse, wrapper process, moved install, launcher handoff |
| Timing | Game running before LilyPad, launch during cache startup, install while app runs, short session, overnight/date boundary |
| Lifecycle | Normal exit, force-stop/relaunch, app kill, game outliving app, machine restart, suspend/resume |
| Storage | Fresh profile, legacy upgrade, malformed JSON, read-only/disk-full simulation, newer unsupported schema, crash during migration |
| Network | Offline start/end, lost success response, expired auth, rate limit, permanent rejection, partial resolution |
| Accounts | A to B with pending/active work, logout during upload, stale refresh after switch, unowned legacy records |
| Notifications | Normal action, dismissal, simultaneous action/dismissal, no service, suppressed notification, late action |
| Packaging | `.deb`, `.rpm`, AppImage; install, upgrade, relaunch, autostart, uninstall/reinstall with data retained |

Commands to establish in CI include `cargo test -p lilypad-core --locked`, `cargo test -p lilypad-gtk --locked`, and `cargo build -p lilypad-gtk --release --locked` on Linux, plus `cargo test -p lilypad --locked` and the Windows build. Add staging integration tests for backend idempotency and packaged desktop smoke tests. Linux-only `cfg` tests must execute on Linux, not merely pass in a Windows core test run.

## Suggested delivery sequence and effort

These are planning ranges for one developer familiar with the code, not measured estimates or deadlines. Re-estimate after Phase 0, especially if GTK does not compile or backend changes are required.

| Phase | Indicative engineering days | Reviewable deliverable |
|---|---:|---|
| 0: Baseline | 1–2 | Linux build baseline, fixtures, supported environments, backend contract |
| 1: Shared services | 3–5 | Small shared-service extractions with Windows regression coverage |
| 2: Migration/accounts | 3–5 | Complete ledger cutover and ownership workflow |
| 3: Lifecycle/detection | 3–5 | Crash-safe lifecycle and validated Linux identities |
| 4: Submission/resolution | 3–5 | Idempotent submissions and resumable resolution |
| 5: GTK integration | 2–4 | Complete desktop workflows and status surfaces |
| 6: Qualification/release | 2–4 | Tested packages, documentation and release candidate |

Total indicative effort: **17–30 engineering days**, plus elapsed beta observation and any production/backend coordination. Work can be delivered as small PRs; the runtime storage cutover must remain internally coherent. Do not ship a hybrid that imports a legacy file once and then continues writing new sessions to that file.

Strict parity ends at matching v0.6.0 outcomes. Legacy ownership usability, resumable game creation, stale worker protection and verification of conflict responses are explicitly identified reliability completion work, not claims that Windows already implements them perfectly. They should be resolved in shared code where necessary to satisfy the release safety criteria above. Broader features such as fully concurrent mapped sessions remain separate.
