//! Resolving a "New Games" entry into a real FrogLog entry. The work -- including making every
//! step safe to retry -- lives in `lilypad_core::resolution`, run through the shared engine.
//! Blocking (synchronous `reqwest`), so callers must run these off the GTK main thread.

use crate::state::AppState;
use lilypad_core::engine::actions::resolve_new_game;
use lilypad_core::resolution::Choice;

/// Creates a brand-new FrogLog entry from the IGDB title the user confirmed.
pub fn resolve_as_new(state: &AppState, appid: &str, igdb_title: &str) -> Result<serde_json::Value, String> {
    let resolved = resolve_new_game(state, appid, Choice::New { igdb_title: igdb_title.to_string() })?;
    // Attached to an existing entry instead (the same game, already owned): report that entry.
    Ok(resolved.created.unwrap_or_else(|| serde_json::json!({ "id": resolved.game_id, "title": resolved.title })))
}

/// Creates a new entry as a replay of the Completed/DNF entry this appid matched.
pub fn resolve_as_replay(state: &AppState, appid: &str) -> Result<serde_json::Value, String> {
    let resolved = resolve_new_game(state, appid, Choice::Replay)?;
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
    resolve_new_game(state, appid, Choice::Existing {
        game_type: game_type.to_string(),
        game_id,
        title: game_title.to_string(),
    })
    .map(|_| ())
}
