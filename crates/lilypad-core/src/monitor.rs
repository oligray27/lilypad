//! Process monitor: a periodic scan of running processes.
//!
//! This used to try WMI `Win32_ProcessStartTrace` first, treating the scan as a fallback. That
//! event class derives from `Win32_SystemTrace` and is restricted to administrators, so
//! subscribing returned `WBEM_E_ACCESS_DENIED` (0x80041003) in every normal installation --
//! LilyPad is never elevated, since an installed app runs as the user and its autostart entry
//! cannot elevate without prompting on every boot. The WMI path therefore never ran, and the
//! scan has always been the real detection mechanism. The dead branch, and its duplicate copy of
//! the mapped-session detection logic, were removed rather than left to be maintained twice.

use crate::config::{ProcessMapConfig, ProcessMapping};
use crate::library_match::{is_finished_status, LibraryIndex, ResolvedLibraryGame};
use crate::steam::{find_installed_game_for_exe_or_cmd, InstalledGame};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// Filters out companion processes that live inside a game's own install directory but aren't
/// the game itself — crash reporters, anti-cheat services, prerequisite installers. These would
/// otherwise be misattributed as "the game" by install-dir matching (`find_installed_game_for_exe`
/// only checks the folder, not which specific binary in it), and since they can start or exit
/// independently of (and sometimes outlive) the real game process, treating them as a session
/// causes duplicate/garbage session records and can even overwrite a correct auto-created
/// `ProcessMapping` with the helper's own exe name. Not exhaustive — a cooldown (see
/// `POST_SESSION_COOLDOWN` usage in `maybe_start_unmapped_tracking`) is the general-purpose
/// backstop for helpers not on this list.
fn is_known_helper_process(exe_name: &str) -> bool {
    const HELPER_EXE_NAMES: &[&str] = &[
        "unitycrashhandler64.exe",
        "unitycrashhandler32.exe",
        "crashpad_handler.exe",
        "crashreporter.exe",
        "easyanticheat.exe",
        "easyanticheat_launcher.exe",
        "easyanticheat_eos_setup.exe",
        "battleye.exe",
        "beservice.exe",
        "beservice_x64.exe",
        // Epic Online Services -- bundled by many games (not just Epic Games Store titles) for
        // anti-cheat/social/overlay features, installed into the game's own folder right
        // alongside the real exe. Confirmed misattributed as "the game" for For Honor, whose
        // EOS helper is literally named eos.exe.
        "eos.exe",
        "eosbootstrapper.exe",
        "eosoverlayrenderer-win32-shipping.exe",
        "eosoverlayrenderer-win64-shipping.exe",
        "vc_redist.x64.exe",
        "vc_redist.x86.exe",
        "dxsetup.exe",
        "dxwebsetup.exe",
        "ue4prereqsetup_x64.exe",
        "ue5prereqsetup_x64.exe",
        "directx_setup.exe",
    ];
    HELPER_EXE_NAMES.contains(&exe_name.to_lowercase().as_str())
}

/// An unmapped game's session, reported as it starts. Grouped into a struct rather than added as
/// more positional parameters: the ended callback is already five untyped `String`s and a float,
/// and this one carries process identity that is easy to transpose by accident.
#[derive(Debug, Clone)]
pub struct UnmappedSessionStart {
    pub title: String,
    pub appid: String,
    pub exe_name: String,
    pub pid: Pid,
    /// The OS process start time; with the pid this identifies the instance, so recovery can
    /// tell the process it was tracking from a relaunch that reused its pid.
    pub process_started_at_secs: Option<u64>,
    pub replay_of: Option<ResolvedLibraryGame>,
}

/// Blocks until the given PID exits, then fires `on_unmapped_session_ended(title, appid,
/// exe_name, duration_secs, replay_of)`, releases the appid from `currently_tracking` so a
/// later relaunch of the same game starts a fresh tracked session, and records the appid's end
/// time in `last_ended_unmapped` so a trailing companion process (e.g. a crash reporter that
/// outlives the game briefly) doesn't get misattributed as a second session. Mirrors
/// `run_wait_thread`'s exit-detection logic, but for an unmapped (no `ProcessMapping`) installed
/// game rather than a tracked one. `replay_of` is `Some` when this appid actually matches an
/// existing but finished (Completed/DNF) library entry — see `maybe_start_unmapped_tracking`.
#[allow(clippy::too_many_arguments)]
fn run_unmapped_wait_thread(
    pid: Pid,
    title: String,
    appid: String,
    exe_name: String,
    started_at: Instant,
    replay_of: Option<ResolvedLibraryGame>,
    currently_tracking: Arc<RwLock<HashSet<String>>>,
    last_ended_unmapped: Arc<RwLock<HashMap<String, Instant>>>,
    on_unmapped_session_started: Arc<dyn Fn(UnmappedSessionStart) + Send + Sync>,
    on_unmapped_session_ended: Arc<dyn Fn(String, String, String, f64, Option<ResolvedLibraryGame>) + Send + Sync>,
) {
    // Announced before the wait begins, so a frontend can record the session durably while it is
    // still running. Previously an unmapped game was only reported once it had *ended*, so a
    // crash mid-play lost the whole session -- there was never a record of it.
    on_unmapped_session_started(UnmappedSessionStart {
        title: title.clone(),
        appid: appid.clone(),
        exe_name: exe_name.clone(),
        pid,
        process_started_at_secs: process_start_time(pid),
        replay_of: replay_of.clone(),
    });
    std::thread::spawn(move || {
        let exited_at = wait_for_exit_with_relaunch_grace(pid, &exe_name);
        let duration_secs = exited_at.saturating_duration_since(started_at).as_secs_f64();
        currently_tracking.write().unwrap().remove(&appid);
        last_ended_unmapped.write().unwrap().insert(appid.clone(), Instant::now());
        on_unmapped_session_ended(title, appid, exe_name, duration_secs, replay_of);
    });
}

/// Decides whether an *existing* `ProcessMapping` should still be trusted for normal tracking,
/// or whether the mapped game's current (freshly refreshed) status makes this launch look like
/// a possible replay. `None` means proceed with normal tracking as always -- not finished, which
/// covers both "never was" and "was, but `resolve_as_existing`'s `resume_finished_game` call
/// already reset it back to In Progress after the user picked Continue last time". `Some` means
/// the caller should skip normal tracking for this launch and instead route it through
/// `start_replay_prompt_tracking`, exactly like a genuinely unmapped detection. Deliberately has
/// no separate "already asked about this" memory: the mapped game's live status *is* that
/// signal, and unlike a persisted flag it can't go stale -- it only reads as finished when the
/// game is actually, currently finished, whether that's the first time or the fifth.
fn check_mapped_game_needs_replay_prompt(mapping: &ProcessMapping, library_index: &LibraryIndex) -> Option<ResolvedLibraryGame> {
    if mapping.r#type.eq_ignore_ascii_case("live") {
        return None; // live-service games have no finished state
    }
    let resolved = library_index.resolve_by_id(mapping.froglog_id)?;
    if !is_finished_status(&resolved.status) {
        return None;
    }
    Some(resolved.clone())
}

/// Detects and repairs a `ProcessMapping` whose `games` row has vanished out from under it --
/// far and away the most common cause is the website's "move to Live Service" action
/// (`games.js`'s `move-to-live-service`/`bulk-move-to-live-service`), which deletes the old
/// `games` row outright and creates a brand-new `live_service_games` row with a different id,
/// with no way to notify LilyPad directly. Without this check, a mapping created back when the
/// game was still "session"/"regular" keeps pointing at a dead id forever: `resolve_by_id` (used
/// by `check_mapped_game_needs_replay_prompt` above) only ever reads `None` as "not finished, no
/// replay prompt needed" and lets the stale mapping through, so the session tracks fine locally
/// and then 404s the instant it tries to submit (`POST /games/{dead_id}/sessions`).
///
/// Re-resolves via the exe's Steam appid against the *current* `LibraryIndex`, whose
/// `by_appid`/`by_igdb_id` maps (unlike `by_id`) span both `games` and `live_service_games` --
/// the move carries the Steam link across into the new row's `game_platform_links`, so the same
/// appid now resolves to the new id/type. Returns `None` (do nothing, proceed with the mapping
/// as-is) when the mapping isn't orphaned at all, or when it is but nothing re-resolves (e.g. the
/// game was deleted outright rather than moved, or it's a non-Steam install with no appid to key
/// off) -- in the latter case the caller is stuck with a mapping that will keep failing to
/// submit, but that's the pre-existing behavior, not something this makes worse.
fn heal_orphaned_mapping(
    mapping: &ProcessMapping,
    exe_path: Option<&Path>,
    cmd: &[std::ffi::OsString],
    installed_games: &Arc<RwLock<Vec<InstalledGame>>>,
    library_index: &LibraryIndex,
) -> Option<ProcessMapping> {
    if mapping.r#type.eq_ignore_ascii_case("live") {
        return None; // live-service mappings have no games-table id to go stale.
    }
    if library_index.resolve_by_id(mapping.froglog_id).is_some() {
        return None; // still resolves in `games` -- not orphaned.
    }
    let games = installed_games.read().unwrap();
    let (found, _matched_path) = find_installed_game_for_exe_or_cmd(exe_path, cmd, &games)?;
    let resolved = library_index.resolve_by_appid(&found.appid)?;
    Some(ProcessMapping {
        process: mapping.process.clone(),
        r#type: resolved.game_type.clone(),
        froglog_id: resolved.id,
        title: Some(resolved.title.clone()),
        title_filter: mapping.title_filter.clone(),
        exe_path: None,
    })
}

/// Whether `mapping` points at a game that has been deleted from FrogLog. Missing from the cached
/// library is not enough: it can be minutes old, and a game added and linked in the meantime is
/// not in it yet. So the library is fetched again, and only a successful fetch that still lacks
/// the game counts. Never true for a live-service mapping (the index has no by-id view of those),
/// or before the library has loaded at all.
fn mapping_is_dead(
    mapping: &ProcessMapping,
    library_index: &Arc<RwLock<LibraryIndex>>,
    refresh_library_index: &Arc<dyn Fn() -> bool + Send + Sync>,
) -> bool {
    if mapping.r#type.eq_ignore_ascii_case("live") {
        return false;
    }
    let missing = |index: &LibraryIndex| index.is_loaded() && index.resolve_by_id(mapping.froglog_id).is_none();
    if !missing(&library_index.read().unwrap()) {
        return false;
    }
    log::info!(
        "[LilyPad] {} #{} (linked to {}) is not in the cached library; refreshing before treating the link as dead",
        mapping.r#type, mapping.froglog_id, mapping.process
    );
    refresh_library_index() && missing(&library_index.read().unwrap())
}

/// Starts tracking a play session for an already-mapped exe whose target game looks like an
/// unacknowledged possible replay (see `check_mapped_game_needs_replay_prompt`), instead of the
/// normal `on_session_started`/`on_session_ended` flow — reuses the same background wait-thread
/// and pending-submission mechanism as a genuinely unmapped detection (`run_unmapped_wait_thread`),
/// so it surfaces in New Games with the "Continue That Entry" / "Log as New Replay" choice
/// rather than silently resuming the finished entry. Since a `ProcessMapping` has no Steam
/// appid of its own, a synthetic `"mapped:<froglog_id>"` key stands in for one — `PendingGameSubmission.appid`
/// is just an opaque dedup/lookup key everywhere it's used, never assumed to be a real Steam id
/// except when parsed for an appid-based IGDB lookup, which the replay UI never does. Returns
/// `false` (does nothing) if a wait-thread for this same mapping is already running, so this is
/// safe to call on every poll tick / title-filter check while the game keeps running.
fn start_replay_prompt_tracking(
    pid: Pid,
    mapping: &ProcessMapping,
    resolved: ResolvedLibraryGame,
    currently_tracking_unmapped: &Arc<RwLock<HashSet<String>>>,
    last_ended_unmapped: &Arc<RwLock<HashMap<String, Instant>>>,
    on_unmapped_session_started: &Arc<dyn Fn(UnmappedSessionStart) + Send + Sync>,
    on_unmapped_session_ended: &Arc<dyn Fn(String, String, String, f64, Option<ResolvedLibraryGame>) + Send + Sync>,
) -> bool {
    let synthetic_appid = format!("mapped:{}", mapping.froglog_id);
    {
        let mut tracking = currently_tracking_unmapped.write().unwrap();
        if tracking.contains(&synthetic_appid) {
            return false;
        }
        tracking.insert(synthetic_appid.clone());
    }
    let title = mapping.title.clone().unwrap_or_else(|| resolved.title.clone());
    run_unmapped_wait_thread(
        pid,
        title,
        synthetic_appid,
        mapping.process.clone(),
        Instant::now(),
        Some(resolved),
        Arc::clone(currently_tracking_unmapped),
        Arc::clone(last_ended_unmapped),
        Arc::clone(on_unmapped_session_started),
        Arc::clone(on_unmapped_session_ended),
    );
    true
}

/// Handles a process that doesn't match any `ProcessMapping`, but does match an installed
/// Steam game. Three outcomes:
/// - The game is already in the FrogLog library under a known Steam appid (`resolve_by_appid`),
///   its status isn't Completed/DNF, and it has no `ProcessMapping` yet (e.g. resolved from a
///   "New Games" entry, or added to FrogLog directly on the website). `on_already_owned_game_needs_link`
///   fires so the caller can persist the new mapping, and the mapping is also returned so the
///   caller can start tracking *this* launch immediately — deferring to "the next poll tick /
///   launch will pick it up" would silently drop the current session entirely on the WMI path,
///   since a process-start event never refires just because a mapping appeared afterward.
/// - The game resolves by appid but its status IS Completed/DNF — relaunching it looks more
///   like a deliberate replay than a continuation, so instead of silently resuming the old
///   entry, this falls through to the same background wait-thread as a genuinely new game, just
///   carrying `replay_of` so the caller can ask the user "continue that entry, or log this as a
///   new replay?" once the session ends, rather than guessing.
/// - Otherwise (no library match at all), starts a background wait-thread (see
///   `run_unmapped_wait_thread`) so the full play session gets recorded once the game exits,
///   guarded by `currently_tracking` so this is safe to call on every poll tick / WMI event
///   without spawning duplicate wait-threads for a game that's still running.
#[allow(clippy::too_many_arguments)]
fn maybe_start_unmapped_tracking(
    exe_path: Option<&Path>,
    cmd: &[std::ffi::OsString],
    pid: Pid,
    installed_games: &Arc<RwLock<Vec<InstalledGame>>>,
    library_index: &Arc<RwLock<LibraryIndex>>,
    config: &Arc<RwLock<ProcessMapConfig>>,
    currently_tracking: &Arc<RwLock<HashSet<String>>>,
    last_ended_unmapped: &Arc<RwLock<HashMap<String, Instant>>>,
    last_library_refresh: &Arc<RwLock<HashMap<String, Instant>>>,
    refresh_library_index: &Arc<dyn Fn() -> bool + Send + Sync>,
    on_unmapped_session_started: &Arc<dyn Fn(UnmappedSessionStart) + Send + Sync>,
    on_unmapped_session_ended: &Arc<dyn Fn(String, String, String, f64, Option<ResolvedLibraryGame>) + Send + Sync>,
    on_already_owned_game_needs_link: &Arc<dyn Fn(ProcessMapping) + Send + Sync>,
) -> Option<ProcessMapping> {
    // The matched path may be a translated Proton/Wine command-line argument rather than
    // `exe_path` itself (see `find_installed_game_for_exe_or_cmd`) -- the exe name recorded from
    // here on must come from *that* path, not `exe_path`, or every Proton game would get
    // misattributed to whichever one happened to resolve first (they all share the same Wine
    // binary as their resolved `exe_path`).
    let (found, matched_path) = {
        let games = installed_games.read().unwrap();
        find_installed_game_for_exe_or_cmd(exe_path, cmd, &games).map(|(g, p)| (g.clone(), p))?
    };
    log::debug!("[LilyPad] matched installed game appid={} via path={:?}", found.appid, matched_path);
    let exe_name = matched_path.file_name().and_then(|n| n.to_str())?.to_string();
    let exe_name = exe_name.as_str();
    if is_known_helper_process(exe_name) {
        return None;
    }

    // A mapping already covers this executable, so the mapped path tracks it. This is reached
    // for a Proton game, whose `waitforexitandrun` wrapper resolves to the game's `.exe` through
    // its command line while the game's own Wine process matches the mapping by name. The
    // library check further down only catches that when the mapped game resolves by appid; a
    // mapping to an entry with no Steam link, or to one deleted since the last refresh, slipped
    // past it and the same play was recorded twice (once mapped, once as a New Game). Asked
    // with the matched path, so an install that a pinned mapping says is a *different* game
    // with the same filename still proceeds.
    if !config.read().unwrap().find_all_for_process(exe_name, Some(&matched_path)).is_empty() {
        return None;
    }

    // A companion process not on the `is_known_helper_process` blocklist can still start or
    // exit around the same time as the real game (e.g. right after it closes) and get
    // misattributed as a new instance of the same appid. Skip it if we very recently finished
    // (or auto-linked) a session for this exact appid.
    {
        let le = last_ended_unmapped.read().unwrap();
        if let Some(last_time) = le.get(&found.appid) {
            if last_time.elapsed() < POST_SESSION_COOLDOWN {
                return None;
            }
        }
    }

    // Before concluding this game is not in the library, make sure the library we are consulting
    // is current. The cache refreshes on a five-minute timer, so a game added on the website
    // minutes ago is still absent from it -- and the consequence is not a delay but a wrong
    // answer: the game is filed as a New Game the user already owns, which they then have to
    // resolve by hand. Refreshing only at this decision point keeps the cost off the common path,
    // and the rate limit stops a genuinely unknown game refreshing on every launch.
    if library_index.read().unwrap().resolve_by_appid(&found.appid).is_none() {
        let due = last_library_refresh
            .read()
            .unwrap()
            .get(&found.appid)
            .is_none_or(|at| at.elapsed() >= LIBRARY_RECHECK_COOLDOWN);
        if due {
            last_library_refresh
                .write()
                .unwrap()
                .insert(found.appid.clone(), Instant::now());
            log::info!(
                "[LilyPad] {} (appid {}) is not in the cached library; refreshing before treating \
                 it as a new game",
                found.name, found.appid
            );
            // A refresh that fails means the library could not be consulted at all. Declaring the
            // game new on that basis would file something the user already owns into the New
            // Games queue for them to clear by hand -- so decline to decide, and let a later tick
            // try again once the network is back. Skipping costs the session; guessing costs the
            // user's trust in the queue.
            if !refresh_library_index() {
                log::warn!(
                    "[LilyPad] could not check the library for {} (appid {}); not tracking it this \
                     time rather than filing a game that may already be owned",
                    found.name, found.appid
                );
                return None;
            }
        }
    }

    let mut replay_of = None;
    // Drop the library guard before callbacks acquire frontend account/config locks.
    let resolved = library_index.read().unwrap().resolve_by_appid(&found.appid).cloned();
    if let Some(resolved) = resolved {
        // If some OTHER process is already mapped to this exact game (most commonly: the game's
        // own renamed "<Game>.exe" process, when *this* process is a Proton/Wine launcher
        // wrapper that never gets renamed -- e.g. the `python3 <proton> waitforexitandrun <exe>`
        // process, whose own comm/exe never matches the stored mapping by name) -- that other
        // process is already the authoritative tracker for this session. Without this check,
        // both processes independently ran this whole function every poll tick, sometimes
        // reaching two different conclusions for the very same launch (one silently
        // auto-linking, the other seeing the game as finished and prompting a replay decision),
        // confirmed against a real Proton session that produced exactly that.
        if config.read().unwrap().mappings.iter().any(|m| m.froglog_id == resolved.id) {
            return None;
        }
        if !is_finished_status(&resolved.status) {
            last_ended_unmapped.write().unwrap().insert(found.appid.clone(), Instant::now());
            // Anything LilyPad auto-links itself should end up session-tracked, not "regular"
            // (single running hours_played total) -- session data is strictly more useful, and the
            // caller is responsible for actually flipping session_tracking on server-side (see
            // `fix_imported_status_if_needed`'s sibling call, `enable_session_tracking`, in the
            // per-platform on_already_owned_game_needs_link callback). Doesn't apply to live-service
            // games, which are a separate concept entirely. Setting it here (rather than leaving it
            // as whatever `resolved.game_type` already says) matters because this exact value is
            // what decides how *this* session gets submitted once it ends -- if it stayed "regular"
            // here, this session's hours would go through `update_game_hours` instead of
            // `add_game_session`, silently missing the session-tracking flip happening
            // asynchronously alongside it.
            let game_type = if resolved.game_type.eq_ignore_ascii_case("live") {
                resolved.game_type.clone()
            } else {
                "session".to_string()
            };
            let mapping = ProcessMapping {
                process: exe_name.to_string(),
                r#type: game_type,
                froglog_id: resolved.id,
                title: Some(resolved.title.clone()),
                title_filter: None,
                // Recorded the first time this mapping actually tracks a session, where the
                // running process's real path is known.
                exe_path: None,
            };
            on_already_owned_game_needs_link(mapping.clone());
            return Some(mapping);
        }
        // Matched appid, but the entry is Completed/DNF -- don't silently resume it. Fall
        // through to the wait-thread path below like a genuinely new game, carrying the match
        // along so the caller can ask "continue that entry, or start a new replay?" once this
        // session ends.
        replay_of = Some(resolved);
    }
    {
        let mut tracking = currently_tracking.write().unwrap();
        if tracking.contains(&found.appid) {
            return None;
        }
        tracking.insert(found.appid.clone());
    }
    run_unmapped_wait_thread(
        pid,
        found.name,
        found.appid,
        exe_name.to_string(),
        Instant::now(),
        replay_of,
        Arc::clone(currently_tracking),
        Arc::clone(last_ended_unmapped),
        Arc::clone(on_unmapped_session_started),
        Arc::clone(on_unmapped_session_ended),
    );
    None
}

/// Returns window titles belonging to the given PID (Windows only).
/// Used to disambiguate processes that share an exe name (e.g. javaw.exe).
#[cfg(windows)]
pub fn get_window_titles_for_pid(target_pid: u32) -> Vec<String> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    };

    struct EnumData {
        target_pid: u32,
        titles: Vec<String>,
    }

    unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let data = &mut *(lparam.0 as *mut EnumData);
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == data.target_pid {
            let len = GetWindowTextLengthW(hwnd);
            if len > 0 {
                let mut buf = vec![0u16; (len + 1) as usize];
                let written = GetWindowTextW(hwnd, &mut buf);
                if written > 0 {
                    buf.truncate(written as usize);
                    if let Ok(s) = String::from_utf16(&buf) {
                        if !s.is_empty() {
                            data.titles.push(s);
                        }
                    }
                }
            }
        }
        BOOL(1) // continue enumeration
    }

    let mut data = EnumData { target_pid, titles: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(enum_callback), LPARAM(&mut data as *mut EnumData as isize));
    }
    data.titles
}

/// Returns window titles belonging to the given PID via X11/XWayland EWMH hints.
/// Native Wayland windows are invisible to this (no cross-client enumeration API exists
/// there by design), so this only helps for X11 sessions or XWayland-backed windows
/// (e.g. most Proton/Wine games) — same limitation every other Linux tray/tracker app has.
#[cfg(target_os = "linux")]
pub fn get_window_titles_for_pid(target_pid: u32) -> Vec<String> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};

    let mut titles = Vec::new();

    let Ok((conn, screen_num)) = x11rb::connect(None) else {
        return titles; // No X server reachable (pure Wayland session, headless, etc.)
    };
    let Some(screen) = conn.setup().roots.get(screen_num) else {
        return titles;
    };
    let root = screen.root;

    let intern = |name: &str| -> Option<u32> {
        conn.intern_atom(false, name.as_bytes()).ok()?.reply().ok().map(|r| r.atom)
    };

    let Some(net_client_list) = intern("_NET_CLIENT_LIST") else {
        return titles;
    };
    let net_wm_pid = intern("_NET_WM_PID");
    let net_wm_name = intern("_NET_WM_NAME");
    let utf8_string = intern("UTF8_STRING");

    let Some(client_list) = conn
        .get_property(false, root, net_client_list, AtomEnum::WINDOW, 0, u32::MAX)
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return titles;
    };
    let windows: Vec<u32> = client_list.value32().map(|it| it.collect()).unwrap_or_default();

    for win in windows {
        let Some(pid_atom) = net_wm_pid else { continue };
        let pid_val = conn
            .get_property(false, win, pid_atom, AtomEnum::CARDINAL, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut it| it.next()));
        if pid_val != Some(target_pid) {
            continue;
        }

        // Prefer _NET_WM_NAME (UTF8_STRING); fall back to legacy WM_NAME.
        let title = net_wm_name
            .zip(utf8_string)
            .and_then(|(name_atom, type_atom)| {
                conn.get_property(false, win, name_atom, type_atom, 0, u32::MAX)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .and_then(|r| String::from_utf8(r.value).ok())
                    .filter(|s| !s.is_empty())
            })
            .or_else(|| {
                conn.get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, u32::MAX)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .and_then(|r| String::from_utf8(r.value).ok())
                    .filter(|s| !s.is_empty())
            });

        if let Some(t) = title {
            titles.push(t);
        }
    }

    titles
}

#[cfg(all(not(windows), not(target_os = "linux")))]
pub fn get_window_titles_for_pid(_target_pid: u32) -> Vec<String> {
    vec![]
}

/// Given candidates (all mappings for an exe) and current window titles, return the best match.
/// Priority: 1) Exact match (returns immediately), 2) Longest substring match, 3) Fallback to no-filter mapping.
fn pick_mapping(candidates: &[ProcessMapping], window_titles: &[String]) -> Option<ProcessMapping> {
    let mut best_match: Option<&ProcessMapping> = None;
    let mut best_match_len = 0;

    for m in candidates {
        if let Some(filter) = &m.title_filter {
            let filter_lower = filter.to_lowercase();
            
            for title in window_titles {
                let title_lower = title.to_lowercase();
                
                // Check for exact match first (highest priority - returns immediately)
                if title_lower == filter_lower {
                    return Some(m.clone());
                }
                
                // Check for substring match
                if title_lower.contains(&filter_lower) {
                    // Only update if this is a longer filter than current best
                    // (more specific = better match)
                    if filter.len() > best_match_len {
                        best_match = Some(m);
                        best_match_len = filter.len();
                    }
                }
            }
        }
    }

    // If we found a substring match, return it
    if let Some(mapping) = best_match {
        return Some(mapping.clone());
    }

    // Fall back to the first mapping with no title filter
    candidates.iter().find(|m| m.title_filter.is_none()).cloned()
}
/// How long to ignore a process after a session ends (prevents brief launcher re-spawns from
/// starting a phantom second session, e.g. javaw.exe relaunching during Minecraft mod pack close).
const POST_SESSION_COOLDOWN: Duration = Duration::from_secs(15);

/// Minimum gap between on-demand library refreshes for the *same* game.
///
/// Keyed per appid rather than globally: a global gap meant a genuinely unlisted game refreshed
/// the whole library once a minute for as long as it ran -- roughly 120 redundant fetches over a
/// two-hour session, none of which could find anything, since the game only appears once the user
/// resolves it. Per-game with a long gap keeps the valuable case (a game added on the website
/// shortly before launch is found immediately) while the five-minute periodic refresh covers
/// anything added later.
const LIBRARY_RECHECK_COOLDOWN: Duration = Duration::from_secs(10 * 60);

/// Games whose exe carries a UAC elevation manifest actually run twice: the unelevated process
/// exits the instant the prompt is accepted, and the elevated copy relaunches right after.
/// Ending the session on that first exit produced a junk seconds-long session followed by a
/// second tracked session for the same sitting. The tell for "this exit is an elevation
/// handoff, not the user quitting" is NOT session length (users legitimately log in for
/// well under a minute just to check a shop) — it's `consent.exe`, the process Windows runs
/// for exactly as long as a UAC prompt is on screen. So: a same-named successor already
/// running at exit is adopted instantly (the blocking `ShellExecuteEx` pattern spawns the
/// elevated copy before the stub finishes exiting), and only while consent.exe is alive do we
/// keep waiting for one to appear (the fire-and-exit launcher pattern, where nothing is
/// running while the user reads the prompt). No prompt on screen means every session —
/// however short — ends with zero added delay. On Linux consent.exe never exists, so this
/// whole mechanism is inert there (UAC is a Windows concept).
///
/// `UAC_PROMPT_WAIT_CAP` bounds the consent.exe wait (secure-desktop auto-deny is 2 minutes;
/// slack on top). `POST_PROMPT_SCAN` keeps scanning briefly after the prompt closes, covering
/// the accept case where the elevated process takes a moment to appear. The only lingering
/// cost: if the session's own exit coincides with an unrelated app's UAC prompt, the end
/// report waits for that prompt to resolve — rare, and the reported duration is unaffected.
const UAC_PROMPT_WAIT_CAP: Duration = Duration::from_secs(150);
const POST_PROMPT_SCAN: Duration = Duration::from_secs(3);

/// Anti-cheat launchers produce the same split session as UAC elevation, but with no
/// `consent.exe` to key off. For Honor is the observed case: `forhonor.exe` starts, runs about
/// four seconds, exits as EAC takes over, and the *real* `forhonor.exe` appears roughly sixteen
/// seconds later. The original single-pass successor scan looked once, at the instant of exit,
/// found nothing, and ended the session -- producing a junk four-second session followed by a
/// separate real one.
///
/// Fall Guys uses EAC too and never showed this, because its launcher and game are different
/// executables (`start_protected_game.exe` -> `FallGuys_client_game.exe`), so the handoff never
/// looks like one process exiting. The tell is not the anti-cheat, it is a same-named relaunch.
///
/// The tell used here is that the segment which just ended was *short*. That is deliberately
/// weaker than the `consent.exe` signal and is used only to decide whether to keep **looking**,
/// never to conclude a handoff happened: if no successor appears, the session still ends with
/// its true duration. `exited_at` is captured before the scan, so the recorded duration is
/// unaffected either way -- the only cost is that a genuinely short session has its *end report*
/// delayed by up to `HANDOFF_SCAN`.
///
/// This is why the earlier note here rejected session length as a tell. That objection was about
/// using length to *classify* the exit, which would have mis-ended real short sessions; using it
/// to extend the search does not.
/// Sized from what a launcher stub actually is, not from what a short play session might be.
/// For Honor's two stubs ran 4.8s and 9.0s; a handoff is over in seconds. The first cut used
/// 60s, which made *every* sub-minute session wait out `HANDOFF_SCAN` before its end was
/// reported -- a real 34-second session took 64 seconds to show its post-play notification.
/// Fifteen seconds still covers a handoff comfortably while leaving ordinary short sessions
/// ending instantly.
const HANDOFF_SEGMENT_MAX: Duration = Duration::from_secs(15);
const HANDOFF_SCAN: Duration = Duration::from_secs(30);

/// How often the successor scan re-checks the process list.
const SCAN_TICK: Duration = Duration::from_millis(300);

/// How long the successor scan keeps looking for a same-named relaunch after a process exits.
///
/// Extracted from the scan loop so the decision table is explicit and testable without a clock:
/// it takes elapsed durations rather than reading one. The rules, in priority order:
///
/// 1. A UAC prompt is on screen -- keep looking up to `UAC_PROMPT_WAIT_CAP`.
/// 2. A prompt has been and gone -- keep looking for `POST_PROMPT_SCAN` after it closed.
/// 3. The segment that just ended was short -- keep looking up to `HANDOFF_SCAN`
///    (the anti-cheat/launcher handoff case).
/// 4. Otherwise -- a normal exit after a real session. Stop immediately, adding no delay.
struct RelaunchScan {
    segment_was_short: bool,
    prompt_seen: bool,
    since_prompt_gone: Duration,
}

impl RelaunchScan {
    /// `segment` is how long the process that just exited was running -- measured *before* the
    /// scan starts, so the scan's own duration cannot count towards it.
    fn new(segment: Duration) -> Self {
        Self {
            segment_was_short: segment < HANDOFF_SEGMENT_MAX,
            prompt_seen: false,
            since_prompt_gone: Duration::ZERO,
        }
    }

    /// `true` to keep scanning. `tick` is how long the caller waits between calls.
    fn keep_scanning(&mut self, prompt_active: bool, since_exit: Duration, tick: Duration) -> bool {
        if prompt_active {
            self.prompt_seen = true;
            self.since_prompt_gone = Duration::ZERO;
            return since_exit <= UAC_PROMPT_WAIT_CAP;
        }
        if self.prompt_seen {
            let gone_for = self.since_prompt_gone;
            self.since_prompt_gone += tick;
            return gone_for <= POST_PROMPT_SCAN;
        }
        self.segment_was_short && since_exit <= HANDOFF_SCAN
    }
}

/// True while a UAC prompt is on screen (consent.exe alive). Always false on non-Windows.
fn uac_prompt_active(system: &System) -> bool {
    system
        .processes()
        .values()
        .any(|p| p.name().to_string_lossy().eq_ignore_ascii_case("consent.exe"))
}

/// Whether `process`'s resolved exe name or reported comm/name matches `process_name`
/// case-insensitively — the same "is this the process we're tracking" test used both to
/// detect a still-alive pid after an untrustworthy `wait()` return and to spot a same-named
/// successor process (UAC elevation relaunch / self-restart).
fn process_matches_name(process: &sysinfo::Process, process_name: &str) -> bool {
    let exe_matches = process
        .exe()
        .and_then(|path| path.file_name().and_then(|n| n.to_str()))
        .is_some_and(|e| e.eq_ignore_ascii_case(process_name));
    let comm = process.name().to_string_lossy();
    // A Wine-hosted game reports its `.exe` name cut to 15 bytes on Linux; see
    // `ProcessMapConfig::find_by_truncated_comm`.
    exe_matches
        || comm.eq_ignore_ascii_case(process_name)
        || crate::config::is_truncated_comm_of(&comm, process_name)
}

/// The same test, additionally requiring the process to be *the same executable on disk* when
/// the original's path is known.
///
/// A bare name match cannot tell two installs apart. Plenty of games ship a `launcher.exe`,
/// `start.exe` or `game.exe`, so a session whose process exits while an unrelated game with an
/// identically-named binary happens to be running would adopt that other game's process and keep
/// billing time to the wrong entry — phase 3's "two same-named games cannot be silently
/// attributed to one another".
///
/// Falls back to name-only when either path is unavailable: `exe()` can be empty for a process
/// whose path cannot be read, and refusing to match then would break adoption entirely rather
/// than making it stricter.
fn process_matches_identity(
    process: &sysinfo::Process,
    process_name: &str,
    expected_exe: Option<&Path>,
) -> bool {
    if !process_matches_name(process, process_name) {
        return false;
    }
    match (expected_exe, process.exe()) {
        (Some(expected), Some(actual)) => paths_equal(expected, actual),
        _ => true,
    }
}

/// Compares two executable paths for "same file on disk". Case-insensitive on Windows, where
/// the same binary is routinely reported with different casing.
fn paths_equal(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        a.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

/// How a process's exit was observed. Logged so that "LilyPad could not wait on this game" is a
/// recorded fact with a cause, rather than something inferred from a generic early return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitMechanism {
    /// Waited on a real `SYNCHRONIZE` handle. The kernel signalled the exit, so this is
    /// authoritative: no enumeration cross-check is needed or performed.
    Handle,
    /// No handle could be opened, so exit was inferred from the process disappearing from
    /// enumeration. Carries why the handle was refused.
    Polling { reason: String },
}

/// Opens a `SYNCHRONIZE` handle for `pid`, or reports why Windows refused.
///
/// A refusal here is the *only* sound basis for claiming a game cannot be waited on: it comes
/// with a real Win32 error code. `ERROR_ACCESS_DENIED` is what an elevated or protected process
/// actually produces. Previously the claim was inferred from `sysinfo`'s `wait()` returning
/// while the process was still listed, which is far more often just exit teardown -- the process
/// object outlives the last handle closing, so a correct wait routinely looks "untrustworthy"
/// for a moment. That inference libelled ordinary games (see the Besiege/Fishlike entries in
/// `PLAN.md`); this does not.
#[cfg(windows)]
fn open_wait_handle(pid: Pid) -> Result<windows::Win32::Foundation::HANDLE, String> {
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
    unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, usize::from(pid) as u32) }.map_err(|e| {
        match e.code().0 as u32 {
            // 0x80070005 -- the genuine "elevated or protected" case.
            0x8007_0005 => "access denied; the game is running elevated or is protected".to_string(),
            // 0x80070057 -- the pid is already gone, so there is nothing to wait on.
            0x8007_0057 => "the process no longer exists".to_string(),
            _ => format!("{e}"),
        }
    })
}

/// Blocks on a `SYNCHRONIZE` handle until the process exits. `true` once it has.
#[cfg(windows)]
fn wait_on_handle(handle: windows::Win32::Foundation::HANDLE) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{WaitForSingleObject, INFINITE};
    let result = unsafe { WaitForSingleObject(handle, INFINITE) };
    unsafe { let _ = CloseHandle(handle); }
    result == WAIT_OBJECT_0
}

/// Blocks until `pid` exits, reporting which mechanism established that.
///
/// Prefers a native handle wait; falls back to existence polling only when Windows actually
/// refuses a handle, and says why. `DISCOVER_TIMEOUT` bounds the case of a pid that vanishes
/// before it is ever observed.
///
/// "Still our process" is decided by the pid's OS start time when it is known, not by name.
/// That is the exact pid-reuse guard, and it is the only test that works for a tracked process
/// whose name is not the game's: an unmapped Proton game is tracked through Proton's
/// `waitforexitandrun` wrapper (reported as `proton`/`python3`), while the session carries the
/// game's `.exe` name, so a name test reported the game as exited on the first check.
fn wait_for_process_exit(
    system: &mut System,
    pid: Pid,
    process_name: &str,
    expected_exe: Option<&Path>,
    started_at_secs: Option<u64>,
    discover_timeout: Duration,
) -> WaitMechanism {
    #[cfg(windows)]
    let polling_reason = match open_wait_handle(pid) {
        Ok(handle) => {
            if wait_on_handle(handle) {
                return WaitMechanism::Handle;
            }
            "the handle wait failed".to_string()
        }
        Err(reason) => reason,
    };
    #[cfg(not(windows))]
    let polling_reason = "not supported on this platform".to_string();

    // Existence polling. The identity requirement doubles as the pid-reuse guard: a recycled
    // pid belonging to an unrelated process must read as "our process exited".
    let discover_start = Instant::now();
    loop {
        system.refresh_processes(ProcessesToUpdate::All);
        let still_ours = |process: &sysinfo::Process| match started_at_secs {
            Some(started) => process.start_time() == started,
            None => process_matches_identity(process, process_name, expected_exe),
        };
        match system.process(pid) {
            Some(process) if still_ours(process) => {}
            // Never seen at all: give it a moment to appear before concluding it is gone.
            None if discover_start.elapsed() <= discover_timeout => {}
            _ => return WaitMechanism::Polling { reason: polling_reason },
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Blocks until the given PID *and any same-named successor process* have exited; returns the
/// moment the last of them was seen exiting (so callers can exclude any trailing successor
/// scan from the session's duration). Successor adoption is what keeps a UAC-elevation
/// relaunch inside one session instead of splitting it in two (see the comment above
/// `UAC_PROMPT_WAIT_CAP` for the full picture).
pub fn wait_for_exit_with_relaunch_grace(initial_pid: Pid, process_name: &str) -> Instant {
    const DISCOVER_TIMEOUT: Duration = Duration::from_secs(5);
    let mut system = System::new_all();
    let mut pid = initial_pid;

    // The executable this session is actually tracking, read once while the process is still
    // alive. Everything below matches against *this file on disk*, not merely its name, so a
    // same-named binary from another game's install directory cannot be adopted as a successor
    // or mistaken for our still-running process. `None` when the path cannot be read, in which
    // case matching falls back to the name alone as before.
    system.refresh_processes(ProcessesToUpdate::Some(&[initial_pid]));
    let expected_exe = system
        .process(initial_pid)
        .and_then(|p| p.exe().map(|path| path.to_path_buf()));
    let mut started_at_secs = system.process(initial_pid).map(|p| p.start_time());
    // A session identified only by a shared runtime (Proton's `python3` entry point, a Wine
    // binary) has no game-specific name to recognise a relaunch by: adopting "another python3"
    // would attach whatever unrelated script happens to be running and keep billing the session.
    // Such a tracked process lives exactly as long as the game, so it needs no successor.
    let adopt_successors = !crate::config::is_shared_host_name(process_name);
    if expected_exe.is_none() {
        log::debug!(
            "[LilyPad] {process_name} (pid {initial_pid:?}) has no readable executable path; \
             successor matching falls back to the process name alone"
        );
    }

    loop {
        // When *this* pid started being waited on. Reset per adopted successor, so "was the
        // segment short?" asks about the process that just exited, not the whole session: a
        // four-second launcher stub is short, the real game it hands off to is not.
        let segment_started = Instant::now();

        // A native handle wait is authoritative -- the kernel signals the exit -- so nothing is
        // cross-checked against enumeration afterwards. That check is what used to produce the
        // "can't be waited on directly (likely UAC-elevated or anticheat-protected)" line for
        // perfectly ordinary games: a process object stays enumerable through teardown, so a
        // correct wait looks untrustworthy for a moment at exit. Polling is now entered only
        // when Windows actually refuses a handle, and the refusal is reported with its cause.
        match wait_for_process_exit(&mut system, pid, process_name, expected_exe.as_deref(), started_at_secs, DISCOVER_TIMEOUT) {
            WaitMechanism::Handle => log::debug!(
                "[LilyPad] {process_name} (pid {pid:?}) exit observed by handle wait"
            ),
            // Polling is simply how exits are observed off Windows; only on Windows does it mean
            // a handle was refused, which is worth recording.
            WaitMechanism::Polling { reason } if cfg!(windows) => log::info!(
                "[LilyPad] {process_name} (pid {pid:?}) could not be waited on ({reason}) \
                 -- exit observed by existence polling instead"
            ),
            WaitMechanism::Polling { .. } => log::info!(
                "[LilyPad] {process_name} (pid {pid:?}) exited"
            ),
        }
        let exited_at = Instant::now();

        // Successor scan: does a same-named process (by resolved exe name or comm, mirroring
        // the poll loop's own matching) exist to adopt this session? A long segment ending gets
        // a single pass and no added delay. The scan is extended while a UAC prompt is up or
        // just closed (see UAC_PROMPT_WAIT_CAP), and after a short segment, which is the
        // anti-cheat launcher-handoff case (see HANDOFF_SEGMENT_MAX).
        // Captured before the scan: using `segment_started.elapsed()` afterwards would fold the
        // scan's own 30 seconds into the reported figure, and previously did.
        let segment = segment_started.elapsed();
        let mut scan = RelaunchScan::new(segment);
        let segment_was_short = scan.segment_was_short;
        let successor = loop {
            if !adopt_successors {
                break None;
            }
            system.refresh_processes(ProcessesToUpdate::All);
            let found = system.processes().iter().find_map(|(p2, proc)| {
                if *p2 == pid || proc.thread_kind().is_some() {
                    return None;
                }
                process_matches_identity(proc, process_name, expected_exe.as_deref()).then_some(*p2)
            });
            if found.is_some() {
                break found;
            }
            if !scan.keep_scanning(uac_prompt_active(&system), exited_at.elapsed(), SCAN_TICK) {
                break None;
            }
            std::thread::sleep(SCAN_TICK);
        };
        let prompt_seen = scan.prompt_seen;
        match successor {
            Some(next) => {
                let cause = if prompt_seen {
                    "UAC elevation"
                } else if segment_was_short {
                    "launcher/anti-cheat handoff"
                } else {
                    "self-restart"
                };
                log::info!(
                    "[LilyPad] {} (pid {:?}) exited after {:?} but relaunched as pid {:?} ({cause}) -- session continues",
                    process_name, pid, segment, next
                );
                pid = next;
                started_at_secs = system.process(next).map(|p| p.start_time());
            }
            None => {
                if prompt_seen {
                    log::info!(
                        "[LilyPad] {} (pid {:?}) exited; a UAC prompt came and went with no same-named relaunch -- ending session",
                        process_name, pid
                    );
                } else if segment_was_short {
                    log::info!(
                        "[LilyPad] {} (pid {:?}) exited after {:?}; no relaunch within {:?} -- ending session",
                        process_name, pid, segment, HANDOFF_SCAN
                    );
                }
                return exited_at;
            }
        }
    }
}

/// Reads a process's OS start time (seconds since the Unix epoch).
///
/// Paired with the pid this identifies a process *instance*, not just a slot: Windows recycles
/// pids, and a game closed and relaunched while LilyPad was down can land on the same one. The
/// start time is what distinguishes "still the session we were tracking" from "a fresh launch
/// that happens to look like it".
pub fn process_start_time(pid: Pid) -> Option<u64> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]));
    system.process(pid).map(|p| p.start_time())
}

/// Active session: we're currently tracking this process.
#[derive(Debug, Clone)]
pub struct ActiveSession {
    #[allow(dead_code)]
    pub process_name: String,
    #[allow(dead_code)]
    pub mapping: ProcessMapping,
    /// The exact process being tracked. Read by frontends that need to record process identity
    /// durably -- the session-start callback fires *after* this is stored, so it is readable
    /// from there without widening `run_poll_loop`'s callback signature.
    pub pid: Pid,
    /// `pid`'s OS start time; see `process_start_time`. `None` when the process vanished before
    /// it could be read, which only makes recovery fall back to matching by name.
    pub started_at_secs: Option<u64>,
    pub started_at: Instant,
}

impl ActiveSession {
    /// The session's length so far: the same clock its submitted duration is taken from.
    pub fn elapsed_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    /// "Hades (1h 23m)", for tray menus and tooltips.
    pub fn label(&self) -> String {
        let title = self.mapping.title.clone().unwrap_or_else(|| self.process_name.clone());
        format!("{title} ({})", crate::duration::format_session_duration(self.elapsed_secs() as f64))
    }
}

/// Clears `current_session` only if it is still the session for `process_name`.
pub fn clear_session_for(current_session: &RwLock<Option<ActiveSession>>, process_name: &str) {
    let mut current = current_session.write().unwrap();
    if current.as_ref().is_some_and(|s| s.process_name.eq_ignore_ascii_case(process_name)) {
        *current = None;
    }
}

/// Block until the process (and any same-named UAC/self-restart successor — see
/// `wait_for_exit_with_relaunch_grace`) exits, then send session-ended data on the channel.
fn run_wait_thread(
    pid: Pid,
    process_name: String,
    mapping: ProcessMapping,
    started_at: Instant,
    sender: mpsc::Sender<(String, ProcessMapping, f64)>,
) {
    std::thread::spawn(move || {
        let exited_at = wait_for_exit_with_relaunch_grace(pid, &process_name);
        let duration_secs = exited_at.saturating_duration_since(started_at).as_secs_f64();
        let _ = sender.send((process_name, mapping, duration_secs));
    });
}


/// Run the monitor: scan running processes every `poll_interval_secs` and start a session for
/// anything that matches. Exit detection is per-session (see `wait_for_exit_with_relaunch_grace`).
///
/// The scan is also what catches a game that was already running when LilyPad started, which no
/// process-start event could ever report.
///
/// # Overlapping games
///
/// **LilyPad tracks one mapped session at a time.** This is a supported constraint, not an
/// oversight, and it is asymmetric in a way worth knowing:
///
/// - Only *mapped* sessions occupy `current_session`. While one is active the scan is skipped
///   entirely, so neither a second mapped game nor any unmapped game is detected.
/// - *Unmapped* sessions are keyed by appid in `currently_tracking_unmapped` and do not occupy
///   `current_session`, so several can run concurrently, and a mapped game can still start while
///   they do.
///
/// The cost is bounded rather than total. A game launched during an active session is not lost —
/// the scan resumes the moment that session ends and picks it up on the next tick, so it is
/// recorded as a partial session starting from then. What is lost is the overlap period.
///
/// Tracking concurrent mapped sessions was considered and rejected for now: `current_session` is
/// a single slot that the tray title, force-stop, the heartbeat and the ledger's
/// `active_ledger_id` all assume, so it is a structural change rather than a local one. The
/// cheaper half-measure — scanning during a session purely to *report* the overlap — was also
/// rejected: it would run a full process enumeration every `poll_interval_secs` throughout
/// gameplay, spending CPU precisely when a game is using it, to produce a log line. The skip is
/// logged at debug instead.
///
/// `installed_games` and `library_index` are refreshed by the caller (Steam library scan +
/// FrogLog games/wishlist fetch, respectively) and read live here, the same way `config` is.
/// `on_unmapped_session_ended(title, appid, exe_name, duration_secs, replay_of)` fires once per
/// full play session (launch to close) of a process that doesn't match any `ProcessMapping` and
/// either doesn't match anything in the user's FrogLog library at all, or matches an entry
/// that's Completed/DNF (`replay_of` is `Some` in that case — see `maybe_start_unmapped_tracking`)
/// — the whole session's duration is tracked in the background the same way a mapped session
/// would be, so the caller finds out (and can notify) only once the game has actually closed.
/// `on_already_owned_game_needs_link(mapping)` fires instead when the installed game turns out
/// to already be in the library (by exact Steam appid) but just has no `ProcessMapping` yet —
/// the caller should persist it. The *current* launch is tracked immediately using that same
/// mapping (via the normal `on_session_started`/`on_session_ended` callbacks) rather than
/// waiting for a later one, since e.g. a WMI process-start event never refires on its own.
/// `on_dead_mapping(mapping)` fires when a running game's mapping points at an entry deleted
/// from FrogLog (see `mapping_is_dead`); the caller must remove it from `config` before
/// returning, and the game is then detected as unmapped.
#[allow(clippy::too_many_arguments)]
pub fn run_poll_loop(
    config: Arc<RwLock<ProcessMapConfig>>,
    current_session: Arc<RwLock<Option<ActiveSession>>>,
    shutdown: Arc<AtomicBool>,
    force_stopped_process: Arc<RwLock<Option<String>>>,
    poll_interval_secs: u64,
    on_session_started: impl Fn(String, ProcessMapping) + Send + Sync + 'static,
    mut on_session_ended: impl FnMut(String, ProcessMapping, f64) + Send + 'static,
    installed_games: Arc<RwLock<Vec<InstalledGame>>>,
    library_index: Arc<RwLock<LibraryIndex>>,
    refresh_library_index: impl Fn() -> bool + Send + Sync + 'static,
    on_unmapped_session_started: impl Fn(UnmappedSessionStart) + Send + Sync + 'static,
    on_unmapped_session_ended: impl Fn(String, String, String, f64, Option<ResolvedLibraryGame>) + Send + Sync + 'static,
    on_already_owned_game_needs_link: impl Fn(ProcessMapping) + Send + Sync + 'static,
    on_dead_mapping: impl Fn(ProcessMapping) + Send + Sync + 'static,
) {
    let (tx, rx) = mpsc::channel::<(String, ProcessMapping, f64)>();

    // Tracks when the last session ended per process name, to suppress phantom re-launches.
    let last_ended: Arc<RwLock<Option<(String, Instant)>>> =
        Arc::new(RwLock::new(None));

    // Appids currently being tracked as an active unmapped session, so a still-running game
    // doesn't spawn a second wait-thread on every poll tick / WMI event. Cleared as soon as
    // that game's session ends, so relaunching it later starts a fresh tracked session.
    let currently_tracking_unmapped: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));

    // When an unmapped/already-owned appid's session most recently ended (or was auto-linked),
    // so a companion process (e.g. a crash reporter that outlives the real game briefly) isn't
    // misattributed as a second session of the same appid within POST_SESSION_COOLDOWN.
    let last_ended_unmapped: Arc<RwLock<HashMap<String, Instant>>> = Arc::new(RwLock::new(HashMap::new()));

    // Wrap in Arc so the scan thread can share them.
    let on_started: Arc<dyn Fn(String, ProcessMapping) + Send + Sync + 'static> =
        Arc::new(on_session_started);
    let refresh_library: Arc<dyn Fn() -> bool + Send + Sync + 'static> =
        Arc::new(refresh_library_index);
    let last_library_refresh: Arc<RwLock<HashMap<String, Instant>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let on_unmapped_started: Arc<dyn Fn(UnmappedSessionStart) + Send + Sync + 'static> =
        Arc::new(on_unmapped_session_started);
    let on_unmapped_ended: Arc<dyn Fn(String, String, String, f64, Option<ResolvedLibraryGame>) + Send + Sync + 'static> =
        Arc::new(on_unmapped_session_ended);
    let on_already_owned: Arc<dyn Fn(ProcessMapping) + Send + Sync + 'static> =
        Arc::new(on_already_owned_game_needs_link);

    log::info!("[LilyPad] scanning for game processes every {poll_interval_secs}s");
    {
        let config = Arc::clone(&config);
        let current_session = Arc::clone(&current_session);
        let shutdown = Arc::clone(&shutdown);
        let tx = tx.clone();
        let last_ended_poll = Arc::clone(&last_ended);
        let force_stopped_poll = Arc::clone(&force_stopped_process);
        let installed_games_poll = Arc::clone(&installed_games);
        let library_index_poll = Arc::clone(&library_index);
        let currently_tracking_unmapped_poll = Arc::clone(&currently_tracking_unmapped);
        let last_ended_unmapped_poll = Arc::clone(&last_ended_unmapped);
        let refresh_library_poll = Arc::clone(&refresh_library);
        let last_library_refresh_poll = Arc::clone(&last_library_refresh);
        let on_unmapped_started_poll = Arc::clone(&on_unmapped_started);
        let on_unmapped_ended_poll = Arc::clone(&on_unmapped_ended);
        let on_already_owned_poll = Arc::clone(&on_already_owned);
        // Called on this thread with `current_session` locked: it must not touch that, and it
        // must drop the mapping from `config` before returning, or the next scan tracks it again.
        let on_dead_mapping_poll = on_dead_mapping;

        std::thread::spawn(move || {
            let mut system = System::new_all();
            while !shutdown.load(Ordering::SeqCst) {
                let session_started = {
                    // Cloned rather than held as a live read guard: the unmapped-detection path
                    // below can synchronously call back into code that write-locks this same
                    // `config` Arc (e.g. auto-linking an already-owned game's exe) — holding a
                    // read guard across that call would deadlock the thread against itself.
                    let cfg = config.read().unwrap().clone();
                    let mut cur = current_session.write().unwrap();
                    // OVERLAPPING GAMES: LilyPad tracks one *mapped* session at a time, by
                    // design. See `run_poll_loop`'s docs for the full policy and its cost.
                    if cur.is_some() {
                        log::debug!(
                            "[LilyPad] scan skipped: already tracking {:?}; a second mapped game \
                             will be picked up when this session ends",
                            cur.as_ref().map(|s| (&s.process_name, &s.mapping.title, s.started_at.elapsed()))
                        );
                        None
                    } else {
                        // Plain `refresh_processes(ProcessesToUpdate::All)` is equivalent to
                        // `ProcessRefreshKind::new().with_memory().with_cpu().with_disk_usage()
                        // .with_exe(UpdateKind::OnlyIfNotSet)` -- it does NOT include `cmd` at
                        // all. `System::new_all()` (used once, at thread startup) does a fully
                        // comprehensive refresh that does populate cmd, so a process already
                        // running when this thread starts gets correct cmd data forever after --
                        // but any process that starts *later* is only ever discovered through
                        // this per-tick refresh, whose cmd never gets populated, leaving
                        // `p.cmd()` permanently empty for it. That's exactly why Proton-game
                        // detection worked when the game was already running before LilyPad
                        // started, but silently never fired when LilyPad was already running and
                        // the game started afterward -- `find_proton_exe_path`'s cmdline scan had
                        // nothing to look at. Explicitly requesting cmd (and exe, for safety)
                        // fixes it for both orderings.
                        system.refresh_processes_specifics(
                            ProcessesToUpdate::All,
                            ProcessRefreshKind::new()
                                .with_memory()
                                .with_cpu()
                                .with_disk_usage()
                                .with_exe(UpdateKind::Always)
                                .with_cmd(UpdateKind::Always),
                        );
                        let mut found = None;
                        'proc_scan: for (pid, p) in system.processes().iter() {
                            // sysinfo surfaces individual threads as their own entries in this
                            // map (their `Pid` is really the kernel thread id, not the owning
                            // process's), always sharing the same comm/exe/cmdline as their
                            // parent process for as long as they haven't changed their own name
                            // -- confirmed against a real Among Us session, where the game's main
                            // process (a single stable pid) came with 80-90+ such thread entries
                            // that appear and disappear continuously as the engine's thread pool
                            // churns. Matching against one of these instead of the real process
                            // is exactly why exit detection (and, before the mapped-game replay
                            // check existed, which appid a launch resolved to) was unreliable --
                            // whichever pid happened to be examined first could easily be a
                            // thread that exits seconds later while the actual game keeps
                            // running, reporting a false "session ended" every time that happens.
                            // `thread_kind()` is `None` for a genuine process and always `None`
                            // on non-Linux, so this is a no-op everywhere else.
                            if p.thread_kind().is_some() {
                                continue 'proc_scan;
                            }
                            // Try the resolved binary name first (right for native processes),
                            // then fall back to the reported process name/"comm" (right for
                            // Wine/Proton-hosted Windows games: Wine itself stays the real
                            // /proc/pid/exe target — e.g. wine64/wine-preloader — but renames
                            // the process's comm to the target .exe, e.g. "Balatro.exe", via
                            // prctl specifically so tools can identify it that way).
                            let exe_name = p.exe().and_then(|path| {
                                path.file_name().and_then(|n| n.to_str().map(String::from))
                            });
                            let comm_name = p.name().to_string_lossy().into_owned();

                            let (name, candidates): (String, Vec<ProcessMapping>) = {
                                let mut result = None;
                                for candidate in exe_name.iter().chain(std::iter::once(&comm_name)) {
                                    // The path disambiguates same-named binaries from different
                                    // installs; see `find_all_for_process` for how it degrades
                                    // when unrecorded or stale.
                                    let matches: Vec<ProcessMapping> = cfg
                                        .find_all_for_process(candidate, p.exe())
                                        .into_iter()
                                        .cloned()
                                        .collect();
                                    if !matches.is_empty() {
                                        result = Some((candidate.clone(), matches));
                                        break;
                                    }
                                }
                                // A Wine-hosted game whose `.exe` name was cut to 15 bytes. The
                                // session is named after the full mapped name, which the exit
                                // waiter also recognises in truncated form.
                                if result.is_none() {
                                    if let Some(full) = cfg.find_by_truncated_comm(&comm_name) {
                                        let matches: Vec<ProcessMapping> = cfg
                                            .find_all_for_process(&full, p.exe())
                                            .into_iter()
                                            .cloned()
                                            .collect();
                                        if !matches.is_empty() {
                                            result = Some((full, matches));
                                        }
                                    }
                                }
                                match result {
                                    Some(v) => v,
                                    None => (exe_name.unwrap_or(comm_name), Vec::new()),
                                }
                            };
                            if candidates.is_empty() {
                                if !cfg.disable_unmapped_game_detection {
                                    if p.cmd().iter().any(|a| a.to_string_lossy().to_lowercase().ends_with(".exe")) {
                                        log::debug!(
                                            "[LilyPad] unmapped-detection candidate pid={:?} exe={:?} cmd={:?}",
                                            pid,
                                            p.exe(),
                                            p.cmd(),
                                        );
                                    }
                                    if let Some(mapping) = maybe_start_unmapped_tracking(
                                        p.exe(),
                                        p.cmd(),
                                        *pid,
                                        &installed_games_poll,
                                        &library_index_poll,
                                        &config,
                                        &currently_tracking_unmapped_poll,
                                        &last_ended_unmapped_poll,
                                        &last_library_refresh_poll,
                                        &refresh_library_poll,
                                        &on_unmapped_started_poll,
                                        &on_unmapped_ended_poll,
                                        &on_already_owned_poll,
                                    ) {
                                        // Already-owned game just got auto-linked — track this
                                        // launch now via the normal path below instead of
                                        // waiting for the next poll tick to notice the mapping.
                                        //
                                        // Matched through Proton's wrapper, this process is
                                        // `python3`/`wine64`: name the session after the game, so
                                        // a force-stop blocks the game's own Wine process too, not
                                        // every other process sharing that runtime.
                                        let name = if crate::config::is_shared_host_name(&name) {
                                            mapping.process.clone()
                                        } else {
                                            name
                                        };
                                        *cur = Some(ActiveSession {
                                            process_name: name.clone(),
                                            mapping: mapping.clone(),
                                            pid: *pid,
                                            started_at_secs: Some(p.start_time()),
                                            started_at: Instant::now(),
                                        });
                                        found = Some((*pid, name, mapping));
                                        break 'proc_scan;
                                    }
                                }
                                continue 'proc_scan;
                            }
                            // Skip if this process is in its post-session cooldown window.
                            {
                                let le = last_ended_poll.read().unwrap();
                                if let Some((ref last_proc, last_time)) = *le {
                                    if last_proc.eq_ignore_ascii_case(&name)
                                        && last_time.elapsed() < POST_SESSION_COOLDOWN
                                    {
                                        continue 'proc_scan;
                                    }
                                }
                            }
                            // Skip if user force-stopped this process (still running)
                            {
                                let fs = force_stopped_poll.read().unwrap();
                                if let Some(ref stopped) = *fs {
                                    if stopped.eq_ignore_ascii_case(&name) {
                                        continue 'proc_scan;
                                    }
                                }
                            }
                            let needs_title_check =
                                candidates.iter().any(|m| m.title_filter.is_some());
                            let mapping = if needs_title_check {
                                let window_titles =
                                    get_window_titles_for_pid(usize::from(*pid) as u32);
                                pick_mapping(&candidates, &window_titles)
                            } else {
                                candidates.into_iter().next()
                            };
                            if let Some(mapping) = mapping {
                                let healed = heal_orphaned_mapping(&mapping, p.exe(), p.cmd(), &installed_games_poll, &library_index_poll.read().unwrap());
                                let mapping = match healed {
                                    Some(healed) => {
                                        log::info!(
                                            "[LilyPad] healed orphaned mapping for {}: {} #{} -> {} #{}",
                                            mapping.process, mapping.r#type, mapping.froglog_id, healed.r#type, healed.froglog_id
                                        );
                                        on_already_owned_poll(healed.clone());
                                        healed
                                    }
                                    None => mapping,
                                };
                                // Its game was deleted on the website and nothing re-resolves it.
                                // Tracking it would only fail to submit, launch after launch, so
                                // unlink it instead: from the next scan the game is detected like
                                // any game not in FrogLog, and its sessions go to New Games.
                                if mapping_is_dead(&mapping, &library_index_poll, &refresh_library_poll) {
                                    log::warn!(
                                        "[LilyPad] {} is linked to {} #{}, which no longer exists in FrogLog; unlinking it",
                                        mapping.process, mapping.r#type, mapping.froglog_id
                                    );
                                    on_dead_mapping_poll(mapping);
                                    continue 'proc_scan;
                                }
                                let replay = check_mapped_game_needs_replay_prompt(&mapping, &library_index_poll.read().unwrap());
                                if let Some(resolved) = replay {
                                    start_replay_prompt_tracking(
                                        *pid,
                                        &mapping,
                                        resolved,
                                        &currently_tracking_unmapped_poll,
                                        &last_ended_unmapped_poll,
                                        &on_unmapped_started_poll,
                                        &on_unmapped_ended_poll,
                                    );
                                    continue 'proc_scan;
                                }
                                *cur = Some(ActiveSession {
                                    process_name: name.clone(),
                                    mapping: mapping.clone(),
                                    pid: *pid,
                                    started_at_secs: Some(p.start_time()),
                                    started_at: Instant::now(),
                                });
                                found = Some((*pid, name, mapping));
                                break 'proc_scan;
                            }
                        }
                        found
                    }
                };

                if let Some((pid, process_name, mapping)) = session_started {
                    let started_at = current_session
                        .read()
                        .unwrap()
                        .as_ref()
                        .map(|s| s.started_at)
                        .unwrap_or_else(Instant::now);
                    on_started(process_name.clone(), mapping.clone());
                    run_wait_thread(pid, process_name, mapping, started_at, tx.clone());
                }

                for _ in 0..poll_interval_secs {
                    if shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        });
    }

    // Session-ended receiver (always runs).
    let current_session = Arc::clone(&current_session);
    let shutdown = Arc::clone(&shutdown);
    std::thread::spawn(move || loop {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok((process_name, mapping, duration_secs)) => {
                // Record end time before clearing session so the cooldown window starts immediately.
                *last_ended.write().unwrap() = Some((process_name.clone(), Instant::now()));
                // Only this waiter's own session. A force-stopped game's waiter reports long after
                // the tray cleared its session, by which point another game may be tracked; clearing
                // that one would make the next scan start it a second time.
                clear_session_for(&current_session, &process_name);
                on_session_ended(process_name, mapping, duration_secs);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_link_callback_does_not_hold_the_library_lock() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("game.exe");
        let installed = Arc::new(RwLock::new(vec![InstalledGame {
            appid: "123".into(), name: "Test game".into(),
            install_dir: directory.path().to_path_buf(),
        }]));
        let game = serde_json::from_value(serde_json::json!({
            "id": 7, "title": "Test game", "steam_app_id": 123,
        })).unwrap();
        let library = Arc::new(RwLock::new(LibraryIndex::build(&[game], &[], &[])));
        let callback_library = library.clone();
        let callback: Arc<dyn Fn(ProcessMapping) + Send + Sync> = Arc::new(move |_| {
            assert!(callback_library.try_write().is_ok(), "callback retained the library guard");
        });
        let result = maybe_start_unmapped_tracking(
            Some(&executable), &[], Pid::from(123usize), &installed, &library,
            &Arc::new(RwLock::new(ProcessMapConfig::default())),
            &Arc::new(RwLock::new(HashSet::new())),
            &Arc::new(RwLock::new(HashMap::new())),
            &Arc::new(RwLock::new(HashMap::new())),
            &(Arc::new(|| true) as Arc<dyn Fn() -> bool + Send + Sync>),
            &(Arc::new(|_| panic!("owned game must not start an unmapped session")) as Arc<dyn Fn(UnmappedSessionStart) + Send + Sync>),
            &(Arc::new(|_, _, _, _, _| {}) as Arc<dyn Fn(String, String, String, f64, Option<ResolvedLibraryGame>) + Send + Sync>),
            &callback,
        );
        assert_eq!(result.unwrap().froglog_id, 7);
    }

    /// One Proton launch is two processes: the wrapper (matched to the game through its command
    /// line) and the game's Wine process (matched to the mapping by name). With a mapping in
    /// place the wrapper must not also start an unmapped session, even when the mapped game
    /// cannot be resolved by appid -- the double-recording seen in real testing.
    #[test]
    fn a_mapped_executable_is_not_also_tracked_as_a_new_game() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("About Fishing.exe");
        let installed = Arc::new(RwLock::new(vec![InstalledGame {
            appid: "999".into(), name: "About Fishing".into(),
            install_dir: directory.path().to_path_buf(),
        }]));
        // The library does not know this appid (deleted game, or an entry with no Steam link).
        let library = Arc::new(RwLock::new(LibraryIndex::build(&[], &[], &[])));
        let config = Arc::new(RwLock::new(ProcessMapConfig {
            mappings: vec![ProcessMapping {
                process: "About Fishing.exe".into(), r#type: "session".into(), froglog_id: 5,
                title: None, title_filter: None, exe_path: None,
            }],
            ..Default::default()
        }));
        let result = maybe_start_unmapped_tracking(
            Some(&executable), &[], Pid::from(123usize), &installed, &library, &config,
            &Arc::new(RwLock::new(HashSet::new())),
            &Arc::new(RwLock::new(HashMap::new())),
            &Arc::new(RwLock::new(HashMap::new())),
            &(Arc::new(|| true) as Arc<dyn Fn() -> bool + Send + Sync>),
            &(Arc::new(|_| panic!("a mapped game must not start an unmapped session")) as Arc<dyn Fn(UnmappedSessionStart) + Send + Sync>),
            &(Arc::new(|_, _, _, _, _| {}) as Arc<dyn Fn(String, String, String, f64, Option<ResolvedLibraryGame>) + Send + Sync>),
            &(Arc::new(|_| panic!("a mapped game must not be auto-linked")) as Arc<dyn Fn(ProcessMapping) + Send + Sync>),
        );
        assert!(result.is_none());
    }

    /// Spawns something that stays alive until killed, so a wait can be observed against it.
    #[cfg(windows)]
    fn spawn_long_lived() -> std::process::Child {
        std::process::Command::new("cmd")
            .args(["/C", "ping -n 30 127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("could not spawn a test process")
    }

    /// The regression this replaced: an ordinary game's exit was being reported as "can't be
    /// waited on directly (likely UAC-elevated or anticheat-protected)", because a correct wait
    /// was cross-checked against enumeration and a process stays enumerable through teardown.
    /// A normal process must resolve by handle wait, never by polling.
    #[cfg(windows)]
    #[test]
    fn an_ordinary_process_exit_is_observed_by_handle_wait_not_polling() {
        let mut child = spawn_long_lived();
        let pid = Pid::from(child.id() as usize);

        let waiter = std::thread::spawn(move || {
            let mut system = System::new_all();
            wait_for_process_exit(&mut system, pid, "cmd.exe", None, None, Duration::from_secs(5))
        });

        // Let the waiter open its handle and block before the process goes away.
        std::thread::sleep(Duration::from_millis(500));
        child.kill().expect("could not kill the test process");
        let _ = child.wait();

        assert_eq!(waiter.join().unwrap(), WaitMechanism::Handle);
    }

    /// A refusal must carry a real reason, so "this game cannot be waited on" is a recorded
    /// fact rather than an inference from a generic early return.
    #[cfg(windows)]
    #[test]
    fn a_refused_handle_reports_why() {
        let mut child = spawn_long_lived();
        let pid = Pid::from(child.id() as usize);
        child.kill().unwrap();
        // Reaped, so the pid is genuinely gone rather than a zombie.
        let _ = child.wait();

        match open_wait_handle(pid) {
            Err(reason) => assert!(
                reason.contains("no longer exists") || reason.contains("access denied"),
                "expected a named cause, got: {reason}"
            ),
            // Pid reuse inside the test window is possible but vanishingly unlikely; if it
            // happens the handle is for an unrelated process and proves nothing either way.
            Ok(handle) => {
                use windows::Win32::Foundation::CloseHandle;
                unsafe { let _ = CloseHandle(handle); }
            }
        }
    }

    const NO_PROMPT: bool = false;
    const PROMPT_UP: bool = true;

    /// The For Honor case, with the timings straight from the log that reported it: the stub
    /// ran ~4s, exited, and the real `forhonor.exe` appeared ~16s later. The old single-pass
    /// scan stopped at the instant of exit, producing a junk 4-second session plus a separate
    /// real one.
    #[test]
    fn a_short_segment_keeps_looking_long_enough_to_catch_an_anti_cheat_relaunch() {
        // For Honor's observed stub length.
        let mut scan = RelaunchScan::new(Duration::from_millis(4_815));
        // Still looking across the whole 16-second gap.
        for secs in [0, 1, 5, 10, 16] {
            assert!(
                scan.keep_scanning(NO_PROMPT, Duration::from_secs(secs), SCAN_TICK),
                "gave up {secs}s after exit, before the relaunch appeared"
            );
        }
        // But bounded: it does not wait for ever on a game that really did just close.
        assert!(!scan.keep_scanning(NO_PROMPT, HANDOFF_SCAN + Duration::from_secs(1), SCAN_TICK));
    }

    /// The property the original single-pass design was protecting, and which must survive:
    /// a real session ending adds no delay at all.
    ///
    /// The 34-second case is the regression that made this a test rather than a comment: with
    /// `HANDOFF_SEGMENT_MAX` at 60s, an ordinary half-minute session sat through the whole
    /// 30-second scan before its post-play notification appeared.
    #[test]
    fn a_normal_exit_after_a_real_session_stops_immediately() {
        for segment in [
            HANDOFF_SEGMENT_MAX,
            Duration::from_secs(34),
            Duration::from_secs(64),
            Duration::from_secs(3_600),
        ] {
            let mut scan = RelaunchScan::new(segment);
            assert!(
                !scan.keep_scanning(NO_PROMPT, Duration::ZERO, SCAN_TICK),
                "a {segment:?} session should end with no added delay"
            );
        }
    }

    /// The boundary itself: a launcher stub is seconds long, a real session is not.
    #[test]
    fn only_launcher_length_segments_count_as_short() {
        assert!(RelaunchScan::new(Duration::from_secs(9)).segment_was_short);
        assert!(!RelaunchScan::new(Duration::from_secs(20)).segment_was_short);
    }

    #[test]
    fn a_uac_prompt_extends_the_scan_and_outlives_a_short_segment_window() {
        let mut scan = RelaunchScan::new(Duration::from_secs(600));
        // A prompt on screen keeps the scan alive well past HANDOFF_SCAN...
        assert!(scan.keep_scanning(PROMPT_UP, HANDOFF_SCAN + Duration::from_secs(30), SCAN_TICK));
        assert!(scan.prompt_seen);
        // ...but not past the cap.
        assert!(!scan.keep_scanning(PROMPT_UP, UAC_PROMPT_WAIT_CAP + Duration::from_secs(1), SCAN_TICK));
    }

    #[test]
    fn a_closed_prompt_gets_a_brief_scan_then_stops() {
        let mut scan = RelaunchScan::new(Duration::from_secs(600));
        assert!(scan.keep_scanning(PROMPT_UP, Duration::from_secs(1), SCAN_TICK));
        // Prompt gone: keep looking briefly for the elevated copy, then stop. Measured rather
        // than counted, so the assertion stays true if SCAN_TICK changes.
        let mut scanned_for = Duration::ZERO;
        while scan.keep_scanning(NO_PROMPT, Duration::from_secs(2), SCAN_TICK) {
            scanned_for += SCAN_TICK;
            assert!(scanned_for < POST_PROMPT_SCAN * 2, "post-prompt scan did not terminate");
        }
        assert!(
            scanned_for >= POST_PROMPT_SCAN,
            "stopped after {scanned_for:?}, before the {POST_PROMPT_SCAN:?} post-prompt window"
        );
    }

    /// A prompt reappearing (a second elevation, or one redrawn on the secure desktop) resets
    /// the post-prompt window rather than letting the earlier one expire mid-wait.
    #[test]
    fn a_reappearing_prompt_resets_the_post_prompt_window() {
        let mut scan = RelaunchScan::new(Duration::from_secs(600));
        assert!(scan.keep_scanning(PROMPT_UP, Duration::from_secs(1), SCAN_TICK));
        assert!(scan.keep_scanning(NO_PROMPT, Duration::from_secs(2), SCAN_TICK));
        assert!(scan.keep_scanning(PROMPT_UP, Duration::from_secs(3), SCAN_TICK));
        assert_eq!(scan.since_prompt_gone, Duration::ZERO);
        assert!(scan.keep_scanning(NO_PROMPT, Duration::from_secs(4), SCAN_TICK));
    }

    /// Phase 3's "two same-named games cannot be silently attributed to one another". Plenty of
    /// games ship a `launcher.exe` or `game.exe`; adopting one as another's successor would keep
    /// billing time to the wrong library entry.
    #[test]
    fn two_installs_sharing_an_executable_name_are_not_the_same_process() {
        let ours = Path::new(r"D:\Games\Celeste\game.exe");
        let theirs = Path::new(r"D:\Games\Hollow Knight\game.exe");
        assert!(!paths_equal(ours, theirs), "different installs must not compare equal");

        // Windows reports the same binary with inconsistent casing; that must still match.
        let same_shouted = Path::new(r"D:\GAMES\Celeste\GAME.EXE");
        #[cfg(windows)]
        assert!(paths_equal(ours, same_shouted), "casing must not split one install in two");
        #[cfg(not(windows))]
        let _ = same_shouted;

        assert!(paths_equal(ours, Path::new(r"D:\Games\Celeste\game.exe")));
    }

    fn active(process_name: &str) -> ActiveSession {
        ActiveSession {
            process_name: process_name.into(),
            mapping: ProcessMapping {
                process: process_name.into(), r#type: "session".into(), froglog_id: 1,
                title: None, title_filter: None, exe_path: None,
            },
            pid: Pid::from(1usize),
            started_at_secs: None,
            started_at: Instant::now(),
        }
    }

    /// Force-stop X, start Y, then X exits: Y must still be the tracked session, or the next
    /// scan starts Y a second time.
    #[test]
    fn a_late_exit_does_not_clear_another_games_session() {
        let current = RwLock::new(Some(active("y.exe")));
        clear_session_for(&current, "x.exe");
        assert_eq!(current.read().unwrap().as_ref().map(|s| s.process_name.as_str()), Some("y.exe"));
        clear_session_for(&current, "Y.EXE");
        assert!(current.read().unwrap().is_none());
    }

    /// Relaunch adoption must not attach an unrelated process that merely shares a runtime.
    #[test]
    fn shared_runtimes_are_not_game_identities() {
        use crate::config::{is_shared_game_host, is_shared_host_name};
        for name in ["python3", "python3.12", "wine64-preloader", "wine", "proton", "wineserver"] {
            assert!(is_shared_host_name(name), "{name}");
        }
        for name in ["Balatro.exe", "portal2_linux", "winemaker.exe", "HotlineMiami.exe"] {
            assert!(!is_shared_host_name(name), "{name}");
        }
        assert!(is_shared_game_host(Path::new(
            "/home/u/.local/share/Steam/steamapps/common/Proton 9.0/files/bin/wine64-preloader"
        )));
    }

    #[test]
    fn filters_known_helper_processes() {
        assert!(is_known_helper_process("UnityCrashHandler64.exe"));
        assert!(is_known_helper_process("unitycrashhandler64.exe"));
        assert!(is_known_helper_process("EasyAntiCheat_Launcher.exe"));
        // For Honor mapped this instead of the real game exe -- see the comment above the list.
        assert!(is_known_helper_process("eos.exe"));
        assert!(is_known_helper_process("EOS.exe"));
        assert!(is_known_helper_process("EOSBootstrapper.exe"));
    }

    fn sample_game(id: i32, title: &str, appid: i64, session_tracking: bool) -> crate::api::Game {
        crate::api::Game {
            id,
            title: Some(title.to_string()),
            hours_played: None,
            description: None,
            img: None,
            platform: None,
            genre: None,
            dev: None,
            studio_country: None,
            start_date: None,
            end_date: None,
            rating: None,
            dnf: None,
            is_public: None,
            rel_date: None,
            status: None,
            forklift_certified: None,
            session_tracking: Some(session_tracking),
            steam_app_id: Some(serde_json::json!(appid)),
            igdb_id: None,
            platform_links: None,
        }
    }

    fn sample_live_service(id: i32, title: &str, appid: i64) -> crate::api::LiveServiceGame {
        crate::api::LiveServiceGame {
            id,
            title: Some(title.to_string()),
            total_hours: None,
            session_count: None,
            last_session_date: None,
            steam_app_id: Some(serde_json::json!(appid)),
            igdb_id: None,
            platform_links: None,
        }
    }

    #[test]
    fn heals_a_mapping_whose_game_moved_from_games_to_live_service() {
        // Mirrors the real bug: the website's "move to Live Service" action deletes the
        // `games` row a `ProcessMapping` was pointing at and creates a new
        // `live_service_games` row (different id), carrying the Steam appid across. The
        // stale local mapping (still "session" type, still the old id) must be repointed
        // at the new live-service id, not left to 404 on submit.
        let games: Vec<crate::api::Game> = vec![];
        let live_service = vec![sample_live_service(99, "Destiny 2", 1085660)];
        let library_index = LibraryIndex::build(&games, &[], &live_service);

        let mapping = ProcessMapping {
            process: "destiny2.exe".to_string(),
            r#type: "session".to_string(),
            froglog_id: 5, // the old, now-deleted `games` id
            title: Some("Destiny 2".to_string()),
            title_filter: None,
            exe_path: None,
        };
        let installed_games: Arc<RwLock<Vec<InstalledGame>>> = Arc::new(RwLock::new(vec![InstalledGame {
            appid: "1085660".to_string(),
            name: "Destiny 2".to_string(),
            install_dir: std::path::PathBuf::from("C:/Games/Destiny 2"),
        }]));
        let exe_path = std::path::PathBuf::from("C:/Games/Destiny 2/destiny2.exe");

        let healed = heal_orphaned_mapping(&mapping, Some(&exe_path), &[], &installed_games, &library_index)
            .expect("orphaned mapping should heal to the new live-service entry");
        assert_eq!(healed.r#type, "live");
        assert_eq!(healed.froglog_id, 99);
        assert_eq!(healed.process, "destiny2.exe");
        assert_eq!(healed.title.as_deref(), Some("Destiny 2"));
    }

    #[test]
    fn leaves_a_still_resolving_mapping_untouched() {
        let games = vec![sample_game(5, "Destiny 2", 1085660, true)];
        let library_index = LibraryIndex::build(&games, &[], &[]);
        let mapping = ProcessMapping {
            process: "destiny2.exe".to_string(),
            r#type: "session".to_string(),
            froglog_id: 5,
            title: Some("Destiny 2".to_string()),
            title_filter: None,
            exe_path: None,
        };
        let installed_games: Arc<RwLock<Vec<InstalledGame>>> = Arc::new(RwLock::new(vec![]));
        assert!(heal_orphaned_mapping(&mapping, None, &[], &installed_games, &library_index).is_none());
    }

    #[test]
    fn a_mapping_is_dead_only_when_a_fresh_library_still_lacks_its_game() {
        let mapping = |r#type: &str, froglog_id| ProcessMapping {
            process: "Celeste.exe".to_string(),
            r#type: r#type.to_string(),
            froglog_id,
            title: Some("Celeste".to_string()),
            title_filter: None,
            exe_path: None,
        };
        let refreshes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // A refresh that installs `fresh` (or fails, with `None`), counting how often it runs.
        let refresher = |index: &Arc<RwLock<LibraryIndex>>, fresh: Option<LibraryIndex>| -> Arc<dyn Fn() -> bool + Send + Sync> {
            let (index, refreshes) = (Arc::clone(index), Arc::clone(&refreshes));
            Arc::new(move || {
                refreshes.fetch_add(1, Ordering::SeqCst);
                match &fresh {
                    Some(fresh) => {
                        *index.write().unwrap() = fresh.clone();
                        true
                    }
                    None => false,
                }
            })
        };
        let with = |ids: &[i32]| {
            let games: Vec<_> = ids.iter().map(|&id| sample_game(id, "Celeste", 504230, true)).collect();
            LibraryIndex::build(&games, &[], &[])
        };

        // Present in the cache: trusted, no network.
        let index = Arc::new(RwLock::new(with(&[7])));
        assert!(!mapping_is_dead(&mapping("session", 7), &index, &refresher(&index, None)));
        assert_eq!(refreshes.load(Ordering::SeqCst), 0);

        // Deleted: missing from the cache and from a fresh fetch.
        let index = Arc::new(RwLock::new(with(&[])));
        assert!(mapping_is_dead(&mapping("session", 7), &index, &refresher(&index, Some(with(&[])))));

        // Linked to a game added since the cache was built: the fresh fetch has it.
        let index = Arc::new(RwLock::new(with(&[])));
        assert!(!mapping_is_dead(&mapping("session", 7), &index, &refresher(&index, Some(with(&[7])))));

        // Offline: the fetch fails, so nothing is concluded.
        let index = Arc::new(RwLock::new(with(&[])));
        assert!(!mapping_is_dead(&mapping("session", 7), &index, &refresher(&index, None)));

        // No library yet (or logged out): an empty index proves nothing.
        let index = Arc::new(RwLock::new(LibraryIndex::default()));
        let before = refreshes.load(Ordering::SeqCst);
        assert!(!mapping_is_dead(&mapping("session", 7), &index, &refresher(&index, Some(with(&[])))));
        assert_eq!(refreshes.load(Ordering::SeqCst), before);

        // Live-service entries are not in the by-id view, so they are never judged.
        let index = Arc::new(RwLock::new(with(&[])));
        assert!(!mapping_is_dead(&mapping("live", 7), &index, &refresher(&index, Some(with(&[])))));
    }

    #[test]
    fn leaves_a_live_type_mapping_untouched() {
        // "live" mappings never had a `games`-table id to begin with -- resolve_by_id would
        // never find them even when perfectly healthy, so healing must not even attempt it.
        let library_index = LibraryIndex::build(&[], &[], &[]);
        let mapping = ProcessMapping {
            process: "destiny2.exe".to_string(),
            r#type: "live".to_string(),
            froglog_id: 99,
            title: Some("Destiny 2".to_string()),
            title_filter: None,
            exe_path: None,
        };
        let installed_games: Arc<RwLock<Vec<InstalledGame>>> = Arc::new(RwLock::new(vec![]));
        assert!(heal_orphaned_mapping(&mapping, None, &[], &installed_games, &library_index).is_none());
    }

    #[test]
    fn leaves_a_genuinely_orphaned_mapping_untouched_when_nothing_resolves() {
        // The game was deleted outright (not moved) -- no appid to re-resolve to, so healing
        // must not fabricate a mapping.
        let library_index = LibraryIndex::build(&[], &[], &[]);
        let mapping = ProcessMapping {
            process: "gone.exe".to_string(),
            r#type: "regular".to_string(),
            froglog_id: 5,
            title: Some("Deleted Game".to_string()),
            title_filter: None,
            exe_path: None,
        };
        let installed_games: Arc<RwLock<Vec<InstalledGame>>> = Arc::new(RwLock::new(vec![]));
        assert!(heal_orphaned_mapping(&mapping, None, &[], &installed_games, &library_index).is_none());
    }
}
