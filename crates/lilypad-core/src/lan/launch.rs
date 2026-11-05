//! What "launch game X" means on this PC, decided only from things already here.
//!
//! A phone sends a game id and optionally the game's Steam launch id (digits). From those, in order:
//! 1. the Steam launch id, as `steam://rungameid/<digits>` (this also starts non-Steam games added to Steam);
//! 2. a mapping for that game whose recorded `exe_path` still exists;
//! 3. what LilyPad can work out for itself from the mapping (a `fallback`): the Steam game whose install folder holds that executable, or
//!    the executable found in a folder LilyPad was told to watch. A mapping typed in by hand only names the executable; the path on disk is
//!    only learned when the game has been seen running, so without this a game that has never been tracked could not be started.
//! Nothing else is ever run. In particular no path, command line or argument from the phone reaches a process.

use crate::config::{LaunchOverride, ProcessMapping};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// Open `steam://rungameid/<id>`.
    Steam(String),
    /// Start this executable, in its own folder. A Windows shortcut (.lnk, .url) works too: the shell follows it as a double-click would.
    Exe(PathBuf),
    /// Open this address with whatever program is registered for it (`curseforge://launch-game?...`, `com.epicgames.launcher://...`).
    Url(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The game type isn't one of "game" or "live_service".
    BadRequest,
    /// No Steam launch id and no mapping with a path that exists: open the game once on this PC first.
    NoLaunchMethod,
}

impl Refusal {
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::BadRequest => "bad_request",
            Refusal::NoLaunchMethod => "no_launch_method",
        }
    }
}

/// Steam launch ids are plain digits (64-bit for non-Steam shortcuts), so anything else is dropped rather than passed on.
pub fn clean_steam_id(raw: Option<&str>) -> Option<String> {
    let id = raw?.trim();
    (!id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_string())
}

fn mapping_matches(mapping: &ProcessMapping, game_type: &str, froglog_id: i32) -> bool {
    mapping.froglog_id == froglog_id
        && match game_type {
            "game" => mapping.r#type == "regular" || mapping.r#type == "session",
            "live_service" => mapping.r#type == "live",
            _ => false,
        }
}

/// The mapping's recorded executable, if the file is still there.
fn recorded_exe(mappings: &[ProcessMapping], game_type: &str, froglog_id: i32, exists: &dyn Fn(&std::path::Path) -> bool) -> Option<PathBuf> {
    mappings
        .iter()
        .filter(|m| mapping_matches(m, game_type, froglog_id))
        .filter_map(|m| m.exe_path.as_deref())
        .map(PathBuf::from)
        .find(|path| exists(path))
}

/// Looks for a file called `name` (any case) in `dir` or up to `depth` folders below it. Symlinked folders are not followed.
pub fn find_file(dir: &std::path::Path, name: &str, depth: usize) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            subdirs.push(entry.path());
        } else if entry.file_name().to_str().is_some_and(|n| n.eq_ignore_ascii_case(name)) {
            return Some(entry.path());
        }
    }
    if depth == 0 {
        return None;
    }
    subdirs.into_iter().find_map(|sub| find_file(&sub, name, depth - 1))
}

/// Address schemes that open or run content rather than a launcher, so are never accepted as a launch command.
const BLOCKED_SCHEMES: &[&str] = &["file", "javascript", "data", "vbscript"];

/// `scheme://rest` with no spaces, where the scheme is a real one (two or more characters, so a drive letter like `C://` is not).
fn parse_url(text: &str) -> Option<String> {
    let (scheme, rest) = text.split_once("://")?;
    let valid = scheme.len() >= 2
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    let allowed = !BLOCKED_SCHEMES.iter().any(|b| scheme.eq_ignore_ascii_case(b));
    (valid && allowed && !rest.is_empty() && !text.chars().any(char::is_whitespace)).then(|| text.to_string())
}

/// Reads a launch command someone typed in on this PC: a `steam://rungameid/<digits>` address, any other `scheme://` address (a launcher's
/// own link), or the full path of an executable or shortcut (optionally in quotes). Anything else, including a relative path, or
/// arguments after a path, is rejected, so nothing a remote device sends can ever become part of what runs.
pub fn parse_command(raw: &str) -> Option<Plan> {
    let text = raw.trim();
    if text.is_empty() || text.chars().any(|c| c.is_control()) {
        return None;
    }
    if let Some(rest) = text.strip_prefix("steam://rungameid/") {
        return clean_steam_id(Some(rest)).map(Plan::Steam);
    }
    if let Some(url) = parse_url(text) {
        return Some(Plan::Url(url));
    }
    let path = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')).unwrap_or(text);
    (!path.contains('"') && Path::new(path).is_absolute()).then(|| Plan::Exe(PathBuf::from(path)))
}

/// The command shown for a plan, in the form `parse_command` reads back.
pub fn describe(plan: &Plan) -> String {
    match plan {
        Plan::Steam(id) => format!("steam://rungameid/{id}"),
        Plan::Exe(path) => path.display().to_string(),
        Plan::Url(url) => url.clone(),
    }
}

/// `plan`, but a command set for this game by hand comes first. One that can't be used (not a valid command, or an executable that
/// is gone) is refused, not quietly replaced with something else, so what the user typed is what runs or nothing does.
pub fn plan_for(
    game_type: &str,
    froglog_id: i32,
    steam_launch_id: Option<&str>,
    overrides: &[LaunchOverride],
    mappings: &[ProcessMapping],
    exists: &dyn Fn(&std::path::Path) -> bool,
    fallback: &dyn Fn(&ProcessMapping) -> Option<Plan>,
) -> Result<Plan, Refusal> {
    if let Some(custom) = overrides.iter().find(|o| o.game_type == game_type && o.froglog_id == froglog_id) {
        return match parse_command(&custom.command) {
            Some(Plan::Exe(path)) if !exists(&path) => Err(Refusal::NoLaunchMethod),
            Some(plan) => Ok(plan),
            None => Err(Refusal::NoLaunchMethod),
        };
    }
    plan(game_type, froglog_id, steam_launch_id, mappings, exists, fallback)
}

pub fn plan(
    game_type: &str,
    froglog_id: i32,
    steam_launch_id: Option<&str>,
    mappings: &[ProcessMapping],
    exists: &dyn Fn(&std::path::Path) -> bool,
    fallback: &dyn Fn(&ProcessMapping) -> Option<Plan>,
) -> Result<Plan, Refusal> {
    if game_type != "game" && game_type != "live_service" {
        return Err(Refusal::BadRequest);
    }
    if let Some(id) = clean_steam_id(steam_launch_id) {
        return Ok(Plan::Steam(id));
    }
    if let Some(path) = recorded_exe(mappings, game_type, froglog_id, exists) {
        return Ok(Plan::Exe(path));
    }
    mappings
        .iter()
        .filter(|m| mapping_matches(m, game_type, froglog_id))
        .find_map(|m| fallback(m))
        .ok_or(Refusal::NoLaunchMethod)
}

/// Whether this PC could launch the game right now (the "Play on PC" button's test).
pub fn can_launch(
    game_type: &str,
    froglog_id: i32,
    steam_launch_id: Option<&str>,
    mappings: &[ProcessMapping],
    exists: &dyn Fn(&std::path::Path) -> bool,
    fallback: &dyn Fn(&ProcessMapping) -> Option<Plan>,
) -> bool {
    plan(game_type, froglog_id, steam_launch_id, mappings, exists, fallback).is_ok()
}

/// Starts something the way Explorer does when you double-click it. On Windows that is what makes a game whose manifest asks for
/// administrator rights start at all (with the UAC prompt on the PC for someone to accept): creating the process directly fails for
/// those with "The requested operation requires elevation" (error 740). It also takes care of quoting, so no path or URL is ever
/// passed through a command line.
#[cfg(windows)]
fn shell_open(target: &std::ffi::OsStr, dir: Option<&Path>) -> std::io::Result<()> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    // ShellExecute wants COM on the calling thread. A thread that already has it (or has it another way) is fine, so the result is ignored.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let operation = HSTRING::from("open");
    let file = HSTRING::from(target);
    let directory = dir.map(|d| HSTRING::from(d.as_os_str()));
    let result = unsafe {
        ShellExecuteW(
            HWND::default(),
            &operation,
            &file,
            PCWSTR::null(),
            directory.as_ref().map(|d| PCWSTR(d.as_ptr())).unwrap_or(PCWSTR::null()),
            SW_SHOWNORMAL,
        )
    };
    // A value above 32 is success; anything else is an error code.
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(result.0 as isize as i32))
    }
}

/// A Windows shortcut or internet shortcut, which the shell opens for us.
#[cfg(windows)]
fn is_shortcut(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("lnk") || e.eq_ignore_ascii_case("url"))
}

/// Starts the plan. Windows hands it to the shell (see `shell_open`); Linux uses `xdg-open` for a Steam URL and runs an executable
/// directly. The executable gets its own folder as the working directory, which many games need.
pub fn start(plan: &Plan) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        match plan {
            Plan::Steam(id) => shell_open(std::ffi::OsStr::new(&format!("steam://rungameid/{id}")), None),
            Plan::Url(url) => shell_open(std::ffi::OsStr::new(url), None),
            // A shortcut carries its own working folder, which passing the shortcut's own would override.
            Plan::Exe(path) if is_shortcut(path) => shell_open(path.as_os_str(), None),
            Plan::Exe(path) => shell_open(path.as_os_str(), path.parent()),
        }
    }
    #[cfg(not(windows))]
    {
        use std::process::{Command, Stdio};
        let mut command = match plan {
            Plan::Steam(id) => {
                let mut c = Command::new("xdg-open");
                c.arg(format!("steam://rungameid/{id}"));
                c
            }
            Plan::Url(url) => {
                let mut c = Command::new("xdg-open");
                c.arg(url);
                c
            }
            Plan::Exe(path) => {
                let mut c = Command::new(path);
                if let Some(dir) = path.parent() {
                    c.current_dir(dir);
                }
                c
            }
        };
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(kind: &str, id: i32, exe: Option<&str>) -> ProcessMapping {
        ProcessMapping { process: "game.exe".into(), r#type: kind.into(), froglog_id: id, title: None, title_filter: None, exe_path: exe.map(String::from) }
    }
    fn there(_: &std::path::Path) -> bool {
        true
    }
    fn gone(_: &std::path::Path) -> bool {
        false
    }
    fn none(_: &ProcessMapping) -> Option<Plan> {
        None
    }

    #[test]
    fn typed_commands_are_a_steam_address_or_an_absolute_path_only() {
        assert_eq!(parse_command("steam://rungameid/620"), Some(Plan::Steam("620".into())));
        assert_eq!(parse_command("  steam://rungameid/620  "), Some(Plan::Steam("620".into())));
        assert_eq!(parse_command("steam://rungameid/620 -novid"), None);
        assert_eq!(parse_command("steam://rungameid/"), None);
        assert_eq!(parse_command("steam://rungameid/abc"), None);
        assert_eq!(parse_command("game.exe"), None);
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("calc.exe & del *"), None);
        #[cfg(windows)]
        {
            assert_eq!(parse_command(r"C:\Games\a.exe"), Some(Plan::Exe(PathBuf::from(r"C:\Games\a.exe"))));
            assert_eq!(parse_command(r#""C:\Games\a.exe""#), Some(Plan::Exe(PathBuf::from(r"C:\Games\a.exe"))));
        }
        #[cfg(not(windows))]
        assert_eq!(parse_command("/games/a.sh"), Some(Plan::Exe(PathBuf::from("/games/a.sh"))));
    }

    #[test]
    fn a_launchers_own_link_is_accepted_but_not_something_that_opens_files() {
        let link = "curseforge://launch-game?instanceId=71127f30-e169-4e9e-a97f-70846b8a9f7c&gameId=432";
        assert_eq!(parse_command(link), Some(Plan::Url(link.into())));
        assert_eq!(parse_command("steam://run/620"), Some(Plan::Url("steam://run/620".into())));
        assert_eq!(parse_command("com.epicgames.launcher://apps/x?action=launch"), Some(Plan::Url("com.epicgames.launcher://apps/x?action=launch".into())));
        assert_eq!(parse_command("curseforge://launch-game?a=1 && calc"), None);
        assert_eq!(parse_command("file:///C:/Windows/System32/calc.exe"), None);
        assert_eq!(parse_command("FILE://x"), None);
        assert_eq!(parse_command("javascript://alert(1)"), None);
        assert_eq!(parse_command("curseforge://"), None);
        // A drive letter is not a scheme: on Windows this is just a path
        #[cfg(windows)]
        assert_eq!(parse_command("C://Games/a.exe"), Some(Plan::Exe(PathBuf::from("C://Games/a.exe"))));
        #[cfg(not(windows))]
        assert_eq!(parse_command("C://Games/a.exe"), None);
        #[cfg(windows)]
        assert_eq!(parse_command(r"C:\Users\me\Desktop\Game.lnk"), Some(Plan::Exe(PathBuf::from(r"C:\Users\me\Desktop\Game.lnk"))));
    }

    fn custom(kind: &str, id: i32, command: &str) -> LaunchOverride {
        LaunchOverride { froglog_id: id, game_type: kind.into(), command: command.into() }
    }

    #[test]
    fn a_typed_command_wins_over_the_phone_and_the_mapping() {
        let m = vec![mapping("regular", 7, Some("C:/old/game.exe"))];
        let overrides = vec![custom("game", 7, "steam://rungameid/99")];
        assert_eq!(plan_for("game", 7, Some("620"), &overrides, &m, &there, &none), Ok(Plan::Steam("99".into())));
        // Another game, or the other kind of game, is not affected
        assert_eq!(plan_for("game", 8, Some("620"), &overrides, &m, &there, &none), Ok(Plan::Steam("620".into())));
        assert_eq!(plan_for("live_service", 7, Some("620"), &overrides, &m, &there, &none), Ok(Plan::Steam("620".into())));
    }

    #[test]
    fn a_typed_command_that_cannot_run_is_refused_rather_than_replaced() {
        let m = vec![mapping("regular", 7, Some("C:/old/game.exe"))];
        let bad = vec![custom("game", 7, "not a command")];
        assert_eq!(plan_for("game", 7, Some("620"), &bad, &m, &there, &none), Err(Refusal::NoLaunchMethod));
        #[cfg(windows)]
        let missing = vec![custom("game", 7, r"C:\gone\game.exe")];
        #[cfg(not(windows))]
        let missing = vec![custom("game", 7, "/gone/game.exe")];
        assert_eq!(plan_for("game", 7, Some("620"), &missing, &m, &gone, &none), Err(Refusal::NoLaunchMethod));
    }

    #[test]
    fn a_steam_id_wins() {
        let m = vec![mapping("regular", 5, Some("C:/Games/x.exe"))];
        assert_eq!(plan("game", 5, Some("12345"), &m, &there, &none), Ok(Plan::Steam("12345".into())));
    }

    #[test]
    fn a_recorded_exe_is_used_without_a_steam_id() {
        let m = vec![mapping("session", 5, Some("C:/Games/x.exe"))];
        assert_eq!(plan("game", 5, None, &m, &there, &none), Ok(Plan::Exe(PathBuf::from("C:/Games/x.exe"))));
    }

    #[test]
    fn an_exe_that_is_no_longer_there_is_not_launched() {
        let m = vec![mapping("regular", 5, Some("C:/Games/x.exe"))];
        assert_eq!(plan("game", 5, None, &m, &gone, &none), Err(Refusal::NoLaunchMethod));
    }

    #[test]
    fn a_mapping_without_a_recorded_path_has_no_method() {
        let m = vec![mapping("regular", 5, None)];
        assert_eq!(plan("game", 5, None, &m, &there, &none), Err(Refusal::NoLaunchMethod));
    }

    #[test]
    fn mappings_of_another_game_or_type_do_not_match() {
        let m = vec![mapping("regular", 6, Some("C:/a.exe")), mapping("live", 5, Some("C:/b.exe"))];
        assert_eq!(plan("game", 5, None, &m, &there, &none), Err(Refusal::NoLaunchMethod));
        assert_eq!(plan("live_service", 5, None, &m, &there, &none), Ok(Plan::Exe(PathBuf::from("C:/b.exe"))));
    }

    #[test]
    fn only_digits_are_accepted_as_a_steam_id() {
        assert_eq!(clean_steam_id(Some("12345678901234567890")), Some("12345678901234567890".into()));
        assert_eq!(clean_steam_id(Some("123456789012345678901")), None);
        assert_eq!(clean_steam_id(Some("1 && calc")), None);
        assert_eq!(clean_steam_id(Some("../x")), None);
        assert_eq!(clean_steam_id(Some("")), None);
        assert_eq!(clean_steam_id(None), None);
        // A bad id is dropped, not passed on, and the mapping is used instead.
        let m = vec![mapping("regular", 5, Some("C:/Games/x.exe"))];
        assert_eq!(plan("game", 5, Some("1;calc"), &m, &there, &none), Ok(Plan::Exe(PathBuf::from("C:/Games/x.exe"))));
    }

    #[test]
    fn what_lilypad_can_work_out_is_used_when_nothing_is_recorded() {
        // The mapping names an executable but was never seen running, so it has no path.
        let m = vec![mapping("session", 5, None)];
        let steam = |_: &ProcessMapping| Some(Plan::Steam("777".into()));
        assert_eq!(plan("game", 5, None, &m, &there, &steam), Ok(Plan::Steam("777".into())));
        // A recorded path still comes first, and a phone's Steam id before that.
        let with_path = vec![mapping("session", 5, Some("C:/Games/x.exe"))];
        assert_eq!(plan("game", 5, None, &with_path, &there, &steam), Ok(Plan::Exe(PathBuf::from("C:/Games/x.exe"))));
        assert_eq!(plan("game", 5, Some("12"), &m, &there, &steam), Ok(Plan::Steam("12".into())));
        // It is only asked about this game's own mappings.
        assert_eq!(plan("game", 6, None, &m, &there, &steam), Err(Refusal::NoLaunchMethod));
    }

    #[test]
    fn a_file_is_found_below_a_folder_whatever_its_case() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("Game").join("Binaries").join("Win64");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("Hellraiser-Win64-Shipping.exe"), b"").unwrap();
        let found = find_file(dir.path(), "hellraiser-win64-shipping.EXE", 4).unwrap();
        assert!(found.ends_with("Hellraiser-Win64-Shipping.exe"));
        assert!(find_file(dir.path(), "other.exe", 4).is_none());
        assert!(find_file(dir.path(), "Hellraiser-Win64-Shipping.exe", 1).is_none(), "too deep for the depth given");
    }

    #[test]
    fn unknown_game_types_are_refused() {
        assert_eq!(plan("anything", 5, Some("1"), &[], &there, &none), Err(Refusal::BadRequest));
    }
}
