use crate::state::AppState;
use adw::prelude::*;
use lilypad_core::config::PendingSession;
use lilypad_core::session_store::UnownedRecord;
use std::cell::RefCell;
use std::rc::Rc;

/// Builds the pending-submissions queue view. Returns the widget and a
/// `reload` closure the caller should invoke each time the view is shown.
///
/// Rows are ledger records owned by the logged-in account. Records imported from a pre-ledger
/// build have no owner and are listed separately, to be assigned or discarded explicitly --
/// never submitted merely because someone happens to be logged in.
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
        "These sessions have not reached FrogLog yet. Log out and back in from the tray to \
         refresh your token, or check your connection, then retry.",
    ));
    desc.set_wrap(true);
    desc.set_halign(gtk4::Align::Start);
    desc.add_css_class("dim-label");
    container.append(&desc);

    // Shown instead of the list when the queue cannot be read: an unreadable store must never
    // look like an empty one.
    let status_label = gtk4::Label::new(None);
    status_label.set_wrap(true);
    status_label.set_halign(gtk4::Align::Start);
    status_label.set_margin_top(12);
    status_label.set_visible(false);
    container.append(&status_label);

    let list_box = gtk4::ListBox::new();
    list_box.set_selection_mode(gtk4::SelectionMode::None);
    list_box.add_css_class("boxed-list");

    let unowned_title = gtk4::Label::new(Some("From an earlier LilyPad version"));
    unowned_title.add_css_class("heading");
    unowned_title.set_halign(gtk4::Align::Start);
    unowned_title.set_margin_top(12);
    let unowned_desc = gtk4::Label::new(Some(
        "Older versions did not record which account these belong to. Assign each one to the \
         account you are logged in as, or discard it.",
    ));
    unowned_desc.set_wrap(true);
    unowned_desc.set_halign(gtk4::Align::Start);
    unowned_desc.add_css_class("dim-label");
    let unowned_list = gtk4::ListBox::new();
    unowned_list.set_selection_mode(gtk4::SelectionMode::None);
    unowned_list.add_css_class("boxed-list");

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    content.append(&list_box);
    content.append(&unowned_title);
    content.append(&unowned_desc);
    content.append(&unowned_list);
    let scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build();
    container.append(&scroller);

    // `reload` is defined recursively (rows need to call it after retry/delete),
    // so it's built up via a RefCell cell that gets filled in immediately after.
    let reload_cell: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

    let reload: Rc<dyn Fn()> = {
        let reload_cell = Rc::clone(&reload_cell);
        Rc::new(move || {
            for list in [&list_box, &unowned_list] {
                while let Some(child) = list.first_child() {
                    list.remove(&child);
                }
            }
            let on_changed: Rc<dyn Fn()> = {
                let reload_cell = Rc::clone(&reload_cell);
                Rc::new(move || {
                    if let Some(reload) = reload_cell.borrow().as_ref() {
                        reload();
                    }
                })
            };

            let store = state.store();
            let sessions = match state.account() {
                None => Err("Log in to see your pending submissions.".to_string()),
                Some(account) => store.pending_sessions(&account).ok_or_else(|| {
                    format!(
                        "Pending submissions cannot be read: {}",
                        store.error().unwrap_or_else(|| "session storage is unavailable".into())
                    )
                }),
            };
            let unowned = store.unowned().unwrap_or_default();

            let (sessions, problem) = match sessions {
                Ok(sessions) => (sessions, None),
                Err(message) => (Vec::new(), Some(message)),
            };
            let nothing = sessions.is_empty() && unowned.is_empty();
            match (&problem, nothing) {
                (Some(message), _) => {
                    status_label.set_text(message);
                    status_label.add_css_class("error");
                    status_label.remove_css_class("dim-label");
                    status_label.set_visible(true);
                }
                (None, true) => {
                    status_label.set_text("No pending submissions.");
                    status_label.remove_css_class("error");
                    status_label.add_css_class("dim-label");
                    status_label.set_visible(true);
                }
                (None, false) => status_label.set_visible(false),
            }
            list_box.set_visible(!sessions.is_empty());
            // Adoption needs someone to adopt them.
            let show_unowned = !unowned.is_empty() && state.logged_in();
            unowned_title.set_visible(show_unowned);
            unowned_desc.set_visible(show_unowned);
            unowned_list.set_visible(show_unowned);
            scroller.set_visible(!sessions.is_empty() || show_unowned);

            for session in sessions {
                list_box.append(&build_row(state.clone(), session, Rc::clone(&on_changed)));
            }
            if show_unowned {
                for record in unowned {
                    unowned_list.append(&build_unowned_row(state.clone(), record, Rc::clone(&on_changed)));
                }
            }
        })
    };
    *reload_cell.borrow_mut() = Some(Rc::clone(&reload));

    (container.upcast(), reload)
}

/// The row's subtitle: the session's details, why it is still pending, and, on its own line,
/// the outcome of the last action on it. Status text lives here rather than beside the buttons:
/// an unwrapped suffix label takes the row's width first, which squeezed the title to one
/// character wide.
fn subtitle_for(session: &PendingSession, reason: &str, status: Option<&str>) -> glib::GString {
    let mut subtitle = format!("{}h · {}", session.hours, session.date);
    if let Some(notes) = &session.notes {
        if !notes.is_empty() {
            subtitle.push_str(&format!(" · {notes}"));
        }
    }
    subtitle.push_str(&format!("\n{reason}"));
    if let Some(status) = status {
        subtitle.push_str(&format!("\n{status}"));
    }
    // AdwActionRow's subtitle is Pango markup, and the reason can be a server-provided message
    // containing anything.
    glib::markup_escape_text(&subtitle)
}

fn build_row(state: AppState, session: PendingSession, on_changed: Rc<dyn Fn()>) -> adw::ActionRow {
    // The title is a game title and can contain "&" (e.g. "Ratchet & Clank"): escape it too.
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&session.title))
        .subtitle(subtitle_for(&session, &session.error, None))
        .subtitle_lines(3)
        .build();

    let retry_btn = gtk4::Button::with_label("Retry");
    retry_btn.set_valign(gtk4::Align::Center);
    let delete_btn = gtk4::Button::with_label("Delete");
    delete_btn.set_valign(gtk4::Align::Center);

    row.add_suffix(&retry_btn);
    row.add_suffix(&delete_btn);

    retry_btn.connect_clicked({
        let state = state.clone();
        let session = session.clone();
        let row = row.clone();
        let delete_btn = delete_btn.clone();
        let on_changed = Rc::clone(&on_changed);
        move |btn| {
            btn.set_sensitive(false);
            delete_btn.set_sensitive(false);
            row.set_subtitle(&subtitle_for(&session, &session.error, Some("Submitting…")));

            let state = state.clone();
            let id = session.id.clone();
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let _ = tx.send_blocking(lilypad_core::engine::actions::retry_pending(&state, &id));
            });

            let on_changed = Rc::clone(&on_changed);
            let btn = btn.clone();
            let delete_btn = delete_btn.clone();
            let row = row.clone();
            let session = session.clone();
            glib::spawn_future_local(async move {
                let Ok(result) = rx.recv().await else { return };
                match result {
                    // Submitted, or moved to New Games (its game was deleted): either way it has
                    // left this list, and the reload shows the New Games count.
                    Ok(_) => on_changed(),
                    Err(e) => {
                        btn.set_sensitive(true);
                        delete_btn.set_sensitive(true);
                        row.set_subtitle(&subtitle_for(&session, &e, Some("Retry failed")));
                    }
                }
            });
        }
    });

    delete_btn.connect_clicked({
        let session = session.clone();
        let row = row.clone();
        move |_| {
            let Some(account) = state.account() else { return };
            match state.store().discard(&session.id, &account) {
                Some(_) => on_changed(),
                None => {
                    let reason = state.store().error().unwrap_or_else(|| "Session storage is unavailable".into());
                    row.set_subtitle(&subtitle_for(&session, &reason, Some("Delete failed")));
                }
            }
        }
    });

    row
}

fn build_unowned_row(state: AppState, record: UnownedRecord, on_changed: Rc<dyn Fn()>) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&record.summary))
        .title_lines(2)
        .build();
    let assign_btn = gtk4::Button::with_label("Assign to me");
    assign_btn.set_valign(gtk4::Align::Center);
    let discard_btn = gtk4::Button::with_label("Discard");
    discard_btn.set_valign(gtk4::Align::Center);
    row.add_suffix(&assign_btn);
    row.add_suffix(&discard_btn);

    let report = {
        let state = state.clone();
        let row = row.clone();
        move |outcome: Option<bool>, on_changed: &Rc<dyn Fn()>| match outcome {
            Some(_) => on_changed(),
            None => {
                let reason = state.store().error().unwrap_or_else(|| "Session storage is unavailable".into());
                row.set_subtitle(&glib::markup_escape_text(&format!("That didn't work: {reason}")));
            }
        }
    };

    assign_btn.connect_clicked({
        let state = state.clone();
        let id = record.id.clone();
        let on_changed = Rc::clone(&on_changed);
        let report = report.clone();
        move |_| {
            let Some(account) = state.account() else { return };
            report(state.store().adopt(&id, &account), &on_changed);
        }
    });
    discard_btn.connect_clicked({
        let id = record.id.clone();
        move |_| report(state.store().discard_unowned(&id), &on_changed)
    });

    row
}
