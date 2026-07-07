use lilypad_core::config::{AuthConfig, ProcessMapConfig};
use lilypad_core::monitor::ActiveSession;
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
}

impl AppState {
    pub fn new(auth: AuthConfig, process_map: ProcessMapConfig) -> Self {
        Self {
            auth: Arc::new(RwLock::new(auth)),
            process_map: Arc::new(RwLock::new(process_map)),
            current_session: Arc::new(RwLock::new(None)),
            force_stopped_process: Arc::new(RwLock::new(None)),
            shutdown: Arc::new(AtomicBool::new(false)),
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
}
