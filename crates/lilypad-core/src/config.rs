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

/// A `games` entry LilyPad already knows about (exact Steam appid match) whose status is
/// "Completed" or "DNF" — present on a `PendingGameSubmission` when relaunching that appid
/// looked more like a deliberate replay than a continuation, so the monitor asked instead of
/// silently resuming it (see `library_match::is_finished_status`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayOf {
    pub id: i32,
    pub game_type: String,
    pub title: String,
    pub status: Option<String>,
}

/// One individual real-world play session accumulated into a `PendingGameSubmission`, with the
/// actual date it happened on -- kept separate (rather than only ever summed into a running
/// total) so resolving the pending item can log each one as its own FrogLog session with its
/// real date, instead of merging everything into a single entry dated "today".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingGameSessionEntry {
    pub date: String,
    pub hours: f64,
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
    /// Running total across `sessions` -- kept as its own field (rather than always
    /// recomputed) purely so existing display code didn't need to change when `sessions` was
    /// added; always kept in lockstep with it by `record_pending_game_submission`.
    pub hours: f64,
    /// Always equal to `sessions.len()`, for the same reason as `hours` above.
    pub session_count: u32,
    pub first_seen_secs: u64,
    pub last_session_secs: u64,
    /// The exe file name LilyPad detected this game running as (e.g. "Celeste.exe") — used to
    /// auto-create a `ProcessMapping` once this entry is resolved, so future sessions are
    /// tracked normally instead of falling through detection again. Defaults to empty for
    /// entries persisted before this field existed.
    #[serde(default)]
    pub exe_name: String,
    /// Set when this appid actually matches an existing (but finished) library entry — i.e.
    /// this isn't a genuinely new game, it's a possible replay of one already marked
    /// Completed/DNF. `None` for a true "never seen this before" entry. Defaults to `None` for
    /// entries persisted before this field existed.
    #[serde(default)]
    pub replay_of: Option<ReplayOf>,
    /// Each individual real-world play session that's been accumulated, in the order recorded
    /// -- resolving this pending item logs each of these as its own FrogLog session (see
    /// `resolve_as_new`/`resolve_as_existing`/`resolve_as_replay`), rather than merging them
    /// into a single entry. Defaults to empty for entries persisted before this field existed
    /// (which then resolve as zero sessions logged rather than silently losing the total --
    /// acceptable since this only affects a pending item that was already sitting unresolved
    /// across an app update, and its `hours`/`session_count` remain visible either way).
    #[serde(default)]
    pub sessions: Vec<PendingGameSessionEntry>,
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
///
/// `replay_of` should be `Some` when this appid actually matches an existing library entry
/// that's Completed/DNF (a possible replay, see `ReplayOf`) rather than a genuinely unknown
/// game — it's refreshed on every accumulated session in case the matched entry's status
/// changed since the pending item was first created.
pub fn record_pending_game_submission(appid: &str, title: &str, exe_name: &str, hours: f64, replay_of: Option<ReplayOf>) -> f64 {
    let mut items = load_pending_game_submissions();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let session_entry = PendingGameSessionEntry { date: today, hours };

    let total_hours = if let Some(existing) = items.iter_mut().find(|i| i.appid == appid) {
        existing.hours += hours;
        existing.session_count += 1;
        existing.last_session_secs = now_secs;
        existing.title = title.to_string();
        existing.exe_name = exe_name.to_string();
        existing.replay_of = replay_of;
        existing.sessions.push(session_entry);
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
            replay_of,
            sessions: vec![session_entry],
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

/// Whether an existing mapping should survive a `link_process_mapping` call that's about to
/// point `process` at `(froglog_id, game_type)`. Drops the mapping being replaced (same
/// `froglog_id`/`game_type`), plus any *other*, unfiltered mapping for the same exe -- an exe
/// should map to exactly one game unless the user has deliberately disambiguated with a title
/// filter (e.g. multiple modpacks sharing javaw.exe). Without the second check, resolving a
/// replay to a fresh entry would leave the exe pointing at both the old and new game, and which
/// one `find_all_by_process` returns first (and therefore which one gets tracked) would be
/// arbitrary -- in practice, always the stale old one, since it was inserted first.
fn survives_relink(existing: &ProcessMapping, process: &str, game_type: &str, froglog_id: i32) -> bool {
    if existing.froglog_id == froglog_id && existing.r#type == game_type {
        return false;
    }
    if existing.title_filter.is_none() && existing.process.eq_ignore_ascii_case(process) {
        return false;
    }
    true
}

/// Creates (or replaces) a `ProcessMapping` linking `process` to a FrogLog game, persisting it
/// immediately. Used when auto-linking an already-owned installed game, when resolving a
/// pending "New Games" entry, and when the user tells LilyPad to keep tracking a mapping it
/// flagged as a possible replay (see `monitor::check_mapped_game_needs_replay_prompt`) — that
/// last case is the only one where a mapping for this exact `froglog_id`/`game_type` might
/// already exist, so any `title_filter` already configured on it is carried over rather than
/// silently dropped. See `survives_relink` for how a stale mapping to a *different* game on the
/// same exe gets cleaned up here too.
pub fn link_process_mapping(
    process_map_arc: &std::sync::Arc<std::sync::RwLock<ProcessMapConfig>>,
    auth: &AuthConfig,
    process: String,
    game_type: String,
    froglog_id: i32,
    title: Option<String>,
) -> Result<(), String> {
    let mut map = process_map_arc.read().unwrap().clone();
    let existing_title_filter = map
        .mappings
        .iter()
        .find(|m| m.froglog_id == froglog_id && m.r#type == game_type)
        .and_then(|m| m.title_filter.clone());
    map.mappings.retain(|m| survives_relink(m, &process, &game_type, froglog_id));
    map.mappings.push(ProcessMapping {
        process,
        r#type: game_type,
        froglog_id,
        title,
        title_filter: existing_title_filter,
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(process: &str, game_type: &str, froglog_id: i32, title_filter: Option<&str>) -> ProcessMapping {
        ProcessMapping {
            process: process.to_string(),
            r#type: game_type.to_string(),
            froglog_id,
            title: None,
            title_filter: title_filter.map(|s| s.to_string()),
        }
    }

    #[test]
    fn relink_drops_the_mapping_being_replaced() {
        let m = mapping("game.exe", "session", 1, None);
        assert!(!survives_relink(&m, "game.exe", "session", 1));
    }

    #[test]
    fn relink_drops_a_stale_unfiltered_mapping_for_the_same_exe_on_a_different_game() {
        // Simulates the replay scenario: "game.exe" was mapped to the old (id=1) entry;
        // resolving "Log as New Replay" links it to a brand-new id=2 entry instead. The old
        // mapping must not survive, or `find_all_by_process("game.exe")` would return both and
        // arbitrarily pick one on the next launch.
        let old = mapping("game.exe", "session", 1, None);
        assert!(!survives_relink(&old, "game.exe", "session", 2));
    }

    #[test]
    fn relink_preserves_a_title_filtered_mapping_for_the_same_exe() {
        // Deliberate multi-game-per-exe setup (e.g. javaw.exe shared by distinct modpacks) --
        // relinking a different game to the same exe must not silently remove this one.
        let filtered = mapping("javaw.exe", "regular", 1, Some("Modpack A"));
        assert!(survives_relink(&filtered, "javaw.exe", "regular", 2));
    }

    #[test]
    fn relink_preserves_mappings_for_other_exes() {
        let other = mapping("other.exe", "regular", 5, None);
        assert!(survives_relink(&other, "game.exe", "session", 2));
    }

    #[test]
    fn relink_exe_match_is_case_insensitive() {
        let old = mapping("Game.EXE", "session", 1, None);
        assert!(!survives_relink(&old, "game.exe", "session", 2));
    }
}
