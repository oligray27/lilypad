//! System tray via ksni (pure-Rust StatusNotifierItem, no GTK3/libappindicator
//! dependency — see the design plan for why `tray-icon` was rejected).
//!
//! `ksni::Tray::menu()` runs on ksni's own background thread, never the GTK main
//! thread, so menu-item callbacks must not touch GTK widgets directly — they only
//! send a `TrayAction` over a channel; a `glib::spawn_future_local` loop in `app.rs`
//! does the actual GTK-thread work.

use crate::state::AppState;
use ksni::menu::StandardItem;
use ksni::{blocking::TrayMethods, MenuItem};
use std::sync::LazyLock;

/// Decodes an embedded PNG into the raw ARGB32 pixel data ksni transmits directly over D-Bus to
/// the tray host, bypassing icon-theme name lookup entirely (see `icon_pixmap` below for why
/// that lookup can't be relied on).
fn load_icon(bytes: &[u8]) -> ksni::Icon {
    let img = image::load_from_memory(bytes).expect("valid embedded tray icon");
    let (width, height) = image::GenericImageView::dimensions(&img);
    let mut data = img.into_rgba8().into_vec();
    for pixel in data.chunks_exact_mut(4) {
        pixel.rotate_right(1); // RGBA -> ARGB, network byte order
    }
    ksni::Icon {
        width: width as i32,
        height: height as i32,
        data,
    }
}

#[derive(Debug, Clone)]
pub enum TrayAction {
    ShowLogin,
    ShowMain,
    ShowMappings,
    ShowPending,
    ShowNewGames,
    Logout,
    ForceStopTracking,
    Quit,
}

pub struct LilypadTray {
    pub state: AppState,
    pub action_tx: async_channel::Sender<TrayAction>,
}

impl LilypadTray {
    fn send(&self, action: TrayAction) {
        let _ = self.action_tx.send_blocking(action);
    }
}

impl ksni::Tray for LilypadTray {
    fn id(&self) -> String {
        "uk.co.froglog.lilypad".into()
    }

    fn title(&self) -> String {
        "LilyPad".into()
    }

    fn icon_name(&self) -> String {
        if self.state.now_tracking_title().is_some() {
            "uk.co.froglog.lilypad-tracking".into()
        } else {
            "uk.co.froglog.lilypad".into()
        }
    }

    /// Primary icon source: `icon_name` above only resolves if the tray host can find that name
    /// in an installed icon theme, which isn't true for an unintegrated AppImage (no appimaged/
    /// AppImageLauncher registering it system-wide) — it shows a generic/blank icon instead.
    /// Embedding the actual pixels sidesteps icon-theme lookup completely, so the real LilyPad
    /// icon shows regardless of how the app was installed.
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        static NORMAL: LazyLock<ksni::Icon> =
            LazyLock::new(|| load_icon(include_bytes!("../../../src-tauri/icons/128x128.png")));
        static TRACKING: LazyLock<ksni::Icon> =
            LazyLock::new(|| load_icon(include_bytes!("../../../src-tauri/icons/128x128_nowplaying.png")));

        let icon = if self.state.now_tracking_title().is_some() { &*TRACKING } else { &*NORMAL };
        vec![icon.clone()]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let title = if let Some(game) = self.state.now_tracking_title() {
            format!("LilyPad - Now Tracking: {game}")
        } else {
            "LilyPad - FrogLog Auto Tracker".into()
        };
        ksni::ToolTip {
            title,
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = Vec::new();

        if let Some(title) = self.state.now_tracking_title() {
            items.push(
                StandardItem {
                    label: format!("Now Tracking: {title}"),
                    enabled: false,
                    activate: Box::new(|_: &mut Self| {}),
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: "Stop Tracking Current Session".into(),
                    activate: Box::new(|t: &mut Self| t.send(TrayAction::ForceStopTracking)),
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: "Logout".into(),
                    activate: Box::new(|t: &mut Self| t.send(TrayAction::Logout)),
                    ..Default::default()
                }
                .into(),
            );
        } else if self.state.logged_in() {
            items.push(
                StandardItem {
                    label: "Configure…".into(),
                    activate: Box::new(|t: &mut Self| t.send(TrayAction::ShowMappings)),
                    ..Default::default()
                }
                .into(),
            );
            let pending_count = lilypad_core::config::load_pending_sessions().len();
            if pending_count > 0 {
                items.push(
                    StandardItem {
                        label: format!("Pending Submissions ({pending_count})"),
                        activate: Box::new(|t: &mut Self| t.send(TrayAction::ShowPending)),
                        ..Default::default()
                    }
                    .into(),
                );
            }
            let new_games_count = lilypad_core::config::load_pending_game_submissions().len();
            if new_games_count > 0 {
                items.push(
                    StandardItem {
                        label: format!("New Games ({new_games_count})"),
                        activate: Box::new(|t: &mut Self| t.send(TrayAction::ShowNewGames)),
                        ..Default::default()
                    }
                    .into(),
                );
            }
            items.push(
                StandardItem {
                    label: "About".into(),
                    activate: Box::new(|t: &mut Self| t.send(TrayAction::ShowMain)),
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: "Logout".into(),
                    activate: Box::new(|t: &mut Self| t.send(TrayAction::Logout)),
                    ..Default::default()
                }
                .into(),
            );
        } else {
            items.push(
                StandardItem {
                    label: "Login".into(),
                    activate: Box::new(|t: &mut Self| t.send(TrayAction::ShowLogin)),
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: "About".into(),
                    activate: Box::new(|t: &mut Self| t.send(TrayAction::ShowMain)),
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| t.send(TrayAction::Quit)),
                ..Default::default()
            }
            .into(),
        );

        items
    }
}

pub fn spawn(state: AppState, action_tx: async_channel::Sender<TrayAction>) -> Option<ksni::blocking::Handle<LilypadTray>> {
    let tray = LilypadTray { state, action_tx };
    match tray.spawn() {
        Ok(handle) => Some(handle),
        Err(e) => {
            log::warn!("[LilyPad] tray failed to start: {e}");
            None
        }
    }
}

/// Thread-safe handle to trigger a tray refresh. `ksni::blocking::Handle` is
/// `Send + Sync`, so this can be called from background threads (e.g. after an
/// auto-submit finishes) as well as the GTK main thread — unlike an `Rc`-based
/// closure, which could only ever be used on the thread that created it.
pub type RefreshTray = std::sync::Arc<dyn Fn() + Send + Sync>;

pub fn make_refresh_tray(
    tray_handle: std::sync::Arc<std::sync::Mutex<Option<ksni::blocking::Handle<LilypadTray>>>>,
) -> RefreshTray {
    std::sync::Arc::new(move || {
        if let Some(handle) = tray_handle.lock().unwrap().as_ref() {
            handle.update(|_| {});
        }
    })
}
