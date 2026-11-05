//! Whether this PC lets phones link and launch games at all. These belong to the install, not to a FrogLog account: pairing is
//! between a phone and this PC, so logging in as someone else must not change who can start games here.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The port LilyPad listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 47821;

fn default_port() -> u16 {
    DEFAULT_PORT
}

/// Ports below 1024 need special rights, so they are not offered.
pub fn valid_port(port: u32) -> bool {
    (1024..=65535).contains(&port)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LanSettings {
    /// "Allow phones to link and launch games". Off by default: nothing listens and nothing is announced.
    #[serde(default)]
    pub enabled: bool,
    /// "Ask me on this PC before launching". Off by default.
    #[serde(default)]
    pub ask_before_launch: bool,
    /// The port to listen on. If it is the default and busy, LilyPad takes any free one; a port chosen here must be free.
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for LanSettings {
    fn default() -> Self {
        LanSettings { enabled: false, ask_before_launch: false, port: DEFAULT_PORT }
    }
}

pub fn default_path() -> PathBuf {
    crate::config::app_data_dir().join("lan_settings.json")
}

pub fn load(path: &Path) -> LanSettings {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

pub fn save(path: &Path, settings: &LanSettings) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(settings).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_is_off_until_chosen() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&dir.path().join("missing.json")), LanSettings { enabled: false, ask_before_launch: false, port: DEFAULT_PORT });
    }

    #[test]
    fn settings_are_saved_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lan_settings.json");
        save(&path, &LanSettings { enabled: true, ask_before_launch: true, port: 50000 }).unwrap();
        assert_eq!(load(&path), LanSettings { enabled: true, ask_before_launch: true, port: 50000 });
    }

    #[test]
    fn a_file_from_before_the_port_existed_gets_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lan_settings.json");
        std::fs::write(&path, r#"{ "enabled": true }"#).unwrap();
        assert_eq!(load(&path).port, DEFAULT_PORT);
        assert!(load(&path).enabled);
    }

    #[test]
    fn only_unprivileged_ports_are_valid() {
        assert!(!valid_port(80));
        assert!(!valid_port(1023));
        assert!(valid_port(1024));
        assert!(valid_port(47821));
        assert!(valid_port(65535));
        assert!(!valid_port(65536));
    }

    #[test]
    fn an_unreadable_file_means_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lan_settings.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(!load(&path).enabled);
    }
}
