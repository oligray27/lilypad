//! What a frontend asks the engine to do on the user's behalf. All blocking (network and
//! storage), so call off any UI thread.

use super::flow::{self, client_for, failed, may_submit, submission_for};
use super::{EngineState, FrontendRef};
use crate::config::ProcessMapping;
use crate::ledger_session::now_secs;
use crate::resolution::{self, Choice, Resolution};
use crate::session_store::Completion;
use crate::submission::{explain_failure, remote_reference, retry_play_session, submit_play_session};

/// How one attempt to submit a finished session ended.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "outcome", content = "message", rename_all = "snake_case")]
pub enum Attempt {
    Submitted,
    /// Failed, and the session is durably in Pending Submissions: nothing left to decide here.
    Queued(String),
    /// Failed and could not be saved, or was refused; the user may try again.
    Failed(String),
}

/// Submits a finished session the user has decided on (with notes and privacy), against its
/// own record. On failure the record is queued for retry with exactly this payload.
#[allow(clippy::too_many_arguments)]
pub fn submit_decision(
    state: &EngineState,
    mapping: &ProcessMapping,
    ledger_id: Option<String>,
    hours: f64,
    notes: Option<String>,
    spoiler: bool,
    is_public: bool,
) -> Attempt {
    // One snapshot for both, so a login change mid-decision cannot pair them up wrongly.
    let auth = state.auth.read().unwrap().clone();
    let store = state.store();
    let account = store.account(&auth);
    if account.is_none() {
        return Attempt::Failed("Log in to FrogLog to submit this session.".into());
    }
    if !may_submit(state, ledger_id.as_deref(), account.as_ref()) {
        return Attempt::Failed(
            "This session was started under a different FrogLog account. Log back in to that account to submit it.".into(),
        );
    }
    let client = client_for(&auth);
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let submission = submission_for(mapping, hours, date.clone(), notes.clone(), spoiler, is_public);
    if let Some(id) = &ledger_id {
        store.save_attempt(id, &submission);
    }
    let result = submit_play_session(
        &client, &mapping.r#type, mapping.froglog_id, Some(date), hours,
        notes, spoiler, is_public, ledger_id.clone(),
    );
    match result {
        Ok(response) => {
            if let (Some(id), Some(account)) = (&ledger_id, &account) {
                let remote = remote_reference(&response, mapping.froglog_id, &mapping.r#type);
                if store.acknowledge(id, account, &remote).is_none() {
                    log::warn!("[LilyPad] session {id} submitted but its acknowledgement was not saved; a retry reuses the same key");
                }
            }
            Attempt::Submitted
        }
        Err(e) => {
            let explanation = explain_failure(&e);
            if store.queue_failed(ledger_id.as_deref(), account, failed(submission, &e)) {
                Attempt::Queued(format!("Submission failed; the session is saved in Pending Submissions. {explanation}"))
            } else {
                Attempt::Failed(format!(
                    "Submission failed and the session could not be saved for retry ({}). {explanation}",
                    store.error().unwrap_or_else(|| "storage unavailable".into())
                ))
            }
        }
    }
}

/// "Do not record session": discards the record. Keeps the row, distinguishable from one that
/// was never recorded. A session with no record has nothing to discard.
pub fn discard_session(state: &EngineState, ledger_id: Option<&str>) -> Result<(), String> {
    let (Some(id), Some(account)) = (ledger_id, state.account()) else { return Ok(()) };
    match state.store().discard(id, &account) {
        Some(_) => Ok(()),
        None => Err(format!(
            "Could not discard this session: {}",
            state.store().error().unwrap_or_else(|| "storage unavailable".into())
        )),
    }
}

/// Resubmits one pending record for the logged-in account and acknowledges it.
pub fn retry_pending(state: &EngineState, id: &str) -> Result<(), String> {
    // Credentials and ownership come from the same snapshot, so a logout/login mid-request can
    // neither submit this row with another token nor acknowledge it for the wrong owner.
    let (auth, account) = state.auth_and_account().ok_or("Not logged in")?;
    let store = state.store();
    // Re-read at action time: the row on screen may be stale (already submitted elsewhere).
    let session = store
        .pending_session(id, &account)
        .ok_or_else(|| store.error().unwrap_or_else(|| "Session not found for this account".into()))?;
    let client = client_for(&auth);
    let result = retry_play_session(&client, &session, Some(session.id.clone())).map_err(|e| explain_failure(&e))?;
    let remote = remote_reference(&result.response, result.game_id, &result.game_type);
    match store.acknowledge(id, &account, &remote) {
        Some(true) => Ok(()),
        Some(false) => Err("The session changed while submitting; check its recorded status".into()),
        None => Err("Submitted, but the result could not be saved. Retrying reuses the same session key.".into()),
    }
}

/// Resolves the logged-in account's New Games entry for `appid`, then links its executable and
/// refreshes the library so the next launch is tracked normally.
pub fn resolve_new_game(state: &EngineState, appid: &str, choice: Choice) -> Result<Resolution, String> {
    let (auth, account) = state.auth_and_account().ok_or("Not logged in")?;
    let client = client_for(&auth);
    // A snapshot, so no lock is held across the network calls.
    let library = state.library_index.read().unwrap().clone();
    let resolved = resolution::resolve(&client, &state.store(), &account, &library, appid, choice)?;
    resolution::link_resolved(&state.process_map, &auth, &resolved);
    state.refresh_library_index();
    Ok(resolved)
}

/// "Stop Tracking Current Session": ends the tracked session now (the user is usually
/// correcting a wrong game) and asks what to do with the time. The process keeps running, so
/// re-tracking it is blocked until it really exits. Returns whether anything was stopped.
pub fn force_stop(state: &EngineState, frontend: &FrontendRef) -> bool {
    let session_info = {
        let sess = state.current_session.read().unwrap();
        sess.as_ref().map(|s| (s.process_name.clone(), s.mapping.clone(), s.started_at.elapsed().as_secs_f64()))
    };
    let Some((process_name, mapping, duration_secs)) = session_info else { return false };
    *state.force_stopped_process.write().unwrap() = Some(process_name.clone());
    *state.current_session.write().unwrap() = None;
    // Close the durable record now. `finish` only applies to an active record, so the monitor's
    // own end event for this process (when it really exits) cannot complete it a second time.
    let ledger_id = match state.active_ledger_id.take() {
        Some(id) => match state.store().complete(&id, now_secs()) {
            Completion::Owned(id) => id,
            // Already ended by its waiter a moment ago; that path presents it.
            Completion::Stale => {
                frontend.changed();
                return true;
            }
        },
        None => None,
    };
    flow::handle_session_ended(state.clone(), frontend.clone(), process_name, mapping, duration_secs, true, ledger_id);
    frontend.changed();
    true
}
