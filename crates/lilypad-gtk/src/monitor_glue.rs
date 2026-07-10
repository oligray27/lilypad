use crate::state::AppState;
use lilypad_core::config::{self, ProcessMapping};
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
        move |title, appid, exe_name, duration_secs| {
            let _ = tx_unmapped.send_blocking(MonitorEvent::UnmappedGameSessionEnded {
                title,
                appid,
                exe_name,
                duration_secs,
            });
        },
        move |mapping: ProcessMapping| {
            let auth = state_for_link.auth.read().unwrap().clone();
            if let Err(e) = config::link_process_mapping(
                &state_for_link.process_map,
                &auth,
                mapping.process,
                mapping.r#type,
                mapping.froglog_id,
                mapping.title,
            ) {
                log::warn!("[LilyPad] failed to auto-link already-owned game: {e}");
            }
        },
    );
}
