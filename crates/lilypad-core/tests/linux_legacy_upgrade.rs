//! Synthetic upgrade fixtures matching the v0.5.5 JSON schema. Never reads app data.
use lilypad_core::session_ledger::{AccountIdentity, SessionLedger, LEGACY_FILES};
use std::path::Path;

const FILES: [(&str, &str); 3] = [
    (
        "active-session.json",
        include_str!("fixtures/linux-legacy/active-session.json"),
    ),
    (
        "pending-sessions.json",
        include_str!("fixtures/linux-legacy/pending-sessions.json"),
    ),
    (
        "pending-game-submissions.json",
        include_str!("fixtures/linux-legacy/pending-game-submissions.json"),
    ),
];

fn seed(path: &Path) {
    for (name, text) in FILES {
        std::fs::write(path.join(name), text).unwrap();
    }
}

fn account(id: &str) -> AccountIdentity {
    AccountIdentity {
        server: "https://fixture.invalid/api".into(),
        account_id: id.into(),
    }
}

#[test]
fn linux_upgrade_preserves_history_and_requires_explicit_ownership() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = dir.path().join("sessions.sqlite");
    let mut ledger = SessionLedger::open(&db).unwrap();
    assert_eq!(ledger.import_legacy(dir.path(), &LEGACY_FILES).unwrap(), 5);
    let owner = account("alice");
    let other = account("bob");
    assert!(ledger.outstanding(Some(&owner)).unwrap().is_empty());

    let active = ledger.interrupted(None).unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].record.process.executable, "portal2_linux");
    assert_eq!(
        active[0].record.last_alive_secs.unwrap() - active[0].record.started_at_secs.unwrap(),
        600
    );
    let pending = ledger.unsubmitted_sessions(None).unwrap();
    assert_eq!(pending.len(), 1);
    let payload = pending[0].record.submission.as_ref().unwrap();
    assert_eq!(
        payload.notes.as_deref(),
        Some("Keep this private note — café")
    );
    assert!(payload.spoiler);
    assert!(!payload.is_public);
    assert_eq!(payload.hours, 1.25);
    assert_eq!(payload.date, "2026-08-20");

    let games = ledger.new_games(None).unwrap();
    let accumulated = games.iter().find(|g| g.appid == "fixture-new").unwrap();
    assert_eq!(accumulated.sessions.len(), 2);
    assert_eq!(accumulated.sessions[0].date, "2026-08-20");
    assert_eq!(accumulated.sessions[1].date, "2026-08-21");
    assert_eq!(accumulated.hours, 1.75);
    assert_eq!(accumulated.exe_name, "Game With Spaces.exe");
    let replay = games.iter().find(|g| g.appid == "fixture-replay").unwrap();
    assert_eq!(replay.replay_of.as_ref().unwrap().id, 103);
    let aggregate = games
        .iter()
        .find(|g| g.appid == "fixture-aggregate")
        .unwrap();
    assert_eq!(aggregate.hours, 4.5);
    assert_eq!(aggregate.session_count, 3);
    assert!(aggregate.sessions.is_empty(), "must not invent play dates");

    for entry in ledger.outstanding(None).unwrap() {
        assert!(ledger.adopt(&entry.record.id, &owner).unwrap());
        assert!(ledger.adopt(&entry.record.id, &other).is_err());
    }
    drop(ledger);
    let mut ledger = SessionLedger::open(&db).unwrap();
    assert_eq!(ledger.import_legacy(dir.path(), &LEGACY_FILES).unwrap(), 0);
    assert_eq!(ledger.outstanding(Some(&owner)).unwrap().len(), 5);
    assert!(ledger.outstanding(Some(&other)).unwrap().is_empty());
    assert!(ledger.outstanding(None).unwrap().is_empty());
    for (name, original) in FILES {
        assert_eq!(
            std::fs::read_to_string(dir.path().join(name)).unwrap(),
            original
        );
    }
}

#[test]
fn malformed_final_file_rolls_back_the_entire_linux_upgrade_and_can_be_retried() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    std::fs::write(dir.path().join(FILES[2].0), "[{broken").unwrap();
    let db = dir.path().join("sessions.sqlite");
    let mut ledger = SessionLedger::open(&db).unwrap();
    assert!(ledger.import_legacy(dir.path(), &LEGACY_FILES).is_err());
    assert!(ledger.outstanding(None).unwrap().is_empty());
    drop(ledger);
    std::fs::write(dir.path().join(FILES[2].0), FILES[2].1).unwrap();
    let mut ledger = SessionLedger::open(&db).unwrap();
    assert_eq!(ledger.import_legacy(dir.path(), &LEGACY_FILES).unwrap(), 5);
}
