//! The only way Atlas talks to the internet, except asking GitHub for updates (updates.rs: Tauri's
//! updater, one address from the config, signed installers only). Every request goes through `Net`, which:
//! - knows three hosts (Rhythia's API, Rhythia's file server, our VPS) and refuses every other URL;
//! - follows no redirects, sends no cookies and nothing about the player in its headers;
//! - paces Rhythia (API and files together) to at most 10 requests a second and 4 at once,
//!   and pauses everything for Rhythia when it answers 429;
//! - asks our VPS nothing while it said to wait (429 and Retry-After): a refused budget only waits
//!   for its window, and knocking on it again and again would close the key for a while;
//! - checks every body for the VPS against `vps_body_ok` (no tokens, passwords or long strings);
//! - reads at most `MAX_JSON_BYTES` of an answer (cover images: what the caller allows);
//! - keeps Rhythia's public answers (catalog pages, collections, map pages, rankings, players' profiles and
//!   top plays; never anything with a sign-in) for `KEEP_FOR`, so going back and forth in Atlas asks Rhythia
//!   once; any change Atlas makes on Rhythia (a collection) forgets them all.
//!
//! The webview has no network access of its own (CSP `connect-src ipc:`), so the frontend can
//! only reach the internet through the commands that use this module.

use std::time::Duration;

use reqwest::{header, redirect, Client, Response, StatusCode, Url};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore, SemaphorePermit};
use tokio::time::{sleep_until, Instant};

/// Hard limits towards Rhythia. Not settings: raising them risks the player's account and
/// Rhythia's servers (see TERMS.md).
pub const RHYTHIA_PER_SECOND: u32 = 10;
pub const RHYTHIA_AT_ONCE: usize = 4;
/// When Rhythia answers 429 without Retry-After, wait this long.
const RHYTHIA_BACKOFF: Duration = Duration::from_secs(30);
const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_VPS_BODY_BYTES: usize = 15_000;
const TIMEOUT: Duration = Duration::from_secs(20);
/// How long a public answer from Rhythia is reused.
const KEEP_FOR: Duration = Duration::from_secs(10 * 60);
/// Public answers kept at most (the oldest go first).
const KEEP_AT_MOST: usize = 300;
/// The read-only Rhythia calls whose answers are kept, when sent without a sign-in.
const KEPT_PATHS: [&str; 8] = [
    "/api/getBeatmaps",
    "/api/getCollections",
    "/api/getCollection",
    "/api/getBeatmapPage",
    "/api/getBeatmapPageById",
    "/api/getLeaderboard",
    "/api/getUserScores",
    "/api/getProfile",
];

/// Rhythia's public answers, by path and body.
#[derive(Default)]
struct Kept {
    answers: std::collections::HashMap<String, (Instant, std::sync::Arc<Vec<u8>>)>,
}

impl Kept {
    fn get(&self, key: &str, now: Instant) -> Option<std::sync::Arc<Vec<u8>>> {
        self.answers.get(key).filter(|(at, _)| now.duration_since(*at) < KEEP_FOR).map(|(_, b)| b.clone())
    }

    fn put(&mut self, key: String, bytes: Vec<u8>, now: Instant) {
        self.answers.retain(|_, (at, _)| now.duration_since(*at) < KEEP_FOR);
        while self.answers.len() >= KEEP_AT_MOST {
            let Some(oldest) = self.answers.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone()) else { break };
            self.answers.remove(&oldest);
        }
        self.answers.insert(key, (now, std::sync::Arc::new(bytes)));
    }
}

/// The key under which an answer is kept: only a public read (no sign-in in the body).
fn kept_key(host: Host, path: &str, body: &Value, text: &str) -> Option<String> {
    let public = body.get("session").is_none_or(|s| s.as_str() == Some(""));
    (host == Host::RhythiaApi && KEPT_PATHS.contains(&path) && public).then(|| format!("{path} {text}"))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Host {
    RhythiaApi,
    RhythiaFiles,
    Vps,
}

impl Host {
    pub const fn name(self) -> &'static str {
        match self {
            Host::RhythiaApi => "production.rhythia.com",
            Host::RhythiaFiles => "static.rhythia.com",
            Host::Vps => "atlas.shuriel.com",
        }
    }

    /// The host of an allowed URL: https, default port, no user info, one of the three names.
    pub fn of(url: &Url) -> Option<Host> {
        if url.scheme() != "https" || url.port().is_some() || !url.username().is_empty() || url.password().is_some() {
            return None;
        }
        [Host::RhythiaApi, Host::RhythiaFiles, Host::Vps]
            .into_iter()
            .find(|h| url.host_str() == Some(h.name()))
    }

    const fn is_rhythia(self) -> bool {
        matches!(self, Host::RhythiaApi | Host::RhythiaFiles)
    }
}

/// Spaces requests to Rhythia evenly and caps how many run at once.
struct Pace {
    next: Mutex<Instant>,
    at_once: Semaphore,
    gap: Duration,
}

impl Pace {
    fn new(per_second: u32, at_once: usize) -> Self {
        Pace {
            next: Mutex::new(Instant::now()),
            at_once: Semaphore::new(at_once),
            gap: Duration::from_secs(1) / per_second,
        }
    }

    async fn wait(&self) -> SemaphorePermit<'_> {
        let permit = self.at_once.acquire().await.expect("the semaphore is never closed");
        let slot = {
            let mut next = self.next.lock().await;
            let slot = (*next).max(Instant::now());
            *next = slot + self.gap;
            slot
        };
        sleep_until(slot).await;
        permit
    }

    async fn pause(&self, how_long: Duration) {
        let until = Instant::now() + how_long;
        let mut next = self.next.lock().await;
        if *next < until {
            *next = until;
        }
    }
}

pub struct Net {
    http: Client,
    rhythia: Pace,
    /// Until when our VPS asked Atlas to wait (its 429 and Retry-After).
    vps_wait: std::sync::Mutex<Option<Instant>>,
    kept: std::sync::Mutex<Kept>,
    /// Changes Atlas made on Rhythia; reads after one ask with another body (`fresh_size`).
    changes: std::sync::atomic::AtomicU32,
    /// The changes the last fresh read saw, and its step (`fresh_size`).
    fresh: std::sync::Mutex<(u32, u32)>,
}

impl Net {
    pub fn new() -> Result<Self, String> {
        let http = Client::builder()
            .user_agent(concat!("AtlasForRhythia/", env!("CARGO_PKG_VERSION")))
            .redirect(redirect::Policy::none())
            .https_only(true)
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| format!("Network setup failed: {e}"))?;
        Ok(Net { http, rhythia: Pace::new(RHYTHIA_PER_SECOND, RHYTHIA_AT_ONCE), vps_wait: std::sync::Mutex::new(None), kept: std::sync::Mutex::default(), changes: Default::default(), fresh: Default::default() })
    }

    /// POST a JSON body to `path` (starts with `/api/`) on `host` and read a JSON answer.
    /// `headers` are allowed only for the VPS, and only its installation key.
    pub async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        host: Host,
        path: &str,
        body: &B,
        headers: &[(&'static str, &str)],
    ) -> Result<T, String> {
        let url = url_for(host, path)?;
        let body = serde_json::to_value(body).map_err(|e| format!("Request could not be built: {e}"))?;
        if host == Host::Vps {
            vps_body_ok(&body)?;
            vps_headers_ok(headers)?;
        } else if !headers.is_empty() {
            return Err("Blocked: extra headers are only for our server.".into());
        }
        let text = serde_json::to_string(&body).map_err(|e| e.to_string())?;
        if host == Host::Vps && text.len() > MAX_VPS_BODY_BYTES {
            return Err("Blocked: request to our server is too large.".into());
        }

        if host == Host::Vps {
            if let Some(left) = self.vps_wait_left() {
                return Err(format!("{} asked Atlas to wait. Try again in {} s.", who(host), left.as_secs().max(1)));
            }
        }
        let key = kept_key(host, path, &body, &text);
        if let Some(bytes) = key.as_ref().and_then(|k| self.kept.lock().unwrap().get(k, Instant::now())) {
            return serde_json::from_slice(&bytes).map_err(|_| format!("{} answered in an unexpected format.", who(host)));
        }
        // A change on Rhythia (a collection made or edited): what was kept may be out of date.
        if host == Host::RhythiaApi && key.is_none() && !KEPT_PATHS.contains(&path) && path != "/api/getProfile" {
            self.forget_kept();
            self.changes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let _permit = if host.is_rhythia() { Some(self.rhythia.wait().await) } else { None };
        let mut req = self
            .http
            .post(url)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json")
            .body(text);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let res = req.send().await.map_err(|e| offline(host, &e))?;
        let bytes = self.read_body(host, res, MAX_JSON_BYTES).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| format!("{} answered in an unexpected format.", who(host)))?;
        // Rhythia answers some refusals with 200 and only an error in the body ("Can't find beatmap page").
        if host == Host::RhythiaApi {
            if let Some(e) = value.get("error").and_then(Value::as_str) {
                return Err(format!("Rhythia: {}", e.chars().take(200).collect::<String>()));
            }
        }
        let value = serde_json::from_value(value).map_err(|_| format!("{} answered in an unexpected format.", who(host)))?;
        if let Some(k) = key {
            self.kept.lock().unwrap().put(k, bytes, Instant::now());
        }
        Ok(value)
    }

    /// A page size (`size` minus 0 to 39) for a read that must see Atlas' latest change on Rhythia. Rhythia keeps
    /// its answers for a while by the exact body: the first read after each change takes the next
    /// size, so it never repeats one of the last 40 bodies read after a change; reads without a change in between
    /// keep the size (and Rhythia's kept answer, which is current).
    pub fn fresh_size(&self, size: u32) -> u32 {
        let changes = self.changes.load(std::sync::atomic::Ordering::SeqCst);
        let mut fresh = self.fresh.lock().unwrap();
        if fresh.0 != changes {
            *fresh = (changes, (fresh.1 + 1) % 40);
        }
        size - fresh.1.min(size / 2)
    }

    /// Drops Rhythia's kept answers (after a change there, or Refresh).
    pub fn forget_kept(&self) {
        self.kept.lock().unwrap().answers.clear();
    }

    /// GET a cover image from Rhythia's file server: only `https://static.rhythia.com/` with a
    /// plain path, at most `max` bytes, paced like every request to Rhythia.
    pub async fn get_image(&self, url: &str, max: usize) -> Result<Vec<u8>, String> {
        Ok(self.get_file(url, max).await?.0)
    }

    /// GET a file from Rhythia's file server (covers, map files) with the SHA-1 the server states for
    /// it (`x-bz-content-sha1`), if any: only `https://static.rhythia.com/` with a plain path (escaped
    /// characters allowed, no `..`, no query), at most `max` bytes, paced like every request to Rhythia.
    pub async fn get_file(&self, url: &str, max: usize) -> Result<(Vec<u8>, Option<String>), String> {
        let url = Url::parse(url).map_err(|_| "Blocked: unexpected address.")?;
        let plain = url.query().is_none()
            && url.fragment().is_none()
            && url.path().len() <= 500
            && url.path().bytes().all(|b| b.is_ascii_alphanumeric() || b"/-_.%".contains(&b))
            && !url.path().contains("..");
        if Host::of(&url) != Some(Host::RhythiaFiles) || !plain {
            return Err("Blocked: unexpected address.".into());
        }
        let _permit = self.rhythia.wait().await;
        let res = self.http.get(url).send().await.map_err(|e| offline(Host::RhythiaFiles, &e))?;
        let sha1 = res
            .headers()
            .get("x-bz-content-sha1")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().trim_start_matches("unverified:").to_ascii_lowercase())
            .filter(|v| v.len() == 40 && v.bytes().all(|b| b.is_ascii_hexdigit()));
        Ok((self.read_body(Host::RhythiaFiles, res, max).await?, sha1))
    }

    /// How long our VPS still wants Atlas to wait, if it does.
    fn vps_wait_left(&self) -> Option<Duration> {
        let until = (*self.vps_wait.lock().unwrap())?;
        until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())
    }

    async fn read_body(&self, host: Host, mut res: Response, max: usize) -> Result<Vec<u8>, String> {
        let status = res.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let wait = res
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|s| Duration::from_secs(s.clamp(1, 3600)))
                .unwrap_or(RHYTHIA_BACKOFF);
            if host.is_rhythia() {
                self.rhythia.pause(wait).await;
            } else {
                *self.vps_wait.lock().unwrap() = Some(Instant::now() + wait);
            }
            return Err(format!("{} asked Atlas to slow down. Try again in {} s.", who(host), wait.as_secs()));
        }
        if res.content_length().is_some_and(|n| n as usize > max) {
            return Err(format!("{} sent an answer that is too large.", who(host)));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|e| offline(host, &e))? {
            if bytes.len() + chunk.len() > max {
                return Err(format!("{} sent an answer that is too large.", who(host)));
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(format!("{} answered with error {}.", who(host), status.as_u16()));
        }
        Ok(bytes)
    }
}

fn url_for(host: Host, path: &str) -> Result<Url, String> {
    let safe = path.starts_with("/api/")
        && path.len() <= 200
        && path.bytes().all(|b| b.is_ascii_alphanumeric() || b"/-_".contains(&b));
    if !safe {
        return Err("Blocked: unexpected address.".into());
    }
    let url = Url::parse(&format!("https://{}{}", host.name(), path)).map_err(|_| "Blocked: unexpected address.")?;
    if Host::of(&url) != Some(host) {
        return Err("Blocked: unexpected address.".into());
    }
    Ok(url)
}

fn who(host: Host) -> &'static str {
    if host.is_rhythia() { "Rhythia" } else { "Atlas server" }
}

fn offline(host: Host, e: &reqwest::Error) -> String {
    if e.is_timeout() {
        format!("{} did not answer in time.", who(host))
    } else {
        format!("Could not reach {}. Check your connection.", who(host))
    }
}

/// Names that never belong in a body for our server, at any depth.
const VPS_FORBIDDEN_FIELDS: [&str; 10] =
    ["session", "token", "access_token", "refresh_token", "password", "login", "email", "auth", "cookie", "userconfig"];
/// No value we send to the VPS is longer (a map hash is 64).
const VPS_MAX_STRING: usize = 64;

/// The VPS gets only short, known values: no field names from the list above, no string longer
/// than a map hash and nothing shaped like a JWT (Rhythia's session is one). Requests are typed
/// structs already; this is the second lock in case a future change gets it wrong.
pub fn vps_body_ok(body: &Value) -> Result<(), String> {
    match body {
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(()),
        Value::String(s) => {
            if s.len() > VPS_MAX_STRING || looks_like_jwt(s) {
                Err("Blocked: a value Atlas never sends to its server.".into())
            } else {
                Ok(())
            }
        }
        Value::Array(items) => items.iter().try_for_each(vps_body_ok),
        Value::Object(fields) => fields.iter().try_for_each(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            if VPS_FORBIDDEN_FIELDS.iter().any(|f| lower.contains(f)) {
                Err("Blocked: a field Atlas never sends to its server.".into())
            } else {
                vps_body_ok(value)
            }
        }),
    }
}

fn looks_like_jwt(s: &str) -> bool {
    s.starts_with("eyJ") || (s.split('.').count() == 3 && s.len() > 40)
}

/// The installation key is the only header Atlas adds for the VPS.
fn vps_headers_ok(headers: &[(&'static str, &str)]) -> Result<(), String> {
    for (name, value) in headers {
        if *name != crate::vps::KEY_HEADER || !crate::vps::is_key(value) {
            return Err("Blocked: a header Atlas never sends to its server.".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_public_answers_only() {
        let key = |host, path, body: Value| kept_key(host, path, &body, &body.to_string());
        assert!(key(Host::RhythiaApi, "/api/getBeatmaps", json!({"session": "", "page": 1})).is_some());
        assert!(key(Host::RhythiaApi, "/api/getCollection", json!({"session": "x.y.z", "collection": 1})).is_none(), "never with a sign-in");
        assert!(key(Host::RhythiaApi, "/api/addCollectionMap", json!({"session": ""})).is_none());
        assert!(key(Host::Vps, "/api/getBeatmaps", json!({})).is_none());
        let t0 = Instant::now();
        let mut kept = Kept::default();
        kept.put("a".into(), b"1".to_vec(), t0);
        assert!(kept.get("a", t0 + Duration::from_secs(60)).is_some());
        assert!(kept.get("a", t0 + KEEP_FOR).is_none(), "out of date after KEEP_FOR");
        for n in 0..KEEP_AT_MOST + 5 {
            kept.put(n.to_string(), Vec::new(), t0 + Duration::from_millis(n as u64));
        }
        assert_eq!(kept.answers.len(), KEEP_AT_MOST);
        assert!(kept.get("0", t0).is_none(), "the oldest went first");
    }

    #[test]
    fn only_three_hosts() {
        let ok = |s: &str| Host::of(&Url::parse(s).unwrap());
        assert_eq!(ok("https://production.rhythia.com/api/getBeatmaps"), Some(Host::RhythiaApi));
        assert_eq!(ok("https://static.rhythia.com/a.rhm"), Some(Host::RhythiaFiles));
        assert_eq!(ok("https://atlas.shuriel.com/api/bundle"), Some(Host::Vps));
        assert_eq!(ok("https://rhythia.shuriel.com/api/maps/kinds"), None, "the extension's address is not Atlas'");
        assert_eq!(ok("http://production.rhythia.com/api/x"), None);
        assert_eq!(ok("https://production.rhythia.com:8443/api/x"), None);
        assert_eq!(ok("https://user@production.rhythia.com/api/x"), None);
        assert_eq!(ok("https://evil.com/api/x"), None);
        assert_eq!(ok("https://production.rhythia.com.evil.com/api/x"), None);
        assert_eq!(ok("https://www.rhythia.com/"), None);
    }

    #[test]
    fn paths_are_plain() {
        assert!(url_for(Host::RhythiaApi, "/api/getBeatmaps").is_ok());
        assert!(url_for(Host::RhythiaApi, "/api/../x").is_err());
        assert!(url_for(Host::RhythiaApi, "/api/x?y=1").is_err());
        assert!(url_for(Host::RhythiaApi, "//evil.com/api/x").is_err());
        assert!(url_for(Host::RhythiaApi, "/other").is_err());
    }

    #[test]
    fn vps_body_refuses_secrets() {
        let hash = "b79b29d8895065db84a45e1e8f507f7e15361f0ef9f6ef44b41da4cdf3391fcf";
        assert!(vps_body_ok(&json!({ "keys": [hash, "123"] })).is_ok());
        assert!(vps_body_ok(&json!({ "session": "" })).is_err());
        assert!(vps_body_ok(&json!({ "keys": [], "x": { "accessToken": 1 } })).is_err());
        assert!(vps_body_ok(&json!({ "keys": ["eyJhbGciOiJIUzI1NiJ9"] })).is_err());
        assert!(vps_body_ok(&json!({ "keys": ["a".repeat(65)] })).is_err());
        assert!(vps_body_ok(&json!({ "keys": ["aaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbbbbbb.cccccccccc"] })).is_err());
    }

    #[test]
    fn vps_headers_only_key() {
        let key = format!("ra1.{}", "A".repeat(48));
        assert!(vps_headers_ok(&[(crate::vps::KEY_HEADER, &key)]).is_ok());
        assert!(vps_headers_ok(&[(crate::vps::KEY_HEADER, "eyJ...")]).is_err());
        assert!(vps_headers_ok(&[("authorization", &key)]).is_err());
    }

    #[test]
    fn a_read_after_each_change_asks_with_a_new_body() {
        let net = Net::new().unwrap();
        assert_eq!(net.fresh_size(100), 100);
        let change = || net.changes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut sizes = Vec::new();
        for _ in 0..40 {
            change();
            change();
            let s = net.fresh_size(100);
            assert_eq!(net.fresh_size(100), s, "no change in between: the same body");
            sizes.push(s);
        }
        sizes.sort();
        sizes.dedup();
        assert_eq!(sizes.len(), 40, "40 changes in a row, 40 different bodies");
        assert!(sizes.iter().all(|s| (61..=100).contains(s)));
    }

    #[test]
    fn waits_for_the_vps_without_asking() {
        let net = Net::new().unwrap();
        assert!(net.vps_wait_left().is_none());
        *net.vps_wait.lock().unwrap() = Some(Instant::now() + Duration::from_secs(90));
        let got: Result<Value, String> =
            tauri::async_runtime::block_on(net.post_json(Host::Vps, "/api/bundle", &json!({}), &[]));
        let e = got.unwrap_err();
        assert!(e.contains("asked Atlas to wait") && (e.contains("90 s") || e.contains("89 s")), "{e}");
        *net.vps_wait.lock().unwrap() = Some(Instant::now());
        assert!(net.vps_wait_left().is_none(), "the wait is over");
    }
}
