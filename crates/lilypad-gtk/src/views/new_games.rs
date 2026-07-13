//! New Games: games LilyPad has detected being played that aren't in the user's FrogLog
//! library yet (`PendingGameSubmission`). Each entry can be resolved by creating a brand-new
//! FrogLog game (IGDB search, with an automatic Steam-appid lookup shortcut) or by logging the
//! accumulated hours against an existing game/live-service entry, or simply dismissed. Ported
//! from the Tauri build's New Games view (`loadNewGamesView()` in `src/main.js`).

use crate::resolve;
use crate::session_flow::client_for;
use crate::state::AppState;
use crate::tray::RefreshTray;
use adw::prelude::*;
use lilypad_core::config::{load_pending_game_submissions, remove_pending_game_submission, PendingGameSubmission, ReplayOf};
use lilypad_core::library_match::normalize_title;
use std::cell::RefCell;
use std::rc::Rc;

/// Best-effort pre-selection for the "Map to Existing" dropdown — exact normalized-title match
/// first, then substring containment either direction. Just a starting point for the user to
/// confirm, not authoritative. Mirrors the Tauri build's `guessBestMatch` in `main.js`.
fn guess_best_match<'a>(pending_title: &str, rows: &'a [(String, i32, String)]) -> Option<&'a (String, i32, String)> {
    let target = normalize_title(pending_title);
    if target.is_empty() {
        return None;
    }
    if let Some(exact) = rows.iter().find(|(_, _, title)| normalize_title(title) == target) {
        return Some(exact);
    }
    rows.iter().find(|(_, _, title)| {
        let rt = normalize_title(title);
        !rt.is_empty() && (rt.contains(&target) || target.contains(&rt))
    })
}

/// Builds the New Games view. Returns the widget and a `reload` closure the caller should
/// invoke each time the view is shown.
pub fn build(state: AppState, refresh_tray: RefreshTray) -> (gtk4::Widget, Rc<dyn Fn()>) {
    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    container.set_margin_top(18);
    container.set_margin_bottom(18);
    container.set_margin_start(18);
    container.set_margin_end(18);

    let title = gtk4::Label::new(Some("New Games"));
    title.add_css_class("title-1");
    title.set_halign(gtk4::Align::Start);
    container.append(&title);

    let desc = gtk4::Label::new(Some(
        "LilyPad noticed you playing these, but they aren't in your FrogLog library yet. \
         Create a new entry, log the hours against a game you already have, or dismiss.",
    ));
    desc.set_wrap(true);
    desc.set_halign(gtk4::Align::Start);
    desc.add_css_class("dim-label");
    container.append(&desc);

    let empty_label = gtk4::Label::new(Some("No games detected outside FrogLog."));
    empty_label.add_css_class("dim-label");
    empty_label.set_margin_top(24);
    empty_label.set_visible(false);
    container.append(&empty_label);

    let list_box = gtk4::ListBox::new();
    list_box.set_selection_mode(gtk4::SelectionMode::None);
    list_box.add_css_class("boxed-list");
    let scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&list_box)
        .build();
    container.append(&scroller);

    // `reload` is defined recursively (rows need to call it after resolve/dismiss), so it's
    // built up via a RefCell cell that gets filled in immediately after (same pattern as
    // views/pending.rs).
    let reload_cell: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

    let reload: Rc<dyn Fn()> = {
        let state = state.clone();
        let refresh_tray = refresh_tray.clone();
        let list_box = list_box.clone();
        let scroller = scroller.clone();
        let empty_label = empty_label.clone();
        let reload_cell = Rc::clone(&reload_cell);
        Rc::new(move || {
            while let Some(child) = list_box.first_child() {
                list_box.remove(&child);
            }
            let items = load_pending_game_submissions();
            let is_empty = items.is_empty();
            empty_label.set_visible(is_empty);
            scroller.set_visible(!is_empty);

            for entry in items {
                let on_changed = {
                    let reload_cell = Rc::clone(&reload_cell);
                    let refresh_tray = refresh_tray.clone();
                    move || {
                        if let Some(reload) = reload_cell.borrow().as_ref() {
                            reload();
                        }
                        refresh_tray();
                    }
                };
                let row = build_row(state.clone(), entry, Rc::new(on_changed));
                list_box.append(&row);
            }
        })
    };
    *reload_cell.borrow_mut() = Some(Rc::clone(&reload));

    (container.upcast(), reload)
}

fn build_row(state: AppState, entry: PendingGameSubmission, on_changed: Rc<dyn Fn()>) -> gtk4::Box {
    if let Some(replay_of) = entry.replay_of.clone() {
        return build_replay_row(state, entry, replay_of, on_changed);
    }

    let row = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    row.set_margin_top(10);
    row.set_margin_bottom(10);
    row.set_margin_start(12);
    row.set_margin_end(12);

    let title_label = gtk4::Label::new(Some(&entry.title));
    title_label.add_css_class("heading");
    title_label.set_halign(gtk4::Align::Start);
    title_label.set_wrap(true);
    row.append(&title_label);

    let meta_label = gtk4::Label::new(Some(&format!(
        "{}h · {} session{}",
        entry.hours,
        entry.session_count,
        if entry.session_count == 1 { "" } else { "s" }
    )));
    meta_label.add_css_class("dim-label");
    meta_label.set_halign(gtk4::Align::Start);
    row.append(&meta_label);

    let status_label = gtk4::Label::new(None);
    status_label.add_css_class("dim-label");
    status_label.set_halign(gtk4::Align::Start);
    status_label.set_visible(false);
    row.append(&status_label);

    let actions_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let create_btn = gtk4::Button::with_label("Create New");
    let map_btn = gtk4::Button::with_label("Map to Existing");
    let dismiss_btn = gtk4::Button::with_label("Dismiss");
    actions_box.append(&create_btn);
    actions_box.append(&map_btn);
    actions_box.append(&dismiss_btn);
    row.append(&actions_box);

    // --- Create New panel ---
    let create_panel = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    create_panel.set_visible(false);
    create_panel.set_margin_top(6);
    row.append(&create_panel);

    let lookup_status = gtk4::Label::new(None);
    lookup_status.add_css_class("dim-label");
    lookup_status.set_halign(gtk4::Align::Start);
    lookup_status.set_visible(false);
    create_panel.append(&lookup_status);

    let auto_match_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    auto_match_box.set_visible(false);
    let auto_match_label = gtk4::Label::new(None);
    auto_match_label.set_halign(gtk4::Align::Start);
    auto_match_box.append(&auto_match_label);
    let auto_match_actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let auto_confirm_btn = gtk4::Button::with_label("Create");
    auto_confirm_btn.add_css_class("suggested-action");
    let auto_search_instead_btn = gtk4::Button::with_label("Search Manually Instead");
    auto_match_actions.append(&auto_confirm_btn);
    auto_match_actions.append(&auto_search_instead_btn);
    auto_match_box.append(&auto_match_actions);
    create_panel.append(&auto_match_box);
    // The igdb title the auto-match found, consumed by auto_confirm_btn's handler.
    let auto_match_title: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let manual_search_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    manual_search_box.set_visible(false);
    let search_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let search_entry = gtk4::Entry::builder()
        .placeholder_text("Search title, or appid:12345 for a Steam appid…")
        .hexpand(true)
        .build();
    let search_btn = gtk4::Button::with_label("Search");
    search_row.append(&search_entry);
    search_row.append(&search_btn);
    manual_search_box.append(&search_row);
    let results_combo = gtk4::ComboBoxText::new();
    results_combo.set_visible(false);
    manual_search_box.append(&results_combo);
    let search_confirm_btn = gtk4::Button::with_label("Use Selected");
    search_confirm_btn.add_css_class("suggested-action");
    search_confirm_btn.set_visible(false);
    manual_search_box.append(&search_confirm_btn);
    create_panel.append(&manual_search_box);

    // --- Map to Existing panel ---
    let map_panel = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    map_panel.set_visible(false);
    map_panel.set_margin_top(6);
    let map_combo = gtk4::ComboBoxText::new();
    map_panel.append(&map_combo);
    let map_note = gtk4::Label::new(Some(
        "If the selected game is already marked Completed or DNF, logging hours here will clear its end date and return it to \"In Progress\" status.",
    ));
    map_note.add_css_class("dim-label");
    map_note.add_css_class("caption");
    map_note.set_halign(gtk4::Align::Start);
    map_note.set_wrap(true);
    map_panel.append(&map_note);
    let map_confirm_btn = gtk4::Button::with_label("Log Hours");
    map_confirm_btn.add_css_class("suggested-action");
    map_panel.append(&map_confirm_btn);
    row.append(&map_panel);

    let is_steam_appid = !entry.appid.is_empty() && entry.appid.chars().all(|c| c.is_ascii_digit());

    // --- Create New: opens the panel and, for Steam games, tries an appid-based IGDB lookup
    // before falling back to manual search. ---
    create_btn.connect_clicked({
        let state = state.clone();
        let map_panel = map_panel.clone();
        let create_panel = create_panel.clone();
        let lookup_status = lookup_status.clone();
        let auto_match_box = auto_match_box.clone();
        let auto_match_label = auto_match_label.clone();
        let manual_search_box = manual_search_box.clone();
        let search_entry = search_entry.clone();
        let auto_match_title = Rc::clone(&auto_match_title);
        let appid = entry.appid.clone();
        let pending_title = entry.title.clone();
        move |_| {
            map_panel.set_visible(false);
            let was_hidden = !create_panel.is_visible();
            create_panel.set_visible(was_hidden);
            if !was_hidden {
                return;
            }

            auto_match_box.set_visible(false);
            manual_search_box.set_visible(false);

            if is_steam_appid {
                lookup_status.set_visible(true);
                lookup_status.set_text("Looking up…");

                let auth = state.auth.read().unwrap().clone();
                let query = format!("appid:{appid}");
                let (tx, rx) = async_channel::bounded(1);
                std::thread::spawn(move || {
                    let client = client_for(&auth);
                    let result = client.search_igdb(&query);
                    let _ = tx.send_blocking(result);
                });

                let lookup_status = lookup_status.clone();
                let auto_match_box = auto_match_box.clone();
                let auto_match_label = auto_match_label.clone();
                let manual_search_box = manual_search_box.clone();
                let search_entry = search_entry.clone();
                let auto_match_title = Rc::clone(&auto_match_title);
                let pending_title = pending_title.clone();
                glib::spawn_future_local(async move {
                    let result = rx.recv().await;
                    let found = match result {
                        Ok(Ok(results)) => results.into_iter().next().and_then(|r| {
                            let name = r.get("name").and_then(|v| v.as_str())?.to_string();
                            let year = r
                                .get("released")
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.get(0..4))
                                .map(|y| format!(" ({y})"))
                                .unwrap_or_default();
                            Some((name, year))
                        }),
                        _ => None,
                    };
                    match found {
                        Some((name, year)) => {
                            lookup_status.set_visible(false);
                            auto_match_label.set_text(&format!("Found: {name}{year}"));
                            *auto_match_title.borrow_mut() = Some(name);
                            auto_match_box.set_visible(true);
                        }
                        None => {
                            lookup_status.set_visible(false);
                            manual_search_box.set_visible(true);
                            if search_entry.text().is_empty() {
                                search_entry.set_text(&pending_title);
                            }
                        }
                    }
                });
            } else {
                lookup_status.set_visible(false);
                manual_search_box.set_visible(true);
                if search_entry.text().is_empty() {
                    search_entry.set_text(&pending_title);
                }
            }
        }
    });

    auto_search_instead_btn.connect_clicked({
        let auto_match_box = auto_match_box.clone();
        let manual_search_box = manual_search_box.clone();
        let search_entry = search_entry.clone();
        let pending_title = entry.title.clone();
        move |_| {
            auto_match_box.set_visible(false);
            manual_search_box.set_visible(true);
            if search_entry.text().is_empty() {
                search_entry.set_text(&pending_title);
            }
        }
    });

    auto_confirm_btn.connect_clicked({
        let state = state.clone();
        let status_label = status_label.clone();
        let auto_match_title = Rc::clone(&auto_match_title);
        let appid = entry.appid.clone();
        let on_changed = Rc::clone(&on_changed);
        move |btn| {
            let Some(igdb_title) = auto_match_title.borrow().clone() else { return };
            btn.set_sensitive(false);
            status_label.remove_css_class("error");
            status_label.set_text("Creating…");
            status_label.set_visible(true);

            let state = state.clone();
            let appid = appid.clone();
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let result = resolve::resolve_as_new(&state, &appid, &igdb_title);
                let _ = tx.send_blocking(result);
            });

            let btn = btn.clone();
            let status_label = status_label.clone();
            let on_changed = Rc::clone(&on_changed);
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(_)) => on_changed(),
                    Ok(Err(e)) => {
                        btn.set_sensitive(true);
                        status_label.add_css_class("error");
                        status_label.set_text(&e);
                        status_label.set_visible(true);
                    }
                    Err(_) => {}
                }
            });
        }
    });

    search_btn.connect_clicked({
        let state = state.clone();
        let search_entry = search_entry.clone();
        let results_combo = results_combo.clone();
        let search_confirm_btn = search_confirm_btn.clone();
        move |_| {
            let query = search_entry.text().trim().to_string();
            if query.is_empty() {
                return;
            }
            results_combo.remove_all();
            results_combo.set_visible(true);
            results_combo.append(None, "Searching…");
            results_combo.set_active(Some(0));

            let auth = state.auth.read().unwrap().clone();
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let client = client_for(&auth);
                let result = client.search_igdb(&query);
                let _ = tx.send_blocking(result);
            });

            let results_combo = results_combo.clone();
            let search_confirm_btn = search_confirm_btn.clone();
            glib::spawn_future_local(async move {
                results_combo.remove_all();
                match rx.recv().await {
                    Ok(Ok(results)) if !results.is_empty() => {
                        for r in &results {
                            let Some(name) = r.get("name").and_then(|v| v.as_str()) else { continue };
                            let year = r
                                .get("released")
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.get(0..4))
                                .map(|y| format!(" ({y})"))
                                .unwrap_or_default();
                            results_combo.append(Some(name), &format!("{name}{year}"));
                        }
                        results_combo.set_active(Some(0));
                        search_confirm_btn.set_visible(true);
                    }
                    Ok(Ok(_)) => {
                        results_combo.append(None, "No results");
                        results_combo.set_active(Some(0));
                        search_confirm_btn.set_visible(false);
                    }
                    Ok(Err(e)) => {
                        results_combo.append(None, &e);
                        results_combo.set_active(Some(0));
                        search_confirm_btn.set_visible(false);
                    }
                    Err(_) => {}
                }
            });
        }
    });

    search_confirm_btn.connect_clicked({
        let state = state.clone();
        let results_combo = results_combo.clone();
        let status_label = status_label.clone();
        let appid = entry.appid.clone();
        let on_changed = Rc::clone(&on_changed);
        move |btn| {
            let Some(igdb_title) = results_combo.active_id().map(|s| s.to_string()) else { return };
            btn.set_sensitive(false);
            status_label.remove_css_class("error");
            status_label.set_text("Creating…");
            status_label.set_visible(true);

            let state = state.clone();
            let appid = appid.clone();
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let result = resolve::resolve_as_new(&state, &appid, &igdb_title);
                let _ = tx.send_blocking(result);
            });

            let btn = btn.clone();
            let status_label = status_label.clone();
            let on_changed = Rc::clone(&on_changed);
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(_)) => on_changed(),
                    Ok(Err(e)) => {
                        btn.set_sensitive(true);
                        status_label.add_css_class("error");
                        status_label.set_text(&e);
                        status_label.set_visible(true);
                    }
                    Err(_) => {}
                }
            });
        }
    });

    // --- Map to Existing: lazily loads games + live-service entries into the dropdown the
    // first time it's opened, pre-selecting a best-guess match. ---
    map_btn.connect_clicked({
        let state = state.clone();
        let create_panel = create_panel.clone();
        let map_panel = map_panel.clone();
        let map_combo = map_combo.clone();
        let pending_title = entry.title.clone();
        move |_| {
            create_panel.set_visible(false);
            let was_hidden = !map_panel.is_visible();
            map_panel.set_visible(was_hidden);
            if !was_hidden || map_combo.model().map(|m| m.iter_n_children(None) > 0).unwrap_or(false) {
                return;
            }

            map_combo.append(None, "Loading…");
            map_combo.set_active(Some(0));

            let auth = state.auth.read().unwrap().clone();
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let client = client_for(&auth);
                let result = client
                    .get_games()
                    .and_then(|games| client.get_live_service_games().map(|live| (games, live)));
                let _ = tx.send_blocking(result);
            });

            let map_combo = map_combo.clone();
            let pending_title = pending_title.clone();
            glib::spawn_future_local(async move {
                map_combo.remove_all();
                match rx.recv().await {
                    Ok(Ok((games, live))) => {
                        let rows: Vec<(String, i32, String)> = games
                            .iter()
                            .map(|g| {
                                (
                                    if g.session_tracking.unwrap_or(false) { "session".to_string() } else { "regular".to_string() },
                                    g.id,
                                    g.title.clone().unwrap_or_else(|| format!("#{}", g.id)),
                                )
                            })
                            .chain(live.iter().map(|g| ("live".to_string(), g.id, g.title.clone().unwrap_or_else(|| format!("#{}", g.id)))))
                            .collect();
                        if rows.is_empty() {
                            map_combo.append(None, "No games in your library yet");
                            map_combo.set_active(Some(0));
                            return;
                        }
                        let guess = guess_best_match(&pending_title, &rows).cloned();
                        for (game_type, id, title) in &rows {
                            map_combo.append(Some(&format!("{game_type}:{id}")), title);
                        }
                        match guess {
                            Some((game_type, id, _)) => {
                                map_combo.set_active_id(Some(&format!("{game_type}:{id}")));
                            }
                            None => {
                                map_combo.set_active(Some(0));
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        map_combo.append(None, &e);
                        map_combo.set_active(Some(0));
                    }
                    Err(_) => {}
                }
            });
        }
    });

    map_confirm_btn.connect_clicked({
        let state = state.clone();
        let map_combo = map_combo.clone();
        let status_label = status_label.clone();
        let appid = entry.appid.clone();
        let on_changed = Rc::clone(&on_changed);
        move |btn| {
            let Some(val) = map_combo.active_id().map(|s| s.to_string()) else { return };
            let Some((game_type, id_str)) = val.split_once(':') else { return };
            let Ok(game_id) = id_str.parse::<i32>() else { return };
            let Some(game_title) = map_combo.active_text().map(|s| s.to_string()) else { return };
            let game_type = game_type.to_string();

            btn.set_sensitive(false);
            status_label.remove_css_class("error");
            status_label.set_text("Logging…");
            status_label.set_visible(true);

            let state = state.clone();
            let appid = appid.clone();
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let result = resolve::resolve_as_existing(&state, &appid, &game_type, game_id, &game_title);
                let _ = tx.send_blocking(result);
            });

            let btn = btn.clone();
            let status_label = status_label.clone();
            let on_changed = Rc::clone(&on_changed);
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(())) => on_changed(),
                    Ok(Err(e)) => {
                        btn.set_sensitive(true);
                        status_label.add_css_class("error");
                        status_label.set_text(&e);
                        status_label.set_visible(true);
                    }
                    Err(_) => {}
                }
            });
        }
    });

    dismiss_btn.connect_clicked({
        let appid = entry.appid.clone();
        let on_changed = Rc::clone(&on_changed);
        move |_| {
            remove_pending_game_submission(&appid);
            on_changed();
        }
    });

    row
}

/// Row for a pending submission whose appid matches an existing library entry that's already
/// Completed/DNF (`entry.replay_of`) — LilyPad deliberately didn't auto-resume that entry (see
/// `maybe_start_unmapped_tracking` in `monitor.rs`), so the user picks whether this session
/// belongs to it after all, or is a fresh replay that should get its own entry.
fn build_replay_row(state: AppState, entry: PendingGameSubmission, replay_of: ReplayOf, on_changed: Rc<dyn Fn()>) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    row.set_margin_top(10);
    row.set_margin_bottom(10);
    row.set_margin_start(12);
    row.set_margin_end(12);

    let title_label = gtk4::Label::new(Some(&entry.title));
    title_label.add_css_class("heading");
    title_label.set_halign(gtk4::Align::Start);
    title_label.set_wrap(true);
    row.append(&title_label);

    let status_text = replay_of.status.clone().unwrap_or_else(|| "finished".to_string());
    let meta_label = gtk4::Label::new(Some(&format!(
        "{}h · {} session{}. This game is already marked as {status_text}",
        entry.hours,
        entry.session_count,
        if entry.session_count == 1 { "" } else { "s" }
    )));
    meta_label.add_css_class("dim-label");
    meta_label.set_halign(gtk4::Align::Start);
    meta_label.set_wrap(true);
    row.append(&meta_label);

    let continue_note = gtk4::Label::new(Some("Logging to the most recent entry will remove its end date and return it to \"In Progress\" status."));
    continue_note.add_css_class("dim-label");
    continue_note.add_css_class("caption");
    continue_note.set_halign(gtk4::Align::Start);
    continue_note.set_wrap(true);
    row.append(&continue_note);

    let status_label = gtk4::Label::new(None);
    status_label.add_css_class("dim-label");
    status_label.set_halign(gtk4::Align::Start);
    status_label.set_visible(false);
    row.append(&status_label);

    let actions_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let continue_btn = gtk4::Button::with_label("Log to most recent entry");
    continue_btn.add_css_class("suggested-action");
    let replay_btn = gtk4::Button::with_label("Create and log to new entry (replay)");
    let dismiss_btn = gtk4::Button::with_label("Dismiss");
    actions_box.append(&continue_btn);
    actions_box.append(&replay_btn);
    actions_box.append(&dismiss_btn);
    row.append(&actions_box);

    continue_btn.connect_clicked({
        let state = state.clone();
        let status_label = status_label.clone();
        let appid = entry.appid.clone();
        let replay_of = replay_of.clone();
        let on_changed = Rc::clone(&on_changed);
        let continue_btn = continue_btn.clone();
        let replay_btn = replay_btn.clone();
        move |_| {
            continue_btn.set_sensitive(false);
            replay_btn.set_sensitive(false);
            status_label.remove_css_class("error");
            status_label.set_text("Logging…");
            status_label.set_visible(true);

            let state = state.clone();
            let appid = appid.clone();
            let replay_of = replay_of.clone();
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let result = resolve::resolve_as_existing(&state, &appid, &replay_of.game_type, replay_of.id, &replay_of.title);
                let _ = tx.send_blocking(result);
            });

            let continue_btn = continue_btn.clone();
            let replay_btn = replay_btn.clone();
            let status_label = status_label.clone();
            let on_changed = Rc::clone(&on_changed);
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(())) => on_changed(),
                    Ok(Err(e)) => {
                        continue_btn.set_sensitive(true);
                        replay_btn.set_sensitive(true);
                        status_label.add_css_class("error");
                        status_label.set_text(&e);
                        status_label.set_visible(true);
                    }
                    Err(_) => {}
                }
            });
        }
    });

    replay_btn.connect_clicked({
        let state = state.clone();
        let status_label = status_label.clone();
        let appid = entry.appid.clone();
        let on_changed = Rc::clone(&on_changed);
        let continue_btn = continue_btn.clone();
        let replay_btn = replay_btn.clone();
        move |_| {
            continue_btn.set_sensitive(false);
            replay_btn.set_sensitive(false);
            status_label.remove_css_class("error");
            status_label.set_text("Creating…");
            status_label.set_visible(true);

            let state = state.clone();
            let appid = appid.clone();
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let result = resolve::resolve_as_replay(&state, &appid);
                let _ = tx.send_blocking(result);
            });

            let continue_btn = continue_btn.clone();
            let replay_btn = replay_btn.clone();
            let status_label = status_label.clone();
            let on_changed = Rc::clone(&on_changed);
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(_)) => on_changed(),
                    Ok(Err(e)) => {
                        continue_btn.set_sensitive(true);
                        replay_btn.set_sensitive(true);
                        status_label.add_css_class("error");
                        status_label.set_text(&e);
                        status_label.set_visible(true);
                    }
                    Err(_) => {}
                }
            });
        }
    });

    dismiss_btn.connect_clicked({
        let appid = entry.appid.clone();
        let on_changed = Rc::clone(&on_changed);
        move |_| {
            remove_pending_game_submission(&appid);
            on_changed();
        }
    });

    row
}
