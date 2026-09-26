use crate::config::{AuthConfig, ProcessMapConfig};
use crate::library_match::LibraryIndex;
use crate::monitor::ActiveSession;
use crate::session_ledger::AccountIdentity;
use crate::session_store::{ActiveRecord, SessionStore};
use crate::steam::InstalledGame;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

pub const DEFAULT_API_URL: &str = "https://api.froglog.co.uk/api";

/// What `local_games::install_locations_fingerprint` returns: cheap to compute, so it gates the
/// full installed-games scan.
type InstallFingerprint = Vec<(PathBuf, Vec<String>)>;

/// Everything the tracking engine shares between its threads and its frontend. Cheap to clone:
/// every field is shared, and the process monitor's background threads need thread-safe access.
#[derive(Clone)]
pub struct EngineState {
    pub auth: Arc<RwLock<AuthConfig>>,
    // Read/increment only while holding `auth`. A new login invalidates old work even
    // when it is for the same username (or the account changes A -> B -> A).
    account_generation: Arc<AtomicU64>,
    pub process_map: Arc<RwLock<ProcessMapConfig>>,
    pub current_session: Arc<RwLock<Option<ActiveSession>>>,
    pub force_stopped_process: Arc<RwLock<Option<String>>>,
    pub shutdown: Arc<AtomicBool>,
    /// Installed games — Steam (manifest scan) + non-Steam (watched directories) — refreshed
    /// periodically in the background.
    pub installed_games: Arc<RwLock<Vec<InstalledGame>>>,
    /// Titles/appids already in the user's FrogLog library, refreshed periodically in the background.
    pub library_index: Arc<RwLock<LibraryIndex>>,
    /// The durable session ledger. Starts unavailable; the frontend opens it once it knows it is
    /// the only running instance (see `engine::open_store`).
    store: Arc<RwLock<SessionStore>>,
    /// Ledger id of the mapped session being tracked, tied to its process. Taken by whichever
    /// path ends it first.
    pub active_ledger_id: ActiveRecord,
    /// Ledger ids of running unmapped sessions, by appid.
    pub active_unmapped_ids: Arc<RwLock<HashMap<String, String>>>,
    /// What the last installed-games scan saw; `None` forces the next check to rescan.
    install_fingerprint: Arc<RwLock<Option<InstallFingerprint>>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(name: &str) -> AuthConfig {
        AuthConfig {
            base_url: Some(DEFAULT_API_URL.into()),
            token: Some(format!("test-token-{name}")),
            username: Some(name.into()),
        }
    }

    fn library(id: i32) -> LibraryIndex {
        let game = serde_json::from_value(serde_json::json!({
            "id": id, "title": format!("Game {id}"), "steam_app_id": id,
        })).unwrap();
        LibraryIndex::build(&[game], &[], &[])
    }

    fn installed() -> Vec<InstalledGame> {
        vec![InstalledGame {
            appid: "123".into(), name: "Test game".into(), install_dir: "/test/game".into(),
        }]
    }

    #[test]
    fn logout_clears_account_caches_and_rejects_old_work() {
        let state = EngineState::new(auth("alice"), ProcessMapConfig::default());
        let (_, generation) = state.account_snapshot().unwrap();
        assert!(state.publish_library(generation, library(1)));
        assert!(state.publish_installed_games(generation, installed()));

        state.apply_account(AuthConfig::default(), ProcessMapConfig::default());
        assert!(state.account_snapshot().is_none());
        assert!(!state.publish_library(generation, library(1)));
        assert!(!state.publish_installed_games(generation, installed()));
        assert!(state.library_index.read().unwrap().resolve_by_id(1).is_none());
        assert!(state.installed_games.read().unwrap().is_empty());
    }

    #[test]
    fn delayed_refresh_cannot_replace_the_next_accounts_library() {
        let state = EngineState::new(auth("alice"), ProcessMapConfig::default());
        let (_, old_generation) = state.account_snapshot().unwrap();
        let worker_state = state.clone();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            wait.recv().unwrap();
            worker_state.publish_library(old_generation, library(1))
        });

        state.apply_account(auth("bob"), ProcessMapConfig::default());
        let (_, new_generation) = state.account_snapshot().unwrap();
        assert!(state.publish_library(new_generation, library(2)));
        release.send(()).unwrap();
        assert!(!worker.join().unwrap());
        let index = state.library_index.read().unwrap();
        assert!(index.resolve_by_id(1).is_none());
        assert!(index.resolve_by_id(2).is_some());
    }

    #[test]
    fn returning_to_the_same_account_does_not_revalidate_old_work() {
        let state = EngineState::new(auth("alice"), ProcessMapConfig::default());
        let (_, old_generation) = state.account_snapshot().unwrap();
        state.apply_account(auth("bob"), ProcessMapConfig::default());
        state.apply_account(auth("alice"), ProcessMapConfig::default());
        assert!(!state.publish_library(old_generation, library(1)));
        assert!(!state.publish_installed_games(old_generation, installed()));
        let (_, current) = state.account_snapshot().unwrap();
        assert!(state.publish_library(current, library(3)));
    }

    #[test]
    fn relogin_invalidates_old_requests_even_with_identical_credentials() {
        let state = EngineState::new(auth("alice"), ProcessMapConfig::default());
        let (_, old_generation) = state.account_snapshot().unwrap();
        state.apply_account(auth("alice"), ProcessMapConfig::default());
        assert!(!state.publish_library(old_generation, library(1)));
    }

    #[test]
    fn switching_accounts_replaces_detection_preferences() {
        let old_map = ProcessMapConfig { share_now_playing: true, ..Default::default() };
        let new_map = ProcessMapConfig { disable_unmapped_game_detection: true, ..Default::default() };
        let state = EngineState::new(auth("alice"), old_map);
        state.apply_account(auth("bob"), new_map);
        let map = state.process_map.read().unwrap();
        assert!(!map.share_now_playing);
        assert!(map.disable_unmapped_game_detection);
    }
}

impl EngineState {
    pub fn new(auth: AuthConfig, process_map: ProcessMapConfig) -> Self {
        Self {
            auth: Arc::new(RwLock::new(auth)),
            account_generation: Arc::new(AtomicU64::new(0)),
            process_map: Arc::new(RwLock::new(process_map)),
            current_session: Arc::new(RwLock::new(None)),
            force_stopped_process: Arc::new(RwLock::new(None)),
            shutdown: Arc::new(AtomicBool::new(false)),
            installed_games: Arc::new(RwLock::new(Vec::new())),
            library_index: Arc::new(RwLock::new(LibraryIndex::default())),
            store: Arc::new(RwLock::new(SessionStore::unavailable(
                "Session storage has not been opened yet".into(),
                DEFAULT_API_URL,
            ))),
            active_ledger_id: ActiveRecord::default(),
            active_unmapped_ids: Arc::new(RwLock::new(HashMap::new())),
            install_fingerprint: Arc::new(RwLock::new(None)),
        }
    }

    /// The saved login and that account's settings, as every frontend starts up.
    pub fn load() -> Self {
        let auth = AuthConfig::load_from(&crate::config::auth_config_path());
        let process_map = ProcessMapConfig::load_from(&crate::config::process_map_path_for_auth(&auth));
        Self::new(auth, process_map)
    }

    pub fn logged_in(&self) -> bool {
        self.auth.read().unwrap().token.is_some()
    }

    /// Cheap to clone: every field is shared.
    pub fn store(&self) -> SessionStore {
        self.store.read().unwrap().clone()
    }

    pub fn set_store(&self, store: SessionStore) {
        *self.store.write().unwrap() = store;
    }

    /// Who owns work recorded right now, or `None` when logged out.
    pub fn account(&self) -> Option<AccountIdentity> {
        let auth = self.auth.read().unwrap().clone();
        self.store().account(&auth)
    }

    /// Credentials and the account they belong to from one snapshot, so a login change between
    /// the two reads cannot pair one account's token with another's queue.
    pub fn auth_and_account(&self) -> Option<(AuthConfig, AccountIdentity)> {
        let auth = self.auth.read().unwrap().clone();
        let account = self.store().account(&auth)?;
        Some((auth, account))
    }

    /// Consumes the force-stop block for `process_name` if one is set, returning whether it was.
    /// Whichever waiter sees the process really exit must clear it, or that game stays
    /// untrackable for the rest of the run.
    pub fn take_force_stop(&self, process_name: &str) -> bool {
        let mut flag = self.force_stopped_process.write().unwrap();
        if flag.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(process_name)) {
            *flag = None;
            true
        } else {
            false
        }
    }

    /// Persist first, so a failed save cannot report a successful login/logout.
    /// All credential changes must use this boundary to invalidate account caches.
    pub fn change_account(&self, auth: AuthConfig) -> Result<(), String> {
        let process_map = if auth.token.is_some() {
            ProcessMapConfig::load_from(&crate::config::process_map_path_for_auth(&auth))
        } else {
            ProcessMapConfig::default()
        };
        auth.save_to(&crate::config::auth_config_path())
            .map_err(|e| format!("Could not save account settings: {e}"))?;
        self.apply_account(auth, process_map);
        Ok(())
    }

    fn apply_account(&self, auth: AuthConfig, process_map: ProcessMapConfig) {
        let mut current = self.auth.write().unwrap();
        self.account_generation.fetch_add(1, Ordering::Relaxed);
        *self.process_map.write().unwrap() = process_map;
        *self.library_index.write().unwrap() = LibraryIndex::default();
        self.installed_games.write().unwrap().clear();
        // Watched directories/exclusions are per account; the next gated check must rescan.
        *self.install_fingerprint.write().unwrap() = None;
        *current = auth;
    }

    fn account_snapshot(&self) -> Option<(AuthConfig, u64)> {
        let auth = self.auth.read().unwrap();
        auth.token.as_ref()?;
        Some((auth.clone(), self.account_generation.load(Ordering::Relaxed)))
    }

    fn publish_library(&self, generation: u64, index: LibraryIndex) -> bool {
        let auth = self.auth.read().unwrap();
        if auth.token.is_none() || self.account_generation.load(Ordering::Relaxed) != generation {
            return false;
        }
        *self.library_index.write().unwrap() = index;
        true
    }

    fn publish_installed_games(&self, generation: u64, games: Vec<InstalledGame>) -> bool {
        let auth = self.auth.read().unwrap();
        if auth.token.is_none() || self.account_generation.load(Ordering::Relaxed) != generation {
            return false;
        }
        *self.installed_games.write().unwrap() = games;
        true
    }

    pub fn now_tracking_title(&self) -> Option<String> {
        self.current_session
            .read()
            .unwrap()
            .as_ref()
            .map(|s| s.mapping.title.clone().unwrap_or_else(|| s.process_name.clone()))
    }

    /// How long the current session has run, in seconds.
    pub fn now_tracking_secs(&self) -> Option<u64> {
        self.current_session.read().unwrap().as_ref().map(|s| s.elapsed_secs())
    }

    /// The current game with its length so far, e.g. "Hades (1h 23m)".
    pub fn now_tracking_label(&self) -> Option<String> {
        self.current_session.read().unwrap().as_ref().map(|s| s.label())
    }

    /// Re-scans Steam's installed games and every configured watched directory, replacing
    /// `installed_games` in one go. Called by the periodic background refresh and immediately
    /// after adding/removing a watched directory, so a newly added folder is picked up right
    /// away instead of waiting for the next scheduled scan.
    pub fn refresh_installed_games(&self) {
        let Some((_, generation)) = self.account_snapshot() else { return };
        let steam_root = crate::steam::find_steam_root();
        let (watched_dirs, excluded_appids): (Vec<String>, std::collections::HashSet<String>) = {
            let cfg = self.process_map.read().unwrap();
            (
                cfg.watched_directories.iter().map(|w| w.path.clone()).collect(),
                cfg.excluded_apps.iter().map(|e| e.appid.clone()).collect(),
            )
        };
        // Taken before the scan, not after: a game installed while the scan runs is then still a
        // difference on the next check rather than being credited to this scan and missed.
        let fingerprint = crate::local_games::install_locations_fingerprint(steam_root.as_deref(), &watched_dirs);
        let mut games = steam_root
            .as_deref()
            .map(crate::steam::scan_installed_games)
            .unwrap_or_default();
        games.extend(crate::local_games::scan_watched_directories(&watched_dirs));
        // User-excluded apps (see `ExcludedApp`) -- e.g. Wallpaper Engine, which manifests
        // exactly like a real Steam game but obviously isn't one. Filtered here rather than in
        // `scan_installed_games` itself so the scan stays a pure "what does Steam say is
        // installed" function; this is where per-user preference gets applied on top of it.
        games.retain(|g| !excluded_appids.contains(&g.appid));
        if self.publish_installed_games(generation, games) {
            *self.install_fingerprint.write().unwrap() = Some(fingerprint);
        }
    }

    /// Rescans installed games only when an install location has changed. The fingerprint reads
    /// directory listings, not manifests, so checking every few seconds is affordable -- which is
    /// what lets a game installed and launched straight away be matched rather than missed.
    pub fn refresh_installed_games_if_changed(&self) {
        let watched: Vec<String> = self
            .process_map
            .read()
            .unwrap()
            .watched_directories
            .iter()
            .map(|w| w.path.clone())
            .collect();
        let steam_root = crate::steam::find_steam_root();
        let fingerprint = crate::local_games::install_locations_fingerprint(steam_root.as_deref(), &watched);
        // Cloned out so the guard is dropped before `refresh_installed_games` takes the write lock.
        let previous = self.install_fingerprint.read().unwrap().clone();
        match previous {
            Some(prev) if prev == fingerprint => return,
            None => log::info!("[LilyPad] scanning installed games"),
            Some(_) => log::info!("[LilyPad] a game install location changed; rescanning installed games"),
        }
        self.refresh_installed_games();
    }

    /// Re-fetches games/wishlist/live-service from FrogLog and rebuilds `library_index`.
    /// Blocking (network calls) -- call from a background thread. Called by the periodic
    /// background refresh and immediately after resolving a "New Games" entry, so a game just
    /// created/mapped doesn't still look "not in my library" to the next detection attempt.
    /// Returns whether the library could actually be consulted, so a caller about to decide
    /// a game is unlisted can decline to guess when it could not.
    pub fn refresh_library_index(&self) -> bool {
        let Some((auth, generation)) = self.account_snapshot() else { return false };
        let client = super::flow::client_for(&auth);
        // Every fetch must succeed. `unwrap_or_default()` on a failed request built an *empty*
        // index and replaced a good one with it, at which point every game the user owns reads
        // as unknown and is filed as a New Game. A stale index is strictly better: it can only
        // miss games added since the last good refresh.
        let (games, wishlist, live_service) = match (
            client.get_games(),
            client.get_wishlist(),
            client.get_live_service_games(),
        ) {
            (Ok(games), Ok(wishlist), Ok(live_service)) => (games, wishlist, live_service),
            _ => {
                log::warn!(
                    "[LilyPad] could not refresh the library; keeping the previous copy rather \
                     than treating every owned game as new"
                );
                return false;
            }
        };
        self.publish_library(generation, LibraryIndex::build(&games, &wishlist, &live_service))
    }
}
