//! The end-of-session flow lives in `lilypad_core::engine::flow`, shared with the Gaming Mode
//! engine. This keeps the names the GTK views use, plus the requests that need the GTK main loop.

pub use lilypad_core::engine::flow::{client_for, format_duration, round_hours};
pub use lilypad_core::engine::SessionEndedData;

/// Window work requested from engine threads, performed on the GTK main loop.
pub enum AppAction {
    ShowSessionPopup(SessionEndedData),
    GoToNewGames,
}
