//! LilyPad's own certificate for the LAN server: a self-signed one made on first use and kept in the app data folder. Phones don't
//! trust it through any authority; they pin its fingerprint while pairing, and a certificate that differs is refused.

use std::path::Path;

pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub fingerprint: String,
}

fn make() -> Result<(Vec<u8>, Vec<u8>), String> {
    let key = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
    let mut params = rcgen::CertificateParams::new(vec!["lilypad.local".to_string()]).map_err(|e| e.to_string())?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "LilyPad");
    let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
    Ok((cert.der().to_vec(), key.serialize_der()))
}

/// The saved certificate and key in `dir`, or new ones, saved. An unreadable or half-written pair is replaced (phones would then
/// need to pair again, which is the right outcome for a certificate that has changed).
pub fn load_or_create(dir: &Path) -> Result<Identity, String> {
    let cert_path = dir.join("lan_cert.der");
    let key_path = dir.join("lan_key.der");
    if let (Ok(cert_der), Ok(key_der)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        if !cert_der.is_empty() && !key_der.is_empty() {
            let fingerprint = super::pairing::fingerprint(&cert_der);
            return Ok(Identity { cert_der, key_der, fingerprint });
        }
    }
    let (cert_der, key_der) = make()?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(&cert_path, &cert_der).map_err(|e| e.to_string())?;
    std::fs::write(&key_path, &key_der).map_err(|e| e.to_string())?;
    let fingerprint = super::pairing::fingerprint(&cert_der);
    Ok(Identity { cert_der, key_der, fingerprint })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_certificate_is_made_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create(dir.path()).unwrap();
        let second = load_or_create(dir.path()).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.cert_der, second.cert_der);
        assert_eq!(first.fingerprint.len(), 64);
    }

    #[test]
    fn a_broken_pair_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create(dir.path()).unwrap();
        std::fs::write(dir.path().join("lan_cert.der"), b"").unwrap();
        let second = load_or_create(dir.path()).unwrap();
        assert_ne!(first.fingerprint, second.fingerprint);
    }
}
