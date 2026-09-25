//! Line-delimited JSON over stdin/stdout, spoken with the Decky plugin's backend.
//!
//! Requests (stdin): `{"id": 7, "cmd": "status", "args": {...}}`.
//! Replies (stdout): `{"id": 7, "ok": true, "result": ...}` or `{"id": 7, "ok": false, "error": "..."}`.
//! Events (stdout, unsolicited): `{"event": "changed"}`, `{"event": "notify", ...}`, ...
//!
//! stdout carries only protocol lines; logs go to stderr.

use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: u64,
    pub cmd: String,
    #[serde(default)]
    pub args: Value,
}

/// Serialises protocol lines onto stdout, one whole line at a time.
#[derive(Clone)]
pub struct Out(Arc<Mutex<std::io::Stdout>>);

impl Out {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(std::io::stdout())))
    }

    fn line(&self, value: &Value) {
        let mut out = self.0.lock().unwrap();
        // A closed stdout means the plugin backend has gone; the stdin reader exits on EOF.
        let _ = writeln!(out, "{value}");
        let _ = out.flush();
    }

    pub fn event(&self, name: &str, mut payload: Value) {
        if let Value::Object(map) = &mut payload {
            map.insert("event".into(), json!(name));
            self.line(&payload);
        } else {
            self.line(&json!({ "event": name }));
        }
    }

    pub fn reply(&self, id: u64, result: Result<Value, String>) {
        self.line(&match result {
            Ok(result) => json!({ "id": id, "ok": true, "result": result }),
            Err(error) => json!({ "id": id, "ok": false, "error": error }),
        });
    }
}
