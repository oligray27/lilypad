//! Transactional storage for the reliability migration. No credentials belong in this store.
//! Legacy imports deliberately have no account owner: a global legacy queue cannot establish
//! which account created it. Callers must resolve that ownership before submitting anything.

use crate::config::{PendingGameSubmission, PendingSession, ProcessMapping};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{error::Error, path::Path, time::Duration};
use uuid::Uuid;

pub type LedgerResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// The legacy JSON files this ledger can absorb, in the order they are being cut over.
/// See `SessionLedger::import_legacy` for why callers name the subset they are replacing.
pub const LEGACY_ACTIVE_SESSION: &str = "active-session.json";
pub const LEGACY_PENDING_SESSIONS: &str = "pending-sessions.json";
pub const LEGACY_PENDING_GAME_SUBMISSIONS: &str = "pending-game-submissions.json";
pub const LEGACY_FILES: [&str; 3] = [
    LEGACY_ACTIVE_SESSION,
    LEGACY_PENDING_SESSIONS,
    LEGACY_PENDING_GAME_SUBMISSIONS,
];

/// A fresh record id. `insert` rejects anything that is not a UUID, so callers building a
/// record by hand use this rather than inventing a key.
pub fn new_session_id() -> String {
    Uuid::new_v4().to_string()
}

/// The ledger lives beside the legacy JSON files it replaces, in the existing app data dir.
pub fn ledger_path() -> std::path::PathBuf {
    crate::config::app_data_dir().join("sessions.sqlite")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountIdentity {
    /// Canonical API origin/base path, supplied by authentication orchestration.
    pub server: String,
    /// Stable server-issued account ID; never an access token.
    pub account_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub executable: String,
    pub canonical_path: Option<String>,
    pub pid: Option<u32>,
    pub started_at_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum SessionTarget {
    Mapped(ProcessMapping),
    Unmapped {
        appid: String,
        title: String,
        replay_of: Option<crate::config::ReplayOf>,
    },
    LegacyPending(PendingSession),
    /// Preserves aggregate-only legacy entries without inventing individual play dates.
    LegacyNewGame(PendingGameSubmission),
}

fn default_true() -> bool {
    true
}

/// What to send to the server for a session that has not been acknowledged yet, plus why the
/// last attempt failed.
///
/// This is what makes a ledger record self-sufficient as a retry queue entry: the record already
/// knows *which* session it is, and this says what submitting it means. It replaces
/// `pending-sessions.json`, whose entries were disconnected from the sessions that produced
/// them -- a failed submission used to become an unrelated queue row, so nothing could tell
/// whether a given session had been sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submission {
    pub game_id: i32,
    pub game_type: String,
    pub title: String,
    pub hours: f64,
    pub date: String,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub spoiler: bool,
    #[serde(default = "default_true")]
    pub is_public: bool,
    /// Last failure, for display. `None` if it has not been attempted yet.
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub failed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub account: Option<AccountIdentity>,
    pub process: ProcessIdentity,
    pub target: SessionTarget,
    pub started_at_secs: Option<u64>,
    pub last_alive_secs: Option<u64>,
    pub ended_at_secs: Option<u64>,
    pub recovered: bool,
    /// Set once the session has ended and what to send has been worked out. Absent while a
    /// session is still active. Defaults to absent for records written before this field.
    #[serde(default)]
    pub submission: Option<Submission>,
}

impl SessionRecord {
    pub fn new(
        account: AccountIdentity,
        process: ProcessIdentity,
        target: SessionTarget,
        now: u64,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            account: Some(account),
            process,
            target,
            started_at_secs: Some(now),
            last_alive_secs: Some(now),
            ended_at_secs: None,
            recovered: false,
            submission: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionState {
    Active,
    Pending,
    Acknowledged,
    Dismissed,
}

impl SubmissionState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Pending => "pending",
            Self::Acknowledged => "acknowledged",
            Self::Dismissed => "dismissed",
        }
    }
}

#[derive(Debug)]
pub struct StoredSession {
    pub record: SessionRecord,
    pub state: SubmissionState,
    pub remote_id: Option<String>,
}

pub struct SessionLedger {
    connection: Connection,
}

impl SessionLedger {
    pub fn open(path: &Path) -> LedgerResult<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(10))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > 1 {
            return Err(format!("Unsupported session ledger version {version}").into());
        }
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY NOT NULL,
                 record TEXT NOT NULL,
                 state TEXT NOT NULL CHECK(state IN ('active','pending','acknowledged','dismissed')),
                 remote_id TEXT
             );
             CREATE TABLE IF NOT EXISTS legacy_imports (
                 filename TEXT PRIMARY KEY NOT NULL,
                 original_json TEXT NOT NULL
             );
             PRAGMA user_version=1;
             COMMIT;"
        )?;
        Ok(Self { connection })
    }

    /// Inserts only; a duplicate ID must never overwrite a previously acknowledged record.
    pub fn insert(&self, record: &SessionRecord, state: SubmissionState) -> LedgerResult<()> {
        Uuid::parse_str(&record.id)?;
        self.connection.execute(
            "INSERT INTO sessions(id,record,state) VALUES (?1,?2,?3)",
            params![record.id, serde_json::to_string(record)?, state.as_str()],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> LedgerResult<Option<StoredSession>> {
        let raw: Option<(String, String, Option<String>)> = self
            .connection
            .query_row(
                "SELECT record,state,remote_id FROM sessions WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        raw.map(|(json, state, remote_id)| {
            let state = match state.as_str() {
                "active" => SubmissionState::Active,
                "pending" => SubmissionState::Pending,
                "acknowledged" => SubmissionState::Acknowledged,
                "dismissed" => SubmissionState::Dismissed,
                _ => return Err(format!("Invalid ledger state: {state}").into()),
            };
            Ok(StoredSession {
                record: serde_json::from_str(&json)?,
                state,
                remote_id,
            })
        })
        .transpose()
    }

    /// Enumerate recoverable work without mixing account queues. Passing None returns only
    /// unowned legacy records, for explicit ownership resolution via `adopt`.
    pub fn outstanding(
        &self,
        account: Option<&AccountIdentity>,
    ) -> LedgerResult<Vec<StoredSession>> {
        self.query("state IN ('active','pending')", account)
    }

    /// Sessions interrupted mid-play: a record left `active` is one whose process was still
    /// running the last time anything wrote to it, so LilyPad died before seeing the exit.
    pub fn interrupted(
        &self,
        account: Option<&AccountIdentity>,
    ) -> LedgerResult<Vec<StoredSession>> {
        self.query("state='active'", account)
    }

    /// Every record awaiting resolution: both retryable sessions and new-game entries.
    ///
    /// Two disjoint queues live in `pending`, and callers almost always want one specifically —
    /// see `unsubmitted_sessions` and `new_games`. This is the union, for counting.
    pub fn unsubmitted(
        &self,
        account: Option<&AccountIdentity>,
    ) -> LedgerResult<Vec<StoredSession>> {
        self.query("state='pending'", account)
    }

    /// Completed sessions awaiting server acknowledgement: the retry queue.
    ///
    /// Excludes new-game entries. Those are a different queue with a different resolution flow —
    /// they have no `game_id` to submit against, because the whole point is that the game is not
    /// in the library yet. Retrying one would post to game 0.
    pub fn unsubmitted_sessions(
        &self,
        account: Option<&AccountIdentity>,
    ) -> LedgerResult<Vec<StoredSession>> {
        Ok(self
            .unsubmitted(account)?
            .into_iter()
            .filter(|s| !matches!(s.record.target, SessionTarget::LegacyNewGame(_)))
            .collect())
    }

    fn query(
        &self,
        predicate: &str,
        account: Option<&AccountIdentity>,
    ) -> LedgerResult<Vec<StoredSession>> {
        let mut statement = self
            .connection
            .prepare(&format!("SELECT id FROM sessions WHERE {predicate} ORDER BY rowid"))?;
        let ids = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut sessions = Vec::new();
        for id in ids {
            if let Some(stored) = self.get(&id?)? {
                if stored.record.account.as_ref() == account {
                    sessions.push(stored);
                }
            }
        }
        Ok(sessions)
    }

    /// Records what a pending session should send, and why the last attempt failed.
    ///
    /// Applies only to a record that is still `pending`: an acknowledged session must never be
    /// re-queued, and an active one has not ended yet. Returns whether it applied.
    pub fn set_submission(&mut self, id: &str, submission: &Submission) -> LedgerResult<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let json: Option<String> = tx
            .query_row(
                "SELECT record FROM sessions WHERE id=?1 AND state='pending'",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(json) = json else { return Ok(false) };
        let mut record: SessionRecord = serde_json::from_str(&json)?;
        record.submission = Some(submission.clone());
        tx.execute(
            "UPDATE sessions SET record=?2 WHERE id=?1",
            params![id, serde_json::to_string(&record)?],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Accumulates one completed play session for a game that is not in the library yet,
    /// merging into the existing entry for the same appid if there is one. Returns that entry's
    /// updated total hours.
    ///
    /// The merge happens inside one immediate transaction, which is the point of moving this off
    /// `pending-game-submissions.json`: the old version read the whole file, mutated it in
    /// memory and wrote it back, so two games finishing at once could lose one of them entirely.
    #[allow(clippy::too_many_arguments)]
    pub fn record_new_game_session(
        &mut self,
        account: Option<&AccountIdentity>,
        appid: &str,
        title: &str,
        exe_name: &str,
        hours: f64,
        replay_of: Option<crate::config::ReplayOf>,
        date: String,
        now: u64,
    ) -> LedgerResult<f64> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = find_pending_new_game(&tx, account, appid)?;
        let entry = crate::config::PendingGameSessionEntry { date, hours };

        let total = match existing {
            Some((id, mut record)) => {
                let SessionTarget::LegacyNewGame(ref mut game) = record.target else {
                    return Err("Pending new-game record has the wrong target".into());
                };
                // Keep the executable of the *longest* session, not the most recent one.
                //
                // A game can put several executables through this entry: Hotline Miami runs
                // `HotlineMiami.exe` for about a second and then `HotlineGL.exe` for the real
                // session, both resolving to the same appid. `exe_name` is what a later resolve
                // turns into a `ProcessMapping`, so taking the last writer meant the mapping
                // could end up pinned to the launcher — and then every future session would be
                // the one second the launcher lives, rather than the play session.
                let longest_so_far = game
                    .sessions
                    .iter()
                    .map(|s| s.hours)
                    .fold(0.0_f64, f64::max);
                if hours >= longest_so_far {
                    game.exe_name = exe_name.to_string();
                }
                game.hours += hours;
                game.session_count += 1;
                game.last_session_secs = now;
                game.title = title.to_string();
                game.replay_of = replay_of;
                game.sessions.push(entry);
                let total = game.hours;
                record.last_alive_secs = Some(now);
                record.ended_at_secs = Some(now);
                tx.execute(
                    "UPDATE sessions SET record=?2 WHERE id=?1",
                    params![id, serde_json::to_string(&record)?],
                )?;
                total
            }
            None => {
                let game = PendingGameSubmission {
                    appid: appid.to_string(),
                    title: title.to_string(),
                    hours,
                    session_count: 1,
                    first_seen_secs: now,
                    last_session_secs: now,
                    exe_name: exe_name.to_string(),
                    replay_of,
                    sessions: vec![entry],
                };
                let record = SessionRecord {
                    id: new_session_id(),
                    account: account.cloned(),
                    process: ProcessIdentity {
                        executable: exe_name.to_string(),
                        ..Default::default()
                    },
                    target: SessionTarget::LegacyNewGame(game),
                    started_at_secs: Some(now),
                    last_alive_secs: Some(now),
                    ended_at_secs: Some(now),
                    recovered: false,
                    submission: None,
                };
                tx.execute(
                    "INSERT INTO sessions(id,record,state) VALUES (?1,?2,?3)",
                    params![
                        record.id,
                        serde_json::to_string(&record)?,
                        SubmissionState::Pending.as_str()
                    ],
                )?;
                hours
            }
        };
        tx.commit()?;
        Ok(total)
    }

    /// Games played but not yet in the library, for this account, oldest first.
    pub fn new_games(
        &self,
        account: Option<&AccountIdentity>,
    ) -> LedgerResult<Vec<PendingGameSubmission>> {
        Ok(self
            .unsubmitted(account)?
            .into_iter()
            .filter_map(|stored| match stored.record.target {
                SessionTarget::LegacyNewGame(game) => Some(game),
                _ => None,
            })
            .collect())
    }

    /// Records that the pending new-game entry for `appid` became a real library entry.
    ///
    /// Acknowledged rather than dismissed, and deliberately so: resolving creates a game and
    /// logs its sessions server-side, which is exactly what acknowledgement means everywhere
    /// else in this store. `remote_id` says what it became. Both outcomes used to be
    /// `dismissed`, which made a game the user added indistinguishable from one they binned —
    /// and the reason rows are kept at all is to keep outcomes distinguishable.
    pub fn resolve_new_game(
        &mut self,
        account: Option<&AccountIdentity>,
        appid: &str,
        remote_id: &str,
    ) -> LedgerResult<bool> {
        if remote_id.is_empty() {
            return Err("Remote reference for a resolved game is empty".into());
        }
        self.settle_new_game(account, appid, "acknowledged", Some(remote_id))
    }

    /// Discards the pending new-game entry for `appid`: the user chose not to add it. Keeps the
    /// row, distinguishable from one that was resolved.
    pub fn dismiss_new_game(
        &mut self,
        account: Option<&AccountIdentity>,
        appid: &str,
    ) -> LedgerResult<bool> {
        self.settle_new_game(account, appid, "dismissed", None)
    }

    fn settle_new_game(
        &mut self,
        account: Option<&AccountIdentity>,
        appid: &str,
        state: &str,
        remote_id: Option<&str>,
    ) -> LedgerResult<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some((id, _)) = find_pending_new_game(&tx, account, appid)? else {
            return Ok(false);
        };
        tx.execute(
            "UPDATE sessions SET state=?2,remote_id=?3 WHERE id=?1",
            params![id, state, remote_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// User-initiated discard. Keeps the row so a discarded session is never silently
    /// indistinguishable from one that was never recorded.
    pub fn dismiss(&self, id: &str) -> LedgerResult<bool> {
        Ok(self.connection.execute(
            "UPDATE sessions SET state='dismissed' WHERE id=?1 AND state IN ('active','pending')",
            [id],
        )? == 1)
    }

    /// Assigns an owner to an imported legacy record, which arrives unowned because the old
    /// global queue files never recorded which account produced them. Deliberately explicit:
    /// guessing here would submit one account's sessions to another. Refuses to move a record
    /// that already has an owner.
    pub fn adopt(&mut self, id: &str, account: &AccountIdentity) -> LedgerResult<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let json: Option<String> = tx
            .query_row("SELECT record FROM sessions WHERE id=?1", [id], |r| r.get(0))
            .optional()?;
        let Some(json) = json else { return Ok(false) };
        let mut record: SessionRecord = serde_json::from_str(&json)?;
        if record.account.is_some() {
            return Err("Session already belongs to an account".into());
        }
        record.account = Some(account.clone());
        tx.execute(
            "UPDATE sessions SET record=?2 WHERE id=?1",
            params![id, serde_json::to_string(&record)?],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// The transaction prevents a stale heartbeat from resurrecting a completed session.
    pub fn checkpoint(&mut self, id: &str, alive_at: u64) -> LedgerResult<bool> {
        self.update_active(id, alive_at, false)
    }

    /// Persists completion before presentation/submission. Repeated completion is a no-op.
    pub fn finish(&mut self, id: &str, ended_at: u64) -> LedgerResult<bool> {
        self.update_active(id, ended_at, true)
    }

    fn update_active(&mut self, id: &str, at: u64, finish: bool) -> LedgerResult<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let json: Option<String> = tx
            .query_row(
                "SELECT record FROM sessions WHERE id=?1 AND state='active'",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(json) = json else {
            return Ok(false);
        };
        let mut record: SessionRecord = serde_json::from_str(&json)?;
        let at = at
            .max(record.started_at_secs.unwrap_or(0))
            .max(record.last_alive_secs.unwrap_or(0));
        record.last_alive_secs = Some(at);
        if finish {
            record.ended_at_secs = Some(at);
        }
        tx.execute(
            "UPDATE sessions SET record=?2,state=?3 WHERE id=?1",
            params![
                id,
                serde_json::to_string(&record)?,
                if finish { "pending" } else { "active" }
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Retains the local record after acknowledgement. Unknown legacy ownership is rejected.
    pub fn acknowledge(
        &self,
        id: &str,
        account: &AccountIdentity,
        remote_id: &str,
    ) -> LedgerResult<bool> {
        if remote_id.is_empty() {
            return Err("Remote acknowledgement ID is empty".into());
        }
        let Some(stored) = self.get(id)? else {
            return Ok(false);
        };
        if stored.record.account.as_ref() != Some(account) {
            return Err("Session account does not match submission account".into());
        }
        Ok(self.connection.execute(
            "UPDATE sessions SET state='acknowledged',remote_id=?2 WHERE id=?1 AND state='pending'",
            params![id, remote_id],
        )? == 1)
    }

    /// Forces subsequent writes to fail, so callers' storage-failure handling can be tested.
    #[cfg(test)]
    pub fn force_read_only_for_test(&self) {
        self.connection.execute_batch("PRAGMA query_only=ON").unwrap();
    }

    /// Imports the named files in one transaction. Malformed/unreadable input aborts the whole
    /// import; originals are never renamed/deleted.
    ///
    /// `files` is explicit rather than "all of them" because each legacy file is imported
    /// exactly once, and the cutover happens one file at a time: importing a file whose legacy
    /// writer is still running would silently strand every write that writer makes afterwards.
    /// Pass a file only in the same change that stops the old code writing it. `LEGACY_FILES`
    /// lists every importable name for the final cutover.
    pub fn import_legacy(&mut self, directory: &Path, files: &[&str]) -> LedgerResult<usize> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut imported = 0;
        for filename in files.iter().copied() {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM legacy_imports WHERE filename=?1)",
                [filename],
                |r| r.get(0),
            )?;
            if exists {
                continue;
            }
            let json = match std::fs::read_to_string(directory.join(filename)) {
                Ok(json) => json,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("Cannot read {filename}: {e}").into()),
            };
            let records = legacy_records(filename, &json)
                .map_err(|e| format!("Cannot import {filename}: {e}"))?;
            for (record, state) in records {
                tx.execute(
                    "INSERT INTO sessions(id,record,state) VALUES (?1,?2,?3)",
                    params![record.id, serde_json::to_string(&record)?, state.as_str()],
                )?;
                imported += 1;
            }
            tx.execute(
                "INSERT INTO legacy_imports(filename,original_json) VALUES (?1,?2)",
                params![filename, json],
            )?;
        }
        tx.commit()?;
        Ok(imported)
    }
}

/// Finds the pending new-game record for `appid` owned by `account`, inside a transaction.
///
/// The account match is part of the key, not a filter applied afterwards: two accounts can each
/// have their own pending entry for the same game, and merging one into the other would attribute
/// someone else's play time.
fn find_pending_new_game(
    tx: &rusqlite::Transaction<'_>,
    account: Option<&AccountIdentity>,
    appid: &str,
) -> LedgerResult<Option<(String, SessionRecord)>> {
    let mut statement =
        tx.prepare("SELECT id,record FROM sessions WHERE state='pending' ORDER BY rowid")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, json) = row?;
        let record: SessionRecord = serde_json::from_str(&json)?;
        if record.account.as_ref() != account {
            continue;
        }
        if matches!(&record.target, SessionTarget::LegacyNewGame(g) if g.appid == appid) {
            return Ok(Some((id, record)));
        }
    }
    Ok(None)
}

fn legacy_records(
    filename: &str,
    json: &str,
) -> LedgerResult<Vec<(SessionRecord, SubmissionState)>> {
    let record = |target, process, started_at_secs, last_alive_secs| SessionRecord {
        id: Uuid::new_v4().to_string(),
        account: None,
        process,
        target,
        started_at_secs,
        last_alive_secs,
        ended_at_secs: None,
        recovered: true,
        submission: None,
    };
    Ok(match filename {
        LEGACY_ACTIVE_SESSION => {
            #[derive(Deserialize)]
            struct Active {
                process: String,
                game_type: String,
                froglog_id: i32,
                title: Option<String>,
                started_at_secs: u64,
                #[serde(default)]
                last_alive_secs: u64,
            }
            let active: Active = serde_json::from_str(json)?;
            let process = ProcessIdentity {
                executable: active.process.clone(),
                ..Default::default()
            };
            let mapping = ProcessMapping {
                process: active.process,
                r#type: active.game_type,
                froglog_id: active.froglog_id,
                title: active.title,
                title_filter: None,
                exe_path: None,
            };
            vec![(
                record(
                    SessionTarget::Mapped(mapping),
                    process,
                    Some(active.started_at_secs),
                    Some(active.last_alive_secs.max(active.started_at_secs)),
                ),
                SubmissionState::Active,
            )]
        }
        // Imported entries become live queue data, not just an audit trail: `submission` is
        // populated so they retry through exactly the same path as a natively-recorded pending
        // session, while `target` keeps the original row verbatim.
        LEGACY_PENDING_SESSIONS => serde_json::from_str::<Vec<PendingSession>>(json)?
            .into_iter()
            .map(|pending| {
                let submission = Submission {
                    game_id: pending.game_id,
                    game_type: pending.game_type.clone(),
                    title: pending.title.clone(),
                    hours: pending.hours,
                    date: pending.date.clone(),
                    notes: pending.notes.clone(),
                    spoiler: pending.spoiler,
                    is_public: pending.is_public,
                    last_error: Some(pending.error.clone()),
                    failed_at: Some(pending.failed_at.clone()),
                };
                let mut r = record(
                    SessionTarget::LegacyPending(pending),
                    ProcessIdentity::default(),
                    None,
                    None,
                );
                r.submission = Some(submission);
                (r, SubmissionState::Pending)
            })
            .collect(),
        LEGACY_PENDING_GAME_SUBMISSIONS => {
            serde_json::from_str::<Vec<PendingGameSubmission>>(json)?
                .into_iter()
                .map(|pending| {
                    let process = ProcessIdentity {
                        executable: pending.exe_name.clone(),
                        ..Default::default()
                    };
                    (
                        record(SessionTarget::LegacyNewGame(pending), process, None, None),
                        SubmissionState::Pending,
                    )
                })
                .collect()
        }
        _ => return Err("Unknown legacy session file".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionRecord {
        SessionRecord::new(
            AccountIdentity {
                server: "https://example.test/api".into(),
                account_id: "42".into(),
            },
            ProcessIdentity {
                executable: "game.exe".into(),
                pid: Some(123),
                started_at_secs: Some(90),
                ..Default::default()
            },
            SessionTarget::Unmapped {
                appid: "123".into(),
                title: "Game".into(),
                replay_of: None,
            },
            100,
        )
    }

    #[test]
    fn completion_survives_reopen_and_stale_workers_cannot_change_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let record = session();
        {
            let mut ledger = SessionLedger::open(&path).unwrap();
            ledger.insert(&record, SubmissionState::Active).unwrap();
            assert!(ledger.checkpoint(&record.id, 150).unwrap());
            assert!(ledger.finish(&record.id, 200).unwrap());
        }
        let mut ledger = SessionLedger::open(&path).unwrap();
        assert!(!ledger.checkpoint(&record.id, 300).unwrap());
        assert!(!ledger.finish(&record.id, 400).unwrap());
        let stored = ledger.get(&record.id).unwrap().unwrap();
        assert_eq!(stored.state, SubmissionState::Pending);
        assert_eq!(stored.record.ended_at_secs, Some(200));
        assert!(ledger
            .acknowledge(&record.id, record.account.as_ref().unwrap(), "remote-1")
            .unwrap());
        assert!(!ledger
            .acknowledge(&record.id, record.account.as_ref().unwrap(), "remote-2")
            .unwrap());
        assert!(ledger.insert(&record, SubmissionState::Active).is_err());
        assert_eq!(
            ledger
                .get(&record.id)
                .unwrap()
                .unwrap()
                .remote_id
                .as_deref(),
            Some("remote-1")
        );
    }

    #[test]
    fn crash_before_completion_keeps_checkpoint_and_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let record = session();
        {
            let mut ledger = SessionLedger::open(&path).unwrap();
            ledger.insert(&record, SubmissionState::Active).unwrap();
            ledger.checkpoint(&record.id, 180).unwrap();
            ledger.checkpoint(&record.id, 120).unwrap();
        }
        let ledger = SessionLedger::open(&path).unwrap();
        let stored = ledger.get(&record.id).unwrap().unwrap();
        assert_eq!(stored.state, SubmissionState::Active);
        assert_eq!(stored.record.last_alive_secs, Some(180));
        assert_eq!(stored.record.process.pid, Some(123));
        assert_eq!(stored.record.account, record.account);
    }

    #[test]
    fn independent_connections_preserve_simultaneous_inserts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let ledger = SessionLedger::open(&path).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let ledger = SessionLedger::open(&path).unwrap();
                    barrier.wait();
                    for _ in 0..25 {
                        ledger.insert(&session(), SubmissionState::Active).unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let count: i64 = ledger
            .connection
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 100);
    }

    const ACTIVE: &str = r#"{"process":"game.exe","game_type":"regular","froglog_id":7,"title":"Game","started_at_secs":100}"#;
    const PENDING: &str = r#"[{"id":"old-id","game_id":7,"game_type":"regular","title":"Game","hours":1.5,"notes":"keep me","spoiler":false,"is_public":true,"date":"2026-09-15","failed_at":"2026-09-15","error":"offline"}]"#;
    const NEW_GAME: &str = r#"[{"appid":"123","title":"Game","hours":2.5,"session_count":2,"first_seen_secs":100,"last_session_secs":200}]"#;

    #[test]
    fn imports_are_atomic_repeatable_and_preserve_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = SessionLedger::open(&dir.path().join("ledger.sqlite")).unwrap();
        std::fs::write(dir.path().join("active-session.json"), ACTIVE).unwrap();
        std::fs::write(dir.path().join("pending-sessions.json"), "broken").unwrap();
        assert!(ledger
            .import_legacy(dir.path(), &LEGACY_FILES)
            .unwrap_err()
            .to_string()
            .contains("pending-sessions.json"));
        let count: i64 = ledger
            .connection
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        std::fs::write(dir.path().join("pending-sessions.json"), PENDING).unwrap();
        std::fs::write(dir.path().join("pending-game-submissions.json"), NEW_GAME).unwrap();
        assert_eq!(ledger.import_legacy(dir.path(), &LEGACY_FILES).unwrap(), 3);
        assert_eq!(ledger.import_legacy(dir.path(), &LEGACY_FILES).unwrap(), 0);
        for (filename, expected) in [
            ("active-session.json", ACTIVE),
            ("pending-sessions.json", PENDING),
            ("pending-game-submissions.json", NEW_GAME),
        ] {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(filename)).unwrap(),
                expected
            );
            let backup: String = ledger
                .connection
                .query_row(
                    "SELECT original_json FROM legacy_imports WHERE filename=?1",
                    [filename],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(backup, expected);
        }
        let mut stmt = ledger
            .connection
            .prepare("SELECT id FROM sessions")
            .unwrap();
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for id in ids {
            let stored = ledger.get(&id).unwrap().unwrap();
            assert!(stored.record.account.is_none());
            assert!(ledger
                .acknowledge(&id, session().account.as_ref().unwrap(), "remote")
                .is_err());
            if let SessionTarget::LegacyNewGame(game) = stored.record.target {
                assert_eq!(game.hours, 2.5);
                assert_eq!(game.session_count, 2);
            }
        }
    }

    /// The cutover happens one file at a time, so importing a subset must not consume the
    /// files whose legacy writers are still running.
    #[test]
    fn a_partial_import_leaves_the_remaining_files_importable() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = SessionLedger::open(&dir.path().join("ledger.sqlite")).unwrap();
        std::fs::write(dir.path().join(LEGACY_ACTIVE_SESSION), ACTIVE).unwrap();
        std::fs::write(dir.path().join(LEGACY_PENDING_SESSIONS), PENDING).unwrap();

        assert_eq!(
            ledger
                .import_legacy(dir.path(), &[LEGACY_ACTIVE_SESSION])
                .unwrap(),
            1
        );
        assert_eq!(ledger.interrupted(None).unwrap().len(), 1);
        assert!(ledger.unsubmitted(None).unwrap().is_empty());

        // The still-live writer appends after the first import; the later cutover must see it.
        let grown = PENDING.replace("]", r#",{"id":"later","game_id":8,"game_type":"regular","title":"Later","hours":0.5,"notes":null,"spoiler":false,"is_public":true,"date":"2026-09-16","failed_at":"2026-09-16","error":"offline"}]"#);
        std::fs::write(dir.path().join(LEGACY_PENDING_SESSIONS), &grown).unwrap();
        assert_eq!(
            ledger
                .import_legacy(dir.path(), &LEGACY_FILES)
                .unwrap(),
            2
        );
        assert_eq!(ledger.unsubmitted(None).unwrap().len(), 2);
        assert_eq!(ledger.outstanding(None).unwrap().len(), 3);
    }

    #[test]
    fn legacy_records_submit_only_after_an_explicit_owner_is_assigned() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = SessionLedger::open(&dir.path().join("ledger.sqlite")).unwrap();
        std::fs::write(dir.path().join(LEGACY_PENDING_SESSIONS), PENDING).unwrap();
        ledger
            .import_legacy(dir.path(), &[LEGACY_PENDING_SESSIONS])
            .unwrap();
        let id = ledger.unsubmitted(None).unwrap()[0].record.id.clone();
        let account = session().account.unwrap();

        // Unowned: invisible to the account's queue, and not submittable.
        assert!(ledger.unsubmitted(Some(&account)).unwrap().is_empty());
        assert!(ledger.acknowledge(&id, &account, "remote").is_err());

        assert!(ledger.adopt(&id, &account).unwrap());
        assert_eq!(ledger.unsubmitted(Some(&account)).unwrap().len(), 1);
        assert!(ledger.unsubmitted(None).unwrap().is_empty());
        assert!(ledger.acknowledge(&id, &account, "remote").unwrap());

        // A second account cannot take a record that already has an owner.
        let mut other = account.clone();
        other.account_id = "someone-else".into();
        assert!(ledger.adopt(&id, &other).is_err());
        assert!(!ledger.adopt("no-such-id", &other).unwrap());
    }

    fn submission() -> Submission {
        Submission {
            game_id: 7,
            game_type: "session".into(),
            title: "Game".into(),
            hours: 1.5,
            date: "2026-09-16".into(),
            notes: None,
            spoiler: false,
            is_public: true,
            last_error: Some("offline".into()),
            failed_at: Some("2026-09-16T12:00:00Z".into()),
        }
    }

    /// The retry queue is now "records still pending", so acknowledging is what removes an entry
    /// from it -- there is no second list that could disagree with the record's own state.
    #[test]
    fn the_retry_queue_is_exactly_the_records_that_are_still_pending() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = SessionLedger::open(&dir.path().join("l.sqlite")).unwrap();
        let record = session();
        let account = record.account.clone().unwrap();
        ledger.insert(&record, SubmissionState::Active).unwrap();

        // An active session is not in the queue: it has not ended yet.
        assert!(ledger.unsubmitted(Some(&account)).unwrap().is_empty());
        // ...and cannot be given a submission payload.
        assert!(!ledger.set_submission(&record.id, &submission()).unwrap());

        ledger.finish(&record.id, 200).unwrap();
        assert!(ledger.set_submission(&record.id, &submission()).unwrap());
        let queued = ledger.unsubmitted(Some(&account)).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].record.submission.as_ref().unwrap().hours, 1.5);
        assert_eq!(
            queued[0].record.submission.as_ref().unwrap().last_error.as_deref(),
            Some("offline")
        );

        // Acknowledging removes it from the queue while keeping the row.
        ledger.acknowledge(&record.id, &account, "session:1").unwrap();
        assert!(ledger.unsubmitted(Some(&account)).unwrap().is_empty());
        assert!(ledger.get(&record.id).unwrap().is_some());
        // An acknowledged session can never be re-queued, however a caller reaches it.
        assert!(!ledger.set_submission(&record.id, &submission()).unwrap());
    }

    /// Imported legacy entries must retry through exactly the same path as natively-recorded
    /// ones, so the import populates `submission` rather than leaving the data only in `target`.
    #[test]
    fn imported_legacy_queue_entries_are_live_retry_data_not_just_an_audit_trail() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = SessionLedger::open(&dir.path().join("l.sqlite")).unwrap();
        std::fs::write(dir.path().join(LEGACY_PENDING_SESSIONS), PENDING).unwrap();
        ledger.import_legacy(dir.path(), &[LEGACY_PENDING_SESSIONS]).unwrap();

        let queued = ledger.unsubmitted(None).unwrap();
        assert_eq!(queued.len(), 1);
        let s = queued[0].record.submission.as_ref().expect("import left no submission payload");
        assert_eq!(s.game_id, 7);
        assert_eq!(s.hours, 1.5);
        assert_eq!(s.notes.as_deref(), Some("keep me"));
        assert_eq!(s.last_error.as_deref(), Some("offline"));
        // The original row is kept verbatim alongside it.
        assert!(matches!(queued[0].record.target, SessionTarget::LegacyPending(_)));
    }

    /// The old `record_pending_game_submission` read the whole JSON file, mutated it in memory
    /// and wrote it back. Two games finishing at once could therefore lose one outright. The
    /// merge is now one transaction, and repeated plays of the same game accumulate rather than
    /// creating duplicate entries.
    #[test]
    fn new_game_sessions_accumulate_per_appid_instead_of_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = SessionLedger::open(&dir.path().join("l.sqlite")).unwrap();
        let account = session().account.unwrap();

        let total = ledger
            .record_new_game_session(Some(&account), "570", "Dota", "dota.exe", 1.5, None, "2026-09-17".into(), 100)
            .unwrap();
        assert_eq!(total, 1.5);

        // A second sitting of the same game merges.
        let total = ledger
            .record_new_game_session(Some(&account), "570", "Dota", "dota.exe", 0.75, None, "2026-09-18".into(), 200)
            .unwrap();
        assert_eq!(total, 2.25);

        // A different game is its own entry.
        ledger
            .record_new_game_session(Some(&account), "620", "Portal 2", "portal2.exe", 3.0, None, "2026-09-18".into(), 300)
            .unwrap();

        let games = ledger.new_games(Some(&account)).unwrap();
        assert_eq!(games.len(), 2);
        let dota = games.iter().find(|g| g.appid == "570").unwrap();
        assert_eq!(dota.hours, 2.25);
        assert_eq!(dota.session_count, 2);
        // Each real-world sitting keeps its own date, rather than being flattened into a total.
        assert_eq!(dota.sessions.len(), 2);
        assert_eq!(dota.sessions[0].date, "2026-09-17");
        assert_eq!(dota.sessions[1].date, "2026-09-18");
        assert_eq!(dota.first_seen_secs, 100);
        assert_eq!(dota.last_session_secs, 200);

        // The exe carried forward is the one that played longest, not the one that played last.
        assert_eq!(dota.exe_name, "dota.exe");

        // Resolving or dismissing takes it out of the queue and leaves the other alone.
        assert!(ledger.dismiss_new_game(Some(&account), "570").unwrap());
        let games = ledger.new_games(Some(&account)).unwrap();
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].appid, "620");
        // Already gone, and unknown appids, are no-ops rather than errors.
        assert!(!ledger.dismiss_new_game(Some(&account), "570").unwrap());
        assert!(!ledger.dismiss_new_game(Some(&account), "nope").unwrap());
    }

    /// Adding a game to the library and binning it are different outcomes, and the row is kept
    /// precisely so they stay distinguishable. Both used to end up `dismissed`, which lost that.
    #[test]
    fn a_resolved_new_game_is_distinguishable_from_a_discarded_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = SessionLedger::open(&dir.path().join("l.sqlite")).unwrap();
        let account = session().account.unwrap();
        for appid in ["570", "620"] {
            ledger
                .record_new_game_session(Some(&account), appid, "G", "g.exe", 1.0, None, "2026-09-17".into(), 100)
                .unwrap();
        }
        let id_of = |l: &SessionLedger, appid: &str| {
            l.unsubmitted(Some(&account))
                .unwrap()
                .into_iter()
                .find(|s| matches!(&s.record.target, SessionTarget::LegacyNewGame(g) if g.appid == appid))
                .map(|s| s.record.id)
                .unwrap()
        };
        let resolved_id = id_of(&ledger, "570");
        let discarded_id = id_of(&ledger, "620");

        assert!(ledger.resolve_new_game(Some(&account), "570", "session:99").unwrap());
        assert!(ledger.dismiss_new_game(Some(&account), "620").unwrap());

        // Both leave the queue...
        assert!(ledger.new_games(Some(&account)).unwrap().is_empty());
        // ...but only the resolved one records what it became.
        let resolved = ledger.get(&resolved_id).unwrap().unwrap();
        assert_eq!(resolved.state, SubmissionState::Acknowledged);
        assert_eq!(resolved.remote_id.as_deref(), Some("session:99"));

        let discarded = ledger.get(&discarded_id).unwrap().unwrap();
        assert_eq!(discarded.state, SubmissionState::Dismissed);
        assert_eq!(discarded.remote_id, None);

        // A resolution with nothing to point at is refused rather than recorded as settled.
        ledger
            .record_new_game_session(Some(&account), "730", "G", "g.exe", 1.0, None, "2026-09-17".into(), 100)
            .unwrap();
        assert!(ledger.resolve_new_game(Some(&account), "730", "").is_err());
        assert_eq!(ledger.new_games(Some(&account)).unwrap().len(), 1);
    }

    /// Both queues live in `pending`, and reading the retry queue as "everything pending" put
    /// new-game entries into it — showing one game in both lists, and offering a Retry that would
    /// have posted against game 0, since a new game has no library id yet.
    #[test]
    fn the_retry_queue_and_the_new_games_queue_are_disjoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = SessionLedger::open(&dir.path().join("l.sqlite")).unwrap();
        let account = session().account.unwrap();

        // A real session awaiting retry...
        let played = session();
        ledger.insert(&played, SubmissionState::Active).unwrap();
        ledger.finish(&played.id, 200).unwrap();
        ledger.set_submission(&played.id, &submission()).unwrap();

        // ...and a game that is not in the library at all.
        ledger
            .record_new_game_session(Some(&account), "504230", "Celeste", "Celeste.exe", 0.5, None, "2026-09-17".into(), 100)
            .unwrap();

        let retryable = ledger.unsubmitted_sessions(Some(&account)).unwrap();
        let new_games = ledger.new_games(Some(&account)).unwrap();
        assert_eq!(retryable.len(), 1, "the new game leaked into the retry queue");
        assert_eq!(retryable[0].record.id, played.id);
        assert_eq!(new_games.len(), 1);
        assert_eq!(new_games[0].appid, "504230");
        // Together they account for everything outstanding, so nothing falls between them.
        assert_eq!(
            ledger.unsubmitted(Some(&account)).unwrap().len(),
            retryable.len() + new_games.len()
        );
    }

    /// Hotline Miami puts two executables through one appid: `HotlineMiami.exe` runs for about a
    /// second, then `HotlineGL.exe` runs the real session. `exe_name` is what a later resolve
    /// turns into a `ProcessMapping`, so taking whichever ran *last* could pin the mapping to the
    /// launcher — and every session after that would be the one second the launcher lives.
    #[test]
    fn a_multi_executable_game_keeps_the_executable_that_actually_played() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = SessionLedger::open(&dir.path().join("l.sqlite")).unwrap();
        let account = session().account.unwrap();
        let mut play = |exe: &str, hours: f64, now: u64| {
            ledger
                .record_new_game_session(
                    Some(&account), "219150", "Hotline Miami", exe, hours, None,
                    "2026-09-17".into(), now,
                )
                .unwrap()
        };

        play("HotlineMiami.exe", 0.01, 100); // the launcher
        play("HotlineGL.exe", 0.30, 200); // the game
        play("HotlineMiami.exe", 0.01, 300); // launcher again, most recent

        let game = &ledger.new_games(Some(&account)).unwrap()[0];
        assert_eq!(
            game.exe_name, "HotlineGL.exe",
            "pinned the launcher instead of the game"
        );
        // Every session still counts towards the total, whichever executable ran it.
        assert_eq!(game.session_count, 3);
        assert!((game.hours - 0.32).abs() < 1e-9);
    }

    /// The appid is not the whole key: two accounts can each be playing the same unlisted game,
    /// and merging one into the other would attribute someone else's hours.
    #[test]
    fn new_game_entries_do_not_merge_across_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = SessionLedger::open(&dir.path().join("l.sqlite")).unwrap();
        let first = session().account.unwrap();
        let mut second = first.clone();
        second.account_id = "someone-else".into();

        ledger
            .record_new_game_session(Some(&first), "570", "Dota", "dota.exe", 1.0, None, "2026-09-17".into(), 100)
            .unwrap();
        let other_total = ledger
            .record_new_game_session(Some(&second), "570", "Dota", "dota.exe", 5.0, None, "2026-09-17".into(), 100)
            .unwrap();

        assert_eq!(other_total, 5.0, "merged into the other account's entry");
        assert_eq!(ledger.new_games(Some(&first)).unwrap()[0].hours, 1.0);
        assert_eq!(ledger.new_games(Some(&second)).unwrap()[0].hours, 5.0);

        // Dismissing one account's entry leaves the other's standing.
        assert!(ledger.dismiss_new_game(Some(&first), "570").unwrap());
        assert!(ledger.new_games(Some(&first)).unwrap().is_empty());
        assert_eq!(ledger.new_games(Some(&second)).unwrap().len(), 1);
    }

    #[test]
    fn dismissing_retains_the_row_and_removes_it_from_every_queue() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = SessionLedger::open(&dir.path().join("ledger.sqlite")).unwrap();
        let record = session();
        ledger.insert(&record, SubmissionState::Pending).unwrap();
        assert!(ledger.dismiss(&record.id).unwrap());
        assert!(ledger.outstanding(record.account.as_ref()).unwrap().is_empty());
        assert_eq!(
            ledger.get(&record.id).unwrap().unwrap().state,
            SubmissionState::Dismissed
        );
        // Already dismissed, and unknown ids, are no-ops rather than errors.
        assert!(!ledger.dismiss(&record.id).unwrap());
        assert!(!ledger.dismiss("no-such-id").unwrap());
    }

    #[test]
    fn unavailable_or_future_storage_returns_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(SessionLedger::open(dir.path()).is_err());
        let path = dir.path().join("ledger.sqlite");
        let ledger = SessionLedger::open(&path).unwrap();
        ledger
            .connection
            .execute_batch("PRAGMA user_version=2")
            .unwrap();
        drop(ledger);
        assert!(SessionLedger::open(&path).is_err());
    }

    #[test]
    fn account_queues_are_isolated_and_write_failure_is_returned() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = SessionLedger::open(&dir.path().join("ledger.sqlite")).unwrap();
        let first = session();
        let mut second = session();
        second.account.as_mut().unwrap().account_id = "another-account".into();
        ledger.insert(&first, SubmissionState::Pending).unwrap();
        ledger.insert(&second, SubmissionState::Pending).unwrap();
        assert_eq!(ledger.outstanding(first.account.as_ref()).unwrap().len(), 1);
        assert_eq!(
            ledger.outstanding(second.account.as_ref()).unwrap()[0]
                .record
                .id,
            second.id
        );
        assert!(ledger.outstanding(None).unwrap().is_empty());
        assert!(ledger
            .acknowledge(&first.id, second.account.as_ref().unwrap(), "remote")
            .is_err());
        ledger
            .connection
            .execute_batch("PRAGMA query_only=ON")
            .unwrap();
        assert!(ledger.insert(&session(), SubmissionState::Pending).is_err());
        assert!(ledger
            .acknowledge(&first.id, first.account.as_ref().unwrap(), "remote")
            .is_err());
        assert_eq!(
            ledger.get(&first.id).unwrap().unwrap().state,
            SubmissionState::Pending
        );
    }
}
