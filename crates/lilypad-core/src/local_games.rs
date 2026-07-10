//! Detection for games not distributed via Steam: the user points LilyPad at a root folder
//! (e.g. their GOG or itch.io library) via Configure, and every immediate subfolder is treated
//! as a separate installed game, with the folder name as a guessed title. Feeds into the same
//! `InstalledGame` list Steam detection produces, so the rest of the pipeline (New Games,
//! resolve, auto-link eligibility, etc.) doesn't need to know the difference.

use crate::steam::InstalledGame;
use std::path::Path;

/// Prefix marking an `InstalledGame.appid` as a synthetic local identity (a folder path) rather
/// than a real Steam appid. `LibraryIndex::resolve_by_appid` will never match one of these —
/// nothing in the FrogLog library has a numeric Steam appid that looks like this — so local
/// games always flow through the manual New Games resolve UI instead of silent auto-link.
pub const LOCAL_ID_PREFIX: &str = "local:";

/// Scans every configured watched root folder for immediate subfolders, treating each as a
/// separate installed game. Missing/unreadable roots are skipped silently (the user may have
/// unplugged a drive, renamed a folder, etc. — not worth surfacing as an error here).
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
