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
        let watched_dirs: Vec<String> = self
            .process_map
            .read()
            .unwrap()
            .watched_directories
            .iter()
            .map(|w| w.path.clone())
            .collect();
        games.extend(lilypad_core::local_games::scan_watched_directories(&watched_dirs));
        *self.installed_games.write().unwrap() = games;
    }
}
