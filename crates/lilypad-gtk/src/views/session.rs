use crate::session_flow::{self, SessionEndedData};
use crate::state::AppState;
use crate::tray::RefreshTray;
use adw::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

/// How one Submit attempt ended, decided on the worker thread.
enum Attempt {
    Submitted,
    /// Failed, and the session is durably in Pending Submissions: terminal for this popup.
    Queued(String),
    /// Failed and could not be saved, or was refused; the user may try again.
    Failed(String),
}

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

    // Set once a failed submission has been saved for retry. From then on this popup has
    // nothing left to decide: the record is retried from Pending Submissions, not from here.
    let queued = Rc::new(Cell::new(false));

    // "Do not record session" discards the record explicitly. Closing the window without a
    // choice leaves it pending, so the session is never lost by accident.
    skip_button.connect_clicked({
        let window = window.clone();
        let state = state.clone();
        let refresh_tray = refresh_tray.clone();
        let error_label = error_label.clone();
        let ledger_id = data.ledger_id.clone();
        let queued = Rc::clone(&queued);
        move |_| {
            if queued.get() {
                window.close();
                return;
            }
            if let (Some(id), Some(account)) = (&ledger_id, state.account()) {
                if state.store().discard(id, &account).is_none() {
                    error_label.set_text(&format!(
                        "Could not discard this session: {}",
                        state.store().error().unwrap_or_else(|| "storage unavailable".into())
                    ));
                    error_label.set_visible(true);
                    return;
                }
                refresh_tray();
            }
            window.close();
        }
    });

    submit_button.connect_clicked({
        let window = window.clone();
        let skip_button = skip_button.clone();
        let queued = Rc::clone(&queued);
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

            skip_button.set_sensitive(false);
            let state = state.clone();
            let mapping = data.mapping.clone();
            let ledger_id = data.ledger_id.clone();

            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let _ = tx.send_blocking(submit(&state, &mapping, ledger_id, hours, notes, spoiler, is_public));
            });

            let window = window.clone();
            let refresh_tray = refresh_tray.clone();
            let btn = btn.clone();
            let skip_button = skip_button.clone();
            let error_label = error_label.clone();
            let queued = Rc::clone(&queued);
            glib::spawn_future_local(async move {
                let Ok(attempt) = rx.recv().await else { return };
                refresh_tray();
                skip_button.set_sensitive(true);
                match attempt {
                    Attempt::Submitted => window.close(),
                    Attempt::Queued(message) => {
                        queued.set(true);
                        skip_button.set_label("Close");
                        error_label.set_text(&message);
                        error_label.set_visible(true);
                    }
                    Attempt::Failed(message) => {
                        btn.set_sensitive(true);
                        error_label.set_text(&message);
                        error_label.set_visible(true);
                    }
                }
            });
        }
    });

    window.present();
}

/// Blocking: submits against the session's record, then acknowledges or queues that record.
fn submit(
    state: &AppState,
    mapping: &lilypad_core::config::ProcessMapping,
    ledger_id: Option<String>,
    hours: f64,
    notes: Option<String>,
    spoiler: bool,
    is_public: bool,
) -> Attempt {
    // One snapshot for both, so a login change mid-popup cannot pair them up wrongly.
    let auth = state.auth.read().unwrap().clone();
    let store = state.store();
    let account = store.account(&auth);
    if account.is_none() {
        return Attempt::Failed("Log in to FrogLog to submit this session.".into());
    }
    if !session_flow::may_submit(state, ledger_id.as_deref(), account.as_ref()) {
        return Attempt::Failed(
            "This session was started under a different FrogLog account. Log back in to that account to submit it.".into(),
        );
    }
    let client = session_flow::client_for(&auth);
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let result = lilypad_core::submission::submit_play_session(
        &client, &mapping.r#type, mapping.froglog_id, Some(date.clone()), hours,
        notes.clone(), spoiler, is_public, ledger_id.clone(),
    );
    match result {
        Ok(response) => {
            if let (Some(id), Some(account)) = (&ledger_id, &account) {
                let remote = lilypad_core::submission::remote_reference(&response, mapping.froglog_id, &mapping.r#type);
                if store.acknowledge(id, account, &remote).is_none() {
                    log::warn!("[LilyPad] session {id} submitted but its acknowledgement was not saved; a retry reuses the same key");
                }
            }
            Attempt::Submitted
        }
        Err(e) => {
            let explanation = lilypad_core::submission::explain_failure(&e);
            let submission = session_flow::failed_submission(mapping, hours, date, notes, spoiler, is_public, &e);
            if store.queue_failed(ledger_id.as_deref(), account, submission) {
                Attempt::Queued(format!("Submission failed; the session is saved in Pending Submissions. {explanation}"))
            } else {
                Attempt::Failed(format!(
                    "Submission failed and the session could not be saved for retry ({}). {explanation}",
                    store.error().unwrap_or_else(|| "storage unavailable".into())
                ))
            }
        }
    }
}
