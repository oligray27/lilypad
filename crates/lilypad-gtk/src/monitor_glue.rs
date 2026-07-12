use crate::session_flow::client_for;
use crate::state::AppState;
use lilypad_core::config::{self, ProcessMapping, ReplayOf};
use lilypad_core::monitor::run_poll_loop;

/// Events pushed from the monitor's background threads; consumed on the GTK main
/// loop so nothing here ever touches a GTK widget off the main thread.
#[derive(Debug, Clone)]
pub enum MonitorEvent {
    SessionStarted {
        process_name: String,
        mapping: ProcessMapping,
    },
    SessionEnded {
        process_name: String,
        mapping: ProcessMapping,
        duration_secs: f64,
    },
    UnmappedGameSessionEnded {
        title: String,
        appid: String,
        exe_name: String,
        duration_secs: f64,
        /// Set when this appid actually matches an existing but Completed/DNF library entry —
        /// this is a possible replay, not a genuinely unknown game. See `ReplayOf`.
        replay_of: Option<ReplayOf>,
    },
}

pub fn start(state: &AppState, tx: async_channel::Sender<MonitorEvent>) {
    let tx_started = tx.clone();
    let tx_ended = tx.clone();
    let tx_unmapped = tx;
    let state_for_link = state.clone();
    run_poll_loop(
        state.process_map.clone(),
        state.current_session.clone(),
        state.shutdown.clone(),
        state.force_stopped_process.clone(),
        2,
        move |process_name, mapping| {
            let _ = tx_started.send_blocking(MonitorEvent::SessionStarted {
                process_name,
                mapping,
            });
        },
        move |process_name, mapping, duration_secs| {
            let _ = tx_ended.send_blocking(MonitorEvent::SessionEnded {
                process_name,
                mapping,
                duration_secs,
            });
        },
        state.installed_games.clone(),
        state.library_index.clone(),
        move |title, appid, exe_name, duration_secs, replay_of| {
            let _ = tx_unmapped.send_blocking(MonitorEvent::UnmappedGameSessionEnded {
                title,
                appid,
                exe_name,
                duration_secs,
                replay_of: replay_of.map(|r| ReplayOf { id: r.id, game_type: r.game_type, title: r.title, status: r.status }),
            });
        },
        move |mapping: ProcessMapping| {
            let auth = state_for_link.auth.read().unwrap().clone();
            if let Err(e) = config::link_process_mapping(
                &state_for_link.process_map,
                &auth,
                mapping.process,
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
            //   No-op if it isn't "Imported".
            // - `monitor.rs` already treats this session as "session"-type locally (see the
            //   comment there), so the backend needs to actually have session_tracking on too,
            //   with any pre-existing hours preserved as a "Pre-tracked hours" session --
            //   enable_session_tracking is idempotent, so calling it even for a game that's
            //   already session-tracked is safe (no duplicate seed session).
            let froglog_id = mapping.froglog_id;
            let game_type = mapping.r#type;
            std::thread::spawn(move || {
                let client = client_for(&auth);
                if let Err(e) = client.fix_imported_status_if_needed(froglog_id, &game_type) {
                    log::warn!("[LilyPad] failed to fix imported status: {e}");
                }
                if !game_type.eq_ignore_ascii_case("live") {
                    if let Err(e) = client.enable_session_tracking(froglog_id) {
                        log::warn!("[LilyPad] failed to enable session tracking: {e}");
                    }
                }
            });
        },
    );
}
