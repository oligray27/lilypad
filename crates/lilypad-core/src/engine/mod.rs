//! The Linux tracking engine: detection, durable recording, crash recovery, submission and New
//! Games, with no user interface of its own. The GTK desktop app and the headless Gaming Mode
//! engine (driven by the Decky plugin) both run exactly this; each supplies a `Frontend` for how
//! it tells the user things and asks for decisions.

pub mod actions;
pub mod flow;
pub mod glue;
pub mod lock;
pub mod state;

pub use flow::SessionEndedData;
pub use state::{EngineState, DEFAULT_API_URL};

use crate::auto_submit::Outcome;
use glue::MonitorEvent;
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// How often to check whether anything has been installed or removed. Cheap because the scan
/// itself is gated on install-location fingerprints; the library fetch stays on the slow cycle.
const INSTALL_SCAN_INTERVAL: Duration = Duration::from_secs(10);
const LIBRARY_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// What the engine needs from whatever is showing it to the user. Called from background
/// threads, so implementations must hand any UI work to their own UI thread.
pub trait Frontend: Send + Sync + 'static {
    /// Something the user should know about.
    fn notify(&self, summary: &str, body: &str);

    /// Tracking state or a queue changed; anything showing them should re-read.
    fn changed(&self);

    /// A mapped game started being tracked.
    fn session_started(&self, title: &str) {
        self.notify("Tracking Started", title);
    }

    /// A finished session needs the user to submit it (with notes) or not record it: auto-submit
    /// is off, it was force-stopped, or the user asked to add notes. Its record is already
    /// pending, so a frontend that cannot ask loses nothing by leaving it in the queue.
    fn needs_decision(&self, data: SessionEndedData);

    /// A live/session game is about to auto-submit: offer "Add Notes" and block until the user
    /// takes it or the interception window ends. See `auto_submit`.
    fn auto_submit_prompt(&self, title: &str, time: &str) -> Result<Outcome, String>;

    /// Submit every finished session as soon as it ends, with the Gaming Mode note, whatever
    /// the auto-submit settings say. For a frontend with nowhere to add notes (Gaming Mode).
    /// A force-stopped session is still left to the user.
    fn submits_every_session(&self) -> bool {
        false
    }

    /// An unlisted game's session went into New Games.
    fn new_game_recorded(&self, title: &str, time: &str, is_replay: bool) {
        let body = if is_replay {
            format!("{title} ({time}) is marked as finished in FrogLog. Resolve?")
        } else {
            format!("{title} ({time}) isn't in your FrogLog yet.")
        };
        self.notify("Session Recorded", &body);
    }
}

pub type FrontendRef = Arc<dyn Frontend>;

/// Opens the durable store (importing the pre-ledger JSON queues once) into `state`. Call only
/// from the single running instance, before `start`. Returns the error to show, if any.
pub fn open_store(state: &EngineState) -> Option<String> {
    let (store, startup_log) = crate::session_store::SessionStore::open(DEFAULT_API_URL);
    for (level, message) in startup_log {
        log::log!(level, "{message}");
    }
    let error = store.error();
    state.set_store(store);
    error
}

/// Starts tracking: recovers interrupted sessions, then runs the installed-games/library
/// refresh and the process monitor in the background. Returns immediately.
pub fn start(state: &EngineState, frontend: FrontendRef) {
    // Recover every session interrupted by a crash, restart or shutdown, before the monitor
    // starts polling (so a resumed game is already in current_session and is not tracked again
    // as new). Still running: resume its record. Gone: credit it up to its last checkpoint --
    // never "now", so downtime is not billed as play -- and run the normal end path.
    flow::recover_interrupted(state, &frontend);

    // Two refreshes on very different budgets. Installed games are local files and the scan is
    // gated on a directory fingerprint, so checking every few seconds costs a few `stat`s and a
    // newly installed game is matched within seconds. The library index is a network fetch, so
    // it stays on the slow cycle (the monitor also refreshes it before deciding a game is new).
    {
        let state = state.clone();
        std::thread::spawn(move || {
            let mut since_library_refresh = LIBRARY_REFRESH_INTERVAL;
            while !state.shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                state.refresh_installed_games_if_changed();
                if since_library_refresh >= LIBRARY_REFRESH_INTERVAL {
                    state.refresh_library_index();
                    since_library_refresh = Duration::ZERO;
                }
                std::thread::sleep(INSTALL_SCAN_INTERVAL);
                since_library_refresh += INSTALL_SCAN_INTERVAL;
            }
        });
    }

    let (tx, rx) = mpsc::channel::<MonitorEvent>();
    glue::start(state, tx);
    let state = state.clone();
    std::thread::spawn(move || {
        for event in rx {
            handle_event(&state, &frontend, event);
        }
    });
}

fn handle_event(state: &EngineState, frontend: &FrontendRef, event: MonitorEvent) {
    match event {
        MonitorEvent::SessionStarted { process_name, mapping, ledger_id } => {
            flow::handle_session_started(state, &process_name, &mapping, ledger_id, chrono::Utc::now().to_rfc3339(), true);
            frontend.session_started(&mapping.title.clone().unwrap_or(mapping.process.clone()));
            frontend.changed();
        }
        // Already completed durably by the glue, which also filtered out a force-stopped or
        // already-completed session.
        MonitorEvent::SessionEnded { process_name, mapping, duration_secs, ledger_id } => {
            flow::handle_session_ended(state.clone(), frontend.clone(), process_name, mapping, duration_secs, false, ledger_id);
        }
        MonitorEvent::UnmappedGameSessionEnded { title, duration_secs, is_replay, saved } => {
            let time = flow::format_duration(duration_secs);
            if saved {
                frontend.new_game_recorded(&title, &time, is_replay);
            } else {
                frontend.notify(
                    "Session Not Saved",
                    &format!(
                        "{title} ({time}) could not be recorded: {}",
                        state.store().error().unwrap_or_else(|| "not logged in".into())
                    ),
                );
            }
            frontend.changed();
        }
    }
}
