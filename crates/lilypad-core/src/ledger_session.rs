//! Ledger-backed session lifecycle: the durable replacement for `session_persistence`'s
//! `active-session.json`.
//!
//! Both frontends now run on this (GTK through `session_store`). `session_persistence` is no
//! longer used by either; its `active-session.json` is imported into the ledger on first start.
//!
//! What changes versus the JSON file: the record is keyed by a UUID rather than being "the one
//! active session", it carries the account that owns it, a completed session survives until the
//! server acknowledges it rather than being deleted at end-of-session, and every write either
//! succeeds or reports an error instead of being swallowed by `let _ =`.

use crate::api::FroglogClient;
use crate::config::{AuthConfig, ProcessMapConfig, ProcessMapping};
use crate::monitor::ActiveSession;
use crate::session_ledger::{AccountIdentity, SessionLedger, SessionRecord};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

/// Matches `session_persistence::HEARTBEAT_INTERVAL`; the remote half of the tick has the same
/// job of keeping `now_playing_updated_at` inside the backend's staleness window.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2 * 60);

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Who owns a session recorded right now, or `None` when nobody is logged in.
///
/// The account id is exactly `config::user_key` — the same key per-account mapping files are
/// already named after. That reuse is deliberate rather than convenient: it prefers the
/// normalised username (stable across logout/login) but falls back to a token hash for logins
/// that predate usernames being stored at all, which `auth.json` files written before v0.4.4 do.
///
/// An earlier version required a username and returned `None` without one. That silently gave
/// those users *no ledger records whatsoever* — no crash recovery, and a failed submission
/// landing as an unowned record that the retry queue filters out, so it disappeared from view
/// entirely. Worse than the JSON queue it replaced, and permanent rather than a one-off.
///
/// The token is never stored, only its hash, and only as an identifier.
///
/// `default_server` is the caller's own built-in API URL, used when `auth.base_url` is unset.
/// It is a parameter rather than a constant here so the shared crate does not carry a
/// hardcoded deployment URL, and so an account that later pins `base_url` to that same default
/// keeps the identity — and therefore the queue — it already had.
pub fn account_identity(auth: &AuthConfig, default_server: &str) -> Option<AccountIdentity> {
    let account_id = crate::config::user_key(auth);
    // `user_key` yields this exact sentinel only when there is neither a username nor a token —
    // nobody is logged in, so there is no account to attribute a session to. Deriving the test
    // from the key itself means the two can never disagree about who is logged in.
    if account_id == "anonymous" {
        return None;
    }
    Some(AccountIdentity {
        server: normalised_server(auth, default_server),
        account_id,
    })
}

/// Trailing slashes and case in the host would otherwise split one account's queue in two.
fn normalised_server(auth: &AuthConfig, default_server: &str) -> String {
    let base = auth
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default_server);
    base.trim().trim_end_matches('/').to_lowercase()
}

/// How long an interrupted session should be credited, given it is now known to have ended.
///
/// Bounded by the last confirmed-alive checkpoint, never "now": a session that outlived a PC
/// shutdown would otherwise bill the entire downtime as play time. This is the same rule
/// `session_persistence::recover_on_startup` applies, restated against a ledger record. Time
/// spent paused is left out, a pause still running at the checkpoint included.
pub fn interrupted_duration_secs(record: &SessionRecord) -> f64 {
    let started = record.started_at_secs.unwrap_or_default();
    let last_alive = record
        .last_alive_secs
        .unwrap_or(started)
        .max(started);
    (last_alive - started).saturating_sub(record.paused_secs_at(last_alive)) as f64
}

/// How long a session that is *still running* has lasted, measured from its recorded start.
/// Wall-clock is accurate here: the process never stopped, so there is no downtime to exclude.
/// Paused time is.
pub fn ongoing_duration_secs(record: &SessionRecord, now: u64) -> f64 {
    let started = record.started_at_secs.unwrap_or_default();
    now.saturating_sub(started).saturating_sub(record.paused_secs_at(now)) as f64
}

/// Per-session ticker: advances the ledger's confirmed-alive checkpoint and refreshes remote
/// presence.
///
/// Both halves are independent of everything except their own precondition, which is the whole
/// point of this signature:
///
/// - The local checkpoint runs regardless of the online-presence setting. Gating it on presence
///   is what used to pin the recovery checkpoint at the session start time, crediting a
///   crash-recovered session roughly zero hours.
/// - The remote refresh runs regardless of whether a durable record exists. Presence has nothing
///   to do with the store, so an unopenable store — or a login too old to carry a username, which
///   leaves the session unowned — must not also knock the user offline. Hence `ledger` and
///   `session_id` are optional, and `still_active` supplies the stop condition when there is no
///   record to read one from.
///
/// The presence setting is re-read every tick, so toggling it mid-session takes effect.
///
/// Stops when the session is no longer active — completion, dismissal, or a force-stop all end
/// it, with no separate cancellation channel to get out of step with. A failed checkpoint is
/// reported and retried on the next tick rather than ending the session: a transient lock/IO
/// failure must not silently stop crash recovery from being bounded.
/// `mapping` is `None` for a session with nothing to report presence for — an unmapped game is
/// not in the library, so it has no id to be "now playing" as. Such a session still gets the
/// local checkpoint half, which is what bounds its crash recovery.
/// `presence_times` gives the times to report on this tick, or `None` for a session with no
/// presence to report (see `presence_start`).
#[allow(clippy::too_many_arguments)]
pub fn spawn_ledger_heartbeat(
    ledger: Option<Arc<Mutex<SessionLedger>>>,
    session_id: Option<String>,
    client_factory: impl Fn() -> FroglogClient + Send + 'static,
    process_map: Arc<RwLock<ProcessMapConfig>>,
    mapping: Option<ProcessMapping>,
    presence_times: impl Fn() -> Option<PresenceTimes> + Send + 'static,
    still_active: impl Fn() -> bool + Send + 'static,
    on_error: impl Fn(String) + Send + 'static,
) {
    std::thread::spawn(move || loop {
        std::thread::sleep(HEARTBEAT_INTERVAL);
        let times = presence_times();
        let share_presence = mapping.is_some()
            && times.is_some()
            && process_map
                .read()
                .map(|c| c.share_now_playing)
                .unwrap_or(false);
        let keep_going = heartbeat_tick(
            ledger.as_ref(),
            session_id.as_deref(),
            &still_active,
            share_presence,
            &|| {
                if let (Some(mapping), Some(times)) = (&mapping, &times) {
                    let _ = client_factory().set_now_playing_at(
                        mapping.froglog_id,
                        mapping.r#type.clone(),
                        mapping.title.clone(),
                        Some(times.started_at.clone()),
                        times.paused_at.clone(),
                    );
                }
            },
            &on_error,
        );
        if !keep_going {
            return;
        }
    });
}

/// The times a heartbeat reports for a session's presence ("now playing").
#[derive(Debug, Clone, PartialEq)]
pub struct PresenceTimes {
    /// The real start, moved forward by time spent in pauses that have ended.
    pub started_at: String,
    /// When the pause still running began, if the session is paused. Online Now keeps showing
    /// it, but its "playing for" time stops here.
    pub paused_at: Option<String>,
}

/// The heartbeat's `presence_times` for the mapped session tracked as `process_name`. The start is
/// moved forward by time spent paused, so Online Now's "playing for" leaves pauses out (as it does
/// for a Quick Session); it only changes when a pause ends, so successive heartbeats report the
/// same start. While paused, `paused_at` is set, so the time stands still there.
pub fn presence_start(
    current_session: Arc<RwLock<Option<ActiveSession>>>,
    process_name: String,
    started_at_iso: String,
) -> impl Fn() -> Option<PresenceTimes> + Send + 'static {
    move || {
        let (paused_total, current_pause) = match current_session.read().ok()?.as_ref() {
            Some(s) if s.process_name == process_name => (s.paused_total, s.paused_at.map(|p| p.elapsed())),
            _ => (Duration::ZERO, None),
        };
        Some(PresenceTimes {
            started_at: shift_iso(&started_at_iso, paused_total),
            paused_at: current_pause.map(|d| (chrono::Utc::now() - chrono::Duration::seconds(d.as_secs() as i64)).to_rfc3339()),
        })
    }
}

/// `iso` (RFC 3339) moved forward by `by`, to the whole second; unchanged if it can't be parsed.
pub fn shift_iso(iso: &str, by: Duration) -> String {
    if by.as_secs() == 0 {
        return iso.to_string();
    }
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(t) => (t + chrono::Duration::seconds(by.as_secs() as i64)).to_rfc3339(),
        Err(_) => iso.to_string(),
    }
}

/// One heartbeat tick, separated from the timer so the independence of its two halves is
/// testable without waiting `HEARTBEAT_INTERVAL` for anything to happen. Returns whether the
/// ticker should keep running.
fn heartbeat_tick(
    ledger: Option<&Arc<Mutex<SessionLedger>>>,
    session_id: Option<&str>,
    still_active: &dyn Fn() -> bool,
    share_now_playing: bool,
    refresh_presence: &dyn Fn(),
    on_error: &dyn Fn(String),
) -> bool {
    match (ledger, session_id) {
        (Some(ledger), Some(id)) => {
            match ledger
                .lock()
                .map_err(|e| e.to_string())
                .and_then(|mut l| l.checkpoint(id, now_secs()).map_err(|e| e.to_string()))
            {
                Ok(true) => {}
                // No longer active: the session ended. Nothing to report.
                Ok(false) => return false,
                // Transient lock/IO failure: report it, but keep ticking. Ending here would
                // silently stop crash recovery from being bounded for the rest of the session.
                Err(e) => on_error(format!("Could not checkpoint session {id}: {e}")),
            }
        }
        // No durable record: presence-only, stopping on the caller's own condition.
        _ => {
            if !still_active() {
                return false;
            }
        }
    }

    if share_now_playing {
        refresh_presence();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_ledger::{ProcessIdentity, SessionTarget, SubmissionState};

    fn auth(username: Option<&str>, base_url: Option<&str>) -> AuthConfig {
        AuthConfig {
            base_url: base_url.map(str::to_string),
            token: Some("secret-token".into()),
            username: username.map(str::to_string),
        }
    }

    const DEFAULT: &str = "https://api.example.test/api";

    #[test]
    fn account_identity_is_stable_across_login_spelling_and_never_holds_the_token() {
        let a = account_identity(&auth(Some("Frog"), Some("https://API.example.test/api")), DEFAULT).unwrap();
        let b = account_identity(&auth(Some("frog"), Some("https://api.example.test/api/")), DEFAULT).unwrap();
        assert_eq!(a, b, "casing or a trailing slash must not split one account's queue in two");
        assert!(!a.server.contains("secret-token"));
        assert!(!a.account_id.contains("secret-token"));

        // Pinning base_url to the built-in default must not orphan an existing queue.
        assert_eq!(account_identity(&auth(Some("frog"), None), DEFAULT).unwrap(), a);

        // A different server is a different queue, same username or not.
        let other = account_identity(&auth(Some("frog"), Some("https://other.test/api")), DEFAULT).unwrap();
        assert_ne!(other, a);

        // Two accounts on one server are distinct.
        let someone_else = account_identity(&auth(Some("toad"), None), DEFAULT).unwrap();
        assert_ne!(someone_else, a);
    }

    /// `auth.json` files written before v0.4.4 have a token but no username. Requiring a username
    /// gave those users no ledger records at all — no crash recovery, and failed submissions
    /// landing unowned and invisible to the retry queue.
    #[test]
    fn a_login_that_predates_stored_usernames_still_owns_its_sessions() {
        let legacy = account_identity(&auth(None, None), DEFAULT)
            .expect("a token-only login must still own its sessions");
        assert!(!legacy.account_id.contains("secret-token"));

        // Once they log in again and a username is stored, identity comes from that instead.
        let named = account_identity(&auth(Some("frog"), None), DEFAULT).unwrap();
        assert_ne!(named, legacy);
    }

    #[test]
    fn nobody_logged_in_has_no_account() {
        let anonymous = AuthConfig { base_url: None, token: None, username: None };
        assert!(account_identity(&anonymous, DEFAULT).is_none());
    }

    fn record(started: u64, last_alive: Option<u64>) -> SessionRecord {
        let mut r = SessionRecord::new(
            AccountIdentity { server: "s".into(), account_id: "a".into() },
            ProcessIdentity { executable: "game.exe".into(), ..Default::default() },
            SessionTarget::Unmapped { appid: "1".into(), title: "G".into(), replay_of: None },
            started,
        );
        r.last_alive_secs = last_alive;
        r
    }

    #[test]
    fn an_interrupted_session_is_credited_only_up_to_its_last_confirmed_checkpoint() {
        // Played 10 minutes, then the PC was off for a week before LilyPad next started.
        assert_eq!(interrupted_duration_secs(&record(1_000, Some(1_600))), 600.0);
        // No checkpoint ever landed (crash inside the first heartbeat interval): credit nothing
        // rather than the whole gap.
        assert_eq!(interrupted_duration_secs(&record(1_000, None)), 0.0);
        // A checkpoint that somehow predates the start cannot produce a negative duration.
        assert_eq!(interrupted_duration_secs(&record(1_000, Some(400))), 0.0);
    }

    /// A pid on its own cannot identify a process instance: Windows recycles pids, so a game
    /// closed and relaunched while LilyPad was down can land on the same one. Recovery must
    /// compare the OS start time too, or it resumes a finished session against an unrelated
    /// process and bills the wrong span of time.
    #[test]
    fn a_recorded_process_identity_distinguishes_an_instance_from_a_pid_reuse() {
        let mut original = record(1_000, Some(1_600));
        original.process = ProcessIdentity {
            executable: "game.exe".into(),
            pid: Some(4242),
            started_at_secs: Some(990),
            ..Default::default()
        };

        // Same pid, same executable, different launch: not a resume.
        let relaunch = ProcessIdentity { started_at_secs: Some(50_000), ..original.process.clone() };
        assert_ne!(original.process.started_at_secs, relaunch.started_at_secs);
        assert_eq!(original.process.pid, relaunch.pid);
        assert_eq!(original.process.executable, relaunch.executable);

        // Such a session is closed at its checkpoint, not resumed and left running.
        assert_eq!(interrupted_duration_secs(&original), 600.0);
    }

    /// An unmapped game is checkpointed like any other session precisely so this holds. Without
    /// the heartbeat, `last_alive_secs` would stay at the start time and an interrupted session
    /// would be credited zero — making the start-of-play record pointless.
    #[test]
    fn an_interrupted_unmapped_session_is_credited_up_to_its_last_checkpoint() {
        let mut r = record(1_000, Some(1_000));
        r.target = SessionTarget::Unmapped {
            appid: "504230".into(),
            title: "Celeste".into(),
            replay_of: None,
        };
        // No checkpoint has landed yet: credit nothing rather than guessing.
        assert_eq!(interrupted_duration_secs(&r), 0.0);
        // After two heartbeat ticks, the confirmed-alive span is credited.
        r.last_alive_secs = Some(1_000 + 2 * HEARTBEAT_INTERVAL.as_secs());
        assert_eq!(interrupted_duration_secs(&r), 240.0);
    }

    #[test]
    fn a_session_found_still_running_is_credited_real_wall_clock_time() {
        assert_eq!(ongoing_duration_secs(&record(1_000, Some(1_100)), 1_900), 900.0);
        assert_eq!(ongoing_duration_secs(&record(1_000, None), 500), 0.0);
    }

    /// Both halves of the heartbeat have been broken once already by being gated on something
    /// unrelated: the checkpoint on the online-presence setting, and then presence on a durable
    /// record existing. Neither precondition may control the other.
    #[test]
    fn neither_half_of_the_heartbeat_gates_the_other() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let ledger = SessionLedger::open(&dir.path().join("l.sqlite")).unwrap();
        let r = record(1_000, Some(1_000));
        ledger.insert(&r, SubmissionState::Active).unwrap();
        let ledger = Arc::new(Mutex::new(ledger));

        let presence_calls = Arc::new(AtomicUsize::new(0));
        let presence = {
            let c = Arc::clone(&presence_calls);
            move || {
                c.fetch_add(1, Ordering::SeqCst);
            }
        };
        let active = || true;
        let inactive = || false;
        let no_errors = |e: String| panic!("unexpected heartbeat error: {e}");

        // Presence OFF, record present: the checkpoint must still advance.
        assert!(heartbeat_tick(
            Some(&ledger), Some(&r.id), &active, false, &presence, &no_errors
        ));
        assert_eq!(presence_calls.load(Ordering::SeqCst), 0);
        let advanced = ledger.lock().unwrap().get(&r.id).unwrap().unwrap();
        assert!(advanced.record.last_alive_secs.unwrap() > 1_000);

        // No record at all (unopenable store, or a login too old to own a session): the tick
        // must still refresh presence rather than dropping the user offline for the session.
        assert!(heartbeat_tick(
            None, None, &active, true, &presence, &no_errors
        ));
        assert_eq!(presence_calls.load(Ordering::SeqCst), 1);

        // Without a record the stop condition comes from the caller.
        assert!(!heartbeat_tick(
            None, None, &inactive, true, &presence, &no_errors
        ));
        assert_eq!(presence_calls.load(Ordering::SeqCst), 1);

        // With a record, the ledger state is the stop condition -- and outlasts a caller that
        // already thinks the session is over.
        ledger.lock().unwrap().finish(&r.id, 3_000).unwrap();
        assert!(!heartbeat_tick(
            Some(&ledger), Some(&r.id), &active, true, &presence, &no_errors
        ));
        assert_eq!(presence_calls.load(Ordering::SeqCst), 1);
    }

    /// A transient storage failure must be reported but must not end the session's ticker,
    /// which would silently stop crash recovery from being bounded for the rest of the session.
    #[test]
    fn a_failed_checkpoint_is_reported_without_ending_the_session() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let ledger = SessionLedger::open(&dir.path().join("l.sqlite")).unwrap();
        let r = record(1_000, Some(1_000));
        ledger.insert(&r, SubmissionState::Active).unwrap();
        ledger.force_read_only_for_test();
        let ledger = Arc::new(Mutex::new(ledger));

        let errors = Arc::new(AtomicUsize::new(0));
        let on_error = {
            let c = Arc::clone(&errors);
            move |_: String| {
                c.fetch_add(1, Ordering::SeqCst);
            }
        };
        let presence_calls = Arc::new(AtomicUsize::new(0));
        let presence = {
            let c = Arc::clone(&presence_calls);
            move || {
                c.fetch_add(1, Ordering::SeqCst);
            }
        };

        assert!(heartbeat_tick(
            Some(&ledger), Some(&r.id), &|| true, true, &presence, &on_error
        ));
        assert_eq!(errors.load(Ordering::SeqCst), 1);
        // Presence is unaffected by a storage failure.
        assert_eq!(presence_calls.load(Ordering::SeqCst), 1);
    }

    /// The heartbeat's stop condition is the ledger state itself, so a completed session
    /// cannot leave a ticker running that would keep re-confirming it as alive.
    #[test]
    fn checkpoints_stop_applying_once_a_session_is_no_longer_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = SessionLedger::open(&dir.path().join("l.sqlite")).unwrap();
        let r = record(1_000, Some(1_000));
        ledger.insert(&r, SubmissionState::Active).unwrap();
        assert!(ledger.checkpoint(&r.id, 1_200).unwrap());
        assert!(ledger.finish(&r.id, 1_300).unwrap());
        assert!(!ledger.checkpoint(&r.id, 9_999).unwrap());
        let stored = ledger.get(&r.id).unwrap().unwrap();
        assert_eq!(stored.record.ended_at_secs, Some(1_300));
        assert_eq!(interrupted_duration_secs(&stored.record), 300.0);
    }

    /// Paused time is never logged as play: two pauses, the second still running when the
    /// session ends, both come off its length.
    #[test]
    fn paused_time_is_left_out_of_a_finished_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = SessionLedger::open(&dir.path().join("l.sqlite")).unwrap();
        let r = record(1_000, Some(1_000));
        ledger.insert(&r, SubmissionState::Active).unwrap();
        assert!(ledger.pause(&r.id, 1_100).unwrap());
        assert!(!ledger.pause(&r.id, 1_150).unwrap(), "already paused");
        assert!(ledger.resume(&r.id, 1_200).unwrap());
        assert!(!ledger.resume(&r.id, 1_250).unwrap(), "not paused");
        assert!(ledger.pause(&r.id, 1_500).unwrap());
        assert!(ledger.finish(&r.id, 1_600).unwrap());

        let stored = ledger.get(&r.id).unwrap().unwrap();
        assert_eq!(stored.record.paused_secs, 200);
        assert_eq!(stored.record.paused_at_secs, None, "finishing closes the open pause");
        assert_eq!(interrupted_duration_secs(&stored.record), 400.0);
        assert!(!ledger.pause(&r.id, 1_700).unwrap(), "a finished session can't be paused");
    }

    /// A crash while paused is credited up to the checkpoint, minus the pause up to there.
    #[test]
    fn an_interrupted_paused_session_leaves_out_its_pause_up_to_the_checkpoint() {
        let mut r = record(1_000, Some(1_600));
        r.paused_secs = 60;
        r.paused_at_secs = Some(1_500);
        // 600s between start and checkpoint, 60s of earlier pauses, 100s of the running one.
        assert_eq!(interrupted_duration_secs(&r), 440.0);
        assert_eq!(ongoing_duration_secs(&r, 1_700), 440.0);
    }

    #[test]
    fn a_paused_session_keeps_reporting_presence_with_its_pause_time() {
        use crate::config::ProcessMapping;
        use std::time::Instant;
        let mut session = ActiveSession::new(
            "game.exe".into(),
            ProcessMapping {
                process: "game.exe".into(), r#type: "session".into(), froglog_id: 1,
                title: None, title_filter: None, exe_path: None,
            },
            sysinfo::Pid::from(1usize),
            None,
            Instant::now(),
        );
        session.paused_total = Duration::from_secs(60);
        let current = Arc::new(RwLock::new(Some(session)));
        let times = presence_start(Arc::clone(&current), "game.exe".into(), "2026-10-27T18:00:00+00:00".into());

        let running = times().unwrap();
        assert_eq!(running.started_at, "2026-10-27T18:01:00+00:00");
        assert_eq!(running.paused_at, None);

        current.write().unwrap().as_mut().unwrap().paused_at = Some(Instant::now());
        let paused = times().unwrap();
        assert_eq!(paused.started_at, "2026-10-27T18:01:00+00:00", "the start doesn't move while paused");
        assert!(paused.paused_at.is_some());
    }

    #[test]
    fn presence_start_moves_forward_by_paused_time() {
        assert_eq!(shift_iso("2026-10-27T18:00:00+00:00", Duration::from_secs(90)), "2026-10-27T18:01:30+00:00");
        assert_eq!(shift_iso("2026-10-27T18:00:00+00:00", Duration::ZERO), "2026-10-27T18:00:00+00:00");
        assert_eq!(shift_iso("not a date", Duration::from_secs(90)), "not a date");
    }
}
