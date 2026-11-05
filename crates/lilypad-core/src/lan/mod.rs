//! Remote launch from a phone on the same network, with nothing relayed through the FrogLog server.
//!
//! LilyPad runs a small HTTPS server on the LAN with its own self-signed certificate, and advertises it over mDNS. The FrogLog
//! Android app links to it once, by pairing (a PIN the phone shows and the user types in LilyPad, proved with HMACs over both
//! certificates' fingerprints, so neither side can be impersonated), and from then on the two pin each other's certificate and
//! talk over mutual TLS. A paired phone can only ask for a game to be launched, by id, and LilyPad decides what that means from
//! its own local data: it never runs a path or a command line the phone sends.
//!
//! The paired list lives only on this PC. Nothing the FrogLog account or website does can add, change or reach a paired phone.

pub mod discovery;
pub mod identity;
pub mod launch;
pub mod pairing;
pub mod server;
pub mod settings;
pub mod store;
