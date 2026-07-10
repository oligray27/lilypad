//! App config and process-to-game mapping.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One entry: process name (e.g. "hl2.exe") -> Froglog game id + type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessMapping {
    pub process: String,
    /// "regular", "live", or "session"
    pub r#type: String,
    pub froglog_id: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional substring to match against window titles (case-insensitive).
    /// When set, the process is only tracked if a window with a matching title is found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_filter: Option<String>,
}

/// A root folder the user has pointed LilyPad at for non-Steam games — every immediate
/// subfolder inside it is treated as a separate installed game (see `local_games.rs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchedDirectory {
    pub path: String,
}

/// In-memory mapping list.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProcessMapConfig {
    pub mappings: Vec<ProcessMapping>,
    #[serde(default)]
    pub auto_submit_regular: bool,
    #[serde(default)]
    pub auto_submit_live: bool,
    #[serde(default)]
    pub auto_submit_session: bool,
    #[serde(default)]
    pub share_now_playing: bool,
    /// When true, LilyPad never scans unmapped processes against installed Steam games /
    /// the FrogLog library (see `monitor::maybe_start_unmapped_tracking`). Defaults to false
    /// (feature enabled) so existing configs opt in without a migration.
    #[serde(default)]
    pub disable_unmapped_game_detection: bool,
    /// Root folders scanned for non-Steam games (see `local_games::scan_watched_directories`).
    #[serde(default)]
    pub watched_directories: Vec<WatchedDirectory>,
}

impl ProcessMapConfig {
    pub fn load_from(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            _ => Self::default(),
        }
    }

    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap_or_default())
    }

    /// Find all mappings for a process name (multiple games can share the same exe, e.g. javaw.exe).
    pub fn find_all_by_process(&self, process_name: &str) -> Vec<&ProcessMapping> {
        let needle = process_name.to_lowercase();
        self.mappings
            .iter()
            .filter(|m| {
                m.process.to_lowercase() == needle
                    || process_name.to_lowercase().ends_with(&m.process.to_lowercase())
            })
            .collect()
    }
}

/// A session that failed to submit (auth expired, offline, etc.) and was saved for later retry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSession {
    pub id: String,
    pub game_id: i32,
    pub game_type: String,
    pub title: String,
    pub hours: f64,
    pub notes: Option<String>,
    pub spoiler: bool,
    pub is_public: bool,
    pub date: String,
    pub failed_at: String,
    pub error: String,
}

pub fn pending_sessions_path() -> PathBuf {
    app_data_dir().join("pending-sessions.json")
}

pub fn load_pending_sessions() -> Vec<PendingSession> {
    match std::fs::read_to_string(pending_sessions_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        _ => vec![],
    }
}

pub fn save_pending_sessions(sessions: &[PendingSession]) {
    let path = pending_sessions_path();
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let _ = std::fs::write(&path, serde_json::to_string_pretty(sessions).unwrap_or_default());
}

/// A completed play session for a Steam game that isn't in the user's FrogLog library yet
/// (see `monitor::run_poll_loop`'s `on_unmapped_session_ended`). Not a "failed submission" like
/// `PendingSession` — this is a game that was never submitted at all, waiting to be resolved
/// into a real FrogLog entry. Hours accumulate across repeated play sessions of the same appid
/// so multiple sessions before the user gets around to resolving it aren't lost or duplicated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingGameSubmission {
    pub appid: String,
    pub title: String,
    pub hours: f64,
    pub session_count: u32,
    pub first_seen_secs: u64,
    pub last_session_secs: u64,
    /// The exe file name LilyPad detected this game running as (e.g. "Celeste.exe") — used to
    /// auto-create a `ProcessMapping` once this entry is resolved, so future sessions are
    /// tracked normally instead of falling through detection again. Defaults to empty for
    /// entries persisted before this field existed.
    #[serde(default)]
    pub exe_name: String,
}

pub fn pending_game_submissions_path() -> PathBuf {
    app_data_dir().join("pending-game-submissions.json")
}

pub fn load_pending_game_submissions() -> Vec<PendingGameSubmission> {
    match std::fs::read_to_string(pending_game_submissions_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        _ => vec![],
    }
}

pub fn save_pending_game_submissions(items: &[PendingGameSubmission]) {
    let path = pending_game_submissions_path();
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let _ = std::fs::write(&path, serde_json::to_string_pretty(items).unwrap_or_default());
}

/// Records a completed session for a not-yet-in-library game: accumulates hours into an
/// existing pending entry for the same appid if one exists, or creates a new one. Returns the
/// updated total hours for that entry, so the caller can put it in a notification.
pub fn record_pending_game_submission(appid: &str, title: &str, exe_name: &str, hours: f64) -> f64 {
    let mut items = load_pending_game_submissions();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let total_hours = if let Some(existing) = items.iter_mut().find(|i| i.appid == appid) {
        existing.hours += hours;
        existing.session_count += 1;
        existing.last_session_secs = now_secs;
        existing.title = title.to_string();
        existing.exe_name = exe_name.to_string();
        existing.hours
    } else {
        items.push(PendingGameSubmission {
            appid: appid.to_string(),
            title: title.to_string(),
            hours,
            session_count: 1,
            first_seen_secs: now_secs,
            last_session_secs: now_secs,
            exe_name: exe_name.to_string(),
        });
        hours
    };

    save_pending_game_submissions(&items);
    total_hours
}

/// Removes a pending game submission once it's been resolved (or dismissed) by the user.
pub fn remove_pending_game_submission(appid: &str) {
    let mut items = load_pending_game_submissions();
    items.retain(|i| i.appid != appid);
    save_pending_game_submissions(&items);
}

/// App data directory (e.g. %APPDATA%/froglog-lilypad on Windows).
pub fn app_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("froglog-lilypad")
}

/// Stable key for the current user so each account has its own process-map file.
/// Uses username when present (stable across logout/login); otherwise token hash for backwards compat.
/// Username is normalized to lowercase so the key is stable regardless of login input casing.
fn process_map_user_key(auth: &AuthConfig) -> String {
    if let Some(ref u) = auth.username {
        let normalized = u.to_lowercase();
        let h = normalized.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
        return format!("{:08x}", h);
    }
    match &auth.token {
        None => "anonymous".to_string(),
        Some(t) => {
            let h = t.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
            format!("{:08x}", h)
        }
    }
}

/// Stable per-user key for building account-scoped filenames (e.g. sync state on mobile).
pub fn user_key(auth: &AuthConfig) -> String {
    process_map_user_key(auth)
}

/// Process-map path for the given auth. Logged-in users get process-map-{key}.json; anonymous uses process-map.json.
pub fn process_map_path_for_auth(auth: &AuthConfig) -> PathBuf {
    let name = if auth.token.is_some() || auth.username.is_some() {
        format!("process-map-{}.json", process_map_user_key(auth))
    } else {
        "process-map.json".to_string()
    };
    app_data_dir().join(name)
}

/// Creates (or replaces) a `ProcessMapping` linking `process` to a FrogLog game, persisting it
/// immediately. Used both when auto-linking an already-owned installed game and when resolving
/// a pending "New Games" entry — in both cases the exe is known to have no existing mapping
/// (that's why detection reached this code path), so no conflict-checking is needed here (that
/// only matters for the manual multi-exe-sharing Configure UI flow, which stays in each
/// frontend since it has its own conflict-reporting UX).
pub fn link_process_mapping(
    process_map_arc: &std::sync::Arc<std::sync::RwLock<ProcessMapConfig>>,
    auth: &AuthConfig,
    process: String,
    game_type: String,
    froglog_id: i32,
    title: Option<String>,
) -> Result<(), String> {
    let mut map = process_map_arc.read().unwrap().clone();
    map.mappings.retain(|m| !(m.froglog_id == froglog_id && m.r#type == game_type));
    map.mappings.push(ProcessMapping { process, r#type: game_type, froglog_id, title, title_filter: None });
    map.save_to(&process_map_path_for_auth(auth)).map_err(|e| e.to_string())?;
    *process_map_arc.write().unwrap() = map;
    Ok(())
}

/// Auth/config: base URL, token, and optional username (used for stable process-map path across re-login).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub base_url: Option<String>,
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

pub fn auth_config_path() -> PathBuf {
    app_data_dir().join("auth.json")
}

impl AuthConfig {
    pub fn load_from(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            _ => Self::default(),
        }
    }
    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap_or_default())
    }
}
