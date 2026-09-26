//! LilyPad's headless tracking engine, for Steam Gaming Mode (Steam Deck, Bazzite and other
//! gamescope sessions), where there is no desktop, tray or window. It runs the same engine as the
//! GTK desktop app (`lilypad_core::engine`) against the same data directory, and is driven by
//! the LilyPad Decky plugin over stdin/stdout (see `protocol`).
//!
//! Only one tracker runs at a time (`engine::lock`). While the desktop app holds the lock this
//! waits, answering only `status`; when the desktop app asks it to hand over, it exits with
//! `EXIT_YIELDED` and the plugin restarts it to wait again.

mod commands;
mod frontend;
mod protocol;

use commands::Engine;
use frontend::DeckyFrontend;
use lilypad_core::engine::lock::TrackerLock;
use lilypad_core::engine::{self, EngineState, FrontendRef};
use protocol::{Out, Request};
use serde_json::json;
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Exit code after handing tracking over to the desktop app. The plugin restarts the engine,
/// which then waits for the lock again.
const EXIT_YIELDED: i32 = 3;

fn main() {
    // Logs to stderr; stdout is the protocol.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("lilypad_core=info,lilypad_engine=info"))
        .target(env_logger::Target::Stderr)
        .init();

    let out = Out::new();
    let state = EngineState::load();
    let frontend = Arc::new(DeckyFrontend::new(out.clone()));
    let tracking = Arc::new(AtomicBool::new(false));
    let engine = Arc::new(Engine { state: state.clone(), frontend: frontend.clone(), tracking: tracking.clone() });

    // Requests are served from the start, even while waiting for the lock, so the panel can
    // say why nothing is being tracked.
    {
        let engine = engine.clone();
        let out = out.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Request>(&line) {
                    Ok(request) => {
                        let engine = engine.clone();
                        let out = out.clone();
                        std::thread::spawn(move || out.reply(request.id, engine.handle(&request.cmd, request.args)));
                    }
                    Err(e) => log::warn!("[LilyPad] ignoring malformed request: {e}"),
                }
            }
            // The plugin backend has gone. Anything in progress is durable in the store.
            log::info!("[LilyPad] stdin closed; exiting");
            std::process::exit(0);
        });
    }

    out.event("started", json!({ "version": env!("CARGO_PKG_VERSION") }));

    // Newer-release checks (lilypad_core::updates), started before the lock so the panel can
    // show an update even while the desktop app is the one tracking. The release's plugin zip
    // and its checksum are looked up here, once per check, so the panel can hand them straight
    // to Decky's installer (`status.update`). `notify` is true only the first time a release is
    // seen -- shared with the desktop app's record -- for the plugin's one-off toast.
    {
        let out = out.clone();
        lilypad_core::updates::spawn_checker(env!("CARGO_PKG_VERSION").to_string(), move |update, notify| {
            let package = lilypad_core::updates::fetch_decky_package(&update, env!("CARGO_PKG_VERSION"));
            let info = json!({
                "version": update.version,
                "url": update.page_url,
                "zip_url": package.as_ref().map(|p| p.url.clone()),
                "zip_sha256": package.and_then(|p| p.sha256),
            });
            *commands::UPDATE.lock().unwrap() = Some(info.clone());
            out.event("update_available", json!({ "update": info, "notify": notify }));
        });
    }

    let lock = wait_for_lock(&out);

    if let Some(error) = engine::open_store(&state) {
        out.event("notify", json!({ "summary": "LilyPad storage problem", "body": error }));
    }
    tracking.store(true, Ordering::SeqCst);
    let frontend_ref: FrontendRef = frontend;
    engine::start(&state, frontend_ref);
    log::info!("[LilyPad] tracking in Gaming Mode as {:?}", state.auth.read().unwrap().username);
    out.event("tracking", json!({ "logged_in": state.logged_in() }));

    // Hand over to the desktop app when it asks. Exiting (rather than stopping in place) is what
    // guarantees no monitor or waiter thread of ours outlives the hand-over; the in-progress
    // session is left active for the desktop app's recovery to resume.
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if lock.yield_requested() {
            log::info!("[LilyPad] the desktop app is starting; handing over tracking");
            out.event("yielding", json!({}));
            drop(lock);
            std::process::exit(EXIT_YIELDED);
        }
    }
}

/// Blocks until this is the only tracker. While the desktop app holds the lock, says so.
fn wait_for_lock(out: &Out) -> TrackerLock {
    let mut announced = false;
    loop {
        match TrackerLock::try_acquire() {
            Ok(Some(lock)) => return lock,
            Ok(None) => {
                if !announced {
                    log::info!("[LilyPad] the desktop app is tracking; waiting");
                    out.event("waiting", json!({ "reason": "desktop" }));
                    announced = true;
                }
            }
            Err(e) => log::warn!("[LilyPad] could not check the tracker lock: {e}"),
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}
