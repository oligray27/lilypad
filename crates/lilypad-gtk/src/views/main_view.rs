use adw::prelude::*;
use std::rc::Rc;

/// Builds the about/main page. Returns the widget and a no-op `refresh` closure kept for
/// call-site symmetry with the other views (this page has nothing left to refresh now that
/// the pending-submissions notice lives on the Configure page instead).
pub fn build(on_show_mappings: impl Fn() + 'static) -> (gtk4::Box, Rc<dyn Fn()>) {
    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    container.set_margin_top(24);
    container.set_margin_bottom(24);
    container.set_margin_start(24);
    container.set_margin_end(24);

    let title = gtk4::Label::new(Some("LilyPad for FrogLog"));
    title.add_css_class("title-1");
    title.set_halign(gtk4::Align::Start);
    container.append(&title);

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
         2. Launch your game as normal, LilyPad detects it automatically.\n\
         3. When you close the game, this window appears so you can submit the session.",
    ));
    steps.set_wrap(true);
    steps.set_halign(gtk4::Align::Start);
    steps.set_justify(gtk4::Justification::Left);
    steps.add_css_class("dim-label");
    container.append(&steps);

    let refresh: Rc<dyn Fn()> = Rc::new(|| {});

    (container, refresh)
}
