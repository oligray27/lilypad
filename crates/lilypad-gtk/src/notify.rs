//! Thin notify-rust wrapper. Errors are logged, never propagated — a failed
//! notification shouldn't interrupt tracking.

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
