mod api;
mod config;
mod monitor;

use api::FroglogClient;
use config::{AuthConfig, ProcessMapConfig, auth_config_path, process_map_path_for_auth};
use monitor::{run_poll_loop, ActiveSession};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::RwLock;
use tauri::menu::{Menu, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, WindowEvent};
use tauri::Wry;
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

/// Tray icon (embedded at compile time from icons/icon.ico).
const TRAY_ICON: tauri::image::Image<'_> = tauri::include_image!("icons/icon.ico");

const DEFAULT_HEIGHT: f64 = 590.0;
const MAIN_ABOUT_HEIGHT: f64 = 320.0;
const WINDOW_WIDTH: f64 = 680.0;
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

fn build_tray_menu(app: &tauri::AppHandle, logged_in: bool) -> Result<Menu<Wry>, Box<dyn std::error::Error + Send + Sync>> {
    let about_item = MenuItemBuilder::with_id("about", "About").build(app)?;
    let quit_item = PredefinedMenuItem::quit(app, Some("Quit"))?;
    if logged_in {
        let assign_exes_item = MenuItemBuilder::with_id("assign_exes", "Configure...").build(app)?;
        let logout_item = MenuItemBuilder::with_id("logout", "Logout").build(app)?;
        let menu = Menu::with_items(app, &[&assign_exes_item, &about_item, &logout_item, &quit_item])?;
        Ok(menu)
    } else {
        let login_item = MenuItemBuilder::with_id("login", "Login").build(app)?;
        let menu = Menu::with_items(app, &[&login_item, &about_item, &quit_item])?;
        Ok(menu)
    }
}

fn update_tray_menu(app: &tauri::AppHandle) -> Result<(), String> {
    let logged_in = app.state::<AppState>().auth.read().unwrap().token.is_some();
    let menu = build_tray_menu(app, logged_in).map_err(|e| e.to_string())?;
    if let Some(tray) = app.tray_by_id("main") {
        tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
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
    // Persist and switch to this account's process map
    let mut auth = state.auth.write().unwrap();
    auth.base_url = Some(base_url.trim_end_matches('/').to_string());
    auth.token = Some(token);
    auth.save_to(&auth_config_path()).map_err(|e| e.to_string())?;
    let map_path = process_map_path_for_auth(&auth);
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

/// Refresh tray menu from current auth state. Called by frontend after login (delayed) so it runs when main thread is idle.
#[tauri::command]
fn refresh_tray_menu(app: tauri::AppHandle) -> Result<(), String> {
    let app_clone = app.clone();
    app.run_on_main_thread(move || {
        let _ = update_tray_menu(&app_clone);
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
    game_type: String, // "regular" | "live"
    game_id: i32,
    hours: f64,
    notes: Option<String>,
) -> Result<serde_json::Value, String> {
    let auth = state.auth.read().unwrap();
    let client = api_client(&auth).ok_or("Not logged in")?;
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    if game_type.eq_ignore_ascii_case("live") {
        client.add_live_service_session(game_id, Some(date), Some(hours), notes)
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
    // Remove any existing mapping for this specific game (by id+type, not exe name,
    // so multiple games can share the same exe with different title filters).
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
fn save_auto_submit(state: tauri::State<AppState>, regular: bool, live: bool) -> Result<(), String> {
    let mut map = state.process_map_arc.read().unwrap().clone();
    map.auto_submit_regular = regular;
    map.auto_submit_live = live;
    map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()))
        .map_err(|e| e.to_string())?;
    *state.process_map_arc.write().unwrap() = map;
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
            let menu = build_tray_menu(&handle, logged_in).map_err(|e| e.to_string())?;

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
                        let _ = update_tray_menu(app);
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.emit("show-login", ());
                        }
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
            run_poll_loop(
                config,
                current_session,
                Arc::new(AtomicBool::new(false)),
                2,
                {
                    let handle = app_handle.clone();
                    move |process_name, mapping| {
                        let title = mapping.title.as_deref().unwrap_or_else(|| process_name.as_str());
                        let body = format!("Tracking started: {}", title);
                        let _ = handle.notification().builder()
                            .title("LilyPad")
                            .body(body)
                            .show();
                    }
                },
                move |process_name, mapping, duration_secs| {
                    let handle = app_handle.clone();
                    tauri::async_runtime::spawn(async move {
                        let state = handle.state::<AppState>();
                        let auto_submit = {
                            let cfg = state.process_map_arc.read().unwrap();
                            if mapping.r#type.eq_ignore_ascii_case("live") { cfg.auto_submit_live } else { cfg.auto_submit_regular }
                        };
                        let auth = state.auth.read().unwrap().clone();
                        drop(state);

                        if auto_submit {
                            let raw_hours = duration_secs / 3600.0;
                            let hours = { let r = (raw_hours * 100.0).round() / 100.0; if r < 0.01 { 0.01 } else { r } };
                            let date = chrono::Local::now().format("%Y-%m-%d").to_string();
                            let mapping2 = mapping.clone();
                            let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
                            // Create the reqwest::blocking::Client inside the OS thread — creating or
                            // dropping it inside a tokio context panics ("cannot start/drop a runtime
                            // from within a runtime").
                            std::thread::spawn(move || {
                                let ok = if let Some(client) = api_client(&auth) {
                                    if mapping2.r#type.eq_ignore_ascii_case("live") {
                                        client.add_live_service_session(mapping2.froglog_id, Some(date), Some(hours), Some("Session auto submitted with LilyPad".to_string())).is_ok()
                                    } else {
                                        client.update_game_hours(mapping2.froglog_id, hours).is_ok()
                                    }
                                } else { false };
                                let _ = tx.send(ok);
                            });
                            let submitted = rx.await.unwrap_or(false);
                            if submitted {
                                let title = mapping.title.as_deref().unwrap_or_else(|| mapping.process.as_str());
                                let _ = handle.notification().builder()
                                    .title("LilyPad")
                                    .body(format!("Session auto-submitted: {}", title))
                                    .show();
                                return;
                            }
                        }

                        // Show popup (auto-submit off, or not logged in, or submission failed)
                        let _ = handle.emit("session-ended", serde_json::json!({
                            "processName": process_name,
                            "mapping": {
                                "process": mapping.process,
                                "type": mapping.r#type,
                                "froglogId": mapping.froglog_id,
                                "title": mapping.title,
                            },
                            "durationSecs": duration_secs,
                        }));
                        if let Some(w) = handle.get_webview_window("main") {
                            let is_live = mapping.r#type.eq_ignore_ascii_case("live");
                            let (sh, tb) = if is_live {
                                (SESSION_HEIGHT_LIVE, SESSION_TASKBAR_LIVE)
                            } else {
                                (SESSION_HEIGHT_REGULAR, SESSION_TASKBAR_REGULAR)
                            };
                            show_session_window(&w, sh, tb);
                        }
                    });
                },
            );

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
