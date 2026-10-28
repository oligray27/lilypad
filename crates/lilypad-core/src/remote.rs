//! "Enable remote control from FrogLog": pausing and resuming the tracked session from the
//! FrogLog site's Quick Session bar.
//!
//! LilyPad stays in charge of its sessions; FrogLog only relays. With the setting on, LilyPad
//! registers this install as a device and reports what it's tracking (`PUT
//! /api/lilypad/devices/:id`), privately -- separate from public now-playing presence -- and holds
//! a long-poll open for commands (`GET .../commands?wait=25`), answering each one (`POST
//! .../commands/:cid/result`). The connection is always outgoing, so nothing here listens on the
//! network. The backend side is `backend/routes/lilypad.js`.
//!
//! A frontend supplies a `RemoteHost` (what's being tracked, and how to pause/resume it) and calls
//! `start` once. Anything that changes what `RemoteHost::state` returns -- a session starting,
//! ending, pausing or resuming, the setting or the login changing -- calls `state_changed`.

use crate::config::{app_data_dir, AuthConfig};
use crate::monitor::ActiveSession;
use serde::{Deserialize, Serialize};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::Duration;

/// How long one command long-poll is held by the server.
const POLL_WAIT_SECS: u64 = 25;
/// Comfortably longer than a poll, so a held request isn't cut off by LilyPad's own timeout.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(POLL_WAIT_SECS + 15);
/// How often the reporter re-checks with nothing to prompt it: picks up a setting changed by a
/// path that didn't call `state_changed`, and retries a report that failed.
const REPORT_RECHECK: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// What this install is tracking, as FrogLog's site sees it. Durations rather than timestamps,
/// so the PC's clock never matters: the server works the timestamps out from when it received
/// the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RemoteState {
    Idle,
    Tracking(SessionReport),
    Paused(SessionReport),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionReport {
    /// The session's ledger record id. Commands name it, so one meant for a session that has
    /// since ended can't act on the next one.
    pub ledger_id: String,
    pub game_id: i32,
    /// LilyPad's mapping type ("regular", "session", "live"); the server maps it.
    pub game_type: String,
    pub title: String,
    /// Play time so far, pauses left out.
    pub active_secs: u64,
    /// Time spent in pauses that have ended.
    pub paused_secs: u64,
    /// How long the pause still running has lasted (0 when not paused).
    pub current_pause_secs: u64,
}

impl RemoteState {
    /// The state for the mapped session being tracked, if it has a ledger record to name. A
    /// session with no record (storage unavailable) can't be targeted, so it reports as idle.
    pub fn of(session: Option<&ActiveSession>, ledger_id: Option<String>) -> Self {
        let (Some(s), Some(ledger_id)) = (session, ledger_id) else { return Self::Idle };
        let report = SessionReport {
            ledger_id,
            game_id: s.mapping.froglog_id,
            game_type: s.mapping.r#type.clone(),
            title: s.mapping.title.clone().unwrap_or_else(|| s.process_name.clone()),
            active_secs: s.active_elapsed().as_secs(),
            paused_secs: s.paused_total.as_secs(),
            current_pause_secs: s.paused_at.map_or(0, |p| p.elapsed().as_secs()),
        };
        if s.is_paused() { Self::Paused(report) } else { Self::Tracking(report) }
    }

    /// The same state, ignoring how much time has passed: reporting again only because a
    /// duration ticked on is pointless, since the server's timestamps already account for it.
    fn shape(&self) -> (u8, Option<&str>) {
        match self {
            Self::Idle => (0, None),
            Self::Tracking(r) => (1, Some(r.ledger_id.as_str())),
            Self::Paused(r) => (2, Some(r.ledger_id.as_str())),
        }
    }
}

/// What the remote-control loop needs from whichever frontend is tracking.
pub trait RemoteHost: Send + Sync + 'static {
    /// The login to talk to FrogLog as.
    fn auth(&self) -> AuthConfig;
    /// Whether "Enable remote control from FrogLog" is on for the logged-in account.
    fn enabled(&self) -> bool;
    /// "windows", "linux" or "steam-deck", shown to the user on the site.
    fn platform(&self) -> &'static str;
    /// The logged-in account's "Device name" (`ProcessMapConfig::device_name`), if set.
    fn device_name(&self) -> Option<String>;
    fn state(&self) -> RemoteState;
    /// Pauses the session `ledger_id`, if it's the one being tracked. Pausing a paused session
    /// is not an error.
    fn pause(&self, ledger_id: &str) -> Result<(), String>;
    fn resume(&self, ledger_id: &str) -> Result<(), String>;
}

static CHANGED: OnceLock<mpsc::Sender<()>> = OnceLock::new();

/// Tells the remote-control loop to report the current state. Cheap, and a no-op before `start`
/// or when remote control isn't running.
pub fn state_changed() {
    if let Some(tx) = CHANGED.get() {
        let _ = tx.send(());
    }
}

/// `device.json` in the app data directory: this install's id. On Linux the desktop app and the
/// Gaming Mode engine share it, which is right: it's the same install, and only one of them
/// tracks at a time. (0.6.6's first build kept the device name here too; it's per account now,
/// in `ProcessMapConfig`, and an old `name` key is ignored.)
#[derive(Default, Serialize, Deserialize)]
struct DeviceFile {
    #[serde(default)]
    id: String,
}

/// Longest device name kept; the site shows it in the Quick Session bar.
const MAX_DEVICE_NAME: usize = 40;

fn device_file_path() -> std::path::PathBuf {
    app_data_dir().join("device.json")
}

fn read_device_file() -> DeviceFile {
    std::fs::read_to_string(device_file_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_device_file(file: &DeviceFile) -> std::io::Result<()> {
    std::fs::create_dir_all(app_data_dir())?;
    std::fs::write(device_file_path(), serde_json::to_string_pretty(file).unwrap_or_default())
}

/// This install's id, created once and kept in `device.json`.
pub fn device_id() -> String {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let mut file = read_device_file();
        if uuid::Uuid::parse_str(&file.id).is_ok() {
            return file.id;
        }
        file.id = uuid::Uuid::new_v4().to_string();
        if let Err(e) = write_device_file(&file) {
            log::warn!("[LilyPad] could not save the remote-control device id: {e}");
        }
        file.id
    })
    .clone()
}

/// A "Device name" as typed, ready to store in `ProcessMapConfig::device_name`: trimmed and cut
/// to length, `None` when blank. The site shows it in the Quick Session bar, e.g. "via Gaming PC".
pub fn clean_device_name(name: &str) -> Option<String> {
    let name: String = name.trim().chars().take(MAX_DEVICE_NAME).collect();
    (!name.is_empty()).then_some(name)
}

/// The computer's own name: the suggestion shown in the empty "Device name" field.
pub fn computer_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "This computer".into())
}

#[derive(Debug, Deserialize)]
struct Command {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
    ledger_id: String,
}

#[derive(Deserialize)]
struct Commands {
    commands: Vec<Command>,
}

/// The remote-control endpoints, on their own HTTP client: the long-poll outlasts
/// `FroglogClient`'s 15s timeout.
struct RemoteClient {
    base: String,
    token: String,
    client: reqwest::blocking::Client,
}

#[derive(Debug)]
enum RemoteError {
    /// The server doesn't know this device (yet): the reporter needs to register it.
    NotRegistered,
    Other(String),
}

impl RemoteClient {
    fn new(auth: &AuthConfig, platform: &str) -> Option<Self> {
        let token = auth.token.clone()?;
        let base = auth
            .base_url
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| crate::engine::DEFAULT_API_URL.to_string());
        let base = base.trim_end_matches('/');
        let base = if base.ends_with("/api") { base.to_string() } else { format!("{base}/api") };
        let client = reqwest::blocking::Client::builder()
            .timeout(CLIENT_TIMEOUT)
            .user_agent(format!("LilyPad/{} ({platform})", env!("CARGO_PKG_VERSION")))
            .build()
            .ok()?;
        Some(Self { base, token, client })
    }

    fn device_url(&self, suffix: &str) -> String {
        format!("{}/lilypad/devices/{}{suffix}", self.base, device_id())
    }

    fn check(res: reqwest::Result<reqwest::blocking::Response>) -> Result<reqwest::blocking::Response, RemoteError> {
        let res = res.map_err(|e| RemoteError::Other(e.to_string()))?;
        let status = res.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            Err(RemoteError::NotRegistered)
        } else if status.is_success() {
            Ok(res)
        } else {
            Err(RemoteError::Other(format!("HTTP {}", status.as_u16())))
        }
    }

    fn report(&self, platform: &str, state: &RemoteState, name: Option<&str>) -> Result<(), RemoteError> {
        let body = serde_json::json!({
            "name": name,
            "platform": platform,
            "app_version": env!("CARGO_PKG_VERSION"),
            "remote_control": true,
            "state": state,
        });
        Self::check(self.client.put(self.device_url("")).bearer_auth(&self.token).json(&body).send()).map(|_| ())
    }

    fn unregister(&self) -> Result<(), RemoteError> {
        match Self::check(self.client.delete(self.device_url("")).bearer_auth(&self.token).send()) {
            Ok(_) | Err(RemoteError::NotRegistered) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn poll(&self) -> Result<Vec<Command>, RemoteError> {
        let res = Self::check(
            self.client
                .get(self.device_url(&format!("/commands?wait={POLL_WAIT_SECS}")))
                .bearer_auth(&self.token)
                .send(),
        )?;
        res.json::<Commands>().map(|c| c.commands).map_err(|e| RemoteError::Other(e.to_string()))
    }

    fn answer(&self, command_id: i64, result: &Result<(), String>) {
        let body = match result {
            Ok(()) => serde_json::json!({ "ok": true }),
            Err(e) => serde_json::json!({ "ok": false, "error": e }),
        };
        let url = self.device_url(&format!("/commands/{command_id}/result"));
        if let Err(e) = self.client.post(url).bearer_auth(&self.token).json(&body).send() {
            log::warn!("[LilyPad] could not send a remote command's result: {e}");
        }
    }
}

/// Starts remote control for this process: a reporter thread that tells FrogLog what's being
/// tracked whenever it changes, and a poller thread that takes commands. Both idle while the
/// setting is off or nobody is logged in. Call once; later calls do nothing.
pub fn start(host: Arc<dyn RemoteHost>) {
    let (tx, rx) = mpsc::channel();
    if CHANGED.set(tx).is_err() {
        return;
    }
    {
        let host = host.clone();
        std::thread::spawn(move || run_reporter(host, rx));
    }
    std::thread::spawn(move || run_poller(host));
    state_changed();
}

/// What was last told to the server, so an unchanged state isn't re-sent: whether remote control
/// was on, under which login, the device name, and the state's shape.
#[derive(PartialEq)]
struct Reported {
    enabled: bool,
    token: Option<String>,
    name: Option<String>,
    shape: (u8, Option<String>),
}

fn run_reporter(host: Arc<dyn RemoteHost>, rx: mpsc::Receiver<()>) {
    let mut last: Option<Reported> = None;
    loop {
        if let Err(mpsc::RecvTimeoutError::Disconnected) = rx.recv_timeout(REPORT_RECHECK) {
            return;
        }
        while rx.try_recv().is_ok() {}

        let auth = host.auth();
        let enabled = host.enabled() && auth.token.is_some();
        let state = if enabled { host.state() } else { RemoteState::Idle };
        let (kind, ledger) = state.shape();
        let name = host.device_name();
        let now = Reported { enabled, token: auth.token.clone(), name: name.clone(), shape: (kind, ledger.map(str::to_string)) };
        if last.as_ref() == Some(&now) {
            continue;
        }

        // Logged out, switched account, or switched the setting off: the old registration goes,
        // so the site stops offering controls for it (and another account can register this id).
        if let Some(prev) = last.as_ref().filter(|p| p.enabled && (!enabled || p.token != now.token)) {
            let prev_auth = AuthConfig { token: prev.token.clone(), ..auth.clone() };
            if let Some(client) = RemoteClient::new(&prev_auth, host.platform()) {
                if let Err(e) = client.unregister() {
                    log::warn!("[LilyPad] could not unregister remote control: {e:?}");
                }
            }
        }
        if !enabled {
            last = Some(now);
            continue;
        }
        let Some(client) = RemoteClient::new(&auth, host.platform()) else { continue };
        match client.report(host.platform(), &state, name.as_deref()) {
            Ok(()) => last = Some(now),
            // Left unrecorded, so the next re-check (or change) tries again.
            Err(e) => {
                log::warn!("[LilyPad] could not report to FrogLog remote control: {e:?}");
                last = None;
            }
        }
    }
}

fn run_poller(host: Arc<dyn RemoteHost>) {
    let mut backoff = Duration::from_secs(2);
    loop {
        let auth = host.auth();
        if !host.enabled() || auth.token.is_none() {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        let Some(client) = RemoteClient::new(&auth, host.platform()) else {
            std::thread::sleep(MAX_BACKOFF);
            continue;
        };
        match client.poll() {
            Ok(commands) => {
                backoff = Duration::from_secs(2);
                for command in commands {
                    let result = match command.kind.as_str() {
                        "pause" => host.pause(&command.ledger_id),
                        "resume" => host.resume(&command.ledger_id),
                        other => Err(format!("This version of LilyPad can't do \"{other}\"")),
                    };
                    log::info!("[LilyPad] remote {} for session {}: {result:?}", command.kind, command.ledger_id);
                    client.answer(command.id, &result);
                    state_changed();
                }
            }
            Err(RemoteError::NotRegistered) => {
                // Not registered yet (or the row was removed): have the reporter register, then
                // try again shortly.
                state_changed();
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Err(RemoteError::Other(e)) => {
                log::warn!("[LilyPad] remote control poll failed: {e}");
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProcessMapping;
    use std::time::Instant;

    fn session(paused: bool) -> ActiveSession {
        let mut s = ActiveSession::new(
            "game.exe".into(),
            ProcessMapping {
                process: "game.exe".into(), r#type: "live".into(), froglog_id: 7,
                title: Some("Game".into()), title_filter: None, exe_path: None,
            },
            sysinfo::Pid::from(1usize),
            None,
            Instant::now() - Duration::from_secs(600),
        );
        s.paused_total = Duration::from_secs(100);
        if paused {
            s.paused_at = Some(Instant::now() - Duration::from_secs(50));
        }
        s
    }

    #[test]
    fn a_session_without_a_record_reports_idle() {
        assert_eq!(RemoteState::of(Some(&session(false)), None), RemoteState::Idle);
        assert_eq!(RemoteState::of(None, Some("id".into())), RemoteState::Idle);
    }

    #[test]
    fn reported_durations_leave_pauses_out() {
        let RemoteState::Paused(r) = RemoteState::of(Some(&session(true)), Some("id".into())) else {
            panic!("expected paused");
        };
        assert_eq!(r.ledger_id, "id");
        assert_eq!(r.paused_secs, 100);
        assert!((49..=51).contains(&r.current_pause_secs));
        // 600s since the start, 100s of earlier pauses and ~50s of the current one.
        assert!((449..=451).contains(&r.active_secs), "active_secs {}", r.active_secs);
    }

    #[test]
    fn state_serialises_with_a_status_tag() {
        let json = serde_json::to_value(RemoteState::of(Some(&session(false)), Some("id".into()))).unwrap();
        assert_eq!(json["status"], "tracking");
        assert_eq!(json["game_id"], 7);
        assert_eq!(json["game_type"], "live");
        assert_eq!(serde_json::to_value(RemoteState::Idle).unwrap(), serde_json::json!({ "status": "idle" }));
    }

    #[test]
    fn device_names_are_trimmed_cut_to_length_and_blank_is_none() {
        assert_eq!(clean_device_name("  Gaming PC "), Some("Gaming PC".into()));
        assert_eq!(clean_device_name("   "), None);
        assert_eq!(clean_device_name(&"x".repeat(60)).map(|n| n.chars().count()), Some(MAX_DEVICE_NAME));
    }

    #[test]
    fn a_state_that_only_ticked_on_has_the_same_shape() {
        let a = RemoteState::of(Some(&session(false)), Some("id".into()));
        let mut later = session(false);
        later.started_at -= Duration::from_secs(30);
        let b = RemoteState::of(Some(&later), Some("id".into()));
        assert_ne!(a, b);
        assert_eq!(a.shape(), b.shape());
    }
}
