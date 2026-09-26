// `session_persistence` is intentionally not imported here any more: the Tauri frontend's
// session lifecycle now runs on the durable ledger (`ledger_session`). The module stays in the
// core crate for the GTK frontend, which is not part of this cutover -- see `PLAN.md`.
use lilypad_core::{api, config, duration, ledger_session, library_match, local_games, monitor, steam};
use lilypad_core::auto_submit;
use lilypad_core::submission::{submit_play_session, explain_failure, remote_reference};
use lilypad_core::resolution;
use lilypad_core::session_store::find_original_process;

use api::FroglogClient;
// `load_pending_sessions`/`save_pending_sessions` are gone: the retry queue is now ledger
// records in `pending`, not a separate JSON file. `PendingSession` survives only as the shape
// the UI already renders, built from ledger records by `pending_sessions_for`.
use config::{AuthConfig, ExcludedApp, ProcessMapConfig, ProcessMapping, PendingSession, WatchedDirectory, auth_config_path, process_map_path_for_auth};
use library_match::LibraryIndex;
use lilypad_core::session_ledger::{
    AccountIdentity, ProcessIdentity, SessionLedger, SessionRecord, SessionTarget, Submission,
    SubmissionState,
};
use monitor::{run_poll_loop, ActiveSession};
use steam::InstalledGame;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::Mutex;
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

const DEFAULT_HEIGHT: f64 = 780.0;
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
            // Fires 150ms after the window was shown, so it lands in a different moment from the
            // calls above -- worth naming separately if it is the one that fails.
            note_window_op("re-focus window after show", handle2.set_focus());
        });
    });
}

#[cfg(not(target_os = "linux"))]
fn nudge_focus(_w: &tauri::WebviewWindow) {}

fn show_window_at_height(w: &tauri::WebviewWindow, width: f64, height: f64) {
    #[cfg(windows)]
    note_window_op("resize main window", w.set_size(tauri::Size::Logical(tauri::LogicalSize { width, height })));
    note_window_op("show main window", w.show());
    #[cfg(not(windows))]
    note_window_op("resize main window", w.set_size(tauri::Size::Logical(tauri::LogicalSize { width, height })));
    note_window_op("centre main window", w.center());
    note_window_op("focus main window", w.set_focus());
    nudge_focus(w);
    apply_theme(w);
}

fn show_session_window(w: &tauri::WebviewWindow, height: f64, taskbar_margin: f64) {
    #[cfg(windows)]
    note_window_op("resize session window", w.set_size(tauri::Size::Logical(tauri::LogicalSize { width: SESSION_WIDTH, height })));
    note_window_op("show session window", w.show());
    #[cfg(not(windows))]
    note_window_op("resize session window", w.set_size(tauri::Size::Logical(tauri::LogicalSize { width: SESSION_WIDTH, height })));
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
        note_window_op("position session window", w.set_position(tauri::Position::Physical(tauri::PhysicalPosition { x, y })));
    }
    note_window_op("focus session window", w.set_focus());
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
/// Reports a window or webview call that failed, instead of discarding it.
///
/// These were all `let _ = ...`, which meant an intermittent WebView2 fault — the window opening
/// to a blank white page until LilyPad was restarted — left nothing in the log but wry's own
/// `WebView2 error: … 0x8007139F` line, with no indication of which operation provoked it.
/// `0x8007139F` is `ERROR_INVALID_STATE`: the webview was not in a usable state for the call.
///
/// Returns whether it succeeded, so a caller can react rather than only record.
fn note_window_op(what: &str, result: tauri::Result<()>) -> bool {
    match result {
        Ok(()) => true,
        Err(e) => {
            log::warn!("[LilyPad] window operation '{what}' failed: {e}");
            false
        }
    }
}

/// Last resort when the webview rejects a script: reload the page.
///
/// A webview that refuses `eval` is the same one that shows the user a blank window, and until
/// now the only way out was restarting LilyPad. Reloading re-navigates it, which is the standard
/// recovery from a blanked WebView2.
///
/// Losing the page's JavaScript state is acceptable *here specifically* because this only runs
/// once the webview has already failed — the state was unreachable anyway. It is deliberately not
/// called speculatively for that reason.
fn recover_webview(w: &tauri::WebviewWindow) {
    log::warn!("[LilyPad] the webview rejected a script; reloading it to clear a blank window");
    if let Err(e) = w.reload() {
        log::error!(
            "[LilyPad] could not reload the webview ({e}); the window may stay blank until \
             LilyPad is restarted"
        );
    }
}

fn apply_theme(w: &tauri::WebviewWindow) {
    let is_dark = linux_prefers_dark()
        .unwrap_or_else(|| w.theme().unwrap_or(tauri::Theme::Light) == tauri::Theme::Dark);
    let theme_str = if is_dark { "dark" } else { "light" };
    let script = format!(
        "document.documentElement.setAttribute('data-theme', '{}')",
        theme_str
    );
    // The first webview call made whenever the window is shown, so it is where a webview that
    // went bad while hidden is noticed.
    if !note_window_op("apply theme", w.eval(&script)) {
        recover_webview(w);
    }
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
    /// Durable session store (see `PLAN.md` phase 1). `None` only when the store could not be
    /// opened at all -- that is a real failure, reported through `storage_error` rather than
    /// being treated as "no sessions", so a broken store never looks like an empty queue.
    ledger: Option<Arc<Mutex<SessionLedger>>>,
    /// Last persistence failure, surfaced to the UI by the `session_storage_error` command and
    /// the `storage-error` event. Cleared when a later write succeeds.
    storage_error: Arc<RwLock<Option<String>>>,
    /// Which games each install location held as of the last scan — Steam manifest names and
    /// watched-directory folder names. Lets a frequent check cost a directory listing instead of
    /// parsing every manifest — see `local_games::install_locations_fingerprint`.
    ///
    /// `None` until the first scan of this run. It has to be distinguishable from "scanned, and
    /// found nothing installed": an empty `Vec` baseline made the first check of every run read as
    /// a change and log one, which is simply the startup scan, not an install.
    install_fingerprint: Arc<RwLock<Option<Vec<(std::path::PathBuf, Vec<String>)>>>>,
    /// Whether the cached library is known to be out of date because its last refresh failed.
    /// Reported by `session_storage_status` so "this game looks new" can be understood as
    /// possibly meaning "we could not check".
    library_stale: Arc<RwLock<bool>>,
    /// Ledger id of the session currently being tracked, tied to its process so a stale waiter
    /// cannot take a later session's record. Held here rather than on `ActiveSession` so
    /// `monitor::run_poll_loop`'s signature, shared with the GTK frontend, stays untouched.
    active_ledger_id: lilypad_core::session_store::ActiveRecord,
    /// Ledger ids of unmapped games currently being played, keyed by appid. A map rather than a
    /// single slot because several unmapped games can be tracked at once — the monitor keys that
    /// on appid too (`currently_tracking_unmapped`).
    active_unmapped_ledger_ids: Arc<RwLock<std::collections::HashMap<String, String>>>,
    /// Ledger id of the most recently completed session, kept so the post-play window's
    /// submission can acknowledge the exact record it came from. The window normally passes
    /// the id back itself (it is in the `session-ended` payload); this is the fallback for a
    /// window opened before this build, or reopened after a restart.
    last_finished_ledger_id: Arc<RwLock<Option<String>>>,
}

const DEFAULT_API_URL: &str = "https://api.froglog.co.uk/api";

/// How often to check whether anything has been installed or removed. Cheap because the scan
/// itself is gated on directory mtimes -- see `refresh_installed_games_if_changed`.
const INSTALL_SCAN_INTERVAL: Duration = Duration::from_secs(10);

fn api_client(auth: &AuthConfig) -> Option<FroglogClient> {
    let base = auth
        .base_url
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_API_URL);
    let mut c = FroglogClient::new(base.to_string());
    c.set_token(auth.token.clone());
    Some(c)
}

impl AppState {
    fn account(&self) -> Option<AccountIdentity> {
        let auth = self.auth.read().ok()?;
        ledger_session::account_identity(&auth, DEFAULT_API_URL)
    }

    /// The shared store operations over this state's own ledger and error slot.
    fn store(&self) -> lilypad_core::session_store::SessionStore {
        lilypad_core::session_store::SessionStore::from_shared(
            self.ledger.clone(),
            Arc::clone(&self.storage_error),
            DEFAULT_API_URL,
        )
    }

    /// Runs `op` against the ledger, recording any failure for the UI. Returns `None` when the
    /// store is unavailable or the operation failed -- callers must treat that as "unknown",
    /// never as "nothing there".
    fn with_ledger<T>(
        &self,
        what: &str,
        op: impl FnOnce(&mut SessionLedger) -> lilypad_core::session_ledger::LedgerResult<T>,
    ) -> Option<T> {
        let ledger = match &self.ledger {
            Some(l) => l,
            None => return None,
        };
        let result = ledger
            .lock()
            .map_err(|e| e.to_string())
            .and_then(|mut l| op(&mut l).map_err(|e| e.to_string()));
        match result {
            Ok(value) => {
                *self.storage_error.write().unwrap() = None;
                Some(value)
            }
            Err(e) => {
                log::error!("[LilyPad] session store: {what} failed: {e}");
                *self.storage_error.write().unwrap() = Some(format!("{what} failed: {e}"));
                None
            }
        }
    }
}

/// Claims sole ownership of this user's LilyPad session, returning whether it was free.
///
/// Two LilyPads running at once is not merely untidy: each runs its own process monitor, so the
/// same launch is detected twice, two sessions are recorded for one sitting, two heartbeats
/// refresh presence, and both write the same SQLite ledger. WAL keeps the database *consistent*
/// under that, but consistent duplicates are still duplicates. The GTK frontend never had this
/// problem — GTK's application identity gives it single-instance behaviour for free — so this is
/// the Windows equivalent rather than a new idea.
///
/// The mutex is deliberately session-local (no `Global\` prefix): two different Windows users
/// signed in at once are two different people with two different FrogLog accounts, and each
/// should get their own LilyPad.
///
/// The handle is simply never closed, which is what keeps the mutex held for the life of the
/// process. `HANDLE` is a `Copy` type with no `Drop`, so letting it fall out of scope closes
/// nothing; Windows releases it when the process exits.
#[cfg(windows)]
fn claim_single_instance() -> bool {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    unsafe {
        match CreateMutexW(None, false, &HSTRING::from("LilyPad.FrogLog.SingleInstance")) {
            Ok(_handle) => GetLastError() != ERROR_ALREADY_EXISTS,
            // If the guard itself cannot be created, start anyway. Refusing to run because a
            // safeguard failed would turn a rare OS hiccup into "LilyPad will not open".
            Err(e) => {
                log::warn!("[LilyPad] could not create the single-instance guard ({e}); starting anyway");
                true
            }
        }
    }
}

#[cfg(not(windows))]
fn claim_single_instance() -> bool {
    true
}

/// Tells the user why the second instance is closing. A tray app that exits silently looks
/// broken — they double-clicked something and apparently nothing happened.
#[cfg(windows)]
fn report_already_running() {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from("LilyPad is already running."),
            &HSTRING::from("LilyPad"),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

#[cfg(not(windows))]
fn report_already_running() {}

/// Reports a storage failure to the UI as well as the log. Tray/window code calls this from
/// paths that have an `AppHandle` but no `State`.
fn report_storage_error(app: &tauri::AppHandle, message: String) {
    log::error!("[LilyPad] session store: {message}");
    {
        let state = app.state::<AppState>();
        *state.storage_error.write().unwrap() = Some(message.clone());
    }
    let _ = app.emit("storage-error", message);
}

/// Opens the durable store and migrates the one legacy file this build has stopped writing.
///
/// A failure here is returned, never swallowed: an unopenable or malformed store must not be
/// mistaken for "no sessions recorded". The import is transactional, leaves the original JSON
/// in place, and runs at most once per file.
///
/// Diagnostics are *returned* rather than logged, because this runs before Tauri's log plugin
/// is installed -- anything logged here would go nowhere, which previously meant a store that
/// failed to open said so only in the UI and left no trace in the log file. The caller replays
/// them with `replay_startup_log` once logging exists.
fn open_session_ledger() -> (Result<SessionLedger, String>, Vec<(log::Level, String)>) {
    // All three legacy files are cut over, so the shared opener imports the full set.
    lilypad_core::session_store::open_ledger_with_import(
        &lilypad_core::session_ledger::ledger_path(),
        &config::app_data_dir(),
    )
}

/// Backfills `auth.username` for logins that predate it being stored (pre-v0.4.4) and moves that
/// install's process-map file onto the username key.
///
/// Without this such an install has no stable account key, which previously meant no ledger
/// records at all and now means a token-hash key that changes whenever the token does. The
/// obvious manual workaround — log out and back in — is actively harmful: logout resets
/// `AuthConfig` to default, so the token-key -> username-key copy in `login` sees the *anonymous*
/// path as the old one and deliberately skips, orphaning the user's mappings.
///
/// One network call, made only when the username is genuinely missing. Failure is not fatal: the
/// install keeps working on its token-hash key and this retries next launch. The old map file is
/// copied, never moved, so a bad outcome is recoverable by hand.
fn backfill_username(auth: &mut AuthConfig, messages: &mut Vec<(log::Level, String)>) {
    if auth.username.is_some() || auth.token.is_none() {
        return;
    }
    let Some(client) = api_client(auth) else { return };
    let username = match client.get_username() {
        Ok(username) => username,
        Err(e) => {
            messages.push((
                log::Level::Warn,
                format!("[LilyPad] could not determine this account's username ({e}); \
                         sessions will be keyed by token until this succeeds"),
            ));
            return;
        }
    };

    let old_map_path = process_map_path_for_auth(auth);
    auth.username = Some(username.clone());
    let new_map_path = process_map_path_for_auth(auth);
    if old_map_path != new_map_path && old_map_path.exists() && !new_map_path.exists() {
        match std::fs::copy(&old_map_path, &new_map_path) {
            Ok(_) => messages.push((
                log::Level::Info,
                format!("[LilyPad] moved mappings onto the username key ({} -> {})",
                    old_map_path.display(), new_map_path.display()),
            )),
            Err(e) => {
                // Do not keep a username we cannot carry the mappings across to: staying on the
                // token key preserves them, and this retries next launch.
                auth.username = None;
                messages.push((
                    log::Level::Error,
                    format!("[LilyPad] could not copy mappings to the username key ({e}); \
                             staying on the token key to keep them"),
                ));
                return;
            }
        }
    }
    if let Err(e) = auth.save_to(&auth_config_path()) {
        auth.username = None;
        messages.push((
            log::Level::Error,
            format!("[LilyPad] could not save the backfilled username ({e})"),
        ));
        return;
    }
    messages.push((
        log::Level::Info,
        format!("[LilyPad] backfilled the account username for this install ({username})"),
    ));
}

/// Emits messages produced before the log plugin was installed.
fn replay_startup_log(messages: Vec<(log::Level, String)>) {
    for (level, message) in messages {
        log::log!(level, "{message}");
    }
}

/// Records a newly detected session and returns its ledger id, or `None` if it could not be
/// stored. A `None` here means this session is *not* crash-recoverable; the caller carries on
/// tracking it in memory (losing the session outright would be worse) but the failure is
/// already logged and surfaced by `with_ledger`.
fn begin_ledger_session(
    state: &AppState,
    process_name: &str,
    mapping: &ProcessMapping,
) -> Option<String> {
    let account = state.account()?;
    // The monitor stores the session before invoking this callback, so the exact pid and its
    // OS start time are readable here without widening `run_poll_loop`'s callback signature
    // (which is shared with the GTK frontend). Recording both is what lets recovery tell the
    // instance we were tracking from a relaunch that reused its pid.
    let (pid, started_at_secs) = state
        .current_session_arc
        .read()
        .ok()
        .and_then(|s| s.as_ref().map(|s| (Some(u32::try_from(usize::from(s.pid)).unwrap_or_default()), s.started_at_secs)))
        .unwrap_or((None, None));
    let record = SessionRecord::new(
        account,
        ProcessIdentity {
            executable: process_name.to_string(),
            pid,
            started_at_secs,
            ..Default::default()
        },
        SessionTarget::Mapped(mapping.clone()),
        ledger_session::now_secs(),
    );
    let id = record.id.clone();
    state.with_ledger("recording session start", |l| {
        l.insert(&record, SubmissionState::Active)
    })?;
    log::info!(
        "[LilyPad] session {} started: {} -> {} #{}",
        id, process_name, mapping.r#type, mapping.froglog_id
    );
    Some(id)
}

/// Marks the tracked session complete and hands back its id, so the submission path can
/// acknowledge exactly that record. Completion is persisted *before* any notification or
/// network call, so a crash between the game exiting and the session being submitted leaves a
/// pending record rather than nothing.
fn finish_ledger_session(state: &AppState, ended_at: u64) -> Option<String> {
    let id = state.active_ledger_id.take()?;
    state.with_ledger("recording session end", |l| l.finish(&id, ended_at))?;
    *state.last_finished_ledger_id.write().unwrap() = Some(id.clone());
    Some(id)
}

/// The ledger id a submission should settle: the one the UI passed back with the session it is
/// submitting, falling back to the last session this process completed.
fn submission_ledger_id(state: &AppState, from_ui: Option<String>) -> Option<String> {
    from_ui
        .filter(|s| !s.is_empty())
        .or_else(|| state.last_finished_ledger_id.read().unwrap().clone())
}

/// Whether a waiter that has just seen its process exit is the one entitled to end the session.
enum Completion {
    /// This waiter owns the end of the session; emit the end event against this record.
    Owned(Option<String>),
    /// Something else already completed this session -- a force-stop, or a second waiter for
    /// the same process. Emitting again would show a duplicate post-play popup and, if the user
    /// submitted it, log a duplicate session on the server.
    Stale,
}

/// Closes a specific durable record on behalf of the waiter that observed the process exit.
///
/// `finish` only applies to a record that is still `active`, which is exactly the stale-worker
/// test: if it reports no such active record, some other path got there first. A storage
/// failure is *not* treated as stale -- losing a real session would be worse than risking a
/// duplicate prompt -- so it degrades to owning the end with no record to settle.
fn complete_session(state: &AppState, id: &str, ended_at: u64) -> Completion {
    match state.with_ledger("recording session end", |l| l.finish(id, ended_at)) {
        Some(true) => {
            *state.last_finished_ledger_id.write().unwrap() = Some(id.to_string());
            Completion::Owned(Some(id.to_string()))
        }
        Some(false) => Completion::Stale,
        None => Completion::Owned(None),
    }
}

/// Removes a mapping whose game turned out not to exist, and re-reads the library.
///
/// The auto-link path builds a mapping from the cached library index. If the game was deleted on
/// the website inside the refresh window, the index still lists it, so LilyPad links to an id the
/// server will 404 on — and then keeps that mapping for ever, tracking every future launch
/// against a dead entry whose post-play popup has nowhere to submit.
///
/// Dropping the mapping is the conservative half: the next launch re-resolves from a refreshed
/// library and either finds the real entry or treats the game as new, which is what should have
/// happened. The session already in flight still fails and queues; it can be discarded from
/// Pending Submissions, and it is not silently lost.
fn drop_dead_mapping(app: &tauri::AppHandle, process: &str, game_type: &str, froglog_id: i32) {
    log::warn!(
        "[LilyPad] game {froglog_id} no longer exists; removing the mapping for {process} that was \
         auto-linked to it and refreshing the library"
    );
    unlink_dead_mapping(app, process, game_type, froglog_id);
    // The cached library is demonstrably out of date -- it just claimed a game that is gone.
    refresh_library_index_state(&app.state::<AppState>());
}

/// Removes a mapping whose game no longer exists (in memory at once, then saved), so the next
/// launch is detected as a game not in FrogLog rather than tracked against a dead id.
fn unlink_dead_mapping(app: &tauri::AppHandle, process: &str, game_type: &str, froglog_id: i32) {
    let state = app.state::<AppState>();
    let auth = state.auth.read().unwrap().clone();
    if let Err(e) = config::remove_dead_mapping(&state.process_map_arc, &auth, process, game_type, froglog_id) {
        log::warn!("[LilyPad] could not save the process map after removing a dead mapping: {e}");
    }
}

/// Queues a session for retry after a failed submission.
///
/// The ledger record produced when the session ended *is* the queue entry — this only attaches
/// what to send and why it failed. `pending-sessions.json` used to hold a separate, unrelated
/// row, so nothing could tell whether a given session had ever been sent.
///
/// If no record exists — storage unavailable, or a login with no username to own a session — one
/// is inserted here rather than dropping the submission. An unowned record cannot be submitted
/// until adopted, which the UI reports, but it is never silently lost.
fn queue_failed_submission(
    app: &tauri::AppHandle,
    ledger_id: Option<&str>,
    submission: Submission,
) {
    let state = app.state::<AppState>();
    if let Some(id) = ledger_id {
        match state.with_ledger("queueing a failed submission", |l| l.set_submission(id, &submission)) {
            Some(true) => {
                log::info!(
                    "[LilyPad] session {id} queued for retry ({}): {}",
                    submission.title,
                    submission.last_error.as_deref().unwrap_or("no error recorded")
                );
                return;
            }
            // Not pending: already acknowledged or dismissed. Re-queueing would resubmit a
            // session the server has already accepted.
            Some(false) => {
                log::warn!("[LilyPad] session {id} is not pending; not queueing it again");
                return;
            }
            None => return,
        }
    }

    let record = SessionRecord {
        id: lilypad_core::session_ledger::new_session_id(),
        account: state.account(),
        process: ProcessIdentity::default(),
        target: SessionTarget::LegacyPending(config::PendingSession {
            id: String::new(),
            game_id: submission.game_id,
            game_type: submission.game_type.clone(),
            title: submission.title.clone(),
            hours: submission.hours,
            notes: submission.notes.clone(),
            spoiler: submission.spoiler,
            is_public: submission.is_public,
            date: submission.date.clone(),
            failed_at: submission.failed_at.clone().unwrap_or_default(),
            error: submission.last_error.clone().unwrap_or_default(),
        }),
        started_at_secs: None,
        last_alive_secs: None,
        ended_at_secs: Some(ledger_session::now_secs()),
        recovered: false,
        submission: Some(submission),
    };
    log::warn!(
        "[LilyPad] a submission failed with no session record behind it; queueing it as {}",
        record.id
    );
    state.with_ledger("queueing an orphaned submission", |l| {
        l.insert(&record, SubmissionState::Pending)
    });
}

/// Saves what is about to be sent for a session before sending it, so a crash or lost response
/// mid-request leaves a retry row with the exact payload (notes, privacy, date) rather than one
/// reconstructed without them. Best-effort: a failed save must not stop the submission.
fn save_attempt(app: &tauri::AppHandle, ledger_id: Option<&str>, submission: Submission) {
    if let Some(id) = ledger_id {
        app.state::<AppState>().store().save_attempt(id, &submission);
    }
}

/// Consumes the force-stop block for `process_name` if one is set, returning whether it was.
///
/// The flag both suppresses the duplicate end event for a session the user already stopped by
/// hand, and stops the monitor re-tracking a process that is still running. It must be cleared
/// by whichever waiter sees the process actually exit, or that game stays untrackable for the
/// rest of the run.
fn take_force_stop(state: &AppState, process_name: &str) -> bool {
    let mut flag = state.force_stopped_process.write().unwrap();
    if flag.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(process_name)) {
        *flag = None;
        true
    } else {
        false
    }
}

/// Records the server's acknowledgement of a submitted session. The local row is kept, not
/// deleted, so a submitted session remains auditable and can never be resubmitted.
fn acknowledge_ledger_session(app: &tauri::AppHandle, id: &str, remote_id: &str) {
    let state = app.state::<AppState>();
    let Some(account) = state.account() else {
        report_storage_error(
            app,
            format!("session {id} submitted but no account identity was available to record it"),
        );
        return;
    };
    match state.with_ledger("recording submission", |l| {
        l.acknowledge(id, &account, remote_id)
    }) {
        Some(true) => {
            log::info!("[LilyPad] session {id} acknowledged as remote {remote_id}");
            // A session counts towards the tray's pending total from the moment it ends, since
            // that is when its record becomes `pending` -- and the end-of-session tray rebuild
            // happens before submission has resolved. Acknowledging is what takes it back out,
            // so the tray has to be told here; otherwise "Pending Submissions (1)" sticks until
            // the next restart, which is exactly what it did.
            let app = app.clone();
            let _ = app.clone().run_on_main_thread(move || {
                let _ = update_tray_state(&app);
            });
        }
        // No pending record under that id. Expected for a queue entry created before this
        // build, which is keyed by a timestamp and has no ledger record behind it.
        Some(false) => log::info!("[LilyPad] no pending session record for {id}; nothing to acknowledge"),
        None => {}
    }
}

/// Resolves every session left `active` in the store at startup.
///
/// Only the first still-running game is resumed: LilyPad tracks one session at a time, and an
/// explicit policy for overlapping games is `PLAN.md` phase 3 item 5. Any further still-running
/// record is closed at its last checkpoint rather than being left `active` forever, where it
/// would be re-resolved on every subsequent startup.
fn recover_interrupted_sessions(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let Some(account) = state.account() else {
        return;
    };
    let Some(interrupted) = state.with_ledger("reading interrupted sessions", |l| {
        l.interrupted(Some(&account))
    }) else {
        return;
    };
    if interrupted.is_empty() {
        return;
    }
    log::info!(
        "[LilyPad] session store: resolving {} interrupted session(s)",
        interrupted.len()
    );

    let mut sys = System::new_all();
    sys.refresh_processes(ProcessesToUpdate::All);
    let mut resumed = false;

    for stored in interrupted {
        let record = stored.record;
        // An unmapped game has no library entry to submit against, so it is credited into the
        // new-games queue instead -- the same place a normally-ended one goes. This is what the
        // start-of-play record exists for: without it the whole session was simply lost.
        if let SessionTarget::Unmapped { appid, title, .. } = record.target.clone() {
            recover_unmapped_session(app, &record, &appid, &title);
            continue;
        }
        let SessionTarget::Mapped(mapping) = record.target.clone() else {
            // Anything else sitting in `active` has nothing to submit against, so close it
            // rather than guessing a game id, and rather than leaving it to be re-resolved on
            // every subsequent startup.
            log::warn!(
                "[LilyPad] session {} was active with no target to resolve; closing it",
                record.id
            );
            let ended_at = record
                .last_alive_secs
                .or(record.started_at_secs)
                .unwrap_or_else(ledger_session::now_secs);
            state.with_ledger("closing an unresolvable interrupted session", |l| {
                l.finish(&record.id, ended_at)
            });
            continue;
        };
        let running_pid = (!resumed)
            .then(|| find_original_process(&sys, &record.process))
            .flatten();

        match running_pid {
            Some(pid) => {
                resumed = true;
                resume_recovered_session(app, &record, &mapping, pid);
            }
            None => close_recovered_session(app, &record, &mapping),
        }
    }
}

/// Credits an interrupted unmapped session into the new-games queue, bounded by its last
/// confirmed-alive checkpoint.
///
/// The session is not resumed even if the game is still running. The monitor re-detects it on
/// its own and opens a fresh record, so resuming here would track it twice; the two records then
/// cover different spans, and the gap while LilyPad was down is correctly excluded from both.
fn recover_unmapped_session(
    app: &tauri::AppHandle,
    record: &SessionRecord,
    appid: &str,
    title: &str,
) {
    let state = app.state::<AppState>();
    if let Some(Some(total)) = state.with_ledger("recovering an unmapped session", |l| {
        l.complete_unmapped(&record.id, ledger_session::now_secs(), true)
    }) {
        log::info!("[LilyPad] recovered {title} ({appid}) through its last checkpoint; {total}h awaiting resolution");
    }
}
/// The game outlived LilyPad: carry on tracking the same ledger record rather than opening a
/// second one, so the recovered session keeps its original start time and id.
fn resume_recovered_session(
    app: &tauri::AppHandle,
    record: &SessionRecord,
    mapping: &ProcessMapping,
    pid: sysinfo::Pid,
) {
    let started_secs = record.started_at_secs.unwrap_or_else(ledger_session::now_secs);
    let started_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(started_secs);
    let elapsed = SystemTime::now()
        .duration_since(started_wall)
        .unwrap_or_default();
    let started_at = Instant::now()
        .checked_sub(elapsed)
        .unwrap_or_else(Instant::now);

    log::info!(
        "[LilyPad] session {} resumed: {} still running as pid {}",
        record.id, record.process.executable, pid
    );

    let state = app.state::<AppState>();
    *state.current_session_arc.write().unwrap() = Some(ActiveSession {
        process_name: record.process.executable.clone(),
        mapping: mapping.clone(),
        pid,
        // Carried from the original record rather than re-read: this is the same process
        // instance, and `find_original_process` has just confirmed its start time matches.
        started_at_secs: record.process.started_at_secs,
        started_at,
    });
    state.active_ledger_id.set(&record.process.executable, Some(record.id.clone()));
    let _ = update_tray_state(app);

    let started_at_iso = chrono::DateTime::<chrono::Utc>::from(started_wall).to_rfc3339();
    start_session_heartbeat(
        app,
        Some(record.id.clone()),
        record.process.executable.clone(),
        mapping.clone(),
        started_at_iso,
    );

    let app = app.clone();
    let process_name = record.process.executable.clone();
    let mapping = mapping.clone();
    let session_id = record.id.clone();
    std::thread::spawn(move || {
        monitor::wait_for_exit_with_relaunch_grace(pid, &process_name);
        let duration_secs = SystemTime::now()
            .duration_since(started_wall)
            .unwrap_or_default()
            .as_secs_f64();
        let state = app.state::<AppState>();
        // Only our own session: after a force-stop another game may be tracked by now.
        monitor::clear_session_for(&state.current_session_arc, &process_name);
        state.active_ledger_id.clear_if(&session_id);

        // The process has really exited now, so release any force-stop block on it. Nothing
        // else will: the monitor never had a waiter for a recovered session, and its own end
        // callback -- which is what normally clears this -- therefore never fires.
        let was_force_stopped = take_force_stop(&state, &process_name);

        match complete_session(&state, &session_id, ledger_session::now_secs()) {
            Completion::Owned(ledger_id) => {
                handle_session_ended(app.clone(), process_name, mapping, duration_secs, ledger_id)
            }
            // Already completed elsewhere -- a force-stop, which has shown its own post-play
            // popup and may already have submitted. A second end event here would prompt again
            // and log the session twice.
            Completion::Stale => log::info!(
                "[LilyPad] session {session_id} already completed{}; \
                 not emitting a second end event for {process_name}",
                if was_force_stopped { " by force-stop" } else { "" }
            ),
        }
    });
}

/// The game was already gone when LilyPad came back. Credit the session up to its last
/// confirmed-alive checkpoint and submit it through the normal end-of-session path.
fn close_recovered_session(
    app: &tauri::AppHandle,
    record: &SessionRecord,
    mapping: &ProcessMapping,
) {
    let duration_secs = ledger_session::interrupted_duration_secs(record);
    let ended_at = record
        .last_alive_secs
        .or(record.started_at_secs)
        .unwrap_or_else(ledger_session::now_secs);

    let state = app.state::<AppState>();
    if state
        .with_ledger("closing interrupted session", |l| l.finish(&record.id, ended_at))
        .is_none()
    {
        return;
    }
    log::info!(
        "[LilyPad] session {} closed on recovery: {:.0}s credited up to last checkpoint",
        record.id, duration_secs
    );
    handle_session_ended(
        app.clone(),
        record.process.executable.clone(),
        mapping.clone(),
        duration_secs,
        Some(record.id.clone()),
    );
}

/// Starts the per-session ticker that keeps the crash-recovery checkpoint (and, when enabled,
/// remote presence) fresh. Storage failures inside the ticker reach the UI like any other.
///
/// Started for every tracked session, including one with no durable record behind it (an
/// unopenable store, or a login with no username to own it). Presence has nothing to do with
/// the store, and skipping the ticker in that case would quietly drop the user off "Online Now"
/// for the whole session. Without a record the ticker falls back to watching the in-memory
/// session for its stop condition.
fn start_session_heartbeat(
    app: &tauri::AppHandle,
    session_id: Option<String>,
    process_name: String,
    mapping: ProcessMapping,
    started_at_iso: String,
) {
    let state = app.state::<AppState>();
    let ledger = state.ledger.clone();
    let auth = state.auth.read().unwrap().clone();
    let process_map = Arc::clone(&state.process_map_arc);
    let current_session = Arc::clone(&state.current_session_arc);
    let app = app.clone();
    ledger_session::spawn_ledger_heartbeat(
        ledger,
        session_id,
        move || api_client(&auth).unwrap_or_else(|| FroglogClient::new(String::new())),
        process_map,
        Some(mapping),
        started_at_iso,
        move || {
            current_session
                .read()
                .map(|s| s.as_ref().is_some_and(|s| s.process_name == process_name))
                .unwrap_or(false)
        },
        move |e| report_storage_error(&app, e),
    );
}

/// Checkpoint-only heartbeat for an unmapped session. Stops when the record is no longer
/// active, which `finish_unmapped_session` brings about at end of play.
fn start_unmapped_heartbeat(app: &tauri::AppHandle, session_id: String, exe_name: String) {
    let state = app.state::<AppState>();
    let Some(ledger) = state.ledger.clone() else { return };
    let auth = state.auth.read().unwrap().clone();
    let process_map = Arc::clone(&state.process_map_arc);
    let app = app.clone();
    ledger_session::spawn_ledger_heartbeat(
        Some(ledger),
        Some(session_id),
        move || api_client(&auth).unwrap_or_else(|| FroglogClient::new(String::new())),
        process_map,
        None,
        String::new(),
        // Unused while a record exists: the ledger's own state is the stop condition. Kept
        // truthful rather than a bare `true` so it cannot mislead if that ever changes.
        move || !exe_name.is_empty(),
        move |e| report_storage_error(&app, e),
    );
}

/// Re-scans Steam's installed games and every configured watched directory, replacing
/// `state.installed_games_arc` in one go. Called both by the periodic background refresh and
/// immediately after adding/removing a watched directory, so a newly added folder is picked up
/// right away instead of waiting for the next scheduled scan.
///
/// Records the fingerprint this scan reflects, so an unconditional scan here also satisfies the
/// next gated check instead of leaving it to find a difference and repeat the work.
fn refresh_installed_games(state: &AppState) {
    let steam_root = steam::find_steam_root();
    let (watched_dirs, excluded_appids): (Vec<String>, std::collections::HashSet<String>) = {
        let cfg = state.process_map_arc.read().unwrap();
        (
            cfg.watched_directories.iter().map(|w| w.path.clone()).collect(),
            cfg.excluded_apps.iter().map(|e| e.appid.clone()).collect(),
        )
    };
    // Taken before the scan, not after: a game installed while the scan runs is then still a
    // difference on the next check rather than being credited to this scan and missed.
    let fingerprint = local_games::install_locations_fingerprint(steam_root.as_deref(), &watched_dirs);
    let mut games = steam_root
        .as_deref()
        .map(steam::scan_installed_games)
        .unwrap_or_default();
    games.extend(local_games::scan_watched_directories(&watched_dirs));
    // User-excluded apps (see `config::ExcludedApp`) -- e.g. Wallpaper Engine, which manifests
    // exactly like a real Steam game but obviously isn't one. Filtered here rather than in
    // `scan_installed_games` itself so the scan stays a pure "what does Steam say is installed"
    // function; this is where per-user preference gets applied on top of it.
    games.retain(|g| !excluded_appids.contains(&g.appid));
    *state.installed_games_arc.write().unwrap() = games;
    *state.install_fingerprint.write().unwrap() = Some(fingerprint);
}

/// Re-fetches games/wishlist/live-service from FrogLog and rebuilds `state.library_index_arc`.
/// Called both by the periodic background refresh and on-demand (see the
/// `refresh_library_index` command) whenever the Configure view opens, since it already fetches
/// this same data for its own display and the shared index otherwise sits stale for up to 5
/// minutes after a change made directly on the FrogLog website.
/// Rescans installed games only when the set of installed games has actually changed.
///
/// The full scan parses every Steam manifest, so running it every few seconds is wasteful; the
/// directory listing it is gated on reads no file contents. That makes it affordable to check
/// often, which is the point: a game installed and launched inside the periodic refresh window
/// used to match nothing and simply not be tracked, because it was absent from the
/// installed-games list. Now it is picked up within seconds of Steam writing its manifest.
fn refresh_installed_games_if_changed(state: &AppState) {
    let (steam_root, watched) = {
        let cfg = state.process_map_arc.read().unwrap();
        (
            steam::find_steam_root(),
            cfg.watched_directories.iter().map(|w| w.path.clone()).collect::<Vec<_>>(),
        )
    };
    let fingerprint = local_games::install_locations_fingerprint(steam_root.as_deref(), &watched);
    // Cloned out so the guard is dropped before `refresh_installed_games` takes the write lock.
    let previous = state.install_fingerprint.read().unwrap().clone();
    match previous {
        Some(prev) if prev == fingerprint => return,
        // Nothing has "changed" on the first check of a run -- there was nothing to change from.
        None => log::info!("[LilyPad] scanning installed games"),
        Some(_) => log::info!("[LilyPad] a game install location changed; rescanning installed games"),
    }
    refresh_installed_games(state);
}

/// Rebuilds the cached view of the user's library, or leaves the previous one in place.
///
/// Every fetch must succeed. Previously each was `unwrap_or_default()`, so a failed request
/// produced an *empty* library index and replaced a perfectly good one with it — at which point
/// every game the user owns reads as unknown and gets filed as a New Game. Phase 5's "offline
/// refresh cannot turn known games into false 'new game' detections" is precisely this.
///
/// The risk grew when `maybe_start_unmapped_tracking` began refreshing on demand immediately
/// before deciding a game is unlisted (phase 3 item 4): what had been a five-minute window became
/// a deterministic outcome of being offline at exactly the wrong moment.
///
/// A stale index is strictly better than an empty one. It can only cause a game added *since* the
/// last good refresh to be treated as new, which is the situation before any refresh existed.
fn refresh_library_index_state(state: &AppState) -> bool {
    let auth = state.auth.read().unwrap().clone();
    let Some(client) = api_client(&auth) else { return false };
    let (games, wishlist, live_service) = match (
        client.get_games(),
        client.get_wishlist(),
        client.get_live_service_games(),
    ) {
        (Ok(games), Ok(wishlist), Ok(live_service)) => (games, wishlist, live_service),
        (games, wishlist, live_service) => {
            let reason = games
                .err()
                .or_else(|| wishlist.err())
                .or_else(|| live_service.err())
                .unwrap_or_else(|| "unknown error".to_string());
            log::warn!(
                "[LilyPad] could not refresh the library ({reason}); keeping the previous copy \
                 rather than treating every owned game as new"
            );
            *state.library_stale.write().unwrap() = true;
            return false;
        }
    };
    *state.library_index_arc.write().unwrap() = LibraryIndex::build(&games, &wishlist, &live_service);
    *state.library_stale.write().unwrap() = false;
    true
}

/// Escapes text interpolated into a Windows toast's XML payload. Game titles (arbitrary
/// user/IGDB text) and formatted durations (e.g. "<1m") both go straight into `<text>...</text>`
/// nodes below -- an unescaped `<`, `&`, or `>` makes `XmlDocument::LoadXml` fail, which makes
/// the whole toast function return before ever calling `notifier.Show()`. In
/// `show_auto_submit_toast` that silent bailout is worse than a missing toast: it drops both
/// oneshot senders, and `tokio::select!`'s `_ = submit_rx` arm fires on ANY completion of that
/// future including the resulting "sender dropped" error, so auto-submit fires immediately and
/// silently with no toast and no chance to add notes.
#[cfg(windows)]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(all(test, windows))]
mod xml_escape_tests {
    use super::xml_escape;

    #[test]
    fn escapes_the_sub_minute_duration_marker() {
        // The regression this exists for: "<1m" broke toast XML and silently killed the
        // whole notification (see xml_escape's doc comment).
        assert_eq!(xml_escape("<1m"), "&lt;1m");
    }

    #[test]
    fn escapes_all_xml_metacharacters() {
        assert_eq!(xml_escape("A & B"), "A &amp; B");
        assert_eq!(xml_escape("<tag>"), "&lt;tag&gt;");
        assert_eq!(xml_escape(r#"say "hi""#), "say &quot;hi&quot;");
        assert_eq!(xml_escape("it's"), "it&apos;s");
    }
}

/// The Windows AUMID LilyPad's toasts are registered under.
#[cfg(windows)]
const TOAST_AUMID: &str = "uk.co.froglog.lilypad";

/// Whether Windows will actually display LilyPad's notifications.
///
/// Deliberately three-valued. `Unknown` must not be collapsed into `Disabled`: the only caller
/// that acts on this skips the user's chance to add notes, and doing that because a probe failed
/// would silently take a feature away on a machine whose notifications work fine.
enum NotificationStatus {
    Enabled,
    /// Windows accepts toasts and silently discards them. Nothing will be seen.
    Disabled(String),
    /// Could not be determined. Treated as working, since assuming otherwise costs the user
    /// something real and assuming this way costs only a short wait.
    Unknown(String),
}

/// Reads the notification setting.
///
/// Showing a toast **succeeds** when notifications are turned off for the app: Windows accepts
/// it and silently drops it, so a `Show()` returning `Ok` is not evidence anything appeared.
/// `ToastNotifier::Setting` is the only thing that reports it.
///
/// This does not catch Focus Assist / Do Not Disturb, which report `Enabled` and suppress the
/// toast anyway — there is no dependable API for that, and the session submits on its timer
/// regardless.
#[cfg(windows)]
fn notification_status() -> NotificationStatus {
    use windows::core::HSTRING;
    use windows::UI::Notifications::{NotificationSetting, ToastNotificationManager};

    // WinRT needs COM initialised on the calling thread, and this runs from the async runtime
    // as well as the main thread. Re-initialising an already-initialised thread returns
    // S_FALSE, or RPC_E_CHANGED_MODE if it is an STA -- in both cases the thread is usable, so
    // the result is deliberately ignored.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }

    let notifier = match ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(TOAST_AUMID)) {
        Ok(n) => n,
        Err(e) => return NotificationStatus::Unknown(format!("the notifier could not be created ({e})")),
    };
    let setting = match notifier.Setting() {
        Ok(s) => s,
        Err(e) => return NotificationStatus::Unknown(format!("the notification setting could not be read ({e})")),
    };
    // windows-rs models these as associated constants rather than a Rust enum, so they are
    // compared rather than matched.
    if setting == NotificationSetting::Enabled {
        NotificationStatus::Enabled
    } else if setting == NotificationSetting::DisabledForApplication {
        NotificationStatus::Disabled("notifications are turned off for LilyPad in Windows notification settings".into())
    } else if setting == NotificationSetting::DisabledForUser {
        NotificationStatus::Disabled("notifications are turned off for this Windows user".into())
    } else if setting == NotificationSetting::DisabledByGroupPolicy {
        NotificationStatus::Disabled("notifications are disabled by group policy".into())
    } else if setting == NotificationSetting::DisabledByManifest {
        NotificationStatus::Disabled("notifications are disabled by the app manifest".into())
    } else {
        NotificationStatus::Unknown(format!("unrecognised notification setting ({})", setting.0))
    }
}

#[cfg(not(windows))]
fn notification_status() -> NotificationStatus {
    NotificationStatus::Enabled
}

/// Logs once at startup whether Windows will display LilyPad's notifications, so a machine that
/// silently drops them is diagnosable from the log alone.
fn log_notification_availability() {
    match notification_status() {
        NotificationStatus::Enabled => log::info!("[LilyPad] notifications are enabled"),
        NotificationStatus::Disabled(reason) => log::warn!(
            "[LilyPad] notifications will not be shown: {reason}. Tracking and auto-submit are \
             unaffected -- finished sessions submit immediately rather than waiting for a toast \
             that cannot appear."
        ),
        NotificationStatus::Unknown(reason) => log::warn!(
            "[LilyPad] could not determine whether notifications will be shown: {reason}. \
             Assuming they work."
        ),
    }
}

/// Shows the Windows toast with an "Add Notes" action button for a finished live/session game.
/// Clicking the action cancels the auto-submit and opens the session popup; dismissing it lets
/// the caller submit immediately instead of waiting out the intercept window.
///
/// Presentation only. Both signals it sends are *early answers* the caller is free not to
/// receive: if the toast cannot be created, is suppressed, or never reports anything, the
/// caller's intercept timer submits the session regardless. Nothing here can queue, delay past
/// the window, or fail a session -- see `auto_submit`.
#[cfg(windows)]
fn show_auto_submit_toast(
    title_str: &str,
    time_str: &str,
    cancel_tx_arc: Arc<RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
    pending_data_arc: Arc<RwLock<Option<serde_json::Value>>>,
    dismissed_tx: tokio::sync::oneshot::Sender<()>,
    app: tauri::AppHandle,
) {
    use windows::core::HSTRING;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastDismissedEventArgs, ToastNotification, ToastNotificationManager};

    let title_owned = xml_escape(title_str);
    let time_owned = xml_escape(time_str);
    let dismissed_tx_arc = Arc::new(std::sync::Mutex::new(Some(dismissed_tx)));
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
        // Every failure below simply returns: with no toast there is no early answer to send,
        // and the caller's intercept timer submits the session when it elapses.
        let doc = match XmlDocument::new() {
            Ok(d) => d,
            Err(e) => { log::warn!("[LilyPad] could not create the auto-submit notification ({e})"); return; },
        };
        if let Err(e) = doc.LoadXml(&HSTRING::from(xml.as_str())) {
            log::warn!("[LilyPad] could not build the auto-submit notification ({e})");
            return;
        }
        let toast = match ToastNotification::CreateToastNotification(&doc) {
            Ok(t) => t,
            Err(e) => { log::warn!("[LilyPad] could not create the auto-submit notification ({e})"); return; },
        };
        let dismissed_tx_arc2 = Arc::clone(&dismissed_tx_arc);
        let _ = toast.Dismissed(&TypedEventHandler::<ToastNotification, ToastDismissedEventArgs>::new(
            move |_, _| {
                if let Some(tx) = dismissed_tx_arc2.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                Ok(())
            },
        ));
        let dismissed_on_activation = Arc::clone(&dismissed_tx_arc);
        let _ = toast.Activated(&TypedEventHandler::<ToastNotification, windows::core::IInspectable>::new(
            move |_, _| {
                // Only the first toast action may claim this session. Retain the
                // sender until cancellation is sent, so closing it cannot win first.
                let mut dismissed_guard = dismissed_on_activation.lock().unwrap();
                let Some(dismissed_sender) = dismissed_guard.take() else { return Ok(()) };
                if let Some(tx) = cancel_tx_arc.write().unwrap().take() {
                    let _ = tx.send(());
                }
                drop(dismissed_sender);
                drop(dismissed_guard);
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
        // A toast that cannot be shown is logged and nothing more. It must not decide the fate
        // of the session: the caller's timer does that, and submits when it elapses. (The
        // caller has already skipped this path entirely when it knows notifications are off.)
        let aumid = HSTRING::from(TOAST_AUMID);
        if let Err(e) = ToastNotificationManager::CreateToastNotifierWithId(&aumid).and_then(|n| n.Show(&toast)) {
            log::warn!(
                "[LilyPad] could not show the auto-submit notification ({e}); \
                 the session will auto-submit on the intercept timer instead"
            );
            return;
        }
        // Hold the toast object alive so WinRT keeps the Dismissed/Activated handlers
        // registered. Outliving the intercept window costs nothing now that expiry here means
        // only "no early answer arrived" rather than "the session failed".
        std::thread::sleep(auto_submit::INTERCEPT_WINDOW + std::time::Duration::from_secs(5));
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

    let body_owned = xml_escape(body);
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
    dismissed_tx: tokio::sync::oneshot::Sender<()>,
    app: tauri::AppHandle,
) {
    let _ = app.notification().builder()
        .title("Session Auto-Submitting")
        .body(format!("{} ({}) — open LilyPad tray to add notes", title_str, time_str))
        .show();
    // No action button available on non-Windows, so there is no early answer to give. Dropping
    // the sender leaves the caller's intercept timer to submit, rather than submitting now --
    // the window is what gives the user time to reach the tray.
    drop(dismissed_tx);
}

/// Shared session-ended handler used by both the normal monitor path and crash recovery.
/// `ledger_id` is the durable record this session was completed as. It is passed in rather than
/// read from shared state because recovery can close several interrupted sessions in a row, and
/// each spawns its own task here -- a "most recently finished" field would be overwritten by the
/// next record before the previous task read it, and both would settle the same record.
fn handle_session_ended(
    app: tauri::AppHandle,
    process_name: String,
    mapping: config::ProcessMapping,
    duration_secs: f64,
    ledger_id: Option<String>,
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

            // Live/session: offer an "Add Notes" intercept for a fixed window, then submit.
            // The toast can bring that answer in early but cannot withhold it -- see
            // `auto_submit`.
            if is_notes_type {
                let title_str = mapping.title.as_deref().unwrap_or_else(|| mapping.process.as_str()).to_string();
                let time_str = duration::format_session_duration(duration_secs);
                let session_data = serde_json::json!({
                    "processName": process_name,
                    "mapping": {
                        "process": mapping.process,
                        "type": mapping.r#type,
                        "froglogId": mapping.froglog_id,
                        "title": mapping.title,
                    },
                    "durationSecs": duration_secs,
                    // Carried through the notification so the notes window submits against the
                    // same record this session was completed as.
                    "ledgerId": ledger_id,
                });
                // The toast's "Add Notes" button is the only thing that can intercept a session
                // during the window -- `intercept_auto_submit` exists as a command but nothing
                // invokes it. So when no toast can appear, the window offers the user nothing
                // and is pure delay before an submission that is going to happen anyway.
                // `Unknown` deliberately takes the normal path: see `NotificationStatus`.
                let outcome = match notification_status() {
                    NotificationStatus::Disabled(reason) => {
                        log::info!(
                            "[LilyPad] game {} finished; submitting immediately because no \
                             notification can be shown to add notes from ({reason})",
                            mapping.froglog_id
                        );
                        auto_submit::Outcome::Submit
                    }
                    _ => {
                        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
                        let (dismissed_tx, dismissed_rx) = tokio::sync::oneshot::channel::<()>();
                        let (cancel_tx_arc, pending_data_arc) = {
                            let state = app.state::<AppState>();
                            *state.auto_submit_cancel_tx.write().unwrap() = Some(cancel_tx);
                            *state.auto_submit_pending_data.write().unwrap() = Some(session_data);
                            (Arc::clone(&state.auto_submit_cancel_tx), Arc::clone(&state.auto_submit_pending_data))
                        };
                        log::info!(
                            "[LilyPad] game {} finished; auto-submitting in {}s unless notes are added",
                            mapping.froglog_id,
                            auto_submit::INTERCEPT_WINDOW.as_secs()
                        );
                        show_auto_submit_toast(&title_str, &time_str, cancel_tx_arc, pending_data_arc, dismissed_tx, app.clone());
                        auto_submit::wait_for_outcome(
                            cancel_rx,
                            dismissed_rx,
                            auto_submit::INTERCEPT_WINDOW,
                        )
                        .await
                    }
                };
                if outcome != auto_submit::Outcome::AddNotes {
                    {
                        let state = app.state::<AppState>();
                        *state.auto_submit_pending_data.write().unwrap() = None;
                    }
                    let auth2 = auth.clone();
                    let game_type2 = mapping.r#type.clone();
                    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
                    save_attempt(&app, ledger_id.as_deref(), Submission {
                        game_id: mapping.froglog_id, game_type: mapping.r#type.clone(), title: title_str.clone(),
                        hours, date: date.clone(), notes: Some("Session auto submitted with LilyPad".to_string()),
                        spoiler: false, is_public: true, last_error: None, failed_at: None,
                    });
                    let (tx2, rx2) = tokio::sync::oneshot::channel();
                    let sync_ref = ledger_id.clone();
                    std::thread::spawn(move || {
                        let result = if let Some(client) = api_client(&auth2) {
                            submit_play_session(&client, &game_type2, mapping.froglog_id,
                                Some(date), hours, Some("Session auto submitted with LilyPad".to_string()),
                                false, true, sync_ref)
                        } else { Err("Not logged in".to_string()) };
                        let _ = tx2.send(result);
                    });
                    let result = rx2.await.unwrap_or_else(|e| Err(format!("Auto-submit worker stopped: {e}")));
                    if let Err(error) = &result {
                        let error = error.clone();
                        log::warn!("[LilyPad] auto-submit failed for game {}: {}", mapping.froglog_id, error);
                        queue_failed_submission(
                            &app,
                            ledger_id.as_deref(),
                            Submission {
                                game_id: mapping.froglog_id,
                                game_type: mapping.r#type.clone(),
                                title: title_str.clone(),
                                hours,
                                date: chrono::Local::now().format("%Y-%m-%d").to_string(),
                                notes: None,
                                spoiler: false,
                                is_public: true,
                                last_error: Some(explain_failure(&error)),
                                failed_at: Some(chrono::Local::now().to_rfc3339()),
                            },
                        );
                        let app2 = app.clone();
                        let _ = app.run_on_main_thread(move || { let _ = update_tray_state(&app2); });
                        let _ = app.notification().builder()
                            .title("Session Queued")
                            .body(format!("{} ({}) — submit failed, open LilyPad to retry", title_str, time_str))
                            .show();
                    } else {
                        log::info!("[LilyPad] auto-submit succeeded for game {}", mapping.froglog_id);
                        if let (Some(id), Ok(value)) = (&ledger_id, &result) {
                            acknowledge_ledger_session(
                                &app,
                                id,
                                &remote_reference(value, mapping.froglog_id, &mapping.r#type),
                            );
                        }
                    }
                } else {
                    log::info!("[LilyPad] auto-submit intercepted for notes for game {}", mapping.froglog_id);
                }
                return;
            }

            // Regular games: silent auto-submit (no notes support)
            let date = chrono::Local::now().format("%Y-%m-%d").to_string();
            let mapping2 = mapping.clone();
            let date2 = date.clone();
            let game_type2 = mapping2.r#type.clone();
            let date_for_submit = date2.clone();
            let game_type_for_submit = game_type2.clone();
            save_attempt(&app, ledger_id.as_deref(), Submission {
                game_id: mapping.froglog_id, game_type: mapping.r#type.clone(),
                title: mapping.title.clone().unwrap_or_else(|| mapping.process.clone()),
                hours, date: date.clone(), notes: Some("Session auto submitted with LilyPad".to_string()),
                spoiler: false, is_public: true, last_error: None, failed_at: None,
            });
            let sync_ref = ledger_id.clone();
            let (tx, rx) = tokio::sync::oneshot::channel::<Result<serde_json::Value, String>>();
            std::thread::spawn(move || {
                    let result = if let Some(client) = api_client(&auth) {
                        log::info!("[LilyPad] handle_session_ended: auto-submitting hours");
                        submit_play_session(
                            &client,
                            &game_type_for_submit,
                            mapping2.froglog_id,
                            Some(date_for_submit.clone()),
                            hours,
                            Some("Session auto submitted with LilyPad".to_string()),
                            false,
                            true,
                            sync_ref,
                        )
                    } else {
                        log::warn!("[LilyPad] handle_session_ended: no API client available");
                        Err("Not logged in".to_string())
                    };
                let _ = tx.send(result);
            });
            let result = rx.await.unwrap_or_else(|e| Err(format!("Auto-submit worker stopped: {e}")));
            let title_str = mapping.title.as_deref().unwrap_or_else(|| mapping.process.as_str()).to_string();
            let time_str = duration::format_session_duration(duration_secs);
            if let Ok(value) = &result {
                if let Some(id) = &ledger_id {
                    acknowledge_ledger_session(
                        &app, id, &remote_reference(value, mapping.froglog_id, &mapping.r#type),
                    );
                }
                let _ = app.notification().builder()
                    .title("Session Auto-Submitted")
                    .body(format!("{} ({})", title_str, time_str))
                    .show();
                return;
            }
            // Auto-submit failed — queue the session's own record for retry.
            queue_failed_submission(
                &app,
                ledger_id.as_deref(),
                Submission {
                    game_id: mapping2.froglog_id,
                    game_type: game_type2,
                    title: title_str.clone(),
                    hours,
                    date: date2,
                    notes: None,
                    spoiler: false,
                    is_public: true,
                    last_error: Some(explain_failure(
                        &result.err().unwrap_or_else(|| "Auto-submit failed (auth or network issue)".to_string()),
                    )),
                    failed_at: Some(chrono::Local::now().to_rfc3339()),
                },
            );
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
             // The window submits against this record rather than owning the session itself.
             "ledgerId": ledger_id,
        }));
    });
}

/// The tray menu's "Now Tracking" item, kept so `tick_session_length` can update its text in
/// place (rebuilding the whole menu every 30 s could disturb it while it is open).
static TRACKING_STATUS_ITEM: Mutex<Option<tauri::menu::MenuItem<Wry>>> = Mutex::new(None);

/// `game_label` is the tracked game with its length so far (`ActiveSession::label`).
fn build_tray_menu(app: &tauri::AppHandle, logged_in: bool, game_label: Option<String>) -> Result<Menu<Wry>, Box<dyn std::error::Error + Send + Sync>> {
    let quit_item = PredefinedMenuItem::quit(app, Some("Quit"))?;
    // While tracking, the status/stop items are *prepended* to the normal menu rather than
    // replacing it. Configure (and everything reachable from it) is safe during a session:
    // the active session's wait-thread holds its own clone of the ProcessMapping, and the
    // monitor skips all start-detection while current_session is occupied, so mapping edits,
    // library refreshes, and rescans can only affect *future* sessions, never the live one.
    let tracking_items = if let Some(ref label) = game_label {
        Some((
            MenuItemBuilder::with_id("tracking_status", format!("Now Tracking: {}", label))
                .enabled(false)
                .build(app)?,
            MenuItemBuilder::with_id("force_stop_tracking", "Stop Tracking Current Session").build(app)?,
        ))
    } else {
        None
    };
    *TRACKING_STATUS_ITEM.lock().unwrap() = tracking_items.as_ref().map(|(status, _)| status.clone());
    if logged_in {
        let assign_exes_item = MenuItemBuilder::with_id("assign_exes", "Configure...").build(app)?;
        let about_item = MenuItemBuilder::with_id("about", "About").build(app)?;
        let logout_item = MenuItemBuilder::with_id("logout", "Logout").build(app)?;
        let pending_count = pending_sessions_for(&app.state::<AppState>()).len();
        let new_games_count = new_games_for(&app.state::<AppState>()).len();
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
    let game_label = state.current_session_arc.read().unwrap().as_ref().map(ActiveSession::label);
    let menu = build_tray_menu(app, logged_in, game_label.clone()).map_err(|e| e.to_string())?;
    if let Some(tray) = app.tray_by_id("main") {
        tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
        if let Some(ref label) = game_label {
            let _ = tray.set_icon(Some(TRAY_ICON_NOWPLAYING.clone()));
            let _ = tray.set_tooltip(Some(format!("LilyPad - Now Tracking: {}", label)));
        } else {
            let _ = tray.set_icon(Some(TRAY_ICON.clone()));
            let _ = tray.set_tooltip(Some("LilyPad - FrogLog Auto Tracker".to_string()));
        }
    }
    Ok(())
}

/// Keeps the session length in the tray tooltip and "Now Tracking" item current while a game
/// runs. Only the text changes; a session starting or ending still goes through
/// `update_tray_state`.
fn tick_session_length(app: tauri::AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(30));
        let label = app.state::<AppState>().current_session_arc.read().unwrap().as_ref().map(ActiveSession::label);
        let Some(label) = label else { continue };
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(item) = TRACKING_STATUS_ITEM.lock().unwrap().as_ref() {
                let _ = item.set_text(format!("Now Tracking: {label}"));
            }
            if let Some(tray) = handle.tray_by_id("main") {
                let _ = tray.set_tooltip(Some(format!("LilyPad - Now Tracking: {label}")));
            }
        });
    });
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
    // Captured before the new username lands: the only safe reason to carry mappings forward.
    let had_no_username = auth.username.is_none();
    auth.base_url = Some(base_url.trim_end_matches('/').to_string());
    auth.token = Some(token);
    auth.username = res.username.clone().or_else(|| Some(username.clone()));
    auth.save_to(&auth_config_path()).map_err(|e| e.to_string())?;
    let map_path = process_map_path_for_auth(&*auth);
    // Migrate token-key -> username-key so a pre-v0.4.4 install keeps its mappings. Restricted to
    // exactly that case: `had_no_username` means the previous auth predates usernames being
    // stored, so it can only be *this* machine's earlier session, not another account.
    //
    // Without that restriction, logging in as B while A's auth was still loaded copied A's
    // mappings into B's file -- one account's game associations silently handed to another,
    // which phase 5 item 3 exists to prevent. Two accounts that both have usernames already get
    // distinct paths, so there is nothing legitimate to copy between them.
    //
    // Still skipped when the old path is the anonymous map, which would otherwise overwrite a
    // returning user's real mappings with whatever was recorded while logged out.
    let anonymous_path = process_map_path_for_auth(&AuthConfig::default());
    if had_no_username
        && old_map_path != map_path
        && old_map_path != anonymous_path
        && old_map_path.exists()
    {
        log::info!(
            "[LilyPad] carrying mappings from this install's pre-username key into {}",
            map_path.display()
        );
        let _ = std::fs::copy(&old_map_path, &map_path);
    }
    let process_map = ProcessMapConfig::load_from(&map_path);
    *state.process_map_arc.write().unwrap() = process_map;
    // The cached library belongs to whoever was signed in before. Cleared and marked stale so the
    // new account is never matched against the previous one's games, and so an empty index is not
    // read as "owns nothing" before the first refresh lands.
    *state.library_index_arc.write().unwrap() = LibraryIndex::default();
    *state.library_stale.write().unwrap() = true;
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
    // Ledger record this submission settles, echoed back from the `session-ended` payload.
    ledger_id: Option<String>,
) -> Result<serde_json::Value, String> {
    let auth = state.auth.read().unwrap().clone();
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let spoiler = spoiler.unwrap_or(false);
    let is_public = is_public.unwrap_or(true);
    let ledger_id = submission_ledger_id(&state, ledger_id);

    save_attempt(&app, ledger_id.as_deref(), Submission {
        game_id, game_type: game_type.clone(), title: title.clone().unwrap_or_default(),
        hours, date: date.clone(), notes: notes.clone(), spoiler, is_public,
        last_error: None, failed_at: None,
    });
    // Reuse the ledger identity on retry. The backend returns an existing session on a
    // confirmed replay; a 409 alone is not an acknowledgement.
    let sync_ref = ledger_id.clone();
    let result = match api_client(&auth) {
        None => Err("Not logged in".to_string()),
        Some(client) => submit_play_session(
            &client, &game_type, game_id, Some(date.clone()), hours,
            notes.clone(), spoiler, is_public, sync_ref,
        ),
    };
    match result {
        Ok(v) => {
            if let Some(id) = ledger_id {
                acknowledge_ledger_session(&app, &id, &remote_reference(&v, game_id, &game_type));
            }
            Ok(v)
        }
        Err(e) => {
            queue_failed_submission(
                &app,
                ledger_id.as_deref(),
                Submission {
                    game_id,
                    game_type,
                    title: title.unwrap_or_default(),
                    hours,
                    date,
                    notes,
                    spoiler,
                    is_public,
                    last_error: Some(explain_failure(&e)),
                    failed_at: Some(chrono::Local::now().to_rfc3339()),
                },
            );
            let app2 = app.clone();
            let _ = app.run_on_main_thread(move || { let _ = update_tray_state(&app2); });
            Ok(serde_json::json!({ "queued": true }))
        }
    }
}

/// The retry queue: every ledger record for this account that has ended but has not been
/// acknowledged. Mapped into the shape the UI already renders, keyed by the ledger id.
///
/// A record with no `submission` is one that ended but never got as far as being sent (a crash
/// between completion and submission). It is still listed — that is the whole point of keeping
/// records until the server confirms them — using what the session itself knows.
fn pending_sessions_for(state: &AppState) -> Vec<PendingSession> {
    let Some(account) = state.account() else { return Vec::new() };
    state.with_ledger("reading the retry queue", |l| l.pending_sessions(&account))
        .unwrap_or_default()
}
#[tauri::command]
fn get_pending_sessions(state: tauri::State<AppState>) -> Vec<PendingSession> {
    pending_sessions_for(&state)
}

/// The last session-storage failure, or `null` when the store is healthy. Exposed so a failure
/// to persist is visible in the app rather than only in the log file -- a store that cannot be
/// written must never look like a store with nothing in it.
#[tauri::command]
fn session_storage_error(state: tauri::State<AppState>) -> Option<String> {
    state.storage_error.read().unwrap().clone()
}

/// Sessions the store still holds for this account: interrupted ones it will resolve at next
/// startup, and completed ones the server has not acknowledged.
#[tauri::command]
fn session_storage_status(state: tauri::State<AppState>) -> serde_json::Value {
    let account = state.account();
    let interrupted = account
        .as_ref()
        .and_then(|a| state.with_ledger("reading interrupted sessions", |l| l.interrupted(Some(a))))
        .map(|v| v.len());
    // The two pending queues are reported separately: they have different resolution flows and
    // folding them together was what let a new-game entry show up as a retryable session.
    let unsubmitted = account
        .as_ref()
        .and_then(|a| state.with_ledger("reading unsubmitted sessions", |l| l.unsubmitted_sessions(Some(a))))
        .map(|v| v.len());
    let new_games = account
        .as_ref()
        .and_then(|a| state.with_ledger("reading the new-games queue", |l| l.new_games(Some(a))))
        .map(|v| v.len());
    // Unowned records are imported legacy entries whose account could not be established; they
    // need explicit adoption before they can be submitted, so they are reported separately
    // rather than folded into this account's counts.
    let unowned = state
        .with_ledger("reading unowned sessions", |l| l.outstanding(None))
        .map(|v| v.len());
    serde_json::json!({
        "available": state.ledger.is_some(),
        "error": state.storage_error.read().unwrap().clone(),
        "interrupted": interrupted,
        "unsubmitted": unsubmitted,
        "newGames": new_games,
        "unowned": unowned,
        "libraryStale": *state.library_stale.read().unwrap(),
    })
}

#[tauri::command]
fn retry_pending_session(
    app: tauri::AppHandle,
    state: tauri::State<AppState>,
    id: String,
) -> Result<(), String> {
    // Credentials and ownership come from the same snapshot. A later logout/login must
    // neither submit this row with another token nor prevent acknowledging the original owner.
    let auth = state.auth.read().unwrap().clone();
    let client = api_client(&auth).ok_or("Not logged in")?;
    let account = ledger_session::account_identity(&auth, DEFAULT_API_URL).ok_or("Not logged in")?;
    let session = state.with_ledger("reading a pending session", |l| l.pending_session(&id, &account))
        .ok_or("Session not found for this account, or storage unavailable")?;
    let result = lilypad_core::submission::retry_play_session(&client, &session, Some(session.id.clone()))?;
    let remote = remote_reference(&result.response, result.game_id, &result.game_type);
    match state.with_ledger("recording retry acknowledgement", |l| l.acknowledge(&id, &account, &remote)) {
        Some(true) => {
            let handle = app.clone();
            let _ = app.run_on_main_thread(move || { let _ = update_tray_state(&handle); });
            Ok(())
        }
        Some(false) => Err("The session changed while submitting; check its recorded status".into()),
        None => Err("Submitted, but the acknowledgement could not be saved. Retry uses the same session key.".into()),
    }
}

/// "Do not record session": the user has looked at a finished session and decided against
/// logging it. Discards its record outright.
///
/// Without this the record simply stayed `pending` — the state meaning "ended but not yet
/// submitted" — so a session the user explicitly declined reappeared in Pending Submissions as
/// though it were a failed one awaiting retry. Dismissing keeps the row, so a declined session
/// stays distinguishable from one that was never recorded, but takes it out of every queue.
#[tauri::command]
fn discard_session(app: tauri::AppHandle, state: tauri::State<AppState>, ledger_id: Option<String>) -> Result<(), String> {
    let Some(id) = submission_ledger_id(&state, ledger_id) else {
        return Ok(());
    };
    let account = state.account().ok_or("Not logged in")?;
    match state.with_ledger("discarding a session", |l| l.dismiss_owned(&id, &account)) {
        Some(true) => {
            log::info!("[LilyPad] session {id} discarded at the user's request");
            let app2 = app.clone();
            let _ = app.run_on_main_thread(move || {
                let _ = update_tray_state(&app2);
            });
            Ok(())
        }
        // Already settled -- most likely submitted a moment ago. Nothing to undo.
        Some(false) => Ok(()),
        None => Err("Session storage is unavailable".to_string()),
    }
}

#[tauri::command]
fn delete_pending_session(state: tauri::State<AppState>, id: String) -> Result<(), String> {
    // Named before dismissing, so the log records which session was thrown away rather than an
    // opaque id. Discarding a queued session is a decision worth being able to reconstruct.
    let describe = pending_sessions_for(&state)
        .into_iter()
        .find(|s| s.id == id)
        .map(|s| format!("{} ({}h, {})", s.title, s.hours, s.date))
        .unwrap_or_else(|| "unknown session".to_string());
    // Dismissing takes it out of every queue while keeping the row, so a discarded session stays
    // distinguishable from one that was never recorded.
    let account = state.account().ok_or("Not logged in")?;
    match state.with_ledger("discarding session", |l| l.dismiss_owned(&id, &account)) {
        Some(true) => {
            log::info!("[LilyPad] queued session {id} discarded: {describe}");
            Ok(())
        }
        Some(false) => Err("Not found".to_string()),
        None => Err("Session storage is unavailable".to_string()),
    }
}

/// Games played but not yet in the library. Ledger records in `pending` whose target is a
/// new-game entry, scoped to this account.
fn new_games_for(state: &AppState) -> Vec<config::PendingGameSubmission> {
    let account = state.account();
    state
        .with_ledger("reading the new-games queue", |l| l.new_games(account.as_ref()))
        .unwrap_or_default()
}

/// Records the executable a mapping was seen running as, the first time it tracks a session.
///
/// This is what upgrades a mapping from "matches any binary with this filename" to "matches this
/// game", so two installs sharing an exe name stop being interchangeable. Mappings made before
/// the field existed have no path and are backfilled here rather than by a migration — the path
/// is only knowable from a running process, which is exactly this moment.
///
/// Also corrects a stale path, which is how a moved install heals: `find_all_for_process` still
/// matches a mapping whose recorded path has vanished from disk, and this rewrites it.
fn record_mapping_exe_path(app: &tauri::AppHandle, mapping: &ProcessMapping) {
    let state = app.state::<AppState>();
    let Some(pid) = state
        .current_session_arc
        .read()
        .ok()
        .and_then(|s| s.as_ref().map(|s| s.pid))
    else {
        return;
    };
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]));
    let Some(exe_path) = system.process(pid).and_then(|p| p.exe().map(|e| e.to_path_buf())) else {
        return;
    };

    let auth = state.auth.read().unwrap().clone();
    match config::backfill_mapping_exe_path(&state.process_map_arc, &auth, mapping, &exe_path) {
        Ok(true) => log::info!(
            "[LilyPad] mapping {} -> {} #{} now pinned to {}",
            mapping.process, mapping.r#type, mapping.froglog_id, exe_path.display()
        ),
        Ok(false) => {}
        Err(e) => log::warn!("[LilyPad] could not save the mapping's executable path: {e}"),
    }
}

/// Records an unmapped game's session durably as it starts, and checkpoints it while it runs.
///
/// Without this an unmapped game was recorded only when it *ended*, so crashing mid-play lost
/// the whole session — there was never a record of it. That was phase 1's remaining gap: its
/// first acceptance criterion asks that a crash at any point after process detection leave a
/// recoverable record, and it did not hold for this path.
///
/// The record is purely for recovery. A session that ends normally settles it and goes through
/// the usual accumulate-into-the-new-games-queue route; only a session interrupted mid-play is
/// resolved from the record itself, at startup.
fn begin_unmapped_session(app: &tauri::AppHandle, start: &monitor::UnmappedSessionStart) {
    let state = app.state::<AppState>();
    let Some(account) = state.account() else { return };
    let record = SessionRecord::new(
        account,
        ProcessIdentity {
            executable: start.exe_name.clone(),
            pid: Some(u32::try_from(usize::from(start.pid)).unwrap_or_default()),
            started_at_secs: start.process_started_at_secs,
            ..Default::default()
        },
        SessionTarget::Unmapped {
            appid: start.appid.clone(),
            title: start.title.clone(),
            replay_of: start.replay_of.as_ref().map(|r| config::ReplayOf {
                id: r.id,
                game_type: r.game_type.clone(),
                title: r.title.clone(),
                status: r.status.clone(),
            }),
        },
        ledger_session::now_secs(),
    );
    let id = record.id.clone();
    if state
        .with_ledger("recording an unmapped session start", |l| {
            l.insert(&record, SubmissionState::Active)
        })
        .is_none()
    {
        return;
    }
    state
        .active_unmapped_ledger_ids
        .write()
        .unwrap()
        .insert(start.appid.clone(), id.clone());
    log::info!(
        "[LilyPad] unmapped session {id} started: {} ({})",
        start.title, start.appid
    );

    // Checkpointed like any other session, so an interrupted one is credited up to its last
    // confirmed-alive tick rather than zero. Presence is `None`: an unmapped game is not in the
    // library, so there is no id to report as "now playing".
    start_unmapped_heartbeat(app, id, start.exe_name.clone());
}

/// Settles the durable record for an unmapped session that ended normally. Its hours go into the
/// new-games queue by the usual route, so the record has done its job.
fn finish_unmapped_session(app: &tauri::AppHandle, appid: &str) -> Option<f64> {
    let state = app.state::<AppState>();
    let id = state.active_unmapped_ledger_ids.write().unwrap().remove(appid)?;
    state.with_ledger("completing an unmapped session", |l| {
        l.complete_unmapped(&id, ledger_session::now_secs(), false)
    }).flatten()
}
/// Records that the user chose not to add a new game. Distinct from resolving it: the row is
/// kept either way, and which outcome it was is the thing worth keeping.
fn dismiss_new_game(state: &AppState, appid: &str) {
    let account = state.account();
    // Read before dismissing, so the log can name what was thrown away rather than just its id.
    let title = new_games_for(state)
        .into_iter()
        .find(|g| g.appid == appid)
        .map(|g| g.title)
        .unwrap_or_else(|| "unknown game".to_string());
    if state
        .with_ledger("discarding a new game", |l| {
            l.dismiss_new_game(account.as_ref(), appid)
        })
        .unwrap_or(false)
    {
        log::info!("[LilyPad] new game {title} (appid {appid}) discarded without being added");
    }
}

#[tauri::command]
fn get_pending_game_submissions(state: tauri::State<AppState>) -> Vec<config::PendingGameSubmission> {
    new_games_for(&state)
}

#[tauri::command]
fn dismiss_pending_game_submission(state: tauri::State<AppState>, appid: String) -> Result<(), String> {
    dismiss_new_game(&state, &appid);
    Ok(())
}

#[tauri::command]
fn search_igdb_games(state: tauri::State<AppState>, query: String) -> Result<Vec<serde_json::Value>, String> {
    let auth = state.auth.read().unwrap();
    let client = api_client(&auth).ok_or("Not logged in")?;
    client.search_igdb(&query)
}

/// Runs a New Games resolution through the shared, retry-safe resolver
/// (`lilypad_core::resolution`), then links the exe and refreshes the library.
///
/// Credentials and account come from one snapshot, and the library is a snapshot too, so no
/// lock is held across the network calls.
fn resolve_pending(
    state: &AppState,
    appid: &str,
    choice: resolution::Choice,
) -> Result<resolution::Resolution, String> {
    let auth = state.auth.read().unwrap().clone();
    let client = api_client(&auth).ok_or("Not logged in")?;
    let account = ledger_session::account_identity(&auth, DEFAULT_API_URL).ok_or("Not logged in")?;
    let library = state.library_index_arc.read().unwrap().clone();
    let resolved = resolution::resolve(&client, &state.store(), &account, &library, appid, choice, resolution::CREATED_NOTE)?;
    resolution::link_resolved(&state.process_map_arc, &auth, &resolved);
    // Without this the library only refreshes every 5 minutes -- relaunching the same game
    // shortly after would still look new (or still finished) and be prompted about again.
    refresh_library_index_state(state);
    Ok(resolved)
}

/// The game a resolution ended up on: the one it created, or the existing one it attached to.
fn resolved_game(state: &AppState, resolved: resolution::Resolution) -> Result<serde_json::Value, String> {
    match resolved.created {
        Some(created) => Ok(created),
        None => {
            let auth = state.auth.read().unwrap().clone();
            api_client(&auth).ok_or("Not logged in")?.get_game_raw(resolved.game_id)
        }
    }
}

/// Resolves a pending game submission by creating a brand-new FrogLog game entry for it, or
/// attaching to the entry the user already has for the same game.
#[tauri::command]
fn resolve_pending_game_as_new(
    state: tauri::State<AppState>,
    appid: String,
    igdb_title: String,
) -> Result<serde_json::Value, String> {
    let resolved = resolve_pending(&state, &appid, resolution::Choice::New { igdb_title })?;
    resolved_game(&state, resolved)
}

/// Resolves a pending game submission flagged as a possible replay by creating a new entry
/// alongside the Completed/DNF one, which is left untouched.
#[tauri::command]
fn resolve_pending_game_as_replay(
    state: tauri::State<AppState>,
    appid: String,
) -> Result<serde_json::Value, String> {
    let resolved = resolve_pending(&state, &appid, resolution::Choice::Replay)?;
    resolved_game(&state, resolved)
}

/// Resolves a pending game submission by logging its sessions against a game/live-service
/// entry the user already has.
#[tauri::command]
fn resolve_pending_game_as_existing(
    state: tauri::State<AppState>,
    appid: String,
    game_type: String, // "regular" | "session" | "live"
    game_id: i32,
    game_title: String,
) -> Result<(), String> {
    resolve_pending(
        &state,
        &appid,
        resolution::Choice::Existing { game_type, game_id, title: game_title },
    )
    .map(|_| ())
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
        // Filled in the first time this mapping tracks a session: the user types an executable
        // name here, and only a running process reveals which file on disk that is.
        exe_path: None,
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
    // Before anything else: opening the ledger, starting the monitor or touching config from a
    // second instance is exactly what this exists to prevent.
    if !claim_single_instance() {
        report_already_running();
        return;
    }

    let mut auth = AuthConfig::load_from(&auth_config_path());
    let mut startup_log = Vec::new();
    // Before anything reads the process map or records a session, so both land on the final
    // account key rather than being written under a token hash and orphaned moments later.
    backfill_username(&mut auth, &mut startup_log);
    let process_map = ProcessMapConfig::load_from(&process_map_path_for_auth(&auth));

    // Durable session store, plus the one-time import of the legacy JSON queues.
    let (opened, ledger_log) = open_session_ledger();
    startup_log.extend(ledger_log);
    let (ledger, ledger_open_error) = match opened {
        Ok(ledger) => (Some(Arc::new(Mutex::new(ledger))), None),
        Err(e) => {
            let message = format!(
                "Session storage unavailable ({e}). Sessions cannot be recorded durably; \
                 crash recovery is disabled until this is resolved."
            );
            startup_log.push((log::Level::Error, format!("[LilyPad] {message}")));
            (None, Some(message))
        }
    };

    let log_dir = dirs::data_local_dir()
        .map(|d| d.join("froglog-lilypad"))
        .unwrap_or_else(|| std::path::PathBuf::from("froglog-lilypad"));
    tauri::Builder::default()
        .plugin(tauri_plugin_log::Builder::new()
            .level(log::LevelFilter::Info)
            // The plugin timestamps in UTC by default, which put every line an hour behind the
            // clock during BST and made the log awkward to line up against what a user actually
            // saw happen. Falls back to UTC on its own if the OS offset cannot be determined.
            .timezone_strategy(tauri_plugin_log::TimezoneStrategy::UseLocal)
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
            ledger,
            storage_error: Arc::new(RwLock::new(ledger_open_error)),
            library_stale: Arc::new(RwLock::new(false)),
            install_fingerprint: Arc::new(RwLock::new(None)),
            active_ledger_id: Default::default(),
            active_unmapped_ledger_ids: Arc::new(RwLock::new(std::collections::HashMap::new())),
            last_finished_ledger_id: Arc::new(RwLock::new(None)),
        })
        .invoke_handler(tauri::generate_handler![
            login,
            logout,
            refresh_tray_menu,
            get_games,
            get_live_service_games,
            submit_session,
            get_pending_sessions,
            discard_session,
            session_storage_error,
            session_storage_status,
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
        .setup(move |app| {
            // The log plugin only exists from here on, so anything the pre-Builder startup work
            // recorded is emitted now rather than being lost.
            replay_startup_log(startup_log);
            // Stated once, up front: a machine that silently drops toasts is otherwise
            // indistinguishable in the log from one where nothing interesting happened.
            log_notification_availability();

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
                        // The cached library is this account's, and nothing else cleared it: the
                        // next user would be matched against the previous user's games until the
                        // first refresh, and could have a launch auto-linked to an entry that is
                        // not theirs. Marked stale rather than merely emptied so an empty index is
                        // never mistaken for "this user owns nothing", which would file every
                        // game they play as new.
                        *state.library_index_arc.write().unwrap() = LibraryIndex::default();
                        *state.library_stale.write().unwrap() = true;
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
                            // Close the durable record now. `finish` only applies to a record
                            // that is still `active`, so the monitor's own end event for this
                            // same process (which arrives later, when the process really does
                            // exit) cannot complete it a second time.
                            let ledger_id = finish_ledger_session(&state, ledger_session::now_secs());
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
                                "ledgerId": ledger_id,
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
            tick_session_length(handle.clone());

            // Show login window on first launch (no saved credentials)
            if !logged_in {
                if let Some(w) = app.get_webview_window("main") {
                    show_window_at_height(&w, WINDOW_WIDTH, MAIN_ABOUT_HEIGHT);
                }
            }

            // Start process monitor in background (share process_map with AppState)
            let app_handle = app.handle().clone();
            let app_handle_for_unmapped = app_handle.clone();
            let app_handle_for_unmapped_start = app_handle.clone();
            let app_handle_for_library_refresh = app_handle.clone();
            let app_handle_for_already_owned = app_handle.clone();
            let state = app.state::<AppState>();
            let config = Arc::clone(&state.process_map_arc);
            let current_session = Arc::clone(&state.current_session_arc);
            let force_stopped_process = Arc::clone(&state.force_stopped_process);
            let installed_games_arc = Arc::clone(&state.installed_games_arc);
            let library_index_arc = Arc::clone(&state.library_index_arc);

            // Two refreshes on very different budgets.
            //
            // Installed games are local files, and the scan is gated on directory mtimes, so
            // checking often costs a few `stat` calls and a newly installed game is picked up
            // within seconds. It used to share the five-minute cycle below, which meant
            // installing a game and launching it straight away matched nothing at all -- it was
            // simply absent from the list the monitor consults.
            //
            // The library index is a network fetch, so it stays on the slow cycle; the on-demand
            // refresh in `maybe_start_unmapped_tracking` covers the case that actually matters
            // between ticks.
            {
                let app_handle_refresh = app_handle.clone();
                std::thread::spawn(move || {
                    let mut since_library_refresh = Duration::from_secs(300);
                    loop {
                        {
                            let state = app_handle_refresh.state::<AppState>();
                            refresh_installed_games_if_changed(&state);
                            if since_library_refresh >= Duration::from_secs(300) {
                                refresh_library_index_state(&state);
                                since_library_refresh = Duration::ZERO;
                            }
                        }
                        std::thread::sleep(INSTALL_SCAN_INTERVAL);
                        since_library_refresh += INSTALL_SCAN_INTERVAL;
                    }
                });
            }

            // Recover every session interrupted by a LilyPad crash, driver restart, or full PC
            // shutdown/reboot. Unlike the single-file scheme this replaces, the store can hold
            // more than one interrupted record, so each is resolved on its own terms: still
            // running means resume tracking it, gone means credit it up to its last
            // confirmed-alive checkpoint (never "now", so downtime is not billed as play time)
            // and put it through the normal end-of-session path.
            recover_interrupted_sessions(&app_handle);

            let handle_dead_mapping = app_handle.clone();
            run_poll_loop(
                config,
                current_session,
                Arc::new(AtomicBool::new(false)),
                force_stopped_process,
                2,
                {
                    let handle = app_handle.clone();
                    move |process_name, mapping: ProcessMapping| {
                        // Record the session durably before anything else, so a crash from here
                        // on leaves a recoverable record rather than nothing.
                        let ledger_id = {
                            let state = handle.state::<AppState>();
                            let id = begin_ledger_session(&state, &process_name, &mapping);
                            state.active_ledger_id.set(&process_name, id.clone());
                            id
                        };
                        // Pin the mapping to this exact binary, so a same-named executable from
                        // another install stops matching it. No-op once recorded.
                        record_mapping_exe_path(&handle, &mapping);
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
                        let mapping_hb = mapping.clone();
                        let started_at_iso_hb = started_at_iso.clone();
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
                        // Always heartbeat, presence or not: only its remote half is about
                        // presence (spawn_ledger_heartbeat gates that itself, re-reading the
                        // setting each tick). Its local half is the crash-recovery
                        // alive-checkpoint -- skipping it used to leave the checkpoint pinned
                        // at the session start time, so a session recovered with presence off
                        // was credited ~0 hours.
                        start_session_heartbeat(
                            &handle,
                            ledger_id,
                            process_name,
                            mapping_hb,
                            started_at_iso_hb,
                        );
                        // Update tray to now-tracking state
                        let handle_tray = handle.clone();
                        let _ = handle.run_on_main_thread(move || {
                            let _ = update_tray_state(&handle_tray);
                        });
                    }
                },
                {
                    move |process_name, mapping, duration_secs| {
                        {
                            let state = app_handle.state::<AppState>();
                            // A force-stopped session was already ended by the tray handler.
                            // Clear the block now the process has genuinely exited.
                            let was_force_stopped = take_force_stop(&state, &process_name);
                            // Only this process's own record, and none at all after a force-stop:
                            // by then the slot may hold a different game started since.
                            let active_id = if was_force_stopped {
                                None
                            } else {
                                state.active_ledger_id.take_for(&process_name)
                            };

                            // Completion is persisted before any notification or network call,
                            // so a crash between the game exiting and the session being
                            // submitted leaves a pending record rather than nothing. `finish`
                            // doubles as the stale-worker test: a record that is no longer
                            // active was ended by someone else (the force-stop above, or a
                            // second waiter for the same process), and emitting again would
                            // show a duplicate popup and log a duplicate session.
                            let completion = match active_id {
                                Some(id) => complete_session(&state, &id, ledger_session::now_secs()),
                                None => Completion::Owned(None),
                            };
                            match completion {
                                _ if was_force_stopped => {}
                                Completion::Owned(ledger_id) => handle_session_ended(
                                    app_handle.clone(), process_name, mapping, duration_secs, ledger_id,
                                ),
                                Completion::Stale => log::info!(
                                    "[LilyPad] session for {process_name} was already completed; \
                                     not emitting a second end event"
                                ),
                            }
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
                    // Called when a running game looks absent from the library, before that is
                    // taken as fact. Synchronous: the monitor re-reads the index immediately
                    // after, so a game added on the website minutes ago resolves correctly
                    // instead of becoming a New Games entry the user already owns.
                    let handle = app_handle_for_library_refresh;
                    move || refresh_library_index_state(&handle.state::<AppState>())
                },
                {
                    let handle = app_handle_for_unmapped_start;
                    move |start: monitor::UnmappedSessionStart| {
                        begin_unmapped_session(&handle, &start);
                    }
                },
                {
                    let handle = app_handle_for_unmapped;
                    move |title: String, appid: String, _exe_name: String, duration_secs: f64, replay_of: Option<lilypad_core::library_match::ResolvedLibraryGame>| {
                        // The recorded owner and target are authoritative, even after logout.
                        // A failed transaction leaves the source available for startup recovery.
                        let Some(total) = finish_unmapped_session(&handle, &appid) else { return; };
                        log::info!("[LilyPad] new game {title} (appid {appid}); {total}h awaiting resolution");
                        let time_str = duration::format_session_duration(duration_secs);
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
                        let mapping_process = mapping.process.clone();
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
                        //
                        // A `404` from either is not best-effort noise: it means the entry this
                        // mapping was just built from no longer exists. The cached library said
                        // it did, which happens when a game is deleted on the website inside the
                        // refresh window. Left alone, the mapping persists pointing at a dead id,
                        // the session is tracked against it, and the post-play popup has nowhere
                        // to submit -- a 404 the user cannot act on.
                        let handle_dead = handle.clone();
                        std::thread::spawn(move || {
                            let Some(client) = api_client(&auth) else { return };
                            let mut missing = false;
                            if let Err(e) = client.fix_imported_status_if_needed(froglog_id, &game_type) {
                                missing |= api::ApiFailure::classify(&e) == api::ApiFailure::NotFound;
                                log::warn!("[LilyPad] failed to fix imported status: {e}");
                            }
                            if !game_type.eq_ignore_ascii_case("live") {
                                if let Err(e) = client.enable_session_tracking(froglog_id) {
                                    missing |= api::ApiFailure::classify(&e) == api::ApiFailure::NotFound;
                                    log::warn!("[LilyPad] failed to enable session tracking: {e}");
                                }
                            }
                            if missing {
                                drop_dead_mapping(&handle_dead, &mapping_process, &game_type, froglog_id);
                            }
                        });
                    }
                },
                // The monitor has just confirmed against a fresh library that this game is
                // gone, so only the mapping needs dropping; the tray is unaffected (nothing
                // started tracking).
                {
                    let handle = handle_dead_mapping;
                    move |mapping: ProcessMapping| {
                        unlink_dead_mapping(&handle, &mapping.process, &mapping.r#type, mapping.froglog_id);
                    }
                },
            );

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
