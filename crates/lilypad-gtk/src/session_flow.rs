//! What happens when a tracked session ends: the auto-submit decision tree,
//! ported from the Tauri build's `handle_session_ended` in lib.rs. Runs
//! entirely off the GTK main thread — the only GTK-thread work it ever
//! triggers (showing the session popup) goes through `AppAction` /
//! `app_tx`, since `notify::show` and `RefreshTray` are both thread-safe and
//! safe to call directly from wherever this code happens to be running.

use crate::notify;
use crate::state::{AppState, DEFAULT_API_URL};
use crate::tray::RefreshTray;
use lilypad_core::api::FroglogClient;
use lilypad_core::config::{AuthConfig, ProcessMapping};

#[derive(Debug, Clone)]
pub struct SessionEndedData {
    pub process_name: String,
    pub mapping: ProcessMapping,
    pub duration_secs: f64,
    pub forced: bool,
}

pub enum AppAction {
    ShowSessionPopup(SessionEndedData),
    GoToNewGames,
}

/// Hours for FrogLog: 2 decimal places, minimum 0.01 (matches the Tauri build).
pub fn round_hours(duration_secs: f64) -> f64 {
    let raw = duration_secs / 3600.0;
    let r = (raw * 100.0).round() / 100.0;
    if r < 0.01 {
        0.01
    } else {
        r
    }
}

pub fn format_duration(duration_secs: f64) -> String {
    lilypad_core::duration::format_session_duration(duration_secs)
}

pub(crate) fn client_for(auth: &AuthConfig) -> FroglogClient {
    let base = auth
        .base_url
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_API_URL.to_string());
    let mut c = FroglogClient::new(base);
    c.set_token(auth.token.clone());
    c
}

/// Reports "now playing" to FrogLog when a tracked session starts, if the user
/// has online presence enabled. Fire-and-forget on a background thread, same
/// as the corresponding `clear_now_playing` call in `handle_session_ended`.
pub fn handle_session_started(state: &AppState, mapping: &ProcessMapping) {
    let share_now_playing = state.process_map.read().unwrap().share_now_playing;
    if !share_now_playing {
        return;
    }
    let auth = state.auth.read().unwrap().clone();
    let mapping = mapping.clone();
    let started_at_iso = chrono::Utc::now().to_rfc3339();
    std::thread::spawn(move || {
        let client = client_for(&auth);
        let _ = client.set_now_playing(mapping.froglog_id, mapping.r#type.clone(), mapping.title.clone(), Some(started_at_iso));
    });
}

/// Entry point called whenever a tracked session ends (normal exit or a
/// user-initiated force-stop from the tray).
pub fn handle_session_ended(
    state: AppState,
    refresh_tray: RefreshTray,
    app_tx: async_channel::Sender<AppAction>,
    process_name: String,
    mapping: ProcessMapping,
    duration_secs: f64,
    forced: bool,
) {
    let auth = state.auth.read().unwrap().clone();

    // Always clear now_playing, regardless of auto-submit / force-stop.
    {
        let auth = auth.clone();
        std::thread::spawn(move || {
            let client = client_for(&auth);
            let _ = client.clear_now_playing();
        });
    }

    // Force-stop never auto-submits (the user is likely correcting a bad
    // mapping) — always show the popup so they can still log the time.
    if forced {
        let _ = app_tx.send_blocking(AppAction::ShowSessionPopup(SessionEndedData {
            process_name,
            mapping,
            duration_secs,
            forced,
        }));
        return;
    }

    let auto_submit = {
        let cfg = state.process_map.read().unwrap();
        match mapping.r#type.as_str() {
            "live" => cfg.auto_submit_live,
            "session" => cfg.auto_submit_session,
            _ => cfg.auto_submit_regular,
        }
    };

    if !auto_submit {
        let _ = app_tx.send_blocking(AppAction::ShowSessionPopup(SessionEndedData {
            process_name,
            mapping,
            duration_secs,
            forced,
        }));
        return;
    }

    let is_notes_type = mapping.r#type.eq_ignore_ascii_case("live") || mapping.r#type.eq_ignore_ascii_case("session");
    let hours = round_hours(duration_secs);

    if is_notes_type {
        // Show a notification with an "Add Notes" action, then wait up to the notification's
        // own timeout for the user to click it before auto-submitting. 5 seconds matches
        // Windows' toast, which sets no explicit duration/scenario and so falls back to the
        // OS's default "short" banner display time (~5s, vs. ~25s for `duration="long"`, which
        // isn't set there).
        //
        // `wait_for_action` only resolves once the notification server actually reports the
        // banner closing (a NotificationClosed/ActionInvoked signal over D-Bus) -- unlike
        // Windows' toast API, which guarantees that callback fires when its timeout elapses,
        // Linux notification daemons aren't required to honor the requested timeout at all (the
        // freedesktop.org spec explicitly allows a server to ignore it), and GNOME Shell in
        // particular is known not to send it until the user actually interacts with the
        // notification when it has action buttons. Relying on `wait_for_action` alone left this
        // thread blocked indefinitely and auto-submit never happening. Race it against our own
        // timer instead, so auto-submit always fires on schedule regardless of whether the
        // notification server ever reports the banner closing.
        std::thread::spawn(move || {
            let title = mapping.title.clone().unwrap_or_else(|| mapping.process.clone());
            let time_str = format_duration(duration_secs);

            let notification = notify_rust::Notification::new()
                .appname("LilyPad")
                .summary("Session Auto-Submitting")
                .body(&format!("{title} ({time_str})"))
                .action("add_notes", "Add Notes")
                .timeout(notify_rust::Timeout::Milliseconds(5_000))
                .show();

            let add_notes_clicked = match notification {
                Ok(handle) => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        handle.wait_for_action(move |action| {
                            let _ = tx.send(action == "add_notes");
                        });
                    });
                    matches!(rx.recv_timeout(std::time::Duration::from_millis(5_500)), Ok(true))
                }
                Err(e) => {
                    log::warn!("[LilyPad] auto-submit notification failed: {e}");
                    false
                }
            };

            if add_notes_clicked {
                let _ = app_tx.send_blocking(AppAction::ShowSessionPopup(SessionEndedData {
                    process_name,
                    mapping,
                    duration_secs,
                    forced: false,
                }));
                return;
            }

            // Matches the Tauri build: a live/session game that's already had its "Add Notes"
            // toast shown gets no further notification on success, only on failure (see
            // `submit_now`'s `notify_on_success`) -- Windows never had one here either.
            submit_now(&auth, &mapping, hours, duration_secs, &refresh_tray, false);
        });
    } else {
        // Regular games: silent auto-submit (no notes support), but do confirm success --
        // matches the Tauri build's regular-game path, which shows "Session Auto-Submitted"
        // here specifically (unlike the live/session path above).
        std::thread::spawn(move || {
            submit_now(&auth, &mapping, hours, duration_secs, &refresh_tray, true);
        });
    }
}

/// Submits a session/hours update immediately, falling back to the pending
/// queue on failure so the play time isn't lost. Runs on a background thread.
/// `hours` is the value sent to the API (rounded); `duration_secs` is only used
/// for the notification's display text. `notify_on_success` controls whether a
/// "Session Auto-Submitted" notification fires on success -- always shown for a failure either
/// way, but on success it should only fire for the regular-game path (see call sites).
fn submit_now(auth: &AuthConfig, mapping: &ProcessMapping, hours: f64, duration_secs: f64, refresh_tray: &RefreshTray, notify_on_success: bool) {
    let client = client_for(auth);
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let title = mapping.title.clone().unwrap_or_else(|| mapping.process.clone());

    let result = if mapping.r#type.eq_ignore_ascii_case("live") {
        client.add_live_service_session(
            mapping.froglog_id,
            Some(date.clone()),
            Some(hours),
            Some("Session auto submitted with LilyPad".to_string()),
            false,
            true,
            None,
        )
    } else if mapping.r#type.eq_ignore_ascii_case("session") {
        client.add_game_session(
            mapping.froglog_id,
            Some(date.clone()),
            Some(hours),
            Some("Session auto submitted with LilyPad".to_string()),
            false,
            true,
            None,
        )
    } else {
        client.update_game_hours(mapping.froglog_id, hours)
    };

    match result {
        Ok(_) => {
            if notify_on_success {
                notify::show("Session Auto-Submitted", &format!("{title} ({})", format_duration(duration_secs)));
            }
        }
        Err(e) => {
            let mut sessions = lilypad_core::config::load_pending_sessions();
            sessions.push(lilypad_core::config::PendingSession {
                id: chrono::Local::now().timestamp_millis().to_string(),
                game_id: mapping.froglog_id,
                game_type: mapping.r#type.clone(),
                title: title.clone(),
                hours,
                notes: None,
                spoiler: false,
                is_public: true,
                date,
                failed_at: chrono::Local::now().to_rfc3339(),
                error: format!("Auto-submit failed (auth or network issue): {e}"),
            });
            lilypad_core::config::save_pending_sessions(&sessions);
            notify::show("Session Queued", &format!("{title} — submit failed, open LilyPad to retry"));
        }
    }
    refresh_tray();
}
