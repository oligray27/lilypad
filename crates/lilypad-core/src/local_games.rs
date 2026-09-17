//! Detection for games not distributed via Steam: the user points LilyPad at a root folder
//! (e.g. their GOG or itch.io library) via Configure, and every immediate subfolder is treated
//! as a separate installed game, with the folder name as a guessed title. Feeds into the same
//! `InstalledGame` list Steam detection produces, so the rest of the pipeline (New Games,
//! resolve, auto-link eligibility, etc.) doesn't need to know the difference.

use crate::steam::InstalledGame;
use std::path::{Path, PathBuf};

/// Prefix marking an `InstalledGame.appid` as a synthetic local identity (a folder path) rather
/// than a real Steam appid. `LibraryIndex::resolve_by_appid` will never match one of these —
/// nothing in the FrogLog library has a numeric Steam appid that looks like this — so local
/// games always flow through the manual New Games resolve UI instead of silent auto-link.
pub const LOCAL_ID_PREFIX: &str = "local:";

/// Scans every configured watched root folder for immediate subfolders, treating each as a
/// separate installed game. Missing/unreadable roots are skipped silently (the user may have
/// unplugged a drive, renamed a folder, etc. — not worth surfacing as an error here).
/// A cheap fingerprint of which games are installed: the names of the Steam manifests in each
/// library folder, and the game folders in each watched root.
///
/// This gates the real scan, which reads and VDF-parses every manifest (~54 files on a typical
/// library) — too expensive to repeat every few seconds, which is why a newly installed game used
/// to go unnoticed for up to five minutes. Listing directory entries reads no file contents and
/// parses nothing, so it is cheap enough to check often.
///
/// **Entry names, not directory mtimes.** The first version compared mtimes, which was wrong in
/// both directions. Steam rewrites `appmanifest_*.acf` constantly — on launch, quit, update
/// checks, playtime accounting — and does so atomically, creating a `.tmp` and renaming over the
/// original. Both are directory-entry operations, so the mtime changed every few minutes and
/// triggered a full rescan each time; measured at 14 rescans in a quarter of an hour with nothing
/// installed. And Windows does *not* dependably update a directory's mtime on removal, so
/// uninstalls were missed entirely. The set of names changes when, and only when, the set of
/// installed games changes.
///
/// Only `appmanifest_*.acf` counts inside a library folder, so Steam's own churn — `.tmp` files
/// mid-write, `downloading/`, `temp/`, `libraryfolders.vdf` — is ignored.
///
/// An unreadable directory yields an empty list rather than being skipped, so a library folder
/// appearing or disappearing is itself a change.
pub fn install_locations_fingerprint(
    steam_root: Option<&Path>,
    watched_roots: &[String],
) -> Vec<(PathBuf, Vec<String>)> {
    let mut fingerprint = Vec::new();
    if let Some(root) = steam_root {
        for library in crate::steam::list_library_folders(root) {
            let dir = library.join("steamapps");
            let manifests = entry_names(&dir, |name, _is_dir| {
                name.to_ascii_lowercase().starts_with("appmanifest_")
                    && name.to_ascii_lowercase().ends_with(".acf")
            });
            fingerprint.push((dir, manifests));
        }
    }
    for root in watched_roots {
        let dir = PathBuf::from(root);
        let folders = entry_names(&dir, |_name, is_dir| is_dir);
        fingerprint.push((dir, folders));
    }
    fingerprint.sort();
    fingerprint.dedup();
    fingerprint
}

/// Sorted names of the entries in `dir` that `keep` accepts. Empty if it cannot be read.
fn entry_names(dir: &Path, keep: impl Fn(&str, bool) -> bool) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            keep(&name, is_dir).then_some(name)
        })
        .collect();
    names.sort();
    names
}

pub fn scan_watched_directories(roots: &[String]) -> Vec<InstalledGame> {
    let mut games = Vec::new();
    for root in roots {
        let root_path = Path::new(root);
        let Ok(entries) = std::fs::read_dir(root_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            games.push(InstalledGame {
                appid: format!("{LOCAL_ID_PREFIX}{}", path.display()),
                name: name.to_string(),
                install_dir: path,
            });
        }
    }
    games
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both directions matter: a game appearing must be seen, and a game disappearing too. The
    /// mtime-based version this replaced missed removals entirely, because Windows does not
    /// dependably update a directory's mtime when an entry is deleted.
    #[test]
    fn the_fingerprint_follows_games_appearing_and_disappearing() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("GOG");
        std::fs::create_dir_all(&watched).unwrap();
        let roots = vec![watched.to_string_lossy().into_owned()];

        let empty = install_locations_fingerprint(None, &roots);
        assert_eq!(empty, install_locations_fingerprint(None, &roots), "unstable when idle");

        std::fs::create_dir(watched.join("Some Game")).unwrap();
        let installed = install_locations_fingerprint(None, &roots);
        assert_ne!(empty, installed, "an installed game must be visible");

        std::fs::remove_dir(watched.join("Some Game")).unwrap();
        assert_eq!(empty, install_locations_fingerprint(None, &roots), "a removal must be visible");
    }

    /// The regression that prompted the rewrite: Steam rewrites manifests constantly and does so
    /// atomically, leaving `.tmp` files in the library folder mid-write. None of that changes
    /// which games are installed, so none of it should force a rescan. A loose file, a
    /// subdirectory, or `libraryfolders.vdf` must likewise be ignored.
    #[test]
    fn steam_writing_to_a_library_folder_is_not_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let steamapps = dir.path().join("steamapps");
        std::fs::create_dir_all(&steamapps).unwrap();
        std::fs::write(steamapps.join("appmanifest_570.acf"), b"x").unwrap();
        // Compared directly, since driving this through a fake Steam root would need a
        // libraryfolders.vdf; the filtering is the part under test.
        let manifests = |d: &Path| {
            entry_names(d, |name, _| {
                let lower = name.to_ascii_lowercase();
                lower.starts_with("appmanifest_") && lower.ends_with(".acf")
            })
        };
        let before = manifests(&steamapps);
        assert_eq!(before, vec!["appmanifest_570.acf".to_string()]);

        // Steam's atomic-write temp file, its working directories, and its own bookkeeping.
        std::fs::write(steamapps.join("appmanifest_570.acf.tmp"), b"x").unwrap();
        std::fs::write(steamapps.join("libraryfolders.vdf"), b"x").unwrap();
        std::fs::create_dir(steamapps.join("downloading")).unwrap();
        assert_eq!(before, manifests(&steamapps), "Steam's own churn must not force a rescan");

        // An actual install must still register.
        std::fs::write(steamapps.join("appmanifest_620.acf"), b"x").unwrap();
        assert_ne!(before, manifests(&steamapps));
    }

    /// A directory that cannot be read yields an empty list rather than being skipped, so a
    /// library folder appearing (a second drive mounted, a watched root created) is a change.
    #[test]
    fn a_missing_location_is_recorded_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("not-yet");
        let roots = vec![absent.to_string_lossy().into_owned()];

        let before = install_locations_fingerprint(None, &roots);
        assert_eq!(before.len(), 1);
        assert!(before[0].1.is_empty());

        std::fs::create_dir(&absent).unwrap();
        std::fs::create_dir(absent.join("A Game")).unwrap();
        assert_ne!(before, install_locations_fingerprint(None, &roots));
    }
    use std::fs;

    #[test]
    fn scans_immediate_subfolders_as_games() {
        let tmp = std::env::temp_dir().join(format!("lilypad_local_games_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("Some Game")).unwrap();
        fs::create_dir_all(tmp.join("Another Game")).unwrap();
        fs::write(tmp.join("loose_file.txt"), b"not a game").unwrap();

        let roots = vec![tmp.to_string_lossy().to_string()];
        let games = scan_watched_directories(&roots);

        assert_eq!(games.len(), 2);
        assert!(games.iter().any(|g| g.name == "Some Game"));
        assert!(games.iter().any(|g| g.name == "Another Game"));
        assert!(games.iter().all(|g| g.appid.starts_with(LOCAL_ID_PREFIX)));

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn missing_root_is_skipped_not_an_error() {
        let games = scan_watched_directories(&["C:\\this\\path\\definitely\\does\\not\\exist".to_string()]);
        assert!(games.is_empty());
    }
}
