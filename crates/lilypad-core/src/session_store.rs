//! Frontend-neutral session storage: the ledger plus the error reporting and lifecycle rules
//! both desktop frontends need around it.
//!
//! `SessionLedger` is the transactional store; this is the layer above it that the Tauri build
//! grew inline in `lib.rs` and the GTK build now shares. Every operation that can fail returns
//! `None` (or `false`) for "unknown", never an empty result, and records the failure so the UI
//! can distinguish an unavailable store from one with nothing in it.

use crate::config::{AuthConfig, PendingGameSubmission, PendingSession, ProcessMapping, ReplayOf};
use crate::ledger_session::{self, now_secs};
use crate::monitor::UnmappedSessionStart;
use crate::session_ledger::{
    AccountIdentity, LedgerResult, ProcessIdentity, SessionLedger, SessionRecord, SessionTarget,
    Submission, SubmissionState, LEGACY_FILES,
};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use sysinfo::{Pid, System};

/// Diagnostics produced before a frontend's logger exists, replayed once it does.
pub type StartupLog = Vec<(log::Level, String)>;

/// Opens the ledger at `ledger_path` and imports every legacy JSON queue from `legacy_dir`.
///
/// A failure is returned, never swallowed: an unopenable or malformed store must not be mistaken
/// for "no sessions recorded". The import is transactional, leaves the original JSON in place,
/// and runs at most once per file. Only call this from a build that no longer writes any of the
/// legacy files -- `import_legacy` consumes each exactly once.
pub fn open_ledger_with_import(
    ledger_path: &Path,
    legacy_dir: &Path,
) -> (Result<SessionLedger, String>, StartupLog) {
    let mut messages = Vec::new();
    let mut ledger = match SessionLedger::open(ledger_path) {
        Ok(ledger) => ledger,
        Err(e) => return (Err(e.to_string()), messages),
    };
    match ledger.import_legacy(legacy_dir, &LEGACY_FILES) {
        Ok(imported) if imported > 0 => messages.push((
            log::Level::Info,
            format!("[LilyPad] session store: migrated {imported} record(s) from the legacy JSON queues"),
        )),
        Ok(_) => {}
        Err(e) => return (Err(e.to_string()), messages),
    }
    messages.push((
        log::Level::Info,
        format!("[LilyPad] session store ready at {}", ledger_path.display()),
    ));
    (Ok(ledger), messages)
}

/// Finds the process a recorded session was tracking, distinguishing it from a *new* launch of
/// the same game.
///
/// With a recorded pid and start time, both must match: a pid alone proves nothing, because pids
/// are recycled and a game closed and relaunched while LilyPad was down can land on the same one.
/// Records with no identity (imported legacy entries) fall back to matching by executable name --
/// a guess, which is why the match is logged.
pub fn find_original_process(sys: &System, identity: &ProcessIdentity) -> Option<Pid> {
    if let (Some(pid), Some(started)) = (identity.pid, identity.started_at_secs) {
        let pid = Pid::from(pid as usize);
        return match sys.process(pid) {
            Some(p) if p.start_time() == started => Some(pid),
            Some(_) => {
                log::info!(
                    "[LilyPad] pid {pid:?} is alive but started at a different time than the \
                     recorded session -- treating it as an unrelated process, not a resume"
                );
                None
            }
            None => None,
        };
    }
    let found = sys.processes().iter().find_map(|(pid, p)| {
        let name = p
            .exe()
            .and_then(|path| path.file_name().and_then(|n| n.to_str().map(String::from)))
            .unwrap_or_else(|| p.name().to_string_lossy().into_owned());
        name.eq_ignore_ascii_case(&identity.executable).then_some(*pid)
    });
    if found.is_some() {
        log::info!(
            "[LilyPad] resuming {} by executable name; the session predates process-identity \
             recording, so this may be a different launch",
            identity.executable
        );
    }
    found
}

/// Whether the caller that just saw a process exit is the one entitled to end its session.
#[derive(Debug, PartialEq, Eq)]
pub enum Completion {
    /// This caller owns the end of the session; present it against this record, if any.
    Owned(Option<String>),
    /// Something else already completed this record (a force-stop, or a second waiter).
    /// Presenting again would show a duplicate popup and could log the session twice.
    Stale,
}

/// The ledger id of the mapped session being tracked, tied to the process it belongs to.
///
/// A bare "active id" slot let a stale waiter end the wrong session: force-stop game X (its
/// record is closed, the slot emptied), launch game Y (the slot now holds Y), then X finally
/// exits and its waiter takes whatever is in the slot -- completing Y's record while Y is still
/// running. Every take here names the process, or the exact id, it is entitled to.
#[derive(Clone, Default)]
pub struct ActiveRecord(Arc<Mutex<Option<(String, String)>>>);

impl ActiveRecord {
    /// Replaces the slot. `None` clears it (the session could not be stored).
    pub fn set(&self, process_name: &str, id: Option<String>) {
        *self.0.lock().unwrap() = id.map(|id| (process_name.to_string(), id));
    }

    /// Takes the id only if it belongs to `process_name`.
    pub fn take_for(&self, process_name: &str) -> Option<String> {
        let mut slot = self.0.lock().unwrap();
        if slot.as_ref().is_some_and(|(p, _)| p.eq_ignore_ascii_case(process_name)) {
            slot.take().map(|(_, id)| id)
        } else {
            None
        }
    }

    /// Takes whatever is being tracked (force-stop acts on the current session by definition).
    pub fn take(&self) -> Option<String> {
        self.0.lock().unwrap().take().map(|(_, id)| id)
    }

    /// Clears the slot only if it still holds `id`.
    pub fn clear_if(&self, id: &str) {
        let mut slot = self.0.lock().unwrap();
        if slot.as_ref().is_some_and(|(_, current)| current == id) {
            *slot = None;
        }
    }
}

/// What startup recovery found for one interrupted mapped session.
#[derive(Debug)]
pub enum Recovered {
    /// The original process is still running: keep tracking the same record.
    Resume { record: SessionRecord, mapping: ProcessMapping, pid: Pid },
    /// The process is gone. The record is closed at its last checkpoint and is now pending;
    /// the frontend runs its normal end-of-session path against it.
    Ended { id: String, process_name: String, mapping: ProcessMapping, duration_secs: f64 },
}

/// A record with no known owner: imported from a pre-ledger build's global queue files, which
/// never recorded which account produced them.
#[derive(Debug, Clone)]
pub struct UnownedRecord {
    pub id: String,
    pub summary: String,
}

/// Queue sizes for one account. `None` means "unknown" (storage unavailable), never zero.
#[derive(Debug, Clone, Copy, Default)]
pub struct QueueCounts {
    pub pending: Option<usize>,
    pub new_games: Option<usize>,
    pub unowned: Option<usize>,
}

#[derive(Clone)]
pub struct SessionStore {
    ledger: Option<Arc<Mutex<SessionLedger>>>,
    error: Arc<RwLock<Option<String>>>,
    default_server: Arc<str>,
}

impl SessionStore {
    /// Opens the store in the app data directory, importing the legacy queues. An open failure
    /// yields an unavailable store carrying the reason, so callers still start up.
    pub fn open(default_server: &str) -> (Self, StartupLog) {
        let path = crate::session_ledger::ledger_path();
        let (opened, mut log) = open_ledger_with_import(&path, &crate::config::app_data_dir());
        let store = match opened {
            Ok(ledger) => Self::from_ledger(ledger, default_server),
            Err(e) => {
                let message = format!(
                    "Session storage unavailable ({e}). Sessions cannot be recorded durably; \
                     crash recovery is disabled until this is resolved."
                );
                log.push((log::Level::Error, format!("[LilyPad] {message}")));
                Self::unavailable(message, default_server)
            }
        };
        (store, log)
    }

    pub fn from_ledger(ledger: SessionLedger, default_server: &str) -> Self {
        Self {
            ledger: Some(Arc::new(Mutex::new(ledger))),
            error: Arc::new(RwLock::new(None)),
            default_server: default_server.into(),
        }
    }

    pub fn unavailable(reason: String, default_server: &str) -> Self {
        Self {
            ledger: None,
            error: Arc::new(RwLock::new(Some(reason))),
            default_server: default_server.into(),
        }
    }

    pub fn is_available(&self) -> bool {
        self.ledger.is_some()
    }

    /// The raw ledger, for the heartbeat ticker.
    pub fn ledger(&self) -> Option<Arc<Mutex<SessionLedger>>> {
        self.ledger.clone()
    }

    /// The last storage failure, or `None` when the store is healthy.
    pub fn error(&self) -> Option<String> {
        self.error.read().ok().and_then(|e| e.clone())
    }

    pub fn report_error(&self, message: String) {
        log::error!("[LilyPad] session store: {message}");
        if let Ok(mut e) = self.error.write() {
            *e = Some(message);
        }
    }

    /// Who owns a session recorded right now under `auth`, or `None` when nobody is logged in.
    pub fn account(&self, auth: &AuthConfig) -> Option<AccountIdentity> {
        ledger_session::account_identity(auth, &self.default_server)
    }

    /// Runs `op` against the ledger, recording any failure. `None` means unavailable or failed --
    /// callers must treat that as "unknown", never as "nothing there".
    pub fn with_ledger<T>(
        &self,
        what: &str,
        op: impl FnOnce(&mut SessionLedger) -> LedgerResult<T>,
    ) -> Option<T> {
        let ledger = self.ledger.as_ref()?;
        let result = ledger
            .lock()
            .map_err(|e| e.to_string())
            .and_then(|mut l| op(&mut l).map_err(|e| e.to_string()));
        match result {
            Ok(value) => {
                if let Ok(mut e) = self.error.write() {
                    *e = None;
                }
                Some(value)
            }
            Err(e) => {
                self.report_error(format!("{what} failed: {e}"));
                None
            }
        }
    }

    /// Records a newly detected mapped session and returns its id. `None` means the session is
    /// not crash-recoverable; the caller keeps tracking it in memory regardless.
    pub fn begin_mapped(
        &self,
        account: Option<AccountIdentity>,
        process_name: &str,
        mapping: &ProcessMapping,
        pid: Option<u32>,
        process_started_at_secs: Option<u64>,
    ) -> Option<String> {
        let account = account?;
        let record = SessionRecord::new(
            account,
            ProcessIdentity {
                executable: process_name.to_string(),
                pid,
                started_at_secs: process_started_at_secs,
                ..Default::default()
            },
            SessionTarget::Mapped(mapping.clone()),
            now_secs(),
        );
        let id = record.id.clone();
        self.with_ledger("recording session start", |l| l.insert(&record, SubmissionState::Active))?;
        log::info!(
            "[LilyPad] session {id} started: {process_name} -> {} #{}",
            mapping.r#type, mapping.froglog_id
        );
        Some(id)
    }

    /// Records an unmapped game's session as it starts, so a crash mid-play still leaves
    /// something to credit into New Games at next startup.
    pub fn begin_unmapped(
        &self,
        account: Option<AccountIdentity>,
        start: &UnmappedSessionStart,
    ) -> Option<String> {
        let account = account?;
        let record = SessionRecord::new(
            account,
            ProcessIdentity {
                executable: start.exe_name.clone(),
                pid: u32::try_from(usize::from(start.pid)).ok(),
                started_at_secs: start.process_started_at_secs,
                ..Default::default()
            },
            SessionTarget::Unmapped {
                appid: start.appid.clone(),
                title: start.title.clone(),
                replay_of: start.replay_of.as_ref().map(|r| ReplayOf {
                    id: r.id,
                    game_type: r.game_type.clone(),
                    title: r.title.clone(),
                    status: r.status.clone(),
                }),
            },
            now_secs(),
        );
        let id = record.id.clone();
        self.with_ledger("recording an unmapped session start", |l| {
            l.insert(&record, SubmissionState::Active)
        })?;
        log::info!("[LilyPad] unmapped session {id} started: {} ({})", start.title, start.appid);
        Some(id)
    }

    /// Closes a specific record for the caller that observed its process exit. `finish` only
    /// applies to an active record, which is the stale-worker test. A storage failure is *not*
    /// treated as stale -- losing a real session would be worse than a duplicate prompt.
    pub fn complete(&self, id: &str, ended_at: u64) -> Completion {
        match self.with_ledger("recording session end", |l| l.finish(id, ended_at)) {
            Some(true) => Completion::Owned(Some(id.to_string())),
            Some(false) => Completion::Stale,
            None => Completion::Owned(None),
        }
    }

    /// Credits a normally-ended unmapped session into New Games and settles its record in one
    /// transaction. Returns the entry's new total hours.
    pub fn complete_unmapped(&self, id: &str) -> Option<f64> {
        self.with_ledger("completing an unmapped session", |l| {
            l.complete_unmapped(id, now_secs(), false)
        })
        .flatten()
    }

    /// Credits an unmapped session that has no start record (its start could not be stored)
    /// straight into New Games, dated today. Returns the entry's new total hours.
    pub fn record_new_game(
        &self,
        account: &AccountIdentity,
        appid: &str,
        title: &str,
        exe_name: &str,
        hours: f64,
        replay_of: Option<ReplayOf>,
    ) -> Option<f64> {
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        self.with_ledger("recording a new game session", |l| {
            l.record_new_game_session(Some(account), appid, title, exe_name, hours, replay_of, date, now_secs())
        })
    }

    /// The recorded owner of `id`. Outer `None`: unknown (unavailable/missing record).
    pub fn owner(&self, id: &str) -> Option<Option<AccountIdentity>> {
        self.with_ledger("reading a session owner", |l| Ok(l.get(id)?.map(|s| s.record.account)))
            .flatten()
    }

    /// Queues a session for retry after a failed submission. Returns whether it is durably saved,
    /// so a caller never claims "saved to Pending Submissions" when it was not.
    ///
    /// The record produced when the session ended *is* the queue entry; this only attaches what
    /// to send and why it failed. With no record behind the submission, one is inserted under
    /// `account` rather than dropping it.
    pub fn queue_failed(
        &self,
        ledger_id: Option<&str>,
        account: Option<AccountIdentity>,
        submission: Submission,
    ) -> bool {
        if let Some(id) = ledger_id {
            return match self.with_ledger("queueing a failed submission", |l| l.set_submission(id, &submission)) {
                Some(true) => {
                    log::info!(
                        "[LilyPad] session {id} queued for retry ({}): {}",
                        submission.title,
                        submission.last_error.as_deref().unwrap_or("no error recorded")
                    );
                    true
                }
                // Not pending: already acknowledged or dismissed. Re-queueing would resubmit a
                // session the server has already accepted.
                Some(false) => {
                    log::warn!("[LilyPad] session {id} is not pending; not queueing it again");
                    false
                }
                None => false,
            };
        }
        let record = SessionRecord {
            id: crate::session_ledger::new_session_id(),
            account,
            process: ProcessIdentity::default(),
            target: SessionTarget::LegacyPending(PendingSession {
                id: String::new(),
                game_id: submission.game_id,
                game_type: submission.game_type.clone(),
                title: submission.title.clone(),
                hours: submission.hours,
                notes: submission.notes.clone(),
                spoiler: submission.spoiler,
                is_public: submission.is_public,
                date: submission.date.clone(),
                failed_at: submission.failed_at.clone().unwrap_or_default(),
                error: submission.last_error.clone().unwrap_or_default(),
            }),
            started_at_secs: None,
            last_alive_secs: None,
            ended_at_secs: Some(now_secs()),
            recovered: false,
            submission: Some(submission),
        };
        log::warn!(
            "[LilyPad] a submission failed with no session record behind it; queueing it as {}",
            record.id
        );
        self.with_ledger("queueing an orphaned submission", |l| l.insert(&record, SubmissionState::Pending))
            .is_some()
    }

    /// Records the server's acknowledgement. `Some(false)`: no pending record under that id.
    pub fn acknowledge(&self, id: &str, account: &AccountIdentity, remote_id: &str) -> Option<bool> {
        let applied = self.with_ledger("recording submission", |l| l.acknowledge(id, account, remote_id))?;
        if applied {
            log::info!("[LilyPad] session {id} acknowledged as remote {remote_id}");
        }
        Some(applied)
    }

    /// User-initiated discard of an owned session. Keeps the row, distinguishable from one that
    /// was never recorded.
    pub fn discard(&self, id: &str, account: &AccountIdentity) -> Option<bool> {
        let applied = self.with_ledger("discarding a session", |l| l.dismiss_owned(id, account))?;
        if applied {
            log::info!("[LilyPad] session {id} discarded at the user's request");
        }
        Some(applied)
    }

    pub fn pending_sessions(&self, account: &AccountIdentity) -> Option<Vec<PendingSession>> {
        self.with_ledger("reading the retry queue", |l| l.pending_sessions(account))
    }

    /// Re-reads one queue row at action time; a stale UI row is not permission to submit it.
    pub fn pending_session(&self, id: &str, account: &AccountIdentity) -> Option<PendingSession> {
        self.with_ledger("reading a pending session", |l| l.pending_session(id, account))
    }

    pub fn new_games(&self, account: &AccountIdentity) -> Option<Vec<PendingGameSubmission>> {
        self.with_ledger("reading the new-games queue", |l| l.new_games(Some(account)))
    }

    /// Records that the New Games entry for `appid` became `remote_id` (e.g. `session:42`).
    pub fn resolve_new_game(&self, account: &AccountIdentity, appid: &str, remote_id: &str) -> Option<bool> {
        self.with_ledger("recording a resolved new game", |l| {
            l.resolve_new_game(Some(account), appid, remote_id)
        })
    }

    pub fn dismiss_new_game(&self, account: &AccountIdentity, appid: &str) -> Option<bool> {
        self.with_ledger("discarding a new game", |l| l.dismiss_new_game(Some(account), appid))
    }

    /// Outstanding records nobody owns, for explicit ownership resolution.
    pub fn unowned(&self) -> Option<Vec<UnownedRecord>> {
        self.with_ledger("reading unowned sessions", |l| l.outstanding(None)).map(|rows| {
            rows.into_iter()
                .map(|s| UnownedRecord { summary: describe(&s.record, s.state), id: s.record.id })
                .collect()
        })
    }

    /// Assigns an unowned record to `account`. An interrupted (still `active`) one is closed at
    /// its last checkpoint as it is adopted, so it lands in a queue rather than waiting for the
    /// next startup's recovery.
    pub fn adopt(&self, id: &str, account: &AccountIdentity) -> Option<bool> {
        self.with_ledger("assigning a session to this account", |l| {
            let Some(stored) = l.get(id)? else { return Ok(false) };
            if stored.record.account.is_some() {
                return Err("Session already belongs to an account".into());
            }
            if !l.adopt(id, account)? {
                return Ok(false);
            }
            if stored.state == SubmissionState::Active {
                match stored.record.target {
                    SessionTarget::Unmapped { .. } => {
                        l.complete_unmapped(id, now_secs(), true)?;
                    }
                    _ => {
                        let ended_at = stored
                            .record
                            .last_alive_secs
                            .or(stored.record.started_at_secs)
                            .unwrap_or_else(now_secs);
                        l.finish(id, ended_at)?;
                    }
                }
            }
            log::info!("[LilyPad] session {id} assigned to account {}", account.account_id);
            Ok(true)
        })
    }

    /// Discards an unowned record. Refuses an owned one: that has its own owner-checked path.
    pub fn discard_unowned(&self, id: &str) -> Option<bool> {
        self.with_ledger("discarding an unowned session", |l| {
            let Some(stored) = l.get(id)? else { return Ok(false) };
            if stored.record.account.is_some() {
                return Err("Session belongs to an account".into());
            }
            l.dismiss(id)
        })
    }

    pub fn counts(&self, account: Option<&AccountIdentity>) -> QueueCounts {
        QueueCounts {
            pending: account.and_then(|a| {
                self.with_ledger("reading unsubmitted sessions", |l| l.unsubmitted_sessions(Some(a)))
                    .map(|v| v.len())
            }),
            new_games: account.and_then(|a| self.new_games(a).map(|v| v.len())),
            unowned: self.with_ledger("reading unowned sessions", |l| l.outstanding(None)).map(|v| v.len()),
        }
    }

    /// Resolves every session this account left `active`, before the monitor starts.
    ///
    /// Unmapped records are credited into New Games up to their last checkpoint. Only the first
    /// still-running mapped game is resumed (LilyPad tracks one mapped session at a time); every
    /// other mapped record is closed at its checkpoint and returned for the normal end path.
    pub fn recover_interrupted(
        &self,
        account: &AccountIdentity,
        find_process: impl Fn(&ProcessIdentity) -> Option<Pid>,
    ) -> Vec<Recovered> {
        let Some(interrupted) = self.with_ledger("reading interrupted sessions", |l| l.interrupted(Some(account))) else {
            return Vec::new();
        };
        if !interrupted.is_empty() {
            log::info!("[LilyPad] session store: resolving {} interrupted session(s)", interrupted.len());
        }
        let mut out = Vec::new();
        let mut resumed = false;
        for stored in interrupted {
            let record = stored.record;
            let checkpoint = record.last_alive_secs.or(record.started_at_secs).unwrap_or_else(now_secs);
            match record.target.clone() {
                SessionTarget::Unmapped { appid, title, .. } => {
                    if let Some(Some(total)) = self.with_ledger("recovering an unmapped session", |l| {
                        l.complete_unmapped(&record.id, now_secs(), true)
                    }) {
                        log::info!("[LilyPad] recovered {title} ({appid}) through its last checkpoint; {total}h awaiting resolution");
                    }
                }
                SessionTarget::Mapped(mapping) => {
                    let pid = (!resumed).then(|| find_process(&record.process)).flatten();
                    if let Some(pid) = pid {
                        resumed = true;
                        log::info!("[LilyPad] session {} resumed: {} still running as pid {pid}", record.id, record.process.executable);
                        out.push(Recovered::Resume { record, mapping, pid });
                        continue;
                    }
                    let duration_secs = ledger_session::interrupted_duration_secs(&record);
                    if self.with_ledger("closing interrupted session", |l| l.finish(&record.id, checkpoint)) == Some(true) {
                        log::info!(
                            "[LilyPad] session {} closed on recovery: {duration_secs:.0}s credited up to last checkpoint",
                            record.id
                        );
                        out.push(Recovered::Ended {
                            id: record.id,
                            process_name: record.process.executable,
                            mapping,
                            duration_secs,
                        });
                    }
                }
                // Nothing to submit against: close rather than guess, and rather than
                // re-resolving it on every subsequent startup.
                _ => {
                    log::warn!("[LilyPad] session {} was active with no target to resolve; closing it", record.id);
                    self.with_ledger("closing an unresolvable interrupted session", |l| l.finish(&record.id, checkpoint));
                }
            }
        }
        out
    }
}

/// One line describing a record, for the ownership-resolution view.
fn describe(record: &SessionRecord, state: SubmissionState) -> String {
    let date = |secs: Option<u64>| {
        secs.and_then(|s| chrono::DateTime::from_timestamp(s as i64, 0))
            .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "unknown date".into())
    };
    match &record.target {
        SessionTarget::LegacyPending(p) => format!("{}: {}h on {} (failed submission)", p.title, p.hours, p.date),
        SessionTarget::LegacyNewGame(g) => format!(
            "{}: {}h over {} session{} (not in your library)",
            g.title, g.hours, g.session_count, if g.session_count == 1 { "" } else { "s" }
        ),
        SessionTarget::Mapped(m) => {
            let title = m.title.clone().unwrap_or_else(|| m.process.clone());
            let what = if state == SubmissionState::Active { "interrupted session" } else { "unsent session" };
            format!("{title}: {what} from {}", date(record.started_at_secs))
        }
        SessionTarget::Unmapped { title, .. } => {
            format!("{title}: interrupted session from {}", date(record.started_at_secs))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "https://api.example.test/api";

    fn store() -> SessionStore {
        SessionStore::from_ledger(SessionLedger::open(Path::new(":memory:")).unwrap(), SERVER)
    }

    fn account(name: &str) -> AccountIdentity {
        AccountIdentity { server: SERVER.into(), account_id: name.into() }
    }

    fn mapping() -> ProcessMapping {
        ProcessMapping {
            process: "game".into(), r#type: "session".into(), froglog_id: 7,
            title: Some("Game".into()), title_filter: None, exe_path: None,
        }
    }

    fn submission(error: &str) -> Submission {
        Submission {
            game_id: 7, game_type: "session".into(), title: "Game".into(), hours: 1.25,
            date: "2026-09-25".into(), notes: Some("note".into()), spoiler: true, is_public: false,
            last_error: Some(error.into()), failed_at: Some("2026-09-25T10:00:00Z".into()),
        }
    }

    #[test]
    fn a_failed_submission_queues_the_sessions_own_record_and_acknowledges_once() {
        let store = store();
        let alice = account("alice");
        let id = store.begin_mapped(Some(alice.clone()), "game", &mapping(), Some(42), Some(100)).unwrap();
        assert_eq!(store.complete(&id, now_secs()), Completion::Owned(Some(id.clone())));
        // A second waiter for the same process must not present the session again.
        assert_eq!(store.complete(&id, now_secs()), Completion::Stale);

        assert!(store.queue_failed(Some(&id), Some(alice.clone()), submission("offline")));
        let rows = store.pending_sessions(&alice).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].error, "offline");
        assert!(!rows[0].is_public && rows[0].spoiler);

        // Another account can neither see nor acknowledge it.
        assert!(store.pending_sessions(&account("bob")).unwrap().is_empty());
        assert_eq!(store.acknowledge(&id, &account("bob"), "session:1"), None);

        assert_eq!(store.acknowledge(&id, &alice, "session:1"), Some(true));
        assert!(store.pending_sessions(&alice).unwrap().is_empty());
        // Acknowledged: a late failure path cannot re-queue it.
        assert!(!store.queue_failed(Some(&id), Some(alice), submission("late")));
    }

    #[test]
    fn a_stale_waiter_cannot_take_the_next_sessions_record() {
        let active = ActiveRecord::default();
        // Game X force-stopped: the tray takes its id.
        active.set("x.exe", Some("id-x".into()));
        assert_eq!(active.take().as_deref(), Some("id-x"));
        // Game Y starts while X is still running.
        active.set("y.exe", Some("id-y".into()));
        // X finally exits: its waiter must not end Y's session.
        assert_eq!(active.take_for("x.exe"), None);
        active.clear_if("id-x");
        // Y's own exit gets its own record.
        assert_eq!(active.take_for("Y.EXE").as_deref(), Some("id-y"));
        assert_eq!(active.take(), None);
    }

    #[test]
    fn a_submission_with_no_record_is_still_saved_under_its_account() {
        let store = store();
        let alice = account("alice");
        assert!(store.queue_failed(None, Some(alice.clone()), submission("offline")));
        let rows = store.pending_sessions(&alice).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].notes.as_deref(), Some("note"));
    }

    #[test]
    fn an_unavailable_store_reports_unknown_rather_than_empty() {
        let store = SessionStore::unavailable("disk on fire".into(), SERVER);
        let alice = account("alice");
        assert!(store.pending_sessions(&alice).is_none());
        assert!(store.new_games(&alice).is_none());
        assert!(!store.queue_failed(None, Some(alice.clone()), submission("x")));
        assert_eq!(store.complete("id", 1), Completion::Owned(None));
        assert!(store.counts(Some(&alice)).pending.is_none());
        assert_eq!(store.error().as_deref(), Some("disk on fire"));
    }

    #[test]
    fn adopting_an_interrupted_legacy_session_closes_it_into_the_retry_queue() {
        let store = store();
        let mut record = SessionRecord::new(
            account("x"), ProcessIdentity { executable: "game".into(), ..Default::default() },
            SessionTarget::Mapped(mapping()), 1_000,
        );
        record.account = None;
        record.last_alive_secs = Some(1_000 + 1_800);
        store.with_ledger("test", |l| l.insert(&record, SubmissionState::Active)).unwrap();

        let unowned = store.unowned().unwrap();
        assert_eq!(unowned.len(), 1);
        assert!(unowned[0].summary.contains("interrupted session"));

        let alice = account("alice");
        assert_eq!(store.adopt(&record.id, &alice), Some(true));
        assert!(store.unowned().unwrap().is_empty());
        let rows = store.pending_sessions(&alice).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hours, 0.5, "credited up to the last checkpoint, not until now");
        // Adoption is one-way.
        assert_eq!(store.adopt(&record.id, &account("bob")), None);
        assert_eq!(store.discard_unowned(&record.id), None);
    }

    #[test]
    fn recovery_resumes_a_running_game_and_closes_the_rest_at_their_checkpoints() {
        let store = store();
        let alice = account("alice");
        let running = store.begin_mapped(Some(alice.clone()), "game", &mapping(), Some(42), Some(100)).unwrap();
        let gone = store.begin_mapped(Some(alice.clone()), "other", &mapping(), Some(43), Some(100)).unwrap();
        store.with_ledger("test", |l| l.checkpoint(&gone, now_secs() + 600)).unwrap();

        let recovered = store.recover_interrupted(&alice, |p| (p.pid == Some(42)).then(|| Pid::from(42usize)));
        assert_eq!(recovered.len(), 2);
        assert!(matches!(&recovered[0], Recovered::Resume { record, .. } if record.id == running));
        match &recovered[1] {
            Recovered::Ended { id, duration_secs, .. } => {
                assert_eq!(id, &gone);
                assert!((599.0..=601.0).contains(duration_secs));
            }
            other => panic!("expected the stopped game to end, got {other:?}"),
        }
        // The resumed record is still active; the closed one is now pending.
        let pending = store.pending_sessions(&alice).unwrap();
        assert_eq!(pending.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), vec![gone.as_str()]);
        // Another account's recovery sees none of it.
        assert!(store.recover_interrupted(&account("bob"), |_| None).is_empty());
    }

    #[test]
    fn an_interrupted_unmapped_session_is_credited_into_new_games_on_recovery() {
        let store = store();
        let alice = account("alice");
        let start = UnmappedSessionStart {
            title: "Celeste".into(), appid: "504230".into(), exe_name: "Celeste".into(),
            pid: Pid::from(9usize), process_started_at_secs: Some(1), replay_of: None,
        };
        let id = store.begin_unmapped(Some(alice.clone()), &start).unwrap();
        store.with_ledger("test", |l| l.checkpoint(&id, now_secs() + 3_600)).unwrap();
        assert!(store.recover_interrupted(&alice, |_| None).is_empty());
        let games = store.new_games(&alice).unwrap();
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].appid, "504230");
        assert!((0.99..=1.01).contains(&games[0].hours));
    }
}
