//! Steam library discovery: locates installed games via Steam's own manifest files
//! (`libraryfolders.vdf` + `appmanifest_*.acf`) so process detection can recognize
//! "this is a game" from the exe's install directory, without exe-name/path heuristics.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One installed game LilyPad knows about, from any source. For Steam games (read from an
/// `appmanifest_<appid>.acf` file), `appid` is the real numeric Steam appid. For non-Steam
/// games (see `local_games.rs`), `appid` is a synthetic `local:<path>` identity instead — it's
/// just "the stable identity of this install," not always a literal Steam appid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledGame {
    pub appid: String,
    pub name: String,
    /// The game's install directory — a running process's exe path is matched against this via
    /// `starts_with`.
    pub install_dir: PathBuf,
}

/// A parsed VDF/KeyValues block: leaf string values plus nested child blocks.
/// Handles the shallow, predictable subset of the format used by Steam's own manifest
/// files (quoted string keys/values, brace-nested blocks) — not a general VDF parser.
#[derive(Debug, Default)]
struct VdfNode {
    values: HashMap<String, String>,
    children: HashMap<String, VdfNode>,
}

fn parse_vdf(input: &str) -> VdfNode {
    let mut chars = input.chars().peekable();
    parse_vdf_block(&mut chars)
}

fn skip_ws_and_comments(chars: &mut std::iter::Peekable<std::str::Chars>) {
    loop {
        while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
            chars.next();
        }
        if chars.peek() == Some(&'/') {
            let mut lookahead = chars.clone();
            lookahead.next();
            if lookahead.peek() == Some(&'/') {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                continue;
            }
        }
        break;
    }
}

fn parse_quoted_string(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
    if chars.peek() != Some(&'"') {
        return None;
    }
    chars.next();
    let mut s = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(s),
            '\\' => {
                if let Some(next) = chars.next() {
                    s.push(next);
                }
            }
            _ => s.push(c),
        }
    }
    Some(s)
}

fn parse_vdf_block(chars: &mut std::iter::Peekable<std::str::Chars>) -> VdfNode {
    let mut node = VdfNode::default();
    loop {
        skip_ws_and_comments(chars);
        match chars.peek() {
            None => break,
            Some('}') => {
                chars.next();
                break;
            }
            Some('"') => {
                let Some(key) = parse_quoted_string(chars) else { break };
                skip_ws_and_comments(chars);
                match chars.peek() {
                    Some('"') => {
                        let value = parse_quoted_string(chars).unwrap_or_default();
                        node.values.insert(key, value);
                    }
                    Some('{') => {
                        chars.next();
                        let child = parse_vdf_block(chars);
                        node.children.insert(key, child);
                    }
                    _ => {}
                }
            }
            Some(_) => {
                chars.next();
            }
        }
    }
    node
}

/// Steam's per-user registry key on Windows, and the conventional default install locations
/// on both platforms if that lookup fails or isn't available (Linux has no registry).
#[cfg(windows)]
pub fn find_steam_root() -> Option<PathBuf> {
    if let Some(path) = registry_steam_path() {
        if path.exists() {
            return Some(path);
        }
    }
    for candidate in ["C:\\Program Files (x86)\\Steam", "C:\\Program Files\\Steam"] {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

#[cfg(windows)]
fn registry_steam_path() -> Option<PathBuf> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ, REG_VALUE_TYPE,
    };

    unsafe {
        let subkey: Vec<u16> = "Software\\Valve\\Steam\0".encode_utf16().collect();
        let mut hkey = Default::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(subkey.as_ptr()), 0, KEY_READ, &mut hkey).is_err() {
            return None;
        }
        let value_name: Vec<u16> = "SteamPath\0".encode_utf16().collect();
        let mut buf = [0u16; 512];
        let mut buf_len = (buf.len() * 2) as u32;
        let mut value_type = REG_VALUE_TYPE::default();
        let result = RegQueryValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            Some(&mut value_type),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut buf_len),
        );
        let _ = RegCloseKey(hkey);
        if result.is_err() {
            return None;
        }
        let chars = (buf_len as usize / 2).min(buf.len());
        // Value is NUL-terminated; drop trailing/embedded NULs before building the string.
        let end = buf[..chars].iter().position(|&c| c == 0).unwrap_or(chars);
        let s = String::from_utf16_lossy(&buf[..end]);
        if s.is_empty() {
            return None;
        }
        Some(PathBuf::from(s.replace('/', "\\")))
    }
}

#[cfg(target_os = "linux")]
pub fn find_steam_root() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    for candidate in [
        home.join(".steam/steam"),
        home.join(".local/share/Steam"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
    ] {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(all(not(windows), not(target_os = "linux")))]
pub fn find_steam_root() -> Option<PathBuf> {
    None
}

/// Every Steam library folder (the main install plus any additional drives added via
/// Steam's "Storage" settings), parsed from `steamapps/libraryfolders.vdf`.
pub fn list_library_folders(steam_root: &Path) -> Vec<PathBuf> {
    let vdf_path = steam_root.join("steamapps").join("libraryfolders.vdf");
    let Ok(contents) = std::fs::read_to_string(&vdf_path) else {
        return vec![steam_root.to_path_buf()];
    };
    let root = parse_vdf(&contents);
    let Some(libraryfolders) = root.children.get("libraryfolders") else {
        return vec![steam_root.to_path_buf()];
    };
    // `parse_quoted_string` already un-escapes `\\` -> `\`, so `path` here is a real,
    // ready-to-use filesystem path.
    let mut folders: Vec<PathBuf> = libraryfolders
        .children
        .values()
        .filter_map(|entry| entry.values.get("path"))
        .map(PathBuf::from)
        .collect();
    if folders.is_empty() {
        folders.push(steam_root.to_path_buf());
    }
    folders
}

/// Every installed game across every library folder, read from each library's
/// `steamapps/appmanifest_*.acf` files.
pub fn scan_installed_games(steam_root: &Path) -> Vec<InstalledGame> {
    let mut games = Vec::new();
    for library in list_library_folders(steam_root) {
        let steamapps_dir = library.join("steamapps");
        let Ok(entries) = std::fs::read_dir(&steamapps_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_manifest = path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("acf"))
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("appmanifest_"));
            if !is_manifest {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            let manifest = parse_vdf(&contents);
            let Some(app_state) = manifest.children.get("AppState") else {
                continue;
            };
            let (Some(appid), Some(name), Some(installdir)) = (
                app_state.values.get("appid"),
                app_state.values.get("name"),
                app_state.values.get("installdir"),
            ) else {
                continue;
            };
            if is_known_non_game(appid, name) {
                continue;
            }
            games.push(InstalledGame {
                appid: appid.clone(),
                name: name.clone(),
                install_dir: steamapps_dir.join("common").join(installdir),
            });
        }
    }
    games
}

/// Filters out entries Steam installs and manifests the exact same way as a real game, but
/// that should never surface as "a game was launched" — shared runtimes and compatibility
/// tools (Steamworks redistributables, Proton, the Steam Linux Runtime containers, etc.).
fn is_known_non_game(appid: &str, name: &str) -> bool {
    const NON_GAME_APPIDS: &[&str] = &[
        "228980", // Steamworks Common Redistributables
    ];
    if NON_GAME_APPIDS.contains(&appid) {
        return true;
    }
    let lower = name.to_lowercase();
    lower.contains("steamworks common redistributables")
        || lower.contains("steam linux runtime")
        || lower.contains("steam controller configs")
        || lower == "proton"
        || lower.starts_with("proton ")
}

/// Finds the installed game (Steam or local) whose install directory contains the given exe
/// path. Case-insensitive on Windows: `Path::starts_with` does an exact, case-sensitive
/// component comparison on every platform, but NTFS is case-preserving *without* being
/// case-sensitive — a shortcut/launcher can report the exe's path with different casing than
/// whatever `read_dir` returned when the install list was built, and a naive `starts_with`
/// would then silently fail to match despite pointing at the same real file. Kept
/// case-sensitive on Linux, where that assumption doesn't hold.
pub fn find_installed_game_for_exe<'a>(
    exe_path: &Path,
    games: &'a [InstalledGame],
) -> Option<&'a InstalledGame> {
    games.iter().find(|g| path_starts_with(exe_path, &g.install_dir))
}

#[cfg(windows)]
fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    let path_lower = path.to_string_lossy().to_lowercase();
    let prefix_lower = prefix.to_string_lossy().to_lowercase();
    Path::new(&path_lower).starts_with(Path::new(&prefix_lower))
}

#[cfg(not(windows))]
fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    path.starts_with(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_acf_manifest() {
        let acf = r#"
"AppState"
{
    "appid"		"620"
    "name"		"Portal 2"
    "installdir"		"Portal 2"
}
"#;
        let node = parse_vdf(acf);
        let app_state = node.children.get("AppState").unwrap();
        assert_eq!(app_state.values.get("appid").unwrap(), "620");
        assert_eq!(app_state.values.get("name").unwrap(), "Portal 2");
        assert_eq!(app_state.values.get("installdir").unwrap(), "Portal 2");
    }

    #[test]
    fn parses_libraryfolders_vdf() {
        let vdf = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
	}
}
"#;
        let node = parse_vdf(vdf);
        let libs = node.children.get("libraryfolders").unwrap();
        assert_eq!(libs.children.len(), 2);
        // parse_quoted_string un-escapes `\\` -> `\`, giving back the real path.
        assert_eq!(
            libs.children.get("0").unwrap().values.get("path").unwrap(),
            "C:\\Program Files (x86)\\Steam"
        );
    }

    #[test]
    fn matches_exe_under_install_dir() {
        let games = vec![InstalledGame {
            appid: "620".to_string(),
            name: "Portal 2".to_string(),
            install_dir: PathBuf::from("C:\\Steam\\steamapps\\common\\Portal 2"),
        }];
        let exe = PathBuf::from("C:\\Steam\\steamapps\\common\\Portal 2\\portal2.exe");
        let found = find_installed_game_for_exe(&exe, &games);
        assert_eq!(found.unwrap().appid, "620");
    }

    #[test]
    #[cfg(windows)]
    fn matches_exe_regardless_of_casing_on_windows() {
        let games = vec![InstalledGame {
            appid: "local:D:\\7S Games\\KILL KNIGHT-DenuvOwO [PORTABLE]".to_string(),
            name: "KILL KNIGHT-DenuvOwO [PORTABLE]".to_string(),
            install_dir: PathBuf::from("D:\\7S Games\\KILL KNIGHT-DenuvOwO [PORTABLE]"),
        }];
        // A shortcut/launcher can report the exe with different casing than what read_dir
        // returned when the install list was built (NTFS is case-preserving, not case-sensitive).
        let exe = PathBuf::from("d:\\7s games\\kill knight-denuvowo [portable]\\KILL KNIGHT.exe");
        let found = find_installed_game_for_exe(&exe, &games);
        assert!(found.is_some());
    }

    #[test]
    fn filters_known_non_game_entries() {
        assert!(is_known_non_game("228980", "Steamworks Common Redistributables"));
        assert!(is_known_non_game("1391110", "Steam Linux Runtime 1.0"));
        assert!(is_known_non_game("1493710", "Steam Linux Runtime - Soldier"));
        assert!(is_known_non_game("9999999", "Proton 7.0"));
        assert!(is_known_non_game("9999998", "Proton Experimental"));
        assert!(!is_known_non_game("620", "Portal 2"));
        assert!(!is_known_non_game("1145360", "Hades"));
    }
}
