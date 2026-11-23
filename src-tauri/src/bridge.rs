//! The link with Reimagined for Rhythia in the player's browser: a WebSocket server on this
//! computer only (127.0.0.1, the first free port of `PORTS`).
//!
//! - Only browser extensions get in: the browser sets the `Origin` header and a web page cannot
//!   fake it, so anything but `chrome-extension://` and `moz-extension://` is refused.
//! - An extension is paired once: it shows a short code, Atlas asks the player (with the same
//!   code) and gives the extension a random token. Later connections send the token and are let
//!   in without asking. Pairings are kept in `bridge.json` and can be removed in Settings.
//! - After pairing the extension can ask Atlas to install a map (the player clicked "Install in
//!   Atlas" on rhythia.com; the daily limit applies as always) or to open a collection, which of
//!   some maps are in the game, the Atlas collections and to add a map to one, and the player's
//!   best pass of every map in the game (`scores`, for the player's own profile in Reimagined).
//!   Requests other than install carry a number (`rid`) that the answer repeats.
//!
//! Nothing about the player's Rhythia account goes either way: no token, login or session. The
//! scores go only to the paired extension on this computer.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use crate::{atlas_link::AtlasLink, install, now_secs, Atlas};

/// Reimagined tries these in the same order.
pub const PORTS: [u16; 3] = [47231, 47232, 47233];
const MAX_MESSAGE: usize = 4096;
const MAX_CONNECTIONS: usize = 6;
const HELLO_WITHIN: Duration = Duration::from_secs(5);
const ANSWER_WITHIN: Duration = Duration::from_secs(120);
/// After a refused pairing, new pairing requests are refused for this long (no prompt spam).
const QUIET_AFTER_NO_SECS: u64 = 30;
const MAX_PAIRED: usize = 10;
/// A connected extension may ask for at most one install a second.
const INSTALL_GAP: Duration = Duration::from_secs(1);
/// Maps in one "Install missing" (one page of a map list).
const MAX_INSTALL_LIST: usize = 100;
/// Reading every best pass is heavier: at most once in this long per connection.
const SCORES_GAP: Duration = Duration::from_secs(5);
/// Map ids in one `have` question.
const MAX_HAVE: usize = 300;
/// Near your rank: at most this many maps (a top is 100 plays).
const MAX_NEAR: usize = 200;
/// "Refresh connection": a connected browser that does not answer a ping in this long is dropped.
const PONG_WITHIN: Duration = Duration::from_secs(4);

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Paired {
    id: u64,
    browser: String,
    origin: String,
    token: String,
    created: u64,
    /// When it last connected or was last connected (updated when it disconnects).
    last_seen: u64,
    /// Reimagined's version, from its last hello (empty in pairings from before 0.13.5).
    #[serde(default)]
    version: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PairedInfo {
    id: u64,
    browser: String,
    created: u64,
    last_seen: u64,
    version: String,
    connected: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// The port Atlas listens on; none when every port was taken.
    port: Option<u16>,
    paired: Vec<PairedInfo>,
    /// A pairing request waiting for the player.
    asking: Option<PairRequest>,
    /// What happened with connections lately, newest first (Settings → Account).
    log: Vec<LogLine>,
}

impl Status {
    pub fn has_pairing(&self) -> bool {
        !self.paired.is_empty()
    }
}

#[derive(Serialize, Clone)]
pub struct LogLine {
    at: u64,
    text: String,
}

/// Lines kept in the connection log (in memory only).
const LOG_LINES: usize = 30;

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PairRequest {
    code: String,
    browser: String,
}

struct Pending {
    request: PairRequest,
    reply: oneshot::Sender<bool>,
}

/// A paired extension connected now: its pairing id, and how to close it when unpaired.
struct Live {
    paired: u64,
    close: mpsc::UnboundedSender<()>,
    /// Messages Atlas starts (asking for the Rhythia sign-in).
    out: mpsc::UnboundedSender<serde_json::Value>,
    /// "Refresh connection": a WebSocket ping (the browser answers it by itself, no message of the
    /// protocol); the sender hears back when the pong comes and is dropped when the connection ends.
    ping: mpsc::UnboundedSender<oneshot::Sender<()>>,
}

pub struct Bridge {
    file: PathBuf,
    paired: Mutex<Vec<Paired>>,
    live: Mutex<Vec<(u64, Live)>>,
    pending: Mutex<Option<Pending>>,
    quiet_until: AtomicU64,
    port: Mutex<Option<u16>>,
    /// The listener runs (or is starting); `check` starts it again only when it is not.
    listening: AtomicBool,
    connections: AtomicUsize,
    next_conn: AtomicU64,
    /// Near your rank from Reimagined, by map hash: in memory only, gone when Atlas closes or the
    /// pairing is removed.
    near: Mutex<std::collections::HashMap<String, Near>>,
    log: Mutex<std::collections::VecDeque<LogLine>>,
}

/// One map of the player's top as Reimagined's history service compares it with players ranked near
/// them: their place among those players (1 is best), how many there are, the group's level against
/// the player ("below", "at", "above") and the middle player's RP and accuracy. No names, no keys.
#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Near {
    pub hash: String,
    pub place: u32,
    pub of: u32,
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub rp_mid: Option<f64>,
    #[serde(default)]
    pub acc_mid: Option<f64>,
}

impl Near {
    fn checked(mut self) -> Option<Near> {
        self.hash = self.hash.to_ascii_lowercase();
        let ok = crate::library::is_hash(&self.hash) && (2..=100_000).contains(&self.of) && (1..=self.of).contains(&self.place);
        self.level = self.level.filter(|l| matches!(l.as_str(), "below" | "at" | "above"));
        self.rp_mid = self.rp_mid.filter(|n| n.is_finite() && (0.0..=100_000.0).contains(n));
        self.acc_mid = self.acc_mid.filter(|n| n.is_finite() && (0.0..=100.0).contains(n));
        ok.then_some(self)
    }
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase", deny_unknown_fields)]
enum FromExtension {
    Hello {
        v: u32,
        browser: String,
        version: String,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        code: Option<String>,
    },
    Ping {},
    /// Disconnect in the Reimagined popup: Atlas drops this pairing too.
    Forget {},
    /// The answer to `session-ask` when no rhythia.com tab with a sign-in is open.
    #[serde(rename = "session-missing")]
    SessionMissing {},
    Install {
        kind: String,
        id: i64,
    },
    /// "Install missing" on a map list in Exodus: Atlas only asks in its own window, the player says yes there.
    #[serde(rename = "install-list")]
    InstallList {
        rid: u32,
        ids: Vec<i64>,
    },
    /// The best pass of every map this player passed in the game (`name`: their Rhythia name,
    /// as the game stores it with each score).
    Scores {
        rid: u32,
        name: String,
    },
    /// Which of these maps are in the game.
    Have {
        rid: u32,
        ids: Vec<i64>,
    },
    /// Near your rank for the maps of the player's top (their own profile in Exodus).
    Near {
        rid: u32,
        maps: Vec<Near>,
    },
    /// The Atlas collections (id, name, number of maps).
    Collections {
        rid: u32,
    },
    /// The player's Rhythia sign-in from the browser, sent only after they agreed there. Atlas takes it
    /// only while they asked for it in Atlas (account.rs: two minutes, once), checks it with Rhythia and
    /// keeps it in the Windows password store; it never goes anywhere but Rhythia's API.
    Session {
        rid: u32,
        session: String,
    },
    /// Adds a map to an Atlas collection: by its id, or by a name (made when there is none).
    Collect {
        rid: u32,
        #[serde(default)]
        collection: Option<u64>,
        #[serde(default)]
        name: Option<String>,
        id: i64,
        #[serde(default)]
        title: String,
        #[serde(default)]
        mapper: String,
    },
}

async fn session_from(app: &AppHandle, rid: u32, session: String) -> serde_json::Value {
    let state = app.state::<Atlas>();
    if !state.account.take_reimagined(now_secs()) {
        return serde_json::json!({ "t": "error", "rid": rid, "text": "Atlas did not ask for it." });
    }
    match state.account.sign_in(&state.net, &session, now_secs(), crate::account::Via::Reimagined).await {
        Ok(player) => {
            let _ = app.emit("account-changed", ());
            serde_json::json!({ "t": "signed-in", "rid": rid, "name": player.name })
        }
        Err(e) => serde_json::json!({ "t": "error", "rid": rid, "text": e }),
    }
}

fn token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Equal tokens, compared in time that does not depend on where they differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `chrome-extension://<32 letters>` or `moz-extension://<uuid>`.
fn extension_origin(origin: &str) -> bool {
    let id = origin.strip_prefix("chrome-extension://").or_else(|| origin.strip_prefix("moz-extension://"));
    id.is_some_and(|id| (16..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

fn browser_name(s: &str) -> String {
    match s {
        "chrome" => "Chrome",
        "edge" => "Edge",
        "firefox" => "Firefox",
        "opera" => "Opera",
        "brave" => "Brave",
        _ => "Browser",
    }
    .into()
}

/// Reimagined's version as it says it ("1.6.0"): digits and dots only, short.
fn version_text(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_digit() || *c == '.').take(16).collect()
}

fn code_ok(s: &str) -> bool {
    s.len() == 4 && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

impl Bridge {
    pub fn load(file: PathBuf) -> Self {
        let mut paired = crate::store::load::<Vec<Paired>>(&file).unwrap_or_default();
        paired.retain(|p| extension_origin(&p.origin) && p.token.len() == 64);
        paired.truncate(MAX_PAIRED);
        Bridge {
            file,
            paired: Mutex::new(paired),
            live: Mutex::new(Vec::new()),
            pending: Mutex::new(None),
            quiet_until: AtomicU64::new(0),
            port: Mutex::new(None),
            listening: AtomicBool::new(false),
            connections: AtomicUsize::new(0),
            next_conn: AtomicU64::new(1),
            near: Mutex::new(std::collections::HashMap::new()),
            log: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// Adds a line to the connection log (no tokens or codes, only what happened).
    fn note(&self, text: String) {
        let mut log = self.log.lock().unwrap();
        log.push_front(LogLine { at: now_secs(), text });
        log.truncate(LOG_LINES);
    }

    /// Near your rank, by map hash (Reimagined's latest list).
    pub fn near(&self) -> std::collections::HashMap<String, Near> {
        self.near.lock().unwrap().clone()
    }

    pub fn status(&self) -> Status {
        let live = self.live.lock().unwrap();
        Status {
            port: *self.port.lock().unwrap(),
            paired: self
                .paired
                .lock()
                .unwrap()
                .iter()
                .map(|p| PairedInfo {
                    id: p.id,
                    browser: p.browser.clone(),
                    created: p.created,
                    last_seen: p.last_seen,
                    version: p.version.clone(),
                    connected: live.iter().any(|(_, l)| l.paired == p.id),
                })
                .collect(),
            asking: self.pending.lock().unwrap().as_ref().map(|p| p.request.clone()),
            log: self.log.lock().unwrap().iter().cloned().collect(),
        }
    }

    /// The player's answer to the pairing request on screen.
    pub fn answer(&self, yes: bool) {
        if let Some(p) = self.pending.lock().unwrap().take() {
            let _ = p.reply.send(yes);
        }
    }

    /// Asks the connected extension for the browser's Rhythia sign-in (account.rs); false when none is
    /// connected. It answers with a `session` message, or `session-missing` without a rhythia.com tab.
    pub fn ask_session(&self) -> bool {
        let live = self.live.lock().unwrap();
        live.iter().any(|(_, l)| l.out.send(serde_json::json!({ "t": "session-ask" })).is_ok())
    }

    /// Removes a pairing and closes its connection; the extension has to pair again.
    pub fn forget(&self, id: u64) {
        let mut paired = self.paired.lock().unwrap();
        if let Some(p) = paired.iter().find(|p| p.id == id) {
            self.note(format!("{}: pairing removed", p.browser));
        }
        paired.retain(|p| p.id != id);
        if paired.is_empty() {
            self.near.lock().unwrap().clear();
        }
        let _ = crate::store::save(&self.file, &*paired);
        for (_, l) in self.live.lock().unwrap().iter().filter(|(_, l)| l.paired == id) {
            let _ = l.close.send(());
        }
    }

    /// The pairing a token belongs to (from the same extension), marked as seen now with the version it says.
    fn known(&self, token: &str, origin: &str, version: &str) -> Option<u64> {
        let mut paired = self.paired.lock().unwrap();
        let p = paired.iter_mut().find(|p| same(&p.token, token) && p.origin == origin)?;
        p.last_seen = now_secs();
        p.version = version_text(version);
        let id = p.id;
        self.save(&paired);
        Some(id)
    }

    /// A pairing's connection ended: it was last seen now.
    fn seen(&self, id: u64) {
        let mut paired = self.paired.lock().unwrap();
        if let Some(p) = paired.iter_mut().find(|p| p.id == id) {
            p.last_seen = now_secs();
            self.save(&paired);
        }
    }

    /// Puts a pairing request on screen, unless one is waiting already.
    fn ask(&self, request: Pending) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if pending.is_some() {
            return false;
        }
        *pending = Some(request);
        true
    }

    fn save(&self, paired: &[Paired]) {
        let _ = crate::store::save(&self.file, &paired.to_vec());
    }
}

fn changed(app: &AppHandle) {
    let _ = app.emit("bridge-changed", app.state::<Atlas>().bridge.status());
}

pub async fn serve(app: AppHandle) {
    if app.state::<Atlas>().bridge.listening.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut found = None;
    for port in PORTS {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)).await {
            found = Some((l, port));
            break;
        }
    }
    let Some((listener, port)) = found else {
        let bridge = &app.state::<Atlas>().bridge;
        bridge.listening.store(false, Ordering::SeqCst);
        bridge.note("Atlas could not open its connection port (47231 to 47233 are taken)".into());
        changed(&app);
        return;
    };
    *app.state::<Atlas>().bridge.port.lock().unwrap() = Some(port);
    changed(&app);
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        if !peer.ip().is_loopback() {
            continue;
        }
        let bridge = &app.state::<Atlas>().bridge;
        if bridge.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            bridge.connections.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            connection(&app, stream).await;
            app.state::<Atlas>().bridge.connections.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// "Refresh connection": listens again if no port could be opened, then pings every connected browser and
/// waits for the answers (a browser that does not answer is dropped by its connection). The status after.
pub async fn check(app: &AppHandle) -> Status {
    let bridge = &app.state::<Atlas>().bridge;
    if bridge.port.lock().unwrap().is_none() {
        tauri::async_runtime::spawn(serve(app.clone()));
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let waits: Vec<oneshot::Receiver<()>> = bridge
        .live
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(_, l)| {
            let (tx, rx) = oneshot::channel();
            l.ping.send(tx).ok().map(|_| rx)
        })
        .collect();
    let asked = waits.len();
    let answered = futures_util::future::join_all(waits.into_iter().map(|rx| tokio::time::timeout(PONG_WITHIN + Duration::from_secs(1), rx)))
        .await
        .into_iter()
        .filter(|r| matches!(r, Ok(Ok(()))))
        .count();
    bridge.note(match (asked, answered) {
        (0, _) => "Checked: no browser connected".into(),
        (a, b) if a == b => "Checked: the browser answered".into(),
        _ => "Checked: a browser did not answer and was dropped".into(),
    });
    changed(app);
    bridge.status()
}

fn send(text: serde_json::Value) -> Message {
    Message::text(text.to_string())
}

// The handshake callback's error type is tungstenite's own (an HTTP response).
#[allow(clippy::result_large_err)]
async fn connection(app: &AppHandle, stream: TcpStream) {
    let mut origin = None;
    let check = |req: &Request, res: Response| -> Result<Response, ErrorResponse> {
        let o = req.headers().get("origin").and_then(|v| v.to_str().ok()).unwrap_or_default();
        if !extension_origin(o) {
            let mut refuse = ErrorResponse::new(Some("Only the Reimagined extension can connect.".into()));
            *refuse.status_mut() = StatusCode::FORBIDDEN;
            return Err(refuse);
        }
        origin = Some(o.to_string());
        Ok(res)
    };
    let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE));
    let Ok(Ok(mut ws)) =
        tokio::time::timeout(HELLO_WITHIN, tokio_tungstenite::accept_hdr_async_with_config(stream, check, Some(config))).await
    else {
        return;
    };
    let Some(origin) = origin else { return };

    // The first message says who it is: a known token, or a code to pair with.
    let hello = match tokio::time::timeout(HELLO_WITHIN, ws.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<FromExtension>(t.as_str()).ok(),
        _ => None,
    };
    let Some(FromExtension::Hello { v: 1, browser, version, token: tok, code }) = hello else {
        app.state::<Atlas>().bridge.note("A browser connected but did not say who it is; closed".into());
        changed(app);
        let _ = ws.send(send(serde_json::json!({ "t": "error", "text": "Unknown message." }))).await;
        return;
    };
    let bridge = &app.state::<Atlas>().bridge;
    let browser = browser_name(&browser);
    let paired_id = match (tok, code) {
        (Some(tok), _) => match bridge.known(&tok, &origin, &version) {
            Some(id) => {
                bridge.note(format!("{browser}: connected"));
                let _ = ws.send(send(serde_json::json!({ "t": "welcome", "atlas": env!("CARGO_PKG_VERSION") }))).await;
                id
            }
            None => {
                bridge.note(format!("{browser}: refused, its pairing is not in this Atlas (connect again in the popup)"));
                changed(app);
                let _ = ws.send(send(serde_json::json!({ "t": "unpaired" }))).await;
                return;
            }
        },
        (None, Some(code)) if code_ok(&code) => {
            let Some(id) = pair(app, &mut ws, &origin, &browser, &version, code).await else { return };
            id
        }
        _ => return,
    };

    let conn = bridge.next_conn.fetch_add(1, Ordering::SeqCst);
    let (close_tx, mut close_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let (ping_tx, mut ping_rx) = mpsc::unbounded_channel::<oneshot::Sender<()>>();
    bridge.live.lock().unwrap().push((conn, Live { paired: paired_id, close: close_tx, out: out_tx, ping: ping_tx }));
    changed(app);
    // A ping sent and not answered yet: until when the answer may take, and who waits for it.
    let mut pong_due: Option<tokio::time::Instant> = None;
    let mut pong_waits: Vec<oneshot::Sender<()>> = Vec::new();

    let mut last_install = tokio::time::Instant::now() - INSTALL_GAP;
    let mut last_scores = tokio::time::Instant::now() - SCORES_GAP;
    loop {
        tokio::select! {
            _ = close_rx.recv() => {
                let _ = ws.send(send(serde_json::json!({ "t": "unpaired" }))).await;
                break;
            }
            Some(out) = out_rx.recv() => {
                if ws.send(send(out)).await.is_err() {
                    break;
                }
            }
            Some(wait) = ping_rx.recv() => {
                pong_waits.push(wait);
                if pong_due.is_none() {
                    if ws.send(Message::Ping(Default::default())).await.is_err() {
                        break;
                    }
                    pong_due = Some(tokio::time::Instant::now() + PONG_WITHIN);
                }
            }
            _ = tokio::time::sleep_until(pong_due.unwrap_or_else(tokio::time::Instant::now)), if pong_due.is_some() => {
                bridge.note(format!("{browser}: did not answer the check, dropped"));
                break;
            }
            msg = ws.next() => {
                let Some(Ok(msg)) = msg else { break };
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    Message::Pong(_) => {
                        pong_due = None;
                        for wait in pong_waits.drain(..) {
                            let _ = wait.send(());
                        }
                        continue;
                    }
                    _ => continue,
                };
                let reply = match serde_json::from_str::<FromExtension>(text.as_str()) {
                    Ok(FromExtension::Ping {}) => serde_json::json!({ "t": "pong" }),
                    Ok(FromExtension::SessionMissing {}) => {
                        let _ = app.emit("reimagined-no-session", ());
                        continue;
                    }
                    Ok(FromExtension::Forget {}) => {
                        bridge.forget(paired_id);
                        continue;
                    }
                    Ok(FromExtension::Scores { rid, name }) => {
                        if last_scores.elapsed() < SCORES_GAP {
                            serde_json::json!({ "t": "error", "rid": rid, "text": "Too fast. Try again in a moment." })
                        } else {
                            last_scores = tokio::time::Instant::now();
                            scores(app, rid, name).await
                        }
                    }
                    Ok(FromExtension::Have { rid, ids }) if ids.len() <= MAX_HAVE => have(rid, ids).await,
                    Ok(FromExtension::Near { rid, maps }) if maps.len() <= MAX_NEAR => {
                        let kept: std::collections::HashMap<String, Near> =
                            maps.into_iter().filter_map(Near::checked).map(|n| (n.hash.clone(), n)).collect();
                        let n = kept.len();
                        *app.state::<Atlas>().bridge.near.lock().unwrap() = kept;
                        let _ = app.emit("near-changed", n);
                        serde_json::json!({ "t": "near", "rid": rid, "kept": n })
                    }
                    Ok(FromExtension::Session { rid, session }) if session.len() <= 2560 => session_from(app, rid, session).await,
                    Ok(FromExtension::Collections { rid }) => {
                        let list: Vec<_> = app.state::<Atlas>().mine.all().into_iter()
                            .map(|c| serde_json::json!({ "id": c.id, "name": c.name, "maps": c.maps.len() }))
                            .collect();
                        serde_json::json!({ "t": "collections", "rid": rid, "list": list })
                    }
                    Ok(FromExtension::Collect { rid, collection, name, id, title, mapper }) if (1..10_000_000_000).contains(&id) => {
                        collect(app, rid, collection, name, id, title, mapper)
                    }
                    Ok(FromExtension::InstallList { rid, ids }) if !ids.is_empty() && ids.len() <= MAX_INSTALL_LIST => {
                        let mut ids: Vec<i64> = ids.into_iter().filter(|n| (1..10_000_000_000).contains(n)).collect();
                        ids.sort_unstable();
                        ids.dedup();
                        if last_install.elapsed() < INSTALL_GAP || ids.is_empty() {
                            serde_json::json!({ "t": "error", "rid": rid, "text": "Too fast. Try again in a moment." })
                        } else {
                            last_install = tokio::time::Instant::now();
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.unminimize();
                                let _ = w.set_focus();
                            }
                            let _ = app.emit("install-ask", serde_json::json!({ "ids": ids, "from": browser }));
                            serde_json::json!({ "t": "asked", "rid": rid, "maps": ids.len() })
                        }
                    }
                    Ok(FromExtension::Install { kind, id }) if (1..10_000_000_000).contains(&id) => {
                        if last_install.elapsed() < INSTALL_GAP {
                            serde_json::json!({ "t": "error", "id": id, "text": "Too fast. Try again in a moment." })
                        } else {
                            last_install = tokio::time::Instant::now();
                            install_from(app, &kind, id)
                        }
                    }
                    _ => serde_json::json!({ "t": "error", "text": "Unknown message." }),
                };
                if ws.send(send(reply)).await.is_err() {
                    break;
                }
            }
        }
    }
    bridge.live.lock().unwrap().retain(|(c, _)| *c != conn);
    bridge.seen(paired_id);
    bridge.note(format!("{browser}: disconnected"));
    changed(app);
}

async fn pair(
    app: &AppHandle,
    ws: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    origin: &str,
    browser: &str,
    version: &str,
    code: String,
) -> Option<u64> {
    let bridge = &app.state::<Atlas>().bridge;
    let refuse = |text: &str| send(serde_json::json!({ "t": "denied", "text": text }));
    if now_secs() < bridge.quiet_until.load(Ordering::SeqCst) {
        let _ = ws.send(refuse("Atlas said no a moment ago. Try again in half a minute.")).await;
        return None;
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    let request = PairRequest { code: code.clone(), browser: browser.to_string() };
    if !bridge.ask(Pending { request, reply: reply_tx }) {
        let _ = ws.send(refuse("Atlas is already asking about another connection.")).await;
        return None;
    }
    let _ = ws.send(send(serde_json::json!({ "t": "pending" }))).await;
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
    let _ = app.emit("bridge-pair", PairRequest { code, browser: browser.to_string() });
    changed(app);

    // Waits for the player; a closed socket ends the wait too.
    let yes = tokio::select! {
        answer = tokio::time::timeout(ANSWER_WITHIN, reply_rx) => matches!(answer, Ok(Ok(true))),
        _ = async { while let Some(Ok(_)) = ws.next().await {} } => false,
    };
    bridge.pending.lock().unwrap().take();
    changed(app);
    if !yes {
        bridge.quiet_until.store(now_secs() + QUIET_AFTER_NO_SECS, Ordering::SeqCst);
        let _ = ws.send(refuse("Atlas did not allow it.")).await;
        return None;
    }
    let token = token().ok()?;
    let now = now_secs();
    let id = {
        let mut paired = bridge.paired.lock().unwrap();
        // One pairing per extension: a new one replaces the old.
        paired.retain(|p| p.origin != origin);
        if paired.len() >= MAX_PAIRED {
            paired.remove(0);
        }
        let id = paired.iter().map(|p| p.id).max().unwrap_or(0) + 1;
        paired.push(Paired { id, browser: browser.to_string(), origin: origin.to_string(), token: token.clone(), created: now, last_seen: now, version: version_text(version) });
        bridge.save(&paired);
        id
    };
    bridge.note(format!("{browser}: paired"));
    ws.send(send(serde_json::json!({ "t": "welcome", "atlas": env!("CARGO_PKG_VERSION"), "token": token }))).await.ok()?;
    Some(id)
}

async fn scores(app: &AppHandle, rid: u32, name: String) -> serde_json::Value {
    let name = crate::library::clip(&name, 100);
    if name.is_empty() {
        return serde_json::json!({ "t": "error", "rid": rid, "text": "No player name." });
    }
    let read = tauri::async_runtime::spawn_blocking(move || {
        let db = crate::game::GameData::locate()?.database();
        crate::library::best_passes(&db, Some(&name))
    })
    .await;
    let passes = match read {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return serde_json::json!({ "t": "error", "rid": rid, "text": e }),
        Err(e) => return serde_json::json!({ "t": "error", "rid": rid, "text": e.to_string() }),
    };
    // Types of Ranked maps, as Atlas knows them (the same answers its own lists use).
    let state = app.state::<Atlas>();
    let hashes: Vec<String> = passes.iter().filter(|p| p.status.as_deref() == Some("RANKED")).filter_map(|p| p.hash.clone()).collect();
    let kinds = state.vps.kinds(&hashes).await;
    let maps: Vec<_> = passes
        .into_iter()
        .map(|p| {
            let k = p.hash.as_ref().and_then(|h| kinds.get(h));
            serde_json::json!({
                "id": p.id, "rp": p.rp, "acc": p.accuracy, "miss": p.misses, "at": p.at, "speed": p.speed, "mods": p.mods,
                "n": p.passes, "stars": p.stars, "status": p.status,
                "kind": k.map(|k| k.kind.clone()), "quantum": k.map(|k| k.quantum),
            })
        })
        .collect();
    serde_json::json!({ "t": "scores", "rid": rid, "maps": maps })
}

async fn have(rid: u32, ids: Vec<i64>) -> serde_json::Value {
    let have = tauri::async_runtime::spawn_blocking(|| crate::game::GameData::locate().and_then(|g| crate::library::online_ids(&g.database())))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let found: Vec<i64> = ids.into_iter().filter(|id| have.contains(id)).collect();
    serde_json::json!({ "t": "have", "rid": rid, "ids": found })
}

fn collect(app: &AppHandle, rid: u32, collection: Option<u64>, name: Option<String>, id: i64, title: String, mapper: String) -> serde_json::Value {
    let mine = &app.state::<Atlas>().mine;
    let item = crate::mine::Item { online_id: Some(id), hash: None, title, mapper };
    let done = match (collection, name) {
        (Some(c), _) => mine.add(c, vec![item]).map(|n| (c, n)),
        (None, Some(name)) => match mine.all().into_iter().find(|c| c.name.eq_ignore_ascii_case(name.trim())) {
            Some(c) => mine.add(c.id, vec![item]).map(|n| (c.id, n)),
            None => mine.create(&name, vec![item], now_secs()).map(|c| (c.id, 1)),
        },
        (None, None) => Err("Pick a collection.".into()),
    };
    match done {
        Ok((c, added)) => {
            let _ = app.emit("collections-changed", ());
            let name = mine.get(c).map(|c| c.name).unwrap_or_default();
            serde_json::json!({ "t": "collected", "rid": rid, "collection": c, "name": name, "added": added > 0 })
        }
        Err(e) => serde_json::json!({ "t": "error", "rid": rid, "text": e }),
    }
}

fn install_from(app: &AppHandle, kind: &str, id: i64) -> serde_json::Value {
    match kind {
        "map" => match install::start(app, vec![id], install::Mode::Game) {
            Ok(plan) => serde_json::json!({ "t": "installed", "id": id, "queued": plan.queued, "already": plan.already }),
            Err(e) => serde_json::json!({ "t": "error", "id": id, "text": e }),
        },
        "collection" => {
            crate::show_link(app, AtlasLink { action: "open", kind: "collection", id });
            serde_json::json!({ "t": "opened", "id": id })
        }
        _ => serde_json::json!({ "t": "error", "text": "Unknown message." }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_and_codes() {
        assert!(extension_origin("chrome-extension://nolildalkjlmlineopggepelhhlkmfhl"));
        assert!(extension_origin("moz-extension://3f1c2b4e-1111-4a2b-9c3d-0123456789ab"));
        assert!(!extension_origin("https://www.rhythia.com"));
        assert!(!extension_origin("chrome-extension://"));
        assert!(!extension_origin("chrome-extension://abc/../x"));
        assert!(!extension_origin("null"));
        assert!(code_ok("AB12") && !code_ok("ab12") && !code_ok("ABCDE"));
        assert_eq!(version_text("1.6.0"), "1.6.0");
        assert_eq!(version_text("1.6.0<b>x</b>"), "1.6.0");
        assert_eq!(version_text(&"9".repeat(40)).len(), 16);
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
        assert_eq!(token().unwrap().len(), 64);
    }

    #[test]
    fn messages_are_strict() {
        let ok = |s: &str| serde_json::from_str::<FromExtension>(s).is_ok();
        assert!(ok(r#"{"t":"hello","v":1,"browser":"chrome","version":"1.6.0","code":"AB12"}"#));
        assert!(ok(r#"{"t":"install","kind":"map","id":5}"#));
        assert!(!ok(r#"{"t":"install","kind":"map","id":5,"session":"x"}"#));
        assert!(!ok(r#"{"t":"other"}"#));
        assert!(ok(r#"{"t":"forget"}"#));
        assert!(!ok(r#"{"t":"forget","id":1}"#));
        assert!(ok(r#"{"t":"session-missing"}"#));
        assert!(ok(r#"{"t":"ping"}"#) && !ok(r#"{"t":"ping","x":1}"#));
        assert!(ok(r#"{"t":"scores","rid":1,"name":"Me"}"#));
        assert!(ok(r#"{"t":"have","rid":2,"ids":[1,2]}"#));
        assert!(ok(r#"{"t":"install-list","rid":5,"ids":[1,2]}"#) && !ok(r#"{"t":"install-list","rid":5,"ids":[1],"to":"zip"}"#));
        assert!(ok(r#"{"t":"collect","rid":3,"name":"Mine","id":5,"title":"x"}"#));
        assert!(!ok(r#"{"t":"scores","rid":1,"name":"Me","token":"x"}"#));
        assert!(ok(r#"{"t":"session","rid":2,"session":"a.b.c"}"#));
        assert!(!ok(r#"{"t":"session","rid":2,"session":"a.b.c","refresh":"x"}"#), "the session alone, nothing else");
        let h = "a".repeat(64);
        assert!(ok(&format!(r#"{{"t":"near","rid":4,"maps":[{{"hash":"{h}","place":3,"of":20,"level":"at","rpMid":300,"accMid":98.5}}]}}"#)));
        assert!(!ok(&format!(r#"{{"t":"near","rid":4,"maps":[{{"hash":"{h}","place":3,"of":20,"name":"x"}}]}}"#)), "no other fields, names included");
        let near = |place, of, level: &str| Near { hash: h.to_uppercase(), place, of, level: Some(level.into()), rp_mid: Some(f64::NAN), acc_mid: Some(99.0) }.checked();
        let n = near(3, 20, "odd").unwrap();
        assert_eq!((n.hash.as_str(), n.level, n.rp_mid, n.acc_mid), (h.as_str(), None, None, Some(99.0)));
        assert!(near(21, 20, "at").is_none() && near(0, 20, "at").is_none() && near(1, 1, "at").is_none());
    }
}
