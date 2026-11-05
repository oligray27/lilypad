//! The proof two sides exchange while pairing: that each knows the PIN, and that each saw the same two certificates.
//!
//! The phone makes the PIN and shows it; the user types it into LilyPad. Both then derive a key from the PIN and a salt the phone
//! chose, and send an HMAC over (role, LilyPad's certificate fingerprint, the phone's certificate fingerprint). A man in the
//! middle terminates TLS with a certificate of their own, so the fingerprint either side sees differs from the real one and the
//! tag fails; and without the PIN they can't make a tag at all. The tags are role-separated, so one can't be replayed as the other.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub const PIN_DIGITS: usize = 4;
const KEY_ROUNDS: u32 = 20_000;
const CONTEXT: &[u8] = b"froglog-lilypad-pair-v1";

/// Who a tag is from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Pc,
    Phone,
}

impl Role {
    fn label(self) -> &'static [u8] {
        match self {
            Role::Pc => b"pc",
            Role::Phone => b"phone",
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

/// A certificate's SHA-256 fingerprint, lowercase hex.
pub fn fingerprint(der: &[u8]) -> String {
    hex(&Sha256::digest(der))
}

/// The PIN is exactly four digits.
pub fn pin_is_valid(pin: &str) -> bool {
    pin.len() == PIN_DIGITS && pin.bytes().all(|b| b.is_ascii_digit())
}

/// The key both sides derive from the PIN. The rounds only make a guess a little dearer: what really limits guessing is that a
/// pairing allows a single attempt and the PIN is never reused.
pub fn derive_key(salt: &[u8], pin: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(CONTEXT);
    hasher.update(salt);
    hasher.update(pin.as_bytes());
    let mut key: [u8; 32] = hasher.finalize().into();
    for _ in 0..KEY_ROUNDS {
        let mut next = Sha256::new();
        next.update(key);
        next.update(salt);
        key = next.finalize().into();
    }
    key
}

fn mac(key: &[u8; 32], role: Role, pc_fingerprint: &str, phone_fingerprint: &str) -> HmacSha256 {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(role.label());
    mac.update(b"|");
    mac.update(pc_fingerprint.as_bytes());
    mac.update(b"|");
    mac.update(phone_fingerprint.as_bytes());
    mac
}

/// The tag a side sends, lowercase hex.
pub fn tag(key: &[u8; 32], role: Role, pc_fingerprint: &str, phone_fingerprint: &str) -> String {
    hex(&mac(key, role, pc_fingerprint, phone_fingerprint).finalize().into_bytes())
}

/// Whether a received tag is right (compared in constant time).
pub fn verify(key: &[u8; 32], role: Role, pc_fingerprint: &str, phone_fingerprint: &str, received: &str) -> bool {
    match from_hex(received) {
        Some(bytes) => mac(key, role, pc_fingerprint, phone_fingerprint).verify_slice(&bytes).is_ok(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PC: &str = "aa11";
    const PHONE: &str = "bb22";

    #[test]
    fn both_sides_agree_when_the_pin_and_certificates_match() {
        let key = derive_key(b"salt-salt-salt-1", "4821");
        let t = tag(&key, Role::Pc, PC, PHONE);
        assert!(verify(&derive_key(b"salt-salt-salt-1", "4821"), Role::Pc, PC, PHONE, &t));
    }

    #[test]
    fn a_wrong_pin_fails() {
        let t = tag(&derive_key(b"salt", "4821"), Role::Pc, PC, PHONE);
        assert!(!verify(&derive_key(b"salt", "4822"), Role::Pc, PC, PHONE, &t));
    }

    #[test]
    fn a_different_salt_fails() {
        let t = tag(&derive_key(b"salt-one", "4821"), Role::Pc, PC, PHONE);
        assert!(!verify(&derive_key(b"salt-two", "4821"), Role::Pc, PC, PHONE, &t));
    }

    #[test]
    fn a_tag_is_not_valid_for_other_certificates() {
        let key = derive_key(b"salt", "4821");
        let t = tag(&key, Role::Pc, PC, PHONE);
        assert!(!verify(&key, Role::Pc, "cc33", PHONE, &t), "another LilyPad certificate (a man in the middle)");
        assert!(!verify(&key, Role::Pc, PC, "dd44", &t), "another phone certificate");
    }

    #[test]
    fn a_tag_from_one_role_is_not_valid_as_the_other() {
        let key = derive_key(b"salt", "4821");
        let t = tag(&key, Role::Pc, PC, PHONE);
        assert!(!verify(&key, Role::Phone, PC, PHONE, &t));
    }

    #[test]
    fn rubbish_tags_are_rejected() {
        let key = derive_key(b"salt", "4821");
        assert!(!verify(&key, Role::Pc, PC, PHONE, ""));
        assert!(!verify(&key, Role::Pc, PC, PHONE, "zz"));
        assert!(!verify(&key, Role::Pc, PC, PHONE, "abc"));
    }

    #[test]
    fn pins_are_four_digits() {
        assert!(pin_is_valid("0042"));
        assert!(!pin_is_valid("42"));
        assert!(!pin_is_valid("00420"));
        assert!(!pin_is_valid("00a2"));
        assert!(!pin_is_valid("００４２"));
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(from_hex(&hex(&bytes)).unwrap(), bytes);
        assert!(from_hex("abc").is_none());
    }

    #[test]
    fn fingerprints_are_stable_hex_digests() {
        assert_eq!(fingerprint(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn known_answers_that_the_phone_side_must_match() {
        let salt: Vec<u8> = (0u8..16).collect();
        let key = derive_key(&salt, "4821");
        assert_eq!(hex(&key), "b48b6b90c18ca19a28cbe135a8b75f65c0c4f80a0617bb7ca8d56610e44e321c");
        assert_eq!(tag(&key, Role::Pc, "aa11", "bb22"), "1a33467fc902a2c580849022e7057908f0a33532ea844f19750a0bb19d109f52");
        assert_eq!(tag(&key, Role::Phone, "aa11", "bb22"), "cfde6e88ad402af23283ddd537cd2a1857265c6ffc787c694ed6b5fd3c1d27ba");
    }
}
