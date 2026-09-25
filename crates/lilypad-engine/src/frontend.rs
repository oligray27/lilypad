//! The engine's `Frontend` in Gaming Mode: everything becomes a protocol event for the Decky
//! plugin, which shows Steam toasts and its Quick Access panel.
//!
//! Gaming Mode has no notes: every finished session is submitted as soon as it ends, with the
//! user's Gaming Mode note. Only a force-stopped session (probably the wrong game) waits for the
//! user to submit it or not record it.

use crate::protocol::Out;
use lilypad_core::auto_submit::Outcome;
use lilypad_core::engine::flow::{format_duration, round_hours};
use lilypad_core::engine::{Frontend, SessionEndedData};
use serde::Serialize;
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// A stopped session waiting for the user in the panel. Its ledger record is already pending,
/// so if the user never answers it simply stays in Pending Submissions.
#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub id: String,
    pub title: String,
    pub time: String,
    pub hours: f64,
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
            data,
        };
        self.decisions.lock().unwrap().push(decision.clone());
        self.out.event("needs_decision", json!({ "decision": decision }));
    }

    /// Never reached: `submits_every_session` skips the Add Notes prompt.
    fn auto_submit_prompt(&self, _title: &str, _time: &str) -> Result<Outcome, String> {
        Ok(Outcome::Submit)
    }

    fn submits_every_session(&self) -> bool {
        true
    }

    fn new_game_recorded(&self, title: &str, time: &str, is_replay: bool) {
        self.out.event("new_game_recorded", json!({ "title": title, "time": time, "is_replay": is_replay }));
    }
}
