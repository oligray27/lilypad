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
