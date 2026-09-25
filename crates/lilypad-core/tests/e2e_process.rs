//! End-to-end checks against real operating-system processes: detection, exit timing, durable
//! recording and crash recovery, with no mocks between the monitor and the OS.
//!
//! The "game" is a copy of this test binary under a unique name, re-run so that it executes only
//! `e2e_sleeper`, which sleeps until killed. That works identically on Windows and Linux without
//! relying on any system program.
//!
//! Ignored by default because they take about half a minute (a tracked session shorter than 15 s
//! is deliberately held open for a possible launcher relaunch). Run with:
//!   cargo test -p lilypad-core --test e2e_process -- --ignored --test-threads=1

use lilypad_core::config::{ProcessMapConfig, ProcessMapping};
use lilypad_core::library_match::LibraryIndex;
use lilypad_core::monitor::{process_start_time, run_poll_loop, ActiveSession};
use lilypad_core::session_ledger::{AccountIdentity, SessionLedger, SubmissionState};
use lilypad_core::session_store::{find_original_process, Completion, Recovered, SessionStore};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};
use sysinfo::{Pid, ProcessesToUpdate, System};

const SERVER: &str = "https://api.example.test/api";
const SLEEPER_ENV: &str = "LILYPAD_E2E_SLEEPER";

/// The body of the fake game. Does nothing in a normal test run.
#[test]
#[ignore = "helper process for the end-to-end tests, not a test"]
fn e2e_sleeper() {
    if std::env::var_os(SLEEPER_ENV).is_some() {
        std::thread::sleep(Duration::from_secs(180));
    }
}

fn game_file_name(stem: &str) -> String {
    format!("{stem}{}", std::env::consts::EXE_SUFFIX)
}

/// A running fake game named `stem`, killed when dropped.
struct Game {
    child: Child,
    _dir: tempfile::TempDir,
}

impl Game {
    fn launch(stem: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().join(game_file_name(stem));
        std::fs::copy(std::env::current_exe().unwrap(), &path).unwrap();
        let child = Command::new(&path)
            .args(["e2e_sleeper", "--exact", "--ignored", "--nocapture", "--test-threads=1"])
            .env(SLEEPER_ENV, "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("could not launch the fake game");
        Game { child, _dir: dir }
    }

    fn pid(&self) -> Pid {
        Pid::from(self.child.id() as usize)
    }

    fn quit(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Game {
    fn drop(&mut self) {
        self.quit();
    }
}

fn store() -> (SessionStore, AccountIdentity) {
    let ledger = SessionLedger::open(Path::new(":memory:")).unwrap();
    let account = AccountIdentity { server: SERVER.into(), account_id: "e2e".into() };
    (SessionStore::from_ledger(ledger, SERVER), account)
}

fn mapping(process: &str) -> ProcessMapping {
    ProcessMapping {
        process: process.into(), r#type: "session".into(), froglog_id: 1,
        title: Some("E2E Game".into()), title_filter: None, exe_path: None,
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
}

enum Event {
    Started(String),
    Ended(f64),
}

/// The whole mapped-session path as a frontend drives it: the monitor finds the running game,
/// the session is recorded before anything else, the exit is observed, and the record is
/// completed with the real duration.
#[test]
#[ignore = "end-to-end: runs a real process for ~20 s"]
fn a_real_game_is_detected_recorded_and_ended_with_its_real_duration() {
    let name = game_file_name("lpad-e2e-track");
    let (store, account) = store();
    let current: Arc<RwLock<Option<ActiveSession>>> = Arc::new(RwLock::new(None));
    let shutdown = Arc::new(AtomicBool::new(false));
    let record: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let (tx, rx) = mpsc::channel::<Event>();

    let mut game = Game::launch("lpad-e2e-track");
    let launched = Instant::now();

    run_poll_loop(
        Arc::new(RwLock::new(ProcessMapConfig { mappings: vec![mapping(&name)], ..Default::default() })),
        Arc::clone(&current),
        Arc::clone(&shutdown),
        Arc::new(RwLock::new(None)),
        1,
        {
            let (store, account, current, record, tx) =
                (store.clone(), account.clone(), Arc::clone(&current), Arc::clone(&record), tx.clone());
            move |process_name: String, mapping: ProcessMapping| {
                let (pid, started) = current
                    .read()
                    .unwrap()
                    .as_ref()
                    .map(|s| (u32::try_from(usize::from(s.pid)).ok(), s.started_at_secs))
                    .unwrap_or((None, None));
                *record.lock().unwrap() =
                    store.begin_mapped(Some(account.clone()), &process_name, &mapping, pid, started);
                let _ = tx.send(Event::Started(process_name));
            }
        },
        {
            let (store, record, tx) = (store.clone(), Arc::clone(&record), tx.clone());
            move |_process_name: String, _mapping: ProcessMapping, duration_secs: f64| {
                if let Some(id) = record.lock().unwrap().as_ref() {
                    assert_eq!(store.complete(id, now_secs()), Completion::Owned(Some(id.clone())));
                }
                let _ = tx.send(Event::Ended(duration_secs));
            }
        },
        Arc::new(RwLock::new(Vec::new())),
        Arc::new(RwLock::new(LibraryIndex::default())),
        || true,
        |_| {},
        |_, _, _, _, _| {},
        |_| {},
    );

    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Event::Started(process)) => assert!(process.eq_ignore_ascii_case(&name), "tracked {process}"),
        _ => panic!("the running game was not detected within 20 s"),
    }
    let id = record.lock().unwrap().clone().expect("the session was not recorded when it started");
    {
        let stored = store.with_ledger("test", |l| l.get(&id)).unwrap().unwrap();
        assert_eq!(stored.state, SubmissionState::Active);
        assert!(stored.record.process.pid.is_some(), "the exact process was not recorded");
    }

    // Longer than the launcher-handoff threshold, so the exit is final rather than held open.
    std::thread::sleep(Duration::from_secs(18).saturating_sub(launched.elapsed()));
    game.quit();

    let duration = match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Event::Ended(duration)) => duration,
        _ => panic!("the game's exit was not observed within 15 s"),
    };
    shutdown.store(true, Ordering::SeqCst);

    assert!(duration >= 16.0 && duration < 40.0, "credited {duration:.1} s for an ~18 s session");
    assert!(current.read().unwrap().is_none(), "the session was left marked as running");
    let stored = store.with_ledger("test", |l| l.get(&id)).unwrap().unwrap();
    assert_eq!(stored.state, SubmissionState::Pending, "an ended session must wait in the retry queue");
    let rows = store.pending_sessions(&account).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
}

/// LilyPad dies mid-session. On restart a game that is still running resumes its original
/// record, and one that has since closed is credited only up to its last checkpoint.
#[test]
#[ignore = "end-to-end: runs real processes"]
fn crash_recovery_resumes_a_running_game_and_closes_a_stopped_one() {
    let (store, account) = store();
    let mut running = Game::launch("lpad-e2e-live");
    let mut stopped = Game::launch("lpad-e2e-gone");
    std::thread::sleep(Duration::from_millis(500));

    let record = |game: &Game, stem: &str| {
        let pid = u32::try_from(usize::from(game.pid())).ok();
        let started = process_start_time(game.pid());
        assert!(started.is_some(), "could not read the start time of {stem}");
        store
            .begin_mapped(Some(account.clone()), &game_file_name(stem), &mapping(&game_file_name(stem)), pid, started)
            .unwrap()
    };
    let live_id = record(&running, "lpad-e2e-live");
    let gone_id = record(&stopped, "lpad-e2e-gone");
    // A checkpoint ten minutes in, so the closed session's credit is checkable.
    store.with_ledger("test", |l| l.checkpoint(&gone_id, now_secs() + 600)).unwrap();
    stopped.quit();

    let mut sys = System::new_all();
    sys.refresh_processes(ProcessesToUpdate::All);
    let recovered = store.recover_interrupted(&account, |identity| find_original_process(&sys, identity));

    let mut resumed = false;
    let mut closed = false;
    for item in recovered {
        match item {
            Recovered::Resume { record, pid, .. } => {
                assert_eq!(record.id, live_id);
                assert_eq!(pid, running.pid());
                resumed = true;
            }
            Recovered::Ended { id, duration_secs, .. } => {
                assert_eq!(id, gone_id);
                assert!((599.0..=602.0).contains(&duration_secs), "credited {duration_secs} s");
                closed = true;
            }
        }
    }
    assert!(resumed, "the still-running game was not resumed");
    assert!(closed, "the stopped game was not closed at its checkpoint");
    running.quit();
}

/// A relaunch that happens to get the same pid is a different process; resuming it would bill
/// the wrong span of time. The recorded OS start time is what tells them apart.
#[test]
#[ignore = "end-to-end: runs a real process"]
fn a_reused_pid_is_not_mistaken_for_the_original_game() {
    let (store, account) = store();
    let game = Game::launch("lpad-e2e-reuse");
    std::thread::sleep(Duration::from_millis(500));
    let started = process_start_time(game.pid()).unwrap();
    let pid = u32::try_from(usize::from(game.pid())).ok();
    let name = game_file_name("lpad-e2e-reuse");
    // Same pid, same name, but recorded as having started an hour earlier.
    let id = store.begin_mapped(Some(account.clone()), &name, &mapping(&name), pid, Some(started - 3600)).unwrap();

    let mut sys = System::new_all();
    sys.refresh_processes(ProcessesToUpdate::All);
    let recovered = store.recover_interrupted(&account, |identity| find_original_process(&sys, identity));
    assert!(
        matches!(recovered.as_slice(), [Recovered::Ended { id: ended, .. }] if *ended == id),
        "a process with a different start time was treated as the original"
    );
}
