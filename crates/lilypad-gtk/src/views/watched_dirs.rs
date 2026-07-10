//! Non-Steam Games: lets the user point LilyPad at root folders (e.g. a GOG or itch.io
//! library) whose immediate subfolders should each be treated as an installed game (see
//! `lilypad_core::local_games`). Mirrors the Tauri build's dedicated watched-directories view.

use crate::state::AppState;
use adw::prelude::*;
use lilypad_core::config::{process_map_path_for_auth, WatchedDirectory};
use std::rc::Rc;

/// Builds the Non-Steam Games view. Returns the widget and a `reload` closure the caller
/// should invoke each time the view is shown.
pub fn build(state: AppState, window: gtk4::Window) -> (gtk4::Widget, Rc<dyn Fn()>) {
    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    container.set_margin_top(18);
    container.set_margin_bottom(18);
    container.set_margin_start(18);
    container.set_margin_end(18);

    let title = gtk4::Label::new(Some("Non-Steam Games"));
    title.add_css_class("title-1");
    title.set_halign(gtk4::Align::Start);
    container.append(&title);

    let desc = gtk4::Label::new(Some(
        "Point LilyPad at a folder (e.g. your GOG or itch.io library). Every subfolder \
         inside it is treated as a separate installed game, so LilyPad can detect sessions \
         for it the same way it does for Steam games.",
    ));
    desc.set_wrap(true);
    desc.set_halign(gtk4::Align::Start);
    desc.add_css_class("dim-label");
    container.append(&desc);

    let empty_label = gtk4::Label::new(Some("No folders added yet."));
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

    let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    footer.set_halign(gtk4::Align::End);
    let add_button = gtk4::Button::with_label("Add Folder…");
    add_button.add_css_class("suggested-action");
    footer.append(&add_button);
    container.append(&footer);

    let reload_cell: Rc<std::cell::RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(std::cell::RefCell::new(None));

    let reload: Rc<dyn Fn()> = {
        let state = state.clone();
        let list_box = list_box.clone();
        let scroller = scroller.clone();
        let empty_label = empty_label.clone();
        let reload_cell = Rc::clone(&reload_cell);
        Rc::new(move || {
            while let Some(child) = list_box.first_child() {
                list_box.remove(&child);
            }
            let dirs = state.process_map.read().unwrap().watched_directories.clone();
            let is_empty = dirs.is_empty();
            empty_label.set_visible(is_empty);
            scroller.set_visible(!is_empty);

            for dir in dirs {
                let row = adw::ActionRow::builder().title(dir.path.clone()).build();
                let remove_btn = gtk4::Button::with_label("Remove");
                remove_btn.set_valign(gtk4::Align::Center);
                row.add_suffix(&remove_btn);

                let state = state.clone();
                let path = dir.path.clone();
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
                    map.watched_directories.retain(|w| w.path != path);
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
        let window = window.clone();
        let reload = Rc::clone(&reload);
        move |_| {
            let state = state.clone();
            let reload = Rc::clone(&reload);
            glib::spawn_future_local(pick_and_add(state, window.clone(), reload));
        }
    });

    async fn pick_and_add(state: AppState, window: gtk4::Window, reload: Rc<dyn Fn()>) {
        let dialog = gtk4::FileDialog::new();
        dialog.set_title("Select a game library folder");
        if let Ok(folder) = dialog.select_folder_future(Some(&window)).await {
            if let Some(path) = folder.path() {
                let path = path.display().to_string();
                let mut map = state.process_map.read().unwrap().clone();
                if !map.watched_directories.iter().any(|w| w.path == path) {
                    map.watched_directories.push(WatchedDirectory { path });
                }
                let _ = map.save_to(&process_map_path_for_auth(&state.auth.read().unwrap()));
                *state.process_map.write().unwrap() = map;
                state.refresh_installed_games();
                reload();
            }
        }
    }

    (container.upcast(), reload)
}
