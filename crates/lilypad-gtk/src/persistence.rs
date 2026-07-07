//! Crash recovery for an in-progress session, ported from the Tauri build's
//! `PersistedSession` handling in `lib.rs`'s `setup()`. If LilyPad crashes or is
//! restarted while tracking a game, this lets it pick back up instead of
//! silently losing the play time.

use crate::session_flow::{self, AppAction};
use crate::state::AppState;
use crate::tray::RefreshTray;
use lilypad_core::config::{app_data_dir, ProcessMapping};
use lilypad_core::monitor::ActiveSession;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};
use sysinfo::{ProcessesToUpdate, System};

#[derive(Serialize, Deserialize)]
struct PersistedSession {
    process: String,
    game_type: String,
    froglog_id: i32,
    title: Option<String>,
    started_at_secs: u64,
}

pub fn active_session_path() -> PathBuf {
    app_data_dir().join("active-session.json")
}

/// Called when a session starts, so it survives a crash/restart.
pub fn persist_session_start(mapping: &ProcessMapping) {
    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
    let persisted = PersistedSession {
        process: mapping.process.clone(),
        game_type: mapping.r#type.clone(),
        froglog_id: mapping.froglog_id,
        title: mapping.title.clone(),
        started_at_secs: secs,
    };
    if let Ok(json) = serde_json::to_string(&persisted) {
        let path = active_session_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, json);
    }
}

/// Called when a session ends normally — the persisted file is no longer needed.
pub fn clear_persisted_session() {
    let _ = std::fs::remove_file(active_session_path());
}

/// Called once at startup, before the process monitor starts polling. If a
/// session was interrupted by a crash/restart: restores it with the correct
/// original start time if the mapped process is still running (and waits for
/// its real exit before reporting it), or reports it immediately if the game
/// already closed while LilyPad was down.
pub fn recover_on_startup(state: AppState, refresh_tray: RefreshTray, app_tx: async_channel::Sender<AppAction>) {
    let path = active_session_path();
    let Ok(json) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(persisted) = serde_json::from_str::<PersistedSession>(&json) else {
        let _ = std::fs::remove_file(&path);
        return;
    };

    let saved_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(persisted.started_at_secs);
    let duration_so_far = SystemTime::now().duration_since(saved_wall).unwrap_or_default();
    let mapping = ProcessMapping {
        process: persisted.process.clone(),
        r#type: persisted.game_type.clone(),
        froglog_id: persisted.froglog_id,
        title: persisted.title.clone(),
        title_filter: None,
    };

    let mut sys = System::new_all();
    sys.refresh_processes(ProcessesToUpdate::All);
    let still_running_pid = sys.processes().iter().find_map(|(pid, p)| {
        let exe_name = p
            .exe()
            .and_then(|path| path.file_name().and_then(|n| n.to_str().map(String::from)))
            .unwrap_or_else(|| p.name().to_string_lossy().into_owned());
        if exe_name.eq_ignore_ascii_case(&persisted.process) {
            Some(*pid)
        } else {
            None
        }
    });

    if let Some(pid) = still_running_pid {
        // Game still running — restore the session with its original start time.
        let started_at = Instant::now().checked_sub(duration_so_far).unwrap_or_else(Instant::now);
        *state.current_session.write().unwrap() = Some(ActiveSession {
            process_name: persisted.process.clone(),
            mapping: mapping.clone(),
            started_at,
        });
        refresh_tray();

        std::thread::spawn(move || {
            let mut sys2 = System::new_all();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                sys2.refresh_processes(ProcessesToUpdate::All);
                if let Some(proc) = sys2.process(pid) {
                    proc.wait();
                    break;
                }
                if Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            let duration_secs = SystemTime::now().duration_since(saved_wall).unwrap_or_default().as_secs_f64();
            let _ = std::fs::remove_file(&path);
            *state.current_session.write().unwrap() = None;
            session_flow::handle_session_ended(state, refresh_tray, app_tx, persisted.process, mapping, duration_secs, false);
        });
    } else {
        // Game already closed while LilyPad was down — report it immediately.
        let _ = std::fs::remove_file(&path);
        session_flow::handle_session_ended(state, refresh_tray, app_tx, persisted.process, mapping, duration_so_far.as_secs_f64(), false);
    }
}
