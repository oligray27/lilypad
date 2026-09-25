//! Linux autostart via a `.desktop` file in `~/.config/autostart/`, replacing
//! tauri-plugin-autostart (which the Tauri/Windows build still uses).

use std::path::{Path, PathBuf};

const DESKTOP_FILE_NAME: &str = "uk.co.froglog.lilypad.desktop";

fn autostart_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("autostart")
}

fn desktop_file_path() -> PathBuf {
    autostart_dir().join(DESKTOP_FILE_NAME)
}

/// What to launch at login. An AppImage runs from a temporary mount (`/tmp/.mount_*`) that is
/// gone after a reboot, so `current_exe()` would point autostart at nothing; the AppImage
/// runtime puts the real file's path in `$APPIMAGE`.
fn launch_path() -> Result<PathBuf, String> {
    if let Some(appimage) = std::env::var_os("APPIMAGE").map(PathBuf::from) {
        if appimage.is_file() {
            return Ok(appimage);
        }
    }
    std::env::current_exe().map_err(|e| format!("cannot determine LilyPad's own path: {e}"))
}

/// A path as a single argument of a desktop entry's `Exec` key.
///
/// Three layers of escaping, per the Desktop Entry spec: `%` introduces field codes, so a
/// literal one is `%%`; an argument containing reserved characters (spaces included) is
/// double-quoted, with `"`, `` ` ``, `$` and `\` backslash-escaped inside the quotes; and the
/// value as a whole is a desktop-entry string, where each backslash is itself written `\\`.
/// Unquoted, a path with a space (`~/My Apps/LilyPad.AppImage`) became two arguments.
fn exec_arg(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('%', "%%");
    const RESERVED: &[char] = &[
        ' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(', ')', '`',
    ];
    let arg = if raw.contains(RESERVED) {
        let mut quoted = String::from("\"");
        for c in raw.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                quoted.push('\\');
            }
            quoted.push(c);
        }
        quoted.push('"');
        quoted
    } else {
        raw
    };
    arg.replace('\\', "\\\\")
}

/// Registers LilyPad to start at login, pointing at the binary (or AppImage) running now.
pub fn enable() -> Result<(), String> {
    let dir = autostart_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let exe = launch_path()?;
    let contents = format!(
        "[Desktop Entry]\nType=Application\nName=LilyPad\nComment=Froglog game time tracker\nExec={}\nIcon=uk.co.froglog.lilypad\nTerminal=false\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
        exec_arg(&exe)
    );
    let path = desktop_file_path();
    std::fs::write(&path, contents).map_err(|e| format!("could not write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::exec_arg;
    use std::path::Path;

    #[test]
    fn a_plain_path_is_written_as_is() {
        assert_eq!(exec_arg(Path::new("/usr/bin/lilypad-gtk")), "/usr/bin/lilypad-gtk");
    }

    #[test]
    fn a_path_with_spaces_stays_one_argument() {
        assert_eq!(
            exec_arg(Path::new("/home/u/My Apps/LilyPad-x86_64.AppImage")),
            "\"/home/u/My Apps/LilyPad-x86_64.AppImage\""
        );
    }

    #[test]
    fn reserved_characters_are_escaped_at_every_layer() {
        // `$` is escaped inside the quotes (`\$`), and that backslash is then doubled for the
        // desktop-entry string layer; `%` becomes `%%` so it is not read as a field code.
        assert_eq!(exec_arg(Path::new("/opt/a$b%c")), "\"/opt/a\\\\$b%%c\"");
    }
}
