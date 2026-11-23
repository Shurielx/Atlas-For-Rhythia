//! "At which speed" (For you) from our server (`/api/foryou`): how the player would do on the maps Atlas
//! shows, at each speed the game offers. Atlas sends the player's public Rhythia number and, for each map, its
//! Rhythia id (0 without one), stars and notes (0 when unknown); the server guesses from the player's public
//! Top 100 and sends back only the rows the window shows. How it guesses stays on the server; nothing from the
//! game's database goes out.
//!
//! The answers are kept on disk (`for-you.json`): a map is asked again after a day (the server reads Top 100
//! lists once a day), and without the server (offline, its limits, an error) the last answer shows for a week.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::net::Net;
use crate::vps::Vps;

/// Maps in one request (the server takes no more), and in one call from the window.
const PER_REQUEST: usize = 100;
const MAX_MAPS: usize = 1_000;
const MAX_SPEEDS: usize = 12;
const MAX_NOTES: i64 = 10_000_000;
/// An answer is asked again after this long, and shown without the server for this long.
const FRESH_SECS: u64 = 86_400;
const SHOWN_SECS: u64 = 7 * 86_400;
/// Answers kept on disk, at most.
const KEPT_AT_MOST: usize = 20_000;
/// Answers kept by another version of this are dropped (2: the server guesses on Rhythia's stars, 0.12.1).
const STORE_VERSION: u32 = 2;

/// A map as the server gets it: [Rhythia id or 0, stars, notes or 0].
pub type Ask = (i64, f64, i64);

#[derive(Deserialize)]
pub struct Answer {
    available: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    speeds: Vec<f64>,
    #[serde(default)]
    maps: Vec<Option<Guess>>,
}

/// One map: per speed [chance a try in %, few, middle and many misses, and with RP its low, middle and high],
/// `bet` the best speed's index or -1, `best` the player's best RP on the map that the server knows.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct Guess {
    pub rows: Vec<Vec<i64>>,
    pub bet: i64,
    pub best: Option<f64>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct ForYou {
    pub speeds: Vec<f64>,
    /// In the order asked; none for a map without an answer.
    pub maps: Vec<Option<Guess>>,
}

/// What the server said, by map, for one player.
#[derive(Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    version: u32,
    player: i64,
    speeds: Vec<f64>,
    /// Map key -> (when, the answer: none when the server could not guess the map).
    maps: HashMap<String, (u64, Option<Guess>)>,
}

#[derive(Default)]
pub struct Kept {
    store: tokio::sync::Mutex<Option<Store>>,
}

fn clean(maps: &[Ask]) -> Vec<Ask> {
    maps.iter()
        .map(|&(id, stars, notes)| {
            let stars = if stars.is_finite() && stars > 0.0 && stars <= 50.0 { (stars * 100.0).round().max(1.0) / 100.0 } else { 0.0 };
            (id.clamp(0, i32::MAX as i64), stars, notes.clamp(0, MAX_NOTES))
        })
        .collect()
}

fn key_of(&(id, stars, notes): &Ask) -> String {
    if id > 0 { format!("o{id}") } else { format!("s{stars}:{notes}") }
}

fn speeds_ok(speeds: &[f64]) -> bool {
    !speeds.is_empty() && speeds.len() <= MAX_SPEEDS && speeds.iter().all(|s| s.is_finite() && (0.1..=10.0).contains(s))
}

fn guess_ok(g: &Guess, speeds: usize) -> bool {
    g.rows.len() == speeds
        && g.rows.iter().all(|r| {
            (r.len() == 4 || r.len() == 7)
                && (0..=100).contains(&r[0])
                && r[1..4].iter().all(|m| (0..=MAX_NOTES).contains(m))
                && r[4..].iter().all(|p| (0..=100_000).contains(p))
        })
        && (-1..speeds as i64).contains(&g.bet)
        && g.best.map_or(true, |b| b.is_finite() && (0.0..=100_000.0).contains(&b))
}

enum Read {
    /// Speeds and an answer per map asked.
    Got(Vec<f64>, Vec<Option<Guess>>),
    /// The server knows no plays of the player.
    NoPlays,
    /// The server's limits for today: the kept answers stay.
    Limit,
}

/// The server's answer for `asked` maps, checked.
fn read(a: Answer, asked: usize) -> Result<Read, String> {
    if !a.available {
        return Ok(if a.reason.as_deref() == Some("limit") { Read::Limit } else { Read::NoPlays });
    }
    if !speeds_ok(&a.speeds) || a.maps.len() != asked {
        return Err("Our server answered in an unexpected format.".into());
    }
    let n = a.speeds.len();
    Ok(Read::Got(a.speeds, a.maps.into_iter().map(|g| g.filter(|g| guess_ok(g, n))).collect()))
}

/// The kept answers read back from disk are used only when they look like ones Atlas wrote.
fn sane(s: &Store) -> bool {
    s.version == STORE_VERSION && speeds_ok(&s.speeds) && s.maps.len() <= KEPT_AT_MOST && s.maps.values().all(|(_, g)| g.as_ref().map_or(true, |g| guess_ok(g, s.speeds.len())))
}

impl Kept {
    /// For you for `maps`: kept answers while fresh, the rest asked, a hundred a request; without the server the
    /// kept ones up to a week old. None when the server knows no plays of the player.
    pub async fn ask(&self, net: &Net, vps: &Vps, player: i64, maps: &[Ask], now: u64, file: &Path) -> Result<Option<ForYou>, String> {
        if maps.is_empty() || maps.len() > MAX_MAPS {
            return Err("Too many maps at once.".into());
        }
        let maps = clean(maps);
        let keys: Vec<String> = maps.iter().map(key_of).collect();
        let mut kept = self.store.lock().await;
        if kept.as_ref().map_or(true, |s| s.player != player) {
            *kept = Some(crate::store::load::<Store>(file).filter(|s| s.player == player && sane(s)).unwrap_or(Store { version: STORE_VERSION, player, ..Default::default() }));
        }
        let store = kept.as_mut().unwrap();
        let age = |k: &str, s: &Store| s.maps.get(k).map(|(at, _)| now.saturating_sub(*at));

        let mut want: Vec<(String, Ask)> = Vec::new();
        for (k, m) in keys.iter().zip(&maps) {
            if age(k, store).map_or(true, |a| a >= FRESH_SECS) && !want.iter().any(|(w, _)| w == k) {
                want.push((k.clone(), *m));
            }
        }
        let mut failed = None;
        let mut changed = false;
        for part in want.chunks(PER_REQUEST) {
            let asks: Vec<Ask> = part.iter().map(|(_, m)| *m).collect();
            match vps.for_you(net, player, &asks, now).await.and_then(|a| read(a, asks.len())) {
                Ok(Read::Got(speeds, guesses)) => {
                    if store.speeds != speeds {
                        store.maps.clear();
                        store.speeds = speeds;
                    }
                    for ((k, _), g) in part.iter().zip(guesses) {
                        store.maps.insert(k.clone(), (now, g));
                    }
                    changed = true;
                }
                Ok(Read::NoPlays) => {
                    *store = Store { version: STORE_VERSION, player, ..Default::default() };
                    let _ = crate::store::save(file, &*store);
                    return Ok(None);
                }
                Ok(Read::Limit) => break,
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        if changed {
            store.maps.retain(|_, (at, _)| now.saturating_sub(*at) < SHOWN_SECS);
            if store.maps.len() > KEPT_AT_MOST {
                let mut by_age: Vec<(u64, String)> = store.maps.iter().map(|(k, (at, _))| (*at, k.clone())).collect();
                by_age.sort();
                for (_, k) in by_age.into_iter().take(store.maps.len() - KEPT_AT_MOST) {
                    store.maps.remove(&k);
                }
            }
            let _ = crate::store::save(file, &*store);
        }
        let shown: Vec<Option<Guess>> = keys
            .iter()
            .map(|k| store.maps.get(k).filter(|(at, _)| now.saturating_sub(*at) < SHOWN_SECS).and_then(|(_, g)| g.clone()))
            .collect();
        if store.speeds.is_empty() || shown.iter().all(|g| g.is_none()) {
            if let Some(e) = failed {
                return Err(e);
            }
        }
        if store.speeds.is_empty() {
            return Ok(Some(ForYou { speeds: Vec::new(), maps: vec![None; keys.len()] }));
        }
        Ok(Some(ForYou { speeds: store.speeds.clone(), maps: shown }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_checks_the_answer() {
        let raw = r#"{"available":true,"speeds":[1,1.15],"maps":[
            {"rows":[[46,39,51,65,338,346,352],[21,78,101,129,400,420,436]],"bet":1,"best":420},
            {"rows":[[46,39,51,65],[21,78,101,129]],"bet":-1,"best":null},
            {"rows":[[146,39,51,65],[21,78,101,129]],"bet":0,"best":null},
            {"rows":[[46,39,51,65]],"bet":0,"best":null},
            {"rows":[[46,39,51,65],[21,78,101,129]],"bet":2,"best":null},
            null]}"#;
        let Read::Got(speeds, maps) = read(serde_json::from_str(raw).unwrap(), 6).unwrap() else { panic!() };
        assert_eq!(speeds, vec![1.0, 1.15]);
        assert_eq!(maps[0].as_ref().unwrap().best, Some(420.0));
        assert!(maps[1].is_some(), "unranked: no RP");
        assert!(maps[2..].iter().all(|g| g.is_none()), "a chance over 100%, a row missing, a bet past the speeds, none");
        assert!(read(serde_json::from_str(raw).unwrap(), 5).is_err(), "another number of maps");
        assert!(matches!(read(serde_json::from_str(r#"{"available":false,"reason":"no_plays"}"#).unwrap(), 1), Ok(Read::NoPlays)));
        assert!(matches!(read(serde_json::from_str(r#"{"available":false,"reason":"limit"}"#).unwrap(), 1), Ok(Read::Limit)));
        assert!(read(serde_json::from_str(r#"{"available":true,"speeds":[],"maps":[]}"#).unwrap(), 0).is_err());
    }

    #[test]
    fn sends_only_short_numbers() {
        assert_eq!(clean(&[(-5, 8.669999837875366, 4138), (7, f64::NAN, -1), (9, 99.0, 20_000_000)]), vec![(0, 8.67, 4138), (7, 0.0, 0), (9, 0.0, MAX_NOTES)]);
        assert_eq!((key_of(&(5, 8.67, 10)), key_of(&(0, 8.67, 10))), ("o5".to_string(), "s8.67:10".to_string()));
    }

    #[test]
    fn kept_answers_must_look_right() {
        let g = Guess { rows: vec![vec![46, 39, 51, 65]], bet: 0, best: None };
        let mut s = Store { version: STORE_VERSION, player: 7, speeds: vec![1.0], maps: HashMap::from([("o1".to_string(), (5, Some(g)))]) };
        assert!(sane(&s));
        s.version = 1;
        assert!(!sane(&s), "kept by another version");
        s.version = STORE_VERSION;
        s.speeds = vec![1.0, 1.15];
        assert!(!sane(&s), "rows of another number of speeds");
    }
}
