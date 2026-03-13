mod api;
mod config;
mod monitor;

use api::FroglogClient;
use config::{AuthConfig, ProcessMapConfig, ProcessMapping, auth_config_path, process_map_path_for_auth};
use monitor::{run_poll_loop, ActiveSession};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime};
use sysinfo::{ProcessesToUpdate, System};
use tauri::menu::{Menu, MenuItemBuilder, PredefinedMenuItem};
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

const DEFAULT_HEIGHT: f64 = 740.0;
const MAIN_ABOUT_HEIGHT: f64 = 335.0;
const WINDOW_WIDTH: f64 = 642.0;
const SESSION_WIDTH: f64 = 440.0;
const SESSION_HEIGHT_REGULAR: f64 = 165.0;
const SESSION_HEIGHT_LIVE: f64 = 255.0;
const SESSION_TASKBAR_REGULAR: f64 = 94.0;
const SESSION_TASKBAR_LIVE: f64 = 94.0;

fn show_window_at_height(w: &tauri::WebviewWindow, width: f64, height: f64) {
    let _ = w.set_size(tauri::Size::Logical(tauri::LogicalSize { width, height }));
    let _ = w.center();
    let _ = w.show();
    let _ = w.set_focus();
}

fn show_session_window(w: &tauri::WebviewWindow, height: f64, taskbar_margin: f64) {
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
    let _ = w.show();
    let _ = w.set_focus();
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
                println!("[LilyPad] handle_session_ended: clearing now_playing");
                let _ = client.clear_now_playing();
            } else {
                println!("[LilyPad] handle_session_ended: no API client available to clear now_playing");
            }
        });

        if auto_submit {
            let raw_hours = duration_secs / 3600.0;
            let hours = { let r = (raw_hours * 100.0).round() / 100.0; if r < 0.01 { 0.01 } else { r } };
            let date = chrono::Local::now().format("%Y-%m-%d").to_string();
            let mapping2 = mapping.clone();
            let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
            std::thread::spawn(move || {
                let ok = if let Some(client) = api_client(&auth) {
                    println!("[LilyPad] handle_session_ended: auto-submitting hours");
                    // clear_now_playing removed from here as it runs above for all cases
                    if mapping2.r#type.eq_ignore_ascii_case("live") {
                        client.add_live_service_session(mapping2.froglog_id, Some(date), Some(hours), Some("Session auto submitted with LilyPad".to_string())).is_ok()
                    } else if mapping2.r#type.eq_ignore_ascii_case("session") {
                        client.add_game_session(mapping2.froglog_id, Some(date), Some(hours), Some("Session auto submitted with LilyPad".to_string())).is_ok()
                    } else {
                        client.update_game_hours(mapping2.froglog_id, hours).is_ok()
                    }
                } else {
                     println!("[LilyPad] handle_session_ended: no API client available");
                    false
                };
                let _ = tx.send(ok);
            });
            let submitted = rx.await.unwrap_or(false);
            if submitted {
                let title = mapping.title.as_deref().unwrap_or_else(|| mapping.process.as_str());
                let _ = app.notification().builder()
                    .title("LilyPad")
                    .body(format!("Session auto-submitted: {}", title))
                    .show();
                return;
            }
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
            if let Some(w) = app.get_webview_window("main") {
            let has_notes = mapping.r#type.eq_ignore_ascii_case("live") || mapping.r#type.eq_ignore_ascii_case("session");
            let (sh, tb) = if has_notes {
                (SESSION_HEIGHT_LIVE, SESSION_TASKBAR_LIVE)
            } else {
                (SESSION_HEIGHT_REGULAR, SESSION_TASKBAR_REGULAR)
             };
            show_session_window(&w, sh, tb);
        }
    });
}

fn build_tray_menu(app: &tauri::AppHandle, logged_in: bool, game_title: Option<String>) -> Result<Menu<Wry>, Box<dyn std::error::Error + Send + Sync>> {
    let quit_item = PredefinedMenuItem::quit(app, Some("Quit"))?;
    if let Some(ref title) = game_title {
        let status_item = MenuItemBuilder::with_id("tracking_status", format!("Now Tracking: {}", title))
            .enabled(false)
            .build(app)?;
        let force_stop_item = MenuItemBuilder::with_id("force_stop_tracking", "Stop Tracking Current Session").build(app)?;
        let logout_item = MenuItemBuilder::with_id("logout", "Logout").build(app)?;
        let menu = Menu::with_items(app, &[&status_item, &force_stop_item, &logout_item, &quit_item])?;
        Ok(menu)
    } else if logged_in {
        let assign_exes_item = MenuItemBuilder::with_id("assign_exes", "Configure...").build(app)?;
        let about_item = MenuItemBuilder::with_id("about", "About").build(app)?;
        let logout_item = MenuItemBuilder::with_id("logout", "Logout").build(app)?;
        let menu = Menu::with_items(app, &[&assign_exes_item, &about_item, &logout_item, &quit_item])?;
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
) -> Result<serde_json::Value, String> {
    let mut client = FroglogClient::new(base_url.trim_end_matches('/').to_string());
    let res = client.login(&username, &password)?;
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
    state: tauri::State<AppState>,
    game_type: String, // "regular" | "live" | "session"
    game_id: i32,
    hours: f64,
    notes: Option<String>,
) -> Result<serde_json::Value, String> {
    let auth = state.auth.read().unwrap();
    let client = api_client(&auth).ok_or("Not logged in")?;
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    if game_type.eq_ignore_ascii_case("live") {
        client.add_live_service_session(game_id, Some(date), Some(hours), notes)
    } else if game_type.eq_ignore_ascii_case("session") {
        client.add_game_session(game_id, Some(date), Some(hours), notes)
    } else {
        client.update_game_hours(game_id, hours)
    }
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
fn save_now_playing_share(state: tauri::State<AppState>, share: bool) -> Result<(), String> {
    let mut map = state.process_map_arc.read().unwrap().clone();
    map.share_now_playing = share;
    map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()))
        .map_err(|e| e.to_string())?;
    *state.process_map_arc.write().unwrap() = map;

    let auth = state.auth.read().unwrap().clone();
    std::thread::spawn(move || {
        if let Some(client) = api_client(&auth) {
            println!("[LilyPad] save_now_playing_share: updating show_current_session on FrogLog to {}", share);
            let _ = client.set_show_current_session(share);
        } else {
            println!("[LilyPad] save_now_playing_share: no API client available to update show_current_session");
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
    let path = app
        .dialog()
        .file()
        .add_filter("Executables", &["exe"])
        .blocking_pick_file();
    Ok(path.and_then(|p| p.as_path().map(|path| path.display().to_string())))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let auth = AuthConfig::load_from(&auth_config_path());
    let process_map = ProcessMapConfig::load_from(&process_map_path_for_auth(&auth));
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            process_map_arc: Arc::new(RwLock::new(process_map)),
            current_session_arc: Arc::new(RwLock::new(None)),
            auth: RwLock::new(auth),
            force_stopped_process: Arc::new(RwLock::new(None)),
        })
        .invoke_handler(tauri::generate_handler![
            login,
            logout,
            refresh_tray_menu,
            get_games,
            get_live_service_games,
            submit_session,
            get_auth_config,
            get_process_mappings,
            save_process_mapping,
            delete_process_mapping,
            save_auto_submit,
            save_now_playing_share,
            hide_window,
            set_window_size,
            open_url,
            pick_exe_file,
        ])
        .on_window_event(|window, event| {
            // Keep app running in tray: closing the main window hides it instead of exiting
            if window.label() == "main" {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
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
            let state = app.state::<AppState>();
            let config = Arc::clone(&state.process_map_arc);
            let current_session = Arc::clone(&state.current_session_arc);
            let force_stopped_process = Arc::clone(&state.force_stopped_process);

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
                        let title = mapping.title.as_deref().unwrap_or_else(|| process_name.as_str());
                        let body = format!("Tracking started: {}", title);
                        let _ = handle.notification().builder()
                            .title("LilyPad")
                            .body(body)
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
            );

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
