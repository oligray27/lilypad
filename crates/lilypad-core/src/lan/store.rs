//! The phones this PC has paired with: a name and the SHA-256 fingerprint of each phone's certificate. Only a certificate listed
//! here is let past the TLS handshake, and removing a phone from the list is how it is revoked (it takes effect on its next
//! request, with no server involved).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairedPhone {
    pub name: String,
    pub fingerprint: String,
    /// When it was paired, RFC 3339.
    pub added_at: String,
}

/// The list from before devices were kept per account. It is adopted by the first account that signs in afterwards.
pub fn legacy_path() -> PathBuf {
    crate::config::app_data_dir().join("lan_paired.json")
}

/// Where one account's paired devices are listed. Devices belong to the FrogLog account LilyPad was signed in as when they linked,
/// because the games they ask for are that account's (a game id means a different game in another account's library), so signing in
/// as someone else must neither show nor honour them.
pub fn path_for_account(key: &str) -> PathBuf {
    crate::config::app_data_dir().join(format!("lan_paired-{key}.json"))
}

/// Whether another account's list (a `lan_paired*.json` beside this one) holds the device: it is linked, but to a different account.
pub fn known_elsewhere(current: &Path, fingerprint: &str) -> bool {
    let Some(dir) = current.parent() else { return false };
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    entries.flatten().any(|entry| {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        path != current && name.starts_with("lan_paired") && name.ends_with(".json") && is_paired(&path, fingerprint)
    })
}

pub fn load(path: &Path) -> Vec<PairedPhone> {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn save(path: &Path, phones: &[PairedPhone]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(phones).unwrap_or_default())
}

pub fn is_paired(path: &Path, fingerprint: &str) -> bool {
    load(path).iter().any(|phone| phone.fingerprint == fingerprint)
}

/// Adds a phone, or renames it if its certificate is already listed.
pub fn add(path: &Path, name: &str, fingerprint: &str) -> std::io::Result<()> {
    let mut phones = load(path);
    let name: String = name.trim().chars().take(60).collect();
    let name = if name.is_empty() { "Phone".to_string() } else { name };
    match phones.iter_mut().find(|phone| phone.fingerprint == fingerprint) {
        Some(existing) => existing.name = name,
        None => phones.push(PairedPhone { name, fingerprint: fingerprint.to_string(), added_at: chrono::Utc::now().to_rfc3339() }),
    }
    save(path, &phones)
}

/// Renames a linked device. Returns whether it was there. A blank name becomes "Remote device".
pub fn rename(path: &Path, fingerprint: &str, name: &str) -> std::io::Result<bool> {
    let mut phones = load(path);
    let name: String = name.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(60).collect();
    let name = if name.is_empty() { "Remote device".to_string() } else { name };
    let Some(phone) = phones.iter_mut().find(|phone| phone.fingerprint == fingerprint) else { return Ok(false) };
    phone.name = name;
    save(path, &phones)?;
    Ok(true)
}

/// Removes one phone. Returns whether it was there.
pub fn remove(path: &Path, fingerprint: &str) -> std::io::Result<bool> {
    let mut phones = load(path);
    let before = phones.len();
    phones.retain(|phone| phone.fingerprint != fingerprint);
    save(path, &phones)?;
    Ok(phones.len() != before)
}

/// Unpairs every phone.
pub fn clear(path: &Path) -> std::io::Result<()> {
    save(path, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lan_paired.json");
        (dir, path)
    }

    #[test]
    fn nothing_is_paired_at_first() {
        let (_dir, path) = temp();
        assert!(load(&path).is_empty());
        assert!(!is_paired(&path, "abc"));
    }

    #[test]
    fn a_added_phone_is_paired_until_removed() {
        let (_dir, path) = temp();
        add(&path, "Oli's Fold", "abc").unwrap();
        assert!(is_paired(&path, "abc"));
        assert!(!is_paired(&path, "abd"));
        assert!(remove(&path, "abc").unwrap());
        assert!(!is_paired(&path, "abc"));
        assert!(!remove(&path, "abc").unwrap());
    }

    #[test]
    fn pairing_the_same_certificate_again_renames_rather_than_duplicates() {
        let (_dir, path) = temp();
        add(&path, "Phone", "abc").unwrap();
        add(&path, "Oli's Fold", "abc").unwrap();
        let phones = load(&path);
        assert_eq!(phones.len(), 1);
        assert_eq!(phones[0].name, "Oli's Fold");
    }

    #[test]
    fn names_are_trimmed_cut_and_never_blank() {
        let (_dir, path) = temp();
        add(&path, "   ", "a").unwrap();
        add(&path, &"x".repeat(200), "b").unwrap();
        let phones = load(&path);
        assert_eq!(phones[0].name, "Phone");
        assert_eq!(phones[1].name.len(), 60);
    }

    #[test]
    fn a_device_linked_to_another_accounts_list_is_known_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let mine = dir.path().join("lan_paired-aaaa.json");
        let theirs = dir.path().join("lan_paired-bbbb.json");
        add(&theirs, "Other phone", "fp1").unwrap();
        add(&mine, "My phone", "fp2").unwrap();
        assert!(known_elsewhere(&mine, "fp1"));
        assert!(!known_elsewhere(&mine, "fp2"), "its own list does not count");
        assert!(!known_elsewhere(&mine, "fp3"));
    }

    #[test]
    fn clear_unpairs_everyone() {
        let (_dir, path) = temp();
        add(&path, "A", "a").unwrap();
        add(&path, "B", "b").unwrap();
        clear(&path).unwrap();
        assert!(load(&path).is_empty());
    }
    #[test]
    fn rename_changes_only_the_named_device() {
        let (_dir, path) = temp();
        add(&path, "One", "aa").unwrap();
        add(&path, "Two", "bb").unwrap();
        assert!(rename(&path, "bb", "  Living room  ").unwrap());
        let names: Vec<_> = load(&path).into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["One", "Living room"]);
        assert!(rename(&path, "aa", "   ").unwrap());
        assert_eq!(load(&path)[0].name, "Remote device");
        assert!(!rename(&path, "zz", "Nope").unwrap());
    }
}
