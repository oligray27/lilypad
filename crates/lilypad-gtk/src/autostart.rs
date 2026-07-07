//! Linux autostart via a `.desktop` file in `~/.config/autostart/`, replacing
//! tauri-plugin-autostart (which the Tauri/Windows build still uses).

use std::path::PathBuf;

const DESKTOP_FILE_NAME: &str = "uk.co.froglog.lilypad.desktop";

fn autostart_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("autostart")
}

fn desktop_file_path() -> PathBuf {
    autostart_dir().join(DESKTOP_FILE_NAME)
}

pub fn enable() {
    let dir = autostart_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("[LilyPad] autostart: could not create {dir:?}: {e}");
        return;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("lilypad-gtk"));
    let contents = format!(
        "[Desktop Entry]\nType=Application\nName=LilyPad\nComment=Froglog game time tracker\nExec={}\nIcon=uk.co.froglog.lilypad\nTerminal=false\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
        exe.display()
    );
    if let Err(e) = std::fs::write(desktop_file_path(), contents) {
        log::warn!("[LilyPad] autostart: could not write desktop file: {e}");
    }
}
