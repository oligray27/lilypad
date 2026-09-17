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

/// One row from the `game_platform_links` table (game-identity redesign, Phase 2) — the
/// structural replacement for the flat `steam_app_id`/`psn_title_id`/etc. columns on the
/// backend. Present on both `Game` and `LiveServiceGame` (live-service platform-linking,
/// 2026-08-19 — `game_platform_links` gained a `game_type` discriminator so one table
/// covers both, see `add_game_platform_links_game_type.sql`); never `WishlistItem`,
/// which has no such join at all. Mirrors the exact JSON shape `GET /games`/
/// `GET /live-service` send (routes/games.js's and routes/liveservice.js's own
/// `platform_links` subqueries).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PlatformLink {
    pub platform_key: String,
    pub external_id: Option<serde_json::Value>,
    pub label: Option<String>,
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
    /// IGDB's real numeric id (game-identity redesign, Phase 1/2) — a much more precise
    /// cross-platform match key than title or even `steam_app_id` alone (catches, e.g., the
    /// same game already logged under a different platform with no Steam appid on record at
    /// all). `#[serde(default)]` since this key didn't always exist on `GET /games`'s
    /// response — keeps LilyPad working against a not-yet-upgraded backend deploy instead of
    /// failing to parse the whole response over one new, optional field.
    #[serde(default)]
    pub igdb_id: Option<serde_json::Value>,
    /// One row per platform actually linked to this playthrough — read preferentially over
    /// `steam_app_id` above in `library_match.rs` (see `resolve_steam_appid`) so this crate
    /// survives the game-identity redesign's eventual Phase 3 cutover (dropping the flat
    /// `steam_app_id` column from `games` entirely) without needing a synchronized LilyPad
    /// release. `#[serde(default)]` for the same "not-yet-upgraded backend" reason as `igdb_id`.
    #[serde(default)]
    pub platform_links: Option<Vec<PlatformLink>>,
}

/// Lighter "want to play" entry — same identity fields as `Game`, used to check whether a
/// detected game is already on the user's Up Next / wishlist rather than fully tracked.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WishlistItem {
    pub id: i32,
    pub title: Option<String>,
    pub steam_app_id: Option<serde_json::Value>,
    #[serde(default)]
    pub igdb_id: Option<serde_json::Value>,
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
    /// See `Game::igdb_id` — `live_service_games` gained the same column in Phase 1.
    #[serde(default)]
    pub igdb_id: Option<serde_json::Value>,
    /// See `Game::platform_links` — `game_platform_links` was extended to cover
    /// `live_service_games` too (live-service platform-linking, 2026-08-19; previously
    /// this table was deliberately out of scope, see the game-identity redesign plan's
    /// original "what NOT to change" note, now superseded). Read preferentially over
    /// `steam_app_id` above in `library_match.rs` (`resolve_steam_appid`), same
    /// forward-compat reasoning as `Game::platform_links` -- this crate survives the
    /// eventual drop of `live_service_games.steam_app_id` without needing a second
    /// synchronized LilyPad release. `#[serde(default)]` for the same "not-yet-upgraded
    /// backend" reason as `igdb_id`.
    #[serde(default)]
    pub platform_links: Option<Vec<PlatformLink>>,
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

/// What a failed request means for whether retrying it can ever work.
///
/// Every request in this client reports failures as a `String`, and changing that would ripple
/// through both frontends -- the GTK one pipes `Result<_, String>` through typed channels -- so
/// the status code is carried *in* the message by `http_error` and recovered here. Stringly, but
/// contained in one place and tested, rather than a type change across ~29 call sites, half of
/// which cannot be compiler-checked on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFailure {
    /// The token is rejected. Retrying unchanged cannot work; it needs a fresh login.
    Unauthorized,
    /// The target is gone. For a session submit this is the orphaned-mapping case -- the game
    /// was deleted or moved between types -- and is worth trying to recover from, not retrying.
    NotFound,
    /// The server already holds this submission (a `sync_ref` replay). Not a failure: the work
    /// is done, and retrying would only ask again.
    AlreadySubmitted,
    /// Asked to slow down. The same request should succeed later, untouched.
    RateLimited,
    /// The request itself is unacceptable. Retrying it identically will fail identically.
    Rejected,
    /// Network, timeout, or a server-side fault. Worth retrying unchanged.
    Transient,
}

impl ApiFailure {
    /// Whether retrying the identical request could plausibly succeed later.
    pub fn is_worth_retrying(self) -> bool {
        matches!(self, Self::RateLimited | Self::Transient)
    }

    /// Classifies an error message produced by this client.
    pub fn classify(error: &str) -> Self {
        // `http_error` formats every HTTP failure as "<status>: <message>".
        if let Some(status) = error
            .split(':')
            .next()
            .and_then(|code| code.trim().parse::<u16>().ok())
        {
            return match status {
                401 | 403 => Self::Unauthorized,
                404 => Self::NotFound,
                409 => Self::AlreadySubmitted,
                408 | 429 => Self::RateLimited,
                500..=599 => Self::Transient,
                _ => Self::Rejected,
            };
        }
        // Not an HTTP response at all. "Not logged in" is raised locally when no token exists,
        // and must not be mistaken for something a retry could fix.
        if error.eq_ignore_ascii_case("Not logged in") || error.contains("Unauthorized") {
            return Self::Unauthorized;
        }
        // Everything else reaching here is a transport failure from reqwest -- connection
        // refused, DNS, TLS, the 15s timeout -- all of which are worth retrying.
        Self::Transient
    }
}

/// Renders an HTTP failure so its status survives into the error message, which is what
/// `ApiFailure::classify` reads back. Prefer this over ad-hoc formatting.
fn http_error(status: reqwest::StatusCode, body: Option<serde_json::Value>) -> String {
    let message = body
        .as_ref()
        .and_then(|b| b["error"].as_str())
        .unwrap_or_else(|| status.canonical_reason().unwrap_or("Request failed"));
    format!("{}: {}", status.as_u16(), message)
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
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
        }
        if !res.status().is_success() {
            let body = res.text().unwrap_or_default();
            let err: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        res.json().map_err(|e: reqwest::Error| e.to_string())
    }

    /// The authenticated account's own username, from `GET /users/me`.
    ///
    /// Used to backfill `AuthConfig::username` for logins that predate it being stored, so those
    /// installs get a stable account key without the user having to log out and back in — which
    /// would lose their process-map file, since logout clears the auth the migration keys off.
    pub fn get_username(&self) -> Result<String, String> {
        let res = self
            .client
            .get(self.url("/users/me"))
            .headers(self.headers())
            .send()
            .map_err(|e: reqwest::Error| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
        }
        if !res.status().is_success() {
            return Err(http_error(res.status(), None));
        }
        let body: serde_json::Value = res.json().map_err(|e: reqwest::Error| e.to_string())?;
        body["username"]
            .as_str()
            .filter(|u| !u.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| "Response contained no username".to_string())
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
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
        }
        if !res.status().is_success() {
            let status = res.status();
            let body: serde_json::Value = res.json().unwrap_or_default();
            return Err(http_error(status, Some(body)));
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
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
        }
        if !res.status().is_success() {
            let status = res.status();
            let body: serde_json::Value = res.json().unwrap_or_default();
            return Err(http_error(status, Some(body)));
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
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
        }
        if !res.status().is_success() {
            let status = res.status();
            let body: serde_json::Value = res.json().unwrap_or_default();
            return Err(http_error(status, Some(body)));
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
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
        }
        if !res.status().is_success() {
            let status = res.status();
            let body: serde_json::Value = res.json().unwrap_or_default();
            return Err(http_error(status, Some(body)));
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
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
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

    /// Turns on session_tracking for a game via PUT /games/{id}, if it isn't on already.
    /// Leaves `hours_played` untouched, but if it's currently > 0, seeds a one-off "Pre-tracked
    /// hours" session for that amount (the backend's own established flow for this, same as
    /// enabling it manually on the website) so the game's session-aggregate total doesn't
    /// visibly drop to zero the moment session tracking turns on.
    ///
    /// Idempotent -- a fresh GET always checks the current `session_tracking` value first and
    /// no-ops if it's already `true`, rather than trusting a possibly-stale caller-supplied
    /// flag. This makes it safe to call unconditionally every time LilyPad links a process to a
    /// game, not just the first time; without this check, calling it again on an
    /// already-session-tracked game would insert a duplicate "Pre-tracked hours" session and
    /// silently inflate the total every single time that game launches.
    pub fn enable_session_tracking(&self, game_id: i32) -> Result<serde_json::Value, String> {
        let mut obj = self.game_payload_base(game_id)?;
        if obj.get("session_tracking").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Ok(serde_json::Value::Object(obj));
        }
        let existing_hours = obj.get("hours_played").and_then(Self::num_from_value);
        obj.insert("session_tracking".to_string(), serde_json::json!(true));
        // Anything LilyPad converts to session tracking gets public sessions by default --
        // only on the initial flip (the early-return above means an already-tracked game's
        // own sessions_public choice is never overridden).
        obj.insert("sessions_public".to_string(), serde_json::json!(true));
        if let Some(h) = existing_hours {
            if h > 0.0 {
                obj.insert("initial_session_hours".to_string(), serde_json::json!(h));
            }
        }
        self.put_game(game_id, serde_json::Value::Object(obj))
    }

    /// If the game is currently Completed or DNF, clears `end_date`, `dnf`, and
    /// `status_override` via PUT /games/{id} so the backend's own `computeStatus` puts it back
    /// to "In Progress". Used whenever the user explicitly logs a fresh session against a
    /// finished game -- e.g. resolving a "New Games" entry via "Continue That Entry" (whether
    /// that's a genuine replay prompt or the general "Map to Existing" picker landing on a
    /// finished game) -- since continuing to pile hours onto a game that still reads as
    /// "Completed" would be misleading. No-op if the game isn't currently finished.
    pub fn resume_finished_game(&self, game_id: i32) -> Result<serde_json::Value, String> {
        let mut obj = self.game_payload_base(game_id)?;
        let status = obj.get("status").and_then(|v| v.as_str());
        if !matches!(status, Some("Completed") | Some("DNF")) {
            return Ok(serde_json::Value::Object(obj));
        }
        obj.insert("dnf".to_string(), serde_json::json!(false));
        obj.insert("end_date".to_string(), serde_json::Value::Null);
        obj.insert("status_override".to_string(), serde_json::Value::Null);
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

    /// Attaches a Steam appid to an existing game via PUT /games/{id}, so the backend's
    /// own dual-write (`routes/games.js`'s `PUT /:id`, `if (steam_app_id) {
    /// recordPlatformLink(...) }`) creates a real `game_platform_links` 'steam' row.
    ///
    /// REAL GAP FOUND LIVE (2026-08-15, testing vibefroggy's "Pokémon Pokopia" attach):
    /// resolving a pending game submission against an *existing* FrogLog entry (either
    /// via the manual "Map to Existing" picker, or the game-identity redesign's newer
    /// automatic igdb_id-match attach) has never once recorded the Steam appid the
    /// session was actually detected under -- logging the session against the right
    /// entry was already correct, but the fact that this exact playthrough is *also* now
    /// known to be on Steam was silently dropped every single time, since the "New
    /// Games" feature first shipped. Not something the LilyPad slice of the game-identity
    /// redesign introduced -- just newly noticed while manually testing it, since that's
    /// the first time anyone tried this exact path against an entry with no Steam link
    /// yet at all.
    ///
    /// Only sets it if the existing game has no `steam_app_id` of its own already --
    /// never overwrites a different one (same caution this whole project applies
    /// everywhere else it touches platform ids, the "LAW0298" bug class).
    pub fn attach_steam_app_id_if_missing(&self, game_id: i32, appid: i64) -> Result<serde_json::Value, String> {
        let mut obj = self.game_payload_base(game_id)?;
        if obj.get("steam_app_id").and_then(Self::num_from_value).is_some() {
            return Ok(serde_json::Value::Object(obj));
        }
        obj.insert("steam_app_id".to_string(), serde_json::json!(appid));
        self.put_game(game_id, serde_json::Value::Object(obj))
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
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
        }
        if !res.status().is_success() {
            let status = res.status();
            let body: serde_json::Value = res.json().unwrap_or_default();
            return Err(http_error(status, Some(body)));
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
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
        }
        if !res.status().is_success() {
            let status = res.status();
            let body: serde_json::Value = res.json().unwrap_or_default();
            return Err(http_error(status, Some(body)));
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
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
        }
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Err("not_found".to_string());
        }
        if !res.status().is_success() {
            let status = res.status();
            let body: serde_json::Value = res.json().unwrap_or_default();
            return Err(http_error(status, Some(body)));
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
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
        }
        if !res.status().is_success() {
            let status = res.status();
            let body: serde_json::Value = res.json().unwrap_or_default();
            return Err(http_error(status, Some(body)));
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
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
        }
        if res.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err("Rate limited: too many games created this hour".to_string());
        }
        if !res.status().is_success() {
            let status = res.status();
            let body: serde_json::Value = res.json().unwrap_or_default();
            return Err(http_error(status, Some(body)));
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
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
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
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
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
            return Err(http_error(reqwest::StatusCode::UNAUTHORIZED, None));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The classifier reads a status code back out of an error message, so the format
    /// `http_error` writes and the format `classify` parses have to stay in step. If these two
    /// ever drift, every failure silently becomes `Transient` and retries for ever.
    #[test]
    fn http_failures_are_classified_by_their_status() {
        let cases = [
            (reqwest::StatusCode::UNAUTHORIZED, ApiFailure::Unauthorized),
            (reqwest::StatusCode::FORBIDDEN, ApiFailure::Unauthorized),
            (reqwest::StatusCode::NOT_FOUND, ApiFailure::NotFound),
            (reqwest::StatusCode::CONFLICT, ApiFailure::AlreadySubmitted),
            (reqwest::StatusCode::TOO_MANY_REQUESTS, ApiFailure::RateLimited),
            (reqwest::StatusCode::BAD_REQUEST, ApiFailure::Rejected),
            (reqwest::StatusCode::UNPROCESSABLE_ENTITY, ApiFailure::Rejected),
            (reqwest::StatusCode::INTERNAL_SERVER_ERROR, ApiFailure::Transient),
            (reqwest::StatusCode::BAD_GATEWAY, ApiFailure::Transient),
        ];
        for (status, expected) in cases {
            let rendered = http_error(status, None);
            assert_eq!(
                ApiFailure::classify(&rendered), expected,
                "{status} rendered as {rendered:?} classified wrongly",
            );
        }
        // A server-supplied message must not displace the code that precedes it.
        let body = serde_json::json!({ "error": "Session with this sync_ref is already being created" });
        assert_eq!(
            ApiFailure::classify(&http_error(reqwest::StatusCode::CONFLICT, Some(body))),
            ApiFailure::AlreadySubmitted
        );
    }

    /// Failures that never reached the server at all.
    #[test]
    fn local_and_transport_failures_are_classified_without_a_status() {
        // Raised locally when there is no token; a retry cannot fix it.
        assert_eq!(ApiFailure::classify("Not logged in"), ApiFailure::Unauthorized);
        // Real reqwest transport text, which is worth retrying unchanged.
        assert_eq!(
            ApiFailure::classify("error sending request for url (https://api.froglog.co.uk/api/games/1/sessions)"),
            ApiFailure::Transient
        );
        assert_eq!(ApiFailure::classify("operation timed out"), ApiFailure::Transient);
    }

    /// Only these two are worth handing back to the retry queue; the rest need a human or are
    /// already done.
    #[test]
    fn only_transient_failures_are_worth_retrying() {
        assert!(ApiFailure::Transient.is_worth_retrying());
        assert!(ApiFailure::RateLimited.is_worth_retrying());
        for settled in [
            ApiFailure::Unauthorized,
            ApiFailure::NotFound,
            ApiFailure::AlreadySubmitted,
            ApiFailure::Rejected,
        ] {
            assert!(!settled.is_worth_retrying(), "{settled:?} should not be retried blindly");
        }
    }
}
