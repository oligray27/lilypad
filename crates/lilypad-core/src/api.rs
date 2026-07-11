//! Froglog API client: login, games, live-service, sessions.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    #[serde(rename = "rememberMe")]
    pub remember_me: bool,
}

#[derive(Debug, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub username: Option<String>,
}

/// API uses snake_case (see update_hours.py). We use snake_case so GET returns correct hours_played.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Game {
    pub id: i32,
    pub title: Option<String>,
    pub hours_played: Option<serde_json::Value>, // DECIMAL from pg arrives as a JSON string
    pub description: Option<String>,
    pub img: Option<String>,
    pub platform: Option<String>,
    pub genre: Option<String>,
    pub dev: Option<String>,
    pub studio_country: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub rating: Option<serde_json::Value>,
    pub dnf: Option<bool>,
    pub is_public: Option<bool>,
    pub rel_date: Option<String>,
    pub status: Option<String>,
    pub forklift_certified: Option<bool>,
    pub session_tracking: Option<bool>,
    /// Steam appid, when this game was added via Steam sync or otherwise linked to a Steam
    /// store page. Arrives as a JSON number or numeric string depending on the pg column path.
    pub steam_app_id: Option<serde_json::Value>,
}

/// Lighter "want to play" entry — same identity fields as `Game`, used to check whether a
/// detected game is already on the user's Up Next / wishlist rather than fully tracked.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WishlistItem {
    pub id: i32,
    pub title: Option<String>,
    pub steam_app_id: Option<serde_json::Value>,
}

// Backend returns snake_case. total_hours is SUM(DECIMAL) → string; session_count is COUNT() → bigint string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveServiceGame {
    pub id: i32,
    pub title: Option<String>,
    pub total_hours: Option<serde_json::Value>,
    pub session_count: Option<serde_json::Value>,
    pub last_session_date: Option<String>,
    pub steam_app_id: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct AddSessionBody {
    pub date: Option<String>,
    pub hours: Option<f64>,
    pub notes: Option<String>,
    pub spoiler: bool,
    pub is_public: bool,
    /// Opaque source identifier (e.g. "lilypad-mobile:{system}|{path}") a syncing
    /// client can stamp on a session to recognize it again later, without
    /// polluting the user-visible `notes` field. Unused by desktop LilyPad.
    pub sync_ref: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NowPlayingBody {
  pub game_id: i32,
  pub game_type: String,
  pub title: Option<String>,
  pub started_at: Option<String>,
}

/// Synchronous Froglog API client (blocking reqwest for use from any thread).
pub struct FroglogClient {
    base_url: String,
    token: Option<String>,
    client: reqwest::blocking::Client,
}

impl FroglogClient {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: None,
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap(),
        }
    }

    pub fn set_token(&mut self, token: Option<String>) {
        self.token = token;
    }

    fn url(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/api") {
            format!("{}{}", base, path)
        } else {
            format!("{}/api{}", base, path)
        }
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        if let Some(t) = &self.token {
            let v = format!("Bearer {}", t);
            if let Ok(val) = v.parse::<reqwest::header::HeaderValue>() {
                h.insert(reqwest::header::AUTHORIZATION, val);
            }
        }
        h
    }

    pub fn login(&self, username: &str, password: &str, remember_me: bool) -> Result<LoginResponse, String> {
        let body = LoginRequest {
            username: username.to_string(),
            password: password.to_string(),
            remember_me,
        };
        let res = self
            .client
            .post(self.url("/auth/login"))
            .headers(self.headers())
            .json(&body)
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Invalid credentials".to_string());
        }
        if !res.status().is_success() {
            let err: serde_json::Value = res.json().unwrap_or_default();
            return Err(err["error"].as_str().unwrap_or("Login failed").to_string());
        }
        res.json().map_err(|e: reqwest::Error| e.to_string())
    }

    /// Raw GET /games so we can parse response without strict Game struct (avoids decode errors).
    fn get_games_raw(&self) -> Result<Vec<serde_json::Value>, String> {
        let res = self
            .client
            .get(self.url("/games"))
            .headers(self.headers())
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized".to_string());
        }
        if !res.status().is_success() {
            let body = res.text().unwrap_or_default();
            let err: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        res.json().map_err(|e: reqwest::Error| e.to_string())
    }

    pub fn get_games(&self) -> Result<Vec<Game>, String> {
        let list: Vec<serde_json::Value> = self.get_games_raw()?;
        list.into_iter()
            .map(|v| serde_json::from_value(v).map_err(|e: serde_json::Error| e.to_string()))
            .collect()
    }

    pub fn get_live_service_games(&self) -> Result<Vec<LiveServiceGame>, String> {
        let res = self
            .client
            .get(self.url("/live-service"))
            .headers(self.headers())
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized".to_string());
        }
        if !res.status().is_success() {
            let err: serde_json::Value = res.json().unwrap_or_default();
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        res.json().map_err(|e: reqwest::Error| e.to_string())
    }

    pub fn get_wishlist(&self) -> Result<Vec<WishlistItem>, String> {
        let res = self
            .client
            .get(self.url("/wishlist"))
            .headers(self.headers())
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized".to_string());
        }
        if !res.status().is_success() {
            let err: serde_json::Value = res.json().unwrap_or_default();
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        res.json().map_err(|e: reqwest::Error| e.to_string())
    }

    pub fn add_game_session(
        &self,
        game_id: i32,
        date: Option<String>,
        hours: Option<f64>,
        notes: Option<String>,
        spoiler: bool,
        is_public: bool,
        sync_ref: Option<String>,
    ) -> Result<serde_json::Value, String> {
        let body = AddSessionBody { date, hours, notes, spoiler, is_public, sync_ref };
        let res = self
            .client
            .post(self.url(&format!("/games/{}/sessions", game_id)))
            .headers(self.headers())
            .json(&body)
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized".to_string());
        }
        if !res.status().is_success() {
            let err: serde_json::Value = res.json().unwrap_or_default();
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        res.json().map_err(|e: reqwest::Error| e.to_string())
    }

    pub fn add_live_service_session(
        &self,
        game_id: i32,
        date: Option<String>,
        hours: Option<f64>,
        notes: Option<String>,
        spoiler: bool,
        is_public: bool,
        sync_ref: Option<String>,
    ) -> Result<serde_json::Value, String> {
        let body = AddSessionBody { date, hours, notes, spoiler, is_public, sync_ref };
        let res = self
            .client
            .post(self.url(&format!("/live-service/{}/sessions", game_id)))
            .headers(self.headers())
            .json(&body)
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized".to_string());
        }
        if !res.status().is_success() {
            let err: serde_json::Value = res.json().unwrap_or_default();
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        res.json().map_err(|e: reqwest::Error| e.to_string())
    }

    /// Fetch a single game (raw JSON) from GET /games by id.
    pub fn get_game_raw(&self, game_id: i32) -> Result<serde_json::Value, String> {
        let games = self.get_games_raw()?;
        games
            .into_iter()
            .find(|v| v.get("id").and_then(|i| i.as_i64()) == Some(game_id as i64))
            .ok_or_else(|| "Game not found".to_string())
    }

    /// Parse a numeric field that pg may serialize as a JSON string (DECIMAL columns).
    pub fn num_from_value(v: &serde_json::Value) -> Option<f64> {
        v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    }

    /// Add hours to a regular game (read current total, PUT current + add_hours).
    pub fn update_game_hours(&self, game_id: i32, add_hours: f64) -> Result<serde_json::Value, String> {
        let existing = self.get_game_raw(game_id)?;
        let current = existing
            .get("hours_played")
            .and_then(Self::num_from_value)
            .unwrap_or(0.0);
        self.set_game_hours_total(game_id, current + add_hours)
    }

    /// PUT /games/{id} with a full games-object payload. Shared by every path that
    /// needs to echo GET's object back (set_game_hours_total, enable_session_tracking)
    /// so the backend's full-overwrite UPDATE never silently drops fields.
    fn put_game(&self, game_id: i32, payload: serde_json::Value) -> Result<serde_json::Value, String> {
        let res = self
            .client
            .put(self.url(&format!("/games/{}", game_id)))
            .headers(self.headers())
            .json(&payload)
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized".to_string());
        }
        if !res.status().is_success() {
            let status = res.status();
            let body_res = res.text().unwrap_or_default();
            let err: serde_json::Value = serde_json::from_str(&body_res).unwrap_or_default();
            let msg = err["error"]
                .as_str()
                .or_else(|| err["message"].as_str())
                .unwrap_or("Request failed");
            return Err(format!("{}: {}", status, msg));
        }
        let body = res.text().map_err(|e: reqwest::Error| e.to_string())?;
        if body.trim().is_empty() {
            return Ok(serde_json::json!({}));
        }
        Ok(serde_json::from_str(&body).unwrap_or(serde_json::json!({})))
    }

    /// Fetch a game and strip the GET-only aggregate fields, ready to be re-sent on a PUT.
    fn game_payload_base(&self, game_id: i32) -> Result<serde_json::Map<String, serde_json::Value>, String> {
        let existing = self.get_game_raw(game_id)?;
        let mut obj = existing.as_object().cloned().ok_or("Game object expected")?;
        // Aggregates from the GET's session JOIN — not games columns, don't send them back.
        for k in ["total_hours", "session_count", "last_session_date"] {
            obj.remove(k);
        }
        obj.remove("initial_session_hours");
        Ok(obj)
    }

    /// Set a regular game's absolute hours_played total via PUT /games/{id}.
    /// Echoes back every field from GET so the backend's full-overwrite UPDATE preserves
    /// metadata (steam_app_id, igdb_slug, status_override, ...) and session_tracking —
    /// a PUT with session_tracking falsy deletes the game's sessions server-side.
    pub fn set_game_hours_total(&self, game_id: i32, new_total_hours: f64) -> Result<serde_json::Value, String> {
        let mut obj = self.game_payload_base(game_id)?;
        let new_hours = (new_total_hours * 100.0).round() / 100.0;
        let new_hours = if new_hours < 0.01 { 0.01 } else { new_hours };
        obj.insert("hours_played".to_string(), serde_json::json!(new_hours));
        self.put_game(game_id, serde_json::Value::Object(obj))
    }

    /// Turn on session_tracking for a game via PUT /games/{id}, leaving hours_played
    /// untouched. When `initial_session_hours` is given, the backend seeds a one-off
    /// "Pre-tracked hours" session so the game's session-aggregate total doesn't
    /// visibly drop to zero the moment session tracking is enabled.
    pub fn enable_session_tracking(
        &self,
        game_id: i32,
        initial_session_hours: Option<f64>,
    ) -> Result<serde_json::Value, String> {
        let mut obj = self.game_payload_base(game_id)?;
        obj.insert("session_tracking".to_string(), serde_json::json!(true));
        if let Some(h) = initial_session_hours {
            if h > 0.0 {
                obj.insert("initial_session_hours".to_string(), serde_json::json!(h));
            }
        }
        self.put_game(game_id, serde_json::Value::Object(obj))
    }

    /// If the game is currently in "Imported" status (added via FrogLog's Steam library bulk
    /// import, which sets `hours_played` from Steam but never a `start_date` -- so it shows as
    /// "owned" but never actually started), transitions it to "In Progress" by setting
    /// `start_date`, mirroring how a brand-new game created from a detected LilyPad session
    /// already gets a `start_date`. The backend recomputes `status` from `start_date` itself
    /// (see `computeStatus` in `backend/routes/games.js`), so setting the date is enough -- no
    /// need to touch `status` in the payload directly.
    ///
    /// Uses the earliest session already logged for the game, if any (rare for a freshly
    /// imported game, since the bulk import doesn't create session rows, but possible if the
    /// user logged one manually before LilyPad got to it); falls back to today otherwise. No-op
    /// if the game isn't "Imported" -- this is meant to be called opportunistically every time
    /// LilyPad links a process to an existing game, not just once.
    pub fn fix_imported_status_if_needed(&self, game_id: i32, game_type: &str) -> Result<(), String> {
        // "Imported" is a `games`-table-only concept (set by the Steam bulk-import route) --
        // live-service games are a separate table/concept entirely.
        if !game_type.eq_ignore_ascii_case("regular") && !game_type.eq_ignore_ascii_case("session") {
            return Ok(());
        }
        let mut obj = self.game_payload_base(game_id)?;
        if obj.get("status").and_then(|v| v.as_str()) != Some("Imported") {
            return Ok(());
        }
        let sessions = self.get_game_sessions(game_id).unwrap_or_default();
        let earliest_session_date = sessions
            .iter()
            .filter_map(|s| s.get("date").and_then(|d| d.as_str()))
            .min()
            .map(|s| s.to_string());
        let start_date = earliest_session_date.unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
        obj.insert("start_date".to_string(), serde_json::json!(start_date));
        self.put_game(game_id, serde_json::Value::Object(obj))?;
        Ok(())
    }

    /// GET /games/{id}/sessions — raw session rows (id, date, hours, notes, ...).
    pub fn get_game_sessions(&self, game_id: i32) -> Result<Vec<serde_json::Value>, String> {
        let res = self
            .client
            .get(self.url(&format!("/games/{}/sessions", game_id)))
            .headers(self.headers())
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized".to_string());
        }
        if !res.status().is_success() {
            let err: serde_json::Value = res.json().unwrap_or_default();
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        res.json().map_err(|e: reqwest::Error| e.to_string())
    }

    /// GET /live-service/{id}/sessions — raw session rows.
    pub fn get_live_service_sessions(&self, game_id: i32) -> Result<Vec<serde_json::Value>, String> {
        let res = self
            .client
            .get(self.url(&format!("/live-service/{}/sessions", game_id)))
            .headers(self.headers())
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized".to_string());
        }
        if !res.status().is_success() {
            let err: serde_json::Value = res.json().unwrap_or_default();
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        res.json().map_err(|e: reqwest::Error| e.to_string())
    }

    /// GET /search/fetch?title= — IGDB-enriched details for a single title.
    /// Returns Err("not_found") when IGDB has no match (backend 404).
    pub fn fetch_game_details(&self, title: &str) -> Result<serde_json::Value, String> {
        let res = self
            .client
            .get(self.url("/search/fetch"))
            .query(&[("title", title)])
            .headers(self.headers())
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized".to_string());
        }
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Err("not_found".to_string());
        }
        if !res.status().is_success() {
            let err: serde_json::Value = res.json().unwrap_or_default();
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        res.json().map_err(|e: reqwest::Error| e.to_string())
    }

    /// GET /search?q= — multi-result IGDB search (title, cover_image, released, etc.). Each
    /// result's `name` field can be passed to `fetch_game_details` to get the richer,
    /// create-ready object for that specific title.
    pub fn search_igdb(&self, query: &str) -> Result<Vec<serde_json::Value>, String> {
        let res = self
            .client
            .get(self.url("/search"))
            .query(&[("q", query)])
            .headers(self.headers())
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized".to_string());
        }
        if !res.status().is_success() {
            let err: serde_json::Value = res.json().unwrap_or_default();
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        res.json().map_err(|e: reqwest::Error| e.to_string())
    }

    /// POST /games — create a game; returns the created row (includes id).
    pub fn create_game(&self, payload: serde_json::Value) -> Result<serde_json::Value, String> {
        let res = self
            .client
            .post(self.url("/games"))
            .headers(self.headers())
            .json(&payload)
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized".to_string());
        }
        if res.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err("Rate limited: too many games created this hour".to_string());
        }
        if !res.status().is_success() {
            let err: serde_json::Value = res.json().unwrap_or_default();
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        res.json().map_err(|e: reqwest::Error| e.to_string())
    }

    pub fn set_now_playing(
        &self,
        game_id: i32,
        game_type: String,
        title: Option<String>,
        started_at: Option<String>,
    ) -> Result<serde_json::Value, String> {
        let body = NowPlayingBody { game_id, game_type, title, started_at };
        log::info!("[LilyPad] set_now_playing request: {:?}", body);
        let res = self
            .client
            .put(self.url("/users/me/now-playing"))
            .headers(self.headers())
            .json(&body)
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized".to_string());
        }
        if !res.status().is_success() {
            let err: serde_json::Value = res.json().unwrap_or_default();
            log::warn!("[LilyPad] set_now_playing failed: {:?}", err);
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        let json = res.json().map_err(|e: reqwest::Error| e.to_string())?;
        log::info!("[LilyPad] set_now_playing success: {:?}", json);
        Ok(json)
    }

    pub fn clear_now_playing(&self) -> Result<(), String> {
        log::info!("[LilyPad] clear_now_playing request");
        let res = self
            .client
            .delete(self.url("/users/me/now-playing"))
            .headers(self.headers())
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized".to_string());
        }
        if !res.status().is_success() {
            let err: serde_json::Value = res.json().unwrap_or_default();
            log::warn!("[LilyPad] clear_now_playing failed: {:?}", err);
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        log::info!("[LilyPad] clear_now_playing success");
        Ok(())
    }

    pub fn set_show_current_session(&self, enabled: bool) -> Result<serde_json::Value, String> {
        let body = serde_json::json!({ "showCurrentSession": enabled });
        log::info!("[LilyPad] set_show_current_session request: {:?}", body);
        let res = self
            .client
            .put(self.url("/users/current-session-visibility"))
            .headers(self.headers())
            .json(&body)
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Unauthorized".to_string());
        }
        if !res.status().is_success() {
            let err: serde_json::Value = res.json().unwrap_or_default();
            log::warn!("[LilyPad] set_show_current_session failed: {:?}", err);
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        let json = res.json().map_err(|e: reqwest::Error| e.to_string())?;
        log::info!("[LilyPad] set_show_current_session success: {:?}", json);
        Ok(json)
    }
}
