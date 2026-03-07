//! Process monitor: WMI event-based start detection (Windows), poll-based fallback.

use crate::config::{ProcessMapConfig, ProcessMapping};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
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

#[cfg(not(windows))]
pub fn get_window_titles_for_pid(_target_pid: u32) -> Vec<String> {
    vec![]
}

/// Given candidates (all mappings for an exe) and current window titles, return the best match.
/// Prefers title_filter matches; falls back to any mapping without a filter.
fn pick_mapping(candidates: &[ProcessMapping], window_titles: &[String]) -> Option<ProcessMapping> {
    for m in candidates {
        if let Some(filter) = &m.title_filter {
            let filter_lower = filter.to_lowercase();
            if window_titles.iter().any(|t| t.to_lowercase().contains(&filter_lower)) {
                return Some(m.clone());
            }
        }
    }
    // Fall back to the first mapping with no title filter
    candidates.iter().find(|m| m.title_filter.is_none()).cloned()
}

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
#[cfg(windows)]
fn try_run_wmi_watch(
    config: Arc<std::sync::RwLock<ProcessMapConfig>>,
    current_session: Arc<std::sync::RwLock<Option<ActiveSession>>>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::Sender<(String, ProcessMapping, f64)>,
    on_session_started: Arc<dyn Fn(String, ProcessMapping) + Send + Sync + 'static>,
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

                std::thread::spawn(move || {
                    let pid = Pid::from(process_id as usize);
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while Instant::now() < deadline {
                        if current_session.read().unwrap().is_some() {
                            return; // Another session started meanwhile.
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
    config: Arc<std::sync::RwLock<ProcessMapConfig>>,
    current_session: Arc<std::sync::RwLock<Option<ActiveSession>>>,
    shutdown: Arc<AtomicBool>,
    poll_interval_secs: u64,
    on_session_started: impl Fn(String, ProcessMapping) + Send + Sync + 'static,
    mut on_session_ended: impl FnMut(String, ProcessMapping, f64) + Send + 'static,
) {
    let (tx, rx) = mpsc::channel::<(String, ProcessMapping, f64)>();

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
    );

    #[cfg(not(windows))]
    let wmi_ok = false;

    // Fall back to polling if WMI is unavailable.
    if !wmi_ok {
        let config = Arc::clone(&config);
        let current_session = Arc::clone(&current_session);
        let shutdown = Arc::clone(&shutdown);
        let tx = tx.clone();

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
