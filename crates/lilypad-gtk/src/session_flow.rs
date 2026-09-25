//! What happens when a tracked session starts, ends, or is recovered after a crash: the
//! auto-submit decision tree, ported from the Tauri build's `handle_session_ended` in lib.rs.
//! Runs entirely off the GTK main thread -- the only GTK-thread work it ever triggers (showing
//! the session popup) goes through `AppAction` / `app_tx`, since `notify::show` and
//! `RefreshTray` are both thread-safe.
//!
//! Every session here is identified by its ledger record. The record is completed before this
//! code runs, so a crash anywhere in here leaves a pending session rather than nothing; a
//! successful submission acknowledges that record, and a failed one attaches its payload to it.

use crate::notify;
use crate::state::{AppState, DEFAULT_API_URL};
use crate::tray::RefreshTray;
use lilypad_core::api::FroglogClient;
use lilypad_core::config::{AuthConfig, ProcessMapping};
use lilypad_core::ledger_session::{self, now_secs};
use lilypad_core::monitor::ActiveSession;
use lilypad_core::session_ledger::{AccountIdentity, SessionRecord, Submission};
use lilypad_core::session_store::{Completion, Recovered};
use lilypad_core::submission::{explain_failure, remote_reference, submit_play_session};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone)]
pub struct SessionEndedData {
    pub process_name: String,
    pub mapping: ProcessMapping,
    pub duration_secs: f64,
    pub forced: bool,
    /// The record this session was completed as; the popup submits or discards exactly this one.
    pub ledger_id: Option<String>,
}

pub enum AppAction {
    ShowSessionPopup(SessionEndedData),
    GoToNewGames,
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
    lilypad_core::duration::format_session_duration(duration_secs)
}

pub(crate) fn client_for(auth: &AuthConfig) -> FroglogClient {
    let base = auth
        .base_url
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_API_URL.to_string());
    let mut c = FroglogClient::new(base);
    c.set_token(auth.token.clone());
    c
}

/// The failed-submission payload for a session, saved against its record for retry.
pub fn failed_submission(
    mapping: &ProcessMapping,
    hours: f64,
    date: String,
    notes: Option<String>,
    spoiler: bool,
    is_public: bool,
    error: &str,
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
        last_error: Some(explain_failure(error)),
        failed_at: Some(chrono::Local::now().to_rfc3339()),
    }
}

/// Whether the logged-in account may submit `ledger_id`. A session belongs to the account that
/// was logged in when it started; submitting it with anyone else's credentials would log one
/// person's play on another's profile. A record with no known owner (storage unavailable) is
/// allowed through, since there is nothing to protect it from.
pub fn may_submit(state: &AppState, ledger_id: Option<&str>, account: Option<&AccountIdentity>) -> bool {
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
    state: &AppState,
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
    state: AppState,
    refresh_tray: RefreshTray,
    app_tx: async_channel::Sender<AppAction>,
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
    refresh_tray();

    // The account changed while this game was running. The record stays pending under the
    // account that played it, and reappears in its queue when that account logs back in.
    if !may_submit(&state, ledger_id.as_deref(), account.as_ref()) {
        let title = mapping.title.clone().unwrap_or_else(|| mapping.process.clone());
        log::info!("[LilyPad] session {ledger_id:?} for {title} belongs to another account; leaving it pending");
        notify::show(
            "Session Saved",
            &format!("{title} was started under a different FrogLog account. It is waiting in that account's Pending Submissions."),
        );
        return;
    }

    let popup = |forced| {
        let _ = app_tx.send_blocking(AppAction::ShowSessionPopup(SessionEndedData {
            process_name: process_name.clone(),
            mapping: mapping.clone(),
            duration_secs,
            forced,
            ledger_id: ledger_id.clone(),
        }));
    };

    // Force-stop never auto-submits (the user is likely correcting a bad
    // mapping) — always show the popup so they can still log the time.
    if forced {
        popup(true);
        return;
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
        popup(false);
        return;
    }

    let is_notes_type = mapping.r#type.eq_ignore_ascii_case("live") || mapping.r#type.eq_ignore_ascii_case("session");
    let hours = round_hours(duration_secs);

    if is_notes_type {
        std::thread::spawn(move || {
            let title = mapping.title.clone().unwrap_or_else(|| mapping.process.clone());
            let time_str = format_duration(duration_secs);
            let outcome = notify::auto_submit_prompt(&title, &time_str);
            // A timer failure leaves the decision with the user, rather than submitting early.
            let show_popup = match outcome {
                Ok(lilypad_core::auto_submit::Outcome::Submit) => false,
                Ok(lilypad_core::auto_submit::Outcome::AddNotes) => true,
                Err(e) => {
                    log::warn!("[LilyPad] {e}");
                    true
                }
            };
            if show_popup {
                let _ = app_tx.send_blocking(AppAction::ShowSessionPopup(SessionEndedData {
                    process_name, mapping, duration_secs, forced: false, ledger_id,
                }));
                return;
            }
            // Matches the Tauri build: a live/session game that's already had its "Add Notes"
            // toast shown gets no further notification on success, only on failure.
            submit_now(&state, &auth, account, &mapping, hours, duration_secs, &refresh_tray, false, ledger_id);
        });
    } else {
        // Regular games: silent auto-submit (no notes support), but do confirm success --
        // matches the Tauri build's regular-game path, which shows "Session Auto-Submitted"
        // here specifically (unlike the live/session path above).
        std::thread::spawn(move || {
            submit_now(&state, &auth, account, &mapping, hours, duration_secs, &refresh_tray, true, ledger_id);
        });
    }
}

/// Submits a session immediately against its record's key, queueing that record for retry on
/// failure so the play time isn't lost. Runs on a background thread.
#[allow(clippy::too_many_arguments)]
fn submit_now(
    state: &AppState,
    auth: &AuthConfig,
    account: Option<AccountIdentity>,
    mapping: &ProcessMapping,
    hours: f64,
    duration_secs: f64,
    refresh_tray: &RefreshTray,
    notify_on_success: bool,
    ledger_id: Option<String>,
) {
    let client = client_for(auth);
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let title = mapping.title.clone().unwrap_or_else(|| mapping.process.clone());
    let store = state.store();

    let result = submit_play_session(
        &client, &mapping.r#type, mapping.froglog_id, Some(date.clone()), hours,
        Some("Session auto submitted with LilyPad".to_string()), false, true, ledger_id.clone(),
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
                notify::show("Session Auto-Submitted", &format!("{title} ({})", format_duration(duration_secs)));
            }
        }
        Err(e) => {
            log::warn!("[LilyPad] auto-submit failed for game {}: {e}", mapping.froglog_id);
            let saved = store.queue_failed(
                ledger_id.as_deref(),
                account,
                failed_submission(mapping, hours, date, None, false, true, &e),
            );
            if saved {
                notify::show("Session Queued", &format!("{title} — submit failed, open LilyPad to retry"));
            } else {
                notify::show(
                    "Session Not Saved",
                    &format!("{title} could not be submitted or saved for retry: {}", store.error().unwrap_or(e)),
                );
            }
        }
    }
    refresh_tray();
}

/// Resolves every session this account left `active` in the store, before the monitor starts
/// polling (so it sees `current_session` already populated for a resumed game and does not start
/// tracking it again as new). A game still running is resumed on its original record; any other
/// is credited up to its last checkpoint and put through the normal end-of-session path.
pub fn recover_interrupted(state: &AppState, refresh_tray: &RefreshTray, app_tx: &async_channel::Sender<AppAction>) {
    let Some(account) = state.account() else { return };
    let mut sys = sysinfo::System::new_all();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All);
    let recovered = state
        .store()
        .recover_interrupted(&account, |identity| lilypad_core::session_store::find_original_process(&sys, identity));
    for item in recovered {
        match item {
            Recovered::Resume { record, mapping, pid } => {
                resume(state.clone(), refresh_tray.clone(), app_tx.clone(), record, mapping, pid)
            }
            Recovered::Ended { id, process_name, mapping, duration_secs } => handle_session_ended(
                state.clone(), refresh_tray.clone(), app_tx.clone(), process_name, mapping, duration_secs, false, Some(id),
            ),
        }
    }
}

/// The game outlived LilyPad: carry on tracking the same record rather than opening a second
/// one, so the session keeps its original start time and id.
fn resume(
    state: AppState,
    refresh_tray: RefreshTray,
    app_tx: async_channel::Sender<AppAction>,
    record: SessionRecord,
    mapping: ProcessMapping,
    pid: sysinfo::Pid,
) {
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
    refresh_tray();

    let started_at_iso = chrono::DateTime::<chrono::Utc>::from(started_wall).to_rfc3339();
    handle_session_started(&state, &process_name, &mapping, Some(record.id.clone()), started_at_iso, false);

    // The monitor has no waiter for a resumed session, so this one ends it.
    std::thread::spawn(move || {
        lilypad_core::monitor::wait_for_exit_with_relaunch_grace(pid, &process_name);
        let duration_secs = SystemTime::now().duration_since(started_wall).unwrap_or_default().as_secs_f64();
        // Only our own session: after a force-stop another game may be tracked by now.
        lilypad_core::monitor::clear_session_for(&state.current_session, &process_name);
        let was_force_stopped = state.take_force_stop(&process_name);
        // Only clear the active id if it is still ours; a force-stop has already taken it.
        state.active_ledger_id.clear_if(&record.id);
        match state.store().complete(&record.id, now_secs()) {
            Completion::Owned(ledger_id) => handle_session_ended(
                state, refresh_tray, app_tx, process_name, mapping, duration_secs, false, ledger_id,
            ),
            // Completed elsewhere -- a force-stop, which has already shown its own popup.
            Completion::Stale => {
                log::info!(
                    "[LilyPad] session {} already completed{}; not presenting {process_name} again",
                    record.id,
                    if was_force_stopped { " by force-stop" } else { "" }
                );
                refresh_tray();
            }
        }
    });
}
