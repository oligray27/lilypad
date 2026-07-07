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
pub fn build(state: AppState, window: gtk4::Window) -> (gtk4::Widget, Rc<dyn Fn()>) {
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

    // --- Auto-submit / presence toggles ---
    let toggles_group = adw::PreferencesGroup::new();
    let auto_regular = adw::SwitchRow::builder().title("Auto-submit regular game sessions").build();
    let auto_session = adw::SwitchRow::builder().title("Auto-submit session-tracked game sessions").build();
    let auto_live = adw::SwitchRow::builder().title("Auto-submit live service sessions").build();
    let share_now_playing = adw::SwitchRow::builder().title("Enable online presence on FrogLog").build();
    toggles_group.add(&auto_regular);
    toggles_group.add(&auto_session);
    toggles_group.add(&auto_live);
    toggles_group.add(&share_now_playing);
    container.append(&toggles_group);

    {
        let cfg = state.process_map.read().unwrap();
        auto_regular.set_active(cfg.auto_submit_regular);
        auto_session.set_active(cfg.auto_submit_session);
        auto_live.set_active(cfg.auto_submit_live);
        share_now_playing.set_active(cfg.share_now_playing);
    }

    // Auto-submit / presence toggles persist immediately (not staged), same as the Tauri build.
    type Setter = fn(&mut lilypad_core::config::ProcessMapConfig, bool);
    let toggle_setters: [(&adw::SwitchRow, Setter); 4] = [
        (&auto_regular, |c, v| c.auto_submit_regular = v),
        (&auto_session, |c, v| c.auto_submit_session = v),
        (&auto_live, |c, v| c.auto_submit_live = v),
        (&share_now_playing, |c, v| c.share_now_playing = v),
    ];
    for (row, setter) in toggle_setters {
        let state = state.clone();
        row.connect_active_notify(move |row| {
            let active = row.is_active();
            let mut cfg = state.process_map.write().unwrap();
            setter(&mut cfg, active);
            let _ = cfg.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()));
        });
    }

    // share_now_playing additionally needs to notify the FrogLog API (fire-and-forget).
    {
        let state = state.clone();
        share_now_playing.connect_active_notify(move |row| {
            let active = row.is_active();
            let auth = state.auth.read().unwrap().clone();
            std::thread::spawn(move || {
                let base = auth.base_url.clone().unwrap_or_else(|| DEFAULT_API_URL.to_string());
                let mut client = FroglogClient::new(base);
                client.set_token(auth.token.clone());
                let _ = client.set_show_current_session(active);
            });
        });
    }

    // --- Mode switch (Games / Session-tracked / Live service) ---
    let mode_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    mode_box.add_css_class("linked");
    let btn_regular = gtk4::ToggleButton::builder().label("Games").active(true).build();
    let btn_session = gtk4::ToggleButton::builder().label("Session tracked").build();
    let btn_live = gtk4::ToggleButton::builder().label("Live service").build();
    btn_session.set_group(Some(&btn_regular));
    btn_live.set_group(Some(&btn_regular));
    mode_box.append(&btn_regular);
    mode_box.append(&btn_session);
    mode_box.append(&btn_live);
    container.append(&mode_box);

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
            let conflicts = mappings_model::detect_conflicts(&vs.pending);
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
                .filter(|r| r.game_type == mode && (needle.is_empty() || r.title.to_lowercase().contains(&needle)))
                .cloned()
                .collect();

            for row in rows {
                let key = row.key();
                let action_row = adw::ActionRow::builder().title(row.title.clone()).build();

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
    for (btn, mode) in [(&btn_regular, "regular"), (&btn_session, "session"), (&btn_live, "live")] {
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
                    map.mappings.push(lilypad_core::config::ProcessMapping {
                        process: new_exe,
                        r#type: game_type,
                        froglog_id: id,
                        title,
                        title_filter: if new_filter.is_empty() { None } else { Some(new_filter) },
                    });
                }
            }
            let _ = map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()));
            drop(map);

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
        Rc::new(move || {
            {
                let cfg = state.process_map.read().unwrap();
                auto_regular.set_active(cfg.auto_submit_regular);
                auto_session.set_active(cfg.auto_submit_session);
                auto_live.set_active(cfg.auto_submit_live);
                share_now_playing.set_active(cfg.share_now_playing);
            }

            let auth = state.auth.read().unwrap().clone();
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let base = auth.base_url.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_API_URL.to_string());
                let mut client = FroglogClient::new(base);
                client.set_token(auth.token.clone());
                let result = client.get_games().and_then(|games| {
                    client.get_live_service_games().map(|live| (games, live))
                });
                let _ = tx.send_blocking(result);
            });

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
