mod app;
mod autostart;
mod frontend;
mod mappings_model;
mod notify;
mod resolve;
mod session_flow;
mod state;
mod tray;
mod views;

use state::AppState;

fn main() -> glib::ExitCode {
    env_logger::init();
    app::run(AppState::load())
}
