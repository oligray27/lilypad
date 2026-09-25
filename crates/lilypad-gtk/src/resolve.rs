//! Resolving a "New Games" entry into a real FrogLog entry. The work -- including making every
//! step safe to retry -- lives in `lilypad_core::resolution`, shared with the Tauri build; this
//! only supplies GTK's account, library snapshot and follow-ups. Blocking (synchronous
//! `reqwest`), so callers must run these off the GTK main thread.

use crate::session_flow::client_for;
use crate::state::AppState;
use lilypad_core::resolution::{self, Choice, Resolution};

fn run(state: &AppState, appid: &str, choice: Choice) -> Result<Resolution, String> {
    // Credentials and account from one snapshot, so a login change mid-resolution cannot pair
    // one account's token with another's entry.
    let (auth, account) = state.auth_and_account().ok_or("Not logged in")?;
    let client = client_for(&auth);
    // A snapshot, so no lock is held across the network calls.
    let library = state.library_index.read().unwrap().clone();
    let resolved = resolution::resolve(&client, &state.store(), &account, &library, appid, choice)?;
    resolution::link_resolved(&state.process_map, &auth, &resolved);
    // Without this the library only refreshes every five minutes, so relaunching the game
    // straight away would still look "not in my library" (or still finished) and be flagged again.
    state.refresh_library_index();
    Ok(resolved)
}

/// Creates a brand-new FrogLog entry from the IGDB title the user confirmed.
pub fn resolve_as_new(state: &AppState, appid: &str, igdb_title: &str) -> Result<serde_json::Value, String> {
    let resolved = run(state, appid, Choice::New { igdb_title: igdb_title.to_string() })?;
    // Attached to an existing entry instead (the same game, already owned): report that entry.
    Ok(resolved.created.unwrap_or_else(|| serde_json::json!({ "id": resolved.game_id, "title": resolved.title })))
}

/// Creates a new entry as a replay of the Completed/DNF entry this appid matched.
pub fn resolve_as_replay(state: &AppState, appid: &str) -> Result<serde_json::Value, String> {
    let resolved = run(state, appid, Choice::Replay)?;
    Ok(resolved.created.unwrap_or_else(|| serde_json::json!({ "id": resolved.game_id, "title": resolved.title })))
}

/// Logs the entry's sessions against a game/live-service entry the user already has.
pub fn resolve_as_existing(
    state: &AppState,
    appid: &str,
    game_type: &str,
    game_id: i32,
    game_title: &str,
) -> Result<(), String> {
    run(state, appid, Choice::Existing {
        game_type: game_type.to_string(),
        game_id,
        title: game_title.to_string(),
    })
    .map(|_| ())
}
