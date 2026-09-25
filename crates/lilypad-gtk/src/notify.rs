//! Thin notify-rust wrapper. Errors are logged, never propagated — a failed
//! notification shouldn't interrupt tracking.

/// Native presentation adapter for the shared interception policy. Runs on a session worker,
/// never GTK's main thread. Async notification creation and action waiting are both cancelled
/// when the application-owned deadline resolves, so an unresponsive daemon cannot leak a
/// permanently blocked notification thread for every session.
///
/// The notification itself is shown for `PROMPT_DISPLAY`, not the whole window: KDE honours the
/// requested timeout literally, and a 25-second popup was intrusive. When it expires the daemon
/// reports it closed, which resolves the decision as "submit" just like a manual dismissal.
/// `INTERCEPT_WINDOW` remains the upper bound for a daemon that never reports back.
pub fn auto_submit_prompt(title: &str, time: &str) -> Result<lilypad_core::auto_submit::Outcome, String> {
    use lilypad_core::auto_submit::{self, INTERCEPT_WINDOW};
    const PROMPT_DISPLAY: std::time::Duration = std::time::Duration::from_secs(5);
    use notify_rust::NotificationResponse;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Could not start auto-submit timer: {e}"))?;
    Ok(runtime.block_on(async {
        let (add_tx, add_rx) = tokio::sync::oneshot::channel();
        let (dismiss_tx, dismiss_rx) = tokio::sync::oneshot::channel();
        let mut handle = None;
        let mut notification = notify_rust::Notification::new();
        notification.appname("LilyPad")
            .summary("Session Auto-Submitting")
            .body(&format!("{title} ({time})"))
            .icon(&icon_ref("uk.co.froglog.lilypad"))
            .action("add_notes", "Add Notes")
            .timeout(notify_rust::Timeout::Milliseconds(PROMPT_DISPLAY.as_millis() as u32));
        let presentation = async {
            match notification.show_async().await {
                Ok(shown) => {
                    handle = Some(shown);
                    handle.as_ref().unwrap().wait_for_action_async(move |response| {
                        match response {
                            NotificationResponse::Default => { let _ = add_tx.send(()); }
                            NotificationResponse::Action(action) if action == "add_notes" => {
                                let _ = add_tx.send(());
                            }
                            NotificationResponse::Closed(_) => { let _ = dismiss_tx.send(()); }
                            _ => {}
                        }
                    }).await;
                }
                Err(e) => log::warn!("[LilyPad] auto-submit notification failed: {e}"),
            }
        };
        let outcome = auto_submit::wait_with_presentation(add_rx, dismiss_rx, INTERCEPT_WINDOW, presentation).await;
        if let Some(handle) = handle {
            // Retire the old action without letting notification cleanup delay submission.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), handle.close_async()).await;
        }
        outcome
    }))
}

/// Resolves to an absolute path to the icon shipped alongside the running binary
/// (`usr/share/icons/hicolor/128x128/apps/<name>.png` next to `usr/bin/`) when one exists —
/// true for .deb/.rpm installs and for an AppImage's own AppDir — falling back to the bare
/// freedesktop icon name otherwise. The notification daemon resolves a bare name via the
/// *system* icon theme, which is empty for an AppImage that isn't integrated via appimaged/
/// AppImageLauncher, so a bare name silently shows a generic icon instead of LilyPad's; an
/// absolute path sidesteps that lookup entirely.
fn icon_ref(name: &str) -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
        .map(|bin_dir| bin_dir.join(format!("../share/icons/hicolor/128x128/apps/{name}.png")))
        .filter(|p| p.exists())
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string())
}

pub fn show(summary: &str, body: &str) {
    if let Err(e) = notify_rust::Notification::new()
        .summary(summary)
        .body(body)
        .appname("LilyPad")
        .icon(&icon_ref("uk.co.froglog.lilypad"))
        .show()
    {
        log::warn!("[LilyPad] notification failed: {e}");
    }
}

/// Shows a notification with a single action button, invoking `on_action` if the user clicks
/// it (or the notification body — most notification daemons treat that the same as the default
/// action). Runs on its own thread since `wait_for_action` blocks until the user interacts or
/// the notification closes — safe to call from the GTK main loop, `on_action` should just send
/// a message back to it rather than touch any GTK widget directly.
pub fn show_with_action(summary: &str, body: &str, action_label: &str, on_action: impl FnOnce() + Send + 'static) {
    let summary = summary.to_string();
    let body = body.to_string();
    let action_label = action_label.to_string();
    std::thread::spawn(move || {
        match notify_rust::Notification::new()
            .summary(&summary)
            .body(&body)
            .appname("LilyPad")
            .icon(&icon_ref("uk.co.froglog.lilypad"))
            .action("default", &action_label)
            .show()
        {
            Ok(handle) => {
                handle.wait_for_action(|action| {
                    if action != "__closed" {
                        on_action();
                    }
                });
            }
            Err(e) => log::warn!("[LilyPad] notification failed: {e}"),
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "run under a private dbus-run-session without a notification daemon"]
    fn no_notification_daemon_preserves_the_interception_window() {
        // Refuse an accidental run against the user's real notification service.
        assert!(notify_rust::get_server_information().is_err());
        let started = std::time::Instant::now();
        assert_eq!(super::auto_submit_prompt("Isolated LilyPad test", "0m").unwrap(),
            lilypad_core::auto_submit::Outcome::Submit);
        assert!(started.elapsed() >= lilypad_core::auto_submit::INTERCEPT_WINDOW);
    }
}
