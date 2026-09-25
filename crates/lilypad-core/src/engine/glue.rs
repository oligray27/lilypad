//! Connects the process monitor to the session store. Every durable write happens on the
//! monitor's own thread *before* an event is sent: an event must never be the only record that
//! a session started or ended.

use super::flow::{client_for, round_hours};
use super::EngineState;
use crate::api::ApiFailure;
use crate::config::{self, ProcessMapping};
use crate::ledger_session::now_secs;
use crate::monitor::{run_poll_loop, UnmappedSessionStart};
use crate::session_store::Completion;
use std::sync::mpsc;

#[derive(Debug, Clone)]
pub enum MonitorEvent {
    SessionStarted {
        process_name: String,
        mapping: ProcessMapping,
        /// `None` when the session could not be stored; it is still tracked in memory.
        ledger_id: Option<String>,
    },
    SessionEnded {
        process_name: String,
        mapping: ProcessMapping,
        duration_secs: f64,
        ledger_id: Option<String>,
    },
    UnmappedGameSessionEnded {
        title: String,
        duration_secs: f64,
        /// Whether this appid matches an existing but Completed/DNF library entry.
        is_replay: bool,
        /// Whether the session reached the New Games queue. `false` means it was lost to a
        /// storage failure, which the user must be told about rather than invited to resolve.
        saved: bool,
    },
}

/// Starts the durable record for the mapped session the monitor has just stored in
/// `current_session`, reading its exact pid and OS start time from there.
pub(crate) fn begin_session(state: &EngineState, process_name: &str, mapping: &ProcessMapping) -> Option<String> {
    let (pid, started_at_secs) = state
        .current_session
        .read()
        .ok()
        .and_then(|s| s.as_ref().map(|s| (u32::try_from(usize::from(s.pid)).ok(), s.started_at_secs)))
        .unwrap_or((None, None));
    let id = state.store().begin_mapped(state.account(), process_name, mapping, pid, started_at_secs);
    state.active_ledger_id.set(process_name, id.clone());
    if let Some(pid) = pid {
        backfill_exe_path(state, mapping, pid);
    }
    id
}

/// Pins the mapping to the binary it was just seen running as, so a same-named executable
/// from another install stops matching it; also how a moved install heals. A no-op for a
/// Wine/Proton host binary, which identifies the runtime rather than the game.
fn backfill_exe_path(state: &EngineState, mapping: &ProcessMapping, pid: u32) {
    let pid = sysinfo::Pid::from(pid as usize);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]));
    let Some(exe_path) = system.process(pid).and_then(|p| p.exe().map(|e| e.to_path_buf())) else { return };
    let auth = state.auth.read().unwrap().clone();
    match config::backfill_mapping_exe_path(&state.process_map, &auth, mapping, &exe_path) {
        Ok(true) => log::info!(
            "[LilyPad] mapping {} -> {} #{} now pinned to {}",
            mapping.process, mapping.r#type, mapping.froglog_id, exe_path.display()
        ),
        Ok(false) => {}
        Err(e) => log::warn!("[LilyPad] could not save the mapping's executable path: {e}"),
    }
}

/// Settles the active mapped record when its process exits. Returns `None` when this end
/// must not be presented: the user already force-stopped it, or another path completed it.
pub(crate) fn finish_session(state: &EngineState, process_name: &str) -> Option<Option<String>> {
    // Already ended synchronously by force-stop. The slot is left alone: it may already hold a
    // different game that started after the force-stop.
    if state.take_force_stop(process_name) {
        return None;
    }
    let completion = match state.active_ledger_id.take_for(process_name) {
        Some(id) => state.store().complete(&id, now_secs()),
        None => Completion::Owned(None),
    };
    match completion {
        Completion::Owned(ledger_id) => Some(ledger_id),
        Completion::Stale => {
            log::info!("[LilyPad] session for {process_name} was already completed; not presenting it again");
            None
        }
    }
}

fn begin_unmapped(state: &EngineState, start: &UnmappedSessionStart) {
    let store = state.store();
    let Some(id) = store.begin_unmapped(state.account(), start) else { return };
    state.active_unmapped_ids.write().unwrap().insert(start.appid.clone(), id.clone());
    // Checkpoint-only: an unmapped game has no library id to report as "now playing". The
    // ledger state is the stop condition, so `still_active` is never consulted here.
    let auth = state.auth.read().unwrap().clone();
    let error_store = store.clone();
    crate::ledger_session::spawn_ledger_heartbeat(
        store.ledger(),
        Some(id),
        move || client_for(&auth),
        state.process_map.clone(),
        None,
        String::new(),
        || true,
        move |e| error_store.report_error(e),
    );
}

/// Credits an ended unmapped session into New Games. The recorded owner and target are
/// authoritative, even after logout; a failed transaction leaves the source for startup recovery.
fn finish_unmapped(
    state: &EngineState,
    title: &str,
    appid: &str,
    exe_name: &str,
    duration_secs: f64,
    replay_of: Option<config::ReplayOf>,
) -> bool {
    let store = state.store();
    let id = state.active_unmapped_ids.write().unwrap().remove(appid);
    let total = match id {
        Some(id) => store.complete_unmapped(&id),
        // No start record (it could not be stored): credit it now under whoever is logged in.
        None => state.account().and_then(|account| {
            store.record_new_game(&account, appid, title, exe_name, round_hours(duration_secs), replay_of)
        }),
    };
    match total {
        Some(total) => {
            log::info!("[LilyPad] new game {title} (appid {appid}); {total}h awaiting resolution");
            true
        }
        None => {
            log::error!("[LilyPad] could not record the session for {title} (appid {appid}): {:?}", store.error());
            false
        }
    }
}

pub fn start(state: &EngineState, tx: mpsc::Sender<MonitorEvent>) {
    let tx_started = tx.clone();
    let tx_ended = tx.clone();
    let tx_unmapped = tx;
    let state_for_start = state.clone();
    let state_for_end = state.clone();
    let state_for_unmapped_start = state.clone();
    let state_for_unmapped_end = state.clone();
    let state_for_link = state.clone();
    let state_for_library_refresh = state.clone();
    run_poll_loop(
        state.process_map.clone(),
        state.current_session.clone(),
        state.shutdown.clone(),
        state.force_stopped_process.clone(),
        2,
        move |process_name, mapping| {
            let ledger_id = begin_session(&state_for_start, &process_name, &mapping);
            let _ = tx_started.send(MonitorEvent::SessionStarted { process_name, mapping, ledger_id });
        },
        move |process_name, mapping, duration_secs| {
            let Some(ledger_id) = finish_session(&state_for_end, &process_name) else { return };
            let _ = tx_ended.send(MonitorEvent::SessionEnded { process_name, mapping, duration_secs, ledger_id });
        },
        state.installed_games.clone(),
        state.library_index.clone(),
        // Called when a running game looks absent from the library, before that is taken as
        // fact: the cache refreshes on a timer, so a game added on the website minutes ago would
        // otherwise be filed as a New Game the user already owns.
        {
            let state = state_for_library_refresh;
            move || state.refresh_library_index()
        },
        move |start: UnmappedSessionStart| begin_unmapped(&state_for_unmapped_start, &start),
        move |title, appid, exe_name, duration_secs, replay_of| {
            let is_replay = replay_of.is_some();
            let replay_of = replay_of.map(|r| config::ReplayOf { id: r.id, game_type: r.game_type, title: r.title, status: r.status });
            let saved = finish_unmapped(&state_for_unmapped_end, &title, &appid, &exe_name, duration_secs, replay_of);
            let _ = tx_unmapped.send(MonitorEvent::UnmappedGameSessionEnded { title, duration_secs, is_replay, saved });
        },
        move |mapping: ProcessMapping| {
            let auth = state_for_link.auth.read().unwrap().clone();
            if let Err(e) = config::link_process_mapping(
                &state_for_link.process_map,
                &auth,
                mapping.process.clone(),
                mapping.r#type.clone(),
                mapping.froglog_id,
                mapping.title,
            ) {
                log::warn!("[LilyPad] failed to auto-link already-owned game: {e}");
            }
            // Best-effort, off this thread (this callback runs on the poll loop itself, and
            // this needs a couple of network round-trips). Two follow-ups now that LilyPad has
            // linked itself to this game:
            // - It might be a Steam-bulk-imported entry that was never actually started (status
            //   "Imported", no start_date) -- fix that up now that it's genuinely being played.
            // - The monitor already treats this session as "session"-type locally, so the backend
            //   needs session_tracking on too, with any pre-existing hours preserved as a
            //   "Pre-tracked hours" session. enable_session_tracking is idempotent.
            let froglog_id = mapping.froglog_id;
            let game_type = mapping.r#type;
            let process = mapping.process;
            let state = state_for_link.clone();
            std::thread::spawn(move || {
                let client = client_for(&auth);
                let not_found = |e: &str| ApiFailure::classify(e) == ApiFailure::NotFound;
                let mut missing = false;
                if let Err(e) = client.fix_imported_status_if_needed(froglog_id, &game_type) {
                    missing |= not_found(&e);
                    log::warn!("[LilyPad] failed to fix imported status: {e}");
                }
                if !game_type.eq_ignore_ascii_case("live") {
                    if let Err(e) = client.enable_session_tracking(froglog_id) {
                        missing |= not_found(&e);
                        log::warn!("[LilyPad] failed to enable session tracking: {e}");
                    }
                }
                // Linked from a stale library to a game deleted on the website since the last
                // refresh. Drop the mapping and re-read the library, so the next launch resolves
                // afresh (the real entry, or New Games) instead of tracking against a dead id for
                // ever. The session already in flight still queues and can be discarded.
                if missing {
                    log::warn!("[LilyPad] game {froglog_id} no longer exists; removing the mapping for {process} and refreshing the library");
                    if let Err(e) = config::remove_dead_mapping(&state.process_map, &auth, &process, &game_type, froglog_id) {
                        log::warn!("[LilyPad] could not save the process map after removing a dead mapping: {e}");
                    }
                    state.refresh_library_index();
                }
            });
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthConfig, ProcessMapConfig};
    use crate::engine::flow::may_submit;
    use crate::engine::DEFAULT_API_URL;
    use crate::session_ledger::SessionLedger;
    use crate::session_store::SessionStore;

    fn auth(name: &str) -> AuthConfig {
        AuthConfig { base_url: None, token: Some(format!("t-{name}")), username: Some(name.into()) }
    }

    fn state(name: &str) -> EngineState {
        let state = EngineState::new(auth(name), ProcessMapConfig::default());
        let ledger = SessionLedger::open(std::path::Path::new(":memory:")).unwrap();
        state.set_store(SessionStore::from_ledger(ledger, DEFAULT_API_URL));
        state
    }

    fn mapping() -> ProcessMapping {
        ProcessMapping {
            process: "game".into(), r#type: "session".into(), froglog_id: 3,
            title: Some("Game".into()), title_filter: None, exe_path: None,
        }
    }

    #[test]
    fn a_normal_exit_completes_the_record_once() {
        let state = state("alice");
        let id = begin_session(&state, "game", &mapping()).unwrap();
        assert_eq!(finish_session(&state, "game"), Some(Some(id.clone())));
        // The record is now in alice's retry queue until acknowledged or discarded.
        let account = state.account().unwrap();
        assert_eq!(state.store().pending_sessions(&account).unwrap()[0].id, id);
    }

    #[test]
    fn a_force_stopped_session_is_not_presented_again_when_the_process_exits() {
        let state = state("alice");
        let id = begin_session(&state, "game", &mapping()).unwrap();
        // What force-stop does, synchronously.
        *state.force_stopped_process.write().unwrap() = Some("game".into());
        let taken = state.active_ledger_id.take().unwrap();
        assert_eq!(state.store().complete(&taken, now_secs()), Completion::Owned(Some(id)));
        // The monitor's later end event must be swallowed, and must release the block.
        assert_eq!(finish_session(&state, "game"), None);
        assert!(state.force_stopped_process.read().unwrap().is_none());
    }

    #[test]
    fn a_force_stopped_games_late_exit_does_not_end_the_next_game() {
        let state = state("alice");
        let _x = begin_session(&state, "x.exe", &mapping()).unwrap();
        *state.force_stopped_process.write().unwrap() = Some("x.exe".into());
        let taken = state.active_ledger_id.take().unwrap();
        state.store().complete(&taken, now_secs());
        // Y starts while X is still running.
        let y = begin_session(&state, "y.exe", &mapping()).unwrap();
        // X finally exits.
        assert_eq!(finish_session(&state, "x.exe"), None);
        // Y's record is still active and still Y's to finish.
        assert_eq!(finish_session(&state, "y.exe"), Some(Some(y)));
    }

    #[test]
    fn a_session_is_submittable_only_by_the_account_that_started_it() {
        let state = state("alice");
        let id = begin_session(&state, "game", &mapping()).unwrap();
        let alice = state.account();
        assert!(may_submit(&state, Some(&id), alice.as_ref()));
        let bob = state.store().account(&auth("bob"));
        assert!(!may_submit(&state, Some(&id), bob.as_ref()));
        assert!(!may_submit(&state, Some(&id), None));
    }

    #[test]
    fn logged_out_play_is_tracked_in_memory_without_a_record() {
        let state = EngineState::new(AuthConfig::default(), ProcessMapConfig::default());
        state.set_store(SessionStore::from_ledger(
            SessionLedger::open(std::path::Path::new(":memory:")).unwrap(),
            DEFAULT_API_URL,
        ));
        assert!(begin_session(&state, "game", &mapping()).is_none());
        assert_eq!(finish_session(&state, "game"), Some(None));
    }
}
