use crate::monitor_glue::{self, MonitorEvent};
use crate::notify;
use crate::persistence;
use crate::session_flow::{self, AppAction};
use crate::state::AppState;
use crate::tray::{self, LilypadTray, TrayAction};
use crate::views;
use adw::prelude::*;
use lilypad_core::config::{auth_config_path, AuthConfig};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

const APP_ID: &str = "uk.co.froglog.lilypad";

/// Per-view default window size, mirroring the Tauri build's `VIEW_SIZE` map —
/// the mappings table in particular needs much more width than the other views.
fn view_size(view: &str) -> (i32, i32) {
    match view {
        "mappings" => (780, 800),
        "pending" => (560, 480),
        "new_games" => (560, 480),
        "watched_dirs" => (550, 480),
        "main" => (560, 340),
        _ => (560, 420),
    }
}

/// Switches the visible stack page and resizes the window to match.
///
/// Windows grow automatically to fit larger content but never auto-shrink —
/// confirmed both ways by an isolated reproduction using this exact widget
/// structure (AdwApplicationWindow + AdwToolbarView + AdwHeaderBar + a
/// non-homogeneous, Crossfade-transitioning Stack): a synchronous
/// `set_visible_child_name` immediately followed by a synchronous
/// `set_default_size` resizes correctly in both directions every time. (Two
/// other approaches were tried and discarded: deferring the resize via
/// `idle_add` fixed shrinking but made the content swap and resize visibly
/// two separate steps; relying purely on each page's `set_size_request` with
/// no imperative resize call only ever grows, matching that "windows don't
/// auto-shrink" rule.)
fn goto_view(window: &adw::ApplicationWindow, stack: &gtk4::Stack, view: &'static str) {
    stack.set_visible_child_name(view);
    let (w, h) = view_size(view);
    window.set_default_size(w, h);
}

/// Where the shared header bar's Back button should navigate to from a given view, mirroring
/// the Tauri build's per-view Back buttons (`pendingBack`, `newGamesBack`, `watchedDirsBack`
/// in `main.js`). Views not listed here (e.g. `main`, `mappings`, reachable directly from the
/// tray) get no Back button.
fn back_target(view: &str) -> Option<&'static str> {
    match view {
        "pending" => Some("main"),
        "new_games" => Some("mappings"),
        "watched_dirs" => Some("mappings"),
        _ => None,
    }
}

pub fn run(state: AppState) -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();

    // GApplication is single-instance per application-id: launching the binary
    // again while an instance is already running doesn't start a new process,
    // it just re-fires `activate` on the existing one over D-Bus. Without this
    // guard that would rebuild an entire second window/tray/monitor-thread set
    // inside the already-running process instead of just refocusing it.
    app.connect_activate(move |app| {
        if let Some(window) = app.windows().first() {
            window.present();
            return;
        }
        build_window(app, state.clone());
    });

    app.run()
}

fn build_window(app: &adw::Application, state: AppState) {
    // Tray, built before the views so its refresh closure can be handed to them.
    // Arc<Mutex<_>> (not Rc<RefCell<_>>) because auto-submit results need to
    // refresh the tray from a background thread (session_flow.rs), not just the
    // GTK main thread.
    let tray_handle: Arc<Mutex<Option<ksni::blocking::Handle<LilypadTray>>>> = Arc::new(Mutex::new(None));
    let (action_tx, action_rx) = async_channel::unbounded::<TrayAction>();
    *tray_handle.lock().unwrap() = tray::spawn(state.clone(), action_tx);
    let refresh_tray = tray::make_refresh_tray(tray_handle);

    let header_bar = adw::HeaderBar::new();

    // Shared across every stack page (see the header-bar comment below); shown/hidden per
    // page by the `visible-child-name` handler wired up after the stack is built.
    let back_button = gtk4::Button::from_icon_name("go-previous-symbolic");
    back_button.set_tooltip_text(Some("Back"));
    back_button.set_visible(false);
    header_bar.pack_start(&back_button);

    // Version + link to the latest release. The Tauri build showed this on every
    // individual view (each had its own `.app-version` span); here the header bar
    // is shared across all stack pages, so adding it once covers every screen.
    // GTK/Adwaita buttons carry their own internal padding regardless of the
    // containing Box's spacing, so tightening this up needs an actual CSS
    // override on the button itself, not just spacing/margin properties.
    let css = gtk4::CssProvider::new();
    css.load_from_string(".lilypad-version-link { padding: 0; margin: 0; min-height: 0; min-width: 0; }");
    gtk4::style_context_add_provider_for_display(
        &gtk4::prelude::WidgetExt::display(&header_bar),
        &css,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let version_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    let version_label = gtk4::Label::new(Some(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    version_label.add_css_class("dim-label");
    version_box.append(&version_label);
    let releases_link = gtk4::LinkButton::builder()
        .label("(?)")
        .uri("https://github.com/oligray27/lilypad/releases/latest")
        .tooltip_text("View releases")
        .build();
    releases_link.add_css_class("flat");
    releases_link.add_css_class("dim-label");
    releases_link.add_css_class("lilypad-version-link");
    version_box.append(&releases_link);
    header_bar.pack_end(&version_box);

    let stack = gtk4::Stack::new();
    stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
    // Without this, Stack sizes itself to its largest child (the mappings page)
    // regardless of which page is visible, so per-view window sizing below would
    // never actually let the window shrink back down on shorter pages.
    stack.set_vhomogeneous(false);
    stack.set_hhomogeneous(false);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header_bar);
    toolbar_view.set_content(Some(&stack));

    // Built before the views so the mappings/session views can use it (file-dialog
    // parent, session popup transient-for).
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("LilyPad - FrogLog Auto Tracker")
        .default_width(560)
        .default_height(420)
        .content(&toolbar_view)
        .build();

    // Tray-resident: closing the window hides it instead of quitting the app.
    window.connect_close_request(|window| {
        window.set_visible(false);
        glib::Propagation::Stop
    });

    let login_widget = views::login::build(state.clone(), {
        let stack = stack.clone();
        let window = window.clone();
        let refresh_tray = refresh_tray.clone();
        move || {
            goto_view(&window, &stack, "main");
            refresh_tray();
        }
    });
    let (pending_widget, reload_pending) = views::pending::build(state.clone());
    let (new_games_widget, reload_new_games) = views::new_games::build(state.clone(), refresh_tray.clone());
    let (watched_dirs_widget, reload_watched_dirs) = views::watched_dirs::build(state.clone(), window.clone().upcast());
    let (mappings_widget, reload_mappings) = views::mappings::build(
        state.clone(),
        window.clone().upcast(),
        {
            let stack = stack.clone();
            let window = window.clone();
            let reload_watched_dirs = Rc::clone(&reload_watched_dirs);
            move || {
                goto_view(&window, &stack, "watched_dirs");
                reload_watched_dirs();
            }
        },
        {
            let stack = stack.clone();
            let window = window.clone();
            let reload_new_games = Rc::clone(&reload_new_games);
            move || {
                goto_view(&window, &stack, "new_games");
                reload_new_games();
            }
        },
        {
            let stack = stack.clone();
            let window = window.clone();
            let reload_pending = Rc::clone(&reload_pending);
            move || {
                goto_view(&window, &stack, "pending");
                reload_pending();
            }
        },
    );
    let (main_widget, refresh_main_notice) = views::main_view::build({
        let stack = stack.clone();
        let window = window.clone();
        let reload_mappings = Rc::clone(&reload_mappings);
        move || {
            goto_view(&window, &stack, "mappings");
            reload_mappings();
        }
    });

    stack.add_named(&login_widget, Some("login"));
    stack.add_named(&main_widget, Some("main"));
    stack.add_named(&mappings_widget, Some("mappings"));
    stack.add_named(&pending_widget, Some("pending"));
    stack.add_named(&new_games_widget, Some("new_games"));
    stack.add_named(&watched_dirs_widget, Some("watched_dirs"));
    let initial_view = if state.logged_in() { "main" } else { "login" };
    stack.set_visible_child_name(initial_view);
    {
        let (w, h) = view_size(initial_view);
        window.set_default_size(w, h);
    }

    if !state.logged_in() {
        window.present();
    }

    // Back button: visibility tracks whatever page is current (covers navigation from the
    // tray too, not just in-window Back/Next flows), click handler jumps to that page's
    // configured parent and reloads it the same way the tray actions do.
    stack.connect_visible_child_name_notify({
        let back_button = back_button.clone();
        move |stack| {
            let visible = stack.visible_child_name().unwrap_or_default();
            back_button.set_visible(back_target(&visible).is_some());
        }
    });
    back_button.connect_clicked({
        let window = window.clone();
        let stack = stack.clone();
        let reload_mappings = Rc::clone(&reload_mappings);
        let refresh_main_notice = Rc::clone(&refresh_main_notice);
        move |_| {
            let current = stack.visible_child_name().unwrap_or_default();
            if let Some(target) = back_target(&current) {
                goto_view(&window, &stack, target);
                match target {
                    "main" => refresh_main_notice(),
                    "mappings" => reload_mappings(),
                    _ => {}
                }
            }
        }
    });

    // Tray menu actions.
    let (app_tx, app_rx) = async_channel::unbounded::<AppAction>();
    glib::spawn_future_local({
        let state = state.clone();
        let window = window.clone();
        let stack = stack.clone();
        let app = app.clone();
        let refresh_tray = refresh_tray.clone();
        let app_tx = app_tx.clone();
        let reload_mappings = Rc::clone(&reload_mappings);
        let reload_pending = Rc::clone(&reload_pending);
        let reload_new_games = Rc::clone(&reload_new_games);
        let refresh_main_notice = Rc::clone(&refresh_main_notice);
        async move {
            while let Ok(action) = action_rx.recv().await {
                match action {
                    TrayAction::ShowLogin => {
                        goto_view(&window, &stack, "login");
                        window.present();
                    }
                    TrayAction::ShowMain => {
                        goto_view(&window, &stack, "main");
                        window.present();
                        refresh_main_notice();
                    }
                    TrayAction::ShowMappings => {
                        goto_view(&window, &stack, "mappings");
                        window.present();
                        reload_mappings();
                    }
                    TrayAction::ShowPending => {
                        goto_view(&window, &stack, "pending");
                        window.present();
                        reload_pending();
                    }
                    TrayAction::ShowNewGames => {
                        goto_view(&window, &stack, "new_games");
                        window.present();
                        reload_new_games();
                    }
                    TrayAction::Logout => {
                        {
                            let mut auth = state.auth.write().unwrap();
                            *auth = AuthConfig::default();
                            let _ = auth.save_to(&auth_config_path());
                        }
                        goto_view(&window, &stack, "login");
                        window.present();
                        refresh_tray();
                    }
                    TrayAction::ForceStopTracking => {
                        // Capture session info before clearing so it can still be
                        // submitted; block re-tracking until the process actually exits
                        // (checked via force_stopped_process in the monitor-event loop
                        // below, mirroring the Tauri build).
                        let session_info = {
                            let sess = state.current_session.read().unwrap();
                            sess.as_ref().map(|s| (s.process_name.clone(), s.mapping.clone(), s.started_at.elapsed().as_secs_f64()))
                        };
                        if let Some((process_name, mapping, duration_secs)) = session_info {
                            *state.force_stopped_process.write().unwrap() = Some(process_name.clone());
                            *state.current_session.write().unwrap() = None;
                            persistence::clear_persisted_session();
                            session_flow::handle_session_ended(
                                state.clone(),
                                refresh_tray.clone(),
                                app_tx.clone(),
                                process_name,
                                mapping,
                                duration_secs,
                                true,
                            );
                        }
                        refresh_tray();
                    }
                    TrayAction::Quit => {
                        app.quit();
                    }
                }
            }
        }
    });

    // Session-ended popups, requested either by session_flow (auto-submit
    // interception / disabled), the force-stop tray action, or crash recovery.
    glib::spawn_future_local({
        let state = state.clone();
        let window = window.clone();
        let stack = stack.clone();
        let refresh_tray = refresh_tray.clone();
        let reload_new_games = Rc::clone(&reload_new_games);
        async move {
            while let Ok(action) = app_rx.recv().await {
                match action {
                    AppAction::ShowSessionPopup(data) => {
                        views::session::show_popup(state.clone(), window.upcast_ref(), refresh_tray.clone(), data);
                    }
                    AppAction::GoToNewGames => {
                        goto_view(&window, &stack, "new_games");
                        window.present();
                        reload_new_games();
                    }
                }
            }
        }
    });

    // Recover a session interrupted by a crash/restart, before the monitor starts
    // polling (so it sees current_session already populated and doesn't try to
    // start tracking the same still-running process as if it were new).
    persistence::recover_on_startup(state.clone(), refresh_tray.clone(), app_tx.clone());

    // Refresh the installed-Steam-games scan and the FrogLog library index
    // periodically in the background, so newly installed games or newly logged
    // games are picked up without restarting LilyPad.
    {
        let refresh_state = state.clone();
        std::thread::spawn(move || loop {
            refresh_state.refresh_installed_games();
            refresh_state.refresh_library_index();
            std::thread::sleep(std::time::Duration::from_secs(300));
        });
    }

    // Process monitor: real background threads (lilypad-core, unchanged from the
    // Tauri build) pushing events consumed here on the GTK main loop.
    let (mon_tx, mon_rx) = async_channel::unbounded::<MonitorEvent>();
    monitor_glue::start(&state, mon_tx);

    glib::spawn_future_local({
        let refresh_tray = refresh_tray.clone();
        async move {
            while let Ok(event) = mon_rx.recv().await {
                match event {
                    MonitorEvent::SessionStarted { mapping, .. } => {
                        persistence::persist_session_start(&mapping);
                        session_flow::handle_session_started(&state, &mapping);
                        let name = mapping.title.clone().unwrap_or(mapping.process.clone());
                        notify::show("Tracking Started", &name);
                        refresh_tray();
                    }
                    MonitorEvent::SessionEnded {
                        process_name,
                        mapping,
                        duration_secs,
                    } => {
                        persistence::clear_persisted_session();
                        // A process that was force-stopped is still running; the monitor
                        // will report its real exit later. That was already handled
                        // synchronously by the force-stop tray action, so skip it here.
                        let was_force_stopped = {
                            let mut fp = state.force_stopped_process.write().unwrap();
                            if fp.as_deref().map(|s| s.eq_ignore_ascii_case(&process_name)).unwrap_or(false) {
                                *fp = None;
                                true
                            } else {
                                false
                            }
                        };
                        if !was_force_stopped {
                            session_flow::handle_session_ended(
                                state.clone(),
                                refresh_tray.clone(),
                                app_tx.clone(),
                                process_name,
                                mapping,
                                duration_secs,
                                false,
                            );
                        }
                        refresh_tray();
                    }
                    MonitorEvent::UnmappedGameSessionEnded { title, appid, exe_name, duration_secs } => {
                        let raw_hours = duration_secs / 3600.0;
                        let hours = {
                            let r = (raw_hours * 100.0).round() / 100.0;
                            if r < 0.01 { 0.01 } else { r }
                        };
                        lilypad_core::config::record_pending_game_submission(&appid, &title, &exe_name, hours);
                        let total_mins = (duration_secs / 60.0).round() as u64;
                        let time_str = if total_mins >= 60 {
                            format!("{}h {}m", total_mins / 60, total_mins % 60)
                        } else {
                            format!("{total_mins}m")
                        };
                        let app_tx = app_tx.clone();
                        notify::show_with_action(
                            "Session Recorded",
                            &format!("{title} ({time_str}) isn't in your FrogLog yet."),
                            "Go to New Games",
                            move || {
                                let _ = app_tx.send_blocking(AppAction::GoToNewGames);
                            },
                        );
                        refresh_tray();
                    }
                }
            }
        }
    });
}
