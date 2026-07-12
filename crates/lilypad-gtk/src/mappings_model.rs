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

/// Collapses rows that share a title down to just the most recent one -- the replay feature
/// creates a brand-new `games` row for each fresh playthrough rather than reusing an old
/// completed one, so without this the mappings table fills up with old, already-finished
/// duplicates of the same game. "Most recent" is the highest `id` (Postgres SERIAL ids are
/// monotonically increasing per insert, and a replay's new row is always inserted after the one
/// it replays), which also means a game just resolved via "Log as New Replay" naturally becomes
/// the one shown here, since its row is newer than the entry it replaced. Live-service rows are
/// left alone -- the replay feature only ever creates `games` rows, so live entries can't have
/// this kind of duplicate, and title-deduping them could otherwise wrongly hide a distinct
/// live-service game and a regular game that happen to share a title.
pub fn dedupe_latest_by_title(rows: Vec<GameRow>) -> Vec<GameRow> {
    let mut latest_by_title: HashMap<String, GameRow> = HashMap::new();
    let mut out = Vec::new();
    for row in rows {
        if row.game_type.eq_ignore_ascii_case("live") {
            out.push(row);
            continue;
        }
        let key = row.title.trim().to_lowercase();
        match latest_by_title.get(&key) {
            Some(existing) if existing.id >= row.id => {}
            _ => {
                latest_by_title.insert(key, row);
            }
        }
    }
    out.extend(latest_by_title.into_values());
    out
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
///
/// `visible` restricts comparison to keys currently shown in the table (see
/// `dedupe_latest_by_title`) -- a mapping left over on an older, now-hidden replay
/// duplicate would otherwise flag a "conflict" the user has no way to see or fix
/// (its row isn't in the list at all), even though nothing is actually wrong with
/// what's on screen.
pub fn detect_conflicts(staged: &StagedMappings, visible: &HashSet<MapKey>) -> HashSet<MapKey> {
    let mut conflicts = HashSet::new();
    let mut by_exe: HashMap<String, Vec<(MapKey, String)>> = HashMap::new();

    for (key, exe) in &staged.exe {
        if !visible.contains(key) {
            continue;
        }
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

    /// Every key in `staged` counts as visible -- what the old (pre-dedup) callers of
    /// `detect_conflicts` implicitly assumed.
    fn all_keys(staged: &StagedMappings) -> HashSet<MapKey> {
        staged.exe.keys().cloned().collect()
    }

    #[test]
    fn no_conflict_when_exes_differ() {
        let s = staged(&[("regular", 1, "hl2.exe", ""), ("regular", 2, "portal2.exe", "")]);
        assert!(detect_conflicts(&s, &all_keys(&s)).is_empty());
    }

    #[test]
    fn conflict_when_same_exe_no_filters() {
        let s = staged(&[("regular", 1, "javaw.exe", ""), ("regular", 2, "javaw.exe", "")]);
        let conflicts = detect_conflicts(&s, &all_keys(&s));
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
        assert!(detect_conflicts(&s, &all_keys(&s)).is_empty());
    }

    #[test]
    fn conflict_when_same_exe_same_filter() {
        let s = staged(&[
            ("regular", 1, "javaw.exe", "Modpack A"),
            ("regular", 2, "javaw.exe", "Modpack A"),
        ]);
        assert_eq!(detect_conflicts(&s, &all_keys(&s)).len(), 2);
    }

    #[test]
    fn conflict_when_one_missing_filter_among_shared_exe() {
        let s = staged(&[
            ("regular", 1, "javaw.exe", "Modpack A"),
            ("regular", 2, "javaw.exe", ""),
        ]);
        assert_eq!(detect_conflicts(&s, &all_keys(&s)).len(), 2);
    }

    #[test]
    fn filter_matching_is_case_insensitive() {
        let s = staged(&[
            ("regular", 1, "javaw.exe", "Modpack A"),
            ("regular", 2, "javaw.exe", "modpack a"),
        ]);
        assert_eq!(detect_conflicts(&s, &all_keys(&s)).len(), 2);
    }

    #[test]
    fn hidden_key_is_excluded_from_conflict_comparison() {
        // Same shape as `conflict_when_same_exe_no_filters`, but id 1's row has been collapsed
        // away by `dedupe_latest_by_title` (e.g. an old replay duplicate) -- only id 2 is
        // "visible", so there's nothing left to conflict with.
        let s = staged(&[("regular", 1, "javaw.exe", ""), ("regular", 2, "javaw.exe", "")]);
        let visible: HashSet<MapKey> = [map_key("regular", 2)].into_iter().collect();
        assert!(detect_conflicts(&s, &visible).is_empty());
    }

    #[test]
    fn conflict_still_flagged_when_both_sides_visible() {
        // Sanity check that hiding is opt-in per key, not a global bypass.
        let s = staged(&[("regular", 1, "javaw.exe", ""), ("regular", 2, "javaw.exe", "")]);
        let visible: HashSet<MapKey> = [map_key("regular", 1), map_key("regular", 2)].into_iter().collect();
        assert_eq!(detect_conflicts(&s, &visible).len(), 2);
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

    fn row(id: i32, title: &str, game_type: &str) -> GameRow {
        GameRow { id, title: title.to_string(), game_type: game_type.to_string() }
    }

    #[test]
    fn dedupe_keeps_only_the_highest_id_per_title() {
        let rows = vec![
            row(1, "Metal Gear Rising: Revengeance", "regular"),
            row(5, "Metal Gear Rising: Revengeance", "session"),
            row(3, "Hades", "regular"),
        ];
        let mut out = dedupe_latest_by_title(rows);
        out.sort_by_key(|r| r.id);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, 3);
        assert_eq!(out[1].id, 5);
        assert_eq!(out[1].game_type, "session");
    }

    #[test]
    fn dedupe_is_case_and_whitespace_insensitive() {
        let rows = vec![row(1, "  Hades ", "regular"), row(2, "hades", "session")];
        let out = dedupe_latest_by_title(rows);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, 2);
    }

    #[test]
    fn dedupe_never_collapses_live_service_rows() {
        let rows = vec![row(1, "Destiny 2", "live"), row(2, "Destiny 2", "live")];
        let out = dedupe_latest_by_title(rows);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn dedupe_does_not_collapse_live_against_regular_with_same_title() {
        let rows = vec![row(1, "Same Title", "regular"), row(2, "Same Title", "live")];
        let out = dedupe_latest_by_title(rows);
        assert_eq!(out.len(), 2);
    }
}
