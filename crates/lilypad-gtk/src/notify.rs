//! Thin notify-rust wrapper. Errors are logged, never propagated — a failed
//! notification shouldn't interrupt tracking.

pub fn show(summary: &str, body: &str) {
    if let Err(e) = notify_rust::Notification::new()
        .summary(summary)
        .body(body)
        .appname("LilyPad")
        // Freedesktop icon name, matching the hicolor icon installed by packaging
        // (crates/lilypad-gtk/data + Cargo.toml deb/rpm assets) — notify-rust
        // doesn't infer this from appname, it has to be set explicitly.
        .icon("uk.co.froglog.lilypad")
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
            .icon("uk.co.froglog.lilypad")
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
