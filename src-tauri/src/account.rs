//! The player's Rhythia sign-in. Rhythia signs in on its own website (Discord or e-mail) and keeps a
//! session token there; Atlas never sees a password. Two ways in:
//! - the Rhythia window: www.rhythia.com in a window of its own (its own browser profile, no access to
//!   Atlas at all), where the player signs in as on the website; Atlas reads only the session token from
//!   that page and catches it on a navigation it cancels (to `atlas-session.invalid`, never sent);
//! - Reimagined: the paired extension hands the token over (bridge.rs), only while the player has asked
//!   for it in Atlas within the last two minutes and agreed in the browser.
//!
//! Either way the token is checked with Rhythia (`getProfile` answers the signed-in player), kept in the
//! Windows password store (secret.rs) and sent only to production.rhythia.com. Rhythia's token lasts about
//! an hour, so Atlas renews it the way the player signed in: the Rhythia window opened hidden for a moment
//! (the website renews its own session), or Reimagined asked again (it reads the browser's sign-in). Who is
//! signed in and which way (no token) is kept in `account.json`, so a restart or an update shows the player
//! signed in while the token is renewed.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::net::Net;
use crate::{now_secs, Atlas};

const SECRET_NAME: &str = "AtlasForRhythia/rhythia-session";
const CAPTURE_HOST: &str = "atlas-session.invalid";
const WINDOW: &str = "rhythia";
const SITE: &str = "https://www.rhythia.com/";
/// A token this close to its end counts as ended.
const MARGIN_SECS: u64 = 60;
/// How long a "Sign in with Reimagined" waits for the extension.
const REIMAGINED_WAIT_SECS: u64 = 120;
/// How long a renewal through Reimagined waits for the extension's answer.
const REIMAGINED_RENEW_SECS: u64 = 20;

/// The visible window opens Rhythia's sign-in form by itself: once the page is ready, it presses the
/// "Sign in" button (once per window; the player can close the form).
const OPEN_FORM: &str = r#"(function(){if(window.top!==window||sessionStorage.getItem('atlas-form'))return;var n=0;var t=setInterval(function(){n++;var b=[].slice.call(document.querySelectorAll('button')).find(function(x){return x.textContent.trim()==='Sign in'&&x.offsetParent;});if(b){sessionStorage.setItem('atlas-form','1');clearInterval(t);b.click();}else if(n>60){clearInterval(t);}},250);})();"#;

/// Reads the session token from the Rhythia page, the same keys Reimagined reads, and hands a new one
/// over by a navigation Atlas cancels. Runs inside www.rhythia.com only.
const READ_SESSION: &str = r#"(function(){try{var t='';var p=localStorage.getItem('rhythia_auth_session_v1');if(p&&p.trim())t=p.trim();if(!t){for(var i=0;i<localStorage.length;i++){var k=localStorage.key(i);if(!k||!/^sb-.+-auth-token$/.test(k))continue;var v=JSON.parse(localStorage.getItem(k)||'null');var a=v&&(v.access_token||(v.session&&v.session.access_token)||(Array.isArray(v)&&v[0]&&v[0].access_token));if(typeof a==='string'&&a.length>10){t=a;break;}}}if(t&&t!==window.__atlasSent){window.__atlasSent=t;location.assign('https://atlas-session.invalid/#'+encodeURIComponent(t));}}catch(e){}})();"#;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Player {
    pub id: i64,
    pub name: String,
}

/// How the player signed in, so Atlas renews the token the same way.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    #[default]
    Window,
    Reimagined,
}

/// `account.json`: who is signed in and how (no token).
#[derive(Serialize, Deserialize)]
struct Kept {
    player: Player,
    via: Via,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub player: Option<Player>,
    /// A token is kept (it may have ended; Atlas renews it when needed).
    pub signed_in: bool,
    /// The kept token has ended and could not be renewed yet.
    pub expired: bool,
    pub via: Via,
    pub reimagined_waiting: bool,
    /// The last renewal or check failed, and why (Settings shows it until a sign-in or check works).
    pub problem: Option<String>,
    /// When Rhythia last accepted the sign-in (unix seconds, 0: not in this run).
    pub checked: u64,
}

fn b64url(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// When a session token (a JWT) ends, in unix seconds, if it looks like one at all.
pub fn expires_at(token: &str) -> Option<u64> {
    if !(40..=2560).contains(&token.len()) {
        return None;
    }
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let payload: serde_json::Value = serde_json::from_slice(&b64url(parts[1])?).ok()?;
    payload.get("exp")?.as_u64()
}

pub struct Account {
    file: PathBuf,
    token: Mutex<Option<String>>,
    player: Mutex<Option<Player>>,
    via: Mutex<Via>,
    reimagined_until: AtomicU64,
    renewing: tokio::sync::Mutex<()>,
    problem: Mutex<Option<String>>,
    checked: AtomicU64,
}

impl Account {
    pub fn load(file: PathBuf) -> Self {
        let token = crate::secret::load(SECRET_NAME).filter(|t| expires_at(t).is_some());
        let kept = token.as_ref().and_then(|_| crate::store::load::<Kept>(&file));
        Account {
            token: Mutex::new(token),
            player: Mutex::new(kept.as_ref().map(|k| k.player.clone())),
            via: Mutex::new(kept.map(|k| k.via).unwrap_or_default()),
            file,
            reimagined_until: AtomicU64::new(0),
            renewing: tokio::sync::Mutex::new(()),
            problem: Mutex::new(None),
            checked: AtomicU64::new(0),
        }
    }

    pub fn status(&self, now: u64) -> Status {
        let signed_in = self.token.lock().unwrap().is_some();
        Status {
            player: self.player.lock().unwrap().clone(),
            signed_in,
            expired: signed_in && self.current(now).is_none(),
            via: *self.via.lock().unwrap(),
            reimagined_waiting: self.reimagined_until.load(Ordering::SeqCst) > now,
            problem: self.problem.lock().unwrap().clone().filter(|_| signed_in),
            checked: self.checked.load(Ordering::SeqCst),
        }
    }

    fn failed(&self, why: &str) -> String {
        *self.problem.lock().unwrap() = Some(why.to_string());
        why.to_string()
    }

    /// The kept token while it lasts.
    fn current(&self, now: u64) -> Option<String> {
        self.token.lock().unwrap().clone().filter(|t| expires_at(t).is_some_and(|e| e > now + MARGIN_SECS))
    }

    /// Checks a token with Rhythia and keeps it; the player it belongs to.
    pub async fn sign_in(&self, net: &Net, token: &str, now: u64, via: Via) -> Result<Player, String> {
        if !expires_at(token).is_some_and(|e| e > now + MARGIN_SECS) {
            return Err("That Rhythia sign-in has ended. Sign in again.".into());
        }
        let player = crate::rhythia::me(net, token).await?;
        // Kept in memory even when the password store refuses it (it is then gone after a restart).
        let _ = crate::secret::save(SECRET_NAME, token);
        *self.token.lock().unwrap() = Some(token.to_string());
        *self.player.lock().unwrap() = Some(player.clone());
        *self.via.lock().unwrap() = via;
        *self.problem.lock().unwrap() = None;
        self.checked.store(now, Ordering::SeqCst);
        let _ = crate::store::save(&self.file, &Kept { player: player.clone(), via });
        Ok(player)
    }

    /// At start and on "Check" in Settings: renews the token when it has ended and asks Rhythia who it
    /// belongs to. `account-changed` follows either way.
    pub async fn check(&self, app: &AppHandle, net: &Net) -> Result<Player, String> {
        if self.token.lock().unwrap().is_none() {
            return Err("Not signed in to Rhythia.".into());
        }
        let done = async {
            let token = self.fresh_token(app, net).await?;
            let player = crate::rhythia::me(net, &token).await.map_err(|e| self.failed(&format!("Rhythia did not answer the check: {e}")))?;
            *self.player.lock().unwrap() = Some(player.clone());
            *self.problem.lock().unwrap() = None;
            self.checked.store(now_secs(), Ordering::SeqCst);
            let via = *self.via.lock().unwrap();
            let _ = crate::store::save(&self.file, &Kept { player: player.clone(), via });
            Ok(player)
        }
        .await;
        let _ = app.emit("account-changed", ());
        done
    }

    /// A token that lasts, renewed through the hidden Rhythia window when the kept one has ended.
    pub async fn fresh_token(&self, app: &AppHandle, net: &Net) -> Result<String, String> {
        if let Some(t) = self.current(now_secs()) {
            return Ok(t);
        }
        if self.token.lock().unwrap().is_none() {
            return Err("Sign in to Rhythia first (Settings).".into());
        }
        let _one = self.renewing.lock().await;
        if let Some(t) = self.current(now_secs()) {
            return Ok(t);
        }
        let ended = "Your Rhythia sign-in has ended. Sign in again (Settings → Account).";
        let via = *self.via.lock().unwrap();
        if via == Via::Reimagined {
            // Reimagined reads the browser's sign-in again (an open rhythia.com tab is needed).
            self.wait_for_reimagined(now_secs());
            if !app.state::<Atlas>().bridge.ask_session() {
                return Err(self.failed("Your Rhythia sign-in has ended. Open your browser with Reimagined, then try again."));
            }
            for _ in 0..REIMAGINED_RENEW_SECS * 2 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Some(t) = self.current(now_secs()) {
                    return Ok(t);
                }
            }
            return Err(self.failed("Your Rhythia sign-in has ended. Open rhythia.com in your browser, then try again."));
        }
        open_window(app, false).map_err(|e| self.failed(&e))?;
        let got = wait_for_token(app, net, 30).await;
        close_window(app);
        got.map_err(|_| self.failed(ended))?;
        self.current(now_secs()).ok_or_else(|| self.failed(ended))
    }

    pub fn sign_out(&self) {
        crate::secret::delete(SECRET_NAME);
        *self.token.lock().unwrap() = None;
        *self.player.lock().unwrap() = None;
        *self.problem.lock().unwrap() = None;
        self.checked.store(0, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.file);
    }

    /// "Sign in with Reimagined": the extension may hand a token over for the next two minutes.
    pub fn wait_for_reimagined(&self, now: u64) {
        self.reimagined_until.store(now + REIMAGINED_WAIT_SECS, Ordering::SeqCst);
    }

    /// Whether the player asked for it; asking again needs a new click.
    pub fn take_reimagined(&self, now: u64) -> bool {
        self.reimagined_until.swap(0, Ordering::SeqCst) > now
    }
}

/// The browser profile of the Rhythia and preview windows, in Atlas' local data folder next to the main
/// window's own (EBWebView), never in the roaming one: Microsoft warns against WebView2 data on a network
/// drive, which a roaming folder can be, and a player saw the Rhythia window stay empty while the main window
/// worked. Versions up to 0.13.8 kept it in the roaming folder; it moves over once (the sign-in on the website
/// moves with it), and what cannot move is deleted, since nothing reads it any more.
fn web_profile(app: &AppHandle) -> Result<PathBuf, String> {
    let local = app.path().app_local_data_dir().map_err(|e| e.to_string())?;
    let profile = local.join("rhythia-web");
    if let Ok(old) = app.path().app_data_dir().map(|d| d.join("rhythia-web")) {
        if old.exists() && old != profile {
            if !profile.exists() {
                let _ = std::fs::create_dir_all(&local);
                let _ = std::fs::rename(&old, &profile);
            }
            if old.exists() {
                let _ = std::fs::remove_dir_all(&old);
            }
        }
    }
    Ok(profile)
}

/// Whether a page has finished loading in the Rhythia window since it opened.
static PAGE_LOADED: AtomicBool = AtomicBool::new(false);
/// How long the visible window may stay without a loaded page before Atlas loads it again, and then
/// before it says so in Settings.
const LOAD_SECS: u64 = 20;

/// The Rhythia window: www.rhythia.com with its own browser profile and no access to Atlas (no
/// capability names it). Only https pages open in it; the token comes on a navigation to
/// `atlas-session.invalid`, which is cancelled before anything is sent.
pub fn open_window(app: &AppHandle, visible: bool) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(WINDOW) {
        if visible {
            let _ = w.show();
            let _ = w.set_focus();
        }
        return Ok(());
    }
    let data = web_profile(app)?;
    let state = app.state::<Atlas>();
    let tx = state.account_tx.clone();
    let mut builder = WebviewWindowBuilder::new(app, WINDOW, WebviewUrl::External(SITE.parse().expect("a fixed address")));
    if visible {
        builder = builder.initialization_script(OPEN_FORM);
    }
    PAGE_LOADED.store(false, Ordering::SeqCst);
    builder
        .title("Sign in to Rhythia")
        .center()
        .inner_size(520.0, 760.0)
        .visible(visible)
        .data_directory(data)
        .on_page_load(|_, page| {
            if page.event() == PageLoadEvent::Finished {
                PAGE_LOADED.store(true, Ordering::SeqCst);
            }
        })
        .on_navigation(move |url| {
            if url.host_str() == Some(CAPTURE_HOST) {
                if let Some(token) = url.fragment().and_then(urlencoding_decode) {
                    let _ = tx.try_send(token);
                }
                return false;
            }
            url.scheme() == "https"
        })
        .build()
        .map_err(|e| format!("The Rhythia window did not open: {e}"))?;
    Ok(())
}

/// The map preview window: Rhythia's own map page (`/maps/<id>`) with everything but its "Map preview"
/// (Rhythia's player with auto play) hidden. Same browser profile as the sign-in window and, like it, no
/// access to Atlas; only rhythia.com pages open in it.
const PREVIEW_WINDOW: &str = "preview";
const SHOW_PREVIEW: &str = r#"(function(){if(window.top!==window||!/^\/maps\/\d+/.test(location.pathname))return;var st=null,n=0;var t=setInterval(function(){n++;if(!st&&document.head){st=document.createElement('style');st.textContent='html,body{overflow:hidden!important}body{visibility:hidden}#atlas-pv{visibility:visible;position:fixed!important;inset:0!important;z-index:2147483646!important;overflow:auto!important;margin:0!important;border-radius:0!important;background:#0a0a0a!important}';document.head.appendChild(st);}var h=[].slice.call(document.querySelectorAll('h2')).find(function(x){return x.textContent.trim()==='Map preview';});var c=h&&h.closest('.rounded-xl');if(c){c.id='atlas-pv';clearInterval(t);}else if(n>150){clearInterval(t);if(st)st.remove();}},100);})();"#;

pub fn open_preview(app: &AppHandle, id: i64) -> Result<(), String> {
    if !(1..10_000_000_000).contains(&id) {
        return Err("Unknown map.".into());
    }
    let url: tauri::Url = format!("https://www.rhythia.com/maps/{id}").parse().map_err(|_| "Unknown map.")?;
    if let Some(w) = app.get_webview_window(PREVIEW_WINDOW) {
        let _ = w.navigate(url);
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    let data = web_profile(app)?;
    WebviewWindowBuilder::new(app, PREVIEW_WINDOW, WebviewUrl::External(url))
        .title("Map preview")
        .center()
        .inner_size(1000.0, 640.0)
        .data_directory(data)
        .initialization_script(SHOW_PREVIEW)
        .on_navigation(|url| url.scheme() == "https" && matches!(url.host_str(), Some("www.rhythia.com" | "rhythia.com")))
        .build()
        .map_err(|e| format!("The preview window did not open: {e}"))?;
    Ok(())
}

fn close_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(WINDOW) {
        let _ = w.close();
    }
}

fn urlencoding_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Asks the open Rhythia window for its session every second until a token that lasts and that Rhythia
/// accepts arrives, the window is closed or `secs` pass.
async fn wait_for_token(app: &AppHandle, net: &Net, secs: u64) -> Result<crate::account::Player, String> {
    let state = app.state::<Atlas>();
    let mut rx = state.account_rx.lock().await;
    while rx.try_recv().is_ok() {}
    for _ in 0..secs {
        let Some(w) = app.get_webview_window(WINDOW) else { return Err("closed".into()) };
        let _ = w.eval(READ_SESSION);
        if let Ok(Some(token)) = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            if let Ok(player) = state.account.sign_in(net, &token, now_secs(), Via::Window).await {
                return Ok(player);
            }
        }
    }
    Err("timeout".into())
}

/// "Sign in to Rhythia": the visible window, waited on for up to ten minutes; it closes once signed in.
/// `account-changed` follows a sign-in, `account-window-closed` a window closed (or given up) without one.
pub async fn sign_in_with_window(app: AppHandle) {
    if let Err(e) = open_window(&app, true) {
        let _ = app.emit("account-window-closed", e);
        return;
    }
    // A window that shows no page: loaded again once, then Settings says so (`account-window-stuck`).
    let watch = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(LOAD_SECS)).await;
        let Some(w) = watch.get_webview_window(WINDOW).filter(|_| !PAGE_LOADED.load(Ordering::SeqCst)) else { return };
        let _ = w.navigate(SITE.parse().expect("a fixed address"));
        tokio::time::sleep(Duration::from_secs(LOAD_SECS)).await;
        if !PAGE_LOADED.load(Ordering::SeqCst) && watch.get_webview_window(WINDOW).is_some() {
            let _ = watch.emit("account-window-stuck", ());
        }
    });
    let state = app.state::<Atlas>();
    if wait_for_token(&app, &state.net, 600).await.is_ok() {
        close_window(&app);
        let _ = app.emit("account-changed", ());
    } else {
        close_window(&app);
        let _ = app.emit("account-window-closed", "");
    }
}

/// Signs out: forgets the token and clears the Rhythia window's own browser profile.
pub async fn sign_out(app: &AppHandle) {
    let state = app.state::<Atlas>();
    state.account.sign_out();
    if open_window(app, false).is_ok() {
        if let Some(w) = app.get_webview_window(WINDOW) {
            let _ = w.clear_all_browsing_data();
        }
        close_window(app);
    }
    let _ = app.emit("account-changed", ());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(exp: u64) -> String {
        let enc = |s: &str| {
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let b = s.as_bytes();
            let mut out = String::new();
            for chunk in b.chunks(3) {
                let n = chunk.iter().fold(0u32, |a, &c| (a << 8) | c as u32) << (8 * (3 - chunk.len()));
                for i in 0..=chunk.len() {
                    out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
                }
            }
            out
        };
        format!("{}.{}.{}", enc(r#"{"alg":"HS256"}"#), enc(&format!(r#"{{"sub":"x","exp":{exp}}}"#)), "c2lnbmF0dXJlc2lnbmF0dXJl")
    }

    #[test]
    fn reads_when_a_token_ends() {
        assert_eq!(expires_at(&jwt(1_800_000_000)), Some(1_800_000_000));
        assert_eq!(expires_at("not.a.token"), None);
        assert_eq!(expires_at(&"a".repeat(3000)), None);
        assert_eq!(expires_at("eyJh.e30.sig-but-too-short"), None);
    }

    #[test]
    fn decodes_the_caught_fragment() {
        assert_eq!(urlencoding_decode("a.b%2Ec").as_deref(), Some("a.b.c"));
        assert_eq!(urlencoding_decode("%zz"), None);
    }

    #[test]
    fn reimagined_waits_only_after_a_click_and_only_once() {
        let a = Account { file: std::env::temp_dir().join("atlas-account-test.json"), via: Mutex::new(Via::Window), token: Mutex::new(None), player: Mutex::new(None), reimagined_until: AtomicU64::new(0), renewing: tokio::sync::Mutex::new(()), problem: Mutex::new(None), checked: AtomicU64::new(0) };
        assert!(!a.take_reimagined(100));
        a.wait_for_reimagined(100);
        assert!(a.status(150).reimagined_waiting);
        assert!(!a.take_reimagined(300), "two minutes have passed");
        a.wait_for_reimagined(100);
        assert!(a.take_reimagined(150));
        assert!(!a.take_reimagined(151), "one click, one hand-over");
    }
}
