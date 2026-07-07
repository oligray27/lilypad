use adw::prelude::*;
use lilypad_core::config::load_pending_sessions;
use std::rc::Rc;

/// Builds the about/main page. Returns the widget and a `refresh_notice`
/// closure the caller should invoke each time this page is shown, so the
/// pending-submissions notice reflects the current queue (mirrors the Tauri
/// build's `loadMainView()` running on every `show-main` event).
pub fn build(on_show_pending: impl Fn() + 'static, on_show_mappings: impl Fn() + 'static) -> (gtk4::Box, Rc<dyn Fn()>) {
    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    container.set_margin_top(24);
    container.set_margin_bottom(24);
    container.set_margin_start(24);
    container.set_margin_end(24);

    let title = gtk4::Label::new(Some("LilyPad for FrogLog"));
    title.add_css_class("title-1");
    title.set_halign(gtk4::Align::Start);
    container.append(&title);

    let notice_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let notice_label = gtk4::Label::new(None);
    notice_label.set_halign(gtk4::Align::Start);
    let notice_link = gtk4::LinkButton::builder().label("View").uri("#").build();
    notice_link.set_visible(false);
    notice_box.append(&notice_label);
    notice_box.append(&notice_link);
    container.append(&notice_box);

    let on_show_pending = Rc::new(on_show_pending);
    notice_link.connect_activate_link({
        let on_show_pending = Rc::clone(&on_show_pending);
        move |_| {
            on_show_pending();
            glib::Propagation::Stop
        }
    });

    let desc = gtk4::Label::new(Some(
        "A lightweight system tray companion for FrogLog, the personal game tracking app. \
         LilyPad watches for game processes in the background and prompts you to log a \
         session when you stop playing.",
    ));
    desc.set_wrap(true);
    desc.set_halign(gtk4::Align::Start);
    desc.set_justify(gtk4::Justification::Left);
    container.append(&desc);

    let configure_button = gtk4::Button::with_label("Configure Games…");
    configure_button.add_css_class("suggested-action");
    configure_button.set_halign(gtk4::Align::Start);
    configure_button.connect_clicked(move |_| on_show_mappings());
    container.append(&configure_button);

    container.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    let steps_heading = gtk4::Label::new(Some("Getting started"));
    steps_heading.add_css_class("heading");
    steps_heading.set_halign(gtk4::Align::Start);
    container.append(&steps_heading);

    let steps = gtk4::Label::new(Some(
        "1. Right-click the tray icon and choose Configure... to link each game's process to \
         its FrogLog entry.\n\
         2. Launch your game as normal — LilyPad detects it automatically.\n\
         3. When you close the game, this window appears so you can submit the session.",
    ));
    steps.set_wrap(true);
    steps.set_halign(gtk4::Align::Start);
    steps.set_justify(gtk4::Justification::Left);
    steps.add_css_class("dim-label");
    container.append(&steps);

    let refresh_notice: Rc<dyn Fn()> = Rc::new(move || {
        let count = load_pending_sessions().len();
        if count > 0 {
            notice_label.set_text(&format!("⚠ {count} pending submission{} —", if count > 1 { "s" } else { "" }));
            notice_link.set_visible(true);
        } else {
            notice_label.set_text("✓ No pending submissions");
            notice_link.set_visible(false);
        }
    });
    refresh_notice();

    (container, refresh_notice)
}
