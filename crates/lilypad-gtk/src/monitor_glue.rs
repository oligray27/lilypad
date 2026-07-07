use crate::state::AppState;
use lilypad_core::config::ProcessMapping;
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
}

pub fn start(state: &AppState, tx: async_channel::Sender<MonitorEvent>) {
    let tx_started = tx.clone();
    let tx_ended = tx;
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
    );
}
