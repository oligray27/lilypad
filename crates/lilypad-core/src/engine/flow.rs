//! What happens when a tracked session starts, ends, or is recovered after a crash: the
//! auto-submit decision tree, ported from the Tauri build's `handle_session_ended`. Runs off any
//! UI thread; anything the user must see or decide goes through the `Frontend`.
//!
//! Every session here is identified by its ledger record. The record is completed before this
//! code runs, so a crash anywhere in here leaves a pending session rather than nothing; a
//! successful submission acknowledges that record, and a failed one attaches its payload to it.

use super::{EngineState, FrontendRef, SubmitPolicy, DEFAULT_API_URL};
use crate::api::{ApiFailure, FroglogClient};
use crate::auto_submit::Outcome;
use crate::config::{AuthConfig, ProcessMapping};
use crate::ledger_session::{self, now_secs};
use crate::monitor::ActiveSession;
use crate::session_ledger::{AccountIdentity, SessionRecord, Submission};
use crate::session_store::{Completion, Recovered};
use crate::submission::{explain_failure, remote_reference, submit_play_session};
use std::time::{Duration, Instant, SystemTime};

/// The note sent with a session LilyPad submitted without asking.
pub const DEFAULT_AUTO_SUBMIT_NOTE: &str = "Session auto submitted with LilyPad";

/// The note Gaming Mode sends: the user's own (blank for none), or the standard one.
pub fn gaming_mode_note(state: &EngineState) -> Option<String> {
    match &state.process_map.read().unwrap().gaming_mode_note {
        Some(note) => Some(note.trim().to_string()).filter(|n| !n.is_empty()),
        None => Some(DEFAULT_AUTO_SUBMIT_NOTE.to_string()),
    }
}

/// A finished session waiting for the user to submit it (with notes) or not record it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionEndedData {
    pub process_name: String,
    pub mapping: ProcessMapping,
    pub duration_secs: f64,
    pub forced: bool,
    /// The record this session was completed as; the decision applies to exactly this one.
    pub ledger_id: Option<String>,
}

/// Hours for FrogLog: 2 decimal places, minimum 0.01 (matches the Tauri build).
pub fn round_hours(duration_secs: f64) -> f64 {
    let raw = duration_secs / 3600.0;
    let r = (raw * 100.0).round() / 100.0;
    if r < 0.01 {
        0.01
    } else {
        r
    }
}

pub fn format_duration(duration_secs: f64) -> String {
    crate::duration::format_session_duration(duration_secs)
}

pub fn client_for(auth: &AuthConfig) -> FroglogClient {
    let base = auth
        .base_url
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_API_URL.to_string());
    let mut c = FroglogClient::new(base);
    c.set_token(auth.token.clone());
    c
}

/// What submitting a session sends, saved against its record before sending (see
/// `SessionStore::save_attempt`) and again, with the failure, if it fails.
pub fn submission_for(
    mapping: &ProcessMapping,
    hours: f64,
    date: String,
    notes: Option<String>,
    spoiler: bool,
    is_public: bool,
) -> Submission {
    Submission {
        game_id: mapping.froglog_id,
        game_type: mapping.r#type.clone(),
        title: mapping.title.clone().unwrap_or_else(|| mapping.process.clone()),
        hours,
        date,
        notes,
        spoiler,
        is_public,
        last_error: None,
        failed_at: None,
    }
}

/// Unlinks a game whose FrogLog entry has been deleted, so its next launch is detected as a game
/// not in FrogLog (New Games) instead of failing to submit again, and re-reads the library.
pub fn unlink_deleted_game(state: &EngineState, mapping: &ProcessMapping) {
    log::warn!(
        "[LilyPad] {} #{} no longer exists in FrogLog; unlinking {}",
        mapping.r#type, mapping.froglog_id, mapping.process
    );
    let auth = state.auth.read().unwrap().clone();
    if let Err(e) = crate::config::remove_dead_mapping(
        &state.process_map, &auth, &mapping.process, &mapping.r#type, mapping.froglog_id,
    ) {
        log::warn!("[LilyPad] could not save the process map after removing a dead mapping: {e}");
    }
    state.refresh_library_index();
}

/// A session failed to submit because its game has been deleted from FrogLog: unlinks the game
/// and moves the session into New Games, under the installed game its executable belongs to.
/// Returns that game's name. `None` leaves the session where it is, in Pending Submissions:
/// LilyPad could not tell which installed game it was (e.g. an executable linked by hand from
/// outside any Steam library or watched folder).
pub fn move_to_new_games(state: &EngineState, ledger_id: &str) -> Option<String> {
    let store = state.store();
    let (executable, mapping) = store.mapped_session(ledger_id)?;
    unlink_deleted_game(state, &mapping);
    let account = state.account()?;
    let game = {
        let games = state.installed_games.read().unwrap();
        let exe_path = mapping.exe_path.as_deref().map(std::path::Path::new);
        crate::steam::find_installed_game_by_executable(&executable, exe_path, &games).cloned()
    };
    let Some(game) = game else {
        log::info!("[LilyPad] could not tell which installed game {executable} is; leaving session {ledger_id} in Pending Submissions");
        return None;
    };
    store.move_to_new_games(ledger_id, &account, &game.appid, &game.name)?;
    Some(game.name)
}

/// `submission` with the reason it failed, for the retry queue.
pub fn failed(mut submission: Submission, error: &str) -> Submission {
    submission.last_error = Some(explain_failure(error));
    submission.failed_at = Some(chrono::Local::now().to_rfc3339());
    submission
}

/// Whether the logged-in account may submit `ledger_id`. A session belongs to the account that
/// was logged in when it started; submitting it with anyone else's credentials would log one
/// person's play on another's profile. A record with no known owner (storage unavailable) is
/// allowed through, since there is nothing to protect it from.
pub fn may_submit(state: &EngineState, ledger_id: Option<&str>, account: Option<&AccountIdentity>) -> bool {
    let Some(id) = ledger_id else { return true };
    match state.store().owner(id) {
        Some(Some(owner)) => account == Some(&owner),
        _ => true,
    }
}

/// Reports "now playing" to FrogLog when a tracked session starts, if the user has online
/// presence enabled, and starts the heartbeat that checkpoints the ledger record (bounding
/// crash recovery) and keeps that presence fresh.
///
/// The heartbeat runs regardless of the presence setting: only its remote half is about
/// presence, and `spawn_ledger_heartbeat` re-reads the setting each tick. It also runs with no
/// record (storage unavailable), so presence still works; `still_active` stops it then.
pub fn handle_session_started(
    state: &EngineState,
    process_name: &str,
    mapping: &ProcessMapping,
    ledger_id: Option<String>,
    started_at_iso: String,
    announce: bool,
) {
    let share_now_playing = state.process_map.read().unwrap().share_now_playing;
    let auth = state.auth.read().unwrap().clone();

    if announce && share_now_playing {
        let auth = auth.clone();
        let mapping = mapping.clone();
        let started_at_iso = started_at_iso.clone();
        std::thread::spawn(move || {
            let client = client_for(&auth);
            let _ = client.set_now_playing(mapping.froglog_id, mapping.r#type.clone(), mapping.title.clone(), Some(started_at_iso));
        });
    }

    let store = state.store();
    let current_session = state.current_session.clone();
    let process_name = process_name.to_string();
    let error_store = store.clone();
    ledger_session::spawn_ledger_heartbeat(
        store.ledger(),
        ledger_id,
        move || client_for(&auth),
        state.process_map.clone(),
        Some(mapping.clone()),
        started_at_iso,
        move || {
            current_session
                .read()
                .map(|s| s.as_ref().is_some_and(|s| s.process_name == process_name))
                .unwrap_or(false)
        },
        move |e| error_store.report_error(e),
    );
}

/// Entry point once a tracked session has ended (normal exit, force-stop, or recovery) and
/// its record has been completed.
#[allow(clippy::too_many_arguments)]
pub fn handle_session_ended(
    state: EngineState,
    frontend: FrontendRef,
    process_name: String,
    mapping: ProcessMapping,
    duration_secs: f64,
    forced: bool,
    ledger_id: Option<String>,
) {
    let auth = state.auth.read().unwrap().clone();
    let account = state.store().account(&auth);

    // Always clear now_playing, regardless of auto-submit / force-stop.
    {
        let auth = auth.clone();
        std::thread::spawn(move || {
            let client = client_for(&auth);
            let _ = client.clear_now_playing();
        });
    }
    frontend.changed();

    // The account changed while this game was running. The record stays pending under the
    // account that played it, and reappears in its queue when that account logs back in.
    if !may_submit(&state, ledger_id.as_deref(), account.as_ref()) {
        let title = mapping.title.clone().unwrap_or_else(|| mapping.process.clone());
        log::info!("[LilyPad] session {ledger_id:?} for {title} belongs to another account; leaving it pending");
        frontend.notify(
            "Session Saved",
            &format!("{title} was started under a different FrogLog account. It is waiting in that account's Pending Submissions."),
        );
        return;
    }

    let decide = |forced| SessionEndedData {
        process_name: process_name.clone(),
        mapping: mapping.clone(),
        duration_secs,
        forced,
        ledger_id: ledger_id.clone(),
    };

    // Force-stop never auto-submits (the user is likely correcting a bad
    // mapping) — always ask, so they can still log the time.
    if forced {
        frontend.needs_decision(decide(true));
        return;
    }

    let hours = round_hours(duration_secs);

    match frontend.submit_policy(&state) {
        SubmitPolicy::Always => {
            let notes = gaming_mode_note(&state);
            std::thread::spawn(move || {
                submit_now(&state, &frontend, &auth, account, &mapping, hours, duration_secs, notes, true, ledger_id);
            });
            return;
        }
        SubmitPolicy::Ask => {
            frontend.needs_decision(decide(false));
            return;
        }
        SubmitPolicy::Settings => {}
    }

    let auto_submit = {
        let cfg = state.process_map.read().unwrap();
        match mapping.r#type.as_str() {
            "live" => cfg.auto_submit_live,
            "session" => cfg.auto_submit_session,
            _ => cfg.auto_submit_regular,
        }
    };

    if !auto_submit {
        frontend.needs_decision(decide(false));
        return;
    }

    let is_notes_type = mapping.r#type.eq_ignore_ascii_case("live") || mapping.r#type.eq_ignore_ascii_case("session");
    let notes = Some(DEFAULT_AUTO_SUBMIT_NOTE.to_string());

    if is_notes_type {
        std::thread::spawn(move || {
            let title = mapping.title.clone().unwrap_or_else(|| mapping.process.clone());
            let time_str = format_duration(duration_secs);
            // A timer failure leaves the decision with the user, rather than submitting early.
            let ask = match frontend.auto_submit_prompt(&title, &time_str) {
                Ok(Outcome::Submit) => false,
                Ok(Outcome::AddNotes) => true,
                Err(e) => {
                    log::warn!("[LilyPad] {e}");
                    true
                }
            };
            if ask {
                frontend.needs_decision(SessionEndedData {
                    process_name, mapping, duration_secs, forced: false, ledger_id,
                });
                return;
            }
            // Matches the Tauri build: a live/session game that's already had its "Add Notes"
            // prompt gets no further notification on success, only on failure.
            submit_now(&state, &frontend, &auth, account, &mapping, hours, duration_secs, notes, false, ledger_id);
        });
    } else {
        // Regular games: silent auto-submit (no notes support), but do confirm success --
        // matches the Tauri build's regular-game path, which shows "Session Auto-Submitted"
        // here specifically (unlike the live/session path above).
        std::thread::spawn(move || {
            submit_now(&state, &frontend, &auth, account, &mapping, hours, duration_secs, notes, true, ledger_id);
        });
    }
}

/// Submits a session immediately against its record's key, queueing that record for retry on
/// failure so the play time isn't lost. Runs on a background thread.
#[allow(clippy::too_many_arguments)]
fn submit_now(
    state: &EngineState,
    frontend: &FrontendRef,
    auth: &AuthConfig,
    account: Option<AccountIdentity>,
    mapping: &ProcessMapping,
    hours: f64,
    duration_secs: f64,
    notes: Option<String>,
    notify_on_success: bool,
    ledger_id: Option<String>,
) {
    let client = client_for(auth);
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let title = mapping.title.clone().unwrap_or_else(|| mapping.process.clone());
    let store = state.store();
    let submission = submission_for(mapping, hours, date.clone(), notes.clone(), false, true);
    if let Some(id) = &ledger_id {
        store.save_attempt(id, &submission);
    }

    let result = submit_play_session(
        &client, &mapping.r#type, mapping.froglog_id, Some(date), hours,
        notes, false, true, ledger_id.clone(),
    );
    match result {
        Ok(response) => {
            if let (Some(id), Some(account)) = (&ledger_id, &account) {
                let remote = remote_reference(&response, mapping.froglog_id, &mapping.r#type);
                if store.acknowledge(id, account, &remote).is_none() {
                    log::warn!("[LilyPad] session {id} submitted but its acknowledgement was not saved; a retry reuses the same key");
                }
            }
            if notify_on_success {
                frontend.notify("Session Auto-Submitted", &format!("{title} ({})", format_duration(duration_secs)));
            }
        }
        Err(e) => {
            log::warn!("[LilyPad] auto-submit failed for game {}: {e}", mapping.froglog_id);
            if ApiFailure::classify(&e) == ApiFailure::NotFound {
                let moved = match &ledger_id {
                    Some(id) => move_to_new_games(state, id),
                    None => {
                        unlink_deleted_game(state, mapping);
                        None
                    }
                };
                if let Some(name) = moved {
                    frontend.new_game_recorded(&name, &format_duration(duration_secs), false);
                    frontend.changed();
                    return;
                }
            }
            let saved = store.queue_failed(ledger_id.as_deref(), account, failed(submission, &e));
            if saved {
                frontend.notify("Session Queued", &format!("{title} — submit failed, open LilyPad to retry"));
            } else {
                frontend.notify(
                    "Session Not Saved",
                    &format!("{title} could not be submitted or saved for retry: {}", store.error().unwrap_or(e)),
                );
            }
        }
    }
    frontend.changed();
}

/// Resolves every session this account left `active` in the store, before the monitor starts
/// polling (so it sees `current_session` already populated for a resumed game and does not start
/// tracking it again as new). A game still running is resumed on its original record; any other
/// is credited up to its last checkpoint and put through the normal end-of-session path.
pub fn recover_interrupted(state: &EngineState, frontend: &FrontendRef) {
    let Some(account) = state.account() else { return };
    let mut sys = sysinfo::System::new_all();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All);
    let recovered = state
        .store()
        .recover_interrupted(&account, |identity| crate::session_store::find_original_process(&sys, identity));
    for item in recovered {
        match item {
            Recovered::Resume { record, mapping, pid } => resume(state.clone(), frontend.clone(), record, mapping, pid),
            Recovered::Ended { id, process_name, mapping, duration_secs } => handle_session_ended(
                state.clone(), frontend.clone(), process_name, mapping, duration_secs, false, Some(id),
            ),
        }
    }
}

/// The game outlived LilyPad: carry on tracking the same record rather than opening a second
/// one, so the session keeps its original start time and id.
fn resume(state: EngineState, frontend: FrontendRef, record: SessionRecord, mapping: ProcessMapping, pid: sysinfo::Pid) {
    let started_secs = record.started_at_secs.unwrap_or_else(now_secs);
    let started_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(started_secs);
    let elapsed = SystemTime::now().duration_since(started_wall).unwrap_or_default();
    let started_at = Instant::now().checked_sub(elapsed).unwrap_or_else(Instant::now);
    let process_name = record.process.executable.clone();

    *state.current_session.write().unwrap() = Some(ActiveSession {
        process_name: process_name.clone(),
        mapping: mapping.clone(),
        pid,
        // Carried from the record: `find_original_process` has just confirmed it matches.
        started_at_secs: record.process.started_at_secs,
        started_at,
    });
    state.active_ledger_id.set(&process_name, Some(record.id.clone()));
    frontend.changed();

    let started_at_iso = chrono::DateTime::<chrono::Utc>::from(started_wall).to_rfc3339();
    handle_session_started(&state, &process_name, &mapping, Some(record.id.clone()), started_at_iso, false);

    // The monitor has no waiter for a resumed session, so this one ends it.
    std::thread::spawn(move || {
        crate::monitor::wait_for_exit_with_relaunch_grace(pid, &process_name);
        let duration_secs = SystemTime::now().duration_since(started_wall).unwrap_or_default().as_secs_f64();
        // Only our own session: after a force-stop another game may be tracked by now.
        crate::monitor::clear_session_for(&state.current_session, &process_name);
        let was_force_stopped = state.take_force_stop(&process_name);
        // Only clear the active id if it is still ours; a force-stop has already taken it.
        state.active_ledger_id.clear_if(&record.id);
        match state.store().complete(&record.id, now_secs()) {
            Completion::Owned(ledger_id) => handle_session_ended(
                state, frontend, process_name, mapping, duration_secs, false, ledger_id,
            ),
            // Completed elsewhere -- a force-stop, which has already asked the user.
            Completion::Stale => {
                log::info!(
                    "[LilyPad] session {} already completed{}; not presenting {process_name} again",
                    record.id,
                    if was_force_stopped { " by force-stop" } else { "" }
                );
                frontend.changed();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProcessMapConfig;
    use crate::engine::{Frontend, SubmitPolicy};
    use crate::session_ledger::SessionLedger;
    use crate::session_store::SessionStore;
    use std::sync::{Arc, Mutex};

    /// Records what the engine asked of it; submits nothing itself.
    struct Recorder {
        policy: SubmitPolicy,
        decisions: Mutex<Vec<SessionEndedData>>,
        prompts: Mutex<usize>,
    }

    impl Frontend for Recorder {
        fn notify(&self, _: &str, _: &str) {}
        fn changed(&self) {}
        fn needs_decision(&self, data: SessionEndedData) {
            self.decisions.lock().unwrap().push(data);
        }
        fn auto_submit_prompt(&self, _: &str, _: &str) -> Result<Outcome, String> {
            *self.prompts.lock().unwrap() += 1;
            Ok(Outcome::AddNotes)
        }
        fn submit_policy(&self, _: &EngineState) -> SubmitPolicy {
            self.policy
        }
    }

    #[test]
    fn with_auto_submit_off_every_finished_session_is_left_to_the_user() {
        // The desktop app's settings would auto-submit this session game; Ask overrides them.
        let map = ProcessMapConfig { auto_submit_session: true, auto_submit_regular: true, ..Default::default() };
        let auth = AuthConfig { base_url: Some("http://127.0.0.1:9".into()), token: Some("t".into()), username: Some("alice".into()) };
        let state = EngineState::new(auth, map);
        state.set_store(SessionStore::from_ledger(SessionLedger::open(std::path::Path::new(":memory:")).unwrap(), DEFAULT_API_URL));
        let recorder = Arc::new(Recorder { policy: SubmitPolicy::Ask, decisions: Mutex::new(Vec::new()), prompts: Mutex::new(0) });
        let frontend: FrontendRef = recorder.clone();
        let mapping = ProcessMapping {
            process: "Celeste.exe".into(), r#type: "session".into(), froglog_id: 7,
            title: Some("Celeste".into()), title_filter: None, exe_path: None,
        };

        handle_session_ended(state, frontend, "Celeste.exe".into(), mapping, 3600.0, false, None);

        let decisions = recorder.decisions.lock().unwrap();
        assert_eq!(decisions.len(), 1, "the session dialog must be asked for");
        assert!(!decisions[0].forced);
        assert_eq!(*recorder.prompts.lock().unwrap(), 0, "no Add Notes prompt: the dialog has notes");
    }
}
