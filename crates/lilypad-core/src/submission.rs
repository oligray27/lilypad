//! Session submission rules shared by the desktop frontends. Storage and presentation stay
//! with the caller for now; this service never acknowledges a ledger record on an API error.
use crate::api::{AddSessionBody, ApiFailure, FroglogClient, LiveServiceGame};
use crate::config::PendingSession;
use serde_json::Value;

/// Narrow API boundary for deterministic submission/failure tests without a real account.
pub trait SessionApi {
    fn enable_session_tracking(&self, game_id: i32) -> Result<(), String>;
    fn add_session(&self, live: bool, game_id: i32, body: AddSessionBody) -> Result<Value, String>;
}

impl SessionApi for FroglogClient {
    fn enable_session_tracking(&self, game_id: i32) -> Result<(), String> {
        FroglogClient::enable_session_tracking(self, game_id).map(|_| ())
    }

    fn add_session(&self, live: bool, game_id: i32, body: AddSessionBody) -> Result<Value, String> {
        if live {
            self.add_live_service_session(
                game_id,
                body.date,
                body.hours,
                body.notes,
                body.spoiler,
                body.is_public,
                body.sync_ref,
            )
        } else {
            self.add_game_session(
                game_id,
                body.date,
                body.hours,
                body.notes,
                body.spoiler,
                body.is_public,
                body.sync_ref,
            )
        }
    }
}

pub trait RetryApi: SessionApi {
    fn live_service_games(&self) -> Result<Vec<LiveServiceGame>, String>;
}

impl RetryApi for FroglogClient {
    fn live_service_games(&self) -> Result<Vec<LiveServiceGame>, String> {
        self.get_live_service_games()
    }
}

/// The effective target may change when a regular game was moved to Live Service. Return it
/// with the acknowledgement so a frontend never records a response against the old target.
pub struct RetriedSession {
    pub response: Value,
    pub game_id: i32,
    pub game_type: String,
}

/// Retry against the original target first. Only a 404 for a non-live game permits recovery
/// via an exact, unambiguous Live Service title. Other errors leave the target untouched.
/// The caller supplies its original submission key; legacy GTK queues do not yet have one
/// that was sent on the initial attempt, so they must not invent a different retry identity.
pub fn retry_play_session(
    client: &impl RetryApi,
    session: &PendingSession,
    sync_ref: Option<String>,
) -> Result<RetriedSession, String> {
    let notes = Some(
        session
            .notes
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "Session submitted from Pending Sessions".into()),
    );
    let submit = |game_type: &str, game_id| {
        submit_play_session(
            client,
            game_type,
            game_id,
            Some(session.date.clone()),
            session.hours,
            notes.clone(),
            session.spoiler,
            session.is_public,
            sync_ref.clone(),
        )
    };
    match submit(&session.game_type, session.game_id) {
        Ok(response) => Ok(RetriedSession {
            response,
            game_id: session.game_id,
            game_type: session.game_type.clone(),
        }),
        Err(error) => {
            if ApiFailure::classify(&error) != ApiFailure::NotFound
                || session.game_type.eq_ignore_ascii_case("live")
            {
                return Err(error);
            }
            let games = client.live_service_games().map_err(|_| error.clone())?;
            let mut matches = games
                .iter()
                .filter(|g| g.title.as_deref() == Some(session.title.as_str()));
            let Some(game) = matches.next() else {
                return Err(error);
            };
            // Picking the first same-titled game risks crediting the wrong entry.
            if matches.next().is_some() {
                return Err(error);
            }
            let response = submit("live", game.id)?;
            log::info!(
                "[LilyPad] recovered pending session {}: {} #{} -> live #{}",
                session.id,
                session.game_type,
                session.game_id,
                game.id
            );
            Ok(RetriedSession {
                response,
                game_id: game.id,
                game_type: "live".into(),
            })
        }
    }
}

/// Submit play through session endpoints, including legacy regular-game mappings. Promotion
/// preserves pre-tracked hours via the existing API client; failure must prevent the POST.
/// `sync_ref` is supplied by the caller and must be reused across retries. This function does
/// not generate an identity, persist pending work, or infer success from a conflict response.
pub fn submit_play_session(
    client: &impl SessionApi,
    game_type: &str,
    game_id: i32,
    date: Option<String>,
    hours: f64,
    notes: Option<String>,
    spoiler: bool,
    is_public: bool,
    sync_ref: Option<String>,
) -> Result<Value, String> {
    let live = game_type.eq_ignore_ascii_case("live");
    if !live && !game_type.eq_ignore_ascii_case("session") {
        log::info!("[LilyPad] enabling session tracking for game {game_id} before logging play");
        client.enable_session_tracking(game_id)?;
    }
    client.add_session(
        live,
        game_id,
        AddSessionBody {
            date,
            hours: Some(hours),
            notes,
            spoiler,
            is_public,
            sync_ref,
        },
    )
}

/// The server id to record against a submitted session. Session-type submissions return the
/// created row, so its `id` is the real reference; anything else can only point at the game.
pub fn remote_reference(response: &Value, game_id: i32, game_type: &str) -> String {
    match response.get("id").and_then(|v| v.as_i64()) {
        Some(id) if !game_type.eq_ignore_ascii_case("regular") => format!("session:{id}"),
        _ => format!("game-hours:{game_id}"),
    }
}

/// Logs each individually-accumulated play session of a New Games entry as its own FrogLog
/// session, via `submit_one(date, hours, sync_ref)`. Falls back to one entry dated today for an
/// aggregate-only entry persisted before per-session tracking existed.
///
/// Each session's `sync_ref` is derived from the appid and its position, which is what makes a
/// partly-completed resolution resumable: resolving again replays the same keys, so the server
/// skips what it already has. `sessions` is only ever appended to, so an index is stable across
/// retries. Returns how many sessions were logged.
pub fn log_each_pending_session(
    entry: &crate::config::PendingGameSubmission,
    mut submit_one: impl FnMut(String, f64, Option<String>) -> Result<Value, String>,
) -> Result<usize, String> {
    let key = |index: usize| Some(format!("newgame:{}#{index}", entry.appid));
    if entry.sessions.is_empty() {
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        submit_one(date, entry.hours, key(0))?;
        return Ok(1);
    }
    let total = entry.sessions.len();
    for (index, session) in entry.sessions.iter().enumerate() {
        // Logged per session so a resolution that fails partway says how far it got.
        log::info!(
            "[LilyPad] logging session {}/{total} for {}: {}h on {}",
            index + 1, entry.title, session.hours, session.date
        );
        submit_one(session.date.clone(), session.hours, key(index))?;
    }
    Ok(total)
}

/// Both frontends present the same actionable failure explanation.
pub fn explain_failure(error: &str) -> String {
    match ApiFailure::classify(error) {
        ApiFailure::NotFound => "This game no longer exists in FrogLog — it was probably deleted. Discard this session, or re-add the game and play it again to re-link it.".into(),
        ApiFailure::Unauthorized => "Not signed in to FrogLog. Log in again, then retry.".into(),
        ApiFailure::RateLimited => "FrogLog asked LilyPad to slow down. Retry in a moment.".into(),
        ApiFailure::Transient => format!("Could not reach FrogLog ({error}). Retry when back online."),
        ApiFailure::Rejected => format!("FrogLog rejected this session ({error})."),
        ApiFailure::Conflict => "FrogLog has not confirmed this session yet. Retry in a moment.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    struct FakeApi {
        calls: RefCell<Vec<Value>>,
        promotion_error: Option<String>,
        submission_error: Option<String>,
        non_live_error: Option<String>,
        live_games: Vec<LiveServiceGame>,
        lookups: Cell<usize>,
    }
    impl SessionApi for FakeApi {
        fn enable_session_tracking(&self, game_id: i32) -> Result<(), String> {
            self.calls
                .borrow_mut()
                .push(serde_json::json!({"promote": game_id}));
            self.promotion_error.clone().map_or(Ok(()), Err)
        }
        fn add_session(
            &self,
            live: bool,
            game_id: i32,
            body: AddSessionBody,
        ) -> Result<Value, String> {
            self.calls
                .borrow_mut()
                .push(serde_json::json!({"live": live, "id": game_id, "body": body}));
            if !live {
                if let Some(error) = &self.non_live_error {
                    return Err(error.clone());
                }
            }
            self.submission_error
                .clone()
                .map_or(Ok(serde_json::json!({"id": 123})), Err)
        }
    }
    impl RetryApi for FakeApi {
        fn live_service_games(&self) -> Result<Vec<LiveServiceGame>, String> {
            self.lookups.set(self.lookups.get() + 1);
            Ok(self.live_games.clone())
        }
    }
    fn submit(client: &FakeApi, kind: &str) -> Result<Value, String> {
        submit_play_session(
            client,
            kind,
            42,
            Some("2026-09-25".into()),
            1.25,
            Some("private note".into()),
            true,
            false,
            Some("stable-session-id".into()),
        )
    }

    #[test]
    fn legacy_regular_mapping_is_promoted_before_its_session_is_sent() {
        let api = FakeApi::default();
        assert_eq!(submit(&api, "REGULAR").unwrap()["id"], 123);
        let calls = api.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], serde_json::json!({"promote": 42}));
        assert_eq!(calls[1]["live"], false);
    }

    #[test]
    fn tracked_and_live_games_use_their_own_endpoint_without_promotion() {
        for (kind, live) in [("SESSION", false), ("LIVE", true)] {
            let api = FakeApi::default();
            submit(&api, kind).unwrap();
            let calls = api.calls.borrow();
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0],
                serde_json::json!({"live": live, "id": 42, "body": {
                    "date": "2026-09-25", "hours": 1.25, "notes": "private note",
                    "spoiler": true, "is_public": false, "sync_ref": "stable-session-id"
                }})
            );
        }
    }

    #[test]
    fn failed_promotion_does_not_send_a_session() {
        let api = FakeApi {
            promotion_error: Some("503: unavailable".into()),
            ..Default::default()
        };
        assert_eq!(submit(&api, "regular").unwrap_err(), "503: unavailable");
        assert_eq!(api.calls.borrow().len(), 1);
    }

    #[test]
    fn ambiguous_failure_and_retry_keep_the_same_payload_and_key() {
        let mut api = FakeApi {
            submission_error: Some("response lost".into()),
            ..Default::default()
        };
        assert!(submit(&api, "session").is_err());
        api.submission_error = None;
        assert!(submit(&api, "session").is_ok());
        let calls = api.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], calls[1]);
    }

    #[test]
    fn conflicts_are_not_treated_as_acknowledgements() {
        let api = FakeApi {
            submission_error: Some(
                "409: Session with this sync_ref is already being created".into(),
            ),
            ..Default::default()
        };
        let error = submit(&api, "live").unwrap_err();
        assert_eq!(ApiFailure::classify(&error), ApiFailure::Conflict);
        assert!(ApiFailure::classify(&error).is_worth_retrying());
        assert!(explain_failure(&error).contains("not confirmed"));
    }

    fn pending() -> PendingSession {
        PendingSession {
            id: "original-id".into(),
            game_id: 42,
            game_type: "session".into(),
            title: "Moved Game".into(),
            hours: 1.25,
            notes: Some("keep me".into()),
            spoiler: true,
            is_public: false,
            date: "2026-09-25".into(),
            failed_at: "2026-09-25T12:00:00Z".into(),
            error: "offline".into(),
        }
    }

    fn live_game(id: i32) -> LiveServiceGame {
        serde_json::from_value(serde_json::json!({"id": id, "title": "Moved Game"})).unwrap()
    }

    #[test]
    fn moved_game_recovery_preserves_key_payload_and_reports_the_effective_target() {
        let api = FakeApi {
            non_live_error: Some("404: Not found".into()),
            live_games: vec![live_game(77)],
            ..Default::default()
        };
        let result = retry_play_session(&api, &pending(), Some("original-id".into())).unwrap();
        assert_eq!((result.game_id, result.game_type.as_str()), (77, "live"));
        assert_eq!(result.response["id"], 123);
        assert_eq!(api.lookups.get(), 1);
        let calls = api.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["body"], calls[1]["body"]);
        assert_eq!(calls[1]["body"]["sync_ref"], "original-id");
        assert_eq!(calls[1]["body"]["notes"], "keep me");
        assert_eq!(calls[1]["body"]["is_public"], false);
    }

    #[test]
    fn retry_does_not_retarget_auth_network_rate_limit_or_conflict_failures() {
        for error in [
            "401: Unauthorized",
            "offline",
            "429: slow down",
            "409: in progress",
        ] {
            let api = FakeApi {
                submission_error: Some(error.into()),
                live_games: vec![live_game(77)],
                ..Default::default()
            };
            assert!(retry_play_session(&api, &pending(), Some("original-id".into())).is_err());
            assert_eq!(api.lookups.get(), 0);
            assert_eq!(api.calls.borrow().len(), 1);
        }
    }

    #[test]
    fn retry_refuses_ambiguous_live_matches_and_missing_live_targets() {
        let api = FakeApi {
            non_live_error: Some("404: Not found".into()),
            live_games: vec![live_game(77), live_game(78)],
            ..Default::default()
        };
        assert!(retry_play_session(&api, &pending(), None).is_err());
        assert_eq!(api.calls.borrow().len(), 1);
        let mut session = pending();
        session.game_type = "live".into();
        let api = FakeApi {
            submission_error: Some("404: Not found".into()),
            ..Default::default()
        };
        assert!(retry_play_session(&api, &session, None).is_err());
        assert_eq!(api.lookups.get(), 0);
    }
}
