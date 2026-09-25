//! Requests from the Decky panel. Each runs on its own thread (most make network calls), so a
//! slow one never holds up the others or the event stream.

use crate::frontend::DeckyFrontend;
use lilypad_core::config::{process_map_path_for_auth, AuthConfig};
use lilypad_core::engine::flow::{client_for, gaming_mode_note, DEFAULT_AUTO_SUBMIT_NOTE};
use lilypad_core::engine::{actions, EngineState, Frontend, FrontendRef, DEFAULT_API_URL};
use lilypad_core::resolution::Choice;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct Engine {
    pub state: EngineState,
    pub frontend: Arc<DeckyFrontend>,
    /// False while the desktop app holds the tracker lock: queues belong to it then.
    pub tracking: Arc<AtomicBool>,
}

fn args<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|e| format!("bad arguments: {e}"))
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}

#[derive(Deserialize)]
struct Login { username: String, password: String }

/// The desktop app's per-type auto-submit settings don't apply here: Gaming Mode submits
/// every session, so the panel leaves them alone.
#[derive(Deserialize)]
struct Settings {
    share_now_playing: Option<bool>,
    detect_unmapped: Option<bool>,
    /// Sent with every session; blank for none.
    session_note: Option<String>,
}

#[derive(Deserialize)]
struct Id { id: String }

#[derive(Deserialize)]
struct AppId { appid: String }

#[derive(Deserialize)]
struct Query { query: String }

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ChoiceArgs {
    New { igdb_title: String },
    Replay,
    Existing { game_type: String, game_id: i32, title: String },
}

#[derive(Deserialize)]
struct Resolve { appid: String, choice: ChoiceArgs }

impl Engine {
    pub fn handle(&self, cmd: &str, a: Value) -> Result<Value, String> {
        let needs_tracking = !matches!(cmd, "status" | "ping");
        if needs_tracking && !self.tracking.load(Ordering::SeqCst) {
            return Err("LilyPad is running on the desktop; its sessions are managed there.".into());
        }
        match cmd {
            "ping" => Ok(json!("pong")),
            "status" => Ok(self.status()),
            "login" => self.login(args(a)?),
            "logout" => self.state.change_account(AuthConfig::default()).map(|_| json!(null)),
            "settings_get" => Ok(self.settings()),
            "settings_set" => self.set_settings(args(a)?),
            "decisions" => to_value(self.frontend.decisions.lock().unwrap().clone()),
            "decision_submit" => self.decision_submit(args(a)?),
            "decision_discard" => self.decision_discard(args(a)?),
            "pending" => {
                let account = self.state.account().ok_or("Not logged in")?;
                let store = self.state.store();
                to_value(store.pending_sessions(&account).ok_or_else(|| store.error().unwrap_or_default())?)
            }
            "pending_retry" => {
                let Id { id } = args(a)?;
                actions::retry_pending(&self.state, &id).map(|_| json!(null))
            }
            "pending_delete" => {
                let Id { id } = args(a)?;
                actions::discard_session(&self.state, Some(&id)).map(|_| json!(null))
            }
            "new_games" => {
                let account = self.state.account().ok_or("Not logged in")?;
                let store = self.state.store();
                to_value(store.new_games(&account).ok_or_else(|| store.error().unwrap_or_default())?)
            }
            "new_game_dismiss" => {
                let AppId { appid } = args(a)?;
                let account = self.state.account().ok_or("Not logged in")?;
                self.state.store().dismiss_new_game(&account, &appid).map(|_| json!(null)).ok_or_else(|| "Session storage is unavailable".into())
            }
            "new_game_resolve" => self.resolve(args(a)?),
            "igdb_search" => {
                let Query { query } = args(a)?;
                let auth = self.state.auth.read().unwrap().clone();
                to_value(client_for(&auth).search_igdb(&query)?)
            }
            "library" => self.library(),
            "force_stop" => {
                let frontend: FrontendRef = self.frontend.clone();
                Ok(json!(actions::force_stop(&self.state, &frontend)))
            }
            other => Err(format!("unknown command {other:?}")),
        }
    }

    fn status(&self) -> Value {
        let tracking = self.tracking.load(Ordering::SeqCst);
        let auth = self.state.auth.read().unwrap().clone();
        let store = self.state.store();
        let counts = if tracking { store.counts(self.state.account().as_ref()) } else { Default::default() };
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "tracking": tracking,
            "logged_in": auth.token.is_some(),
            "username": auth.username,
            "now_tracking": self.state.now_tracking_title(),
            "pending": counts.pending.map(|p| p + counts.unowned.unwrap_or(0)),
            "new_games": counts.new_games,
            "decisions": self.frontend.decisions.lock().unwrap().len(),
            "storage_error": if tracking { store.error() } else { None },
        })
    }

    fn login(&self, Login { username, password }: Login) -> Result<Value, String> {
        let client = lilypad_core::api::FroglogClient::new(DEFAULT_API_URL.to_string());
        let res = client.login(username.trim(), &password, true)?;
        self.state.change_account(AuthConfig {
            base_url: Some(DEFAULT_API_URL.to_string()),
            token: Some(res.token),
            username: res.username.or(Some(username.trim().to_string())),
        })?;
        let state = self.state.clone();
        std::thread::spawn(move || {
            state.refresh_installed_games();
            state.refresh_library_index();
        });
        Ok(self.status())
    }

    fn settings(&self) -> Value {
        let map = self.state.process_map.read().unwrap();
        json!({
            "share_now_playing": map.share_now_playing,
            "detect_unmapped": !map.disable_unmapped_game_detection,
            "session_note": map.gaming_mode_note.clone().unwrap_or_else(|| DEFAULT_AUTO_SUBMIT_NOTE.to_string()),
        })
    }

    fn set_settings(&self, s: Settings) -> Result<Value, String> {
        let auth = self.state.auth.read().unwrap().clone();
        if auth.token.is_none() {
            return Err("Not logged in".into());
        }
        let mut map = self.state.process_map.read().unwrap().clone();
        if let Some(v) = s.share_now_playing { map.share_now_playing = v; }
        if let Some(v) = s.detect_unmapped { map.disable_unmapped_game_detection = !v; }
        if let Some(v) = s.session_note { map.gaming_mode_note = Some(v.trim().to_string()); }
        map.save_to(&process_map_path_for_auth(&auth)).map_err(|e| format!("Could not save settings: {e}"))?;
        *self.state.process_map.write().unwrap() = map;
        Ok(self.settings())
    }

    fn decision_submit(&self, Id { id }: Id) -> Result<Value, String> {
        let decision = self.frontend.find_decision(&id).ok_or("That session has already been dealt with")?;
        let attempt = actions::submit_decision(
            &self.state, &decision.data.mapping, decision.data.ledger_id.clone(), decision.hours,
            gaming_mode_note(&self.state), false, true,
        );
        // Submitted, or safely in Pending Submissions: either way nothing is left to decide here.
        if !matches!(attempt, actions::Attempt::Failed(_)) {
            self.frontend.take_decision(&id);
            self.frontend.changed();
        }
        to_value(attempt)
    }

    fn decision_discard(&self, Id { id }: Id) -> Result<Value, String> {
        let decision = self.frontend.find_decision(&id).ok_or("That session has already been dealt with")?;
        actions::discard_session(&self.state, decision.data.ledger_id.as_deref())?;
        self.frontend.take_decision(&id);
        self.frontend.changed();
        Ok(json!(null))
    }

    fn resolve(&self, Resolve { appid, choice }: Resolve) -> Result<Value, String> {
        let choice = match choice {
            ChoiceArgs::New { igdb_title } => Choice::New { igdb_title },
            ChoiceArgs::Replay => Choice::Replay,
            ChoiceArgs::Existing { game_type, game_id, title } => Choice::Existing { game_type, game_id, title },
        };
        let resolved = actions::resolve_new_game(&self.state, &appid, choice)?;
        self.frontend.changed();
        Ok(json!({ "game_type": resolved.game_type, "game_id": resolved.game_id, "title": resolved.title, "sessions_logged": resolved.sessions_logged }))
    }

    /// The user's games and live-service entries, for "add to a game I already have".
    fn library(&self) -> Result<Value, String> {
        let auth = self.state.auth.read().unwrap().clone();
        let client = client_for(&auth);
        let mut rows: Vec<Value> = client
            .get_games()?
            .into_iter()
            .map(|g| {
                let game_type = if g.session_tracking.unwrap_or(false) { "session" } else { "regular" };
                json!({ "game_type": game_type, "game_id": g.id, "title": g.title.unwrap_or_default(), "status": g.status })
            })
            .collect();
        rows.extend(client.get_live_service_games()?.into_iter().map(|g| {
            json!({ "game_type": "live", "game_id": g.id, "title": g.title.unwrap_or_default(), "status": null })
        }));
        rows.sort_by_key(|r| r["title"].as_str().unwrap_or_default().to_lowercase());
        Ok(Value::Array(rows))
    }
}
