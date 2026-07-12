//! Matching a detected game (Steam appid + title) against the user's existing FrogLog
//! library (`games` + `wishlist`), to decide whether it's worth notifying about.

use crate::api::{Game, LiveServiceGame, WishlistItem};
use std::collections::{HashMap, HashSet};

/// Lowercases, strips punctuation, and strips common edition suffixes so titles that
/// differ only cosmetically (store title vs. FrogLog title) still match.
pub fn normalize_title(s: &str) -> String {
    const EDITION_SUFFIXES: &[&str] = &[
        "game of the year edition",
        "goty edition",
        "definitive edition",
        "complete edition",
        "deluxe edition",
        "ultimate edition",
        "enhanced edition",
        "remastered",
        "remake",
    ];

    let lower = s.to_lowercase();
    let stripped: String = lower
        .chars()
        .map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' })
        .collect();
    let mut normalized = stripped.split_whitespace().collect::<Vec<_>>().join(" ");

    for suffix in EDITION_SUFFIXES {
        if let Some(stripped) = normalized.strip_suffix(suffix) {
            normalized = stripped.trim_end().to_string();
        }
    }
    normalized
}

fn steam_app_id_str(v: &Option<serde_json::Value>) -> Option<String> {
    v.as_ref().and_then(|v| {
        v.as_str()
            .map(|s| s.to_string())
            .or_else(|| v.as_i64().map(|n| n.to_string()))
    })
}

/// A `games` or `live_service` entry the monitor can confidently auto-link a detected exe to
/// (i.e. create a `ProcessMapping` for without asking the user) — resolved by exact Steam
/// appid only. Wishlist entries are deliberately excluded: there's no way to log hours against
/// a wishlist item, so `game_type` here is always "regular"/"session"/"live".
#[derive(Debug, Clone)]
pub struct ResolvedLibraryGame {
    pub id: i32,
    pub game_type: String,
    pub title: String,
    /// The `games` row's status ("Completed", "DNF", "In Progress", etc.) at the time the
    /// library index was last refreshed. `None` for live-service entries, which have no
    /// finished state. Used to decide whether relaunching this appid should silently resume
    /// tracking, or should be treated as a possible replay (see `is_finished_status`).
    pub status: Option<String>,
}

/// Whether a `games` row's status represents a "done with this" state — relaunching an appid
/// that maps to one of these is more likely to be a deliberate replay than a continuation of
/// the same playthrough, so the monitor should ask rather than silently resume it.
pub fn is_finished_status(status: &Option<String>) -> bool {
    matches!(status.as_deref().map(str::to_lowercase).as_deref(), Some("completed") | Some("dnf"))
}

/// Index of everything already in the user's FrogLog library, built once and refreshed
/// periodically. Three queries: `contains` (is this title/appid known at all, informational),
/// `resolve_by_appid` (can we confidently auto-link this exe to an existing trackable entry —
/// only true for an exact appid match against `games`/`live_service`), and `resolve_by_id`
/// (what's the current state of a `games` row a `ProcessMapping` already points at, regardless
/// of whether it has a Steam appid at all — used to decide whether an *existing* mapping should
/// still be trusted, see `monitor::check_mapped_game_needs_replay_prompt`).
#[derive(Debug, Default, Clone)]
pub struct LibraryIndex {
    steam_appids: HashSet<String>,
    normalized_titles: HashSet<String>,
    by_appid: HashMap<String, ResolvedLibraryGame>,
    by_id: HashMap<i32, ResolvedLibraryGame>,
}

impl LibraryIndex {
    pub fn build(games: &[Game], wishlist: &[WishlistItem], live_service: &[LiveServiceGame]) -> Self {
        let mut steam_appids = HashSet::new();
        let mut normalized_titles = HashSet::new();
        let mut by_appid = HashMap::new();
        let mut by_id = HashMap::new();

        // Keeps the highest-`id` (most recently created) row for a given key, regardless of
        // what order `games`/`live_service` happen to arrive in -- `GET /games` sorts by
        // `start_date DESC`, so a replay's fresh entry is actually iterated *before* the older
        // entry it replayed, and a plain `insert` would let that older row win the overwrite
        // and silently become "the" entry for a shared appid (see `resolve_by_appid`).
        fn insert_newest(map: &mut HashMap<String, ResolvedLibraryGame>, key: String, candidate: ResolvedLibraryGame) {
            match map.get(&key) {
                Some(existing) if existing.id >= candidate.id => {}
                _ => {
                    map.insert(key, candidate);
                }
            }
        }

        for g in games {
            if let Some(title) = &g.title {
                normalized_titles.insert(normalize_title(title));
                let game_type = if g.session_tracking.unwrap_or(false) { "session" } else { "regular" };
                by_id.insert(g.id, ResolvedLibraryGame { id: g.id, game_type: game_type.to_string(), title: title.clone(), status: g.status.clone() });
            }
            if let Some(appid) = steam_app_id_str(&g.steam_app_id) {
                steam_appids.insert(appid.clone());
                if let Some(title) = &g.title {
                    let game_type = if g.session_tracking.unwrap_or(false) { "session" } else { "regular" };
                    insert_newest(&mut by_appid, appid, ResolvedLibraryGame { id: g.id, game_type: game_type.to_string(), title: title.clone(), status: g.status.clone() });
                }
            }
        }
        for w in wishlist {
            if let Some(appid) = steam_app_id_str(&w.steam_app_id) {
                steam_appids.insert(appid);
            }
            if let Some(title) = &w.title {
                normalized_titles.insert(normalize_title(title));
            }
        }
        for l in live_service {
            if let Some(title) = &l.title {
                normalized_titles.insert(normalize_title(title));
            }
            if let Some(appid) = steam_app_id_str(&l.steam_app_id) {
                steam_appids.insert(appid.clone());
                if let Some(title) = &l.title {
                    insert_newest(&mut by_appid, appid, ResolvedLibraryGame { id: l.id, game_type: "live".to_string(), title: title.clone(), status: None });
                }
            }
        }

        Self { steam_appids, normalized_titles, by_appid, by_id }
    }

    pub fn contains(&self, appid: &str, title: &str) -> bool {
        self.steam_appids.contains(appid) || self.normalized_titles.contains(&normalize_title(title))
    }

    pub fn resolve_by_appid(&self, appid: &str) -> Option<&ResolvedLibraryGame> {
        self.by_appid.get(appid)
    }

    /// Current state of a `games` row by its FrogLog id, regardless of whether it has a Steam
    /// appid at all -- unlike `resolve_by_appid`, this covers every `games` entry (manually
    /// added games included), since an existing `ProcessMapping` can point at any of them.
    /// Never resolves live-service or wishlist entries (`ProcessMapping`s of type "live" have no
    /// finished state to check, and wishlist entries aren't trackable at all).
    pub fn resolve_by_id(&self, id: i32) -> Option<&ResolvedLibraryGame> {
        self.by_id.get(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_edition_suffixes_and_punctuation() {
        assert_eq!(normalize_title("Portal 2"), "portal 2");
        assert_eq!(normalize_title("Hollow Knight: Voidheart Edition"), "hollow knight voidheart edition");
        assert_eq!(normalize_title("Some Game - Definitive Edition"), "some game");
        assert_eq!(normalize_title("Some Game (GOTY Edition)"), "some game");
    }

    fn sample_game(id: i32, title: &str, appid: i64, session_tracking: bool) -> Game {
        sample_game_with_status(id, title, appid, session_tracking, None)
    }

    fn sample_game_with_status(id: i32, title: &str, appid: i64, session_tracking: bool, status: Option<&str>) -> Game {
        Game {
            id,
            title: Some(title.to_string()),
            hours_played: None,
            description: None,
            img: None,
            platform: None,
            genre: None,
            dev: None,
            studio_country: None,
            start_date: None,
            end_date: None,
            rating: None,
            dnf: None,
            is_public: None,
            rel_date: None,
            status: status.map(|s| s.to_string()),
            forklift_certified: None,
            session_tracking: Some(session_tracking),
            steam_app_id: Some(serde_json::json!(appid)),
        }
    }

    #[test]
    fn matches_by_appid_before_title() {
        let games = vec![sample_game(1, "Some Other Name", 620, false)];
        let index = LibraryIndex::build(&games, &[], &[]);
        assert!(index.contains("620", "Totally Different Title"));
        assert!(!index.contains("999", "Totally Different Title"));
    }

    #[test]
    fn resolves_trackable_game_by_appid_only() {
        let games = vec![sample_game(42, "Hades", 1145360, true)];
        let index = LibraryIndex::build(&games, &[], &[]);
        let resolved = index.resolve_by_appid("1145360").unwrap();
        assert_eq!(resolved.id, 42);
        assert_eq!(resolved.game_type, "session");
        assert_eq!(resolved.title, "Hades");
        assert!(index.resolve_by_appid("999").is_none());
    }

    #[test]
    fn resolve_by_appid_prefers_the_highest_id_when_a_replay_shares_the_appid() {
        // `GET /games` sorts by start_date DESC, so a fresh replay's row (higher id, more
        // recent start_date) is iterated *before* the old finished entry it replayed -- this
        // must not let the older row win just because it happens to be processed last.
        let games = vec![
            sample_game_with_status(7, "Metal Gear Rising: Revengeance", 235460, true, Some("Completed")),
            sample_game_with_status(1, "Metal Gear Rising: Revengeance", 235460, false, Some("Completed")),
        ];
        let index = LibraryIndex::build(&games, &[], &[]);
        let resolved = index.resolve_by_appid("235460").unwrap();
        assert_eq!(resolved.id, 7);
    }

    #[test]
    fn resolve_by_appid_ignores_input_order() {
        // Same games as above, reversed -- the result must be identical either way.
        let games = vec![
            sample_game_with_status(1, "Metal Gear Rising: Revengeance", 235460, false, Some("Completed")),
            sample_game_with_status(7, "Metal Gear Rising: Revengeance", 235460, true, Some("Completed")),
        ];
        let index = LibraryIndex::build(&games, &[], &[]);
        let resolved = index.resolve_by_appid("235460").unwrap();
        assert_eq!(resolved.id, 7);
    }

    #[test]
    fn resolve_by_id_works_without_a_steam_appid() {
        let games = vec![Game {
            id: 99,
            title: Some("Manually Added Game".to_string()),
            hours_played: None,
            description: None,
            img: None,
            platform: None,
            genre: None,
            dev: None,
            studio_country: None,
            start_date: None,
            end_date: None,
            rating: None,
            dnf: None,
            is_public: None,
            rel_date: None,
            status: Some("Completed".to_string()),
            forklift_certified: None,
            session_tracking: Some(false),
            steam_app_id: None,
        }];
        let index = LibraryIndex::build(&games, &[], &[]);
        let resolved = index.resolve_by_id(99).unwrap();
        assert_eq!(resolved.title, "Manually Added Game");
        assert!(is_finished_status(&resolved.status));
        assert!(index.resolve_by_id(100).is_none());
    }

    #[test]
    fn resolved_game_carries_finished_status() {
        let games = vec![sample_game_with_status(7, "Metal Gear Rising: Revengeance", 235460, false, Some("Completed"))];
        let index = LibraryIndex::build(&games, &[], &[]);
        let resolved = index.resolve_by_appid("235460").unwrap();
        assert!(is_finished_status(&resolved.status));
    }

    #[test]
    fn is_finished_status_matches_completed_and_dnf_case_insensitively() {
        assert!(is_finished_status(&Some("Completed".to_string())));
        assert!(is_finished_status(&Some("dnf".to_string())));
        assert!(!is_finished_status(&Some("In Progress".to_string())));
        assert!(!is_finished_status(&Some("Dormant".to_string())));
        assert!(!is_finished_status(&None));
    }

    #[test]
    fn wishlist_entries_are_not_resolvable_for_auto_link() {
        let wishlist = vec![WishlistItem {
            id: 5,
            title: Some("Wishlisted Game".to_string()),
            steam_app_id: Some(serde_json::json!(111)),
        }];
        let index = LibraryIndex::build(&[], &wishlist, &[]);
        assert!(index.contains("111", "anything"));
        assert!(index.resolve_by_appid("111").is_none());
    }
}
