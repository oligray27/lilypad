use lilypad_core::config::{AuthConfig, ProcessMapConfig};
use lilypad_core::library_match::LibraryIndex;
use lilypad_core::monitor::ActiveSession;
use lilypad_core::steam::InstalledGame;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

pub const DEFAULT_API_URL: &str = "https://api.froglog.co.uk/api";

/// Shared app state. `Arc<RwLock<_>>` (not `Rc<RefCell<_>>`) because the process
/// monitor's background threads (in lilypad-core, unchanged from the Tauri build)
/// need thread-safe shared access to the process map / current session.
#[derive(Clone)]
pub struct AppState {
    pub auth: Arc<RwLock<AuthConfig>>,
    pub process_map: Arc<RwLock<ProcessMapConfig>>,
    pub current_session: Arc<RwLock<Option<ActiveSession>>>,
    pub force_stopped_process: Arc<RwLock<Option<String>>>,
    pub shutdown: Arc<AtomicBool>,
    /// Installed games — Steam (manifest scan) + non-Steam (watched directories) — refreshed
    /// periodically in the background.
    pub installed_games: Arc<RwLock<Vec<InstalledGame>>>,
    /// Titles/appids already in the user's FrogLog library, refreshed periodically in the background.
    pub library_index: Arc<RwLock<LibraryIndex>>,
}

impl AppState {
    pub fn new(auth: AuthConfig, process_map: ProcessMapConfig) -> Self {
        Self {
            auth: Arc::new(RwLock::new(auth)),
            process_map: Arc::new(RwLock::new(process_map)),
            current_session: Arc::new(RwLock::new(None)),
            force_stopped_process: Arc::new(RwLock::new(None)),
            shutdown: Arc::new(AtomicBool::new(false)),
            installed_games: Arc::new(RwLock::new(Vec::new())),
            library_index: Arc::new(RwLock::new(LibraryIndex::default())),
        }
    }

    pub fn logged_in(&self) -> bool {
        self.auth.read().unwrap().token.is_some()
    }

    pub fn now_tracking_title(&self) -> Option<String> {
        self.current_session
            .read()
            .unwrap()
            .as_ref()
            .map(|s| s.mapping.title.clone().unwrap_or_else(|| s.process_name.clone()))
    }

    /// Re-scans Steam's installed games and every configured watched directory, replacing
    /// `installed_games` in one go. Called both by the periodic background refresh in
    /// `app.rs` and immediately after adding/removing a watched directory (Non-Steam Games
    /// view), so a newly added folder is picked up right away instead of waiting for the
    /// next scheduled scan — mirrors the Tauri build's `refresh_installed_games`.
    pub fn refresh_installed_games(&self) {
        let mut games = lilypad_core::steam::find_steam_root()
            .map(|root| lilypad_core::steam::scan_installed_games(&root))
            .unwrap_or_default();
        let (watched_dirs, excluded_appids): (Vec<String>, std::collections::HashSet<String>) = {
            let cfg = self.process_map.read().unwrap();
            (
                cfg.watched_directories.iter().map(|w| w.path.clone()).collect(),
                cfg.excluded_apps.iter().map(|e| e.appid.clone()).collect(),
            )
        };
        games.extend(lilypad_core::local_games::scan_watched_directories(&watched_dirs));
        // User-excluded apps (see `ExcludedApp`) -- e.g. Wallpaper Engine, which manifests
        // exactly like a real Steam game but obviously isn't one. Filtered here rather than in
        // `scan_installed_games` itself so the scan stays a pure "what does Steam say is
        // installed" function; this is where per-user preference gets applied on top of it.
        games.retain(|g| !excluded_appids.contains(&g.appid));
        *self.installed_games.write().unwrap() = games;
    }

    /// Re-fetches games/wishlist/live-service from FrogLog and rebuilds `library_index`.
    /// Blocking (network calls) -- call from a background thread. Called both by the periodic
    /// background refresh in `app.rs` and immediately after resolving a "New Games" entry
    /// (`resolve.rs`), so a game just created/mapped doesn't still look "not in my library" to
    /// the next detection attempt for up to 5 minutes (the periodic refresh interval) --
    /// mirrors `refresh_installed_games`'s immediate-refresh-after-mutation pattern.
    pub fn refresh_library_index(&self) {
        let auth = self.auth.read().unwrap().clone();
        if auth.token.is_none() {
            return;
        }
        let client = crate::session_flow::client_for(&auth);
        let games = client.get_games().unwrap_or_default();
        let wishlist = client.get_wishlist().unwrap_or_default();
        let live_service = client.get_live_service_games().unwrap_or_default();
        *self.library_index.write().unwrap() =
            lilypad_core::library_match::LibraryIndex::build(&games, &wishlist, &live_service);
    }
}
