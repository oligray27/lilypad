//! Pure data logic for the mappings view: staged edits and conflict detection.
//! Ported from `main.js`'s `pendingExeFor`/`savedExeFor`/`detectConflicts`/
//! `refreshMappingsState` (lines ~158-458 of the Tauri frontend). No GTK
//! dependency, so it's testable standalone.

use lilypad_core::config::ProcessMapping;
use std::collections::{HashMap, HashSet};

pub type MapKey = (String, i32); // (game_type, froglog_id)

pub fn map_key(game_type: &str, id: i32) -> MapKey {
    (game_type.to_string(), id)
}

pub fn norm(s: &str) -> String {
    s.trim().to_string()
}

/// One row backing the table: a game/live-service entry the user can map an
/// executable to.
#[derive(Debug, Clone)]
pub struct GameRow {
    pub id: i32,
    pub title: String,
    /// "regular" | "session" | "live"
    pub game_type: String,
}

impl GameRow {
    pub fn key(&self) -> MapKey {
        map_key(&self.game_type, self.id)
    }
}

/// exe / title-filter values keyed by (game_type, id). Used for both the
/// on-disk snapshot ("saved") and the in-progress edit buffer ("pending").
#[derive(Debug, Clone, Default)]
pub struct StagedMappings {
    pub exe: HashMap<MapKey, String>,
    pub title_filter: HashMap<MapKey, String>,
}

impl StagedMappings {
    pub fn from_process_mappings(mappings: &[ProcessMapping]) -> Self {
        let mut exe = HashMap::new();
        let mut title_filter = HashMap::new();
        for m in mappings {
            let key = map_key(&m.r#type, m.froglog_id);
            exe.insert(key.clone(), m.process.clone());
            title_filter.insert(key, m.title_filter.clone().unwrap_or_default());
        }
        Self { exe, title_filter }
    }
}

/// Mirrors `detectConflicts()`: two entries conflict if they share the same
/// (case-insensitive) exe and either both lack a title filter, share the same
/// filter, or any entry among that group lacks one.
pub fn detect_conflicts(staged: &StagedMappings) -> HashSet<MapKey> {
    let mut conflicts = HashSet::new();
    let mut by_exe: HashMap<String, Vec<(MapKey, String)>> = HashMap::new();

    for (key, exe) in &staged.exe {
        let exe = norm(exe);
        if exe.is_empty() {
            continue;
        }
        let filter = staged
            .title_filter
            .get(key)
            .map(|s| norm(s).to_lowercase())
            .unwrap_or_default();
        by_exe.entry(exe.to_lowercase()).or_default().push((key.clone(), filter));
    }

    for entries in by_exe.values() {
        if entries.len() < 2 {
            continue;
        }
        let has_missing_filter = entries.iter().any(|(_, f)| f.is_empty());
        for i in 0..entries.len() {
            for j in (i + 1)..entries.len() {
                let (ka, fa) = &entries[i];
                let (kb, fb) = &entries[j];
                let a_bare = fa.is_empty();
                let b_bare = fb.is_empty();
                let same_filter = !fa.is_empty() && !fb.is_empty() && fa == fb;
                if (a_bare && b_bare) || same_filter || has_missing_filter {
                    conflicts.insert(ka.clone());
                    conflicts.insert(kb.clone());
                }
            }
        }
    }

    conflicts
}

/// Mirrors the dirty-check in `refreshMappingsState()`: true if any key's
/// pending exe/filter differs from what's saved on disk.
pub fn is_dirty(pending: &StagedMappings, saved: &StagedMappings) -> bool {
    let mut all_keys: HashSet<&MapKey> = HashSet::new();
    all_keys.extend(pending.exe.keys());
    all_keys.extend(saved.exe.keys());
    all_keys.extend(pending.title_filter.keys());
    all_keys.extend(saved.title_filter.keys());

    for k in all_keys {
        let pe = pending.exe.get(k).map(|s| norm(s)).unwrap_or_default();
        let se = saved.exe.get(k).map(|s| norm(s)).unwrap_or_default();
        let pf = pending.title_filter.get(k).map(|s| norm(s)).unwrap_or_default();
        let sf = saved.title_filter.get(k).map(|s| norm(s)).unwrap_or_default();
        if pe != se || pf != sf {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged(entries: &[(&str, i32, &str, &str)]) -> StagedMappings {
        let mut s = StagedMappings::default();
        for (game_type, id, exe, filter) in entries {
            let key = map_key(game_type, *id);
            s.exe.insert(key.clone(), exe.to_string());
            s.title_filter.insert(key, filter.to_string());
        }
        s
    }

    #[test]
    fn no_conflict_when_exes_differ() {
        let s = staged(&[("regular", 1, "hl2.exe", ""), ("regular", 2, "portal2.exe", "")]);
        assert!(detect_conflicts(&s).is_empty());
    }

    #[test]
    fn conflict_when_same_exe_no_filters() {
        let s = staged(&[("regular", 1, "javaw.exe", ""), ("regular", 2, "javaw.exe", "")]);
        let conflicts = detect_conflicts(&s);
        assert_eq!(conflicts.len(), 2);
        assert!(conflicts.contains(&map_key("regular", 1)));
        assert!(conflicts.contains(&map_key("regular", 2)));
    }

    #[test]
    fn no_conflict_when_same_exe_distinct_filters() {
        let s = staged(&[
            ("regular", 1, "javaw.exe", "Modpack A"),
            ("regular", 2, "javaw.exe", "Modpack B"),
        ]);
        assert!(detect_conflicts(&s).is_empty());
    }

    #[test]
    fn conflict_when_same_exe_same_filter() {
        let s = staged(&[
            ("regular", 1, "javaw.exe", "Modpack A"),
            ("regular", 2, "javaw.exe", "Modpack A"),
        ]);
        assert_eq!(detect_conflicts(&s).len(), 2);
    }

    #[test]
    fn conflict_when_one_missing_filter_among_shared_exe() {
        let s = staged(&[
            ("regular", 1, "javaw.exe", "Modpack A"),
            ("regular", 2, "javaw.exe", ""),
        ]);
        assert_eq!(detect_conflicts(&s).len(), 2);
    }

    #[test]
    fn filter_matching_is_case_insensitive() {
        let s = staged(&[
            ("regular", 1, "javaw.exe", "Modpack A"),
            ("regular", 2, "javaw.exe", "modpack a"),
        ]);
        assert_eq!(detect_conflicts(&s).len(), 2);
    }

    #[test]
    fn dirty_detects_new_exe() {
        let saved = StagedMappings::default();
        let pending = staged(&[("regular", 1, "hl2.exe", "")]);
        assert!(is_dirty(&pending, &saved));
    }

    #[test]
    fn not_dirty_when_equal() {
        let saved = staged(&[("regular", 1, "hl2.exe", "")]);
        let pending = staged(&[("regular", 1, "hl2.exe", "")]);
        assert!(!is_dirty(&pending, &saved));
    }

    #[test]
    fn dirty_ignores_surrounding_whitespace() {
        let saved = staged(&[("regular", 1, "hl2.exe", "")]);
        let pending = staged(&[("regular", 1, "  hl2.exe  ", "")]);
        assert!(!is_dirty(&pending, &saved));
    }
}
