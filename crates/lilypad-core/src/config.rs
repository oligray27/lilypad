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
    /// Full path of the executable this mapping was last seen running as.
    ///
    /// `process` alone is a basename, which cannot tell two installs apart — plenty of games ship
    /// a `game.exe` or `launcher.exe`. This is recorded the first time the mapping tracks a
    /// session and then used to disambiguate (see `find_all_for_process`).
    ///
    /// `None` for mappings made before this existed and for any not yet seen running; matching
    /// falls back to the basename in that case, so nothing breaks while it is unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe_path: Option<String>,
}

/// A root folder the user has pointed LilyPad at for non-Steam games — every immediate
/// subfolder inside it is treated as a separate installed game (see `local_games.rs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchedDirectory {
    pub path: String,
}

/// A Steam appid the user has explicitly told LilyPad to never treat as a game — e.g.
/// Wallpaper Engine, which installs and manifests exactly like a real game (its own
/// `appmanifest_*.acf`, its own `steamapps/common/` folder) but obviously isn't one. Unlike
/// `steam::is_known_non_game`'s hardcoded, maintainer-only list (Steamworks Redistributables,
/// Proton itself, etc. — things that are *never* a game for any user), this is user-configured
/// per appid, for the much larger set of things that are legitimate Steam "apps" but not games
/// anyone would want session-tracked. `name` is stored alongside `appid` purely for display in
/// the exclusion list UI, since a bare numeric id isn't meaningful on its own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExcludedApp {
    pub appid: String,
    pub name: String,
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
    // `unattended_mode` (plus the two toggles it saved and restored) lived here. It forced
    // auto-submit on and silenced every notification, as a workaround for a Windows install
    // with no notification component: submission used to wait on the toast reporting dismissal,
    // so a machine that could not show toasts filed good sessions onto the pending queue as
    // failures. Submission no longer depends on the notification system at all (see
    // `auto_submit`), so the workaround is gone. `ProcessMapConfig` does not deny unknown
    // fields, so existing config files carrying these keys still load; the keys are ignored.
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
    /// Steam appids excluded from detection entirely (see `ExcludedApp`) — filtered out of the
    /// installed-games scan by each platform's `refresh_installed_games`, so an excluded app
    /// behaves as though it were never installed.
    #[serde(default)]
    pub excluded_apps: Vec<ExcludedApp>,
    /// Whether `seed_default_exclusions` has already run once for this config -- prevents a
    /// default exclusion (currently just Wallpaper Engine) from silently reappearing after the
    /// user explicitly removes it.
    #[serde(default)]
    pub default_exclusions_seeded: bool,
    /// The note sent with sessions submitted from Steam Gaming Mode (the Decky plugin), which
    /// submits every session without asking. `None` is the standard note, an empty string none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gaming_mode_note: Option<String>,
    /// Gaming Mode's auto-submit switch turned off: instead of submitting, the Decky plugin asks
    /// on game close (notes, submit or not). Stored inverted so existing configs keep
    /// auto-submitting.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gaming_mode_ask: bool,
}

impl ProcessMapConfig {
    pub fn load_from(path: &std::path::Path) -> Self {
        let mut cfg: Self = match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            _ => Self::default(),
        };
        if cfg.seed_default_exclusions() {
            let _ = cfg.save_to(path);
        }
        cfg
    }

    /// One-time seeding of exclusions almost every user wants (currently just Wallpaper Engine,
    /// appid 431960 -- installs and manifests exactly like a real game but obviously isn't one).
    /// Unlike `steam::is_known_non_game`'s hardcoded, unconditional filter, this only
    /// *pre-populates* the user's own editable `excluded_apps` list once -- if they later remove
    /// it, `default_exclusions_seeded` staying `true` means it never comes back. Runs for both a
    /// brand-new config and an existing one from before this field existed (`#[serde(default)]`
    /// gives both the same starting `false`), so upgrading an existing install seeds it too.
    /// Returns `true` if it changed anything, so the caller knows to persist.
    fn seed_default_exclusions(&mut self) -> bool {
        if self.default_exclusions_seeded {
            return false;
        }
        self.default_exclusions_seeded = true;
        if !self.excluded_apps.iter().any(|e| e.appid == "431960") {
            self.excluded_apps.push(ExcludedApp {
                appid: "431960".to_string(),
                name: "Wallpaper Engine".to_string(),
            });
        }
        true
    }

    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap_or_default())
    }

    /// Find all mappings for a process name (multiple games can share the same exe, e.g. javaw.exe).
    ///
    /// Exact, case-insensitive. This used to also accept a *suffix* match
    /// (`process_name.ends_with(m.process)`), which silently attributed unrelated games to each
    /// other: a mapping for `game.exe` matched a running `MyGame.exe`, and one for `e.exe` matched
    /// everything. Callers only ever pass a basename — either `exe().file_name()` or the reported
    /// comm name — so there was no path prefix for a suffix rule to strip.
    ///
    /// It arrived with the Linux support commit, where the plausible motivation is `/proc/pid/comm`
    /// being truncated to 15 characters for Proton-hosted games. A suffix test does not help
    /// there either: a truncated name shares a *prefix* with the real one, not a suffix. So this
    /// loses no working case.
    pub fn find_all_by_process(&self, process_name: &str) -> Vec<&ProcessMapping> {
        self.find_all_for_process(process_name, None)
    }

    /// Find the mappings for a running process, using its executable path to disambiguate
    /// same-named binaries from different installs.
    ///
    /// The path is preferred but not absolute, because making it absolute would silently stop
    /// tracking a game whose install moved — a Steam library moved to another drive would break
    /// every mapping at once, with no error. The rules, in order:
    ///
    /// 1. A mapping whose recorded path *is* this executable wins outright.
    /// 2. Otherwise mappings with no recorded path are used — legacy entries, and ones not yet
    ///    seen running. They get their path filled in the first time they track a session.
    /// 3. Otherwise every candidate names a different file. If those files are gone from disk the
    ///    install moved, so those mappings are matched anyway and will re-record their new path.
    ///    If they still exist, this really is a different game that happens to share a filename,
    ///    and nothing matches.
    ///
    /// The disk check only runs in case 3, which is rare, so the common path costs no I/O.
    pub fn find_all_for_process(
        &self,
        process_name: &str,
        exe_path: Option<&std::path::Path>,
    ) -> Vec<&ProcessMapping> {
        let by_name: Vec<&ProcessMapping> = self
            .mappings
            .iter()
            .filter(|m| m.process.eq_ignore_ascii_case(process_name))
            .collect();
        // A Wine/Proton host binary is shared by every game it runs, so its path says nothing
        // about which game this is -- match by name alone, as if no path were known.
        let Some(actual) = exe_path.filter(|p| !is_shared_game_host(p)) else {
            return by_name;
        };

        let exact: Vec<&ProcessMapping> = by_name
            .iter()
            .copied()
            .filter(|m| m.exe_path.as_deref().is_some_and(|p| same_path(p, actual)))
            .collect();
        if !exact.is_empty() {
            return exact;
        }

        let unrecorded: Vec<&ProcessMapping> = by_name
            .iter()
            .copied()
            .filter(|m| m.exe_path.is_none())
            .collect();
        if !unrecorded.is_empty() {
            return unrecorded;
        }

        by_name
            .into_iter()
            .filter(|m| {
                m.exe_path
                    .as_deref()
                    .is_some_and(|p| !std::path::Path::new(p).exists())
            })
            .collect()
    }

    /// Records the executable a mapping was seen running as, so it can be told apart from a
    /// same-named binary elsewhere. Returns whether anything changed, so the caller knows to save.
    ///
    /// Also updates a path that has gone stale, which is how a moved install heals itself.
    pub fn record_mapping_exe_path(
        &mut self,
        process_name: &str,
        froglog_id: i32,
        game_type: &str,
        exe_path: &std::path::Path,
    ) -> bool {
        // Pinning a mapping to `wine64-preloader` would identify nothing, and once Proton is
        // upgraded the old (still installed) binary would make the mapping stop matching.
        if is_shared_game_host(exe_path) {
            return false;
        }
        let Some(mapping) = self.mappings.iter_mut().find(|m| {
            m.process.eq_ignore_ascii_case(process_name)
                && m.froglog_id == froglog_id
                && m.r#type.eq_ignore_ascii_case(game_type)
        }) else {
            return false;
        };
        let path = exe_path.to_string_lossy().into_owned();
        if mapping.exe_path.as_deref().is_some_and(|p| same_path(p, exe_path)) {
            return false;
        }
        mapping.exe_path = Some(path);
        true
    }

    /// The mapped process name that `comm` is the kernel-truncated form of, if exactly one
    /// mapping could be meant. Linux caps a process's reported name at 15 bytes, and Wine sets
    /// that name from the game's `.exe`, so `HotlineMiami.exe` is reported as `HotlineMiami.ex`
    /// and never matches its mapping exactly. Two mappings sharing the same first 15 bytes are
    /// ambiguous and match nothing, rather than guessing.
    pub fn find_by_truncated_comm(&self, comm: &str) -> Option<String> {
        let mut names = self
            .mappings
            .iter()
            .filter(|m| is_truncated_comm_of(comm, &m.process))
            .map(|m| m.process.as_str());
        let first = names.next()?;
        if names.any(|n| !n.eq_ignore_ascii_case(first)) {
            log::info!("[LilyPad] process name {comm:?} is truncated and matches several mappings; not guessing");
            return None;
        }
        Some(first.to_string())
    }
}

/// The longest process name Linux reports (`TASK_COMM_LEN` is 16 including the terminator).
pub const COMM_MAX_LEN: usize = 15;

/// Whether a reported process name `comm` could be the truncated form of `full`. Always false
/// off Linux, where reported names are not truncated this way.
pub fn is_truncated_comm_of(comm: &str, full: &str) -> bool {
    cfg!(target_os = "linux")
        && comm.len() == COMM_MAX_LEN
        && full.len() > COMM_MAX_LEN
        && full.as_bytes()[..COMM_MAX_LEN].eq_ignore_ascii_case(comm.as_bytes())
}

/// Process names of executables that host many different games (Wine, Proton's Python entry
/// point). Such a name or path identifies the runtime, not the game.
pub fn is_shared_host_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "wine" | "wine64" | "wine-preloader" | "wine64-preloader" | "wineserver" | "proton"
    ) || name.starts_with("python")
}

pub fn is_shared_game_host(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_shared_host_name)
}

/// Removes the mapping of `process` to a game that turned out not to exist, saving the account's
/// map. Returns whether one was removed. Left in place, every future launch would track against
/// an id the server 404s on. The mapping is dropped in memory even if saving fails, so this
/// session does not keep tracking against it; the error is still returned for logging.
pub fn remove_dead_mapping(
    process_map_arc: &std::sync::Arc<std::sync::RwLock<ProcessMapConfig>>,
    auth: &AuthConfig,
    process: &str,
    game_type: &str,
    froglog_id: i32,
) -> Result<bool, String> {
    let map = {
        let mut map = process_map_arc.write().unwrap();
        let before = map.mappings.len();
        map.mappings.retain(|m| {
            !(m.froglog_id == froglog_id
                && m.r#type.eq_ignore_ascii_case(game_type)
                && m.process.eq_ignore_ascii_case(process))
        });
        if map.mappings.len() == before {
            return Ok(false);
        }
        map.clone()
    };
    map.save_to(&process_map_path_for_auth(auth)).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Pins `mapping` to the executable it was just seen running as, saving the account's map.
/// Returns whether anything changed. No-op for a shared Wine/Proton host binary.
pub fn backfill_mapping_exe_path(
    process_map_arc: &std::sync::Arc<std::sync::RwLock<ProcessMapConfig>>,
    auth: &AuthConfig,
    mapping: &ProcessMapping,
    exe_path: &std::path::Path,
) -> Result<bool, String> {
    let mut map = process_map_arc.read().unwrap().clone();
    if !map.record_mapping_exe_path(&mapping.process, mapping.froglog_id, &mapping.r#type, exe_path) {
        return Ok(false);
    }
    map.save_to(&process_map_path_for_auth(auth)).map_err(|e| e.to_string())?;
    *process_map_arc.write().unwrap() = map;
    Ok(true)
}

/// Case-insensitive on Windows, where the same binary is routinely reported with different
/// casing; exact elsewhere.
fn same_path(recorded: &str, actual: &std::path::Path) -> bool {
    let actual = actual.to_string_lossy();
    #[cfg(windows)]
    {
        recorded.eq_ignore_ascii_case(&actual)
    }
    #[cfg(not(windows))]
    {
        recorded == actual
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
        exe_path: None,
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

    /// The suffix rule this replaced would attribute one game's launch to another's mapping,
    /// silently and permanently — `game.exe` matched `MyGame.exe`, and `e.exe` matched everything.
    #[test]
    fn a_mapping_matches_only_its_own_executable() {
        let cfg = ProcessMapConfig {
            mappings: vec![
                mapping("game.exe", "session", 1, None),
                mapping("Fishlike.exe", "session", 2, None),
            ],
            ..Default::default()
        };

        // Exact, and case-insensitive because Windows reports casing inconsistently.
        assert_eq!(cfg.find_all_by_process("game.exe").len(), 1);
        assert_eq!(cfg.find_all_by_process("GAME.EXE").len(), 1);
        assert_eq!(cfg.find_all_by_process("fishlike.exe")[0].froglog_id, 2);

        // A different game whose name merely ends with a mapped one must not match.
        assert!(cfg.find_all_by_process("MyGame.exe").is_empty());
        assert!(cfg.find_all_by_process("NotFishlike.exe").is_empty());
        assert!(cfg.find_all_by_process("othergame.exe").is_empty());
    }

    /// The point of recording a path: two installs shipping `game.exe` are different games, and a
    /// basename alone cannot tell them apart.
    #[test]
    fn a_recorded_path_tells_two_installs_with_the_same_executable_name_apart() {
        let dir = tempfile::tempdir().unwrap();
        let celeste = dir.path().join("Celeste").join("game.exe");
        let hollow = dir.path().join("Hollow Knight").join("game.exe");
        for p in [&celeste, &hollow] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        }
        let mut a = mapping("game.exe", "session", 1, None);
        a.exe_path = Some(celeste.to_string_lossy().into_owned());
        let mut b = mapping("game.exe", "session", 2, None);
        b.exe_path = Some(hollow.to_string_lossy().into_owned());
        let cfg = ProcessMapConfig { mappings: vec![a, b], ..Default::default() };

        let matched = cfg.find_all_for_process("game.exe", Some(&celeste));
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].froglog_id, 1, "matched the wrong install");
        assert_eq!(cfg.find_all_for_process("game.exe", Some(&hollow))[0].froglog_id, 2);

        // A third, unmapped game called game.exe matches neither -- both recorded paths still
        // exist, so this is genuinely a different binary rather than a moved install.
        let stranger = dir.path().join("Other").join("game.exe");
        std::fs::create_dir_all(stranger.parent().unwrap()).unwrap();
        std::fs::write(&stranger, b"x").unwrap();
        assert!(cfg.find_all_for_process("game.exe", Some(&stranger)).is_empty());
    }

    /// Making the path authoritative would silently stop tracking a game whose install moved --
    /// a Steam library moved to another drive would break every mapping at once, with no error.
    /// A recorded path that no longer exists is treated as stale rather than as a mismatch.
    #[test]
    fn a_mapping_whose_install_moved_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        let moved_to = dir.path().join("NewDrive").join("game.exe");
        std::fs::create_dir_all(moved_to.parent().unwrap()).unwrap();
        std::fs::write(&moved_to, b"x").unwrap();

        let mut m = mapping("game.exe", "session", 1, None);
        m.exe_path = Some(dir.path().join("GoneDrive").join("game.exe").to_string_lossy().into_owned());
        let mut cfg = ProcessMapConfig { mappings: vec![m], ..Default::default() };

        let matched = cfg.find_all_for_process("game.exe", Some(&moved_to));
        assert_eq!(matched.len(), 1, "a moved install must not silently stop being tracked");

        // ...and the next session re-pins it, so the stale path does not linger.
        assert!(cfg.record_mapping_exe_path("game.exe", 1, "session", &moved_to));
        assert_eq!(cfg.mappings[0].exe_path.as_deref(), Some(moved_to.to_string_lossy().as_ref()));
        assert!(!cfg.record_mapping_exe_path("game.exe", 1, "session", &moved_to), "no rewrite needed");
    }

    /// Mappings made before the field existed have no path and must keep working untouched.
    #[test]
    fn a_mapping_with_no_recorded_path_still_matches_on_name_alone() {
        let cfg = ProcessMapConfig {
            mappings: vec![mapping("game.exe", "session", 1, None)],
            ..Default::default()
        };
        let anywhere = std::path::Path::new(r"D:\Anywhere\game.exe");
        assert_eq!(cfg.find_all_for_process("game.exe", Some(anywhere)).len(), 1);
        assert_eq!(cfg.find_all_for_process("game.exe", None).len(), 1);
    }

    /// Several games can legitimately share one executable (modpacks under javaw.exe), which is
    /// why this returns a list and the caller disambiguates by window title.
    #[test]
    fn one_executable_can_still_serve_several_mappings() {
        let cfg = ProcessMapConfig {
            mappings: vec![
                mapping("javaw.exe", "session", 1, Some("Modpack A")),
                mapping("javaw.exe", "session", 2, Some("Modpack B")),
            ],
            ..Default::default()
        };
        assert_eq!(cfg.find_all_by_process("javaw.exe").len(), 2);
    }

    fn mapping(process: &str, game_type: &str, froglog_id: i32, title_filter: Option<&str>) -> ProcessMapping {
        ProcessMapping {
            process: process.to_string(),
            r#type: game_type.to_string(),
            froglog_id,
            title: None,
            title_filter: title_filter.map(|s| s.to_string()),
            exe_path: None,
        }
    }

    /// Recording `wine64-preloader` would identify nothing, and after a Proton upgrade the old
    /// (still installed) binary would make the mapping match nothing either.
    #[test]
    fn a_wine_host_is_never_recorded_or_used_as_a_games_path() {
        let wine = std::path::Path::new(
            "/home/u/.local/share/Steam/steamapps/common/Proton 9.0/files/bin/wine64-preloader",
        );
        let mut cfg = ProcessMapConfig { mappings: vec![mapping("Balatro.exe", "session", 1, None)], ..Default::default() };
        assert!(!cfg.record_mapping_exe_path("Balatro.exe", 1, "session", wine));
        assert!(cfg.mappings[0].exe_path.is_none());

        // Even a path recorded by an older build must not stop a newer Proton from matching.
        cfg.mappings[0].exe_path = Some(wine.to_string_lossy().into_owned());
        let newer = std::path::Path::new(
            "/home/u/.local/share/Steam/steamapps/common/Proton - Experimental/files/bin/wine64-preloader",
        );
        assert_eq!(cfg.find_all_for_process("Balatro.exe", Some(newer)).len(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_truncated_wine_process_name_finds_its_mapping_only_when_unambiguous() {
        let mut cfg = ProcessMapConfig {
            mappings: vec![mapping("HotlineMiami.exe", "session", 1, None), mapping("Hades.exe", "session", 2, None)],
            ..Default::default()
        };
        assert_eq!(cfg.find_by_truncated_comm("HotlineMiami.ex").as_deref(), Some("HotlineMiami.exe"));
        // Only a genuinely truncated (15-byte) name is treated as one.
        assert_eq!(cfg.find_by_truncated_comm("HotlineMiami"), None);
        assert_eq!(cfg.find_by_truncated_comm("Hades.exe"), None);
        // Two games sharing their first 15 bytes: refuse to guess.
        cfg.mappings.push(mapping("HotlineMiami.exe2", "session", 3, None));
        assert_eq!(cfg.find_by_truncated_comm("HotlineMiami.ex"), None);
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

    #[test]
    fn seed_default_exclusions_adds_wallpaper_engine_once() {
        let mut cfg = ProcessMapConfig::default();
        assert!(cfg.seed_default_exclusions());
        assert!(cfg.excluded_apps.iter().any(|e| e.appid == "431960"));
        assert!(cfg.default_exclusions_seeded);
    }

    #[test]
    fn seed_default_exclusions_is_a_no_op_once_already_seeded() {
        let mut cfg = ProcessMapConfig::default();
        cfg.default_exclusions_seeded = true;
        assert!(!cfg.seed_default_exclusions());
        assert!(cfg.excluded_apps.is_empty());
    }

    #[test]
    fn seed_default_exclusions_does_not_reintroduce_a_removed_default() {
        // Simulates: fresh config seeded once, user then explicitly removed Wallpaper Engine.
        // Loading again later must not silently bring it back.
        let mut cfg = ProcessMapConfig::default();
        cfg.seed_default_exclusions();
        cfg.excluded_apps.retain(|e| e.appid != "431960");
        assert!(!cfg.seed_default_exclusions());
        assert!(cfg.excluded_apps.is_empty());
    }

    #[test]
    fn seed_default_exclusions_does_not_duplicate_an_existing_entry() {
        let mut cfg = ProcessMapConfig::default();
        cfg.excluded_apps.push(ExcludedApp { appid: "431960".to_string(), name: "Wallpaper Engine".to_string() });
        cfg.seed_default_exclusions();
        assert_eq!(cfg.excluded_apps.iter().filter(|e| e.appid == "431960").count(), 1);
    }
}
