//! Announces LilyPad on the local network (mDNS / DNS-SD) so the FrogLog app can list the PCs it can pair with.
//!
//! The announcement is only a hint for finding the address and port: it carries the device name and a short fingerprint prefix, and
//! nothing in it is trusted. Pairing and every later request prove who is who through the certificates.

use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::HashMap;

pub const SERVICE_TYPE: &str = "_froglog-lilypad._tcp.local.";

pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

/// Letters, digits, spaces and dashes only, as DNS labels want, and never empty.
pub fn clean_label(name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == ' ' || *c == '-').take(40).collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() { "LilyPad".to_string() } else { cleaned }
}

/// The host name part: the label with spaces turned into dashes, as `<label>.local.`.
pub fn host_name(label: &str) -> String {
    format!("{}.local.", label.replace(' ', "-"))
}

/// This PC's IPv4 addresses on its networks (no loopback, no 169.254 self-assigned ones). They go in the announcement because the
/// address mDNS hands a phone is sometimes an IPv6 link-local one (fe80::...), which can't be connected to without knowing the interface.
pub fn local_ipv4s() -> Vec<String> {
    let mut found: Vec<String> = if_addrs::get_if_addrs()
        .map(|interfaces| {
            interfaces
                .into_iter()
                .filter(|i| !i.is_loopback())
                .filter_map(|i| match i.ip() {
                    std::net::IpAddr::V4(v4) if !v4.is_link_local() && !v4.is_unspecified() => Some(v4.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found.dedup();
    found
}

impl Advertiser {
    pub fn start(device_name: &str, port: u16, fingerprint: &str) -> Result<Advertiser, String> {
        let label = clean_label(device_name);
        let mut properties = HashMap::new();
        properties.insert("name".to_string(), device_name.chars().take(60).collect::<String>());
        properties.insert("fp".to_string(), fingerprint.chars().take(16).collect::<String>());
        properties.insert("v".to_string(), "1".to_string());
        properties.insert("ip".to_string(), local_ipv4s().join(","));
        let info = ServiceInfo::new(SERVICE_TYPE, &label, &host_name(&label), "", port, properties).map_err(|e| e.to_string())?.enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
        daemon.register(info).map_err(|e| e.to_string())?;
        Ok(Advertiser { daemon, fullname })
    }

    pub fn stop(self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_dns_safe_and_never_empty() {
        assert_eq!(clean_label("Oli's Gaming PC!"), "Olis Gaming PC");
        assert_eq!(clean_label("  "), "LilyPad");
        assert_eq!(clean_label("日本語"), "LilyPad");
        assert_eq!(clean_label(&"a".repeat(100)).len(), 40);
    }

    #[test]
    fn the_ipv4_addresses_listed_are_real_ones() {
        for address in local_ipv4s() {
            let parsed: std::net::Ipv4Addr = address.parse().expect("each is an IPv4 address");
            assert!(!parsed.is_loopback() && !parsed.is_link_local());
        }
    }

    #[test]
    fn host_names_have_no_spaces() {
        assert_eq!(host_name("Olis Gaming PC"), "Olis-Gaming-PC.local.");
    }
}
