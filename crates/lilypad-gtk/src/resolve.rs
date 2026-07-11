//! Resolving a "New Games" entry (a completed session for a game not yet in the user's
//! FrogLog library) into a real FrogLog entry — either by creating a brand-new game or by
//! logging the accumulated hours against an existing one. Ported from the Tauri build's
//! `resolve_pending_game_as_new` / `resolve_pending_game_as_existing` Tauri commands
//! (`src-tauri/src/lib.rs`), as plain functions since GTK has no Tauri IPC layer to hang
//! `#[tauri::command]`s off of. Both are blocking (synchronous `reqwest`), so callers must
//! run them off the GTK main thread, same as every other network call in this crate.

use crate::session_flow::client_for;
use crate::state::AppState;
use lilypad_core::config;

/// Resolves a pending game submission by creating a brand-new FrogLog game entry for it.
/// `igdb_title` is the exact title of the IGDB search result the user picked; enriched details
/// for it are fetched fresh (via `fetch_game_details`) rather than trusting the lightweight
/// search result, since `/search/fetch` returns the richer, create-ready field set.
pub fn resolve_as_new(state: &AppState, appid: &str, igdb_title: &str) -> Result<serde_json::Value, String> {
    let pending = config::load_pending_game_submissions();
    let entry = pending
        .iter()
        .find(|p| p.appid == appid)
        .ok_or("Pending submission not found")?
        .clone();

    let auth = state.auth.read().unwrap().clone();
    let client = client_for(&auth);

    // If IGDB has no exact match for the confirmed title, fall back to `igdb_title` itself —
    // the title the user actually confirmed — never `entry.title`, which is only LilyPad's
    // original guess (literally the exe's folder name for a non-Steam game).
    let mut payload = client
        .fetch_game_details(igdb_title)
        .unwrap_or_else(|_| serde_json::json!({ "title": igdb_title }));
    let obj = payload.as_object_mut().ok_or("Unexpected response shape")?;
    // /search/fetch names this field "dev_country"; POST /games expects "studio_country".
    if let Some(country) = obj.remove("dev_country") {
        obj.insert("studio_country".to_string(), country);
    }
    obj.insert("platform".to_string(), serde_json::json!("PC"));
    obj.insert("is_public".to_string(), serde_json::json!(true));
    // Session-tracked, matching how LilyPad actually recorded this play time.
    obj.insert("session_tracking".to_string(), serde_json::json!(true));
    // Start date is when LilyPad first saw this game being played, not today.
    if let Some(start_date) = chrono::DateTime::from_timestamp(entry.first_seen_secs as i64, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
    {
        obj.insert("start_date".to_string(), serde_json::json!(start_date));
    }
    // Prefer our own detected Steam appid over whatever (possibly null) match IGDB found.
    if let Ok(appid_num) = entry.appid.parse::<i64>() {
        obj.insert("steam_app_id".to_string(), serde_json::json!(appid_num));
    }

    let created = client.create_game(payload)?;
    let created_id = created.get("id").and_then(|v| v.as_i64()).ok_or("Created game missing id")?;
    let created_title = created.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());

    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    client.add_game_session(
        created_id as i32,
        Some(date),
        Some(entry.hours),
        Some("Session logged from LilyPad".to_string()),
        false,
        true,
        None,
    )?;

    config::remove_pending_game_submission(appid);

    // Link the exe to the newly-created game so future sessions are tracked normally instead
    // of falling through detection again. Best-effort: a failure here shouldn't fail the
    // resolve itself.
    if !entry.exe_name.is_empty() {
        if let Err(e) = config::link_process_mapping(
            &state.process_map,
            &auth,
            entry.exe_name.clone(),
            "session".to_string(),
            created_id as i32,
            created_title.or_else(|| Some(entry.title.clone())),
        ) {
            log::warn!("[LilyPad] failed to link newly-created game's exe: {e}");
        }
    }

    // Without this, library_index only refreshes every 5 minutes -- relaunching the same game
    // shortly after resolving it here would still look "not in my library" to the next
    // detection attempt and get flagged as a new game all over again.
    state.refresh_library_index();

    Ok(created)
}

/// Resolves a pending game submission by logging its accumulated hours against a game/live
/// service entry the user already has in their FrogLog library.
pub fn resolve_as_existing(
    state: &AppState,
    appid: &str,
    game_type: &str,
    game_id: i32,
    game_title: &str,
) -> Result<(), String> {
    let pending = config::load_pending_game_submissions();
    let entry = pending
        .iter()
        .find(|p| p.appid == appid)
        .ok_or("Pending submission not found")?
        .clone();

    let auth = state.auth.read().unwrap().clone();
    let client = client_for(&auth);
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let notes = Some("Logged from LilyPad's untracked-session detection".to_string());
    // Anything mapped here should end up session-tracked, not "regular" (a single running
    // hours_played total) -- matches the silent already-owned auto-link path (see the comment
    // in monitor.rs). enable_session_tracking is idempotent and preserves any pre-existing hours
    // as a "Pre-tracked hours" session, same flow as enabling it manually on the website.
    let effective_game_type = if game_type.eq_ignore_ascii_case("live") {
        "live"
    } else {
        if let Err(e) = client.enable_session_tracking(game_id) {
            log::warn!("[LilyPad] failed to enable session tracking: {e}");
        }
        "session"
    };
    if effective_game_type == "live" {
        client.add_live_service_session(game_id, Some(date), Some(entry.hours), notes, false, true, None)?;
    } else {
        client.add_game_session(game_id, Some(date), Some(entry.hours), notes, false, true, None)?;
    }
    // Best-effort: a game picked here might be a Steam-bulk-imported entry that was never
    // actually started (status "Imported", no start_date) -- now that a real session's been
    // logged against it, transition it to "In Progress". No-op if it isn't "Imported".
    if let Err(e) = client.fix_imported_status_if_needed(game_id, effective_game_type) {
        log::warn!("[LilyPad] failed to fix imported status: {e}");
    }
    config::remove_pending_game_submission(appid);

    // Link the exe to this game so future sessions are tracked normally instead of falling
    // through detection again. Uses the existing game's own confirmed title, not
    // `entry.title` (LilyPad's original guess). Best-effort, same as resolve_as_new.
    if !entry.exe_name.is_empty() {
        if let Err(e) = config::link_process_mapping(
            &state.process_map,
            &auth,
            entry.exe_name.clone(),
            effective_game_type.to_string(),
            game_id,
            Some(game_title.to_string()),
        ) {
            log::warn!("[LilyPad] failed to link exe to existing game: {e}");
        }
    }

    // See the matching comment in resolve_as_new -- avoids a 5-minute stale-library window.
    state.refresh_library_index();

    Ok(())
}
