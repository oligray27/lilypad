//! Process monitor: WMI event-based start detection (Windows), poll-based fallback.

use crate::config::{ProcessMapConfig, ProcessMapping};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessesToUpdate, System};

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

/// Linux X11 implementation: walks the window tree via _NET_WM_PID and returns window titles.
/// Returns an empty vec if not running under X11 (pure Wayland, headless, etc.).
#[cfg(target_os = "linux")]
pub fn get_window_titles_for_pid(target_pid: u32) -> Vec<String> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, GetPropertyReply};
    use x11rb::rust_connection::RustConnection;

    fn get_u32_prop(conn: &RustConnection, window: u32, atom: u32) -> Option<u32> {
        let reply: GetPropertyReply = conn
            .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        if reply.format == 32 && reply.value.len() == 4 {
            Some(u32::from_ne_bytes(reply.value[..4].try_into().ok()?))
        } else {
            None
        }
    }

    fn get_string_prop(conn: &RustConnection, window: u32, atom: u32, type_atom: u32) -> Option<String> {
        let reply = conn
            .get_property(false, window, atom, type_atom, 0, u32::MAX)
            .ok()?
            .reply()
            .ok()?;
        if reply.value.is_empty() { return None; }
        String::from_utf8(reply.value).ok()
    }

    fn collect_titles(
        conn: &RustConnection,
        window: u32,
        target_pid: u32,
        net_wm_pid: u32,
        net_wm_name: u32,
        utf8_string: u32,
        titles: &mut Vec<String>,
    ) {
        if let Some(pid) = get_u32_prop(conn, window, net_wm_pid) {
            if pid == target_pid {
                let title = get_string_prop(conn, window, net_wm_name, utf8_string)
                    .or_else(|| get_string_prop(conn, window, u32::from(AtomEnum::WM_NAME), u32::from(AtomEnum::STRING)));
                if let Some(t) = title {
                    let t = t.trim().to_owned();
                    if !t.is_empty() && !titles.contains(&t) {
                        titles.push(t);
                    }
                }
            }
        }
        if let Some(reply) = conn.query_tree(window).ok().and_then(|c| c.reply().ok()) {
            for child in reply.children {
                collect_titles(conn, child, target_pid, net_wm_pid, net_wm_name, utf8_string, titles);
            }
        }
    }

    // No DISPLAY set → pure Wayland or headless; return empty without attempting a connection.
    if std::env::var_os("DISPLAY").is_none() {
        return vec![];
    }

    let (conn, screen_num) = match x11rb::connect(None) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    let intern = |name: &str| -> Option<u32> {
        conn.intern_atom(false, name.as_bytes()).ok()?.reply().ok().map(|r| r.atom)
    };

    let net_wm_pid  = match intern("_NET_WM_PID")  { Some(a) => a, None => return vec![] };
    let net_wm_name = match intern("_NET_WM_NAME") { Some(a) => a, None => return vec![] };
    let utf8_string = match intern("UTF8_STRING")  { Some(a) => a, None => return vec![] };

    let mut titles = Vec::new();
    collect_titles(&conn, root, target_pid, net_wm_pid, net_wm_name, utf8_string, &mut titles);
    titles
}

/// macOS and any other non-Windows, non-Linux platform: window title detection not implemented.
#[cfg(not(any(windows, target_os = "linux")))]
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
    pub process_name: String,
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
#[cfg(windows)]
fn try_run_wmi_watch(
    config: Arc<RwLock<ProcessMapConfig>>,
    current_session: Arc<RwLock<Option<ActiveSession>>>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::Sender<(String, ProcessMapping, f64)>,
    on_session_started: Arc<dyn Fn(String, ProcessMapping) + Send + Sync + 'static>,
    last_ended: Arc<RwLock<Option<(String, Instant)>>>,
    force_stopped_process: Arc<RwLock<Option<String>>>,
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
            let candidates: Vec<ProcessMapping> = {
                let cfg = config.read().unwrap();
                cfg.find_all_by_process(&event.process_name)
                    .into_iter()
                    .cloned()
                    .collect()
            };

            if candidates.is_empty() {
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
pub fn run_poll_loop(
    config: Arc<RwLock<ProcessMapConfig>>,
    current_session: Arc<RwLock<Option<ActiveSession>>>,
    shutdown: Arc<AtomicBool>,
    force_stopped_process: Arc<RwLock<Option<String>>>,
    poll_interval_secs: u64,
    on_session_started: impl Fn(String, ProcessMapping) + Send + Sync + 'static,
    mut on_session_ended: impl FnMut(String, ProcessMapping, f64) + Send + 'static,
) {
    let (tx, rx) = mpsc::channel::<(String, ProcessMapping, f64)>();

    // Tracks when the last session ended per process name, to suppress phantom re-launches.
    let last_ended: Arc<RwLock<Option<(String, Instant)>>> =
        Arc::new(RwLock::new(None));

    // Wrap in Arc so it can be shared between WMI path and polling fallback.
    let on_started: Arc<dyn Fn(String, ProcessMapping) + Send + Sync + 'static> =
        Arc::new(on_session_started);

    #[cfg(windows)]
    let wmi_ok = try_run_wmi_watch(
        Arc::clone(&config),
        Arc::clone(&current_session),
        Arc::clone(&shutdown),
        tx.clone(),
        Arc::clone(&on_started),
        Arc::clone(&last_ended),
        Arc::clone(&force_stopped_process),
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

        std::thread::spawn(move || {
            let mut system = System::new_all();
            while !shutdown.load(Ordering::SeqCst) {
                let session_started = {
                    let cfg = config.read().unwrap();
                    let mut cur = current_session.write().unwrap();
                    if cur.is_some() {
                        None
                    } else {
                        system.refresh_processes(ProcessesToUpdate::All);
                        let mut found = None;
                        'proc_scan: for (pid, p) in system.processes().iter() {
                            let name = p
                                .exe()
                                .and_then(|path| {
                                    path.file_name().and_then(|n| n.to_str().map(String::from))
                                })
                                .unwrap_or_else(|| p.name().to_string_lossy().into_owned());
                            let candidates: Vec<ProcessMapping> = cfg
                                .find_all_by_process(&name)
                                .into_iter()
                                .cloned()
                                .collect();
                            if candidates.is_empty() {
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
