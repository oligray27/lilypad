use lilypad_core::{api, config, library_match, local_games, monitor, steam};

use api::FroglogClient;
use config::{AuthConfig, ExcludedApp, ProcessMapConfig, ProcessMapping, PendingSession, WatchedDirectory, auth_config_path, process_map_path_for_auth, load_pending_sessions, save_pending_sessions};
use library_match::LibraryIndex;
use monitor::{run_poll_loop, ActiveSession};
use steam::InstalledGame;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime};
use sysinfo::{ProcessesToUpdate, System};
use tauri::menu::{IsMenuItem, Menu, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, WindowEvent};
use tauri::Wry;
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

/// Tray icon (embedded at compile time from icons/icon.ico).
const TRAY_ICON: tauri::image::Image<'_> = tauri::include_image!("icons/icon.ico");
/// Tray icon shown while a game session is active.
const TRAY_ICON_NOWPLAYING: tauri::image::Image<'_> = tauri::include_image!("icons/icon_nowplaying.ico");

const DEFAULT_HEIGHT: f64 = 760.0;
const MAIN_ABOUT_HEIGHT: f64 = 335.0;
const WINDOW_WIDTH: f64 = 642.0;
const SESSION_WIDTH: f64 = 440.0;
const SESSION_HEIGHT_REGULAR: f64 = 155.0;
const SESSION_HEIGHT_LIVE: f64 = 284.0;
const SESSION_TASKBAR_REGULAR: f64 = 94.0;
const SESSION_TASKBAR_LIVE: f64 = 94.0;

/// GTK/Wayland can map a window with stale title-bar hit-test regions when its size/position
/// is set before it's shown, leaving the CSD min/max/close buttons unresponsive until something
/// (e.g. a double-click) forces a relayout. Showing first, then re-requesting focus shortly
/// after the window is fully mapped, gives GTK a second chance to settle. No-op on other platforms.
#[cfg(target_os = "linux")]
fn nudge_focus(w: &tauri::WebviewWindow) {
    let handle = w.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let handle2 = handle.clone();
        let _ = handle.app_handle().run_on_main_thread(move || {
            let _ = handle2.set_focus();
        });
    });
}

#[cfg(not(target_os = "linux"))]
fn nudge_focus(_w: &tauri::WebviewWindow) {}

fn show_window_at_height(w: &tauri::WebviewWindow, width: f64, height: f64) {
    let _ = w.show();
    let _ = w.set_size(tauri::Size::Logical(tauri::LogicalSize { width, height }));
    let _ = w.center();
    let _ = w.set_focus();
    nudge_focus(w);
    apply_theme(w);
}

fn show_session_window(w: &tauri::WebviewWindow, height: f64, taskbar_margin: f64) {
    let _ = w.show();
    let _ = w.set_size(tauri::Size::Logical(tauri::LogicalSize { width: SESSION_WIDTH, height }));
    if let Ok(Some(monitor)) = w.primary_monitor() {
        let scale = monitor.scale_factor();
        let mon_size = monitor.size();
        let mon_pos = monitor.position();
        let win_w = (SESSION_WIDTH * scale) as i32;
        let win_h = (height * scale) as i32;
        let margin = (12.0 * scale) as i32;
        let tb = (taskbar_margin * scale) as i32;
        let x = mon_pos.x + mon_size.width as i32 - win_w - margin;
        let y = mon_pos.y + mon_size.height as i32 - win_h - tb;
        let _ = w.set_position(tauri::Position::Physical(tauri::PhysicalPosition { x, y }));
    }
    let _ = w.set_focus();
    nudge_focus(w);
    apply_theme(w);
}

/// Reads the desktop's dark-mode preference from the XDG Desktop Portal Settings API
/// (`org.freedesktop.appearance` / `color-scheme`, 1 = prefer-dark). GTK3 (and therefore
/// WebKitGTK, which Tauri uses on Linux) only follows the legacy `gtk-theme` name, not
/// GNOME's `color-scheme` gsetting, so `WebviewWindow::theme()` reports the wrong thing on
/// stock GNOME. The portal is the desktop-agnostic source of truth (also used by Firefox/Chromium).
#[cfg(target_os = "linux")]
fn linux_prefers_dark() -> Option<bool> {
    let conn = zbus::blocking::Connection::session().ok()?;
    let reply = conn
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Settings"),
            "Read",
            &("org.freedesktop.appearance", "color-scheme"),
        )
        .ok()?;
    // Read() returns `v` wrapping the setting's own value, which for this key is itself
    // a variant-wrapped u32 — so we may need to unwrap one or two layers of Value::Value.
    let body = reply.body();
    let mut value: zbus::zvariant::Value = body.deserialize().ok()?;
    loop {
        match value {
            zbus::zvariant::Value::U32(n) => return Some(n == 1),
            zbus::zvariant::Value::Value(inner) => value = *inner,
            _ => return None,
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn linux_prefers_dark() -> Option<bool> {
    None
}

/// Reflects the OS/window theme into the DOM as `<html data-theme="dark|light">`.
/// WebKitGTK (Linux) doesn't reliably surface the desktop's dark-mode preference via
/// `prefers-color-scheme` the way WebView2 (Windows) does, so main.css keys off this
/// attribute instead of relying solely on the media query.
fn apply_theme(w: &tauri::WebviewWindow) {
    let is_dark = linux_prefers_dark()
        .unwrap_or_else(|| w.theme().unwrap_or(tauri::Theme::Light) == tauri::Theme::Dark);
    let theme_str = if is_dark { "dark" } else { "light" };
    let _ = w.eval(&format!(
        "document.documentElement.setAttribute('data-theme', '{}')",
        theme_str
    ));
}

/// Shared state for the app.
struct AppState {
    /// Shared with process monitor so mappings are visible without restart.
    process_map_arc: Arc<RwLock<ProcessMapConfig>>,
    current_session_arc: Arc<RwLock<Option<ActiveSession>>>,
    auth: RwLock<AuthConfig>,
    /// Set to a process name when the user force-stops tracking. Clears when the process actually exits.
    /// Prevents the monitor from immediately re-starting a session for the still-running process.
    force_stopped_process: Arc<RwLock<Option<String>>>,
    /// Used to cancel a live/session auto-submit countdown (user clicked "Add Notes").
    auto_submit_cancel_tx: Arc<RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
    /// Holds session data for a live/session auto-submit that is still in the 25-second intercept window.
    auto_submit_pending_data: Arc<RwLock<Option<serde_json::Value>>>,
    /// Installed games — Steam (manifest scan) + non-Steam (watched directories) — refreshed
    /// periodically in the background.
    installed_games_arc: Arc<RwLock<Vec<InstalledGame>>>,
    /// Titles/appids already in the user's FrogLog library, refreshed periodically in the background.
    library_index_arc: Arc<RwLock<LibraryIndex>>,
}

fn api_client(auth: &AuthConfig) -> Option<FroglogClient> {
    let base = auth
        .base_url
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("https://api.froglog.co.uk/api");
    let mut c = FroglogClient::new(base.to_string());
    c.set_token(auth.token.clone());
    Some(c)
}

/// Re-scans Steam's installed games and every configured watched directory, replacing
/// `state.installed_games_arc` in one go. Called both by the periodic background refresh and
/// immediately after adding/removing a watched directory, so a newly added folder is picked up
/// right away instead of waiting for the next scheduled scan.
fn refresh_installed_games(state: &AppState) {
    let mut games = steam::find_steam_root()
        .map(|root| steam::scan_installed_games(&root))
        .unwrap_or_default();
    let (watched_dirs, excluded_appids): (Vec<String>, std::collections::HashSet<String>) = {
        let cfg = state.process_map_arc.read().unwrap();
        (
            cfg.watched_directories.iter().map(|w| w.path.clone()).collect(),
            cfg.excluded_apps.iter().map(|e| e.appid.clone()).collect(),
        )
    };
    games.extend(local_games::scan_watched_directories(&watched_dirs));
    // User-excluded apps (see `config::ExcludedApp`) -- e.g. Wallpaper Engine, which manifests
    // exactly like a real Steam game but obviously isn't one. Filtered here rather than in
    // `scan_installed_games` itself so the scan stays a pure "what does Steam say is installed"
    // function; this is where per-user preference gets applied on top of it.
    games.retain(|g| !excluded_appids.contains(&g.appid));
    *state.installed_games_arc.write().unwrap() = games;
}

/// Re-fetches games/wishlist/live-service from FrogLog and rebuilds `state.library_index_arc`.
/// Called both by the periodic background refresh and on-demand (see the
/// `refresh_library_index` command) whenever the Configure view opens, since it already fetches
/// this same data for its own display and the shared index otherwise sits stale for up to 5
/// minutes after a change made directly on the FrogLog website.
fn refresh_library_index_state(state: &AppState) {
    let auth = state.auth.read().unwrap().clone();
    let Some(client) = api_client(&auth) else { return };
    let games = client.get_games().unwrap_or_default();
    let wishlist = client.get_wishlist().unwrap_or_default();
    let live_service = client.get_live_service_games().unwrap_or_default();
    *state.library_index_arc.write().unwrap() = LibraryIndex::build(&games, &wishlist, &live_service);
}

/// Persisted to disk when a session starts; cleared when it ends normally.
/// Survives LilyPad crashes so sessions can be recovered on next launch.
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedSession {
    process: String,
    game_type: String,
    froglog_id: i32,
    title: Option<String>,
    started_at_secs: u64,
}

/// Send a Windows toast notification with an "Add Notes" action button for live/session auto-submit.
/// Clicking the action button cancels the auto-submit and opens the session popup.
/// When the toast is dismissed/expires, submit_tx fires to trigger auto-submit.
#[cfg(windows)]
fn show_auto_submit_toast(
    title_str: &str,
    time_str: &str,
    cancel_tx_arc: Arc<RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
    pending_data_arc: Arc<RwLock<Option<serde_json::Value>>>,
    submit_tx: tokio::sync::oneshot::Sender<()>,
    app: tauri::AppHandle,
) {
    use windows::core::HSTRING;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastDismissedEventArgs, ToastNotification, ToastNotificationManager};

    let title_owned = title_str.to_string();
    let time_owned = time_str.to_string();
    let submit_tx_arc = Arc::new(std::sync::Mutex::new(Some(submit_tx)));
    std::thread::spawn(move || {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            );
        }
        let xml = format!(
            concat!(
                r#"<toast><visual><binding template="ToastGeneric">"#,
                r#"<text>Session Auto-Submitting</text>"#,
                r#"<text>{} ({})</text>"#,
                r#"</binding></visual>"#,
                r#"<actions>"#,
                r#"<action content="Add Notes" arguments="add-notes"/>"#,
                r#"</actions></toast>"#,
            ),
            title_owned, time_owned
        );
        let doc = match XmlDocument::new() {
            Ok(d) => d,
            Err(_) => return,
        };
        if doc.LoadXml(&HSTRING::from(xml.as_str())).is_err() { return; }
        let toast = match ToastNotification::CreateToastNotification(&doc) {
            Ok(t) => t,
            Err(_) => return,
        };
        let submit_tx_arc2 = Arc::clone(&submit_tx_arc);
        let _ = toast.Dismissed(&TypedEventHandler::<ToastNotification, ToastDismissedEventArgs>::new(
            move |_, _| {
                if let Some(tx) = submit_tx_arc2.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                Ok(())
            },
        ));
        let _ = toast.Activated(&TypedEventHandler::<ToastNotification, windows::core::IInspectable>::new(
            move |_, _| {
                // Drop submit_tx so the submit_rx branch never fires.
                submit_tx_arc.lock().unwrap().take();
                if let Some(tx) = cancel_tx_arc.write().unwrap().take() {
                    let _ = tx.send(());
                }
                if let Some(session_data) = pending_data_arc.write().unwrap().take() {
                    let app2 = app.clone();
                    let _ = app.run_on_main_thread(move || {
                        if let Some(w) = app2.get_webview_window("main") {
                            show_session_window(&w, SESSION_HEIGHT_LIVE, SESSION_TASKBAR_LIVE);
                        }
                        let _ = app2.emit("session-ended", session_data);
                    });
                }
                Ok(())
            },
        ));
        let aumid = HSTRING::from("uk.co.froglog.lilypad");
        if let Ok(notifier) = ToastNotificationManager::CreateToastNotifierWithId(&aumid) {
            let _ = notifier.Show(&toast);
        }
        // Keep toast alive so WinRT holds event handlers until dismissed.
        std::thread::sleep(std::time::Duration::from_secs(30));
    });
}

/// Send a Windows toast with a "View New Games" action button for a completed session of a
/// game that isn't in the FrogLog library. Clicking the button (or the toast body) opens the
/// main window on the New Games view, where the session can be resolved into a real entry.
#[cfg(windows)]
fn show_pending_game_toast(body: &str, app: tauri::AppHandle) {
    use windows::core::HSTRING;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

    let body_owned = body.to_string();
    std::thread::spawn(move || {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            );
        }
        let xml = format!(
            concat!(
                r#"<toast><visual><binding template="ToastGeneric">"#,
                r#"<text>Session Recorded</text>"#,
                r#"<text>{}</text>"#,
                r#"</binding></visual>"#,
                r#"<actions>"#,
                r#"<action content="Go to New Games" arguments="view-new-games"/>"#,
                r#"</actions></toast>"#,
            ),
            body_owned
        );
        let doc = match XmlDocument::new() {
            Ok(d) => d,
            Err(_) => return,
        };
        if doc.LoadXml(&HSTRING::from(xml.as_str())).is_err() { return; }
        let toast = match ToastNotification::CreateToastNotification(&doc) {
            Ok(t) => t,
            Err(_) => return,
        };
        let _ = toast.Activated(&TypedEventHandler::<ToastNotification, windows::core::IInspectable>::new(
            move |_, _| {
                let app2 = app.clone();
                let _ = app.run_on_main_thread(move || {
                    if let Some(w) = app2.get_webview_window("main") {
                        show_window_at_height(&w, WINDOW_WIDTH, 500.0);
                        let _ = w.emit("show-new-games", ());
                    }
                });
                Ok(())
            },
        ));
        let aumid = HSTRING::from("uk.co.froglog.lilypad");
        if let Ok(notifier) = ToastNotificationManager::CreateToastNotifierWithId(&aumid) {
            let _ = notifier.Show(&toast);
        }
        // Keep toast alive so WinRT holds event handlers until dismissed.
        std::thread::sleep(std::time::Duration::from_secs(30));
    });
}

#[cfg(not(windows))]
fn show_pending_game_toast(body: &str, app: tauri::AppHandle) {
    let _ = app.notification().builder()
        .title("Session Recorded")
        .body(body)
        .show();
}

#[cfg(not(windows))]
fn show_auto_submit_toast(
    title_str: &str,
    time_str: &str,
    _cancel_tx_arc: Arc<RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
    _pending_data_arc: Arc<RwLock<Option<serde_json::Value>>>,
    submit_tx: tokio::sync::oneshot::Sender<()>,
    app: tauri::AppHandle,
) {
    let _ = app.notification().builder()
        .title("Session Auto-Submitting")
        .body(format!("{} ({}) — open LilyPad tray to add notes", title_str, time_str))
        .show();
    // No action button available on non-Windows — submit immediately.
    let _ = submit_tx.send(());
}

/// Shared session-ended handler used by both the normal monitor path and crash recovery.
fn handle_session_ended(
    app: tauri::AppHandle,
    process_name: String,
    mapping: config::ProcessMapping,
    duration_secs: f64,
) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let auto_submit = {
            let cfg = state.process_map_arc.read().unwrap();
            if mapping.r#type.eq_ignore_ascii_case("live") {
                cfg.auto_submit_live
            } else if mapping.r#type.eq_ignore_ascii_case("session") {
                cfg.auto_submit_session
            } else {
                cfg.auto_submit_regular
            }
        };
        let auth = state.auth.read().unwrap().clone();
        drop(state);

        // Always clear now_playing when a session ends, regardless of auto_submit setting
        let auth_clear = auth.clone();
        std::thread::spawn(move || {
            if let Some(client) = api_client(&auth_clear) {
                log::info!("[LilyPad] handle_session_ended: clearing now_playing");
                let _ = client.clear_now_playing();
            } else {
                log::warn!("[LilyPad] handle_session_ended: no API client available to clear now_playing");
            }
        });

        if auto_submit {
            let raw_hours = duration_secs / 3600.0;
            let hours = { let r = (raw_hours * 100.0).round() / 100.0; if r < 0.01 { 0.01 } else { r } };
            let is_notes_type = mapping.r#type.eq_ignore_ascii_case("live") || mapping.r#type.eq_ignore_ascii_case("session");

            // Live/session: show toast with "Add Notes" button; auto-submit when toast is dismissed.
            if is_notes_type {
                let title_str = mapping.title.as_deref().unwrap_or_else(|| mapping.process.as_str()).to_string();
                let total_mins = (duration_secs / 60.0).round() as u64;
                let time_str = if total_mins >= 60 { format!("{}h {}m", total_mins / 60, total_mins % 60) } else { format!("{}m", total_mins) };
                let session_data = serde_json::json!({
                    "processName": process_name,
                    "mapping": {
                        "process": mapping.process,
                        "type": mapping.r#type,
                        "froglogId": mapping.froglog_id,
                        "title": mapping.title,
                    },
                    "durationSecs": duration_secs,
                });
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
                let (submit_tx, submit_rx) = tokio::sync::oneshot::channel::<()>();
                let (cancel_tx_arc, pending_data_arc) = {
                    let state = app.state::<AppState>();
                    *state.auto_submit_cancel_tx.write().unwrap() = Some(cancel_tx);
                    *state.auto_submit_pending_data.write().unwrap() = Some(session_data);
                    (Arc::clone(&state.auto_submit_cancel_tx), Arc::clone(&state.auto_submit_pending_data))
                };
                show_auto_submit_toast(&title_str, &time_str, cancel_tx_arc, pending_data_arc, submit_tx, app.clone());
                tokio::select! {
                    _ = cancel_rx => {
                        // User clicked "Add Notes" — intercept_auto_submit shows the popup.
                    }
                    _ = submit_rx => {
                        // Toast dismissed — auto-submit now.
                        {
                            let state = app.state::<AppState>();
                            *state.auto_submit_pending_data.write().unwrap() = None;
                        }
                        let auth2 = auth.clone();
                        let game_type2 = mapping.r#type.clone();
                        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
                        let (tx2, rx2) = tokio::sync::oneshot::channel::<bool>();
                        std::thread::spawn(move || {
                            let ok = if let Some(client) = api_client(&auth2) {
                                if game_type2.eq_ignore_ascii_case("live") {
                                    client.add_live_service_session(mapping.froglog_id, Some(date), Some(hours), Some("Session auto submitted with LilyPad".to_string()), false, true, None).is_ok()
                                } else {
                                    client.add_game_session(mapping.froglog_id, Some(date), Some(hours), Some("Session auto submitted with LilyPad".to_string()), false, true, None).is_ok()
                                }
                            } else { false };
                            let _ = tx2.send(ok);
                        });
                        let submitted = rx2.await.unwrap_or(false);
                        if !submitted {
                            let mut sessions = load_pending_sessions();
                            sessions.push(PendingSession {
                                id: chrono::Local::now().timestamp_millis().to_string(),
                                game_id: mapping.froglog_id,
                                game_type: mapping.r#type.clone(),
                                title: title_str.clone(),
                                hours,
                                notes: None,
                                spoiler: false,
                                is_public: true,
                                date: chrono::Local::now().format("%Y-%m-%d").to_string(),
                                failed_at: chrono::Local::now().to_rfc3339(),
                                error: "Auto-submit failed (auth or network issue)".to_string(),
                            });
                            save_pending_sessions(&sessions);
                            let app2 = app.clone();
                            let _ = app.run_on_main_thread(move || { let _ = update_tray_state(&app2); });
                            let _ = app.notification().builder()
                                .title("Session Queued")
                                .body(format!("{} ({}) — submit failed, open LilyPad to retry", title_str, time_str))
                                .show();
                        }
                    }
                }
                return;
            }

            // Regular games: silent auto-submit (no notes support)
            let date = chrono::Local::now().format("%Y-%m-%d").to_string();
            let mapping2 = mapping.clone();
            let date2 = date.clone();
            let game_type2 = mapping2.r#type.clone();
            let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
            std::thread::spawn(move || {
                let ok = if let Some(client) = api_client(&auth) {
                    log::info!("[LilyPad] handle_session_ended: auto-submitting hours");
                    client.update_game_hours(mapping2.froglog_id, hours).is_ok()
                } else {
                    log::warn!("[LilyPad] handle_session_ended: no API client available");
                    false
                };
                let _ = tx.send(ok);
            });
            let submitted = rx.await.unwrap_or(false);
            let title_str = mapping.title.as_deref().unwrap_or_else(|| mapping.process.as_str()).to_string();
            let total_mins = (duration_secs / 60.0).round() as u64;
            let disp_h = total_mins / 60;
            let disp_m = total_mins % 60;
            let time_str = if disp_h > 0 { format!("{}h {}m", disp_h, disp_m) } else { format!("{}m", disp_m) };
            if submitted {
                let _ = app.notification().builder()
                    .title("Session Auto-Submitted")
                    .body(format!("{} ({})", title_str, time_str))
                    .show();
                return;
            }
            // Auto-submit failed — save to pending queue and notify
            let mut sessions = load_pending_sessions();
            sessions.push(PendingSession {
                id: chrono::Local::now().timestamp_millis().to_string(),
                game_id: mapping2.froglog_id,
                game_type: game_type2,
                title: title_str.clone(),
                hours,
                notes: None,
                spoiler: false,
                is_public: true,
                date: date2,
                failed_at: chrono::Local::now().to_rfc3339(),
                error: "Auto-submit failed (auth or network issue)".to_string(),
            });
            save_pending_sessions(&sessions);
            let app2 = app.clone();
            let _ = app.run_on_main_thread(move || { let _ = update_tray_state(&app2); });
            let _ = app.notification().builder()
                .title("Session Queued")
                .body(format!("{} ({}) — submit failed, open LilyPad to retry", title_str, time_str))
                .show();
            return;
        }

        if let Some(w) = app.get_webview_window("main") {
            let has_notes = mapping.r#type.eq_ignore_ascii_case("live") || mapping.r#type.eq_ignore_ascii_case("session");
            let (sh, tb) = if has_notes {
                (SESSION_HEIGHT_LIVE, SESSION_TASKBAR_LIVE)
            } else {
                (SESSION_HEIGHT_REGULAR, SESSION_TASKBAR_REGULAR)
            };
            show_session_window(&w, sh, tb);
        }
        let _ = app.emit("session-ended", serde_json::json!({
             "processName": process_name,
             "mapping": {
                 "process": mapping.process,
                 "type": mapping.r#type,
                 "froglogId": mapping.froglog_id,
                 "title": mapping.title,
            },
             "durationSecs": duration_secs,
        }));
    });
}

fn build_tray_menu(app: &tauri::AppHandle, logged_in: bool, game_title: Option<String>) -> Result<Menu<Wry>, Box<dyn std::error::Error + Send + Sync>> {
    let quit_item = PredefinedMenuItem::quit(app, Some("Quit"))?;
    // While tracking, the status/stop items are *prepended* to the normal menu rather than
    // replacing it. Configure (and everything reachable from it) is safe during a session:
    // the active session's wait-thread holds its own clone of the ProcessMapping, and the
    // monitor skips all start-detection while current_session is occupied, so mapping edits,
    // library refreshes, and rescans can only affect *future* sessions, never the live one.
    let tracking_items = if let Some(ref title) = game_title {
        Some((
            MenuItemBuilder::with_id("tracking_status", format!("Now Tracking: {}", title))
                .enabled(false)
                .build(app)?,
            MenuItemBuilder::with_id("force_stop_tracking", "Stop Tracking Current Session").build(app)?,
        ))
    } else {
        None
    };
    if logged_in {
        let assign_exes_item = MenuItemBuilder::with_id("assign_exes", "Configure...").build(app)?;
        let about_item = MenuItemBuilder::with_id("about", "About").build(app)?;
        let logout_item = MenuItemBuilder::with_id("logout", "Logout").build(app)?;
        let pending_count = config::load_pending_sessions().len();
        let new_games_count = config::load_pending_game_submissions().len();
        let pending_item = if pending_count > 0 {
            Some(MenuItemBuilder::with_id("pending_sessions", format!("Pending Submissions ({})", pending_count)).build(app)?)
        } else {
            None
        };
        let new_games_item = if new_games_count > 0 {
            Some(MenuItemBuilder::with_id("new_games", format!("New Games ({})", new_games_count)).build(app)?)
        } else {
            None
        };
        let mut items: Vec<&dyn IsMenuItem<Wry>> = Vec::new();
        if let Some((ref status_item, ref force_stop_item)) = tracking_items {
            items.push(status_item);
            items.push(force_stop_item);
        }
        items.push(&assign_exes_item);
        if let Some(ref item) = new_games_item {
            items.push(item);
        }
        if let Some(ref item) = pending_item {
            items.push(item);
        }
        items.push(&about_item);
        items.push(&logout_item);
        items.push(&quit_item);
        let menu = Menu::with_items(app, &items)?;
        Ok(menu)
    } else if let Some((ref status_item, ref force_stop_item)) = tracking_items {
        // Tracking without a login shouldn't happen, but keep the session controls reachable.
        let menu = Menu::with_items(app, &[status_item, force_stop_item, &quit_item])?;
        Ok(menu)
    } else {
        let login_item = MenuItemBuilder::with_id("login", "Login").build(app)?;
        let about_item = MenuItemBuilder::with_id("about", "About").build(app)?;
        let menu = Menu::with_items(app, &[&login_item, &about_item, &quit_item])?;
        Ok(menu)
    }
}

fn update_tray_state(app: &tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let logged_in = state.auth.read().unwrap().token.is_some();
    let game_title = {
        let sess = state.current_session_arc.read().unwrap();
        sess.as_ref().map(|s| s.mapping.title.clone().unwrap_or_else(|| s.process_name.clone()))
    };
    let menu = build_tray_menu(app, logged_in, game_title.clone()).map_err(|e| e.to_string())?;
    if let Some(tray) = app.tray_by_id("main") {
        tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
        if let Some(ref title) = game_title {
            let _ = tray.set_icon(Some(TRAY_ICON_NOWPLAYING.clone()));
            let _ = tray.set_tooltip(Some(format!("LilyPad - Now Tracking: {}", title)));
        } else {
            let _ = tray.set_icon(Some(TRAY_ICON.clone()));
            let _ = tray.set_tooltip(Some("LilyPad - FrogLog Auto Tracker".to_string()));
        }
    }
    Ok(())
}

#[tauri::command]
fn login(
    _app: tauri::AppHandle,
    state: tauri::State<AppState>,
    base_url: String,
    username: String,
    password: String,
    remember_me: bool,
) -> Result<serde_json::Value, String> {
    let mut client = FroglogClient::new(base_url.trim_end_matches('/').to_string());
    let res = client.login(&username, &password, remember_me)?;
    let token = res.token.clone();
    client.set_token(Some(token.clone()));
    // Persist and switch to this account's process map (username = stable key so same user reuses same map after re-login)
    let mut auth = state.auth.write().unwrap();
    let old_map_path = process_map_path_for_auth(&*auth);
    auth.base_url = Some(base_url.trim_end_matches('/').to_string());
    auth.token = Some(token);
    auth.username = res.username.clone().or_else(|| Some(username.clone()));
    auth.save_to(&auth_config_path()).map_err(|e| e.to_string())?;
    let map_path = process_map_path_for_auth(&*auth);
    // Migrate: if path changed (token-key -> username-key) and old file exists, copy so mappings are preserved.
    // Do not copy when old path is the anonymous map (process-map.json) or we would overwrite user data after re-login.
    let anonymous_path = process_map_path_for_auth(&AuthConfig::default());
    if old_map_path != map_path && old_map_path != anonymous_path && old_map_path.exists() {
        let _ = std::fs::copy(&old_map_path, &map_path);
    }
    let process_map = ProcessMapConfig::load_from(&map_path);
    *state.process_map_arc.write().unwrap() = process_map;
    Ok(serde_json::json!({ "token": res.token, "username": res.username }))
}

#[tauri::command]
fn logout(_app: tauri::AppHandle, state: tauri::State<AppState>) -> Result<(), String> {
    let mut auth = state.auth.write().unwrap();
    *auth = AuthConfig::default();
    auth.save_to(&auth_config_path()).map_err(|e| e.to_string())?;
    Ok(())
}

/// Refresh tray state from current auth/session state. Called by frontend after login (delayed) so it runs when main thread is idle.
#[tauri::command]
fn refresh_tray_menu(app: tauri::AppHandle) -> Result<(), String> {
    let app_clone = app.clone();
    app.run_on_main_thread(move || {
        let _ = update_tray_state(&app_clone);
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn get_games(state: tauri::State<AppState>) -> Result<Vec<api::Game>, String> {
    let auth = state.auth.read().unwrap();
    let client = api_client(&auth).ok_or("No API URL configured")?;
    client.get_games()
}

#[tauri::command]
fn get_live_service_games(state: tauri::State<AppState>) -> Result<Vec<api::LiveServiceGame>, String> {
    let auth = state.auth.read().unwrap();
    let client = api_client(&auth).ok_or("No API URL configured")?;
    client.get_live_service_games()
}

#[tauri::command]
fn submit_session(
    app: tauri::AppHandle,
    state: tauri::State<AppState>,
    game_type: String, // "regular" | "live" | "session"
    game_id: i32,
    hours: f64,
    notes: Option<String>,
    spoiler: Option<bool>,
    is_public: Option<bool>,
    title: Option<String>,
) -> Result<serde_json::Value, String> {
    let auth = state.auth.read().unwrap();
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let spoiler = spoiler.unwrap_or(false);
    let is_public = is_public.unwrap_or(true);
    let result = match api_client(&auth) {
        None => Err("Not logged in".to_string()),
        Some(client) => if game_type.eq_ignore_ascii_case("live") {
            client.add_live_service_session(game_id, Some(date.clone()), Some(hours), notes.clone(), spoiler, is_public, None)
        } else if game_type.eq_ignore_ascii_case("session") {
            client.add_game_session(game_id, Some(date.clone()), Some(hours), notes.clone(), spoiler, is_public, None)
        } else {
            client.update_game_hours(game_id, hours)
        },
    };
    match result {
        Ok(v) => Ok(v),
        Err(e) => {
            let mut sessions = load_pending_sessions();
            sessions.push(PendingSession {
                id: chrono::Local::now().timestamp_millis().to_string(),
                game_id,
                game_type,
                title: title.unwrap_or_default(),
                hours,
                notes,
                spoiler,
                is_public,
                date,
                failed_at: chrono::Local::now().to_rfc3339(),
                error: e,
            });
            save_pending_sessions(&sessions);
            let app2 = app.clone();
            let _ = app.run_on_main_thread(move || { let _ = update_tray_state(&app2); });
            Ok(serde_json::json!({ "queued": true }))
        }
    }
}

#[tauri::command]
fn get_pending_sessions() -> Vec<PendingSession> {
    load_pending_sessions()
}

#[tauri::command]
fn retry_pending_session(
    state: tauri::State<AppState>,
    id: String,
) -> Result<(), String> {
    let mut sessions = load_pending_sessions();
    let idx = sessions.iter().position(|s| s.id == id).ok_or("Not found")?;
    let s = sessions[idx].clone();
    let auth = state.auth.read().unwrap().clone();
    let client = api_client(&auth).ok_or("Not logged in")?;
    let notes = Some(s.notes.filter(|n| !n.is_empty()).unwrap_or_else(|| "Session submitted from Pending Sessions".to_string()));
    if s.game_type.eq_ignore_ascii_case("live") {
        client.add_live_service_session(s.game_id, Some(s.date), Some(s.hours), notes, s.spoiler, s.is_public, None)?;
    } else if s.game_type.eq_ignore_ascii_case("session") {
        client.add_game_session(s.game_id, Some(s.date), Some(s.hours), notes, s.spoiler, s.is_public, None)?;
    } else {
        client.update_game_hours(s.game_id, s.hours)?;
    }
    sessions.remove(idx);
    save_pending_sessions(&sessions);
    Ok(())
}

#[tauri::command]
fn delete_pending_session(id: String) -> Result<(), String> {
    let mut sessions = load_pending_sessions();
    sessions.retain(|s| s.id != id);
    save_pending_sessions(&sessions);
    Ok(())
}

#[tauri::command]
fn get_pending_game_submissions() -> Vec<config::PendingGameSubmission> {
    config::load_pending_game_submissions()
}

#[tauri::command]
fn dismiss_pending_game_submission(appid: String) -> Result<(), String> {
    config::remove_pending_game_submission(&appid);
    Ok(())
}

#[tauri::command]
fn search_igdb_games(state: tauri::State<AppState>, query: String) -> Result<Vec<serde_json::Value>, String> {
    let auth = state.auth.read().unwrap();
    let client = api_client(&auth).ok_or("Not logged in")?;
    client.search_igdb(&query)
}

/// Logs each individually-accumulated real-world play session in `entry.sessions` as its own
/// FrogLog session (each with its own real date), via `submit_one(date, hours)`. Falls back to a
/// single entry dated today using the accumulated `entry.hours` total for a pending item
/// persisted before per-session tracking existed (`sessions` defaults to empty in that case) --
/// otherwise resolving it would silently lose the hours rather than just merging them.
fn log_each_pending_session(
    entry: &config::PendingGameSubmission,
    mut submit_one: impl FnMut(String, f64) -> Result<serde_json::Value, String>,
) -> Result<(), String> {
    if entry.sessions.is_empty() {
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        submit_one(date, entry.hours)?;
    } else {
        for session in &entry.sessions {
            submit_one(session.date.clone(), session.hours)?;
        }
    }
    Ok(())
}

/// Resolves a pending game submission by creating a brand-new FrogLog game entry for it.
/// `igdb_title` is the exact title of the IGDB search result the user picked; enriched details
/// for it are fetched fresh (via `fetch_game_details`) rather than trusting the lightweight
/// search result, since `/search/fetch` returns the richer, create-ready field set.
#[tauri::command]
fn resolve_pending_game_as_new(
    state: tauri::State<AppState>,
    appid: String,
    igdb_title: String,
) -> Result<serde_json::Value, String> {
    let pending = config::load_pending_game_submissions();
    let entry = pending
        .iter()
        .find(|p| p.appid == appid)
        .ok_or("Pending submission not found")?
        .clone();

    let auth = state.auth.read().unwrap();
    let client = api_client(&auth).ok_or("Not logged in")?;

    // If IGDB has no exact match for the confirmed title (`/search/fetch` can fail even when
    // `/search` just found it, for a handful of titles), fall back to `igdb_title` itself — the
    // title the user actually confirmed — never `entry.title`, which is only LilyPad's original
    // guess (literally the exe's folder name for a non-Steam game, often full of release-group
    // junk like "-DenuvOwO [PORTABLE]").
    let mut payload = client
        .fetch_game_details(&igdb_title)
        .unwrap_or_else(|_| serde_json::json!({ "title": igdb_title.clone() }));
    let obj = payload.as_object_mut().ok_or("Unexpected response shape")?;
    // /search/fetch names this field "dev_country"; POST /games expects "studio_country".
    if let Some(country) = obj.remove("dev_country") {
        obj.insert("studio_country".to_string(), country);
    }
    obj.insert("platform".to_string(), serde_json::json!("PC"));
    obj.insert("is_public".to_string(), serde_json::json!(true));
    // Session-tracked, matching how LilyPad actually recorded this play time (discrete
    // sessions, not a single running total) — hours go in via add_game_session below rather
    // than hours_played, same as a session-tracked game's aggregate is always computed.
    obj.insert("session_tracking".to_string(), serde_json::json!(true));
    // Start date is when LilyPad first saw this game being played, not today (when the user
    // finally got around to resolving it) — first_seen_secs is exactly that.
    if let Some(start_date) = chrono::DateTime::from_timestamp(entry.first_seen_secs as i64, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
    {
        obj.insert("start_date".to_string(), serde_json::json!(start_date));
    }
    // Prefer our own detected Steam appid over whatever (possibly null) match IGDB found —
    // ours is known-accurate since it's literally how this session was detected. The backend
    // column is numeric, so parse it rather than sending the string appid as-is.
    if let Ok(appid_num) = entry.appid.parse::<i64>() {
        obj.insert("steam_app_id".to_string(), serde_json::json!(appid_num));
    }

    let created = client.create_game(payload)?;
    let created_id = created.get("id").and_then(|v| v.as_i64()).ok_or("Created game missing id")?;
    // The confirmed/matched title, not `entry.title` (LilyPad's original guess — literally the
    // exe's folder name for a non-Steam game) — that's what should show up in the tray/session
    // popup/notifications going forward.
    let created_title = created.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());

    log_each_pending_session(&entry, |date, hours| {
        client.add_game_session(
            created_id as i32,
            Some(date),
            Some(hours),
            Some("Session logged from LilyPad".to_string()),
            false,
            true,
            None,
        )
    })?;

    config::remove_pending_game_submission(&appid);

    // Link the exe to the newly-created game so future sessions are tracked normally instead
    // of falling through detection again. Best-effort: a failure here shouldn't fail the
    // resolve itself, and older pending entries (persisted before exe_name existed) have
    // nothing to link.
    if !entry.exe_name.is_empty() {
        if let Err(e) = config::link_process_mapping(
            &state.process_map_arc,
            &auth,
            entry.exe_name.clone(),
            "session".to_string(),
            created_id as i32,
            created_title.or_else(|| Some(entry.title.clone())),
        ) {
            log::warn!("[LilyPad] failed to link newly-created game's exe: {e}");
        }
    }

    Ok(created)
}

/// Resolves a pending game submission flagged as a possible replay (`entry.replay_of` is
/// `Some`) by creating a brand-new FrogLog game entry for it — distinct from the old
/// Completed/DNF entry, which is left untouched. Mirrors `resolve_pending_game_as_new`, but
/// skips the IGDB lookup entirely: the game is already known (same Steam appid as the existing
/// entry), so this just fetches that entry's own details via `GET /games/{id}` and reuses them
/// as the starting point for the new row, rather than re-deriving them from an IGDB search.
/// `replay: true` is set explicitly rather than relying on the backend's title-based auto-detect
/// on `POST /games` — we already know for certain this is the same title, no guessing needed.
#[tauri::command]
fn resolve_pending_game_as_replay(
    state: tauri::State<AppState>,
    appid: String,
) -> Result<serde_json::Value, String> {
    let pending = config::load_pending_game_submissions();
    let entry = pending
        .iter()
        .find(|p| p.appid == appid)
        .ok_or("Pending submission not found")?
        .clone();
    let replay_of = entry.replay_of.clone().ok_or("Pending submission has no replay match")?;

    let auth = state.auth.read().unwrap();
    let client = api_client(&auth).ok_or("Not logged in")?;

    let mut payload = client.get_game_raw(replay_of.id)?;
    let obj = payload.as_object_mut().ok_or("Unexpected response shape")?;
    // Strip everything identity/progress-related from the old entry -- this is a fresh
    // playthrough, not a continuation, so none of it should carry over.
    for field in ["id", "hours_played", "start_date", "end_date", "status", "status_override", "user_id", "created_at"] {
        obj.remove(field);
    }
    obj.insert("dnf".to_string(), serde_json::json!(false));
    obj.insert("session_tracking".to_string(), serde_json::json!(true));
    obj.insert("replay".to_string(), serde_json::json!(true));
    if let Some(start_date) = chrono::DateTime::from_timestamp(entry.first_seen_secs as i64, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
    {
        obj.insert("start_date".to_string(), serde_json::json!(start_date));
    }

    let created = client.create_game(payload)?;
    let created_id = created.get("id").and_then(|v| v.as_i64()).ok_or("Created game missing id")?;
    let created_title = created.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());

    log_each_pending_session(&entry, |date, hours| {
        client.add_game_session(
            created_id as i32,
            Some(date),
            Some(hours),
            Some("Session logged from LilyPad".to_string()),
            false,
            true,
            None,
        )
    })?;

    config::remove_pending_game_submission(&appid);

    // Link the exe to the newly-created replay entry, not the old one, so future sessions of
    // this playthrough are tracked against it instead of falling through detection again.
    if !entry.exe_name.is_empty() {
        if let Err(e) = config::link_process_mapping(
            &state.process_map_arc,
            &auth,
            entry.exe_name.clone(),
            "session".to_string(),
            created_id as i32,
            created_title.or(Some(replay_of.title)),
        ) {
            log::warn!("[LilyPad] failed to link newly-created replay's exe: {e}");
        }
    }

    // Without this, the library index only refreshes every 5 minutes -- relaunching the same
    // appid shortly after resolving it here would still see the old Completed/DNF entry and
    // prompt the replay question all over again instead of auto-linking to the new entry.
    refresh_library_index_state(&state);

    Ok(created)
}

/// Resolves a pending game submission by logging its accumulated hours against a game/live
/// service entry the user already has in their FrogLog library.
#[tauri::command]
fn resolve_pending_game_as_existing(
    state: tauri::State<AppState>,
    appid: String,
    game_type: String, // "regular" | "session" | "live"
    game_id: i32,
    game_title: String,
) -> Result<(), String> {
    let pending = config::load_pending_game_submissions();
    let entry = pending
        .iter()
        .find(|p| p.appid == appid)
        .ok_or("Pending submission not found")?
        .clone();

    let auth = state.auth.read().unwrap();
    let client = api_client(&auth).ok_or("Not logged in")?;
    // Anything mapped here should end up session-tracked, not "regular" (a single running
    // hours_played total) -- matches the silent already-owned auto-link path (see the comment
    // in monitor.rs). enable_session_tracking is idempotent and preserves any pre-existing hours
    // as a "Pre-tracked hours" session, same flow as enabling it manually on the website.
    let effective_game_type = if game_type.eq_ignore_ascii_case("live") {
        "live".to_string()
    } else {
        if let Err(e) = client.enable_session_tracking(game_id) {
            log::warn!("[LilyPad] failed to enable session tracking: {e}");
        }
        // If this game is currently Completed/DNF, logging a fresh session against it means
        // it's being played again -- clear its end date so it reads as "In Progress" instead of
        // silently piling hours onto a game that still looks finished. No-op otherwise.
        if let Err(e) = client.resume_finished_game(game_id) {
            log::warn!("[LilyPad] failed to resume finished game: {e}");
        }
        "session".to_string()
    };
    let notes = Some("Logged from LilyPad's untracked-session detection".to_string());
    log_each_pending_session(&entry, |date, hours| {
        if effective_game_type == "live" {
            client.add_live_service_session(game_id, Some(date), Some(hours), notes.clone(), false, true, None)
        } else {
            client.add_game_session(game_id, Some(date), Some(hours), notes.clone(), false, true, None)
        }
    })?;
    // Best-effort: a game picked here might be a Steam-bulk-imported entry that was never
    // actually started (status "Imported", no start_date) -- now that a real session's been
    // logged against it, transition it to "In Progress". No-op if it isn't "Imported".
    if let Err(e) = client.fix_imported_status_if_needed(game_id, &effective_game_type) {
        log::warn!("[LilyPad] failed to fix imported status: {e}");
    }
    config::remove_pending_game_submission(&appid);

    // Link the exe to this game so future sessions are tracked normally instead of falling
    // through detection again. Uses the existing game's own confirmed title (from the caller),
    // not `entry.title` (LilyPad's original guess). Best-effort, same as resolve_pending_game_as_new.
    if !entry.exe_name.is_empty() {
        if let Err(e) = config::link_process_mapping(
            &state.process_map_arc,
            &auth,
            entry.exe_name.clone(),
            effective_game_type,
            game_id,
            Some(game_title),
        ) {
            log::warn!("[LilyPad] failed to link exe to existing game: {e}");
        }
    }

    // Without this, the library index only refreshes every 5 minutes -- relaunching the same
    // exe shortly after resolving it here would still see the old Completed/DNF status and
    // prompt the replay question all over again instead of resuming normally. Matches the GTK
    // build's `resolve_as_existing`, which already does this.
    refresh_library_index_state(&state);

    Ok(())
}

/// Cancel the live/session auto-submit countdown and open the session popup so the user can add notes.
#[tauri::command]
fn intercept_auto_submit(
    app: tauri::AppHandle,
    state: tauri::State<AppState>,
) -> Result<(), String> {
    let cancel_tx = state.auto_submit_cancel_tx.write().unwrap().take();
    if let Some(tx) = cancel_tx {
        let _ = tx.send(());
    }
    let data = state.auto_submit_pending_data.write().unwrap().take();
    if let Some(session_data) = data {
        if let Some(w) = app.get_webview_window("main") {
            show_session_window(&w, SESSION_HEIGHT_LIVE, SESSION_TASKBAR_LIVE);
        }
        let _ = app.emit("session-ended", session_data);
    }
    Ok(())
}

#[tauri::command]
fn get_auth_config(state: tauri::State<AppState>) -> AuthConfig {
    state.auth.read().unwrap().clone()
}

#[tauri::command]
fn get_process_mappings(state: tauri::State<AppState>) -> ProcessMapConfig {
    state.process_map_arc.read().unwrap().clone()
}

#[tauri::command]
fn save_process_mapping(
    state: tauri::State<AppState>,
    process: String,
    game_type: String,
    froglog_id: i32,
    title: Option<String>,
    title_filter: Option<String>,
) -> Result<(), String> {
    let mut map = state.process_map_arc.read().unwrap().clone();
    
    // Check existing mappings for this process
    let existing_for_process: Vec<&ProcessMapping> = map
        .mappings
        .iter()
        .filter(|m| m.process.eq_ignore_ascii_case(&process))
        .collect();

    // Check if there are other mappings for this exe (excluding the one we're updating)
    let has_other_mappings = existing_for_process.iter().any(|m| 
        !(m.froglog_id == froglog_id && m.r#type == game_type)
    );

    if has_other_mappings {
        // Check if all existing mappings for this exe have filters
        let existing_missing_filter = existing_for_process.iter()
            .filter(|m| !(m.froglog_id == froglog_id && m.r#type == game_type))
            .any(|m| m.title_filter.as_ref().map_or(true, |s: &String| s.trim().is_empty()));
        
        if existing_missing_filter {
            return Err(
                "Multiple games are mapped to this executable. All mappings must have a Window Title Filter to distinguish them. Please add filters to existing mappings first.".to_string()
            );
        }

        // Ensure the new/updated mapping also has a filter
        if title_filter.as_ref().map_or(true, |s: &String| s.trim().is_empty()) {
            return Err(
                "Window Title Filter is required when multiple games share the same executable.".to_string()
            );
        }
        
        // Check for duplicate filters
        let duplicate_filter = existing_for_process.iter()
            .filter(|m| !(m.froglog_id == froglog_id && m.r#type == game_type))
            .any(|m| {
                if let (Some(existing), Some(new)) = (&m.title_filter, &title_filter) {
                    existing.trim().to_lowercase() == new.trim().to_lowercase()
                } else {
                    false
                }
            });
            
        if duplicate_filter {
            return Err(
                "Window Title Filter must be unique for each game using the same executable.".to_string()
            );
        }
    }

    // Remove any existing mapping for this specific game (by id+type)
    map.mappings
        .retain(|m| !(m.froglog_id == froglog_id && m.r#type == game_type));

    map.mappings.push(config::ProcessMapping {
        process,
        r#type: game_type,
        froglog_id,
        title,
        title_filter,
    });

    map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()))
        .map_err(|e| e.to_string())?;
    *state.process_map_arc.write().unwrap() = map;
    Ok(())
}

/// Turns on session tracking for a game (idempotent -- no-op if it's already on), preserving
/// any pre-existing hours as a "Pre-tracked hours" session. Used by the Configure UI when the
/// user manually maps an exe to a "regular" game, since anything LilyPad tracks should end up
/// session-tracked -- matches the automatic behavior on the silent already-owned auto-link path
/// and the New Games "Map to Existing" flow.
#[tauri::command]
fn enable_session_tracking(state: tauri::State<AppState>, game_id: i32) -> Result<(), String> {
    let auth = state.auth.read().unwrap();
    let client = api_client(&auth).ok_or("Not logged in")?;
    client.enable_session_tracking(game_id)?;
    Ok(())
}

/// Forces an immediate refresh of `library_index_arc` (see `refresh_library_index_state`).
/// Called from the Configure view on open, which already fetches this same games/live-service
/// data for its own display -- otherwise the shared index used for auto-link/New-Games
/// detection can sit stale for up to 5 minutes (the periodic refresh interval) after a change
/// made directly on the FrogLog website.
#[tauri::command]
fn refresh_library_index(state: tauri::State<AppState>) {
    refresh_library_index_state(&state);
    // Also rescan installed games (Steam + watched non-Steam directories) here -- a game added
    // to an already-watched folder has no dedicated immediate-refresh trigger of its own (unlike
    // adding/removing the watched directory itself), so without this it would otherwise sit
    // undetected until the next 300s periodic scan. Configure opening is already the established
    // "refresh anything that might be stale" moment (see `refresh_library_index_state`'s doc).
    refresh_installed_games(&state);
}

#[tauri::command]
fn save_auto_submit(state: tauri::State<AppState>, regular: bool, live: bool, session: bool) -> Result<(), String> {
    let mut map = state.process_map_arc.read().unwrap().clone();
    map.auto_submit_regular = regular;
    map.auto_submit_live = live;
    map.auto_submit_session = session;
    map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()))
        .map_err(|e| e.to_string())?;
    *state.process_map_arc.write().unwrap() = map;
    Ok(())
}

#[tauri::command]
fn save_unmapped_detection(state: tauri::State<AppState>, disabled: bool) -> Result<(), String> {
    let mut map = state.process_map_arc.read().unwrap().clone();
    map.disable_unmapped_game_detection = disabled;
    map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()))
        .map_err(|e| e.to_string())?;
    *state.process_map_arc.write().unwrap() = map;
    Ok(())
}

#[tauri::command]
fn save_now_playing_share(state: tauri::State<AppState>, share: bool) -> Result<(), String> {
    let mut map = state.process_map_arc.read().unwrap().clone();
    map.share_now_playing = share;
    map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()))
        .map_err(|e| e.to_string())?;
    *state.process_map_arc.write().unwrap() = map;

    let auth = state.auth.read().unwrap().clone();
    std::thread::spawn(move || {
        if let Some(client) = api_client(&auth) {
            log::info!("[LilyPad] save_now_playing_share: updating show_current_session on FrogLog to {}", share);
            let _ = client.set_show_current_session(share);
        } else {
            log::warn!("[LilyPad] save_now_playing_share: no API client available to update show_current_session");
        }
    });

    Ok(())
}

#[tauri::command]
fn delete_process_mapping(
    state: tauri::State<AppState>,
    froglog_id: i32,
    game_type: String,
) -> Result<(), String> {
    let mut map = state.process_map_arc.read().unwrap().clone();
    map.mappings
        .retain(|m| !(m.froglog_id == froglog_id && m.r#type == game_type));
    map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()))
        .map_err(|e| e.to_string())?;
    *state.process_map_arc.write().unwrap() = map;
    Ok(())
}

#[tauri::command]
fn open_url(app: tauri::AppHandle, url: String) -> Result<(), String> {
    // Open URL in the system default browser
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_window_size(
    app: tauri::AppHandle,
    width: Option<f64>,
    height: Option<f64>,
) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        let current = w.inner_size().map_err(|e| e.to_string())?;
        let scale = w.scale_factor().unwrap_or(1.0);
        let cur_w = current.width as f64 / scale;
        let cur_h = current.height as f64 / scale;
        let new_width = width.filter(|&x| x > 0.0).unwrap_or(cur_w);
        let new_height = height.filter(|&x| x > 0.0).unwrap_or(cur_h);
        w.set_size(tauri::Size::Logical(tauri::LogicalSize {
            width: new_width,
            height: new_height,
        }))
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn hide_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        w.hide().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn pick_exe_file(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let dialog = app.dialog().file();
    // Native Linux/macOS binaries conventionally have no file extension, so only
    // constrain the picker to `.exe` on Windows.
    #[cfg(windows)]
    let dialog = dialog.add_filter("Executables", &["exe"]);
    let path = dialog.blocking_pick_file();
    Ok(path.and_then(|p| p.as_path().map(|path| path.display().to_string())))
}

#[tauri::command]
fn pick_directory(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let path = app.dialog().file().blocking_pick_folder();
    Ok(path.and_then(|p| p.as_path().map(|path| path.display().to_string())))
}

#[tauri::command]
fn get_watched_directories(state: tauri::State<AppState>) -> Vec<WatchedDirectory> {
    state.process_map_arc.read().unwrap().watched_directories.clone()
}

#[tauri::command]
fn add_watched_directory(state: tauri::State<AppState>, path: String) -> Result<(), String> {
    let mut map = state.process_map_arc.read().unwrap().clone();
    if !map.watched_directories.iter().any(|w| w.path == path) {
        map.watched_directories.push(WatchedDirectory { path });
    }
    map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()))
        .map_err(|e| e.to_string())?;
    *state.process_map_arc.write().unwrap() = map;
    // Scan immediately rather than waiting for the next 300s background refresh, so a newly
    // added folder's games show up right away.
    refresh_installed_games(&state);
    Ok(())
}

#[tauri::command]
fn remove_watched_directory(state: tauri::State<AppState>, path: String) -> Result<(), String> {
    let mut map = state.process_map_arc.read().unwrap().clone();
    map.watched_directories.retain(|w| w.path != path);
    map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()))
        .map_err(|e| e.to_string())?;
    *state.process_map_arc.write().unwrap() = map;
    refresh_installed_games(&state);
    Ok(())
}

/// Returns everything the current scan considers "installed" (Steam + watched non-Steam
/// directories), already excluding anything in `excluded_apps` (see `refresh_installed_games`)
/// -- used to populate the Excluded Games picker with what's left to exclude.
#[tauri::command]
fn get_installed_games(state: tauri::State<AppState>) -> Vec<InstalledGame> {
    state.installed_games_arc.read().unwrap().clone()
}

#[tauri::command]
fn get_excluded_apps(state: tauri::State<AppState>) -> Vec<ExcludedApp> {
    state.process_map_arc.read().unwrap().excluded_apps.clone()
}

#[tauri::command]
fn add_excluded_app(state: tauri::State<AppState>, appid: String, name: String) -> Result<(), String> {
    let mut map = state.process_map_arc.read().unwrap().clone();
    if !map.excluded_apps.iter().any(|e| e.appid == appid) {
        map.excluded_apps.push(ExcludedApp { appid, name });
    }
    map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()))
        .map_err(|e| e.to_string())?;
    *state.process_map_arc.write().unwrap() = map;
    // Scan immediately rather than waiting for the next 300s background refresh, so the
    // excluded app stops being detectable right away.
    refresh_installed_games(&state);
    Ok(())
}

#[tauri::command]
fn remove_excluded_app(state: tauri::State<AppState>, appid: String) -> Result<(), String> {
    let mut map = state.process_map_arc.read().unwrap().clone();
    map.excluded_apps.retain(|e| e.appid != appid);
    map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()))
        .map_err(|e| e.to_string())?;
    *state.process_map_arc.write().unwrap() = map;
    refresh_installed_games(&state);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let auth = AuthConfig::load_from(&auth_config_path());
    let process_map = ProcessMapConfig::load_from(&process_map_path_for_auth(&auth));
    let log_dir = dirs::data_local_dir()
        .map(|d| d.join("froglog-lilypad"))
        .unwrap_or_else(|| std::path::PathBuf::from("froglog-lilypad"));
    tauri::Builder::default()
        .plugin(tauri_plugin_log::Builder::new()
            .level(log::LevelFilter::Info)
            .target(tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Folder {
                path: log_dir,
                file_name: Some("lilypad".to_string()),
            }))
            .build())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            process_map_arc: Arc::new(RwLock::new(process_map)),
            current_session_arc: Arc::new(RwLock::new(None)),
            auth: RwLock::new(auth),
            force_stopped_process: Arc::new(RwLock::new(None)),
            auto_submit_cancel_tx: Arc::new(RwLock::new(None)),
            auto_submit_pending_data: Arc::new(RwLock::new(None)),
            installed_games_arc: Arc::new(RwLock::new(Vec::new())),
            library_index_arc: Arc::new(RwLock::new(LibraryIndex::default())),
        })
        .invoke_handler(tauri::generate_handler![
            login,
            logout,
            refresh_tray_menu,
            get_games,
            get_live_service_games,
            submit_session,
            get_pending_sessions,
            retry_pending_session,
            delete_pending_session,
            get_pending_game_submissions,
            dismiss_pending_game_submission,
            search_igdb_games,
            resolve_pending_game_as_new,
            resolve_pending_game_as_existing,
            resolve_pending_game_as_replay,
            intercept_auto_submit,
            get_auth_config,
            get_process_mappings,
            save_process_mapping,
            enable_session_tracking,
            refresh_library_index,
            delete_process_mapping,
            save_auto_submit,
            save_now_playing_share,
            save_unmapped_detection,
            hide_window,
            set_window_size,
            open_url,
            pick_exe_file,
            pick_directory,
            get_watched_directories,
            add_watched_directory,
            remove_watched_directory,
            get_installed_games,
            get_excluded_apps,
            add_excluded_app,
            remove_excluded_app,
        ])
        .on_window_event(|window, event| {
            if window.label() == "main" {
                // Keep app running in tray: closing the main window hides it instead of exiting
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
                // Live-update the DOM theme attribute when the OS/desktop theme changes.
                if let WindowEvent::ThemeChanged(_) = event {
                    if let Some(w) = window.app_handle().get_webview_window("main") {
                        apply_theme(&w);
                    }
                }
            }
        })
        .setup(|app| {
            let _ = app.autolaunch().enable();
            let handle = app.handle().clone();
            let logged_in = app.state::<AppState>().auth.read().unwrap().token.is_some();
            let menu = build_tray_menu(&handle, logged_in, None).map_err(|e| e.to_string())?;

            let _tray = TrayIconBuilder::with_id("main")
                .icon(TRAY_ICON.clone())
                .tooltip("LilyPad - FrogLog Auto Tracker")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| {
                    let id = event.id.as_ref();
                    if id == "login" {
                        if let Some(w) = app.get_webview_window("main") {
                            show_window_at_height(&w, WINDOW_WIDTH, MAIN_ABOUT_HEIGHT);
                            let _ = w.emit("show-login", ());
                        }
                    } else if id == "assign_exes" {
                        if let Some(w) = app.get_webview_window("main") {
                            show_window_at_height(&w, WINDOW_WIDTH, DEFAULT_HEIGHT);
                            let _ = w.emit("open-mappings", ());
                        }
                    } else if id == "about" {
                        if let Some(w) = app.get_webview_window("main") {
                            show_window_at_height(&w, WINDOW_WIDTH, MAIN_ABOUT_HEIGHT);
                            let _ = w.emit("show-main", ());
                        }
                    } else if id == "logout" {
                        let state = app.state::<AppState>();
                        let mut auth = state.auth.write().unwrap();
                        *auth = AuthConfig::default();
                        let _ = auth.save_to(&auth_config_path());
                        drop(auth);
                        // Switch to anonymous process map so next user doesn't see previous account's mappings
                        let empty_auth = AuthConfig::default();
                        let map_path = process_map_path_for_auth(&empty_auth);
                        let process_map = ProcessMapConfig::load_from(&map_path);
                        *state.process_map_arc.write().unwrap() = process_map;
                        let _ = update_tray_state(app);
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.emit("show-login", ());
                        }
                    } else if id == "pending_sessions" {
                        if let Some(w) = app.get_webview_window("main") {
                            show_window_at_height(&w, WINDOW_WIDTH, 500.0);
                            let _ = w.emit("show-pending", ());
                        }
                    } else if id == "new_games" {
                        if let Some(w) = app.get_webview_window("main") {
                            show_window_at_height(&w, WINDOW_WIDTH, 500.0);
                            let _ = w.emit("show-new-games", ());
                        }
                    } else if id == "force_stop_tracking" {
                        let state = app.state::<AppState>();
                        // Grab session info before clearing so we can submit it
                        let session_info = {
                            let sess = state.current_session_arc.read().unwrap();
                            sess.as_ref().map(|s| (s.process_name.clone(), s.mapping.clone(), s.started_at.elapsed().as_secs_f64()))
                        };
                        if let Some((process_name, mapping, duration_secs)) = session_info {
                            // Block re-tracking until the process actually exits
                            *state.force_stopped_process.write().unwrap() = Some(process_name.clone());
                            // Clear in-memory session
                            *state.current_session_arc.write().unwrap() = None;
                            // Delete persisted session file
                            if let Ok(data_dir) = app.path().app_data_dir() {
                                let _ = std::fs::remove_file(data_dir.join("active-session.json"));
                            }
                            // Always show the popup (never auto-submit on force stop —
                            // user is likely correcting a bad mapping)
                            let auth = state.auth.read().unwrap().clone();
                            std::thread::spawn(move || {
                                if let Some(client) = api_client(&auth) {
                                    let _ = client.clear_now_playing();
                                }
                            });
                            let _ = app.emit("session-ended", serde_json::json!({
                                "processName": process_name,
                                "mapping": {
                                    "process": mapping.process,
                                    "type": mapping.r#type,
                                    "froglogId": mapping.froglog_id,
                                    "title": mapping.title,
                                },
                                "durationSecs": duration_secs,
                                "forced": true,
                            }));
                            if let Some(w) = app.get_webview_window("main") {
                                let has_notes = mapping.r#type.eq_ignore_ascii_case("live") || mapping.r#type.eq_ignore_ascii_case("session");
                                let (sh, tb) = if has_notes {
                                    (SESSION_HEIGHT_LIVE, SESSION_TASKBAR_LIVE)
                                } else {
                                    (SESSION_HEIGHT_REGULAR, SESSION_TASKBAR_REGULAR)
                                };
                                show_session_window(&w, sh, tb);
                            }
                        }
                        // Reset tray to idle state
                        let _ = update_tray_state(app);
                    }
                })
                .build(app)?;

            // Show login window on first launch (no saved credentials)
            if !logged_in {
                if let Some(w) = app.get_webview_window("main") {
                    show_window_at_height(&w, WINDOW_WIDTH, MAIN_ABOUT_HEIGHT);
                }
            }

            // Start process monitor in background (share process_map with AppState)
            let app_handle = app.handle().clone();
            let app_handle_for_unmapped = app_handle.clone();
            let app_handle_for_already_owned = app_handle.clone();
            let state = app.state::<AppState>();
            let config = Arc::clone(&state.process_map_arc);
            let current_session = Arc::clone(&state.current_session_arc);
            let force_stopped_process = Arc::clone(&state.force_stopped_process);
            let installed_games_arc = Arc::clone(&state.installed_games_arc);
            let library_index_arc = Arc::clone(&state.library_index_arc);

            // Refresh the installed-Steam-games scan and the FrogLog library index
            // periodically in the background, so newly installed games or newly logged
            // games are picked up without restarting LilyPad.
            {
                let app_handle_refresh = app_handle.clone();
                std::thread::spawn(move || loop {
                    {
                        let state = app_handle_refresh.state::<AppState>();
                        refresh_installed_games(&state);
                        refresh_library_index_state(&state);
                    }
                    std::thread::sleep(Duration::from_secs(300));
                });
            }

            // Path used to persist the active session across crashes/restarts.
            let session_path = app_handle.path().app_data_dir()
                .map(|d| d.join("active-session.json"))
                .unwrap_or_else(|_| std::path::PathBuf::from("active-session.json"));

            // Recover any session that was interrupted by a LilyPad crash or driver restart.
            if let Ok(json) = std::fs::read_to_string(&session_path) {
                if let Ok(persisted) = serde_json::from_str::<PersistedSession>(&json) {
                    let saved_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(persisted.started_at_secs);
                    let duration_so_far = SystemTime::now().duration_since(saved_wall).unwrap_or_default();
                    let recovery_mapping = ProcessMapping {
                        process: persisted.process.clone(),
                        r#type: persisted.game_type.clone(),
                        froglog_id: persisted.froglog_id,
                        title: persisted.title.clone(),
                        title_filter: None,
                    };

                    let mut sys = System::new_all();
                    sys.refresh_processes(ProcessesToUpdate::All);
                    let still_running_pid = sys.processes().iter().find_map(|(pid, p)| {
                        let exe_name = p.exe()
                            .and_then(|path| path.file_name().and_then(|n| n.to_str().map(String::from)))
                            .unwrap_or_else(|| p.name().to_string_lossy().into_owned());
                        if exe_name.eq_ignore_ascii_case(&persisted.process) { Some(*pid) } else { None }
                    });

                    if let Some(pid) = still_running_pid {
                        // Game still running — restore the session with the correct original start time.
                        let started_at = Instant::now().checked_sub(duration_so_far).unwrap_or_else(Instant::now);
                        *current_session.write().unwrap() = Some(ActiveSession {
                            process_name: persisted.process.clone(),
                            mapping: recovery_mapping.clone(),
                            started_at,
                        });
                        let session_path_w = session_path.clone();
                        let current_session_w = Arc::clone(&current_session);
                        let handle_w = app_handle.clone();
                        let process_name_w = persisted.process.clone();
                        let saved_secs = persisted.started_at_secs;
                        std::thread::spawn(move || {
                            let mut sys2 = System::new_all();
                            let deadline = Instant::now() + Duration::from_secs(10);
                            loop {
                                sys2.refresh_processes(ProcessesToUpdate::All);
                                if let Some(proc) = sys2.process(pid) {
                                    proc.wait();
                                    break;
                                }
                                if Instant::now() > deadline { break; }
                                std::thread::sleep(Duration::from_secs(1));
                            }
                            let duration_secs = SystemTime::now()
                                .duration_since(SystemTime::UNIX_EPOCH + Duration::from_secs(saved_secs))
                                .unwrap_or_default()
                                .as_secs_f64();
                            let _ = std::fs::remove_file(&session_path_w);
                            *current_session_w.write().unwrap() = None;
                            handle_session_ended(handle_w, process_name_w, recovery_mapping, duration_secs);
                        });
                    } else {
                        // Game already gone — report the session immediately.
                        let _ = std::fs::remove_file(&session_path);
                        handle_session_ended(app_handle.clone(), persisted.process, recovery_mapping, duration_so_far.as_secs_f64());
                    }
                }
            }

            run_poll_loop(
                config,
                current_session,
                Arc::new(AtomicBool::new(false)),
                force_stopped_process,
                2,
                {
                    let handle = app_handle.clone();
                    let session_path_start = session_path.clone();
                    move |process_name, mapping: ProcessMapping| {
                        // Persist session start so it survives a LilyPad crash.
                        let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
                        if let Ok(json) = serde_json::to_string(&PersistedSession {
                            process: mapping.process.clone(),
                            game_type: mapping.r#type.clone(),
                            froglog_id: mapping.froglog_id,
                            title: mapping.title.clone(),
                            started_at_secs: secs,
                        }) {
                            if let Some(parent) = session_path_start.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            let _ = std::fs::write(&session_path_start, json);
                        }
                        let game_name = mapping.title.as_deref().unwrap_or_else(|| process_name.as_str());
                        let _ = handle.notification().builder()
                            .title("Tracking Started")
                            .body(game_name)
                            .show();
                        let started_at_iso = chrono::Utc::now().to_rfc3339();
                        let (auth, share_now_playing) = {
                            let state = handle.state::<AppState>();
                            let auth_guard = state.auth.read().unwrap();
                            let map_guard = state.process_map_arc.read().unwrap();
                            (auth_guard.clone(), map_guard.share_now_playing)
                        };
                        if share_now_playing {
                            std::thread::spawn(move || {
                                if let Some(client) = api_client(&auth) {
                                    let _ = client.set_now_playing(
                                        mapping.froglog_id,
                                        mapping.r#type.clone(),
                                        mapping.title.clone(),
                                        Some(started_at_iso),
                                    );
                                }
                            });
                        }
                        // Update tray to now-tracking state
                        let handle_tray = handle.clone();
                        let _ = handle.run_on_main_thread(move || {
                            let _ = update_tray_state(&handle_tray);
                        });
                    }
                },
                {
                    let session_path_end = session_path.clone();
                    let force_stopped_end = Arc::clone(&state.force_stopped_process);
                    move |process_name, mapping, duration_secs| {
                        let _ = std::fs::remove_file(&session_path_end);
                        // If force-stopped, clear the flag but skip handle_session_ended
                        // (it was already called from the tray handler)
                        let was_force_stopped = {
                            let mut fp = force_stopped_end.write().unwrap();
                            if fp.as_deref().map(|s| s.eq_ignore_ascii_case(&process_name)).unwrap_or(false) {
                                *fp = None;
                                true
                            } else {
                                false
                            }
                        };
                        if !was_force_stopped {
                            handle_session_ended(app_handle.clone(), process_name, mapping, duration_secs);
                        }
                        // Reset tray to idle state
                        let handle_tray = app_handle.clone();
                        let _ = app_handle.run_on_main_thread(move || {
                            let _ = update_tray_state(&handle_tray);
                        });
                    }
                },
                installed_games_arc,
                library_index_arc,
                {
                    let handle = app_handle_for_unmapped;
                    move |title: String, appid: String, exe_name: String, duration_secs: f64, replay_of: Option<lilypad_core::library_match::ResolvedLibraryGame>| {
                        let raw_hours = duration_secs / 3600.0;
                        let hours = { let r = (raw_hours * 100.0).round() / 100.0; if r < 0.01 { 0.01 } else { r } };
                        let replay_of = replay_of.map(|r| config::ReplayOf { id: r.id, game_type: r.game_type, title: r.title, status: r.status });
                        config::record_pending_game_submission(&appid, &title, &exe_name, hours, replay_of.clone());
                        let total_mins = (duration_secs / 60.0).round() as u64;
                        let time_str = if total_mins >= 60 { format!("{}h {}m", total_mins / 60, total_mins % 60) } else { format!("{}m", total_mins) };
                        let body = if replay_of.is_some() {
                            format!("{title} ({time_str}) is marked as finished in FrogLog. Resolve?")
                        } else {
                            format!("{title} ({time_str}) isn't in your FrogLog yet.")
                        };
                        show_pending_game_toast(&body, handle.clone());
                        let handle_tray = handle.clone();
                        let _ = handle.run_on_main_thread(move || {
                            let _ = update_tray_state(&handle_tray);
                        });
                    }
                },
                {
                    let handle = app_handle_for_already_owned;
                    move |mapping: ProcessMapping| {
                        let state = handle.state::<AppState>();
                        let auth = state.auth.read().unwrap().clone();
                        let froglog_id = mapping.froglog_id;
                        let game_type = mapping.r#type.clone();
                        if let Err(e) = config::link_process_mapping(
                            &state.process_map_arc,
                            &auth,
                            mapping.process,
                            mapping.r#type,
                            mapping.froglog_id,
                            mapping.title,
                        ) {
                            log::warn!("[LilyPad] failed to auto-link already-owned game: {e}");
                        } else {
                            let handle2 = handle.clone();
                            let _ = handle.run_on_main_thread(move || {
                                let _ = update_tray_state(&handle2);
                            });
                        }
                        // Best-effort, off this thread. Two follow-ups now that LilyPad has
                        // linked itself to this game:
                        // - It might be a Steam-bulk-imported entry that was never actually
                        //   started (status "Imported", no start_date) -- fix that up now that
                        //   it's genuinely being played. No-op if it isn't "Imported".
                        // - monitor.rs already treats this session as "session"-type locally,
                        //   so the backend needs session_tracking on too, with any pre-existing
                        //   hours preserved as a "Pre-tracked hours" session --
                        //   enable_session_tracking is idempotent, so calling it even for an
                        //   already-session-tracked game is safe (no duplicate seed session).
                        std::thread::spawn(move || {
                            if let Some(client) = api_client(&auth) {
                                if let Err(e) = client.fix_imported_status_if_needed(froglog_id, &game_type) {
                                    log::warn!("[LilyPad] failed to fix imported status: {e}");
                                }
                                if !game_type.eq_ignore_ascii_case("live") {
                                    if let Err(e) = client.enable_session_tracking(froglog_id) {
                                        log::warn!("[LilyPad] failed to enable session tracking: {e}");
                                    }
                                }
                            }
                        });
                    }
                },
            );

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
