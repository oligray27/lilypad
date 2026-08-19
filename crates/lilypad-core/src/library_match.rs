//! Matching a detected game (Steam appid + title) against the user's existing FrogLog
//! library (`games` + `wishlist`), to decide whether it's worth notifying about.

use crate::api::{Game, LiveServiceGame, PlatformLink, WishlistItem};
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

fn igdb_id_i64(v: &Option<serde_json::Value>) -> Option<i64> {
    v.as_ref().and_then(|v| v.as_i64())
}

/// Resolves a `games`/`live_service_games` row's Steam appid, preferring the
/// `game_platform_links`-shaped `steam` entry over the legacy flat `steam_app_id` scalar
/// column when both are present. Game-identity redesign, Phase 2's "port
/// platform_key/external_id matching" — this crate previously read `steam_app_id` only,
/// which will silently stop resolving anything the moment the backend's eventual Phase 3
/// cutover drops that column from `games` (see `Game::platform_links`'s doc comment).
/// Reading `platform_links` first, with the scalar column as a fallback, means this
/// crate keeps working across that cutover without needing a synchronized LilyPad release.
fn resolve_steam_appid(steam_app_id: &Option<serde_json::Value>, platform_links: &Option<Vec<PlatformLink>>) -> Option<String> {
    if let Some(links) = platform_links {
        if let Some(link) = links.iter().find(|l| l.platform_key == "steam") {
            if let Some(id) = steam_app_id_str(&link.external_id) {
                return Some(id);
            }
        }
    }
    steam_app_id_str(steam_app_id)
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
    /// This row's `igdb_id`, when known — carried through so a caller that resolved via
    /// `resolve_by_igdb_id` doesn't need a second lookup just to get it back.
    pub igdb_id: Option<i64>,
}

/// Whether a `games` row's status represents a "done with this" state — relaunching an appid
/// that maps to one of these is more likely to be a deliberate replay than a continuation of
/// the same playthrough, so the monitor should ask rather than silently resume it.
pub fn is_finished_status(status: &Option<String>) -> bool {
    matches!(status.as_deref().map(str::to_lowercase).as_deref(), Some("completed") | Some("dnf"))
}

/// Index of everything already in the user's FrogLog library, built once and refreshed
/// periodically. Four queries: `contains` (is this title/appid known at all, informational),
/// `resolve_by_appid` (can we confidently auto-link this exe to an existing trackable entry —
/// only true for an exact appid match against `games`/`live_service`), `resolve_by_id`
/// (what's the current state of a `games` row a `ProcessMapping` already points at, regardless
/// of whether it has a Steam appid at all — used to decide whether an *existing* mapping should
/// still be trusted, see `monitor::check_mapped_game_needs_replay_prompt`), and
/// `resolve_by_igdb_id` (same idea as `resolve_by_appid`, but keyed on IGDB's numeric id
/// instead of a Steam appid — this crate's own detection never produces an igdb_id on its
/// own, so this tier only ever gets consulted from a flow that already fetched one over the
/// network for another reason, e.g. `resolve_pending_game_as_new`'s post-search-confirm check).
#[derive(Debug, Default, Clone)]
pub struct LibraryIndex {
    steam_appids: HashSet<String>,
    normalized_titles: HashSet<String>,
    by_appid: HashMap<String, ResolvedLibraryGame>,
    by_id: HashMap<i32, ResolvedLibraryGame>,
    by_igdb_id: HashMap<i64, ResolvedLibraryGame>,
}

impl LibraryIndex {
    pub fn build(games: &[Game], wishlist: &[WishlistItem], live_service: &[LiveServiceGame]) -> Self {
        let mut steam_appids = HashSet::new();
        let mut normalized_titles = HashSet::new();
        let mut by_appid = HashMap::new();
        let mut by_id = HashMap::new();
        let mut by_igdb_id = HashMap::new();

        // Ranks candidates for a shared appid/titleId by status rather than just "most
        // recently created" -- two (or more) rows can legitimately share the same platform
        // id (e.g. the FrogLog import conflict UI's "Create new entry" choice), and an
        // actively in-progress entry should always win auto-tracking over some other row
        // that merely happens to share the id, regardless of which was created later.
        // Mirrors the same tie-break in the backend's findLibraryGameBySteamAppId
        // (steamLibraryMatch.js) et al.: In Progress > Dormant > Imported > blank/unset,
        // then session-tracked before not, then highest id last.
        fn status_priority(status: &Option<String>) -> u8 {
            match status.as_deref().map(str::to_lowercase).as_deref() {
                Some("in progress") => 1,
                Some("dormant") => 2,
                Some("imported") => 3,
                _ => 4,
            }
        }

        fn is_preferred(candidate: &ResolvedLibraryGame, existing: &ResolvedLibraryGame) -> bool {
            let candidate_priority = status_priority(&candidate.status);
            let existing_priority = status_priority(&existing.status);
            if candidate_priority != existing_priority {
                return candidate_priority < existing_priority;
            }
            let candidate_tracked = candidate.game_type == "session";
            let existing_tracked = existing.game_type == "session";
            if candidate_tracked != existing_tracked {
                return candidate_tracked && !existing_tracked;
            }
            candidate.id > existing.id
        }

        // `GET /games` sorts by `start_date DESC`, so a replay's fresh entry is actually
        // iterated *before* the older entry it replayed -- a plain `insert` would let that
        // older row win the overwrite and silently become "the" entry for a shared appid/
        // igdb_id (see `resolve_by_appid`/`resolve_by_igdb_id`), so every candidate is ranked
        // via `is_preferred` above regardless of iteration order. Generic over the key type so
        // the same ranking logic backs both `by_appid` (String) and `by_igdb_id` (i64).
        fn insert_preferred<K: std::hash::Hash + Eq>(map: &mut HashMap<K, ResolvedLibraryGame>, key: K, candidate: ResolvedLibraryGame) {
            match map.get(&key) {
                Some(existing) if !is_preferred(&candidate, existing) => {}
                _ => {
                    map.insert(key, candidate);
                }
            }
        }

        for g in games {
            let igdb_id = igdb_id_i64(&g.igdb_id);
            if let Some(title) = &g.title {
                normalized_titles.insert(normalize_title(title));
                let game_type = if g.session_tracking.unwrap_or(false) { "session" } else { "regular" };
                by_id.insert(g.id, ResolvedLibraryGame { id: g.id, game_type: game_type.to_string(), title: title.clone(), status: g.status.clone(), igdb_id });
            }
            if let Some(appid) = resolve_steam_appid(&g.steam_app_id, &g.platform_links) {
                steam_appids.insert(appid.clone());
                if let Some(title) = &g.title {
                    let game_type = if g.session_tracking.unwrap_or(false) { "session" } else { "regular" };
                    insert_preferred(&mut by_appid, appid, ResolvedLibraryGame { id: g.id, game_type: game_type.to_string(), title: title.clone(), status: g.status.clone(), igdb_id });
                }
            }
            if let (Some(iid), Some(title)) = (igdb_id, &g.title) {
                let game_type = if g.session_tracking.unwrap_or(false) { "session" } else { "regular" };
                insert_preferred(&mut by_igdb_id, iid, ResolvedLibraryGame { id: g.id, game_type: game_type.to_string(), title: title.clone(), status: g.status.clone(), igdb_id });
            }
        }
        for w in wishlist {
            if let Some(appid) = steam_app_id_str(&w.steam_app_id) {
                steam_appids.insert(appid);
            }
            if let Some(title) = &w.title {
                normalized_titles.insert(normalize_title(title));
            }
            // No by_igdb_id entry for wishlist -- same reasoning as by_appid always excluding
            // it: there's no way to log hours (or resolve a replay) against a wishlist item.
        }
        for l in live_service {
            let igdb_id = igdb_id_i64(&l.igdb_id);
            if let Some(title) = &l.title {
                normalized_titles.insert(normalize_title(title));
            }
            // Live-service platform-linking (2026-08-19): reads platform_links first now,
            // same as the `games` loop above via resolve_steam_appid -- previously this
            // called steam_app_id_str(&l.steam_app_id) directly, since LiveServiceGame had
            // no platform_links field to prefer at all. Forward-compat with the eventual
            // drop of live_service_games.steam_app_id, mirroring why the `games` side
            // already reads this way.
            if let Some(appid) = resolve_steam_appid(&l.steam_app_id, &l.platform_links) {
                steam_appids.insert(appid.clone());
                if let Some(title) = &l.title {
                    insert_preferred(&mut by_appid, appid, ResolvedLibraryGame { id: l.id, game_type: "live".to_string(), title: title.clone(), status: None, igdb_id });
                }
            }
            if let (Some(iid), Some(title)) = (igdb_id, &l.title) {
                insert_preferred(&mut by_igdb_id, iid, ResolvedLibraryGame { id: l.id, game_type: "live".to_string(), title: title.clone(), status: None, igdb_id });
            }
        }

        Self { steam_appids, normalized_titles, by_appid, by_id, by_igdb_id }
    }

    pub fn contains(&self, appid: &str, title: &str) -> bool {
        self.steam_appids.contains(appid) || self.normalized_titles.contains(&normalize_title(title))
    }

    pub fn resolve_by_appid(&self, appid: &str) -> Option<&ResolvedLibraryGame> {
        self.by_appid.get(appid)
    }

    /// igdb_id match, any status -- mirrors the backend's `findGameByIgdbId`
    /// (`gameIdentity.js`). Unlike `resolve_by_appid`, this crate's own detection never
    /// produces an igdb_id on its own (Steam manifests and watched-directory folders give a
    /// Steam appid or a synthetic local id, never an IGDB id), so this is only ever useful
    /// from a caller that already resolved one over the network for its own reasons --
    /// currently just `resolve_pending_game_as_new`'s post-IGDB-search-confirm check, never
    /// the ambient per-poll-tick detection loop (ambient detection making its own IGDB calls
    /// is exactly what the game-identity redesign plan's LilyPad note says not to do).
    pub fn resolve_by_igdb_id(&self, igdb_id: i64) -> Option<&ResolvedLibraryGame> {
        self.by_igdb_id.get(&igdb_id)
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
        sample_game_full(id, title, Some(appid), session_tracking, status, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn sample_game_full(
        id: i32,
        title: &str,
        appid: Option<i64>,
        session_tracking: bool,
        status: Option<&str>,
        igdb_id: Option<i64>,
        platform_links: Option<Vec<PlatformLink>>,
    ) -> Game {
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
            steam_app_id: appid.map(|a| serde_json::json!(a)),
            igdb_id: igdb_id.map(|i| serde_json::json!(i)),
            platform_links,
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
    fn resolve_by_appid_prefers_in_progress_over_a_higher_id_imported_duplicate() {
        // The exact scenario that motivated the status-priority tie-break: a manually
        // tracked, actively in-progress entry (lower id) must keep winning auto-tracking
        // over a freshly created duplicate that merely happens to share the same appid
        // (e.g. from the FrogLog import conflict UI's "Create new entry" choice) with a
        // higher id but a less-committed "Imported" status.
        let games = vec![
            sample_game_with_status(9298, "Stellar Blade", 3489700, false, Some("In Progress")),
            sample_game_with_status(9536, "Stellar Blade", 3489700, false, Some("Imported")),
        ];
        let index = LibraryIndex::build(&games, &[], &[]);
        let resolved = index.resolve_by_appid("3489700").unwrap();
        assert_eq!(resolved.id, 9298);
    }

    #[test]
    fn resolve_by_id_works_without_a_steam_appid() {
        let games = vec![sample_game_full(99, "Manually Added Game", None, false, Some("Completed"), None, None)];
        let index = LibraryIndex::build(&games, &[], &[]);
        let resolved = index.resolve_by_id(99).unwrap();
        assert_eq!(resolved.title, "Manually Added Game");
        assert!(is_finished_status(&resolved.status));
        assert!(index.resolve_by_id(100).is_none());
    }

    #[test]
    fn resolves_by_igdb_id() {
        let games = vec![sample_game_full(5, "Hades II", Some(1145360), true, Some("In Progress"), Some(52014), None)];
        let index = LibraryIndex::build(&games, &[], &[]);
        let resolved = index.resolve_by_igdb_id(52014).unwrap();
        assert_eq!(resolved.id, 5);
        assert_eq!(resolved.game_type, "session");
        assert!(!is_finished_status(&resolved.status));
        assert!(index.resolve_by_igdb_id(999999).is_none());
    }

    #[test]
    fn steam_appid_resolves_from_platform_links_with_no_scalar_column_at_all() {
        // Forward-compat with the game-identity redesign's eventual Phase 3 cutover
        // (dropping `games.steam_app_id` entirely) -- this must keep resolving purely off
        // `platform_links` once the scalar column stops being sent at all.
        let games = vec![sample_game_full(
            8,
            "Celeste",
            None,
            true,
            Some("In Progress"),
            None,
            Some(vec![PlatformLink { platform_key: "steam".to_string(), external_id: Some(serde_json::json!("504230")), label: None }]),
        )];
        let index = LibraryIndex::build(&games, &[], &[]);
        let resolved = index.resolve_by_appid("504230").unwrap();
        assert_eq!(resolved.id, 8);
    }

    #[test]
    fn steam_appid_prefers_platform_links_over_the_legacy_scalar_when_both_present() {
        let games = vec![sample_game_full(
            9,
            "Celeste",
            Some(999999), // legacy scalar -- should lose
            true,
            Some("In Progress"),
            None,
            Some(vec![PlatformLink { platform_key: "steam".to_string(), external_id: Some(serde_json::json!("504230")), label: None }]),
        )];
        let index = LibraryIndex::build(&games, &[], &[]);
        assert!(index.resolve_by_appid("504230").is_some());
        assert!(index.resolve_by_appid("999999").is_none());
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
            igdb_id: None,
        }];
        let index = LibraryIndex::build(&[], &wishlist, &[]);
        assert!(index.contains("111", "anything"));
        assert!(index.resolve_by_appid("111").is_none());
    }

    fn sample_live_service(id: i32, title: &str, appid: Option<i64>, platform_links: Option<Vec<PlatformLink>>) -> LiveServiceGame {
        LiveServiceGame {
            id,
            title: Some(title.to_string()),
            total_hours: None,
            session_count: None,
            last_session_date: None,
            steam_app_id: appid.map(|a| serde_json::json!(a)),
            igdb_id: None,
            platform_links,
        }
    }

    // Live-service platform-linking (2026-08-19) -- mirrors the two `games`-side tests
    // above (steam_appid_resolves_from_platform_links_with_no_scalar_column_at_all /
    // steam_appid_prefers_platform_links_over_the_legacy_scalar_when_both_present)
    // exactly, now that LiveServiceGame carries the same platform_links field.
    #[test]
    fn live_service_steam_appid_resolves_from_platform_links_with_no_scalar_column_at_all() {
        let live_service = vec![sample_live_service(
            30,
            "Destiny 2",
            None,
            Some(vec![PlatformLink { platform_key: "steam".to_string(), external_id: Some(serde_json::json!("1085660")), label: None }]),
        )];
        let index = LibraryIndex::build(&[], &[], &live_service);
        let resolved = index.resolve_by_appid("1085660").unwrap();
        assert_eq!(resolved.id, 30);
        assert_eq!(resolved.game_type, "live");
    }

    #[test]
    fn live_service_steam_appid_prefers_platform_links_over_the_legacy_scalar_when_both_present() {
        let live_service = vec![sample_live_service(
            31,
            "Destiny 2",
            Some(999999), // legacy scalar -- should lose
            Some(vec![PlatformLink { platform_key: "steam".to_string(), external_id: Some(serde_json::json!("1085660")), label: None }]),
        )];
        let index = LibraryIndex::build(&[], &[], &live_service);
        assert!(index.resolve_by_appid("1085660").is_some());
        assert!(index.resolve_by_appid("999999").is_none());
    }

    #[test]
    fn live_service_steam_appid_still_resolves_from_the_legacy_scalar_alone() {
        // No platform_links at all (a not-yet-upgraded backend, or an LS row nothing has
        // dual-written yet) -- must keep working exactly as before this change.
        let live_service = vec![sample_live_service(32, "Fortnite", Some(1172470), None)];
        let index = LibraryIndex::build(&[], &[], &live_service);
        let resolved = index.resolve_by_appid("1172470").unwrap();
        assert_eq!(resolved.id, 32);
        assert_eq!(resolved.game_type, "live");
    }
}
