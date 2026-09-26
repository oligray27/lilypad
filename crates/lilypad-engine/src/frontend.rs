//! The engine's `Frontend` in Gaming Mode: everything becomes a protocol event for the Decky
//! plugin, which shows Steam toasts and its Quick Access panel.
//!
//! With the plugin's auto-submit switch on (the default), every finished session is submitted as
//! soon as it ends, with the user's Gaming Mode note. With it off, each one waits for the user,
//! who is shown the session dialog (notes, submit or not) as the game closes. A force-stopped
//! session (probably the wrong game) always waits.

use crate::protocol::Out;
use lilypad_core::auto_submit::Outcome;
use lilypad_core::engine::flow::{format_duration, round_hours};
use lilypad_core::engine::{EngineState, Frontend, SessionEndedData, SubmitPolicy};
use serde::Serialize;
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// A finished session waiting for the user. Its ledger record is already pending, so if the
/// user never answers (and the engine restarts) it simply shows in Pending Submissions.
#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub id: String,
    pub title: String,
    pub time: String,
    pub hours: f64,
    /// Live-service and session-tracked games take notes, spoiler and visibility.
    pub takes_notes: bool,
    pub forced: bool,
    #[serde(skip)]
    pub data: SessionEndedData,
}

pub struct DeckyFrontend {
    out: Out,
    next_id: AtomicU64,
    pub decisions: Mutex<Vec<Decision>>,
}

impl DeckyFrontend {
    pub fn new(out: Out) -> Self {
        Self { out, next_id: AtomicU64::new(1), decisions: Mutex::new(Vec::new()) }
    }

    pub fn take_decision(&self, id: &str) -> Option<Decision> {
        let mut decisions = self.decisions.lock().unwrap();
        let index = decisions.iter().position(|d| d.id == id)?;
        Some(decisions.remove(index))
    }

    pub fn find_decision(&self, id: &str) -> Option<Decision> {
        self.decisions.lock().unwrap().iter().find(|d| d.id == id).cloned()
    }
}

impl Frontend for DeckyFrontend {
    fn notify(&self, summary: &str, body: &str) {
        self.out.event("notify", json!({ "summary": summary, "body": body }));
    }

    fn changed(&self) {
        self.out.event("changed", json!({}));
    }

    fn session_started(&self, title: &str) {
        self.out.event("session_started", json!({ "title": title }));
    }

    fn needs_decision(&self, data: SessionEndedData) {
        let id = data
            .ledger_id
            .clone()
            .unwrap_or_else(|| format!("local-{}", self.next_id.fetch_add(1, Ordering::Relaxed)));
        let decision = Decision {
            id,
            title: data.mapping.title.clone().unwrap_or_else(|| data.mapping.process.clone()),
            time: format_duration(data.duration_secs),
            hours: round_hours(data.duration_secs),
            takes_notes: matches!(data.mapping.r#type.to_ascii_lowercase().as_str(), "live" | "session"),
            forced: data.forced,
            data,
        };
        self.decisions.lock().unwrap().push(decision.clone());
        self.out.event("needs_decision", json!({ "decision": decision }));
    }

    /// Never reached: neither of this frontend's policies uses the Add Notes prompt.
    fn auto_submit_prompt(&self, _title: &str, _time: &str) -> Result<Outcome, String> {
        Ok(Outcome::Submit)
    }

    fn submit_policy(&self, state: &EngineState) -> SubmitPolicy {
        if state.process_map.read().unwrap().gaming_mode_ask {
            SubmitPolicy::Ask
        } else {
            SubmitPolicy::Always
        }
    }

    fn new_game_recorded(&self, title: &str, time: &str, is_replay: bool) {
        self.out.event("new_game_recorded", json!({ "title": title, "time": time, "is_replay": is_replay }));
    }
}
