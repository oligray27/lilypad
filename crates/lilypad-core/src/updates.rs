//! Update awareness: notices when a newer LilyPad release is out. It never downloads or installs
//! anything -- each frontend points the user at the release instead.
//!
//! GitHub's "latest release" endpoint (which already skips drafts and pre-releases) is checked
//! shortly after start-up and then once a day. The newest release found is kept in memory
//! (`available`) for tray menus and panels to read at any time. A small file in the app data
//! directory records which version the user has already been *notified* about, so the desktop
//! notification or Steam toast appears once per release rather than on every launch. On Linux the
//! GTK app and the Gaming Mode engine share that directory, so a release is announced once between
//! them.

use crate::config::app_data_dir;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex, RwLock};
use std::time::Duration;

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/oligray27/lilypad/releases/latest";
/// Where to send the user when there is no better, platform-specific download.
pub const RELEASES_PAGE: &str = "https://github.com/oligray27/lilypad/releases/latest";

/// Delay before the first check, so it doesn't compete with start-up (login, library refresh).
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(20);
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// A release newer than the running version.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpdateInfo {
    /// Without the tag's leading `v`, e.g. `0.6.3`.
    pub version: String,
    /// The release's own page: notes plus every platform's download.
    pub page_url: String,
    /// The Windows installer asset, when the release has one.
    pub windows_installer_url: Option<String>,
    /// The Decky plugin zip (`LilyPad-<version>.zip`), which the plugin can hand to Decky's own
    /// installer; see `fetch_decky_package`.
    pub decky_zip_url: Option<String>,
    /// The release's `SHA256SUMS` asset, when it has one.
    pub checksums_url: Option<String>,
}

/// The Decky plugin zip of an update, ready for Decky's installer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeckyPackage {
    pub url: String,
    /// From the release's `SHA256SUMS`, for Decky to verify the download against. `None` when the
    /// release has no checksum for the zip (or it couldn't be fetched); Decky then installs
    /// without verifying, as its own "Install from URL" does.
    pub sha256: Option<String>,
}

impl UpdateInfo {
    /// Where this platform's "update" action should go: the Windows installer when there is one,
    /// otherwise the release page (Linux has three package formats, and the Decky plugin a zip,
    /// so the user picks there).
    pub fn download_url(&self) -> &str {
        if cfg!(windows) {
            if let Some(url) = &self.windows_installer_url {
                return url;
            }
        }
        &self.page_url
    }
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// `v0.6.2` / `0.6.2` / `0.6.2-beta.1` -> (0, 6, 2). Anything after `-` or `+` is ignored, so a
/// pre-release is never treated as newer than the same numbered release.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next().unwrap_or(Some(0))?;
    let patch = parts.next().unwrap_or(Some(0))?;
    Some((major, minor, patch))
}

fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// The update a release represents for `current`, if it is one.
fn update_from(release: &Release, current: &str) -> Option<UpdateInfo> {
    if release.draft || release.prerelease || !is_newer(&release.tag_name, current) {
        return None;
    }
    let asset_url = |matches: &dyn Fn(&str) -> bool| {
        release
            .assets
            .iter()
            .find(|a| matches(&a.name.to_ascii_lowercase()))
            .map(|a| a.browser_download_url.clone())
    };
    Some(UpdateInfo {
        version: release.tag_name.trim_start_matches(['v', 'V']).to_string(),
        page_url: release.html_url.clone(),
        windows_installer_url: asset_url(&|name| name.ends_with("-setup.exe")),
        decky_zip_url: asset_url(&|name| name.starts_with("lilypad-") && name.ends_with(".zip")),
        checksums_url: asset_url(&|name| name == "sha256sums"),
    })
}

fn http_client(current: &str) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        // GitHub's API rejects requests without a User-Agent.
        .user_agent(format!("LilyPad/{current}"))
        .build()
        .map_err(|e| e.to_string())
}

/// The checksum listed for `file_name` in `sha256sum` output ("<hash>  <name>", or
/// "<hash> *<name>" in binary mode), lower-cased.
fn sha256_in_sums(sums: &str, file_name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.trim().split_once(char::is_whitespace)?;
        let name = name.trim_start().trim_start_matches('*');
        (name == file_name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

/// The update's Decky plugin zip and its checksum, fetching the release's `SHA256SUMS`. `None`
/// when the release has no plugin zip.
pub fn fetch_decky_package(update: &UpdateInfo, current: &str) -> Option<DeckyPackage> {
    let url = update.decky_zip_url.clone()?;
    let file_name = url.rsplit('/').next().unwrap_or_default().to_string();
    let sha256 = update.checksums_url.as_deref().and_then(|sums_url| {
        let fetched = http_client(current)
            .and_then(|client| client.get(sums_url).send().map_err(|e| e.to_string()))
            .and_then(|response| response.error_for_status().map_err(|e| e.to_string()))
            .and_then(|response| response.text().map_err(|e| e.to_string()));
        match fetched {
            Ok(sums) => sha256_in_sums(&sums, &file_name),
            Err(e) => {
                log::warn!("[LilyPad] could not fetch the release checksums: {e}");
                None
            }
        }
    });
    if sha256.is_none() {
        log::warn!("[LilyPad] no checksum for {file_name}; the update will install unverified");
    }
    Some(DeckyPackage { url, sha256 })
}

fn fetch_latest(current: &str) -> Result<Release, String> {
    let client = http_client(current)?;
    let response = client
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("GitHub returned {}", response.status()));
    }
    response.json::<Release>().map_err(|e| e.to_string())
}

/// Checks once. `Ok(None)` means up to date.
pub fn check(current: &str) -> Result<Option<UpdateInfo>, String> {
    Ok(update_from(&fetch_latest(current)?, current))
}

static AVAILABLE: RwLock<Option<UpdateInfo>> = RwLock::new(None);

/// The newest release found by the background checker, if it is newer than this build. Always
/// `None` while update checks are turned off.
pub fn available() -> Option<UpdateInfo> {
    AVAILABLE.read().unwrap().clone()
}

/// Wakes the checker early (checks just turned back on). Set by `spawn_checker`.
static WAKE: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);

/// The user's choice, app-wide (not per account): `update-settings.json` in the app data
/// directory, `{"check_for_updates": bool}`. Written by the Windows installer's "Automatically
/// check for updates?" question (src-tauri/windows/installer-hooks.nsh -- keep the format in
/// step) and by the Configure checkbox on Windows and Linux. On Linux the Gaming Mode engine
/// shares the file, so the desktop app's checkbox covers it too.
#[derive(Debug, Serialize, Deserialize)]
struct UpdateSettings {
    #[serde(default = "enabled_by_default")]
    check_for_updates: bool,
}

fn enabled_by_default() -> bool {
    true
}

fn settings_path() -> PathBuf {
    app_data_dir().join("update-settings.json")
}

/// Missing or unreadable means on: installs from before the setting existed, Linux packages
/// (no install-time question) and silent Windows installs all check unless the user opts out.
fn read_enabled(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<UpdateSettings>(s.trim_start_matches('\u{feff}')).ok())
        .map_or(true, |s| s.check_for_updates)
}

fn write_enabled(path: &Path, enabled: bool) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string(&UpdateSettings { check_for_updates: enabled }).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

/// Whether automatic update checks are on.
pub fn checks_enabled() -> bool {
    read_enabled(&settings_path())
}

/// Turns automatic update checks on or off. Off also drops any update already found, so
/// frontends refreshing afterwards stop showing it; on checks straight away.
pub fn set_checks_enabled(enabled: bool) -> Result<(), String> {
    write_enabled(&settings_path(), enabled)?;
    if enabled {
        if let Some(wake) = WAKE.lock().unwrap().as_ref() {
            let _ = wake.send(());
        }
    } else {
        *AVAILABLE.write().unwrap() = None;
    }
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct NotifiedRecord {
    notified_version: Option<String>,
}

fn notified_path() -> PathBuf {
    app_data_dir().join("update-check.json")
}

/// Records `version` as announced; returns whether it hadn't been already (i.e. whether to show
/// the one-off notification now).
fn claim_notification(path: &Path, version: &str) -> bool {
    let record: NotifiedRecord = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if record.notified_version.as_deref() == Some(version) {
        return false;
    }
    let record = NotifiedRecord { notified_version: Some(version.to_string()) };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(path, serde_json::to_string(&record).unwrap_or_default()) {
        // Still notify: failing to remember only means a repeat notice next launch.
        log::warn!("[LilyPad] could not record the update notice: {e}");
    }
    true
}

/// Starts the background checker. `on_update(info, notify)` runs on the checker's thread each
/// time a newer release is found (after start-up, then daily while one is still out); `notify`
/// is true only the first time this release is seen, for the one-off notification. Frontends
/// that need their main thread must hop there themselves. Check failures (offline, rate limit)
/// are logged and retried at the next interval.
///
/// `LILYPAD_PRETEND_VERSION` (e.g. `0.0.1`) overrides the running version for this check only,
/// so the notices can be tested without an older build (release test plan, section K).
pub fn spawn_checker(current: String, on_update: impl Fn(UpdateInfo, bool) + Send + 'static) {
    let current = match std::env::var("LILYPAD_PRETEND_VERSION") {
        Ok(pretend) if !pretend.trim().is_empty() => {
            log::info!("[LilyPad] update check pretending to be {pretend} (LILYPAD_PRETEND_VERSION)");
            pretend.trim().to_string()
        }
        _ => current,
    };
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    *WAKE.lock().unwrap() = Some(wake_tx);
    std::thread::spawn(move || {
        let wait = |timeout: Duration| {
            // The sender lives in WAKE for the process's life, so this only returns early
            // when woken; the sleep is just a guard against ever spinning.
            if let Err(mpsc::RecvTimeoutError::Disconnected) = wake_rx.recv_timeout(timeout) {
                std::thread::sleep(timeout);
            }
        };
        wait(FIRST_CHECK_DELAY);
        loop {
            if !checks_enabled() {
                log::info!("[LilyPad] update checks are turned off");
                *AVAILABLE.write().unwrap() = None;
            } else {
                match check(&current) {
                    Ok(Some(info)) => {
                        log::info!("[LilyPad] update available: {} (running {current})", info.version);
                        *AVAILABLE.write().unwrap() = Some(info.clone());
                        let notify = claim_notification(&notified_path(), &info.version);
                        on_update(info, notify);
                    }
                    Ok(None) => log::info!("[LilyPad] up to date ({current})"),
                    Err(e) => log::warn!("[LilyPad] update check failed: {e}"),
                }
            }
            wait(CHECK_INTERVAL);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, assets: &[&str]) -> Release {
        Release {
            tag_name: tag.into(),
            html_url: format!("https://github.com/oligray27/lilypad/releases/tag/{tag}"),
            draft: false,
            prerelease: false,
            assets: assets
                .iter()
                .map(|name| Asset { name: name.to_string(), browser_download_url: format!("https://dl/{name}") })
                .collect(),
        }
    }

    #[test]
    fn versions_compare_numerically_not_as_text() {
        assert!(is_newer("v0.6.10", "0.6.9"));
        assert!(is_newer("v0.7.0", "0.6.2"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("v0.6.2", "0.6.2"));
        assert!(!is_newer("v0.6.1", "0.6.2"));
        // A pre-release suffix never makes the same number newer, and junk is never newer.
        assert!(!is_newer("v0.6.2-beta.1", "0.6.2"));
        assert!(!is_newer("latest", "0.6.2"));
    }

    #[test]
    fn a_newer_release_becomes_an_update_with_its_installer() {
        let r = release("v0.6.3", &["LilyPad-0.6.3.zip", "LilyPad_0.6.3_x64-setup.exe", "SHA256SUMS"]);
        let info = update_from(&r, "0.6.2").expect("newer");
        assert_eq!(info.version, "0.6.3");
        assert_eq!(info.windows_installer_url.as_deref(), Some("https://dl/LilyPad_0.6.3_x64-setup.exe"));
        assert!(info.page_url.ends_with("/v0.6.3"));
    }

    #[test]
    fn same_older_draft_and_prerelease_are_not_updates() {
        assert_eq!(update_from(&release("v0.6.2", &[]), "0.6.2"), None);
        assert_eq!(update_from(&release("v0.6.1", &[]), "0.6.2"), None);
        let mut draft = release("v0.7.0", &[]);
        draft.draft = true;
        assert_eq!(update_from(&draft, "0.6.2"), None);
        let mut pre = release("v0.7.0", &[]);
        pre.prerelease = true;
        assert_eq!(update_from(&pre, "0.6.2"), None);
    }

    #[test]
    fn the_decky_zip_and_checksums_are_found_but_not_confused_with_other_assets() {
        let r = release("v0.6.3", &["LilyPad-x86_64.AppImage", "LilyPad-0.6.3.zip", "SHA256SUMS", "LilyPad_0.6.3_x64-setup.exe"]);
        let info = update_from(&r, "0.6.2").unwrap();
        assert_eq!(info.decky_zip_url.as_deref(), Some("https://dl/LilyPad-0.6.3.zip"));
        assert_eq!(info.checksums_url.as_deref(), Some("https://dl/SHA256SUMS"));
        let bare = update_from(&release("v0.6.3", &["LilyPad-x86_64.AppImage"]), "0.6.2").unwrap();
        assert_eq!((bare.decky_zip_url, bare.checksums_url), (None, None));
    }

    #[test]
    fn checksums_are_read_from_sha256sum_output() {
        let hash = "5a9260fd3dbd8c7c6dee671d0a3c0bc059959d8648a98865748bffc12664018b";
        let sums = format!(
            "31f6305b8eab51fbf1d576ac45a510cf6d148a682d1ee746213ef84719ecc078  lilypad-gtk_0.6.2-1_amd64.deb\r\n\
             {}  LilyPad-0.6.2.zip\n",
            hash.to_uppercase()
        );
        assert_eq!(sha256_in_sums(&sums, "LilyPad-0.6.2.zip").as_deref(), Some(hash));
        assert_eq!(sha256_in_sums(&format!("{hash} *LilyPad-0.6.2.zip"), "LilyPad-0.6.2.zip").as_deref(), Some(hash));
        // Only an exact file name, and only a real SHA-256.
        assert_eq!(sha256_in_sums(&sums, "LilyPad-0.6.zip"), None);
        assert_eq!(sha256_in_sums("abc123  LilyPad-0.6.2.zip", "LilyPad-0.6.2.zip"), None);
    }

    #[test]
    fn a_release_without_an_installer_falls_back_to_its_page() {
        let info = update_from(&release("v0.6.3", &["lilypad-gtk_0.6.3-1_amd64.deb"]), "0.6.2").unwrap();
        assert_eq!(info.windows_installer_url, None);
        assert_eq!(info.download_url(), info.page_url);
    }

    #[test]
    fn each_release_is_announced_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("update-check.json");
        assert!(claim_notification(&path, "0.6.3"));
        assert!(!claim_notification(&path, "0.6.3"));
        assert!(claim_notification(&path, "0.6.4"));
        assert!(!claim_notification(&path, "0.6.4"));
    }

    #[test]
    fn checks_default_on_and_follow_the_saved_choice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("update-settings.json");
        assert!(read_enabled(&path), "no file yet: on");
        write_enabled(&path, false).unwrap();
        assert!(!read_enabled(&path));
        write_enabled(&path, true).unwrap();
        assert!(read_enabled(&path));
        std::fs::write(&path, "garbage").unwrap();
        assert!(read_enabled(&path), "unreadable: on");
    }

    /// Byte for byte what the Windows installer hook writes (installer-hooks.nsh), including a
    /// BOM in case an editor or a later NSIS change adds one.
    #[test]
    fn the_installers_file_is_understood() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-settings.json");
        std::fs::write(&path, r#"{"check_for_updates":false}"#).unwrap();
        assert!(!read_enabled(&path));
        std::fs::write(&path, "\u{feff}{\"check_for_updates\":false}\r\n").unwrap();
        assert!(!read_enabled(&path));
        std::fs::write(&path, r#"{"check_for_updates":true}"#).unwrap();
        assert!(read_enabled(&path));
    }

    #[test]
    fn a_corrupt_record_still_allows_the_notice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(claim_notification(&path, "0.6.3"));
        assert!(!claim_notification(&path, "0.6.3"));
    }

    /// Hits the real GitHub API: `cargo test -p lilypad-core updates -- --ignored`.
    #[test]
    #[ignore = "network"]
    fn live_check_against_github() {
        let info = check("0.0.1").expect("GitHub reachable").expect("any release is newer than 0.0.1");
        assert!(parse_version(&info.version).is_some());
        assert!(info.windows_installer_url.is_some(), "releases ship a Windows installer");
        assert_eq!(check(&info.version).unwrap(), None, "the latest release is not newer than itself");
        let package = fetch_decky_package(&info, "0.0.1").expect("releases ship the Decky zip");
        assert_eq!(package.sha256.map(|h| h.len()), Some(64), "SHA256SUMS lists the Decky zip");
    }

    #[test]
    fn github_release_json_parses() {
        let json = r#"{"tag_name":"v0.6.3","html_url":"https://github.com/x/releases/tag/v0.6.3",
            "draft":false,"prerelease":false,"name":"LilyPad v0.6.3",
            "assets":[{"name":"LilyPad_0.6.3_x64-setup.exe","browser_download_url":"https://dl/setup.exe","size":1}]}"#;
        let r: Release = serde_json::from_str(json).unwrap();
        assert_eq!(update_from(&r, "0.6.2").unwrap().windows_installer_url.as_deref(), Some("https://dl/setup.exe"));
    }
}
