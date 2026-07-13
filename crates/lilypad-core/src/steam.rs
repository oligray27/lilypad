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

/// Finds the installed game a running process belongs to, trying the resolved exe path first
/// and falling back to parsing a Proton/Wine command line on Linux.
///
/// Steam launches a Proton game as `proton waitforexitandrun <path>` -- confirmed against a real
/// running session, that `<path>` is a plain native Linux path (e.g.
/// `/home/user/.../steamapps/common/Among Us/Among Us.exe`), not a Windows-style `Z:\...` path as
/// might be assumed. The process actually carrying this command line isn't the game process
/// itself (Wine renames *that* process's comm to the target .exe, deep inside the Wine process
/// tree, with no useful cmdline of its own) -- it's `proton`'s own "waitforexitandrun" wrapper
/// (and the Steam launch/sandbox wrappers around it), which conveniently blocks for the entire
/// game session, making it a good process to track for exit detection too. The resolved
/// `/proc/pid/exe` for any of these wrapper processes is nowhere near the game's install
/// directory, so plain `find_installed_game_for_exe` alone can never match a Proton game (this
/// was the actual bug behind "New Games" detection silently never firing for Proton titles, even
/// though exe-name-based `ProcessMapping` matching had already been made Proton-aware
/// separately). A `Z:\...` Windows-style path is also accepted as a fallback, in case some other
/// launch method (a non-Steam Wine invocation) does pass one -- Proton/Wine's default prefix
/// maps drive `Z:` directly onto the Unix filesystem root.
///
/// Returns the matched game together with whichever path actually matched it -- for the Proton
/// fallback that's the *translated* path (e.g. `.../Portal 2/portal2.exe`), not the resolved
/// `exe_path` (Wine's own binary, e.g. `wine64`). Callers must derive the exe name to record
/// from this returned path, never from `exe_path` directly -- every Proton game shares the same
/// Wine/Proton binary, so naively recording "wine64" as the matched exe would misattribute every
/// future Proton launch (of any game) to whichever one happened to resolve first.
pub fn find_installed_game_for_exe_or_cmd<'a>(
    exe_path: Option<&Path>,
    cmd: &[std::ffi::OsString],
    games: &'a [InstalledGame],
) -> Option<(&'a InstalledGame, PathBuf)> {
    if let Some(path) = exe_path {
        if let Some(found) = find_installed_game_for_exe(path, games) {
            return Some((found, path.to_path_buf()));
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(proton_path) = find_proton_exe_path(cmd) {
            if let Some(found) = find_installed_game_for_exe(&proton_path, games) {
                return Some((found, proton_path));
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cmd;
    }
    None
}

/// Finds the `<exe>` argument of a genuine `proton waitforexitandrun <exe>` invocation in a
/// process's own argv, in whichever path form the launcher happened to pass it.
///
/// `proton` must appear within the first few positions of *this exact process's* own argv (its
/// own command, give or take a `python3` interpreter prefix), immediately followed by
/// `waitforexitandrun` -- not just found anywhere in the array. Confirmed against a real
/// captured Among Us launch: every wrapper in Steam's launch chain (`reaper`, pressure-vessel's
/// `srt-bwrap`/`pv-adverb`, the SteamLinuxRuntime entry-point) inherits the *entire* downstream
/// command as trailing arguments of its own, so all of them carry the identical trailing
/// `.../proton waitforexitandrun .../Among Us.exe` sequence too -- just buried increasingly deep
/// behind each wrapper's own flags (`proton` sat at argv index 7 for `reaper`, deeper still for
/// `srt-bwrap`/`pv-adverb`, versus index 0 or 1 for the actual `proton`/`python3 proton`
/// process). A naive "does this cmd contain that sequence anywhere" scan matched all of them
/// equally, and which one the poll loop happened to examine first (arbitrary `HashMap`
/// iteration order) ended up as the PID tracked for exit detection -- those wrapper layers don't
/// reliably share the game's exact lifetime the way the real `proton waitforexitandrun` process
/// is specifically designed to (see `find_installed_game_for_exe_or_cmd`), so which one won was
/// producing inconsistent, unreliable session tracking across launches.
///
/// The path itself is usually a plain native Linux path (confirmed against that same real
/// session); a `Z:\...` Windows-style path is also accepted as a fallback for launch methods
/// that might pass one instead, translated via Proton/Wine's default prefix convention of
/// mapping drive `Z:` onto the Unix root `/`.
#[cfg(target_os = "linux")]
fn find_proton_exe_path(cmd: &[std::ffi::OsString]) -> Option<PathBuf> {
    let proton_idx = cmd.iter().take(3).position(|arg| {
        let s = arg.to_str().unwrap_or_default();
        s == "proton" || s.ends_with("/proton")
    })?;
    if cmd.get(proton_idx + 1)?.to_str()? != "waitforexitandrun" {
        return None;
    }
    let arg = cmd.get(proton_idx + 2)?.to_str()?;
    if !arg.to_lowercase().ends_with(".exe") {
        return None;
    }
    if arg.starts_with('/') {
        return Some(PathBuf::from(arg));
    }
    let rest = arg.strip_prefix("Z:").or_else(|| arg.strip_prefix("z:"))?;
    Some(PathBuf::from(rest.replace('\\', "/")))
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
        // Built via `Path::join` rather than a hardcoded backslash string -- a literal
        // "C:\Steam\..." path is meaningless on Linux (backslash isn't a separator there), so
        // this test previously passed on Windows but failed on Linux despite the matching logic
        // itself being correct on both platforms.
        let install_dir = PathBuf::from("Steam").join("steamapps").join("common").join("Portal 2");
        let games = vec![InstalledGame {
            appid: "620".to_string(),
            name: "Portal 2".to_string(),
            install_dir: install_dir.clone(),
        }];
        let exe = install_dir.join("portal2.exe");
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
    #[cfg(target_os = "linux")]
    fn matches_proton_game_via_native_cmdline_path() {
        // Captured from an actual running Proton session (Among Us via Proton Experimental) --
        // Steam's own launch chain passes the target .exe as a plain native Linux path, not a
        // Windows-style "Z:\..." one.
        let games = vec![InstalledGame {
            appid: "945360".to_string(),
            name: "Among Us".to_string(),
            install_dir: PathBuf::from("/home/user/.local/share/Steam/steamapps/common/Among Us"),
        }];
        // python3's own resolved binary -- not the game, this alone must not match.
        let python_exe = PathBuf::from("/usr/bin/python3.11");
        assert!(find_installed_game_for_exe(&python_exe, &games).is_none());

        let cmd: Vec<std::ffi::OsString> = vec![
            "/home/user/.local/share/Steam/steamapps/common/Proton - Experimental/proton".into(),
            "waitforexitandrun".into(),
            "/home/user/.local/share/Steam/steamapps/common/Among Us/Among Us.exe".into(),
        ];
        let (found, matched_path) = find_installed_game_for_exe_or_cmd(Some(&python_exe), &cmd, &games).unwrap();
        assert_eq!(found.appid, "945360");
        assert_eq!(matched_path.file_name().unwrap(), "Among Us.exe");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn ignores_a_wrapper_process_that_merely_inherited_the_proton_command() {
        // Captured verbatim from a real Among Us launch: reaper's own argv (Steam's process
        // supervisor, launched for every Steam game) inherits the *entire* downstream command,
        // so it carries the same trailing "proton waitforexitandrun <exe>" sequence as the real
        // proton process -- just with `proton` sitting at index 7, not index 0/1. This process
        // must NOT match, or which of several such wrapper processes the poll loop happens to
        // examine first (arbitrary HashMap iteration order) ends up non-deterministically chosen
        // as the PID tracked for exit detection, none of which reliably share the game's exact
        // lifetime the way the real proton process does.
        let games = vec![InstalledGame {
            appid: "945360".to_string(),
            name: "Among Us".to_string(),
            install_dir: PathBuf::from("/home/ogray/.local/share/Steam/steamapps/common/Among Us"),
        }];
        let reaper_exe = PathBuf::from("/home/ogray/.local/share/Steam/ubuntu12_32/reaper");
        let cmd: Vec<std::ffi::OsString> = vec![
            "/home/ogray/.local/share/Steam/ubuntu12_32/reaper".into(),
            "SteamLaunch".into(),
            "AppId=945360".into(),
            "--".into(),
            "/home/ogray/.local/share/Steam/steamapps/common/SteamLinuxRuntime_4/_v2-entry-point".into(),
            "--verb=waitforexitandrun".into(),
            "--".into(),
            "/home/ogray/.local/share/Steam/steamapps/common/Proton - Experimental/proton".into(),
            "waitforexitandrun".into(),
            "/home/ogray/.local/share/Steam/steamapps/common/Among Us/Among Us.exe".into(),
        ];
        assert!(find_installed_game_for_exe_or_cmd(Some(&reaper_exe), &cmd, &games).is_none());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn ignores_a_pressure_vessel_wrapper_that_merely_inherited_the_proton_command() {
        // Same real launch, one layer deeper: pressure-vessel's pv-adverb, buried behind a long
        // run of its own sandbox-setup flags before the inherited proton command appears.
        let games = vec![InstalledGame {
            appid: "945360".to_string(),
            name: "Among Us".to_string(),
            install_dir: PathBuf::from("/home/ogray/.local/share/Steam/steamapps/common/Among Us"),
        }];
        let pv_adverb_exe =
            PathBuf::from("/usr/lib/pressure-vessel/from-host/libexec/steam-runtime-tools-0/pv-adverb");
        let cmd: Vec<std::ffi::OsString> = vec![
            "/usr/lib/pressure-vessel/from-host/libexec/steam-runtime-tools-0/pv-adverb".into(),
            "--prefix=/usr/lib/pressure-vessel/from-host".into(),
            "--generate-locales".into(),
            "--fd".into(),
            "11".into(),
            "--regenerate-ld.so-cache".into(),
            "/var/pressure-vessel/ldso".into(),
            "--overrides-path".into(),
            "/usr/lib/pressure-vessel/overrides".into(),
            "--assign-fd=1=3".into(),
            "--assign-fd=2=4".into(),
            "--".into(),
            "/home/ogray/.local/share/Steam/steamapps/common/Proton - Experimental/proton".into(),
            "waitforexitandrun".into(),
            "/home/ogray/.local/share/Steam/steamapps/common/Among Us/Among Us.exe".into(),
        ];
        assert!(find_installed_game_for_exe_or_cmd(Some(&pv_adverb_exe), &cmd, &games).is_none());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn matches_the_real_python3_proton_process_specifically() {
        // The actual process this is all meant to identify: python3 running the proton script
        // directly, with nothing ahead of it but the interpreter -- captured from the same real
        // launch as the two wrapper tests above.
        let games = vec![InstalledGame {
            appid: "945360".to_string(),
            name: "Among Us".to_string(),
            install_dir: PathBuf::from("/home/ogray/.local/share/Steam/steamapps/common/Among Us"),
        }];
        let python_exe = PathBuf::from("/usr/bin/python3.11");
        let cmd: Vec<std::ffi::OsString> = vec![
            "python3".into(),
            "/home/ogray/.local/share/Steam/steamapps/common/Proton - Experimental/proton".into(),
            "waitforexitandrun".into(),
            "/home/ogray/.local/share/Steam/steamapps/common/Among Us/Among Us.exe".into(),
        ];
        let (found, matched_path) = find_installed_game_for_exe_or_cmd(Some(&python_exe), &cmd, &games).unwrap();
        assert_eq!(found.appid, "945360");
        assert_eq!(matched_path.file_name().unwrap(), "Among Us.exe");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn matches_proton_game_via_windows_style_cmdline_path() {
        // Fallback for a launch method that passes a Windows-style path instead (not confirmed
        // against a real process, unlike the native-path case above, but kept as a defensive
        // second form).
        let games = vec![InstalledGame {
            appid: "620".to_string(),
            name: "Portal 2".to_string(),
            install_dir: PathBuf::from("/home/user/.steam/steam/steamapps/common/Portal 2"),
        }];
        let wine_exe = PathBuf::from("/home/user/.steam/steam/steamapps/common/Proton 9.0/dist/bin/wine64");
        assert!(find_installed_game_for_exe(&wine_exe, &games).is_none());

        let cmd: Vec<std::ffi::OsString> = vec![
            "/home/user/.steam/steam/steamapps/common/Proton 9.0/proton".into(),
            "waitforexitandrun".into(),
            "Z:\\home\\user\\.steam\\steam\\steamapps\\common\\Portal 2\\portal2.exe".into(),
        ];
        let (found, matched_path) = find_installed_game_for_exe_or_cmd(Some(&wine_exe), &cmd, &games).unwrap();
        assert_eq!(found.appid, "620");
        // The name recorded for this session must come from the translated path (the real
        // game exe), never from the Wine/Proton binary that actually resolved for the process.
        assert_eq!(matched_path.file_name().unwrap(), "portal2.exe");
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
