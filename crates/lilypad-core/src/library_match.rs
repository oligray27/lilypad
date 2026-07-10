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
}

/// Index of everything already in the user's FrogLog library, built once and refreshed
/// periodically. Two queries: `contains` (is this title/appid known at all, informational),
/// and `resolve_by_appid` (can we confidently auto-link this exe to an existing trackable
/// entry — only true for an exact appid match against `games`/`live_service`).
#[derive(Debug, Default, Clone)]
pub struct LibraryIndex {
    steam_appids: HashSet<String>,
    normalized_titles: HashSet<String>,
    by_appid: HashMap<String, ResolvedLibraryGame>,
}

impl LibraryIndex {
    pub fn build(games: &[Game], wishlist: &[WishlistItem], live_service: &[LiveServiceGame]) -> Self {
        let mut steam_appids = HashSet::new();
        let mut normalized_titles = HashSet::new();
        let mut by_appid = HashMap::new();

        for g in games {
            if let Some(title) = &g.title {
                normalized_titles.insert(normalize_title(title));
            }
            if let Some(appid) = steam_app_id_str(&g.steam_app_id) {
                steam_appids.insert(appid.clone());
                if let Some(title) = &g.title {
                    let game_type = if g.session_tracking.unwrap_or(false) { "session" } else { "regular" };
                    by_appid.insert(appid, ResolvedLibraryGame { id: g.id, game_type: game_type.to_string(), title: title.clone() });
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
                    by_appid.insert(appid, ResolvedLibraryGame { id: l.id, game_type: "live".to_string(), title: title.clone() });
                }
            }
        }

        Self { steam_appids, normalized_titles, by_appid }
    }

    pub fn contains(&self, appid: &str, title: &str) -> bool {
        self.steam_appids.contains(appid) || self.normalized_titles.contains(&normalize_title(title))
    }

    pub fn resolve_by_appid(&self, appid: &str) -> Option<&ResolvedLibraryGame> {
        self.by_appid.get(appid)
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
            status: None,
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
