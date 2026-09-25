//! Runs the real engine binary against a throwaway data directory and speaks its protocol:
//! startup, requests, waiting while the desktop app holds the tracker lock, and handing over
//! when asked. Linux-only: the data directory is redirected with XDG_DATA_HOME, which is how the
//! engine finds it there (Windows has no equivalent override).
#![cfg(target_os = "linux")]

use lilypad_core::engine::lock::TrackerLock;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

struct Engine {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<Value>,
    next_id: u64,
}

impl Engine {
    fn start(data_home: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_lilypad-engine"))
            .env("XDG_DATA_HOME", data_home)
            .env("HOME", data_home)
            .env("RUST_LOG", "error")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines().map_while(Result::ok) {
                let _ = tx.send(serde_json::from_str::<Value>(&line).expect("stdout must be protocol lines only"));
            }
        });
        Engine { child, stdin, lines, next_id: 1 }
    }

    /// The next event named `name`, skipping others; replies are never events.
    fn event(&self, name: &str) -> Value {
        loop {
            let line = self.lines.recv_timeout(Duration::from_secs(20)).unwrap_or_else(|_| panic!("no {name} event"));
            if line["event"] == name {
                return line;
            }
        }
    }

    fn request(&mut self, cmd: &str) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(self.stdin, "{}", json!({ "id": id, "cmd": cmd, "args": {} })).unwrap();
        loop {
            let line = self.lines.recv_timeout(Duration::from_secs(20)).expect("no reply");
            if line["id"] == id {
                return line;
            }
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn starts_answers_and_hands_over_to_the_desktop_app() {
    let data_home = tempfile::tempdir().unwrap();
    let mut engine = Engine::start(data_home.path());
    engine.event("started");
    engine.event("tracking");

    assert_eq!(engine.request("ping")["result"], "pong");
    let status = engine.request("status");
    assert_eq!(status["ok"], true);
    assert_eq!(status["result"]["tracking"], true);
    assert_eq!(status["result"]["logged_in"], false);
    let unknown = engine.request("no_such_command");
    assert_eq!(unknown["ok"], false);

    // The desktop app starts and asks for the lock.
    let dir = data_home.path().join("froglog-lilypad");
    let desktop = TrackerLock::take_over_in(&dir, Duration::from_secs(10)).expect("the engine must hand over");
    engine.event("yielding");
    assert_eq!(engine.child.wait().unwrap().code(), Some(3), "the hand-over exit code");
    drop(desktop);
}

#[test]
fn waits_while_the_desktop_app_is_tracking() {
    let data_home = tempfile::tempdir().unwrap();
    let dir = data_home.path().join("froglog-lilypad");
    let desktop = TrackerLock::try_acquire_in(&dir).unwrap().unwrap();

    let mut engine = Engine::start(data_home.path());
    engine.event("waiting");
    let status = engine.request("status");
    assert_eq!(status["result"]["tracking"], false);
    // Queues belong to the desktop app meanwhile.
    assert_eq!(engine.request("pending")["ok"], false);

    // Back to Gaming Mode: the desktop app has gone, so the engine takes over.
    drop(desktop);
    engine.event("tracking");
    assert_eq!(engine.request("status")["result"]["tracking"], true);
}
