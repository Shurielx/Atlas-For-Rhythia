//! The players ranked near the player and how they play the maps in their tops. First from Shuriel's server
//! (`/api/near`, one request: the player's public Rhythia number goes out, only counts and medians come back,
//! for a band of a few hundred players around them, each map only with 5 players or more; players in
//! Rhythia's top 5000). Else, or when the server does not know the player, from Rhythia's public data: the
//! player's position (`getProfile`), the leaderboard page around it (`getLeaderboard`) and the top 100 plays
//! of the `NEIGHBOURS` closest players (`getUserScores`, `AT_ONCE` at a time; about 25 requests, paced like
//! every request to Rhythia). Either is kept for `KEEP_SECS`, also on disk (`near.json`, no names), so Atlas
//! started again shows it at once.
//!
//! The window uses it for "Played near your rank" (maps that group plays and the player does not have) and
//! for Near your rank on a map (how the group, those above and those below play it). Maps are keyed by
//! their song key (`songId`, the game's `Maps.LegacyId`).

use std::collections::HashMap;
use std::sync::Mutex;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::library::clip;
use crate::net::{Host, Net};
use crate::rhythia::OnlineMap;

/// Players in the group Atlas makes itself (the closest by position, above and below).
const NEIGHBOURS: usize = 20;
/// The group is asked again after this long (or when the player asks).
const KEEP_SECS: u64 = 6 * 3600;
const PER_PAGE: i64 = 50;
/// Players' top plays asked at once (the pace of every request to Rhythia still applies).
const AT_ONCE: usize = 4;
/// Maps in a group, at most (from the server, or read back from disk).
const MAX_MAPS: usize = 2_000;

/// How some players play a map: how many, and the middle of their RP, speed, misses and accuracy.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Around {
    pub players: i64,
    pub rp: f64,
    pub speed: Option<f64>,
    pub misses: Option<f64>,
    /// Hits of all notes, in percent.
    pub accuracy: Option<f64>,
}

/// One map in the group's tops.
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Play {
    /// The map's song key (`songId`).
    pub song: String,
    pub title: String,
    /// The map as the server knows it (none when the group comes from Rhythia): the list shows it at once.
    pub map: Option<OnlineMap>,
    pub all: Around,
    pub above: Option<Around>,
    pub below: Option<Around>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub at: u64,
    pub me: i64,
    pub position: i64,
    /// Players in the group above and below the player.
    pub above: i64,
    pub below: i64,
    /// "server" or "rhythia".
    pub from: String,
    pub plays: Vec<Play>,
}

#[derive(Default)]
pub struct Near {
    kept: Mutex<Option<Group>>,
    building: tokio::sync::Mutex<()>,
}

impl Near {
    /// The group for the player `me`: the kept one while it lasts (in memory, else in `file`), else asked
    /// again (the server first); `step(done, of)` follows the requests to Rhythia.
    pub async fn group(
        &self,
        net: &Net,
        vps: &crate::vps::Vps,
        me: i64,
        force: bool,
        now: u64,
        file: &std::path::Path,
        step: &(dyn Fn(usize, usize) + Sync),
    ) -> Result<Group, String> {
        let fresh = |g: &Group| g.me == me && g.at <= now && now - g.at < KEEP_SECS;
        if !force {
            if let Some(g) = self.kept.lock().unwrap().clone().filter(fresh) {
                return Ok(g);
            }
        }
        let _one = self.building.lock().await;
        if !force {
            if let Some(g) = self.kept.lock().unwrap().clone().filter(fresh) {
                return Ok(g);
            }
            if let Some(g) = crate::store::load::<Group>(file).filter(fresh).filter(sane) {
                *self.kept.lock().unwrap() = Some(g.clone());
                return Ok(g);
            }
        }
        step(0, 1);
        let g = match vps.near(net, me, now).await.ok().and_then(|a| from_server(a, me, now)) {
            Some(g) => {
                step(1, 1);
                g
            }
            None => build(net, me, now, step).await?,
        };
        *self.kept.lock().unwrap() = Some(g.clone());
        let _ = crate::store::save(file, &g);
        Ok(g)
    }
}

/// A group read back from disk is used only when it looks like one Atlas wrote.
fn sane(g: &Group) -> bool {
    g.plays.len() <= MAX_MAPS && g.plays.iter().all(|p| song_ok(&p.song) && p.title.len() <= 1200 && around_ok(&p.all))
}

fn around_ok(a: &Around) -> bool {
    a.players > 0 && a.rp.is_finite() && (0.0..=100_000.0).contains(&a.rp)
}

// ---- From Shuriel's server ----

#[derive(Deserialize)]
pub struct ServerNear {
    #[serde(default)]
    available: bool,
    rank: Option<f64>,
    band: Option<ServerBand>,
    #[serde(default)]
    maps: Vec<ServerMap>,
}

#[derive(Deserialize)]
struct ServerBand {
    above: Option<f64>,
    below: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerMap {
    key: Option<String>,
    id: Option<f64>,
    hash: Option<String>,
    title: Option<String>,
    mapper: Option<String>,
    status: Option<String>,
    stars: Option<f64>,
    length_ms: Option<f64>,
    plays: Option<f64>,
    all: Option<ServerAround>,
    above: Option<ServerAround>,
    below: Option<ServerAround>,
}

#[derive(Deserialize)]
struct ServerAround {
    players: Option<f64>,
    rp: Option<f64>,
    speed: Option<f64>,
    misses: Option<f64>,
    acc: Option<f64>,
}

fn server_around(a: Option<ServerAround>) -> Option<Around> {
    let a = a?;
    let around = Around {
        players: a.players.and_then(whole).filter(|n| *n > 0 && *n < 100_000)?,
        rp: a.rp.filter(|r| r.is_finite() && (0.0..=100_000.0).contains(r))?,
        speed: a.speed.filter(|s| s.is_finite() && (0.1..=10.0).contains(s)),
        misses: a.misses.filter(|m| m.is_finite() && (0.0..=1_000_000.0).contains(m)),
        accuracy: a.acc.filter(|x| x.is_finite() && (0.0..=100.0).contains(x)),
    };
    Some(around)
}

/// The server's answer as a group; none when the server does not know the player (or answered oddly).
fn from_server(a: ServerNear, me: i64, now: u64) -> Option<Group> {
    if !a.available || a.maps.len() > MAX_MAPS {
        return None;
    }
    let position = a.rank.and_then(whole).filter(|p| *p > 0)?;
    let band = a.band?;
    let count = |n: Option<f64>| n.and_then(whole).filter(|n| (0..100_000).contains(n)).unwrap_or(0);
    let plays = a
        .maps
        .into_iter()
        .filter_map(|m| {
            let song = m.key.filter(|k| song_ok(k))?;
            let title = clip(&m.title.unwrap_or_default(), 300);
            let map = m.id.and_then(whole).filter(|n| *n > 0 && *n < 10_000_000_000).map(|id| OnlineMap {
                id,
                tags: Vec::new(),
                title: title.clone(),
                mapper: clip(&m.mapper.unwrap_or_default(), 60),
                stars: m.stars.filter(|s| s.is_finite() && (0.0..=100_000.0).contains(s)),
                length_ms: m.length_ms.and_then(whole).filter(|n| (0..86_400_000).contains(n)),
                plays: m.plays.and_then(whole).filter(|n| *n >= 0).unwrap_or(0),
                status: m.status.filter(|s| matches!(s.as_str(), "RANKED" | "APPROVED" | "QUALIFIED" | "UNRANKED")),
                hash: m.hash.map(|h| h.to_ascii_lowercase()).filter(|h| crate::library::is_hash(h)),
                cover: false,
                installed: false,
            });
            Some(Play { song, title, map, all: server_around(m.all)?, above: server_around(m.above), below: server_around(m.below) })
        })
        .collect();
    Some(Group { at: now, me, position, above: count(band.above), below: count(band.below), from: "server".into(), plays })
}

// ---- From Rhythia ----

#[derive(Serialize)]
struct BoardQuery {
    session: &'static str,
    page: i64,
    spin: bool,
    vr: bool,
}

#[derive(Deserialize)]
struct BoardAnswer {
    #[serde(default)]
    leaderboard: Vec<RawRow>,
}

#[derive(Deserialize)]
struct RawRow {
    id: Option<f64>,
    position: Option<f64>,
}

#[derive(Serialize)]
struct ScoresQuery {
    session: &'static str,
    id: i64,
    limit: u32,
}

#[derive(Deserialize)]
struct ScoresAnswer {
    #[serde(default)]
    top: Vec<RawTop>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTop {
    song_id: Option<String>,
    beatmap_hash: Option<String>,
    beatmap_title: Option<String>,
    #[serde(rename = "awarded_sp")]
    awarded_sp: Option<f64>,
    passed: Option<bool>,
    speed: Option<f64>,
    misses: Option<f64>,
    beatmap_notes: Option<f64>,
}

/// One player's best play of a map.
struct MemberPlay {
    rp: f64,
    speed: Option<f64>,
    misses: Option<i64>,
    accuracy: Option<f64>,
    above: bool,
}

fn whole(n: f64) -> Option<i64> {
    (n.is_finite() && n.fract() == 0.0 && n.abs() < 1e12).then_some(n as i64)
}

/// A song key Atlas takes: short, plain characters.
fn song_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control)
}

/// The middle value (the lower of the two middle ones), as `median` in the window.
fn middle(mut v: Vec<f64>) -> Option<f64> {
    v.retain(|x| x.is_finite());
    v.sort_by(|a, b| a.total_cmp(b));
    (!v.is_empty()).then(|| v[(v.len() - 1) / 2])
}

fn around(list: &[&MemberPlay]) -> Option<Around> {
    Some(Around {
        players: list.len() as i64,
        rp: middle(list.iter().map(|p| p.rp).collect())?,
        speed: middle(list.iter().filter_map(|p| p.speed).collect()),
        misses: middle(list.iter().filter_map(|p| p.misses.map(|m| m as f64)).collect()),
        accuracy: middle(list.iter().filter_map(|p| p.accuracy).collect()),
    })
}

async fn build(net: &Net, me: i64, now: u64, step: &(dyn Fn(usize, usize) + Sync)) -> Result<Group, String> {
    // The profile, at most two leaderboard pages, then the players' top plays.
    let of = 3 + NEIGHBOURS;
    step(0, of);
    let position = crate::rhythia::rank(net, me).await?.position.ok_or("Rhythia has no rank for you yet. Play a ranked map first.")?;
    step(1, of);
    // The leaderboard page with the player, and the next one when they are near its edge.
    let page = (position - 1) / PER_PAGE + 1;
    let mut pages = vec![page];
    let at = (position - 1) % PER_PAGE;
    if at < NEIGHBOURS as i64 / 2 && page > 1 {
        pages.push(page - 1);
    }
    if at >= PER_PAGE - NEIGHBOURS as i64 / 2 {
        pages.push(page + 1);
    }
    let mut rows: Vec<(i64, i64)> = Vec::new(); // (id, position)
    let queries: Vec<BoardQuery> = pages.iter().map(|&page| BoardQuery { session: "", page, spin: false, vr: false }).collect();
    let boards = futures_util::future::join_all(queries.iter().map(|q| net.post_json::<_, BoardAnswer>(Host::RhythiaApi, "/api/getLeaderboard", q, &[]))).await;
    for board in boards {
        rows.extend(board?.leaderboard.into_iter().filter_map(|r| Some((r.id.and_then(whole).filter(|n| *n > 0)?, r.position.and_then(whole)?))));
    }
    step(3, of);
    rows.retain(|r| r.0 != me);
    rows.sort_by_key(|r| (r.1 - position).abs());
    rows.dedup_by_key(|r| r.0);
    rows.truncate(NEIGHBOURS);

    let mut plays: HashMap<String, (String, Vec<MemberPlay>)> = HashMap::new();
    let asks: Vec<_> = rows
        .iter()
        .map(|&(id, pos)| {
            let query = ScoresQuery { session: "", id, limit: 100 };
            let above = pos < position;
            Box::pin(async move { (above, net.post_json::<_, ScoresAnswer>(Host::RhythiaApi, "/api/getUserScores", &query, &[]).await) })
                as std::pin::Pin<Box<dyn std::future::Future<Output = (bool, Result<ScoresAnswer, String>)> + Send + '_>>
        })
        .collect();
    let mut asked = futures_util::stream::iter(asks).buffer_unordered(AT_ONCE);
    let mut done = 3;
    while let Some((above, answer)) = asked.next().await {
        done += 1;
        step(done.min(of), of);
        let Ok(scores) = answer else { continue };
        let mut best: HashMap<String, (MemberPlay, String)> = HashMap::new();
        for t in scores.top.into_iter().take(100).filter(|t| t.passed != Some(false)) {
            let (Some(song), Some(rp)) = (t.song_id, t.awarded_sp.filter(|r| r.is_finite() && (0.0..=100_000.0).contains(r))) else { continue };
            if !song_ok(&song) {
                continue;
            }
            let misses = t.misses.and_then(whole).filter(|m| *m >= 0);
            let notes = t.beatmap_notes.and_then(whole).filter(|n| *n > 0 && *n < 1_000_000);
            let play = MemberPlay { rp, speed: t.speed.filter(|s| s.is_finite() && (0.1..=10.0).contains(s)), misses, accuracy: accuracy(misses, notes), above };
            let title = clip(&t.beatmap_title.unwrap_or_default(), 300);
            match best.get(&song) {
                Some((b, _)) if b.rp >= rp => {}
                _ => {
                    best.insert(song, (play, title));
                }
            }
        }
        for (song, (play, title)) in best {
            plays.entry(song).or_insert_with(|| (title, Vec::new())).1.push(play);
        }
    }
    drop(asked);
    let mut plays: Vec<Play> = plays
        .into_iter()
        .filter_map(|(song, (title, list))| {
            let all: Vec<&MemberPlay> = list.iter().collect();
            let above: Vec<&MemberPlay> = list.iter().filter(|p| p.above).collect();
            let below: Vec<&MemberPlay> = list.iter().filter(|p| !p.above).collect();
            Some(Play { song, title, map: None, all: around(&all)?, above: around(&above), below: around(&below) })
        })
        .collect();
    plays.sort_by(|a, b| b.all.players.cmp(&a.all.players).then_with(|| a.title.cmp(&b.title)));
    let above = rows.iter().filter(|r| r.1 < position).count() as i64;
    Ok(Group { at: now, me, position, above, below: rows.len() as i64 - above, from: "rhythia".into(), plays })
}

/// Hits of all notes in percent, from the misses and the map's notes.
fn accuracy(misses: Option<i64>, notes: Option<i64>) -> Option<f64> {
    let (m, n) = (misses?, notes?);
    (m <= n).then(|| 100.0 * (n - m) as f64 / n as f64)
}

/// One map in the player's counted plays on Rhythia: its best RP, by song key and map hash.
#[derive(Serialize, Debug, PartialEq)]
pub struct TopPlay {
    pub rp: f64,
    pub song: Option<String>,
    pub hash: Option<String>,
}

/// The player's best plays on Rhythia (`getUserScores` top, at most 100; Rhythia's total RP is the 99 best,
/// the n-th ×0.97^(n−1)), the best per map first. Public data; nothing goes out but the player's number.
pub async fn my_top(net: &Net, id: i64) -> Result<Vec<TopPlay>, String> {
    let query = ScoresQuery { session: "", id, limit: 100 };
    let answer: ScoresAnswer = net.post_json(Host::RhythiaApi, "/api/getUserScores", &query, &[]).await?;
    Ok(top_plays(answer.top))
}

fn top_plays(raw: Vec<RawTop>) -> Vec<TopPlay> {
    let mut out: Vec<TopPlay> = Vec::new();
    for t in raw.into_iter().take(100).filter(|t| t.passed != Some(false)) {
        let Some(rp) = t.awarded_sp.filter(|r| r.is_finite() && (0.0..=100_000.0).contains(r)) else { continue };
        let song = t.song_id.filter(|s| song_ok(s));
        let hash = t.beatmap_hash.map(|h| h.to_ascii_lowercase()).filter(|h| crate::library::is_hash(h));
        if song.is_none() && hash.is_none() {
            continue;
        }
        let same = |p: &TopPlay| (song.is_some() && p.song == song) || (hash.is_some() && p.hash == hash);
        match out.iter_mut().find(|p| same(p)) {
            Some(p) if p.rp >= rp => {}
            Some(p) => p.rp = rp,
            None => out.push(TopPlay { rp, song, hash }),
        }
    }
    out.sort_by(|a, b| b.rp.total_cmp(&a.rp));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_plays_best_per_map_sorted() {
        let raw: Vec<RawTop> = serde_json::from_value(serde_json::json!([
            {"songId": "a", "beatmapHash": "AB".repeat(32), "awarded_sp": 200.0},
            {"songId": "b", "awarded_sp": 300.0},
            {"songId": "a", "awarded_sp": 250.0},
            {"songId": "c", "awarded_sp": 999.0, "passed": false},
            {"songId": "", "awarded_sp": 50.0},
            {"songId": "d", "awarded_sp": f64::NAN}
        ]))
        .unwrap();
        let top = top_plays(raw);
        assert_eq!(top.len(), 2);
        assert_eq!((top[0].rp, top[0].song.as_deref()), (300.0, Some("b")));
        assert_eq!((top[1].rp, top[1].hash.as_deref()), (250.0, Some("ab".repeat(32).as_str())));
    }

    #[test]
    fn song_keys_and_numbers() {
        assert!(song_ok("xaz_-_scenar10_-_speedcore_angel"));
        assert!(!song_ok(""));
        assert!(!song_ok("a\nb"));
        assert_eq!(whole(12.0), Some(12));
        assert_eq!(whole(1.5), None);
        assert_eq!(whole(f64::NAN), None);
        assert_eq!(accuracy(Some(27), Some(4077)), Some(100.0 * 4050.0 / 4077.0));
        assert_eq!(accuracy(Some(5), Some(4)), None);
        assert_eq!(accuracy(None, Some(4)), None);
        assert_eq!(middle(vec![3.0, 1.0, 2.0, 4.0]), Some(2.0));
        assert_eq!(middle(vec![]), None);
    }

    #[test]
    fn reads_the_server_answer() {
        let raw = r#"{"available":true,"rank":227,"rankObservedOn":"2026-11-21","band":{"above":120,"below":115,"from":113,"to":340},
          "maps":[{"key":"rom_-_call_me_maybe","id":12933,"hash":"95CC888CF608CE38FB5C163A40A3CF8FC7EB80B834AFA8525798BEA933E7E8FA","title":"CALL ME MAYBE",
            "mapper":"Rom","status":"RANKED","stars":7.14,"lengthMs":94113,"notes":1521,"plays":24008,
            "all":{"players":229,"rp":434,"acc":97.5,"speed":1.45,"misses":38.0},
            "above":{"players":118,"rp":446,"acc":97.7,"speed":1.45,"misses":35.0},"below":null},
           {"key":"","id":1,"all":{"players":9,"rp":1}},
           {"key":"no_players","id":2,"all":{"players":0,"rp":1}}]}"#;
        let g = from_server(serde_json::from_str(raw).unwrap(), 7, 100).unwrap();
        assert_eq!((g.position, g.above, g.below, g.from.as_str(), g.plays.len()), (227, 120, 115, "server", 1), "maps without a key or players are left out");
        let p = &g.plays[0];
        assert_eq!(p.all, Around { players: 229, rp: 434.0, speed: Some(1.45), misses: Some(38.0), accuracy: Some(97.5) });
        assert!(p.below.is_none());
        let m = p.map.as_ref().unwrap();
        assert_eq!((m.id, m.status.as_deref(), m.hash.as_deref().map(|h| h.starts_with("95cc")), m.cover), (12933, Some("RANKED"), Some(true), false));
        assert!(from_server(serde_json::from_str(r#"{"available":false,"reason":"not_ranked"}"#).unwrap(), 7, 100).is_none());
    }
}
