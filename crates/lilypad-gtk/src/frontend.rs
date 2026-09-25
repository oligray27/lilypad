//! How the tracking engine talks to the desktop user: desktop notifications, the tray, and the
//! session window. Called from engine threads, so window work goes to the GTK main loop over
//! `app_tx`; notifications and tray refreshes are thread-safe.

use crate::notify;
use crate::session_flow::AppAction;
use crate::tray::RefreshTray;
use lilypad_core::auto_submit::Outcome;
use lilypad_core::engine::{Frontend, SessionEndedData};

pub struct GtkFrontend {
    pub refresh_tray: RefreshTray,
    pub app_tx: async_channel::Sender<AppAction>,
}

impl Frontend for GtkFrontend {
    fn notify(&self, summary: &str, body: &str) {
        notify::show(summary, body);
    }

    fn changed(&self) {
        (self.refresh_tray)();
    }

    fn needs_decision(&self, data: SessionEndedData) {
        let _ = self.app_tx.send_blocking(AppAction::ShowSessionPopup(data));
    }

    fn auto_submit_prompt(&self, title: &str, time: &str) -> Result<Outcome, String> {
        notify::auto_submit_prompt(title, time)
    }

    fn new_game_recorded(&self, title: &str, time: &str, is_replay: bool) {
        let body = if is_replay {
            format!("{title} ({time}) is marked as finished in FrogLog. Resolve?")
        } else {
            format!("{title} ({time}) isn't in your FrogLog yet.")
        };
        let app_tx = self.app_tx.clone();
        notify::show_with_action("Session Recorded", &body, "Go to New Games", move || {
            let _ = app_tx.send_blocking(AppAction::GoToNewGames);
        });
    }
}
