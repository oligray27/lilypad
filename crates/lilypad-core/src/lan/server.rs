//! The LAN server: HTTPS with mutual TLS, one request per connection, on a port LilyPad picks.
//!
//! Every client must present a certificate (so TLS asks for one), but the handshake accepts any: who a client is gets decided
//! here, afterwards, by the SHA-256 fingerprint of the certificate it presented. A fingerprint in the paired list may use the
//! authenticated routes; during an open pairing window an unpaired client may use only the two `/pair` routes.
//!
//! Pairing (the PIN is made and shown by the phone, typed into LilyPad):
//! 1. `POST /pair/begin {name, salt}`: held open until the PIN is typed in LilyPad (`submit_pin`), then answered with LilyPad's
//!    tag over both certificates' fingerprints. The phone checks it, which proves the PIN and that nobody sits in the middle.
//! 2. `POST /pair/finish {tag}`: the phone's own tag. If it verifies, the phone's fingerprint is stored.
//! A pairing is a single attempt: a wrong tag closes the window, and the phone starts again with a new PIN.

use super::identity::Identity;
use super::pairing::{self, Role};
use super::{launch, store};
use crate::config::ProcessMapping;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::CryptoProvider;
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const MAX_HEAD: usize = 8 * 1024;
const MAX_BODY: usize = 8 * 1024;
const MAX_CONNECTIONS: usize = 16;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `/pair/begin` waits for the PIN to be typed.
const PIN_WAIT: Duration = Duration::from_secs(120);
/// How many PINs the PC's user may type for one request before it is refused. A wrong one is known when the device finds LilyPad's tag
/// does not match, and it then asks (`/pair/next`) for another go.
const MAX_PIN_ATTEMPTS: u32 = 5;
/// A request whose tag has been sent but which hears nothing more (the device went away) stops blocking others after this long.
const TAG_STALE: Duration = Duration::from_secs(30);
/// The shortest gap between two launches from one phone.
const LAUNCH_GAP: Duration = Duration::from_secs(2);

/// What the server needs from the app around it.
pub trait LanHost: Send + Sync + 'static {
    /// The setting that lets phones link and launch at all. Off: every request is refused.
    fn enabled(&self) -> bool;
    fn device_name(&self) -> String;
    /// "Ask me on this PC before launching".
    fn ask_before_launch(&self) -> bool;
    fn mappings(&self) -> Vec<ProcessMapping>;
    /// A session is being tracked now, so no game is launched on top of it.
    fn tracking(&self) -> bool;
    /// The Steam app ids installed on this PC. A game that is installed has a licence, so starting it through Steam will work.
    fn installed_steam_ids(&self) -> Vec<String> {
        Vec::new()
    }
    /// How to start a mapped game whose executable's path was never recorded, worked out from what this PC has (its Steam library, its
    /// watched folders). None if it can't.
    fn resolve_launch(&self, _mapping: &ProcessMapping) -> Option<launch::Plan> {
        None
    }
    /// The FrogLog username LilyPad is signed in as: the account whose library its mappings (and so every game id) belong to. None when
    /// signed out or not known.
    fn account(&self) -> Option<String>;
    /// Where this account's paired devices are listed (see `store::path_for_account`).
    fn paired_path(&self) -> PathBuf;
    /// Asks this PC's user whether to allow a launch, and waits for the answer. Only called when `ask_before_launch`.
    fn confirm_launch(&self, phone: &str, title: &str) -> bool;
    /// Something worth showing the user (a pairing request, a result, a launch).
    fn event(&self, event: Event);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A phone has asked to pair and LilyPad should ask for its PIN.
    PairingRequested(String),
    Paired(String),
    PairingFailed,
    /// The PIN typed was not right. `remaining` more may be typed; at 0 the request has been refused.
    WrongPin { remaining: u32 },
    /// A device started a game on this PC. `game_id` and `game_type` ("game" or "live_service") say which one in the account's library.
    Launched { phone: String, title: String, method: &'static str, game_id: i32, game_type: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PairingStatus {
    /// The phone that has asked to pair, and is waiting for its PIN.
    pub waiting_phone: Option<String>,
}

/// A name is the name the device gave itself and nothing more: anyone on the network can claim to be anyone, so it is only shown to
/// the PC's user as a hint. The name that is stored is the one the PC's user types when giving the code (`assigned_name`).
struct Pending {
    name: String,
    assigned_name: Option<String>,
    phone_fp: String,
    salt: Vec<u8>,
    /// PINs that did not match so far.
    attempts: u32,
    /// When this request last moved (it began, or a tag was sent).
    touched: Instant,
    pin: Option<String>,
    /// Set once LilyPad's tag has been sent: the key the phone's tag must match.
    key: Option<[u8; 32]>,
}

#[derive(Default)]
struct PairState {
    pending: Option<Pending>,
    /// The waiting request was refused on the PC (not cancelled by the phone), so its answer says so.
    refused: bool,
}

struct Inner {
    host: Arc<dyn LanHost>,
    identity: Identity,
    tls: Arc<rustls::ServerConfig>,
    state: Mutex<PairState>,
    changed: Condvar,
    stop: AtomicBool,
    active: AtomicUsize,
    last_launch: Mutex<HashMap<String, Instant>>,
    /// When each client last asked to pair, to keep a device on the network from flooding LilyPad with requests.
    begin_log: Mutex<HashMap<String, Vec<Instant>>>,
}

pub struct LanServer {
    inner: Arc<Inner>,
    port: u16,
}

#[derive(Debug)]
struct AnyClientCert(Arc<CryptoProvider>);

impl ClientCertVerifier for AnyClientCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(&self, _end: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>], _now: UnixTime) -> Result<ClientCertVerified, rustls::Error> {
        // Whether this certificate is one we know is decided per request, by fingerprint.
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn tls_config(identity: &Identity) -> Result<Arc<rustls::ServerConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone()));
    let config = rustls::ServerConfig::builder_with_provider(provider.clone())
        // TLS 1.2 as well as 1.3: the app supports Android 7 and up, and only Android 10+ has 1.3.
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| e.to_string())?
        .with_client_cert_verifier(Arc::new(AnyClientCert(provider)))
        .with_single_cert(vec![CertificateDer::from(identity.cert_der.clone())], key)
        .map_err(|e| e.to_string())?;
    Ok(Arc::new(config))
}

impl LanServer {
    /// Starts listening on `port` (or any free port if that one is taken). `data_dir` holds the certificate and the paired list.
    pub fn start(host: Arc<dyn LanHost>, data_dir: &std::path::Path, port: u16) -> Result<LanServer, String> {
        Self::start_with(host, data_dir, port, false)
    }

    /// Whether nothing is listening on the port (it is bound for a moment, then let go).
    pub fn port_is_free(port: u16) -> bool {
        TcpListener::bind(("0.0.0.0", port)).is_ok()
    }

    /// `exact`: listen on that port or fail, instead of taking any free one when it is busy.
    pub fn start_with(host: Arc<dyn LanHost>, data_dir: &std::path::Path, port: u16, exact: bool) -> Result<LanServer, String> {
        let identity = super::identity::load_or_create(data_dir)?;
        let tls = tls_config(&identity)?;
        let listener = match TcpListener::bind(("0.0.0.0", port)) {
            Ok(listener) => listener,
            Err(e) if exact => return Err(format!("Port {port} could not be used ({e}). Is something else using it?")),
            Err(_) => TcpListener::bind(("0.0.0.0", 0)).map_err(|e| e.to_string())?,
        };
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let inner = Arc::new(Inner {
            host,
            identity,
            tls,
            state: Mutex::new(PairState::default()),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            last_launch: Mutex::new(HashMap::new()),
            begin_log: Mutex::new(HashMap::new()),
        });
        let accepting = inner.clone();
        std::thread::Builder::new()
            .name("lilypad-lan".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if accepting.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    if accepting.active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                        accepting.active.fetch_sub(1, Ordering::SeqCst);
                        continue;
                    }
                    let inner = accepting.clone();
                    std::thread::spawn(move || {
                        let _ = handle(&inner, stream);
                        inner.active.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(LanServer { inner, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// LilyPad's certificate fingerprint (what phones pin).
    pub fn fingerprint(&self) -> &str {
        &self.inner.identity.fingerprint
    }

    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self.inner.state.lock().unwrap().pending = None;
        self.inner.changed.notify_all();
        // Wakes the accept loop so it sees the flag.
        let _ = TcpStream::connect_timeout(&([127, 0, 0, 1], self.port).into(), Duration::from_millis(500));
    }

    /// The PC's user turned the request down. The phone is told it was refused.
    pub fn refuse_pairing(&self) {
        let mut state = self.inner.state.lock().unwrap();
        if state.pending.is_some() {
            state.refused = true;
            state.pending = None;
            self.inner.changed.notify_all();
        }
    }

    /// Drops the request that is waiting for its PIN (LilyPad's Cancel). The phone is told it was cancelled.
    pub fn cancel_pairing(&self) {
        self.inner.state.lock().unwrap().pending = None;
        self.inner.changed.notify_all();
    }

    pub fn pairing_status(&self) -> PairingStatus {
        let state = self.inner.state.lock().unwrap();
        PairingStatus {
            waiting_phone: state.pending.as_ref().filter(|p| p.pin.is_none() && p.key.is_none()).map(|p| p.name.clone()),
        }
    }

    /// The PIN typed into LilyPad for the device that is waiting, with the friendly name the PC's user gives it (blank gives "Remote
    /// device"). The device's own claim about its name is never what is stored, so it cannot pass itself off as another.
    pub fn submit_pin(&self, pin: &str, friendly_name: &str) -> Result<(), &'static str> {
        if !pairing::pin_is_valid(pin) {
            return Err("invalid_pin");
        }
        let mut state = self.inner.state.lock().unwrap();
        match state.pending.as_mut() {
            Some(pending) if pending.pin.is_none() && pending.key.is_none() => {
                let name: String = friendly_name.chars().filter(|c| !c.is_control()).take(60).collect();
                let name = name.trim();
                pending.assigned_name = Some(if name.is_empty() { "Remote device".to_string() } else { name.to_string() });
                pending.pin = Some(pin.to_string());
                self.inner.changed.notify_all();
                Ok(())
            }
            _ => Err("no_request"),
        }
    }
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

type Stream = rustls::StreamOwned<rustls::ServerConnection, TcpStream>;

fn read_request(stream: &mut Stream) -> Result<Request, ()> {
    let mut data: Vec<u8> = Vec::new();
    let mut buf = [0u8; 2048];
    let head_end = loop {
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if data.len() > MAX_HEAD {
            return Err(());
        }
        let n = stream.read(&mut buf).map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        data.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&data[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_string();
    let path = first.next().unwrap_or("").to_string();
    let mut length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().map_err(|_| ())?;
            }
        }
    }
    if length > MAX_BODY {
        return Err(());
    }
    let mut body = data[head_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut buf).map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(length);
    Ok(Request { method, path, body })
}

fn respond(stream: &mut Stream, status: u16, body: &Value) {
    let text = body.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        409 => "Conflict",
        429 => "Too Many Requests",
        _ => "Error",
    };
    let message = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
    let _ = stream.write_all(message.as_bytes());
    let _ = stream.flush();
    stream.conn.send_close_notify();
    let _ = stream.flush();
}

fn handle(inner: &Arc<Inner>, tcp: TcpStream) -> Result<(), ()> {
    tcp.set_read_timeout(Some(IO_TIMEOUT)).map_err(|_| ())?;
    tcp.set_write_timeout(Some(IO_TIMEOUT)).map_err(|_| ())?;
    let conn = rustls::ServerConnection::new(inner.tls.clone()).map_err(|_| ())?;
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    let request = read_request(&mut stream)?;
    // The handshake finished while the request was read, so the client's certificate is known.
    let Some(phone_fp) = stream.conn.peer_certificates().and_then(|certs| certs.first()).map(|cert| pairing::fingerprint(cert.as_ref())) else {
        respond(&mut stream, 401, &json!({ "error": "no_certificate" }));
        return Ok(());
    };
    if !inner.host.enabled() {
        respond(&mut stream, 403, &json!({ "error": "disabled" }));
        return Ok(());
    }
    let (status, body) = route(inner, &request, &phone_fp);
    // Every request and how it ended (never what was in it), so a launch that does nothing can be traced from the log.
    log::info!("[LilyPad] remote device {} {} -> {}{}", request.method, request.path, status, body["error"].as_str().map(|e| format!(" ({e})")).unwrap_or_default());
    respond(&mut stream, status, &body);
    Ok(())
}

/// Whether the account the app says it is in is the one LilyPad is signed in as (case does not matter). It is the app's own claim, so this
/// stops mix-ups between accounts rather than being a defence: the device is already trusted to ask for launches.
fn same_account(inner: &Arc<Inner>, body: &Value) -> bool {
    match (inner.host.account(), body["account"].as_str()) {
        (Some(mine), Some(theirs)) => mine.eq_ignore_ascii_case(theirs.trim()),
        _ => false,
    }
}

fn bad(code: &str) -> (u16, Value) {
    (400, json!({ "error": code }))
}

fn route(inner: &Arc<Inner>, request: &Request, phone_fp: &str) -> (u16, Value) {
    let json_body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/pair/begin") => pair_begin(inner, &json_body, phone_fp),
        ("POST", "/pair/finish") => pair_finish(inner, &json_body, phone_fp),
        ("POST", "/pair/next") => pair_next(inner, phone_fp),
        ("POST", "/pair/cancel") => pair_cancel(inner, phone_fp),
        _ => {
            let paired_path = inner.host.paired_path();
            let Some(phone) = store::load(&paired_path).into_iter().find(|p| p.fingerprint == phone_fp) else {
                // Linked, but under another account (or LilyPad is signed out): not the same as removed, and the device keeps its link.
                if store::known_elsewhere(&paired_path, phone_fp) {
                    let error = if inner.host.account().is_none() { "lilypad_signed_out" } else { "different_account" };
                    return (409, json!({ "error": error }));
                }
                return (401, json!({ "error": "not_paired" }));
            };
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/info") => (
                    200,
                    json!({
                        "name": inner.host.device_name(),
                        "version": env!("CARGO_PKG_VERSION"),
                        "tracking": inner.host.tracking(),
                        "ask_before_launch": inner.host.ask_before_launch(),
                    }),
                ),
                ("GET", "/launchable") => {
                    // Only real Steam app ids (the digits), not the "local:" ids LilyPad gives games it finds in watched folders.
                    let steam: Vec<String> = inner.host.installed_steam_ids().into_iter().filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).collect();
                    (200, json!({ "games": launchable(&inner.host.mappings()), "steam_installed": steam }))
                }
                ("POST", "/launch") => launch_game(inner, &phone, &json_body),
                ("DELETE", "/pair") => {
                    let _ = store::remove(&paired_path, phone_fp);
                    (200, json!({ "ok": true }))
                }
                _ => (404, json!({ "error": "not_found" })),
            }
        }
    }
}

/// The games this PC can start from a recorded executable (Steam ones the phone already knows from the site).
fn launchable(mappings: &[ProcessMapping]) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    mappings
        .iter()
        .filter(|m| m.exe_path.as_deref().map(|p| std::path::Path::new(p).exists()).unwrap_or(false))
        .filter_map(|m| {
            let kind = if m.r#type == "live" { "live_service" } else { "game" };
            seen.insert((kind, m.froglog_id)).then(|| json!({ "account": "tester", "game_type": kind, "game_id": m.froglog_id }))
        })
        .collect()
}

fn launch_game(inner: &Arc<Inner>, phone: &store::PairedPhone, body: &Value) -> (u16, Value) {
    // A game id is only meaningful in the library it came from, so the account the app is in has to be the one LilyPad is in.
    if !same_account(inner, body) {
        return (409, json!({ "error": "different_account" }));
    }
    let Some(game_type) = body["game_type"].as_str() else { return bad("bad_request") };
    let Some(game_id) = body["game_id"].as_i64().and_then(|id| i32::try_from(id).ok()) else { return bad("bad_request") };
    let title: String = body["title"].as_str().unwrap_or("").chars().filter(|c| !c.is_control()).take(80).collect();
    {
        let mut last = inner.last_launch.lock().unwrap();
        if last.get(&phone.fingerprint).map(|t| t.elapsed() < LAUNCH_GAP).unwrap_or(false) {
            return (429, json!({ "error": "too_fast" }));
        }
        last.insert(phone.fingerprint.clone(), Instant::now());
    }
    if inner.host.tracking() {
        return (409, json!({ "error": "session_running" }));
    }
    let plan = match launch::plan(game_type, game_id, body["steam_launch_id"].as_str(), &inner.host.mappings(), &|p| p.exists(), &|m| inner.host.resolve_launch(m)) {
        Ok(plan) => plan,
        Err(refusal) => return (if refusal == launch::Refusal::BadRequest { 400 } else { 409 }, json!({ "error": refusal.code() })),
    };
    if inner.host.ask_before_launch() && !inner.host.confirm_launch(&phone.name, &title) {
        return (403, json!({ "error": "declined" }));
    }
    let method = match plan {
        launch::Plan::Steam(_) => "steam",
        launch::Plan::Exe(_) => "exe",
    };
    log::info!("[LilyPad] starting game {game_id} for {} by {}", phone.name, match &plan { launch::Plan::Steam(id) => format!("Steam {id}"), launch::Plan::Exe(path) => path.display().to_string() });
    match launch::start(&plan) {
        Ok(()) => {
            inner.host.event(Event::Launched { phone: phone.name.clone(), title, method, game_id, game_type: game_type.to_string() });
            (200, json!({ "ok": true, "method": method }))
        }
        Err(e) => {
            log::warn!("[LilyPad] LAN launch failed: {e}");
            (500, json!({ "error": "launch_failed" }))
        }
    }
}

fn pair_begin(inner: &Arc<Inner>, body: &Value, phone_fp: &str) -> (u16, Value) {
    {
        // At most a handful of requests a minute from one client.
        let mut log = inner.begin_log.lock().unwrap();
        let times = log.entry(phone_fp.to_string()).or_default();
        times.retain(|t| t.elapsed() < Duration::from_secs(60));
        if times.len() >= 5 {
            return (429, json!({ "error": "too_fast" }));
        }
        times.push(Instant::now());
    }
    // Linking is for the account LilyPad is signed in as, since its mappings are that account's library.
    if inner.host.account().is_none() {
        return (409, json!({ "error": "lilypad_signed_out" }));
    }
    if !same_account(inner, body) {
        return (409, json!({ "error": "different_account" }));
    }
    let name: String = body["name"].as_str().unwrap_or("").chars().filter(|c| !c.is_control()).take(60).collect();
    let Some(salt) = body["salt"].as_str().and_then(pairing::from_hex).filter(|s| (8..=64).contains(&s.len())) else { return bad("bad_request") };
    {
        let mut state = inner.state.lock().unwrap();
        // One request at a time, but a client may start again (its earlier wait then ends as cancelled).
        let blocked = state.pending.as_ref().map(|p| p.phone_fp != phone_fp && !(p.key.is_some() && p.touched.elapsed() > TAG_STALE)).unwrap_or(false);
        if blocked {
            return (409, json!({ "error": "already_in_progress" }));
        }
        inner.changed.notify_all();
        state.pending = Some(Pending {
            name: name.clone(),
            assigned_name: None,
            phone_fp: phone_fp.to_string(),
            salt,
            attempts: 0,
            touched: Instant::now(),
            pin: None,
            key: None,
        });
    }
    inner.host.event(Event::PairingRequested(name));
    await_tag(inner, phone_fp)
}

/// Held open until the PIN is typed into LilyPad (then answered with LilyPad's tag), the request is cancelled or refused, or it takes
/// too long.
fn await_tag(inner: &Arc<Inner>, phone_fp: &str) -> (u16, Value) {
    let deadline = Instant::now() + PIN_WAIT;
    let mut state = inner.state.lock().unwrap();
    loop {
        if inner.stop.load(Ordering::SeqCst) {
            return (403, json!({ "error": "pairing_closed" }));
        }
        let (pin, salt) = match state.pending.as_ref() {
            Some(p) if p.phone_fp == phone_fp => (p.pin.clone(), p.salt.clone()),
            // Refused or cancelled on the PC, or cancelled by the phone itself.
            _ => {
                let code = if state.refused { "refused" } else { "cancelled" };
                state.refused = false;
                return (403, json!({ "error": code }));
            }
        };
        if let Some(pin) = pin {
            let key = pairing::derive_key(&salt, &pin);
            let tag = pairing::tag(&key, Role::Pc, &inner.identity.fingerprint, phone_fp);
            if let Some(p) = state.pending.as_mut() {
                p.key = Some(key);
                p.touched = Instant::now();
            }
            return (200, json!({ "tag": tag }));
        }
        if Instant::now() >= deadline {
            state.pending = None;
            return (408, json!({ "error": "no_pin" }));
        }
        let wait = deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(1));
        state = inner.changed.wait_timeout(state, wait).unwrap().0;
    }
}

/// The device found LilyPad's tag did not match, which means the PIN typed was wrong, and asks for another go. Up to
/// `MAX_PIN_ATTEMPTS` PINs are allowed in all; after that the request is refused. Otherwise this is held open like `/pair/begin`, until
/// the next PIN is typed.
fn pair_next(inner: &Arc<Inner>, phone_fp: &str) -> (u16, Value) {
    let remaining;
    {
        let mut state = inner.state.lock().unwrap();
        let Some(pending) = state.pending.as_mut().filter(|p| p.phone_fp == phone_fp && p.key.is_some()) else {
            return (403, json!({ "error": "pairing_closed" }));
        };
        pending.attempts += 1;
        remaining = MAX_PIN_ATTEMPTS.saturating_sub(pending.attempts);
        if remaining == 0 {
            state.pending = None;
        } else {
            pending.pin = None;
            pending.key = None;
            pending.touched = Instant::now();
        }
        inner.changed.notify_all();
    }
    inner.host.event(Event::WrongPin { remaining });
    if remaining == 0 {
        return (403, json!({ "error": "too_many_attempts" }));
    }
    await_tag(inner, phone_fp)
}

/// The phone gives up while waiting for its PIN to be typed. Its pending request is dropped (waking its held `/pair/begin`), and the
/// window stays open so it can try again.
fn pair_cancel(inner: &Arc<Inner>, phone_fp: &str) -> (u16, Value) {
    let mut state = inner.state.lock().unwrap();
    if state.pending.as_ref().map(|p| p.phone_fp == phone_fp).unwrap_or(false) {
        state.pending = None;
        inner.changed.notify_all();
    }
    (200, json!({ "ok": true }))
}

fn pair_finish(inner: &Arc<Inner>, body: &Value, phone_fp: &str) -> (u16, Value) {
    let received = body["tag"].as_str().unwrap_or("");
    let mut state = inner.state.lock().unwrap();
    let ok_so_far = state.pending.as_ref().map(|p| p.phone_fp == phone_fp && p.key.is_some()).unwrap_or(false);
    if !ok_so_far {
        return (403, json!({ "error": "pairing_closed" }));
    }
    let pending = state.pending.take().unwrap();
    // One attempt only, right or wrong: the request is already taken out above.
    drop(state);
    let key = pending.key.unwrap();
    if !pairing::verify(&key, Role::Phone, &inner.identity.fingerprint, phone_fp, received) {
        inner.host.event(Event::PairingFailed);
        return (403, json!({ "error": "wrong_pin" }));
    }
    let name = pending.assigned_name.clone().unwrap_or_else(|| "Remote device".to_string());
    match store::add(&inner.host.paired_path(), &name, phone_fp) {
        Ok(()) => {
            inner.host.event(Event::Paired(name));
            (200, json!({ "ok": true, "name": inner.host.device_name() }))
        }
        Err(e) => {
            log::warn!("[LilyPad] could not save the paired phone: {e}");
            (500, json!({ "error": "save_failed" }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};

    struct TestHost {
        tracking: AtomicBool,
        ask: AtomicBool,
        answer: AtomicBool,
        mappings: Mutex<Vec<ProcessMapping>>,
        events: Mutex<Vec<Event>>,
        installed: Mutex<Vec<String>>,
        account: Mutex<Option<String>>,
        dir: tempfile::TempDir,
    }
    impl TestHost {
        fn new() -> Arc<TestHost> {
            Arc::new(TestHost {
                tracking: AtomicBool::new(false),
                ask: AtomicBool::new(false),
                answer: AtomicBool::new(true),
                mappings: Mutex::new(vec![]),
                events: Mutex::new(vec![]),
                installed: Mutex::new(vec![]),
                account: Mutex::new(Some("tester".to_string())),
                dir: tempfile::tempdir().unwrap(),
            })
        }
    }
    impl LanHost for TestHost {
        fn enabled(&self) -> bool {
            true
        }
        fn device_name(&self) -> String {
            "Test PC".into()
        }
        fn ask_before_launch(&self) -> bool {
            self.ask.load(Ordering::SeqCst)
        }
        fn mappings(&self) -> Vec<ProcessMapping> {
            self.mappings.lock().unwrap().clone()
        }
        fn tracking(&self) -> bool {
            self.tracking.load(Ordering::SeqCst)
        }
        fn installed_steam_ids(&self) -> Vec<String> {
            self.installed.lock().unwrap().clone()
        }
        fn account(&self) -> Option<String> {
            self.account.lock().unwrap().clone()
        }
        fn paired_path(&self) -> PathBuf {
            let key = self.account().unwrap_or_else(|| "signed-out".to_string());
            self.dir.path().join(format!("lan_paired-{key}.json"))
        }
        fn confirm_launch(&self, _phone: &str, _title: &str) -> bool {
            self.answer.load(Ordering::SeqCst)
        }
        fn event(&self, event: Event) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[derive(Debug)]
    struct AnyServer(Arc<CryptoProvider>);
    impl ServerCertVerifier for AnyServer {
        fn verify_server_cert(&self, _e: &CertificateDer<'_>, _i: &[CertificateDer<'_>], _n: &rustls::pki_types::ServerName<'_>, _o: &[u8], _t: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
        }
        fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    /// A stand-in for the phone: its own certificate, and requests that return the status, the body and LilyPad's fingerprint.
    struct Phone {
        config: Arc<rustls::ClientConfig>,
        fingerprint: String,
        port: u16,
    }
    impl Phone {
        fn new(port: u16) -> Phone {
            let key = rcgen::KeyPair::generate().unwrap();
            let cert = rcgen::CertificateParams::new(vec!["phone".into()]).unwrap().self_signed(&key).unwrap();
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AnyServer(provider)))
                .with_client_auth_cert(vec![cert.der().clone()], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())))
                .unwrap();
            Phone { config: Arc::new(config), fingerprint: pairing::fingerprint(cert.der()), port }
        }
        fn request(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value, String) {
            let tcp = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            let conn = rustls::ClientConnection::new(self.config.clone(), "lilypad.local".try_into().unwrap()).unwrap();
            let mut stream = rustls::StreamOwned::new(conn, tcp);
            let payload = body.map(|b| b.to_string()).unwrap_or_default();
            let message = format!("{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{payload}", payload.len());
            stream.write_all(message.as_bytes()).unwrap();
            let mut response = Vec::new();
            let _ = stream.read_to_end(&mut response);
            let server_fp = pairing::fingerprint(stream.conn.peer_certificates().unwrap()[0].as_ref());
            let text = String::from_utf8_lossy(&response).to_string();
            let status: u16 = text.split(' ').nth(1).unwrap().parse().unwrap();
            let json = text.split("\r\n\r\n").nth(1).and_then(|b| serde_json::from_str(b).ok()).unwrap_or(Value::Null);
            (status, json, server_fp)
        }
    }

    fn server(host: &Arc<TestHost>) -> (tempfile::TempDir, LanServer) {
        let dir = tempfile::tempdir().unwrap();
        let server = LanServer::start(host.clone(), dir.path(), 0).unwrap();
        (dir, server)
    }

    /// Runs a full pairing with the given PIN typed at LilyPad and the phone's own PIN, returning the final status.
    fn pair(server: &LanServer, phone: &Phone, phone_pin: &str, typed_pin: &str) -> u16 {
        let salt = [7u8; 16];
        let begin = std::thread::scope(|scope| {
            let handle = scope.spawn(|| phone.request("POST", "/pair/begin", Some(json!({ "account": "tester", "name": "Test Phone", "salt": pairing::hex(&salt) }))));
            for _ in 0..200 {
                if server.pairing_status().waiting_phone.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(server.pairing_status().waiting_phone.as_deref(), Some("Test Phone"));
            server.submit_pin(typed_pin, "Assigned Name").unwrap();
            handle.join().unwrap()
        });
        assert_eq!(begin.0, 200);
        let key = pairing::derive_key(&salt, phone_pin);
        let tag = begin.1["tag"].as_str().unwrap();
        if !pairing::verify(&key, Role::Pc, &begin.2, &phone.fingerprint, tag) {
            // The phone would stop here and say the PIN was wrong, so it sends nothing.
            return 0;
        }
        let finish = phone.request("POST", "/pair/finish", Some(json!({ "tag": pairing::tag(&key, Role::Phone, &begin.2, &phone.fingerprint) })));
        finish.0
    }

    #[test]
    fn an_unpaired_phone_is_refused_and_a_paired_one_is_let_in() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        assert_eq!(phone.request("GET", "/info", None).0, 401);
        assert_eq!(pair(&server, &phone, "4821", "4821"), 200);
        let (status, info, fp) = phone.request("GET", "/info", None);
        assert_eq!(status, 200);
        assert_eq!(info["name"], "Test PC");
        assert_eq!(fp, server.fingerprint());
        // The name stored is the one the PC's user gave, not the one the device claimed ("Test Phone").
        assert!(host.events.lock().unwrap().contains(&Event::Paired("Assigned Name".into())));
        assert_eq!(store::load(&host.paired_path())[0].name, "Assigned Name");
        // Another phone is still refused.
        assert_eq!(Phone::new(server.port()).request("GET", "/info", None).0, 401);
    }

    /// A device that has just been told LilyPad's tag and found it does not match asks for another go.
    fn next_go(phone: &Phone, server: &LanServer, typed: &str) -> (u16, Value) {
        std::thread::scope(|scope| {
            let next = scope.spawn(|| phone.request("POST", "/pair/next", None));
            while server.pairing_status().waiting_phone.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            server.submit_pin(typed, "Phone").unwrap();
            let (status, body, _) = next.join().unwrap();
            (status, body)
        })
    }

    fn start_waiting(phone: &Phone, server: &LanServer, salt: [u8; 16], typed: &str) -> (u16, Value, String) {
        std::thread::scope(|scope| {
            let begin = scope.spawn(|| phone.request("POST", "/pair/begin", Some(json!({ "account": "tester", "name": "P", "salt": pairing::hex(&salt) }))));
            while server.pairing_status().waiting_phone.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            server.submit_pin(typed, "Phone").unwrap();
            begin.join().unwrap()
        })
    }

    #[test]
    fn a_wrong_pin_can_be_retyped_and_then_pairs() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        let salt = [5u8; 16];
        let key = pairing::derive_key(&salt, "4821");
        let (status, body, pc_fp) = start_waiting(&phone, &server, salt, "0000");
        assert_eq!(status, 200);
        assert!(!pairing::verify(&key, Role::Pc, &pc_fp, &phone.fingerprint, body["tag"].as_str().unwrap()), "the wrong PIN shows as a tag that does not match");
        // The device asks again and the right PIN is typed this time.
        let (status, body) = next_go(&phone, &server, "4821");
        assert_eq!(status, 200);
        assert!(pairing::verify(&key, Role::Pc, &pc_fp, &phone.fingerprint, body["tag"].as_str().unwrap()));
        let finish = phone.request("POST", "/pair/finish", Some(json!({ "tag": pairing::tag(&key, Role::Phone, &pc_fp, &phone.fingerprint) })));
        assert_eq!(finish.0, 200);
        assert_eq!(phone.request("GET", "/info", None).0, 200);
        assert!(host.events.lock().unwrap().contains(&Event::WrongPin { remaining: 4 }));
    }

    #[test]
    fn five_wrong_pins_are_allowed_and_then_the_request_is_refused() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        // The first PIN typed, then four more goes: five in all, each wrong.
        assert_eq!(start_waiting(&phone, &server, [6u8; 16], "0000").0, 200);
        for _ in 0..4 {
            assert_eq!(next_go(&phone, &server, "1111").0, 200);
        }
        // The fifth wrong one is the last: asking again is refused for good.
        let (status, body, _) = phone.request("POST", "/pair/next", None);
        assert_eq!((status, body["error"].as_str()), (403, Some("too_many_attempts")));
        assert!(server.pairing_status().waiting_phone.is_none());
        assert_eq!(phone.request("GET", "/info", None).0, 401);
        assert!(host.events.lock().unwrap().contains(&Event::WrongPin { remaining: 0 }));
        // A fresh request is accepted again.
        std::thread::scope(|scope| {
            let again = scope.spawn(|| phone.request("POST", "/pair/begin", Some(json!({ "account": "tester", "name": "P", "salt": pairing::hex(&[7u8; 16]) }))));
            while server.pairing_status().waiting_phone.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            server.cancel_pairing();
            again.join().unwrap();
        });
    }

    #[test]
    fn asking_for_another_go_without_a_tag_having_been_sent_is_refused() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        let (status, body, _) = phone.request("POST", "/pair/next", None);
        assert_eq!((status, body["error"].as_str()), (403, Some("pairing_closed")));
    }

    #[test]
    fn a_wrong_pin_pairs_nothing() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        // The PIN typed at LilyPad differs from the phone's: the phone notices from LilyPad's tag and stops.
        assert_eq!(pair(&server, &phone, "4821", "4822"), 0);
        assert_eq!(phone.request("GET", "/info", None).0, 401);
    }

    #[test]
    fn a_forged_finish_tag_is_refused_and_closes_the_window() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        let salt = [9u8; 16];
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| phone.request("POST", "/pair/begin", Some(json!({ "account": "tester", "name": "P", "salt": pairing::hex(&salt) }))));
            while server.pairing_status().waiting_phone.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            server.submit_pin("1234", "").unwrap();
            handle.join().unwrap();
        });
        let (status, _, _) = phone.request("POST", "/pair/finish", Some(json!({ "tag": "00" })));
        assert_eq!(status, 403);
        assert_eq!(phone.request("GET", "/info", None).0, 401);
        assert!(server.pairing_status().waiting_phone.is_none(), "one attempt only");
        assert!(host.events.lock().unwrap().contains(&Event::PairingFailed));
    }

    #[test]
    fn a_phone_can_cancel_while_waiting_and_try_again() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        let begin = |salt: u8| phone.request("POST", "/pair/begin", Some(json!({ "account": "tester", "name": "P", "salt": pairing::hex(&[salt; 16]) })));
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| begin(1));
            while server.pairing_status().waiting_phone.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(phone.request("POST", "/pair/cancel", None).0, 200);
            let (status, body, _) = waiting.join().unwrap();
            assert_eq!((status, body["error"].as_str()), (403, Some("cancelled")));
        });
        assert!(server.pairing_status().waiting_phone.is_none());
        // Another begin is accepted straight away.
        std::thread::scope(|scope| {
            let again = scope.spawn(|| begin(2));
            while server.pairing_status().waiting_phone.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            server.cancel_pairing();
            let (status, body, _) = again.join().unwrap();
            assert_eq!((status, body["error"].as_str()), (403, Some("cancelled")), "cancelling on the PC ends the wait the same way");
        });
    }

    #[test]
    fn a_request_refused_on_the_pc_says_so() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| phone.request("POST", "/pair/begin", Some(json!({ "account": "tester", "name": "P", "salt": pairing::hex(&[3u8; 16]) }))));
            while server.pairing_status().waiting_phone.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            server.refuse_pairing();
            let (status, body, _) = waiting.join().unwrap();
            assert_eq!((status, body["error"].as_str()), (403, Some("refused")));
        });
        // Nothing lingers: the next request is a normal one.
        std::thread::scope(|scope| {
            let again = scope.spawn(|| phone.request("POST", "/pair/begin", Some(json!({ "account": "tester", "name": "P", "salt": pairing::hex(&[4u8; 16]) }))));
            while server.pairing_status().waiting_phone.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            server.cancel_pairing();
            assert_eq!(again.join().unwrap().1["error"].as_str(), Some("cancelled"));
        });
    }

    #[test]
    fn a_chosen_port_must_be_free_but_the_default_may_fall_back() {
        let host = TestHost::new();
        let (dir, first) = server(&host);
        let taken = first.port();
        assert!(LanServer::start_with(host.clone(), dir.path(), taken, true).is_err(), "a port chosen by the user is used or refused");
        let fallback = LanServer::start_with(host.clone(), dir.path(), taken, false).unwrap();
        assert_ne!(fallback.port(), taken, "the default port may move aside");
        fallback.stop();
    }

    #[test]
    fn a_busy_port_is_reported_as_not_free() {
        let host = TestHost::new();
        let (_dir, running) = server(&host);
        assert!(!LanServer::port_is_free(running.port()));
        running.stop();
    }

    #[test]
    fn a_device_linked_under_one_account_is_kept_but_not_served_under_another() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        assert_eq!(pair(&server, &phone, "1212", "1212"), 200);
        assert_eq!(phone.request("GET", "/info", None).0, 200);
        // LilyPad signs in as somebody else: the link is not removed, and it says why it can't be used.
        *host.account.lock().unwrap() = Some("other".to_string());
        let (status, body, _) = phone.request("GET", "/info", None);
        assert_eq!((status, body["error"].as_str()), (409, Some("different_account")));
        // Signed out altogether.
        *host.account.lock().unwrap() = None;
        let (status, body, _) = phone.request("GET", "/info", None);
        assert_eq!((status, body["error"].as_str()), (409, Some("lilypad_signed_out")));
        // Back as the original account, it works again, with nothing re-linked.
        *host.account.lock().unwrap() = Some("tester".to_string());
        assert_eq!(phone.request("GET", "/info", None).0, 200);
        // A device nobody linked is still just not paired.
        *host.account.lock().unwrap() = Some("other".to_string());
        assert_eq!(Phone::new(server.port()).request("GET", "/info", None).0, 401);
    }

    #[test]
    fn linking_and_launching_need_the_account_to_match() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        // A device in another FrogLog account can't link: its game ids would mean other games here.
        let begin = phone.request("POST", "/pair/begin", Some(json!({ "account": "somebody-else", "name": "P", "salt": pairing::hex(&[1u8; 16]) })));
        assert_eq!((begin.0, begin.1["error"].as_str()), (409, Some("different_account")));
        // Case does not matter.
        assert_eq!(pair(&server, &phone, "3434", "3434"), 200);
        let (status, body, _) = phone.request("POST", "/launch", Some(json!({ "account": "SOMEBODY", "game_type": "game", "game_id": 5 })));
        assert_eq!((status, body["error"].as_str()), (409, Some("different_account")));
        std::thread::sleep(LAUNCH_GAP);
        let (status, body, _) = phone.request("POST", "/launch", Some(json!({ "account": "TESTER", "game_type": "game", "game_id": 5 })));
        assert_eq!((status, body["error"].as_str()), (409, Some("no_launch_method")), "the account matched, so it got as far as looking for the game");
        // LilyPad signed out can't link anyone.
        *host.account.lock().unwrap() = None;
        let signed_out = phone.request("POST", "/pair/begin", Some(json!({ "account": "tester", "name": "P", "salt": pairing::hex(&[2u8; 16]) })));
        assert_eq!((signed_out.0, signed_out.1["error"].as_str()), (409, Some("lilypad_signed_out")));
    }

    #[test]
    fn launchable_lists_the_steam_games_installed_here() {
        let host = TestHost::new();
        *host.installed.lock().unwrap() = vec!["504230".into(), "local:D:\\7S Games\\Truxton Extreme".into(), "".into(), "1085660".into()];
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        assert_eq!(pair(&server, &phone, "6767", "6767"), 200);
        let (status, body, _) = phone.request("GET", "/launchable", None);
        assert_eq!(status, 200);
        assert_eq!(body["steam_installed"], json!(["504230", "1085660"]), "only real Steam app ids are reported");
    }

    #[test]
    fn a_client_cannot_flood_pairing_requests() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        // Bad requests still count: the sixth in a minute is refused outright.
        let statuses: Vec<u16> = (0..6).map(|_| phone.request("POST", "/pair/begin", Some(json!({ "account": "tester", "name": "P", "salt": "zz" }))).0).collect();
        assert_eq!(statuses, vec![400, 400, 400, 400, 400, 429]);
    }

    #[test]
    fn only_one_other_device_may_be_waiting_at_a_time() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let first = Phone::new(server.port());
        let second = Phone::new(server.port());
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| first.request("POST", "/pair/begin", Some(json!({ "account": "tester", "name": "A", "salt": pairing::hex(&[1u8; 16]) }))));
            while server.pairing_status().waiting_phone.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            let (status, body, _) = second.request("POST", "/pair/begin", Some(json!({ "account": "tester", "name": "B", "salt": pairing::hex(&[2u8; 16]) })));
            assert_eq!((status, body["error"].as_str()), (409, Some("already_in_progress")));
            server.cancel_pairing();
            waiting.join().unwrap();
        });
    }

    #[test]
    fn a_phone_removed_from_the_list_loses_access_at_once() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        assert_eq!(pair(&server, &phone, "1111", "1111"), 200);
        assert_eq!(phone.request("GET", "/info", None).0, 200);
        store::remove(&host.paired_path(), &phone.fingerprint).unwrap();
        assert_eq!(phone.request("GET", "/info", None).0, 401);
    }

    #[test]
    fn launching_is_checked_before_anything_runs() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        assert_eq!(pair(&server, &phone, "2222", "2222"), 200);
        let launch = |body: Value| phone.request("POST", "/launch", Some(body));
        // No Steam id and no mapping: nothing to launch.
        let (status, body, _) = launch(json!({ "account": "tester", "game_type": "game", "game_id": 5 }));
        assert_eq!((status, body["error"].as_str()), (409, Some("no_launch_method")));
        std::thread::sleep(LAUNCH_GAP);
        // A session is running: refused before anything is resolved.
        host.tracking.store(true, Ordering::SeqCst);
        let (status, body, _) = launch(json!({ "account": "tester", "game_type": "game", "game_id": 5, "steam_launch_id": "123" }));
        assert_eq!((status, body["error"].as_str()), (409, Some("session_running")));
        std::thread::sleep(LAUNCH_GAP);
        host.tracking.store(false, Ordering::SeqCst);
        // The user said no on this PC.
        host.ask.store(true, Ordering::SeqCst);
        host.answer.store(false, Ordering::SeqCst);
        let (status, body, _) = launch(json!({ "account": "tester", "game_type": "game", "game_id": 5, "steam_launch_id": "123" }));
        assert_eq!((status, body["error"].as_str()), (403, Some("declined")));
        std::thread::sleep(LAUNCH_GAP);
        // A bad request.
        let (status, _, _) = launch(json!({ "account": "tester", "game_type": "nonsense", "game_id": 5 }));
        assert_eq!(status, 400);
    }

    #[test]
    fn launches_from_one_phone_are_rate_limited() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        assert_eq!(pair(&server, &phone, "3333", "3333"), 200);
        let body = json!({ "account": "tester", "game_type": "game", "game_id": 5 });
        assert_eq!(phone.request("POST", "/launch", Some(body.clone())).0, 409);
        assert_eq!(phone.request("POST", "/launch", Some(body)).0, 429);
    }

    #[test]
    fn a_phone_can_unpair_itself() {
        let host = TestHost::new();
        let (_dir, server) = server(&host);
        let phone = Phone::new(server.port());
        assert_eq!(pair(&server, &phone, "5555", "5555"), 200);
        assert_eq!(phone.request("DELETE", "/pair", None).0, 200);
        assert_eq!(phone.request("GET", "/info", None).0, 401);
    }
}
