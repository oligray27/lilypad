use crate::state::{AppState, DEFAULT_API_URL};
use adw::prelude::*;
use lilypad_core::api::FroglogClient;
use lilypad_core::config::{auth_config_path, process_map_path_for_auth, ProcessMapConfig};
use std::rc::Rc;

/// Builds the login view. `on_success` is called (on the GTK main thread) once
/// login succeeds and `AppState`/on-disk config have been updated.
pub fn build(state: AppState, on_success: impl Fn() + 'static) -> gtk4::Box {
    let on_success = Rc::new(on_success);

    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    container.set_margin_top(48);
    container.set_margin_bottom(48);
    container.set_margin_start(24);
    container.set_margin_end(24);
    container.set_valign(gtk4::Align::Center);
    container.set_width_request(400);
    container.set_halign(gtk4::Align::Center);

    let title = gtk4::Label::new(Some("LilyPad – Login"));
    title.add_css_class("title-1");
    container.append(&title);

    let subtitle = gtk4::Label::new(Some(
        "Enter your FrogLog credentials. LilyPad runs in the system tray and tracks game time once process mappings are set up.",
    ));
    subtitle.set_wrap(true);
    subtitle.add_css_class("dim-label");
    container.append(&subtitle);

    let group = adw::PreferencesGroup::new();
    let username_row = adw::EntryRow::builder().title("Username").build();
    let password_row = adw::PasswordEntryRow::builder().title("Password").build();
    group.add(&username_row);
    group.add(&password_row);
    container.append(&group);

    let remember_me = gtk4::CheckButton::with_label("Remember me");
    remember_me.set_active(true);
    container.append(&remember_me);

    let error_label = gtk4::Label::new(None);
    error_label.add_css_class("error");
    error_label.set_wrap(true);
    error_label.set_visible(false);
    container.append(&error_label);

    let submit = gtk4::Button::with_label("Log in");
    submit.add_css_class("suggested-action");
    container.append(&submit);

    submit.connect_clicked(move |btn| {
        let username = username_row.text().to_string();
        let password = password_row.text().to_string();
        let remember = remember_me.is_active();

        if username.trim().is_empty() || password.is_empty() {
            error_label.set_text("Username and password are required.");
            error_label.set_visible(true);
            return;
        }

        btn.set_sensitive(false);
        error_label.set_visible(false);
        error_label.set_text("");

        let (tx, rx) = async_channel::bounded(1);
        let username_for_thread = username.clone();
        std::thread::spawn(move || {
            let client = FroglogClient::new(DEFAULT_API_URL.to_string());
            let result = client.login(&username_for_thread, &password, remember);
            let _ = tx.send_blocking(result);
        });

        let state = state.clone();
        let btn = btn.clone();
        let error_label = error_label.clone();
        let on_success = Rc::clone(&on_success);
        let username = username.clone();
        glib::spawn_future_local(async move {
            let Ok(result) = rx.recv().await else { return };
            btn.set_sensitive(true);
            match result {
                Ok(res) => {
                    let map_path = {
                        let mut auth = state.auth.write().unwrap();
                        auth.base_url = Some(DEFAULT_API_URL.to_string());
                        auth.token = Some(res.token.clone());
                        auth.username = res.username.clone().or_else(|| Some(username.clone()));
                        let _ = auth.save_to(&auth_config_path());
                        process_map_path_for_auth(&auth)
                    };
                    let process_map = ProcessMapConfig::load_from(&map_path);
                    *state.process_map.write().unwrap() = process_map;
                    on_success();
                }
                Err(e) => {
                    error_label.set_text(&e);
                    error_label.set_visible(true);
                }
            }
        });
    });

    container
}
