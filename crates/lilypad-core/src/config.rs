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

/// Process-map path for the given auth. Logged-in users get process-map-{key}.json; anonymous uses process-map.json.
pub fn process_map_path_for_auth(auth: &AuthConfig) -> PathBuf {
    let name = if auth.token.is_some() || auth.username.is_some() {
        format!("process-map-{}.json", process_map_user_key(auth))
    } else {
        "process-map.json".to_string()
    };
    app_data_dir().join(name)
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
