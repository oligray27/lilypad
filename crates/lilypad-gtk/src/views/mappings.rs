use crate::mappings_model::{self, GameRow, MapKey, StagedMappings};
use crate::state::{AppState, DEFAULT_API_URL};
use adw::prelude::*;
use lilypad_core::api::FroglogClient;
use lilypad_core::config::process_map_path_for_auth;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

struct ViewState {
    all_rows: Vec<GameRow>,
    saved: StagedMappings,
    pending: StagedMappings,
    mode: String,
    search: String,
    /// (exe entry, title-filter entry) per key, live only while that row is rendered
    /// (rebuilt whenever the mode/search filter changes) — used to apply conflict
    /// css highlighting without a full list rebuild on every keystroke.
    entry_widgets: HashMap<MapKey, (gtk4::Entry, gtk4::Entry)>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            all_rows: Vec::new(),
            saved: StagedMappings::default(),
            pending: StagedMappings::default(),
            mode: "regular".to_string(),
            search: String::new(),
            entry_widgets: HashMap::new(),
        }
    }
}

/// Builds the mappings view. Returns the widget and a `reload` closure the
/// caller should invoke each time the view is shown (mirrors the Tauri
/// frontend's `loadMappingsView()` running on every `open-mappings` event).
pub fn build(
    state: AppState,
    window: gtk4::Window,
    on_show_watched_dirs: impl Fn() + 'static,
    on_show_new_games: impl Fn() + 'static,
    on_show_pending: impl Fn() + 'static,
) -> (gtk4::Widget, Rc<dyn Fn()>) {
    let view_state = Rc::new(RefCell::new(ViewState::default()));

    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    container.set_margin_top(18);
    container.set_margin_bottom(18);
    container.set_margin_start(18);
    container.set_margin_end(18);

    let title = gtk4::Label::new(Some("Configuration"));
    title.add_css_class("title-1");
    title.set_halign(gtk4::Align::Start);
    container.append(&title);

    let desc = gtk4::Label::new(Some(
        "Type the executable name (e.g. hl2.exe on Windows, or just hl2 on Linux) for each game. \
         Once a session ends, LilyPad will prompt you to log it.",
    ));
    desc.set_wrap(true);
    desc.set_halign(gtk4::Align::Start);
    desc.add_css_class("dim-label");
    container.append(&desc);

    // --- Pending submissions notice — sessions that failed to auto-submit and are waiting
    // to be retried. Mirrors the Tauri build's `mappingsPendingNotice`. ---
    let pending_notice_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let pending_notice_label = gtk4::Label::new(None);
    pending_notice_label.set_halign(gtk4::Align::Start);
    let pending_notice_link = gtk4::LinkButton::builder().label("View").uri("#").build();
    pending_notice_link.set_visible(false);
    pending_notice_box.append(&pending_notice_label);
    pending_notice_box.append(&pending_notice_link);
    container.append(&pending_notice_box);

    pending_notice_link.connect_activate_link(move |_| {
        on_show_pending();
        glib::Propagation::Stop
    });

    // --- New Games notice (permanent, mirrors the pending-submissions notice on the About
    // page) — games LilyPad has detected being played that aren't in the FrogLog library yet. ---
    let new_games_notice_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let new_games_notice_label = gtk4::Label::new(None);
    new_games_notice_label.set_halign(gtk4::Align::Start);
    let new_games_notice_link = gtk4::LinkButton::builder().label("View").uri("#").build();
    new_games_notice_link.set_visible(false);
    new_games_notice_box.append(&new_games_notice_label);
    new_games_notice_box.append(&new_games_notice_link);
    container.append(&new_games_notice_box);

    new_games_notice_link.connect_activate_link(move |_| {
        on_show_new_games();
        glib::Propagation::Stop
    });

    // --- Auto-submit / presence toggles ---
    // Laid out as a 2-column grid of checkboxes (rather than one full-width switch row per
    // toggle) so this section doesn't dominate the view's vertical space.
    let toggles_grid = gtk4::Grid::new();
    toggles_grid.set_column_spacing(24);
    toggles_grid.set_row_spacing(8);
    toggles_grid.set_column_homogeneous(true);
    let auto_regular = gtk4::CheckButton::with_label("Auto-submit regular game sessions");
    let auto_session = gtk4::CheckButton::with_label("Auto-submit session-tracked game sessions");
    let auto_live = gtk4::CheckButton::with_label("Auto-submit live service sessions");
    let share_now_playing = gtk4::CheckButton::with_label("Enable online presence on FrogLog");
    let detect_unmapped = gtk4::CheckButton::with_label("Detect games not in your FrogLog library");
    toggles_grid.attach(&auto_regular, 0, 0, 1, 1);
    toggles_grid.attach(&auto_session, 1, 0, 1, 1);
    toggles_grid.attach(&auto_live, 0, 1, 1, 1);
    toggles_grid.attach(&share_now_playing, 1, 1, 1, 1);
    toggles_grid.attach(&detect_unmapped, 0, 2, 1, 1);
    container.append(&toggles_grid);

    {
        let cfg = state.process_map.read().unwrap();
        auto_regular.set_active(cfg.auto_submit_regular);
        auto_session.set_active(cfg.auto_submit_session);
        auto_live.set_active(cfg.auto_submit_live);
        share_now_playing.set_active(cfg.share_now_playing);
        detect_unmapped.set_active(!cfg.disable_unmapped_game_detection);
    }

    // Auto-submit / presence toggles persist immediately (not staged), same as the Tauri build.
    type Setter = fn(&mut lilypad_core::config::ProcessMapConfig, bool);
    let toggle_setters: [(&gtk4::CheckButton, Setter); 5] = [
        (&auto_regular, |c, v| c.auto_submit_regular = v),
        (&auto_session, |c, v| c.auto_submit_session = v),
        (&auto_live, |c, v| c.auto_submit_live = v),
        (&share_now_playing, |c, v| c.share_now_playing = v),
        (&detect_unmapped, |c, v| c.disable_unmapped_game_detection = !v),
    ];
    for (check, setter) in toggle_setters {
        let state = state.clone();
        check.connect_toggled(move |check| {
            let active = check.is_active();
            let mut cfg = state.process_map.write().unwrap();
            setter(&mut cfg, active);
            let _ = cfg.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()));
        });
    }

    // share_now_playing additionally needs to notify the FrogLog API (fire-and-forget).
    {
        let state = state.clone();
        share_now_playing.connect_toggled(move |check| {
            let active = check.is_active();
            let auth = state.auth.read().unwrap().clone();
            std::thread::spawn(move || {
                let base = auth.base_url.clone().unwrap_or_else(|| DEFAULT_API_URL.to_string());
                let mut client = FroglogClient::new(base);
                client.set_token(auth.token.clone());
                let _ = client.set_show_current_session(active);
            });
        });
    }

    // --- Mode switch (Games / Live service) ---
    // "Games" now covers both regular and session-tracked entries in one merged list -- LilyPad
    // auto-enables session tracking on anything it links itself to (see resolve.rs/monitor_glue.rs
    // and the Apply handler below), so the distinction is increasingly not worth a whole separate
    // tab. The underlying `game_type` ("regular" vs "session") is still tracked per-row internally
    // (it's how the backend actually stores it), just no longer surfaced as a filter here.
    let mode_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    mode_box.add_css_class("linked");
    let btn_regular = gtk4::ToggleButton::builder().label("Games").active(true).build();
    let btn_live = gtk4::ToggleButton::builder().label("Live service").build();
    btn_live.set_group(Some(&btn_regular));
    mode_box.append(&btn_regular);
    mode_box.append(&btn_live);

    let watched_dirs_btn = gtk4::Button::with_label("Non-Steam Games…");
    watched_dirs_btn.connect_clicked(move |_| on_show_watched_dirs());

    let mode_row = gtk4::CenterBox::new();
    mode_row.set_start_widget(Some(&mode_box));
    mode_row.set_end_widget(Some(&watched_dirs_btn));
    container.append(&mode_row);

    // --- Search ---
    let search_entry = gtk4::SearchEntry::new();
    search_entry.set_placeholder_text(Some("Search…"));
    container.append(&search_entry);

    // --- Error / status line ---
    let error_label = gtk4::Label::new(None);
    error_label.add_css_class("error");
    error_label.set_wrap(true);
    error_label.set_halign(gtk4::Align::Start);
    error_label.set_visible(false);
    container.append(&error_label);

    // --- Row list ---
    let list_box = gtk4::ListBox::new();
    list_box.set_selection_mode(gtk4::SelectionMode::None);
    list_box.add_css_class("boxed-list");
    let scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&list_box)
        .build();
    container.append(&scroller);

    // --- Footer ---
    let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    footer.set_halign(gtk4::Align::End);
    let apply_button = gtk4::Button::with_label("Apply");
    apply_button.add_css_class("suggested-action");
    apply_button.set_sensitive(false);
    footer.append(&apply_button);
    container.append(&footer);

    // --- Shared refresh: recompute conflicts/dirty, update apply button + highlighting ---
    let refresh_state: Rc<dyn Fn()> = {
        let view_state = Rc::clone(&view_state);
        let apply_button = apply_button.clone();
        let error_label = error_label.clone();
        Rc::new(move || {
            let vs = view_state.borrow();
            // Only compare mappings for rows still visible in the table -- a mapping left over
            // on an older, now-hidden replay duplicate (see `dedupe_latest_by_title`) shouldn't
            // flag a conflict the user has no row to see or fix.
            let visible_keys: HashSet<MapKey> = vs.all_rows.iter().map(|r| r.key()).collect();
            let conflicts = mappings_model::detect_conflicts(&vs.pending, &visible_keys);
            let dirty = mappings_model::is_dirty(&vs.pending, &vs.saved);
            apply_button.set_sensitive(conflicts.is_empty() && dirty);
            if conflicts.is_empty() {
                error_label.set_visible(false);
            } else {
                error_label.set_text(
                    "Conflicted mappings highlighted below. Each mapping must have a unique executable or title filter combination.",
                );
                error_label.set_visible(true);
            }
            for (key, (exe_entry, filter_entry)) in vs.entry_widgets.iter() {
                let has_conflict = conflicts.contains(key);
                exe_entry.set_css_classes(if has_conflict { &["error"] } else { &[] });
                filter_entry.set_css_classes(if has_conflict { &["error"] } else { &[] });
            }
        })
    };

    // --- Rebuild the visible row list for the current mode/search ---
    let rebuild_list: Rc<dyn Fn()> = {
        let view_state = Rc::clone(&view_state);
        let list_box = list_box.clone();
        let refresh_state = Rc::clone(&refresh_state);
        Rc::new(move || {
            while let Some(child) = list_box.first_child() {
                list_box.remove(&child);
            }
            let mut vs = view_state.borrow_mut();
            vs.entry_widgets.clear();
            let mode = vs.mode.clone();
            let needle = vs.search.to_lowercase();
            let rows: Vec<GameRow> = vs
                .all_rows
                .iter()
                .filter(|r| {
                    // "Games" mode (mode == "regular") matches both regular and session-tracked
                    // rows -- only "live" is still its own separate mode.
                    let mode_matches = if mode == "live" { r.game_type == "live" } else { r.game_type != "live" };
                    mode_matches && (needle.is_empty() || r.title.to_lowercase().contains(&needle))
                })
                .cloned()
                .collect();

            for row in rows {
                let key = row.key();
                // AdwActionRow's title is interpreted as Pango markup by default, so a raw
                // title containing "&"/"<"/">" (e.g. "Ratchet & Clank") fails to parse and the
                // row is left with no title at all -- escape before setting.
                let action_row = adw::ActionRow::builder().title(glib::markup_escape_text(&row.title)).build();

                let exe_entry = gtk4::Entry::builder()
                    .placeholder_text("e.g. hl2.exe")
                    .width_chars(14)
                    .valign(gtk4::Align::Center)
                    .text(vs.pending.exe.get(&key).cloned().unwrap_or_default())
                    .build();
                let browse_btn = gtk4::Button::with_label("Browse…");
                browse_btn.set_valign(gtk4::Align::Center);
                let filter_entry = gtk4::Entry::builder()
                    .placeholder_text("optional")
                    .width_chars(14)
                    .valign(gtk4::Align::Center)
                    .text(vs.pending.title_filter.get(&key).cloned().unwrap_or_default())
                    .build();

                action_row.add_suffix(&exe_entry);
                action_row.add_suffix(&browse_btn);
                action_row.add_suffix(&filter_entry);
                list_box.append(&action_row);

                vs.entry_widgets.insert(key.clone(), (exe_entry.clone(), filter_entry.clone()));

                {
                    let view_state = Rc::clone(&view_state);
                    let refresh_state = Rc::clone(&refresh_state);
                    let key = key.clone();
                    exe_entry.connect_changed(move |e| {
                        view_state.borrow_mut().pending.exe.insert(key.clone(), e.text().to_string());
                        refresh_state();
                    });
                }
                {
                    let view_state = Rc::clone(&view_state);
                    let refresh_state = Rc::clone(&refresh_state);
                    let key = key.clone();
                    filter_entry.connect_changed(move |e| {
                        view_state.borrow_mut().pending.title_filter.insert(key.clone(), e.text().to_string());
                        refresh_state();
                    });
                }
                {
                    let window = window.clone();
                    let exe_entry = exe_entry.clone();
                    browse_btn.connect_clicked(move |_| {
                        let dialog = gtk4::FileDialog::new();
                        dialog.set_title("Select the game's executable");
                        let exe_entry = exe_entry.clone();
                        glib::spawn_future_local(clone_window_future(window.clone(), dialog, exe_entry));
                    });
                }
            }
            drop(vs);
            refresh_state();
        })
    };

    async fn clone_window_future(window: gtk4::Window, dialog: gtk4::FileDialog, exe_entry: gtk4::Entry) {
        if let Ok(file) = dialog.open_future(Some(&window)).await {
            if let Some(path) = file.path() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    exe_entry.set_text(name);
                }
            }
        }
    }

    // Mode buttons.
    for (btn, mode) in [(&btn_regular, "regular"), (&btn_live, "live")] {
        let view_state = Rc::clone(&view_state);
        let rebuild_list = Rc::clone(&rebuild_list);
        let mode = mode.to_string();
        btn.connect_toggled(move |b| {
            if !b.is_active() {
                return;
            }
            view_state.borrow_mut().mode = mode.clone();
            rebuild_list();
        });
    }

    // Search.
    {
        let view_state = Rc::clone(&view_state);
        let rebuild_list = Rc::clone(&rebuild_list);
        search_entry.connect_search_changed(move |e| {
            view_state.borrow_mut().search = e.text().to_string();
            rebuild_list();
        });
    }

    // Apply: diff pending vs saved, persist each changed mapping.
    {
        let state = state.clone();
        let view_state = Rc::clone(&view_state);
        let error_label = error_label.clone();
        let apply_button_for_handler = apply_button.clone();
        apply_button.connect_clicked(move |_| {
            let mut vs = view_state.borrow_mut();
            let all_keys: HashSet<MapKey> = vs
                .pending
                .exe
                .keys()
                .chain(vs.saved.exe.keys())
                .cloned()
                .collect();

            // Any "regular" row being actively saved here (a real change, not just re-saving
            // something untouched) gets promoted to "session" -- matches LilyPad's own
            // auto-link behavior, and is why the mode switch above no longer distinguishes them.
            // Collected here and enabled server-side in the background after the local save,
            // since this handler runs on the GTK main thread.
            let mut needs_session_enable: Vec<i32> = Vec::new();
            let mut map = state.process_map.write().unwrap();
            for key in &all_keys {
                let new_exe = mappings_model::norm(vs.pending.exe.get(key).map(|s| s.as_str()).unwrap_or(""));
                let old_exe = mappings_model::norm(vs.saved.exe.get(key).map(|s| s.as_str()).unwrap_or(""));
                let new_filter = mappings_model::norm(vs.pending.title_filter.get(key).map(|s| s.as_str()).unwrap_or(""));
                let old_filter = mappings_model::norm(vs.saved.title_filter.get(key).map(|s| s.as_str()).unwrap_or(""));
                if new_exe == old_exe && new_filter == old_filter {
                    continue;
                }
                let (game_type, id) = key.clone();
                map.mappings.retain(|m| !(m.froglog_id == id && m.r#type == game_type));
                if !new_exe.is_empty() {
                    let title = vs.all_rows.iter().find(|r| r.key() == *key).map(|r| r.title.clone());
                    let effective_type = if game_type == "regular" {
                        needs_session_enable.push(id);
                        "session".to_string()
                    } else {
                        game_type
                    };
                    map.mappings.push(lilypad_core::config::ProcessMapping {
                        process: new_exe,
                        r#type: effective_type,
                        froglog_id: id,
                        title,
                        title_filter: if new_filter.is_empty() { None } else { Some(new_filter) },
                    });
                }
            }
            let _ = map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()));
            drop(map);

            if !needs_session_enable.is_empty() {
                let auth = state.auth.read().unwrap().clone();
                std::thread::spawn(move || {
                    let base = auth.base_url.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_API_URL.to_string());
                    let mut client = FroglogClient::new(base);
                    client.set_token(auth.token.clone());
                    for id in needs_session_enable {
                        if let Err(e) = client.enable_session_tracking(id) {
                            log::warn!("[LilyPad] failed to enable session tracking for game {id}: {e}");
                        }
                    }
                });
            }

            vs.saved = vs.pending.clone();
            drop(vs);

            error_label.remove_css_class("error");
            error_label.add_css_class("success");
            error_label.set_text("Mappings saved successfully!");
            error_label.set_visible(true);
            apply_button_for_handler.set_sensitive(false);

            let error_label_reset = error_label.clone();
            glib::timeout_add_seconds_local(3, move || {
                error_label_reset.set_visible(false);
                error_label_reset.remove_css_class("success");
                error_label_reset.add_css_class("error");
                glib::ControlFlow::Break
            });
        });
    }

    let reload: Rc<dyn Fn()> = {
        let state = state.clone();
        let view_state = Rc::clone(&view_state);
        let rebuild_list = Rc::clone(&rebuild_list);
        let error_label = error_label.clone();
        let auto_regular = auto_regular.clone();
        let auto_session = auto_session.clone();
        let auto_live = auto_live.clone();
        let share_now_playing = share_now_playing.clone();
        let detect_unmapped = detect_unmapped.clone();
        let new_games_notice_label = new_games_notice_label.clone();
        let new_games_notice_link = new_games_notice_link.clone();
        let pending_notice_label = pending_notice_label.clone();
        let pending_notice_link = pending_notice_link.clone();
        Rc::new(move || {
            {
                let cfg = state.process_map.read().unwrap();
                auto_regular.set_active(cfg.auto_submit_regular);
                auto_session.set_active(cfg.auto_submit_session);
                auto_live.set_active(cfg.auto_submit_live);
                share_now_playing.set_active(cfg.share_now_playing);
                detect_unmapped.set_active(!cfg.disable_unmapped_game_detection);
            }

            let pending_count = lilypad_core::config::load_pending_sessions().len();
            if pending_count > 0 {
                pending_notice_label.set_text(&format!(
                    "⚠ {pending_count} pending submission{}",
                    if pending_count > 1 { "s" } else { "" },
                ));
                pending_notice_link.set_visible(true);
            } else {
                pending_notice_label.set_text("✓ No pending submissions");
                pending_notice_link.set_visible(false);
            }

            let new_games_count = lilypad_core::config::load_pending_game_submissions().len();
            if new_games_count > 0 {
                new_games_notice_label.set_text(&format!(
                    "⚠ {new_games_count} game{} detected that {} not in FrogLog yet.",
                    if new_games_count > 1 { "s" } else { "" },
                    if new_games_count > 1 { "are" } else { "is" },
                ));
                new_games_notice_link.set_visible(true);
            } else {
                new_games_notice_label.set_text("✓ No games detected outside FrogLog");
                new_games_notice_link.set_visible(false);
            }

            let auth = state.auth.read().unwrap().clone();
            let (tx, rx) = async_channel::bounded(1);
            {
                let state = state.clone();
                std::thread::spawn(move || {
                    let base = auth.base_url.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_API_URL.to_string());
                    let mut client = FroglogClient::new(base);
                    client.set_token(auth.token.clone());
                    let result = client.get_games().and_then(|games| {
                        client.get_live_service_games().map(|live| (games, live))
                    });
                    // Opening Configure already fetches this same data -- also refresh the
                    // shared library_index the monitor uses for auto-link/New-Games detection,
                    // so it doesn't sit stale for up to 5 minutes (the periodic refresh
                    // interval) after a change made directly on the FrogLog website.
                    state.refresh_library_index();
                    // Also rescan installed games (Steam + watched non-Steam directories) --
                    // a game added to an already-watched folder has no dedicated immediate-
                    // refresh trigger of its own (unlike adding/removing the watched directory
                    // itself), so without this it would otherwise sit undetected until the next
                    // 300s periodic scan.
                    state.refresh_installed_games();
                    let _ = tx.send_blocking(result);
                });
            }

            let state = state.clone();
            let view_state = Rc::clone(&view_state);
            let rebuild_list = Rc::clone(&rebuild_list);
            let error_label = error_label.clone();
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok((games, live))) => {
                        let known_keys: HashSet<MapKey> = games
                            .iter()
                            .map(|g| {
                                mappings_model::map_key(
                                    if g.session_tracking.unwrap_or(false) { "session" } else { "regular" },
                                    g.id,
                                )
                            })
                            .chain(live.iter().map(|g| mappings_model::map_key("live", g.id)))
                            .collect();
                        {
                            let mut map = state.process_map.write().unwrap();
                            let before = map.mappings.len();
                            map.mappings.retain(|m| known_keys.contains(&mappings_model::map_key(&m.r#type, m.froglog_id)));
                            if map.mappings.len() != before {
                                let _ = map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()));
                            }
                        }

                        let rows: Vec<GameRow> = games
                            .iter()
                            .map(|g| GameRow {
                                id: g.id,
                                title: g.title.clone().unwrap_or_else(|| format!("#{}", g.id)),
                                game_type: if g.session_tracking.unwrap_or(false) { "session".into() } else { "regular".into() },
                            })
                            .chain(live.iter().map(|g| GameRow {
                                id: g.id,
                                title: g.title.clone().unwrap_or_else(|| format!("#{}", g.id)),
                                game_type: "live".into(),
                            }))
                            .collect();
                        // Collapse replay duplicates (same title, multiple `games` rows) down to
                        // just the most recent playthrough -- see `dedupe_latest_by_title`.
                        let rows = mappings_model::dedupe_latest_by_title(rows);

                        let staged = StagedMappings::from_process_mappings(&state.process_map.read().unwrap().mappings);
                        {
                            let mut vs = view_state.borrow_mut();
                            vs.all_rows = rows;
                            vs.saved = staged.clone();
                            vs.pending = staged;
                        }
                        error_label.set_visible(false);
                        rebuild_list();
                    }
                    Ok(Err(e)) => {
                        error_label.remove_css_class("success");
                        error_label.add_css_class("error");
                        error_label.set_text(&format!("Could not load data: {e}. Log in via Settings if needed."));
                        error_label.set_visible(true);
                    }
                    Err(_) => {}
                }
            });
        })
    };

    (container.upcast(), reload)
}
