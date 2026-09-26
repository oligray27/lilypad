//! Resolving a New Games entry (play recorded for a game not in the library) into a FrogLog
//! entry, shared by both desktop frontends. Blocking; call off any UI thread.
//!
//! Every step is safe to repeat, because a resolution can fail anywhere:
//!
//! - **Game creation is keyed.** `POST /games` carries `client_ref = newgame:<record id>`, so a
//!   retry after a lost response gets the game the first attempt created instead of a second one
//!   (on a server with `games.client_ref`; older servers ignore the field).
//! - **The destination is recorded before anything is uploaded.** Once a game has been created,
//!   or an existing one chosen, the entry remembers it. A retry continues against that game
//!   whatever the user picks next, so an entry's sessions can never be split across two games.
//! - **Uploads are keyed per entry.** Each session's `sync_ref` is `newgame:<record id>#<index>`.
//!   The record id is fixed for the entry's life and `sessions` is only appended to, so a retry
//!   replays the same keys and the server skips what it already has. Keying by appid instead (as
//!   before) made a *later* batch for the same game reuse the first batch's keys, and the server
//!   silently dropped its sessions as duplicates.
//! - **Settling is last.** Only a fully uploaded entry leaves the queue.

use crate::api::{AddSessionBody, FroglogClient, GameCreation};
use crate::config::{self, AuthConfig, PendingGameSubmission, ProcessMapConfig};
use crate::library_match::{is_finished_status, LibraryIndex};
use crate::session_ledger::{AccountIdentity, NewGameEntry};
use crate::session_store::SessionStore;
use crate::submission::SessionApi;
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};

/// The API calls a resolution makes, narrowed so failure paths can be tested without a server.
pub trait ResolveApi: SessionApi {
    fn fetch_game_details(&self, title: &str) -> Result<Value, String>;
    fn get_game_raw(&self, game_id: i32) -> Result<Value, String>;
    fn create_game_keyed(&self, payload: Value, client_ref: &str, confirm_new: bool) -> Result<GameCreation, String>;
    fn resume_finished_game(&self, game_id: i32) -> Result<(), String>;
    fn attach_steam_app_id_if_missing(&self, game_id: i32, appid: i64) -> Result<(), String>;
    fn fix_imported_status_if_needed(&self, game_id: i32, game_type: &str) -> Result<(), String>;
}

impl ResolveApi for FroglogClient {
    fn fetch_game_details(&self, title: &str) -> Result<Value, String> {
        FroglogClient::fetch_game_details(self, title)
    }
    fn get_game_raw(&self, game_id: i32) -> Result<Value, String> {
        FroglogClient::get_game_raw(self, game_id)
    }
    fn create_game_keyed(&self, payload: Value, client_ref: &str, confirm_new: bool) -> Result<GameCreation, String> {
        FroglogClient::create_game_keyed(self, payload, client_ref, confirm_new)
    }
    fn resume_finished_game(&self, game_id: i32) -> Result<(), String> {
        FroglogClient::resume_finished_game(self, game_id).map(|_| ())
    }
    fn attach_steam_app_id_if_missing(&self, game_id: i32, appid: i64) -> Result<(), String> {
        FroglogClient::attach_steam_app_id_if_missing(self, game_id, appid).map(|_| ())
    }
    fn fix_imported_status_if_needed(&self, game_id: i32, game_type: &str) -> Result<(), String> {
        FroglogClient::fix_imported_status_if_needed(self, game_id, game_type)
    }
}

/// What the user chose to do with an entry.
#[derive(Debug, Clone)]
pub enum Choice {
    /// Create a new library entry from the IGDB title the user confirmed.
    New { igdb_title: String },
    /// Create a new entry as a replay of the finished one this entry matched.
    Replay,
    /// Log against an entry the user already has.
    Existing { game_type: String, game_id: i32, title: String },
}

#[derive(Debug, Clone)]
pub struct Resolution {
    pub game_type: String,
    pub game_id: i32,
    /// The entry's own title, for the mapping and future notifications.
    pub title: String,
    /// The executable to link to the entry, empty for entries recorded before that existed.
    pub exe_name: String,
    pub sessions_logged: usize,
    /// The created game as the server returned it, when this resolution created one.
    pub created: Option<Value>,
}

/// The note on the sessions uploaded into a game a resolution has just created, from the desktop
/// apps and from Steam Gaming Mode (the Decky plugin) respectively.
pub const CREATED_NOTE: &str = "Session logged from LilyPad";
pub const CREATED_NOTE_STEAMOS: &str = "Session logged from LilyPad via SteamOS";

/// The note on sessions logged against a game the user already had.
const ATTACHED_NOTE: &str = "Logged from LilyPad's untracked-session detection";

/// Resolves the logged-in account's New Games entry for `appid`. `library` is a snapshot of the
/// cached library index, used to attach instead of creating a duplicate. `created_note` is
/// `CREATED_NOTE` or `CREATED_NOTE_STEAMOS`, for the sessions of a game this creates.
pub fn resolve(
    api: &impl ResolveApi,
    store: &SessionStore,
    account: &AccountIdentity,
    library: &LibraryIndex,
    appid: &str,
    choice: Choice,
    created_note: &str,
) -> Result<Resolution, String> {
    let entry = store
        .new_game_entry(account, appid)
        .ok_or_else(|| store.error().unwrap_or_else(|| "Session storage is unavailable".into()))?
        .ok_or("Pending submission not found")?;

    // An earlier attempt got as far as choosing or creating the game: finish that, whatever
    // was picked this time, rather than creating another or splitting the sessions.
    if let Some(target) = &entry.target {
        let (game_type, game_id) = parse_target(target)?;
        log::info!(
            "[LilyPad] resuming the resolution of {} (appid {appid}) into {target}",
            entry.game.title
        );
        let title = match &choice {
            Choice::Existing { title, .. } if !title.is_empty() => title.clone(),
            _ => entry.game.title.clone(),
        };
        return upload_and_settle(api, store, account, &entry, &game_type, game_id, title, None, None);
    }

    match choice {
        Choice::Existing { game_type, game_id, title } => {
            attach(api, store, account, &entry, &game_type, game_id, title)
        }
        Choice::New { igdb_title } => create_new(api, store, account, library, &entry, &igdb_title, created_note),
        Choice::Replay => create_replay(api, store, account, &entry, created_note),
    }
}

/// Links the resolved entry's executable to it, so future launches are tracked normally rather
/// than falling through detection again. Best-effort: the resolution itself already succeeded.
pub fn link_resolved(process_map: &Arc<RwLock<ProcessMapConfig>>, auth: &AuthConfig, resolution: &Resolution) {
    if resolution.exe_name.is_empty() {
        return;
    }
    if let Err(e) = config::link_process_mapping(
        process_map,
        auth,
        resolution.exe_name.clone(),
        resolution.game_type.clone(),
        resolution.game_id,
        Some(resolution.title.clone()),
    ) {
        log::warn!("[LilyPad] failed to link {} to its resolved game: {e}", resolution.exe_name);
    }
}

fn parse_target(target: &str) -> Result<(String, i32), String> {
    let (game_type, id) = target
        .split_once(':')
        .ok_or_else(|| format!("Unreadable resolution target {target:?}"))?;
    let id = id.parse().map_err(|_| format!("Unreadable resolution target {target:?}"))?;
    Ok((game_type.to_string(), id))
}

fn client_ref(entry: &NewGameEntry) -> String {
    format!("newgame:{}", entry.id)
}

/// Records where the entry is going before anything is uploaded to it.
fn record_target(store: &SessionStore, account: &AccountIdentity, entry: &NewGameEntry, game_type: &str, game_id: i32) -> Result<(), String> {
    let target = format!("{game_type}:{game_id}");
    match store.set_new_game_target(account, &entry.id, &target) {
        Some(true) => Ok(()),
        Some(false) => Err("This entry is no longer pending".into()),
        None => Err(format!(
            "Could not record the destination {target} before uploading ({}). Nothing was uploaded; retrying reuses the same game.",
            store.error().unwrap_or_else(|| "storage unavailable".into())
        )),
    }
}

/// Log against an entry the user already has. Anything mapped this way should end up
/// session-tracked, not "regular" (a single running total), matching the silent auto-link path.
fn attach(
    api: &impl ResolveApi,
    store: &SessionStore,
    account: &AccountIdentity,
    entry: &NewGameEntry,
    game_type: &str,
    game_id: i32,
    title: String,
) -> Result<Resolution, String> {
    let effective_type = if game_type.eq_ignore_ascii_case("live") {
        "live"
    } else {
        // All best-effort and idempotent: enable_session_tracking keeps pre-existing hours as a
        // "Pre-tracked hours" session; resuming a Completed/DNF game makes a fresh session read
        // as "In Progress"; a real Steam appid is attached if the entry lacks one.
        if let Err(e) = api.enable_session_tracking(game_id) {
            log::warn!("[LilyPad] failed to enable session tracking: {e}");
        }
        if let Err(e) = api.resume_finished_game(game_id) {
            log::warn!("[LilyPad] failed to resume finished game: {e}");
        }
        if let Ok(appid) = entry.game.appid.parse::<i64>() {
            if let Err(e) = api.attach_steam_app_id_if_missing(game_id, appid) {
                log::warn!("[LilyPad] failed to attach steam_app_id: {e}");
            }
        }
        "session"
    };
    record_target(store, account, entry, effective_type, game_id)?;
    upload_and_settle(api, store, account, entry, effective_type, game_id, title, None, None)
}

fn create_new(
    api: &impl ResolveApi,
    store: &SessionStore,
    account: &AccountIdentity,
    library: &LibraryIndex,
    entry: &NewGameEntry,
    igdb_title: &str,
    created_note: &str,
) -> Result<Resolution, String> {
    // If IGDB has no exact match for the confirmed title, fall back to that title itself --
    // never `entry.title`, which is only LilyPad's guess (for a non-Steam game, literally the
    // exe's folder name).
    let mut payload = api
        .fetch_game_details(igdb_title)
        .unwrap_or_else(|_| json!({ "title": igdb_title }));
    let obj = payload.as_object_mut().ok_or("Unexpected response shape")?;

    // The same game already in the library under another platform (matched by IGDB id, which
    // the appid-only detection cannot see) and not finished: attach rather than duplicate. A
    // finished match falls through to creation -- the genuine-replay case.
    if let Some(igdb_id) = obj.get("igdb_id").and_then(|v| v.as_i64()) {
        if let Some(resolved) = library.resolve_by_igdb_id(igdb_id) {
            if !is_finished_status(&resolved.status) {
                return attach(api, store, account, entry, &resolved.game_type.clone(), resolved.id, resolved.title.clone());
            }
        }
    }

    // /search/fetch names this field "dev_country"; POST /games expects "studio_country".
    if let Some(country) = obj.remove("dev_country") {
        obj.insert("studio_country".into(), country);
    }
    obj.insert("is_public".into(), json!(true));
    // Session-tracked, matching how LilyPad recorded this play (discrete sessions).
    obj.insert("session_tracking".into(), json!(true));
    obj.insert("sessions_public".into(), json!(true));
    if let Some(start) = local_date(entry.game.first_seen_secs) {
        obj.insert("start_date".into(), json!(start));
    }
    // Our own detected appid is known-accurate. A non-Steam detection (`local:<path>`) must
    // *clear* the Steam SKU `/search/fetch` found for the title, or a copy never launched
    // through Steam gains a Steam link and a permanently empty Trophies tab.
    if let Ok(appid) = entry.game.appid.parse::<i64>() {
        obj.insert("steam_app_id".into(), json!(appid));
    } else {
        obj.remove("steam_app_id");
        obj.insert("platform_chips".into(), json!(["PC (Non-Steam)"]));
        obj.insert("no_achievements".into(), json!(true));
    }

    match api.create_game_keyed(payload, &client_ref(entry), false)? {
        GameCreation::Created(created) => {
            let game_id = created_id(&created)?;
            let title = created["title"].as_str().map(str::to_string).unwrap_or_else(|| igdb_title.to_string());
            record_target(store, account, entry, "session", game_id)?;
            upload_and_settle(api, store, account, entry, "session", game_id, title, Some(created), Some(created_note))
        }
        // The server knows this is an unfinished playthrough the user already has (by IGDB id,
        // or a platform link already on that entry), which the local index had not caught.
        GameCreation::AlreadyOwned { game_id, title } => {
            log::info!("[LilyPad] {} is already in the library as #{game_id}; attaching instead of creating", entry.game.title);
            attach(api, store, account, entry, "session", game_id, title.unwrap_or_else(|| igdb_title.to_string()))
        }
    }
}

/// A fresh playthrough of a Completed/DNF entry: a new row built from the old one's details,
/// flagged as a replay explicitly rather than left to the server's title-based guess.
fn create_replay(
    api: &impl ResolveApi,
    store: &SessionStore,
    account: &AccountIdentity,
    entry: &NewGameEntry,
    created_note: &str,
) -> Result<Resolution, String> {
    let replay_of = entry.game.replay_of.clone().ok_or("Pending submission has no replay match")?;
    let mut payload = api.get_game_raw(replay_of.id)?;
    let obj = payload.as_object_mut().ok_or("Unexpected response shape")?;
    // Nothing identity- or progress-related carries over into a new playthrough.
    for field in ["id", "hours_played", "start_date", "end_date", "status", "status_override", "user_id", "created_at"] {
        obj.remove(field);
    }
    obj.insert("dnf".into(), json!(false));
    obj.insert("session_tracking".into(), json!(true));
    obj.insert("sessions_public".into(), json!(true));
    obj.insert("replay".into(), json!(true));
    if let Some(start) = local_date(entry.game.first_seen_secs) {
        obj.insert("start_date".into(), json!(start));
    }
    // confirm_new: the user has explicitly chosen a separate entry, so the server must not
    // stop to ask about an unfinished playthrough it finds.
    match api.create_game_keyed(payload, &client_ref(entry), true)? {
        GameCreation::Created(created) => {
            let game_id = created_id(&created)?;
            let title = created["title"].as_str().map(str::to_string).unwrap_or_else(|| replay_of.title.clone());
            record_target(store, account, entry, "session", game_id)?;
            upload_and_settle(api, store, account, entry, "session", game_id, title, Some(created), Some(created_note))
        }
        GameCreation::AlreadyOwned { game_id, title } => {
            attach(api, store, account, entry, "session", game_id, title.unwrap_or(replay_of.title))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn upload_and_settle(
    api: &impl ResolveApi,
    store: &SessionStore,
    account: &AccountIdentity,
    entry: &NewGameEntry,
    game_type: &str,
    game_id: i32,
    title: String,
    created: Option<Value>,
    // `Some(note)` when this resolution created the game.
    created_note: Option<&str>,
) -> Result<Resolution, String> {
    let live = game_type.eq_ignore_ascii_case("live");
    let created_here = created_note.is_some();
    let notes = created_note.unwrap_or(ATTACHED_NOTE);
    let logged = upload_sessions(&entry.game, &entry.id, |date, hours, sync_ref| {
        api.add_session(live, game_id, AddSessionBody {
            date: Some(date), hours: Some(hours), notes: Some(notes.to_string()),
            spoiler: false, is_public: true, sync_ref: Some(sync_ref),
        })
    })?;
    if !created_here {
        // An "Imported" entry that was never started becomes "In Progress" now that it has a
        // real session. Best-effort; no-op otherwise.
        if let Err(e) = api.fix_imported_status_if_needed(game_id, game_type) {
            log::warn!("[LilyPad] failed to fix imported status: {e}");
        }
    }
    let remote_id = format!("{game_type}:{game_id}");
    match store.resolve_new_game(account, &entry.game.appid, &remote_id) {
        Some(_) => log::info!(
            "[LilyPad] new game {} (appid {}) resolved as {remote_id}; {logged} session(s) logged",
            entry.game.title, entry.game.appid
        ),
        // Everything is on the server. The entry keeps its destination, so resolving again only
        // re-sends keys the server already has and then settles.
        None => return Err(format!(
            "Logged to FrogLog, but LilyPad could not mark this entry resolved ({}). Resolving it again is safe.",
            store.error().unwrap_or_else(|| "storage unavailable".into())
        )),
    }
    Ok(Resolution {
        game_type: game_type.to_string(),
        game_id,
        title,
        exe_name: entry.game.exe_name.clone(),
        sessions_logged: logged,
        created,
    })
}

/// Sends each recorded sitting as its own session with its own date, keyed
/// `newgame:<record id>#<index>`. An entry recorded before per-sitting tracking has only a total,
/// sent once, dated today.
fn upload_sessions(
    game: &PendingGameSubmission,
    record_id: &str,
    mut submit_one: impl FnMut(String, f64, String) -> Result<Value, String>,
) -> Result<usize, String> {
    let key = |index: usize| format!("newgame:{record_id}#{index}");
    if game.sessions.is_empty() {
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        submit_one(date, game.hours, key(0))?;
        return Ok(1);
    }
    let total = game.sessions.len();
    for (index, session) in game.sessions.iter().enumerate() {
        // Logged per session so a resolution that fails partway says how far it got.
        log::info!(
            "[LilyPad] logging session {}/{total} for {}: {}h on {}",
            index + 1, game.title, session.hours, session.date
        );
        submit_one(session.date.clone(), session.hours, key(index))?;
    }
    Ok(total)
}

fn created_id(created: &Value) -> Result<i32, String> {
    created["id"]
        .as_i64()
        .and_then(|id| i32::try_from(id).ok())
        .ok_or_else(|| "Created game missing id".to_string())
}

/// When LilyPad first saw the game played, not when the user got round to resolving it.
fn local_date(secs: u64) -> Option<String> {
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_ledger::SessionLedger;
    use std::cell::RefCell;
    use std::collections::HashMap;

    const SERVER: &str = "https://api.example.test/api";

    /// A server that behaves like FrogLog's idempotency contract: `client_ref` returns the game
    /// it already created, and a repeated `sync_ref` returns the existing session.
    #[derive(Default)]
    struct FakeServer {
        games_by_ref: RefCell<HashMap<String, i32>>,
        creates: RefCell<usize>,
        sessions: RefCell<HashMap<(i32, String), f64>>,
        /// Fail the Nth session upload (0-based, counted across calls) once.
        fail_upload_at: RefCell<Option<usize>>,
        uploads: RefCell<usize>,
        /// Create succeeds on the server but the response is lost, once.
        lose_create_response: RefCell<bool>,
        already_owned: Option<i32>,
        /// Every uploaded session's note, in order.
        notes: RefCell<Vec<Option<String>>>,
    }

    impl SessionApi for FakeServer {
        fn enable_session_tracking(&self, _: i32) -> Result<(), String> { Ok(()) }
        fn add_session(&self, _live: bool, game_id: i32, body: AddSessionBody) -> Result<Value, String> {
            let n = { let mut u = self.uploads.borrow_mut(); *u += 1; *u - 1 };
            if *self.fail_upload_at.borrow() == Some(n) {
                *self.fail_upload_at.borrow_mut() = None;
                return Err("error sending request".into());
            }
            self.notes.borrow_mut().push(body.notes.clone());
            self.sessions.borrow_mut().entry((game_id, body.sync_ref.unwrap())).or_insert(body.hours.unwrap());
            Ok(json!({ "id": n }))
        }
    }

    impl ResolveApi for FakeServer {
        fn fetch_game_details(&self, title: &str) -> Result<Value, String> { Ok(json!({ "title": title })) }
        fn get_game_raw(&self, id: i32) -> Result<Value, String> { Ok(json!({ "id": id, "title": "Old run" })) }
        fn create_game_keyed(&self, _payload: Value, client_ref: &str, _confirm_new: bool) -> Result<GameCreation, String> {
            if let Some(game_id) = self.already_owned {
                return Ok(GameCreation::AlreadyOwned { game_id, title: Some("Owned".into()) });
            }
            let id = *self.games_by_ref.borrow_mut().entry(client_ref.to_string()).or_insert_with(|| {
                *self.creates.borrow_mut() += 1;
                100 + *self.creates.borrow() as i32
            });
            if std::mem::take(&mut *self.lose_create_response.borrow_mut()) {
                return Err("error sending request for url (https://api.example.test/api/games)".into());
            }
            Ok(GameCreation::Created(json!({ "id": id, "title": "Created" })))
        }
        fn resume_finished_game(&self, _: i32) -> Result<(), String> { Ok(()) }
        fn attach_steam_app_id_if_missing(&self, _: i32, _: i64) -> Result<(), String> { Ok(()) }
        fn fix_imported_status_if_needed(&self, _: i32, _: &str) -> Result<(), String> { Ok(()) }
    }

    fn setup(sittings: usize) -> (SessionStore, AccountIdentity) {
        let store = SessionStore::from_ledger(SessionLedger::open(std::path::Path::new(":memory:")).unwrap(), SERVER);
        let account = AccountIdentity { server: SERVER.into(), account_id: "alice".into() };
        for _ in 0..sittings {
            store.record_new_game(&account, "620", "Portal 2", "portal2", 1.0, None).unwrap();
        }
        (store, account)
    }

    fn new_game() -> Choice {
        Choice::New { igdb_title: "Portal 2".into() }
    }

    #[test]
    fn a_lost_create_response_does_not_create_a_second_game() {
        let (store, account) = setup(2);
        let api = FakeServer { lose_create_response: RefCell::new(true), ..Default::default() };
        let library = LibraryIndex::default();
        assert!(resolve(&api, &store, &account, &library, "620", new_game(), CREATED_NOTE).is_err());
        let done = resolve(&api, &store, &account, &library, "620", new_game(), CREATED_NOTE).unwrap();
        assert_eq!(*api.creates.borrow(), 1, "the retry must get the game the first attempt created");
        assert_eq!(done.game_id, 101);
        assert_eq!(api.sessions.borrow().len(), 2);
        assert!(store.new_games(&account).unwrap().is_empty());
    }

    #[test]
    fn an_upload_failure_after_creation_resumes_against_the_same_game() {
        let (store, account) = setup(3);
        // Second session upload fails: the game exists and one session is on the server.
        let api = FakeServer { fail_upload_at: RefCell::new(Some(1)), ..Default::default() };
        let library = LibraryIndex::default();
        assert!(resolve(&api, &store, &account, &library, "620", new_game(), CREATED_NOTE).is_err());
        assert_eq!(store.new_game_entry(&account, "620").unwrap().unwrap().target.as_deref(), Some("session:101"));

        // Even choosing differently on the retry cannot split the entry across two games.
        let retry = Choice::Existing { game_type: "session".into(), game_id: 7, title: "Other".into() };
        let done = resolve(&api, &store, &account, &library, "620", retry, CREATED_NOTE).unwrap();
        assert_eq!(done.game_id, 101);
        assert_eq!(*api.creates.borrow(), 1);
        let sessions = api.sessions.borrow();
        assert_eq!(sessions.len(), 3, "each sitting exactly once, the already-uploaded one not twice");
        assert!(sessions.keys().all(|(game, _)| *game == 101));
    }

    #[test]
    fn a_later_batch_for_the_same_game_does_not_reuse_the_first_batchs_keys() {
        let (store, account) = setup(1);
        let api = FakeServer::default();
        let library = LibraryIndex::default();
        let existing = || Choice::Existing { game_type: "session".into(), game_id: 7, title: "Portal 2".into() };
        resolve(&api, &store, &account, &library, "620", existing(), CREATED_NOTE).unwrap();
        // Played again later, before a mapping existed: a new entry for the same appid.
        store.record_new_game(&account, "620", "Portal 2", "portal2", 2.0, None).unwrap();
        resolve(&api, &store, &account, &library, "620", existing(), CREATED_NOTE).unwrap();
        assert_eq!(api.sessions.borrow().len(), 2, "the second batch's session must not be dropped as a duplicate");
    }

    #[test]
    fn a_game_the_server_says_is_already_owned_is_attached_not_duplicated() {
        let (store, account) = setup(1);
        let api = FakeServer { already_owned: Some(55), ..Default::default() };
        let done = resolve(&api, &store, &account, &LibraryIndex::default(), "620", new_game(), CREATED_NOTE).unwrap();
        assert_eq!((done.game_type.as_str(), done.game_id), ("session", 55));
        assert!(done.created.is_none());
        assert_eq!(*api.creates.borrow(), 0);
        assert!(store.new_games(&account).unwrap().is_empty());
    }

    #[test]
    fn sessions_carry_the_callers_note_for_a_created_game_and_the_attach_note_otherwise() {
        let (store, account) = setup(1);
        let api = FakeServer::default();
        resolve(&api, &store, &account, &LibraryIndex::default(), "620", new_game(), CREATED_NOTE_STEAMOS).unwrap();
        store.record_new_game(&account, "620", "Portal 2", "portal2", 1.0, None).unwrap();
        let existing = Choice::Existing { game_type: "session".into(), game_id: 7, title: "Portal 2".into() };
        resolve(&api, &store, &account, &LibraryIndex::default(), "620", existing, CREATED_NOTE_STEAMOS).unwrap();
        assert_eq!(
            *api.notes.borrow(),
            vec![Some(CREATED_NOTE_STEAMOS.to_string()), Some(ATTACHED_NOTE.to_string())]
        );
    }

    #[test]
    fn another_accounts_entry_cannot_be_resolved() {
        let (store, _) = setup(1);
        let bob = AccountIdentity { server: SERVER.into(), account_id: "bob".into() };
        let api = FakeServer::default();
        assert!(resolve(&api, &store, &bob, &LibraryIndex::default(), "620", new_game(), CREATED_NOTE).is_err());
        assert_eq!(*api.creates.borrow(), 0);
    }
}
