//! Froglog API client: login, games, live-service, sessions.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
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
}

// Backend returns snake_case. total_hours is SUM(DECIMAL) → string; session_count is COUNT() → bigint string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveServiceGame {
    pub id: i32,
    pub title: Option<String>,
    pub total_hours: Option<serde_json::Value>,
    pub session_count: Option<serde_json::Value>,
    pub last_session_date: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AddSessionBody {
    pub date: Option<String>,
    pub hours: Option<f64>,
    pub notes: Option<String>,
    pub spoiler: bool,
    pub is_public: bool,
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

    pub fn login(&self, username: &str, password: &str) -> Result<LoginResponse, String> {
        let body = LoginRequest {
            username: username.to_string(),
            password: password.to_string(),
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

    pub fn add_game_session(
        &self,
        game_id: i32,
        date: Option<String>,
        hours: Option<f64>,
        notes: Option<String>,
        spoiler: bool,
        is_public: bool,
    ) -> Result<serde_json::Value, String> {
        let body = AddSessionBody { date, hours, notes, spoiler, is_public };
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
    ) -> Result<serde_json::Value, String> {
        let body = AddSessionBody { date, hours, notes, spoiler, is_public };
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

    /// Update a regular game's hours via PUT /games/{id} with body (snake_case).
    /// Builds payload from existing game so backend receives exactly the fields it expects.
    pub fn update_game_hours(&self, game_id: i32, add_hours: f64) -> Result<serde_json::Value, String> {
        let games = self.get_games_raw()?;
        let existing = games
            .into_iter()
            .find(|v| v.get("id").and_then(|i| i.as_i64()) == Some(game_id as i64))
            .ok_or("Game not found")?;
        let obj = existing.as_object().ok_or("Game object expected")?;
        let num_from = |v: &serde_json::Value| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));
        let current = obj
            .get("hours_played")
            .or_else(|| obj.get("hoursPlayed"))
            .and_then(num_from)
            .unwrap_or(0.0);
        let new_hours = ((current + add_hours) * 100.0).round() / 100.0;
        let new_hours = if new_hours < 0.01 { 0.01 } else { new_hours };
        // Backend PUT expects these keys (snake_case). Send only these to avoid type/serialization issues.
        let payload = serde_json::json!({
            "title": obj.get("title").unwrap_or(&serde_json::Value::Null),
            "description": obj.get("description").unwrap_or(&serde_json::Value::Null),
            "img": obj.get("img").unwrap_or(&serde_json::Value::Null),
            "platform": obj.get("platform").unwrap_or(&serde_json::Value::Null),
            "genre": obj.get("genre").unwrap_or(&serde_json::Value::Null),
            "dev": obj.get("dev").unwrap_or(&serde_json::Value::Null),
            "studio_country": obj.get("studio_country").unwrap_or(&serde_json::Value::Null),
            "hours_played": new_hours,
            "start_date": obj.get("start_date").unwrap_or(&serde_json::Value::Null),
            "end_date": obj.get("end_date").unwrap_or(&serde_json::Value::Null),
            "rating": obj.get("rating").unwrap_or(&serde_json::Value::Null),
            "dnf": obj.get("dnf").and_then(|v| v.as_bool()).unwrap_or(false),
            "is_public": obj.get("is_public").and_then(|v| v.as_bool()).unwrap_or(true),
            "rel_date": obj.get("rel_date").unwrap_or(&serde_json::Value::Null),
            "forklift_certified": obj.get("forklift_certified").and_then(|v| v.as_bool()).unwrap_or(false),
        });
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

    pub fn set_now_playing(
        &self,
        game_id: i32,
        game_type: String,
        title: Option<String>,
        started_at: Option<String>,
    ) -> Result<serde_json::Value, String> {
        let body = NowPlayingBody { game_id, game_type, title, started_at };
        println!("[LilyPad] set_now_playing request: {:?}", body);
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
            println!("[LilyPad] set_now_playing failed: {:?}", err);
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        let json = res.json().map_err(|e: reqwest::Error| e.to_string())?;
        println!("[LilyPad] set_now_playing success: {:?}", json);
        Ok(json)
    }

    pub fn clear_now_playing(&self) -> Result<(), String> {
        println!("[LilyPad] clear_now_playing request");
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
            println!("[LilyPad] clear_now_playing failed: {:?}", err);
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        println!("[LilyPad] clear_now_playing success");
        Ok(())
    }

    pub fn set_show_current_session(&self, enabled: bool) -> Result<serde_json::Value, String> {
        let body = serde_json::json!({ "showCurrentSession": enabled });
        println!("[LilyPad] set_show_current_session request: {:?}", body);
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
            println!("[LilyPad] set_show_current_session failed: {:?}", err);
            return Err(err["error"].as_str().unwrap_or("Request failed").to_string());
        }
        let json = res.json().map_err(|e: reqwest::Error| e.to_string())?;
        println!("[LilyPad] set_show_current_session success: {:?}", json);
        Ok(json)
    }
}
