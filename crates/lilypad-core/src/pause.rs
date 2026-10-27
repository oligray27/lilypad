//! Pausing and resuming the tracked session -- from LilyPad itself (tray "Pause Tracking", the
//! Steam Deck panel), or from the FrogLog site with remote control on (see `remote`). Shared by
//! every frontend: the engine (Linux desktop, Gaming Mode) and the Windows app call the same
//! thing with their own state.
//!
//! A pause is saved on the session's ledger record first (that's what the logged duration comes
//! from, and it survives a restart), then mirrored on the in-memory `ActiveSession` (what the tray
//! and the remote state show). Paused time is never logged as play. A paused session stays in
//! Online Now, but its "playing for" time stops: presence carries `paused_at` (sent here
//! straight away, and by the heartbeat after that, see `ledger_session::presence_start`).

use crate::config::AuthConfig;
use crate::ledger_session::now_secs;
use crate::monitor::ActiveSession;
use crate::session_store::{ActiveRecord, SessionStore};
use std::sync::RwLock;
use std::time::Instant;

/// Pauses (`paused`) or resumes the tracked mapped session, if `ledger_id` is the one being
/// tracked. Pausing a paused session, or resuming a running one, does nothing and succeeds.
pub fn set_paused(
    current_session: &RwLock<Option<ActiveSession>>,
    active_ledger_id: &ActiveRecord,
    store: &SessionStore,
    auth: &AuthConfig,
    share_now_playing: bool,
    ledger_id: &str,
    paused: bool,
) -> Result<(), String> {
    if active_ledger_id.current().as_deref() != Some(ledger_id) {
        return Err("That session isn't being tracked any more".into());
    }
    let mut guard = current_session.write().unwrap();
    let Some(session) = guard.as_mut() else { return Err("Nothing is being tracked".into()) };
    if session.is_paused() == paused {
        return Ok(());
    }

    let at = now_secs();
    let saved = if paused { store.pause(ledger_id, at) } else { store.resume(ledger_id, at) };
    // With no storage at all there's no record to keep it on, but the in-memory pause still
    // keeps the time shown right; with storage, a failed save must not leave the two disagreeing.
    if !saved && store.ledger().is_some() {
        return Err(format!(
            "Could not save the {}: {}",
            if paused { "pause" } else { "resume" },
            store.error().unwrap_or_else(|| "the session has already ended".into())
        ));
    }
    if paused {
        session.paused_at = Some(Instant::now());
    } else if let Some(paused_at) = session.paused_at.take() {
        session.paused_total += paused_at.elapsed();
    }
    let presence = share_now_playing.then(|| (session.mapping.clone(), session.active_elapsed()));
    drop(guard);

    if let Some((mapping, active)) = presence {
        let auth = auth.clone();
        std::thread::spawn(move || {
            let client = crate::engine::flow::client_for(&auth);
            // A start that leaves the pauses out, so "playing for" is play time; while paused it
            // stands still at `paused_at`.
            let now = chrono::Utc::now();
            let start = now - chrono::Duration::seconds(active.as_secs() as i64);
            let _ = client.set_now_playing_at(
                mapping.froglog_id,
                mapping.r#type.clone(),
                mapping.title.clone(),
                Some(start.to_rfc3339()),
                paused.then(|| now.to_rfc3339()),
            );
        });
    }
    crate::remote::state_changed();
    Ok(())
}
