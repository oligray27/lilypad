//! Excluded Games: lets the user tell LilyPad to completely ignore specific Steam apps that
//! aren't really games but install and manifest exactly like one (e.g. Wallpaper Engine has its
//! own `appmanifest_*.acf` and `steamapps/common/` folder same as any real game). Distinct from
//! `steam::is_known_non_game`'s hardcoded, maintainer-only list of things that are *never* a
//! game for any user (Steamworks Redistributables, Proton itself) -- this is user-configured per
//! appid. Mirrors the Non-Steam Games view's structure (`watched_dirs.rs`).

use crate::state::AppState;
use adw::prelude::*;
use lilypad_core::config::{process_map_path_for_auth, ExcludedApp};
use std::cell::RefCell;
use std::rc::Rc;

/// Builds the Excluded Games view. Returns the widget and a `reload` closure the caller should
/// invoke each time the view is shown.
pub fn build(state: AppState) -> (gtk4::Widget, Rc<dyn Fn()>) {
    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    container.set_margin_top(18);
    container.set_margin_bottom(18);
    container.set_margin_start(18);
    container.set_margin_end(18);

    let title = gtk4::Label::new(Some("Excluded Games"));
    title.add_css_class("title-1");
    title.set_halign(gtk4::Align::Start);
    container.append(&title);

    let desc = gtk4::Label::new(Some(
        "Some Steam apps aren't really games but install exactly like one (e.g. Wallpaper \
         Engine). Exclude them here and LilyPad will never detect or track sessions for them.",
    ));
    desc.set_wrap(true);
    desc.set_halign(gtk4::Align::Start);
    desc.add_css_class("dim-label");
    container.append(&desc);

    let empty_label = gtk4::Label::new(Some("No games excluded."));
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

    // --- Add panel: pick from currently-detected installed games, toggled by the footer button ---
    let add_panel = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    add_panel.set_visible(false);
    add_panel.set_margin_top(6);
    let add_combo = gtk4::ComboBoxText::new();
    add_combo.set_hexpand(true);
    add_panel.append(&add_combo);
    let add_confirm_btn = gtk4::Button::with_label("Exclude");
    add_confirm_btn.add_css_class("suggested-action");
    add_panel.append(&add_confirm_btn);
    container.append(&add_panel);

    let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    footer.set_halign(gtk4::Align::End);
    let add_button = gtk4::Button::with_label("Exclude a Game…");
    add_button.add_css_class("suggested-action");
    footer.append(&add_button);
    container.append(&footer);

    let reload_cell: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

    let reload: Rc<dyn Fn()> = {
        let state = state.clone();
        let list_box = list_box.clone();
        let scroller = scroller.clone();
        let empty_label = empty_label.clone();
        let add_panel = add_panel.clone();
        let reload_cell = Rc::clone(&reload_cell);
        Rc::new(move || {
            add_panel.set_visible(false);
            while let Some(child) = list_box.first_child() {
                list_box.remove(&child);
            }
            let excluded = state.process_map.read().unwrap().excluded_apps.clone();
            let is_empty = excluded.is_empty();
            empty_label.set_visible(is_empty);
            scroller.set_visible(!is_empty);

            for app in excluded {
                // AdwActionRow's title is Pango markup by default -- escape the name so one
                // containing "&"/"<"/">" doesn't fail to parse and render blank.
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&app.name))
                    .subtitle(format!("Appid {}", app.appid))
                    .build();
                let remove_btn = gtk4::Button::with_label("Remove");
                remove_btn.set_valign(gtk4::Align::Center);
                row.add_suffix(&remove_btn);

                let state = state.clone();
                let appid = app.appid.clone();
                let on_changed = {
                    let reload_cell = Rc::clone(&reload_cell);
                    move || {
                        if let Some(reload) = reload_cell.borrow().as_ref() {
                            reload();
                        }
                    }
                };
                remove_btn.connect_clicked(move |_| {
                    let mut map = state.process_map.read().unwrap().clone();
                    map.excluded_apps.retain(|e| e.appid != appid);
                    let _ = map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()));
                    *state.process_map.write().unwrap() = map;
                    state.refresh_installed_games();
                    on_changed();
                });

                list_box.append(&row);
            }
        })
    };
    *reload_cell.borrow_mut() = Some(Rc::clone(&reload));

    add_button.connect_clicked({
        let state = state.clone();
        let add_panel = add_panel.clone();
        let add_combo = add_combo.clone();
        move |_| {
            let was_hidden = !add_panel.is_visible();
            add_panel.set_visible(was_hidden);
            if !was_hidden {
                return;
            }
            add_combo.remove_all();
            let excluded_ids: std::collections::HashSet<String> =
                state.process_map.read().unwrap().excluded_apps.iter().map(|e| e.appid.clone()).collect();
            let candidates: Vec<_> = state
                .installed_games
                .read()
                .unwrap()
                .iter()
                .filter(|g| !excluded_ids.contains(&g.appid) && !g.appid.starts_with("local:"))
                .cloned()
                .collect();
            if candidates.is_empty() {
                add_combo.append(None, "No eligible installed games detected");
                add_combo.set_active(Some(0));
                add_combo.set_sensitive(false);
            } else {
                add_combo.set_sensitive(true);
                for game in &candidates {
                    add_combo.append(Some(&game.appid), &format!("{} ({})", game.name, game.appid));
                }
                add_combo.set_active(Some(0));
            }
        }
    });

    add_confirm_btn.connect_clicked({
        let state = state.clone();
        let add_combo = add_combo.clone();
        let add_panel = add_panel.clone();
        let reload = Rc::clone(&reload);
        move |_| {
            let Some(appid) = add_combo.active_id().map(|s| s.to_string()) else { return };
            let Some(name) = add_combo.active_text().map(|s| s.to_string()) else { return };
            // Strip the " (appid)" suffix added for display -- keep just the game's own name.
            let name = name.rsplit_once(" (").map(|(n, _)| n.to_string()).unwrap_or(name);
            let mut map = state.process_map.read().unwrap().clone();
            if !map.excluded_apps.iter().any(|e| e.appid == appid) {
                map.excluded_apps.push(ExcludedApp { appid, name });
            }
            let _ = map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()));
            *state.process_map.write().unwrap() = map;
            state.refresh_installed_games();
            add_panel.set_visible(false);
            reload();
        }
    });

    (container.upcast(), reload)
}
