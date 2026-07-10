mod app;
mod autostart;
mod mappings_model;
mod monitor_glue;
mod notify;
mod persistence;
mod resolve;
mod session_flow;
mod state;
mod tray;
mod views;

use lilypad_core::config::{auth_config_path, process_map_path_for_auth, AuthConfig, ProcessMapConfig};
use state::AppState;

fn main() -> glib::ExitCode {
    env_logger::init();

    let auth = AuthConfig::load_from(&auth_config_path());
    let process_map = ProcessMapConfig::load_from(&process_map_path_for_auth(&auth));

    // Matches the Tauri build's behavior: autostart is enabled unconditionally on launch.
    autostart::enable();

    let state = AppState::new(auth, process_map);
    app::run(state)
}
