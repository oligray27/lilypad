//! Process monitor: WMI event-based start detection (Windows), poll-based fallback.

use crate::config::{ProcessMapConfig, ProcessMapping};
use crate::library_match::LibraryIndex;
use crate::steam::{find_installed_game_for_exe_or_cmd, InstalledGame};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessesToUpdate, System};

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

/// Blocks until the given PID exits, then fires `on_unmapped_session_ended(title, appid,
/// exe_name, duration_secs)`, releases the appid from `currently_tracking` so a later relaunch
/// of the same game starts a fresh tracked session, and records the appid's end time in
/// `last_ended_unmapped` so a trailing companion process (e.g. a crash reporter that outlives
/// the game briefly) doesn't get misattributed as a second session. Mirrors `run_wait_thread`'s
/// exit-detection logic, but for an unmapped (no `ProcessMapping`) installed game rather than
/// a tracked one.
#[allow(clippy::too_many_arguments)]
fn run_unmapped_wait_thread(
    pid: Pid,
    title: String,
    appid: String,
    exe_name: String,
    started_at: Instant,
    currently_tracking: Arc<RwLock<HashSet<String>>>,
    last_ended_unmapped: Arc<RwLock<HashMap<String, Instant>>>,
    on_unmapped_session_ended: Arc<dyn Fn(String, String, String, f64) + Send + Sync>,
) {
    std::thread::spawn(move || {
        let mut system = System::new_all();
        let wait_start = Instant::now();
        const DISCOVER_TIMEOUT: Duration = Duration::from_secs(5);

        loop {
            system.refresh_processes(ProcessesToUpdate::All);
            if let Some(process) = system.process(pid) {
                process.wait();
                break;
            }
            if wait_start.elapsed() > DISCOVER_TIMEOUT {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        let duration_secs = started_at.elapsed().as_secs_f64();
        currently_tracking.write().unwrap().remove(&appid);
        last_ended_unmapped.write().unwrap().insert(appid.clone(), Instant::now());
        on_unmapped_session_ended(title, appid, exe_name, duration_secs);
    });
}

/// Handles a process that doesn't match any `ProcessMapping`, but does match an installed
/// Steam game. Two outcomes:
/// - The game is already in the FrogLog library under a known Steam appid (`resolve_by_appid`)
///   but has no `ProcessMapping` yet (e.g. resolved from a "New Games" entry, or added to
///   FrogLog directly on the website). `on_already_owned_game_needs_link` fires so the caller
///   can persist the new mapping, and the mapping is also returned so the caller can start
///   tracking *this* launch immediately — deferring to "the next poll tick / launch will pick
///   it up" would silently drop the current session entirely on the WMI path, since a process-
///   start event never refires just because a mapping appeared afterward.
/// - Otherwise, starts a background wait-thread (see `run_unmapped_wait_thread`) so the full
///   play session gets recorded once the game exits, guarded by `currently_tracking` so this is
///   safe to call on every poll tick / WMI event without spawning duplicate wait-threads for a
///   game that's still running.
#[allow(clippy::too_many_arguments)]
fn maybe_start_unmapped_tracking(
    exe_path: Option<&Path>,
    cmd: &[std::ffi::OsString],
    pid: Pid,
    installed_games: &Arc<RwLock<Vec<InstalledGame>>>,
    library_index: &Arc<RwLock<LibraryIndex>>,
    currently_tracking: &Arc<RwLock<HashSet<String>>>,
    last_ended_unmapped: &Arc<RwLock<HashMap<String, Instant>>>,
    on_unmapped_session_ended: &Arc<dyn Fn(String, String, String, f64) + Send + Sync>,
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

    if let Some(resolved) = library_index.read().unwrap().resolve_by_appid(&found.appid) {
        last_ended_unmapped.write().unwrap().insert(found.appid.clone(), Instant::now());
        let mapping = ProcessMapping {
            process: exe_name.to_string(),
            r#type: resolved.game_type.clone(),
            froglog_id: resolved.id,
            title: Some(resolved.title.clone()),
            title_filter: None,
        };
        on_already_owned_game_needs_link(mapping.clone());
        return Some(mapping);
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
        Arc::clone(currently_tracking),
        Arc::clone(last_ended_unmapped),
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

/// Active session: we're currently tracking this process.
#[derive(Debug, Clone)]
pub struct ActiveSession {
    #[allow(dead_code)]
    pub process_name: String,
    #[allow(dead_code)]
    pub mapping: ProcessMapping,
    pub started_at: Instant,
}

/// Block until process exits (using sysinfo Process::wait), then send session-ended data on the channel.
fn run_wait_thread(
    pid: Pid,
    process_name: String,
    mapping: ProcessMapping,
    started_at: Instant,
    sender: mpsc::Sender<(String, ProcessMapping, f64)>,
) {
    std::thread::spawn(move || {
        let mut system = System::new_all();
        let wait_start = Instant::now();
        const DISCOVER_TIMEOUT: Duration = Duration::from_secs(5);

        loop {
            system.refresh_processes(ProcessesToUpdate::All);
            if let Some(process) = system.process(pid) {
                process.wait();
                let duration_secs = started_at.elapsed().as_secs_f64();
                let _ = sender.send((process_name, mapping, duration_secs));
                break;
            }
            if wait_start.elapsed() > DISCOVER_TIMEOUT {
                let duration_secs = started_at.elapsed().as_secs_f64();
                let _ = sender.send((process_name, mapping, duration_secs));
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

/// Attempt to start a WMI-based process start monitor.
/// Returns true if WMI started successfully, false if it should fall back to polling.
#[allow(clippy::too_many_arguments)]
#[cfg(windows)]
fn try_run_wmi_watch(
    config: Arc<RwLock<ProcessMapConfig>>,
    current_session: Arc<RwLock<Option<ActiveSession>>>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::Sender<(String, ProcessMapping, f64)>,
    on_session_started: Arc<dyn Fn(String, ProcessMapping) + Send + Sync + 'static>,
    last_ended: Arc<RwLock<Option<(String, Instant)>>>,
    force_stopped_process: Arc<RwLock<Option<String>>>,
    installed_games: Arc<RwLock<Vec<InstalledGame>>>,
    library_index: Arc<RwLock<LibraryIndex>>,
    currently_tracking_unmapped: Arc<RwLock<HashSet<String>>>,
    last_ended_unmapped: Arc<RwLock<HashMap<String, Instant>>>,
    on_unmapped_session_ended: Arc<dyn Fn(String, String, String, f64) + Send + Sync + 'static>,
    on_already_owned_game_needs_link: Arc<dyn Fn(ProcessMapping) + Send + Sync + 'static>,
) -> bool {
    use serde::Deserialize;
    use wmi::{COMLibrary, WMIConnection};

    // COM objects are not Send — create everything inside the thread.
    // Use a channel to signal whether init succeeded so the caller can fall back to polling.
    let (init_tx, init_rx) = mpsc::channel::<bool>();

    std::thread::spawn(move || {
        #[derive(Deserialize, Debug)]
        #[serde(rename = "Win32_ProcessStartTrace")]
        struct ProcessStartTrace {
            #[serde(rename = "ProcessName")]
            process_name: String,
            #[serde(rename = "ProcessId")]
            process_id: u32,
        }

        let com_lib = match COMLibrary::new() {
            Ok(lib) => lib,
            Err(_) => { let _ = init_tx.send(false); return; }
        };
        let wmi_con = match WMIConnection::new(com_lib) {
            Ok(con) => con,
            Err(_) => { let _ = init_tx.send(false); return; }
        };
        let iter = match wmi_con.notification::<ProcessStartTrace>() {
            Ok(iter) => iter,
            Err(_) => { let _ = init_tx.send(false); return; }
        };

        let _ = init_tx.send(true);

        for result in iter {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            let event = match result {
                Ok(e) => e,
                Err(_) => continue,
            };

            // Collect all candidate mappings for this exe (may be multiple for javaw.exe etc.)
            let (candidates, unmapped_detection_disabled): (Vec<ProcessMapping>, bool) = {
                let cfg = config.read().unwrap();
                (
                    cfg.find_all_by_process(&event.process_name).into_iter().cloned().collect(),
                    cfg.disable_unmapped_game_detection,
                )
            };

            if candidates.is_empty() {
                if unmapped_detection_disabled {
                    continue;
                }
                let pid = Pid::from(event.process_id as usize);
                let exe_path = {
                    let mut sys = System::new();
                    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]));
                    sys.process(pid).and_then(|p| p.exe().map(|p| p.to_path_buf()))
                };
                if let Some(mapping) = maybe_start_unmapped_tracking(
                    exe_path.as_deref(),
                    // WMI process-start events are Windows-only, where Proton/Wine doesn't
                    // exist -- no command-line fallback is needed here.
                    &[],
                    pid,
                    &installed_games,
                    &library_index,
                    &currently_tracking_unmapped,
                    &last_ended_unmapped,
                    &on_unmapped_session_ended,
                    &on_already_owned_game_needs_link,
                ) {
                    // Already-owned game just got auto-linked — start tracking this exact
                    // launch now rather than waiting for a later one, since this WMI process-
                    // start event won't fire again for it.
                    let mut cur = current_session.write().unwrap();
                    if cur.is_none() {
                        let started_at = Instant::now();
                        *cur = Some(ActiveSession {
                            process_name: event.process_name.clone(),
                            mapping: mapping.clone(),
                            started_at,
                        });
                        drop(cur);
                        on_session_started(event.process_name.clone(), mapping.clone());
                        run_wait_thread(pid, event.process_name.clone(), mapping, started_at, tx.clone());
                    }
                }
                continue;
            }

            let needs_title_check = candidates.iter().any(|m| m.title_filter.is_some());

            if !needs_title_check {
                // No title filter — start immediately (existing behaviour).
                // Skip if this process just ended (brief launcher re-spawn cooldown).
                {
                    let le = last_ended.read().unwrap();
                    if let Some((ref last_proc, last_time)) = *le {
                        if last_proc.eq_ignore_ascii_case(&event.process_name)
                            && last_time.elapsed() < POST_SESSION_COOLDOWN
                        {
                            continue;
                        }
                    }
                }
                // Skip if user force-stopped this process (still running)
                {
                    let fs = force_stopped_process.read().unwrap();
                    if let Some(ref stopped) = *fs {
                        if stopped.eq_ignore_ascii_case(&event.process_name) {
                            continue;
                        }
                    }
                }
                let mut cur = current_session.write().unwrap();
                if cur.is_some() {
                    continue;
                }
                let mapping = candidates.into_iter().next().unwrap();
                let pid = Pid::from(event.process_id as usize);
                let started_at = Instant::now();
                *cur = Some(ActiveSession {
                    process_name: event.process_name.clone(),
                    mapping: mapping.clone(),
                    started_at,
                });
                drop(cur);
                on_session_started(event.process_name.clone(), mapping.clone());
                run_wait_thread(pid, event.process_name, mapping, started_at, tx.clone());
            } else {
                // At least one mapping has a title filter — poll for the window to appear.
                let current_session = Arc::clone(&current_session);
                let tx = tx.clone();
                let on_session_started = Arc::clone(&on_session_started);
                let process_name = event.process_name.clone();
                let process_id = event.process_id;
                let last_ended_clone = Arc::clone(&last_ended);
                let force_stopped_clone = Arc::clone(&force_stopped_process);

                std::thread::spawn(move || {
                    let pid = Pid::from(process_id as usize);
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while Instant::now() < deadline {
                        if current_session.read().unwrap().is_some() {
                            return; // Another session started meanwhile.
                        }
                        // Skip if user force-stopped this process
                        {
                            let fs = force_stopped_clone.read().unwrap();
                            if let Some(ref stopped) = *fs {
                                if stopped.eq_ignore_ascii_case(&process_name) {
                                    return;
                                }
                            }
                        }
                        // Respect cooldown from a recent session end for this process.
                        {
                            let le = last_ended_clone.read().unwrap();
                            if let Some((ref last_proc, last_time)) = *le {
                                if last_proc.eq_ignore_ascii_case(&process_name)
                                    && last_time.elapsed() < POST_SESSION_COOLDOWN
                                {
                                    std::thread::sleep(Duration::from_millis(500));
                                    continue;
                                }
                            }
                        }
                        let window_titles = get_window_titles_for_pid(process_id);
                        if let Some(mapping) = pick_mapping(&candidates, &window_titles) {
                            let mut cur = current_session.write().unwrap();
                            if cur.is_some() {
                                return;
                            }
                            let started_at = Instant::now();
                            *cur = Some(ActiveSession {
                                process_name: process_name.clone(),
                                mapping: mapping.clone(),
                                started_at,
                            });
                            drop(cur);
                            on_session_started(process_name.clone(), mapping.clone());
                            run_wait_thread(pid, process_name, mapping, started_at, tx);
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(500));
                    }
                });
            }
        }
    });

    // Wait up to 5 seconds for the thread to confirm WMI initialised successfully.
    matches!(init_rx.recv_timeout(Duration::from_secs(5)), Ok(true))
}

/// Run the monitor. On Windows, tries WMI event-based start detection first;
/// falls back to polling if WMI is unavailable. Exit detection always uses process.wait().
///
/// `installed_games` and `library_index` are refreshed by the caller (Steam library scan +
/// FrogLog games/wishlist fetch, respectively) and read live here, the same way `config` is.
/// `on_unmapped_session_ended(title, appid, exe_name, duration_secs)` fires once per full play
/// session (launch to close) of a process that doesn't match any `ProcessMapping` but does
/// match an installed Steam game that isn't already in the user's FrogLog library — the whole
/// session's duration is tracked in the background the same way a mapped session would be, so
/// the caller finds out (and can notify) only once the game has actually closed.
/// `on_already_owned_game_needs_link(mapping)` fires instead when the installed game turns out
/// to already be in the library (by exact Steam appid) but just has no `ProcessMapping` yet —
/// the caller should persist it. The *current* launch is tracked immediately using that same
/// mapping (via the normal `on_session_started`/`on_session_ended` callbacks) rather than
/// waiting for a later one, since e.g. a WMI process-start event never refires on its own.
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
    on_unmapped_session_ended: impl Fn(String, String, String, f64) + Send + Sync + 'static,
    on_already_owned_game_needs_link: impl Fn(ProcessMapping) + Send + Sync + 'static,
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

    // Wrap in Arc so it can be shared between WMI path and polling fallback.
    let on_started: Arc<dyn Fn(String, ProcessMapping) + Send + Sync + 'static> =
        Arc::new(on_session_started);
    let on_unmapped_ended: Arc<dyn Fn(String, String, String, f64) + Send + Sync + 'static> =
        Arc::new(on_unmapped_session_ended);
    let on_already_owned: Arc<dyn Fn(ProcessMapping) + Send + Sync + 'static> =
        Arc::new(on_already_owned_game_needs_link);

    #[cfg(windows)]
    let wmi_ok = try_run_wmi_watch(
        Arc::clone(&config),
        Arc::clone(&current_session),
        Arc::clone(&shutdown),
        tx.clone(),
        Arc::clone(&on_started),
        Arc::clone(&last_ended),
        Arc::clone(&force_stopped_process),
        Arc::clone(&installed_games),
        Arc::clone(&library_index),
        Arc::clone(&currently_tracking_unmapped),
        Arc::clone(&last_ended_unmapped),
        Arc::clone(&on_unmapped_ended),
        Arc::clone(&on_already_owned),
    );

    #[cfg(not(windows))]
    let wmi_ok = false;

    // Fall back to polling if WMI is unavailable.
    if !wmi_ok {
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
        let on_unmapped_ended_poll = Arc::clone(&on_unmapped_ended);
        let on_already_owned_poll = Arc::clone(&on_already_owned);

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
                    if cur.is_some() {
                        log::debug!(
                            "[LilyPad] poll tick skipped: current_session already occupied by {:?}",
                            cur.as_ref().map(|s| (&s.process_name, &s.mapping.title, s.started_at.elapsed()))
                        );
                        None
                    } else {
                        system.refresh_processes(ProcessesToUpdate::All);
                        let mut found = None;
                        'proc_scan: for (pid, p) in system.processes().iter() {
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
                                    let matches: Vec<ProcessMapping> =
                                        cfg.find_all_by_process(candidate).into_iter().cloned().collect();
                                    if !matches.is_empty() {
                                        result = Some((candidate.clone(), matches));
                                        break;
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
                                        &currently_tracking_unmapped_poll,
                                        &last_ended_unmapped_poll,
                                        &on_unmapped_ended_poll,
                                        &on_already_owned_poll,
                                    ) {
                                        // Already-owned game just got auto-linked — track this
                                        // launch now via the normal path below instead of
                                        // waiting for the next poll tick to notice the mapping.
                                        *cur = Some(ActiveSession {
                                            process_name: name.clone(),
                                            mapping: mapping.clone(),
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
                                *cur = Some(ActiveSession {
                                    process_name: name.clone(),
                                    mapping: mapping.clone(),
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
                *current_session.write().unwrap() = None;
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
    fn filters_known_helper_processes() {
        assert!(is_known_helper_process("UnityCrashHandler64.exe"));
        assert!(is_known_helper_process("unitycrashhandler64.exe"));
        assert!(is_known_helper_process("EasyAntiCheat_Launcher.exe"));
        assert!(!is_known_helper_process("Among Us.exe"));
        assert!(!is_known_helper_process("Celeste.exe"));
    }
}
