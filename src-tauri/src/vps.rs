//! Our server, at Atlas' own address (atlas.shuriel.com: `/api/key`, `/api/bundle`, `/api/near` and `/api/foryou`,
//! nothing else).
//! Atlas keeps the list of map types the server knows (`map-types.json`) and looks types up in it
//! locally, so the list never tells the server which maps the player looks at or has ("At which speed" does:
//! it sends the maps on screen). About once a day Atlas sends only an anonymous installation key (`ra1.`) and the version
//! of the list it has; the server answers with the maps changed since then (and the ones gone), or
//! nothing when it is current. The server's budgets per key and per address decide how often, and
//! the operator can block a key or an address. Nothing in this app is a secret: an edited copy of
//! Atlas gets the same answers as the real one. For "Played near your rank" (near.rs) Atlas sends the
//! player's public Rhythia number and gets back only counts and medians of the players ranked around them; for
//! "At which speed" (foryou.rs) the same number and the maps' ids, stars and notes, and gets back rows to show.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::library::is_hash;
use crate::net::{Host, Net};

pub const KEY_HEADER: &str = "x-atlas-key";
/// The server renews keys after 20 days and drops them after 30.
const KEY_RENEW_AFTER_SECS: u64 = 20 * 86_400;
/// How often Atlas asks whether the list changed, and how long it waits after a failed try.
const CHECK_EVERY_SECS: u64 = 86_400;
const RETRY_AFTER_FAIL_SECS: u64 = 3_600;
const MAX_MAPS: usize = 200_000;

pub fn is_key(s: &str) -> bool {
    s.len() == 52
        && s.starts_with("ra1.")
        && s[4..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A map in the list: the map's sha256 hash, lowercase.
pub fn is_map_key(s: &str) -> bool {
    is_hash(s) && !s.bytes().any(|b| b.is_ascii_uppercase())
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SavedKey {
    key: String,
    obtained_at: u64,
}

#[derive(Deserialize)]
struct KeyAnswer {
    key: String,
}

/// How a map plays: its type and its stream, jump and tech shares (0 to 1).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Kind {
    pub kind: String,
    pub stream: Option<f64>,
    pub jump: Option<f64>,
    pub tech: Option<f64>,
    #[serde(default)]
    pub quantum: bool,
}

/// The kept list: the server's version of it, when Atlas last heard from the server (unix seconds),
/// and the maps by hash.
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct TypeList {
    #[serde(default)]
    version: String,
    #[serde(default)]
    checked_at: u64,
    #[serde(default)]
    maps: HashMap<String, Kind>,
}

#[derive(Serialize)]
struct BundleQuery<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    have: Option<&'a str>,
}

#[derive(Serialize)]
struct NearQuery {
    player: i64,
}

#[derive(Serialize)]
struct ForYouQuery<'a> {
    player: i64,
    maps: &'a [crate::foryou::Ask],
}

#[derive(Deserialize)]
struct BundleAnswer {
    version: String,
    full: bool,
    #[serde(default)]
    maps: Vec<Value>,
    #[serde(default)]
    gone: Vec<String>,
}

/// One map of the server's list: `[hash, kind letter, stream %, jump %, tech %, quantum 0/1]`.
fn map_of(row: &Value) -> Option<(String, Kind)> {
    let a = row.as_array().filter(|a| a.len() == 6)?;
    let hash = a[0].as_str().filter(|h| is_map_key(h))?.to_string();
    let kind = match a[1].as_str()? {
        "j" => "jump",
        "s" => "stream",
        "t" => "tech",
        "v" => "varied",
        _ => return None,
    };
    let share = |v: &Value| v.as_f64().filter(|n| (0.0..=100.0).contains(n)).map(|n| n / 100.0);
    Some((hash, Kind { kind: kind.into(), stream: share(&a[2]), jump: share(&a[3]), tech: share(&a[4]), quantum: a[5].as_u64() == Some(1) }))
}

/// Takes the server's answer into the list; true when the list changed.
fn apply(list: &mut TypeList, answer: BundleAnswer, now: u64) -> Result<bool, String> {
    let plain = !answer.version.is_empty() && answer.version.len() <= 40 && answer.version.bytes().all(|b| b.is_ascii_hexdigit() || b == b'.');
    if !plain || answer.maps.len() > MAX_MAPS || answer.gone.len() > MAX_MAPS {
        return Err("Atlas server answered in an unexpected format.".into());
    }
    let changed = answer.full || !answer.maps.is_empty() || !answer.gone.is_empty();
    if answer.full {
        list.maps.clear();
    }
    list.maps.extend(answer.maps.iter().filter_map(map_of));
    for hash in &answer.gone {
        list.maps.remove(hash);
    }
    list.version = answer.version;
    list.checked_at = now;
    Ok(changed)
}

pub struct Vps {
    key_file: PathBuf,
    key: Mutex<Option<SavedKey>>,
    list_file: PathBuf,
    list: Mutex<TypeList>,
    /// When a try to reach the server last failed (unix seconds).
    failed_at: Mutex<u64>,
}

impl Vps {
    pub fn load(key_file: PathBuf, list_file: PathBuf) -> Self {
        let saved = crate::store::load::<SavedKey>(&key_file).filter(|k| is_key(&k.key));
        let mut list = crate::store::load::<TypeList>(&list_file).unwrap_or_default();
        list.maps.retain(|hash, k| is_map_key(hash) && ["jump", "stream", "tech", "varied"].contains(&k.kind.as_str()));
        Vps { key_file, key: Mutex::new(saved), list_file, list: Mutex::new(list), failed_at: Mutex::new(0) }
    }

    async fn key(&self, net: &Net, now: u64) -> Result<String, String> {
        let mut saved = self.key.lock().await;
        if let Some(k) = saved.as_ref() {
            if now >= k.obtained_at && now - k.obtained_at < KEY_RENEW_AFTER_SECS {
                return Ok(k.key.clone());
            }
        }
        let answer = net.post_json::<_, KeyAnswer>(Host::Vps, "/api/key", &serde_json::json!({}), &[]).await?;
        if !is_key(&answer.key) {
            return Err("Atlas server answered in an unexpected format.".into());
        }
        let fresh = SavedKey { key: answer.key, obtained_at: now };
        let _ = crate::store::save(&self.key_file, &fresh);
        let key = fresh.key.clone();
        *saved = Some(fresh);
        Ok(key)
    }

    /// Map types for these hashes, from the kept list (no request). Maps the server has no type for
    /// are left out.
    pub async fn kinds(&self, hashes: &[String]) -> HashMap<String, Kind> {
        let list = self.list.lock().await;
        hashes
            .iter()
            .filter_map(|h| {
                let k = h.to_ascii_lowercase();
                let kind = list.maps.get(&k)?.clone();
                Some((k, kind))
            })
            .collect()
    }

    /// Whether it is time to ask the server about the list: never asked, a day since the last
    /// answer, and not within an hour of a failed try.
    pub async fn due(&self, now: u64) -> bool {
        let list = self.list.lock().await;
        let failed = *self.failed_at.lock().await;
        (list.version.is_empty() || now.saturating_sub(list.checked_at) >= CHECK_EVERY_SECS)
            && now.saturating_sub(failed) >= RETRY_AFTER_FAIL_SECS
    }

    /// Asks the server what changed in the list since the version Atlas has; true when it changed.
    pub async fn refresh(&self, net: &Net, now: u64) -> Result<bool, String> {
        let got = self.ask_server(net, now).await;
        let answer = match got {
            Ok(a) => a,
            Err(e) => {
                *self.failed_at.lock().await = now;
                return Err(e);
            }
        };
        let mut list = self.list.lock().await;
        let changed = apply(&mut list, answer, now)?;
        let _ = crate::store::save(&self.list_file, &*list);
        Ok(changed)
    }

    async fn ask_server(&self, net: &Net, now: u64) -> Result<BundleAnswer, String> {
        let have = self.list.lock().await.version.clone();
        let have = (!have.is_empty()).then_some(have.as_str());
        self.with_key(net, now, |key| async move { net.post_json::<_, BundleAnswer>(Host::Vps, "/api/bundle", &BundleQuery { have }, &[(KEY_HEADER, &key)]).await })
            .await
    }

    /// How the players ranked around `player` (their public Rhythia number) play the maps in their tops.
    pub async fn near(&self, net: &Net, player: i64, now: u64) -> Result<crate::near::ServerNear, String> {
        if player <= 0 {
            return Err("Unknown player.".into());
        }
        self.with_key(net, now, |key| async move {
            net.post_json::<_, crate::near::ServerNear>(Host::Vps, "/api/near", &NearQuery { player }, &[(KEY_HEADER, &key)]).await
        })
        .await
    }

    /// For you on `maps` (at most 100) for `player` (their public Rhythia number).
    pub async fn for_you(&self, net: &Net, player: i64, maps: &[crate::foryou::Ask], now: u64) -> Result<crate::foryou::Answer, String> {
        if player <= 0 {
            return Err("Unknown player.".into());
        }
        self.with_key(net, now, |key| async move {
            net.post_json::<_, crate::foryou::Answer>(Host::Vps, "/api/foryou", &ForYouQuery { player, maps }, &[(KEY_HEADER, &key)]).await
        })
        .await
    }

    /// Asks with the installation key; a key the server no longer takes (expired, or its secret changed) is
    /// dropped and a new one asked for once.
    async fn with_key<T, F, Fut>(&self, net: &Net, now: u64, ask: F) -> Result<T, String>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<T, String>>,
    {
        match ask(self.key(net, now).await?).await {
            Err(e) if e.ends_with("error 401.") => {
                *self.key.lock().await = None;
                let _ = std::fs::remove_file(&self.key_file);
                ask(self.key(net, now).await?).await
            }
            got => got,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_and_map_keys() {
        assert!(is_key(&format!("ra1.{}", "a_B-9".repeat(9) + "abc")));
        assert!(!is_key("ra1.short"));
        assert!(!is_key(&format!("rk1.{}", "a_B-9".repeat(9) + "abc")), "an extension key is dropped and an Atlas key asked for");
        assert!(!is_key(&format!("rk2.{}", "A".repeat(48))));
        assert!(is_map_key(&"ab".repeat(32)));
        assert!(!is_map_key(&"AB".repeat(32)));
        assert!(!is_map_key("12345"));
    }

    fn answer(v: Value) -> BundleAnswer {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn whole_list_then_changes_then_nothing() {
        let (a, b, c) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        let mut list = TypeList::default();
        let full = json!({ "version": "0a1b2c3d.1.5", "full": true, "maps": [[a, "s", 61, 20, 19, 0], [b, "j", 10, 80, 10, 1]], "gone": [] });
        assert!(apply(&mut list, answer(full), 100).unwrap());
        assert_eq!(list.maps[&a], Kind { kind: "stream".into(), stream: Some(0.61), jump: Some(0.2), tech: Some(0.19), quantum: false });
        assert!(list.maps[&b].quantum);
        let delta = json!({ "version": "0a1b2c3d.2.5", "full": false, "maps": [[a, "t", 0, 0, 100, 0], [c, "v", null, null, null, 0]], "gone": [b] });
        assert!(apply(&mut list, answer(delta), 200).unwrap());
        assert_eq!((list.maps[&a].kind.as_str(), list.maps.contains_key(&b), list.maps[&c].stream), ("tech", false, None));
        let same = json!({ "version": "0a1b2c3d.2.5", "full": false, "maps": [], "gone": [] });
        assert!(!apply(&mut list, answer(same), 300).unwrap());
        assert_eq!((list.version.as_str(), list.checked_at, list.maps.len()), ("0a1b2c3d.2.5", 300, 2));
        // A new whole list replaces everything.
        let fresh = json!({ "version": "ffffffff.1.5", "full": true, "maps": [[c, "j", 1, 2, 3, 0]] });
        assert!(apply(&mut list, answer(fresh), 400).unwrap());
        assert_eq!(list.maps.len(), 1);
    }

    #[test]
    fn odd_rows_and_versions_are_refused() {
        let mut list = TypeList::default();
        let rows = json!({ "version": "0a.1.0", "full": true, "maps": [
            ["A".repeat(64), "s", 1, 1, 1, 0], ["d".repeat(64), "x", 1, 1, 1, 0], ["e".repeat(64), "s", 150, -1, 1, 0], ["f".repeat(64), "s", 1, 1]
        ] });
        apply(&mut list, answer(rows), 1).unwrap();
        assert_eq!(list.maps.len(), 1, "only the row with a plain hash and type");
        assert_eq!((list.maps[&"e".repeat(64)].stream, list.maps[&"e".repeat(64)].jump), (None, None));
        for version in ["", "<script>", &"1".repeat(41)] {
            assert!(apply(&mut list, answer(json!({ "version": version, "full": false })), 2).is_err(), "{version}");
        }
    }
}
