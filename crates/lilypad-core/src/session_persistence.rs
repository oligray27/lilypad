//! Local persistence + periodic heartbeat for the currently-tracked session, shared by both
//! frontends (Tauri and GTK previously each had their own near-identical copy of this).
//!
//! One "tick every HEARTBEAT_INTERVAL while a session is active" mechanism solves two otherwise
//! unrelated-looking bugs at once:
//! - Local: crash recovery (`recover_on_startup`) needs to know when the game was last confirmed
//!   still running, not "now" -- otherwise a session that survives a full PC shutdown/reboot (the
//!   tracked process is obviously gone on the next boot) gets billed for the entire downtime
//!   instead of just the real play time before shutdown.
//! - Remote: `now_playing_updated_at` needs periodic refreshing so the backend's staleness check
//!   (see `backend/lib/liveSession.js`) doesn't hide a still-genuinely-playing user just because
//!   nothing's pinged it in a while.
//!
//! Both halves of the tick are independent of network connectivity: the local file write always
//! succeeds, and the remote refresh is best-effort (silently no-ops offline, same as the initial
//! `set_now_playing` call already does) -- an offline machine still tracks and recovers sessions
//! correctly, it just won't show as "online" to anyone else.

use crate::api::FroglogClient;
use crate::config::{app_data_dir, ProcessMapConfig, ProcessMapping};
use crate::monitor::{wait_for_exit_with_relaunch_grace, ActiveSession};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};
use sysinfo::{ProcessesToUpdate, System};

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSession {
    process: String,
    game_type: String,
    froglog_id: i32,
    title: Option<String>,
    started_at_secs: u64,
    /// Last time the heartbeat (or session start) could confirm the session was still active.
    /// Used instead of "now" when recovering a session whose process is no longer running, so
    /// downtime (PC off/asleep, or LilyPad simply not running) is never billed as play time.
    #[serde(default)]
    last_alive_secs: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn active_session_path() -> PathBuf {
    app_data_dir().join("active-session.json")
}

fn write_persisted(persisted: &PersistedSession) {
    if let Ok(json) = serde_json::to_string(persisted) {
        let path = active_session_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, json);
    }
}

/// Called when a session starts, so it survives a crash/restart.
pub fn persist_session_start(mapping: &ProcessMapping) {
    let secs = now_secs();
    write_persisted(&PersistedSession {
        process: mapping.process.clone(),
        game_type: mapping.r#type.clone(),
        froglog_id: mapping.froglog_id,
        title: mapping.title.clone(),
        started_at_secs: secs,
        last_alive_secs: secs,
    });
}

/// Called when a session ends normally — the persisted file is no longer needed.
pub fn clear_persisted_session() {
    let _ = std::fs::remove_file(active_session_path());
}

/// Spawned once per tracked session (from the on-session-started path, in both frontends).
/// Stops itself once `current_session` no longer matches `process_name` (session ended locally).
pub fn spawn_session_heartbeat(
    client_factory: impl Fn() -> FroglogClient + Send + 'static,
    process_map: Arc<RwLock<ProcessMapConfig>>,
    current_session: Arc<RwLock<Option<ActiveSession>>>,
    process_name: String,
    mapping: ProcessMapping,
    started_at_iso: String,
) {
    std::thread::spawn(move || loop {
        std::thread::sleep(HEARTBEAT_INTERVAL);
        let still_active = current_session
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|s| s.process_name == process_name);
        if !still_active {
            return;
        }

        // Local: bound crash-recovery duration to the last confirmed-alive tick.
        if let Ok(json) = std::fs::read_to_string(active_session_path()) {
            if let Ok(mut persisted) = serde_json::from_str::<PersistedSession>(&json) {
                persisted.last_alive_secs = now_secs();
                write_persisted(&persisted);
            }
        }

        // Remote: best-effort, no-op if offline or presence sharing is off.
        if process_map.read().unwrap().share_now_playing {
            let _ = client_factory().set_now_playing(
                mapping.froglog_id,
                mapping.r#type.clone(),
                mapping.title.clone(),
                Some(started_at_iso.clone()),
            );
        }
    });
}

/// Called once at startup, before the process monitor starts polling. Restores a session
/// interrupted by a crash/restart, or reports it immediately if the game had already stopped
/// running while LilyPad was down. No-op if no session was persisted (or it fails to parse).
pub fn recover_on_startup(
    current_session: Arc<RwLock<Option<ActiveSession>>>,
    on_restored: impl FnOnce(ProcessMapping, String /* started_at_iso */) + Send + 'static,
    on_session_ended: impl FnOnce(String, ProcessMapping, f64) + Send + 'static,
) {
    let path = active_session_path();
    let Ok(json) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(persisted) = serde_json::from_str::<PersistedSession>(&json) else {
        let _ = std::fs::remove_file(&path);
        return;
    };

    let saved_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(persisted.started_at_secs);
    let last_alive_wall = SystemTime::UNIX_EPOCH
        + Duration::from_secs(persisted.last_alive_secs.max(persisted.started_at_secs));
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
        // Game still running (LilyPad itself crashed/restarted, PC never went down) — wall-clock
        // time since the original start is still accurate here.
        let duration_so_far = SystemTime::now().duration_since(saved_wall).unwrap_or_default();
        let started_at = Instant::now().checked_sub(duration_so_far).unwrap_or_else(Instant::now);
        *current_session.write().unwrap() = Some(ActiveSession {
            process_name: persisted.process.clone(),
            mapping: mapping.clone(),
            started_at,
        });

        let started_at_iso = chrono::DateTime::<chrono::Utc>::from(saved_wall).to_rfc3339();
        on_restored(mapping.clone(), started_at_iso);

        let process_name = persisted.process.clone();
        std::thread::spawn(move || {
            wait_for_exit_with_relaunch_grace(pid, &process_name);
            let duration_secs = SystemTime::now().duration_since(saved_wall).unwrap_or_default().as_secs_f64();
            clear_persisted_session();
            *current_session.write().unwrap() = None;
            on_session_ended(persisted.process, mapping, duration_secs);
        });
    } else {
        // Game (and possibly the whole PC) was gone before LilyPad relaunched — use the last
        // confirmed-alive tick as the effective end time, not "now", so unaccounted-for downtime
        // is never billed as play time.
        let duration_secs = last_alive_wall.duration_since(saved_wall).unwrap_or_default().as_secs_f64();
        clear_persisted_session();
        on_session_ended(persisted.process, mapping, duration_secs);
    }
}
