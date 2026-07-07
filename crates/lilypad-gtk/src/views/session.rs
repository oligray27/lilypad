use crate::session_flow::{self, SessionEndedData};
use crate::state::{AppState, DEFAULT_API_URL};
use crate::tray::RefreshTray;
use adw::prelude::*;
use lilypad_core::api::FroglogClient;
use lilypad_core::config::{load_pending_sessions, save_pending_sessions, PendingSession};

/// Builds and presents the session-ended popup as its own small top-level
/// window (not a stack page) — the original positions this near the tray
/// corner via absolute pixel coordinates, which Wayland has no client-side API
/// for, so this is a normal compositor-placed window instead.
pub fn show_popup(state: AppState, parent: &gtk4::Window, refresh_tray: RefreshTray, data: SessionEndedData) {
    let has_notes = data.mapping.r#type.eq_ignore_ascii_case("live") || data.mapping.r#type.eq_ignore_ascii_case("session");
    let game_title = data.mapping.title.clone().unwrap_or_else(|| data.mapping.process.clone());

    let header_bar = adw::HeaderBar::new();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(16);
    content.set_margin_end(16);

    let header_label = gtk4::Label::new(Some(if data.forced { "Session Ended (Forced)" } else { "Session Ended" }));
    header_label.add_css_class("title-2");
    header_label.set_halign(gtk4::Align::Start);
    content.append(&header_label);

    let info_label = gtk4::Label::new(Some(&format!(
        "{game_title} – {}",
        session_flow::format_duration(data.duration_secs)
    )));
    info_label.set_halign(gtk4::Align::Start);
    content.append(&info_label);

    let notes_buffer = gtk4::TextBuffer::new(None);
    let spoiler_check = gtk4::CheckButton::with_label("Contains spoilers");
    let hide_public_check = gtk4::CheckButton::with_label("Hide from public");

    if has_notes {
        let notes_label = gtk4::Label::new(Some("Notes (optional)"));
        notes_label.set_halign(gtk4::Align::Start);
        content.append(&notes_label);

        let notes_view = gtk4::TextView::with_buffer(&notes_buffer);
        notes_view.set_wrap_mode(gtk4::WrapMode::WordChar);
        let notes_scroller = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .height_request(60)
            .child(&notes_view)
            .build();
        notes_scroller.add_css_class("card");
        content.append(&notes_scroller);

        let checks_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        checks_box.append(&spoiler_check);
        checks_box.append(&hide_public_check);
        content.append(&checks_box);
    }

    let error_label = gtk4::Label::new(None);
    error_label.add_css_class("error");
    error_label.set_wrap(true);
    error_label.set_halign(gtk4::Align::Start);
    error_label.set_visible(false);
    content.append(&error_label);

    let actions_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    actions_box.set_margin_top(4);
    let submit_button = gtk4::Button::with_label("Submit to FrogLog");
    submit_button.add_css_class("suggested-action");
    let skip_button = gtk4::Button::with_label("Do not record session");
    actions_box.append(&submit_button);
    actions_box.append(&skip_button);
    content.append(&actions_box);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header_bar);
    toolbar_view.set_content(Some(&content));

    let window = adw::Window::builder()
        .transient_for(parent)
        .default_width(420)
        .default_height(if has_notes { 320 } else { 170 })
        .content(&toolbar_view)
        .title("LilyPad")
        .build();

    skip_button.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });

    submit_button.connect_clicked({
        let window = window.clone();
        move |btn| {
            let notes = if has_notes {
                let (start, end) = notes_buffer.bounds();
                let text = notes_buffer.text(&start, &end, false).to_string();
                let trimmed = text.trim();
                if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
            } else {
                None
            };
            let spoiler = has_notes && spoiler_check.is_active();
            let is_public = !(has_notes && hide_public_check.is_active());
            let hours = session_flow::round_hours(data.duration_secs);

            btn.set_sensitive(false);
            error_label.set_visible(false);

            let auth = state.auth.read().unwrap().clone();
            let mapping = data.mapping.clone();
            let title = mapping.title.clone().unwrap_or_else(|| mapping.process.clone());

            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let base = auth.base_url.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_API_URL.to_string());
                let mut client = FroglogClient::new(base);
                client.set_token(auth.token.clone());
                let date = chrono::Local::now().format("%Y-%m-%d").to_string();
                let result = if mapping.r#type.eq_ignore_ascii_case("live") {
                    client.add_live_service_session(mapping.froglog_id, Some(date), Some(hours), notes.clone(), spoiler, is_public)
                } else if mapping.r#type.eq_ignore_ascii_case("session") {
                    client.add_game_session(mapping.froglog_id, Some(date), Some(hours), notes.clone(), spoiler, is_public)
                } else {
                    client.update_game_hours(mapping.froglog_id, hours)
                };
                let _ = tx.send_blocking((result, notes, spoiler, is_public, hours, mapping, title));
            });

            let window = window.clone();
            let refresh_tray = refresh_tray.clone();
            let btn = btn.clone();
            let error_label = error_label.clone();
            glib::spawn_future_local(async move {
                let Ok((result, notes, spoiler, is_public, hours, mapping, title)) = rx.recv().await else { return };
                match result {
                    Ok(_) => {
                        window.close();
                    }
                    Err(e) => {
                        let mut sessions = load_pending_sessions();
                        sessions.push(PendingSession {
                            id: chrono::Local::now().timestamp_millis().to_string(),
                            game_id: mapping.froglog_id,
                            game_type: mapping.r#type.clone(),
                            title,
                            hours,
                            notes,
                            spoiler,
                            is_public,
                            date: chrono::Local::now().format("%Y-%m-%d").to_string(),
                            failed_at: chrono::Local::now().to_rfc3339(),
                            error: e.clone(),
                        });
                        save_pending_sessions(&sessions);
                        refresh_tray();
                        btn.set_sensitive(true);
                        error_label.set_text(&format!(
                            "Submission failed, session saved to Pending Submissions. {e}"
                        ));
                        error_label.set_visible(true);
                    }
                }
            });
        }
    });

    window.present();
}
