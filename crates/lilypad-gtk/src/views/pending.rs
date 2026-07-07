use crate::state::{AppState, DEFAULT_API_URL};
use adw::prelude::*;
use lilypad_core::api::FroglogClient;
use lilypad_core::config::{load_pending_sessions, save_pending_sessions, PendingSession};
use std::cell::RefCell;
use std::rc::Rc;

/// Builds the pending-submissions queue view. Returns the widget and a
/// `reload` closure the caller should invoke each time the view is shown.
pub fn build(state: AppState) -> (gtk4::Widget, Rc<dyn Fn()>) {
    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    container.set_margin_top(18);
    container.set_margin_bottom(18);
    container.set_margin_start(18);
    container.set_margin_end(18);

    let title = gtk4::Label::new(Some("Pending Submissions"));
    title.add_css_class("title-1");
    title.set_halign(gtk4::Align::Start);
    container.append(&title);

    let desc = gtk4::Label::new(Some(
        "These sessions failed to submit. Log out and back in from the tray to refresh your \
         token, or check your connection, then retry.",
    ));
    desc.set_wrap(true);
    desc.set_halign(gtk4::Align::Start);
    desc.add_css_class("dim-label");
    container.append(&desc);

    let empty_label = gtk4::Label::new(Some("No pending submissions."));
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

    // `reload` is defined recursively (rows need to call it after retry/delete),
    // so it's built up via a RefCell cell that gets filled in immediately after.
    let reload_cell: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

    let reload: Rc<dyn Fn()> = {
        let list_box = list_box.clone();
        let scroller = scroller.clone();
        let empty_label = empty_label.clone();
        let reload_cell = Rc::clone(&reload_cell);
        Rc::new(move || {
            while let Some(child) = list_box.first_child() {
                list_box.remove(&child);
            }
            let sessions = load_pending_sessions();
            let is_empty = sessions.is_empty();
            empty_label.set_visible(is_empty);
            scroller.set_visible(!is_empty);

            for session in sessions {
                let on_changed = {
                    let reload_cell = Rc::clone(&reload_cell);
                    move || {
                        if let Some(reload) = reload_cell.borrow().as_ref() {
                            reload();
                        }
                    }
                };
                let row = build_row(state.clone(), session, Rc::new(on_changed));
                list_box.append(&row);
            }
        })
    };
    *reload_cell.borrow_mut() = Some(Rc::clone(&reload));

    (container.upcast(), reload)
}

fn build_row(state: AppState, session: PendingSession, on_changed: Rc<dyn Fn()>) -> adw::ActionRow {
    let mut subtitle = format!("{}h · {}", session.hours, session.date);
    if let Some(notes) = &session.notes {
        if !notes.is_empty() {
            subtitle.push_str(&format!(" · {notes}"));
        }
    }
    subtitle.push_str(&format!("\n{}", session.error));

    let row = adw::ActionRow::builder()
        .title(session.title.clone())
        .subtitle(subtitle)
        .subtitle_lines(2)
        .build();

    let status_label = gtk4::Label::new(None);
    status_label.add_css_class("dim-label");
    status_label.set_visible(false);

    let retry_btn = gtk4::Button::with_label("Retry");
    retry_btn.set_valign(gtk4::Align::Center);
    let delete_btn = gtk4::Button::with_label("Delete");
    delete_btn.set_valign(gtk4::Align::Center);

    row.add_suffix(&status_label);
    row.add_suffix(&retry_btn);
    row.add_suffix(&delete_btn);

    retry_btn.connect_clicked({
        let session = session.clone();
        let status_label = status_label.clone();
        let delete_btn = delete_btn.clone();
        let on_changed = Rc::clone(&on_changed);
        move |btn| {
            btn.set_sensitive(false);
            delete_btn.set_sensitive(false);
            status_label.set_text("Submitting…");
            status_label.set_visible(true);

            let auth = state.auth.read().unwrap().clone();
            let session = session.clone();
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let base = auth.base_url.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_API_URL.to_string());
                let mut client = FroglogClient::new(base);
                client.set_token(auth.token.clone());
                let notes = Some(
                    session
                        .notes
                        .clone()
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| "Session submitted from Pending Sessions".to_string()),
                );
                let result = if session.game_type.eq_ignore_ascii_case("live") {
                    client.add_live_service_session(session.game_id, Some(session.date.clone()), Some(session.hours), notes, session.spoiler, session.is_public)
                } else if session.game_type.eq_ignore_ascii_case("session") {
                    client.add_game_session(session.game_id, Some(session.date.clone()), Some(session.hours), notes, session.spoiler, session.is_public)
                } else {
                    client.update_game_hours(session.game_id, session.hours)
                };
                let _ = tx.send_blocking((result, session.id.clone()));
            });

            let on_changed = Rc::clone(&on_changed);
            let status_label = status_label.clone();
            let btn = btn.clone();
            let delete_btn = delete_btn.clone();
            glib::spawn_future_local(async move {
                let Ok((result, session_id)) = rx.recv().await else { return };
                match result {
                    Ok(_) => {
                        let mut sessions = load_pending_sessions();
                        sessions.retain(|s| s.id != session_id);
                        save_pending_sessions(&sessions);
                        on_changed();
                    }
                    Err(e) => {
                        btn.set_sensitive(true);
                        delete_btn.set_sensitive(true);
                        status_label.set_text(&e);
                    }
                }
            });
        }
    });

    delete_btn.connect_clicked({
        let session_id = session.id.clone();
        let on_changed = Rc::clone(&on_changed);
        move |_| {
            let mut sessions = load_pending_sessions();
            sessions.retain(|s| s.id != session_id);
            save_pending_sessions(&sessions);
            on_changed();
        }
    });

    row
}
