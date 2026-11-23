//! Reads the game's `rhythia.db` read-only. The game changes its schema with updates, so every
//! column is checked before use, and every value is checked for type and size: the database is
//! data from outside like any other.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, Row};
use serde::Serialize;

const MAX_MAPS: usize = 100_000;
const MAX_TITLE: usize = 300;
const MAX_MAPPERS: usize = 20;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryMap {
    pub id: i64,
    pub online_id: Option<i64>,
    pub hash: Option<String>,
    pub status: Option<String>,
    pub title: String,
    pub mappers: Vec<String>,
    pub difficulty_name: Option<String>,
    pub stars: Option<f64>,
    pub duration_ms: Option<i64>,
    pub notes: Option<i64>,
    pub plays: i64,
    pub passes: i64,
    pub best_accuracy: Option<f64>,
    /// Most RP from a passed attempt.
    pub best_rp: Option<f64>,
    pub last_played: Option<String>,
    /// File name (sha256) of the cover in the game's `cache/covers`.
    pub cover: Option<String>,
    /// File name (sha256) of the song in the game's `cache/audio`.
    pub audio: Option<String>,
    /// Where the map's own preview starts, in ms (0 or none: not set).
    pub preview_ms: Option<i64>,
    /// File name of the map in the game's `cache/maps` (`<sha256>.rhc`).
    pub file: Option<String>,
    /// The map's song key (`LegacyId`, Rhythia's `songId`): how players' scores name the map.
    pub legacy_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Library {
    pub maps: Vec<LibraryMap>,
    pub attempts: i64,
}

fn open(db: &Path) -> Result<Connection, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let c = Connection::open_with_flags(db, flags).map_err(|_| "The game's database could not be opened.")?;
    c.busy_timeout(Duration::from_secs(3)).map_err(|e| e.to_string())?;
    c.pragma_update(None, "query_only", true).map_err(|e| e.to_string())?;
    Ok(c)
}

fn columns(c: &Connection, table: &str) -> HashSet<String> {
    let Ok(mut stmt) = c.prepare(&format!("PRAGMA table_info({table})")) else { return HashSet::new() };
    stmt.query_map([], |r| r.get::<_, String>(1))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

pub fn read(db: &Path) -> Result<Library, String> {
    let c = open(db)?;
    let maps = columns(&c, "Maps");
    if !maps.contains("Id") || !maps.contains("Title") {
        return Err("This game version stores maps differently. Atlas needs an update.".into());
    }
    // Names come only from these fixed lists, never from the database or the player.
    let col = |name: &str| if maps.contains(name) { format!("m.{name}") } else { "NULL".into() };
    let scores = columns(&c, "Scores");
    let with_scores = ["MapId", "Passed", "Accuracy", "Points", "CreatedAt"].iter().all(|n| scores.contains(*n));
    let join = if with_scores && maps.contains("OnlineId") {
        "LEFT JOIN (SELECT MapId, COUNT(*) plays, SUM(Passed = 1) passes, \
         MAX(CASE WHEN Passed = 1 THEN Accuracy END) best, MAX(CASE WHEN Passed = 1 THEN Points END) rp,          MAX(CreatedAt) last \
         FROM Scores WHERE MapId IS NOT NULL GROUP BY MapId) s ON s.MapId = m.OnlineId"
    } else {
        "LEFT JOIN (SELECT NULL MapId, NULL plays, NULL passes, NULL best, NULL rp, NULL last) s ON 0"
    };
    let sql = format!(
        "SELECT m.Id, {}, {}, {}, m.Title, {}, {}, {}, {}, {}, s.plays, s.passes, s.best, s.last, s.rp, {}, {}, {}, {}, {} FROM Maps m {join} LIMIT {MAX_MAPS}",
        col("OnlineId"),
        col("MapHash"),
        col("OnlineStatus"),
        col("MappersJson"),
        col("DifficultyName"),
        col("StarRating"),
        col("Duration"),
        col("NoteCount"),
        col("ImagePath"),
        col("AudioPath"),
        col("PreviewTime"),
        col("Path"),
        col("LegacyId"),
    );
    let busy = |_| "The game's database is busy. Try again in a moment.".to_string();
    let mut stmt = c.prepare(&sql).map_err(busy)?;
    let list = stmt
        .query_map([], |r| Ok(map_row(r)))
        .map_err(busy)?
        .flatten()
        .flatten()
        .collect();
    let attempts = if scores.contains("Id") {
        c.query_row("SELECT COUNT(*) FROM Scores", [], |r| Ok(int(r, 0).unwrap_or(0))).unwrap_or(0)
    } else {
        0
    };
    Ok(Library { maps: list, attempts })
}

/// `OnlineId` of every map in the game, to skip maps the player already has.
pub fn online_ids(db: &Path) -> Result<HashSet<i64>, String> {
    let c = open(db)?;
    if !columns(&c, "Maps").contains("OnlineId") {
        return Ok(HashSet::new());
    }
    let mut stmt = c.prepare("SELECT OnlineId FROM Maps WHERE OnlineId IS NOT NULL").map_err(|e| e.to_string())?;
    let ids = stmt.query_map([], |r| Ok(int(r, 0))).map_err(|e| e.to_string())?.flatten().flatten().collect();
    Ok(ids)
}

fn map_row(r: &Row) -> Option<LibraryMap> {
    Some(LibraryMap {
        id: int(r, 0)?,
        online_id: int(r, 1).filter(|n| *n > 0),
        hash: text(r, 2, 64).map(|h| h.to_ascii_lowercase()).filter(|h| is_hash(h)),
        status: text(r, 3, 16).filter(|s| matches!(s.as_str(), "RANKED" | "APPROVED" | "QUALIFIED" | "UNRANKED")),
        title: text(r, 4, MAX_TITLE).unwrap_or_default(),
        mappers: text(r, 5, 4096)
            .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
            .map(|v| v.into_iter().take(MAX_MAPPERS).map(|s| clip(&s, 100)).collect())
            .unwrap_or_default(),
        difficulty_name: text(r, 6, 100),
        stars: real(r, 7).filter(|s| (0.0..=100.0).contains(s)),
        duration_ms: int(r, 8).filter(|n| *n >= 0),
        notes: int(r, 9).filter(|n| *n >= 0),
        plays: int(r, 10).unwrap_or(0).max(0),
        passes: int(r, 11).unwrap_or(0).max(0),
        best_accuracy: real(r, 12).filter(|a| (0.0..=100.0).contains(a)),
        last_played: text(r, 13, 40),
        best_rp: real(r, 14).filter(|n| (0.0..=100_000.0).contains(n)),
        cover: text(r, 15, 200).and_then(|p| p.strip_prefix("/cache/covers/").map(str::to_ascii_lowercase)).filter(|h| is_hash(h)),
        audio: text(r, 16, 200).and_then(|p| p.strip_prefix("/cache/audio/").map(str::to_ascii_lowercase)).filter(|h| is_hash(h)),
        preview_ms: real(r, 17).filter(|n| n.is_finite() && *n > 0.0 && *n < 3_600_000.0).map(|n| n as i64),
        file: text(r, 18, 200)
            .and_then(|p| p.strip_prefix("/cache/maps/").map(str::to_ascii_lowercase))
            .filter(|n| n.strip_suffix(".rhc").is_some_and(is_hash)),
        legacy_id: text(r, 19, 200).filter(|s| !s.is_empty()),
    })
}

const MAX_ATTEMPTS: usize = 1000;

/// One attempt at a map, passed or failed.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    pub at: Option<String>,
    pub passed: bool,
    pub accuracy: Option<f64>,
    pub rp: Option<f64>,
    pub score: Option<i64>,
    pub misses: Option<i64>,
    pub hits: Option<i64>,
    /// Where a failed attempt ended, in ms.
    pub fail_ms: Option<i64>,
    pub speed: Option<f64>,
    pub mods: Vec<String>,
}

/// Every attempt at one map, newest first: by its online id, or by its hash for local maps.
pub fn attempts(db: &Path, online_id: Option<i64>, hash: Option<&str>) -> Result<Vec<Attempt>, String> {
    let c = open(db)?;
    let scores = columns(&c, "Scores");
    let col = |name: &str| if scores.contains(name) { name.to_string() } else { "NULL".into() };
    let hash = hash.filter(|h| is_hash(h)).map(str::to_ascii_lowercase);
    let (filter, key): (&str, rusqlite::types::Value) = match (online_id.filter(|n| *n > 0), hash) {
        (Some(id), _) if scores.contains("MapId") => ("MapId = ?1", id.into()),
        (_, Some(h)) if scores.contains("BeatmapHash") => ("lower(BeatmapHash) = ?1", h.into()),
        _ => return Ok(Vec::new()),
    };
    let sql = format!(
        "SELECT {}, {}, {}, {}, {}, {}, {}, {}, {}, {} FROM Scores WHERE {filter} ORDER BY {} DESC LIMIT {MAX_ATTEMPTS}",
        col("CreatedAt"), col("Passed"), col("Accuracy"), col("Points"), col("TotalScore"),
        col("Misses"), col("Hits"), col("FailTime"), col("Speed"), col("Mods"),
        if scores.contains("CreatedAt") { "CreatedAt" } else { "rowid" },
    );
    let busy = |_| "The game's database is busy. Try again in a moment.".to_string();
    let mut stmt = c.prepare(&sql).map_err(busy)?;
    let rows = stmt
        .query_map([key], |r| {
            Ok(Attempt {
                at: text(r, 0, 40),
                passed: int(r, 1) == Some(1),
                accuracy: real(r, 2).filter(|a| (0.0..=100.0).contains(a)),
                rp: real(r, 3).filter(|n| (0.0..=100_000.0).contains(n)),
                score: int(r, 4).filter(|n| *n >= 0),
                misses: int(r, 5).filter(|n| *n >= 0),
                hits: int(r, 6).filter(|n| *n >= 0),
                fail_ms: int(r, 7).filter(|n| *n > 0),
                speed: real(r, 8).filter(|n| (0.1..=10.0).contains(n)),
                mods: text(r, 9, 2000)
                    .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
                    .map(|v| v.into_iter().take(20).map(|m| clip(&m, 40)).collect())
                    .unwrap_or_default(),
            })
        })
        .map_err(busy)?
        .flatten()
        .collect();
    Ok(rows)
}

/// A player's best pass of one map, for Reimagined (over the bridge). Every map counts once, however
/// often it was played: what the player can do, not how much they tried.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BestPass {
    /// The map's id on Rhythia.
    pub id: i64,
    pub hash: Option<String>,
    pub rp: Option<f64>,
    pub accuracy: Option<f64>,
    pub misses: Option<i64>,
    pub at: Option<String>,
    pub speed: Option<f64>,
    pub mods: Vec<String>,
    /// How many times the map was passed.
    pub passes: i64,
    pub stars: Option<f64>,
    pub status: Option<String>,
}

/// The best pass (most RP, then best accuracy) of every map `player` passed (every player in this game
/// when `None`). Only whole passes: failed attempts, practice started part way in (`StartFrom`) and spin
/// mode are left out.
pub fn best_passes(db: &Path, player: Option<&str>) -> Result<Vec<BestPass>, String> {
    let c = open(db)?;
    let scores = columns(&c, "Scores");
    if !["MapId", "Passed", "Points", "Accuracy"].iter().all(|n| scores.contains(*n)) || (player.is_some() && !scores.contains("PlayerName")) {
        return Ok(Vec::new());
    }
    let maps = columns(&c, "Maps");
    // Names come only from these fixed lists, never from the database or the player.
    let sc = |name: &str| if scores.contains(name) { name.to_string() } else { format!("NULL AS {name}") };
    let mc = |name: &str| if maps.contains("OnlineId") && maps.contains(name) { format!("m.{name}") } else { "NULL".into() };
    let mut whole = String::new();
    if player.is_some() {
        whole.push_str(" AND lower(PlayerName) = lower(?1)");
    } else {
        whole.push_str(" AND ?1 IS NULL");
    }
    if scores.contains("StartFrom") {
        whole.push_str(" AND COALESCE(StartFrom, 0) = 0");
    }
    if scores.contains("Spin") {
        whole.push_str(" AND COALESCE(Spin, 0) = 0");
    }
    let join = if maps.contains("OnlineId") { "LEFT JOIN Maps m ON m.OnlineId = s.MapId" } else { "LEFT JOIN (SELECT NULL) m ON 0" };
    let sql = format!(
        "SELECT s.MapId, {}, s.Points, s.Accuracy, s.Misses, s.CreatedAt, s.Speed, s.Mods, s.n, {}, {} FROM (
           SELECT MapId, Points, Accuracy, {}, {}, {}, {},
             COUNT(*) OVER (PARTITION BY MapId) n,
             ROW_NUMBER() OVER (PARTITION BY MapId ORDER BY Points DESC, Accuracy DESC) k
           FROM Scores WHERE Passed = 1 AND MapId IS NOT NULL{whole}
         ) s {join} WHERE s.k = 1 LIMIT {MAX_MAPS}",
        mc("MapHash"),
        mc("StarRating"),
        mc("OnlineStatus"),
        sc("Misses"),
        sc("CreatedAt"),
        sc("Speed"),
        sc("Mods"),
    );
    let busy = |_| "The game's database is busy. Try again in a moment.".to_string();
    let mut stmt = c.prepare(&sql).map_err(busy)?;
    let rows = stmt
        .query_map([player.map(|p| clip(p, 100))], |r| {
            Ok(int(r, 0).filter(|n| *n > 0).map(|id| BestPass {
                id,
                hash: text(r, 1, 64).map(|h| h.to_ascii_lowercase()).filter(|h| is_hash(h)),
                rp: real(r, 2).filter(|n| (0.0..=100_000.0).contains(n)),
                accuracy: real(r, 3).filter(|a| (0.0..=100.0).contains(a)),
                misses: int(r, 4).filter(|n| *n >= 0),
                at: text(r, 5, 40),
                speed: real(r, 6).filter(|n| (0.1..=10.0).contains(n)),
                mods: text(r, 7, 2000)
                    .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
                    .map(|v| v.into_iter().take(20).map(|m| clip(&m, 40)).collect())
                    .unwrap_or_default(),
                passes: int(r, 8).unwrap_or(1).max(1),
                stars: real(r, 9).filter(|s| (0.0..=100.0).contains(s)),
                status: text(r, 10, 16).filter(|s| matches!(s.as_str(), "RANKED" | "APPROVED" | "QUALIFIED" | "UNRANKED")),
            }))
        })
        .map_err(busy)?
        .flatten()
        .flatten()
        .collect();
    Ok(rows)
}

/// Days kept for Your game.
const PICTURE_DAYS: u32 = 400;

/// Tries and passes of one map on one day.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayPlays {
    /// `YYYY-MM-DD`, as the game writes it.
    pub day: String,
    /// The map's id on Rhythia (none for local maps).
    pub map: Option<i64>,
    pub tries: i64,
    pub passes: i64,
}

/// Tries and passes per day and map over the last `PICTURE_DAYS` days, oldest first.
pub fn days(db: &Path) -> Result<Vec<DayPlays>, String> {
    let c = open(db)?;
    let scores = columns(&c, "Scores");
    if !["MapId", "Passed", "CreatedAt"].iter().all(|n| scores.contains(*n)) {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT substr(CreatedAt, 1, 10) d, MapId, COUNT(*), SUM(Passed = 1) FROM Scores \
         WHERE CreatedAt >= date('now', '-{PICTURE_DAYS} days') GROUP BY d, MapId ORDER BY d LIMIT 100000"
    );
    let busy = |_| "The game's database is busy. Try again in a moment.".to_string();
    let mut stmt = c.prepare(&sql).map_err(busy)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(text(r, 0, 10).filter(|d| d.len() == 10 && d.as_bytes()[4] == b'-').map(|day| DayPlays {
                day,
                map: int(r, 1).filter(|n| *n > 0),
                tries: int(r, 2).unwrap_or(0).max(0),
                passes: int(r, 3).unwrap_or(0).max(0),
            }))
        })
        .map_err(busy)?
        .flatten()
        .flatten()
        .collect();
    Ok(rows)
}

/// Tries and passes since `secs` (Unix seconds; the game writes `CreatedAt` in UTC).
pub fn tries_since(db: &Path, secs: u64) -> Result<(i64, i64), String> {
    let c = open(db)?;
    let scores = columns(&c, "Scores");
    if !scores.contains("Passed") || !scores.contains("CreatedAt") {
        return Ok((0, 0));
    }
    c.query_row(
        "SELECT COUNT(*), COALESCE(SUM(Passed = 1), 0) FROM Scores WHERE CreatedAt >= datetime(?1, 'unixepoch')",
        [secs as i64],
        |r| Ok((int(r, 0).unwrap_or(0), int(r, 1).unwrap_or(0))),
    )
    .map_err(|e| e.to_string())
}

/// The newest attempt's `Scores.Id` (0 when there are none), to notice new attempts.
pub fn newest_try(db: &Path) -> Result<i64, String> {
    let c = open(db)?;
    if !columns(&c, "Scores").contains("Id") {
        return Ok(0);
    }
    c.query_row("SELECT COALESCE(MAX(Id), 0) FROM Scores", [], |r| Ok(int(r, 0).unwrap_or(0))).map_err(|e| e.to_string())
}

/// An attempt that came in while Atlas was open.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewTry {
    pub online_id: Option<i64>,
    pub title: String,
    pub passed: bool,
    pub accuracy: Option<f64>,
    pub rp: Option<f64>,
}

/// Attempts after `after` (`Scores.Id`), oldest first, at most 20.
pub fn tries_after(db: &Path, after: i64) -> Result<Vec<NewTry>, String> {
    let c = open(db)?;
    let scores = columns(&c, "Scores");
    if !["Id", "MapId", "Passed", "Accuracy", "Points"].iter().all(|n| scores.contains(*n)) {
        return Ok(Vec::new());
    }
    let join = if columns(&c, "Maps").contains("OnlineId") { "LEFT JOIN Maps m ON m.OnlineId = s.MapId" } else { "LEFT JOIN (SELECT NULL Title) m ON 0" };
    let sql = format!("SELECT s.MapId, m.Title, s.Passed, s.Accuracy, s.Points FROM Scores s {join} WHERE s.Id > ?1 ORDER BY s.Id LIMIT 20");
    let mut stmt = c.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([after], |r| {
            Ok(NewTry {
                online_id: int(r, 0).filter(|n| *n > 0),
                title: text(r, 1, MAX_TITLE).unwrap_or_default(),
                passed: int(r, 2) == Some(1),
                accuracy: real(r, 3).filter(|a| (0.0..=100.0).contains(a)),
                rp: real(r, 4).filter(|n| (0.0..=100_000.0).contains(n)),
            })
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    Ok(rows)
}

/// A collection the player made in the game.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameCollection {
    pub id: i64,
    pub name: String,
    /// Local `Maps.Id` of its maps.
    pub maps: Vec<i64>,
}

pub fn game_collections(db: &Path) -> Result<Vec<GameCollection>, String> {
    let c = open(db)?;
    let (cols, links) = (columns(&c, "MapCollections"), columns(&c, "MapMapCollection"));
    if !cols.contains("Id") || !cols.contains("Name") || !links.contains("FavouritesId") || !links.contains("MapsId") {
        return Ok(Vec::new());
    }
    let mut out: Vec<GameCollection> = c
        .prepare("SELECT Id, Name FROM MapCollections ORDER BY Id LIMIT 1000")
        .and_then(|mut s| {
            s.query_map([], |r| Ok(int(r, 0).map(|id| GameCollection { id, name: text(r, 1, 200).unwrap_or_default(), maps: Vec::new() })))
                .map(|rows| rows.flatten().flatten().collect())
        })
        .map_err(|e| e.to_string())?;
    let mut stmt = c.prepare("SELECT FavouritesId, MapsId FROM MapMapCollection LIMIT 1000000").map_err(|e| e.to_string())?;
    let pairs = stmt.query_map([], |r| Ok((int(r, 0), int(r, 1)))).map_err(|e| e.to_string())?;
    for (col, map) in pairs.flatten() {
        if let (Some(col), Some(map)) = (col, map) {
            if let Some(g) = out.iter_mut().find(|g| g.id == col) {
                g.maps.push(map);
            }
        }
    }
    Ok(out)
}

pub fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn int(r: &Row, i: usize) -> Option<i64> {
    match r.get_ref(i).ok()? {
        ValueRef::Integer(n) => Some(n),
        ValueRef::Real(f) if f.is_finite() => Some(f as i64),
        ValueRef::Text(t) => std::str::from_utf8(t).ok()?.trim().parse().ok(),
        _ => None,
    }
}

fn real(r: &Row, i: usize) -> Option<f64> {
    match r.get_ref(i).ok()? {
        ValueRef::Integer(n) => Some(n as f64),
        ValueRef::Real(f) => Some(f).filter(|f| f.is_finite()),
        ValueRef::Text(t) => std::str::from_utf8(t).ok()?.trim().parse().ok().filter(|f: &f64| f.is_finite()),
        _ => None,
    }
}

fn text(r: &Row, i: usize, max: usize) -> Option<String> {
    match r.get_ref(i).ok()? {
        ValueRef::Text(t) => Some(clip(&String::from_utf8_lossy(t), max)).filter(|s| !s.is_empty()),
        _ => None,
    }
}

pub fn clip(s: &str, max: usize) -> String {
    s.trim().chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_passes_only_whole_passes() {
        let dir = std::env::temp_dir().join(format!("atlas-lib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("rhythia.db");
        let c = Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE Maps (Id INTEGER PRIMARY KEY, OnlineId INTEGER, MapHash TEXT, StarRating REAL, OnlineStatus TEXT, Title TEXT);
             CREATE TABLE Scores (Id INTEGER PRIMARY KEY, CreatedAt TEXT, PlayerName TEXT, MapId INTEGER, Accuracy REAL, Misses INTEGER,
               Points REAL, Mods TEXT, Passed INTEGER, Speed REAL, Spin INTEGER, StartFrom INTEGER);
             INSERT INTO Maps VALUES (1, 10, NULL, 4.5, 'RANKED', 'a'), (2, 20, NULL, 2.0, 'UNRANKED', 'b');
             INSERT INTO Scores VALUES
               (1, '2026-01-01', 'Me', 10, 97.0, 9, 300, '[]', 1, 1.0, 0, 0),
               (2, '2026-01-02', 'Me', 10, 99.0, 2, 320, '[]', 1, 1.0, 0, 0),
               (3, '2026-01-03', 'Me', 10, 99.9, 0, 900, '[]', 0, 1.0, 0, 0),
               (4, '2026-01-04', 'Me', 10, 99.9, 0, 999, '[]', 1, 1.0, 0, 30000),
               (5, '2026-01-05', 'Me', 20, 95.0, 4, NULL, '[\"mod_nofail\"]', 1, 1.25, 0, 0),
               (6, '2026-01-06', 'Me', 30, 99.0, 1, 500, '[]', 1, 1.0, 1, 0),
               (7, '2026-01-07', 'Other', 20, 99.5, 0, 50, '[]', 1, 1.0, 0, 0);",
        )
        .unwrap();
        drop(c);
        assert_eq!(best_passes(&db, None).unwrap().len(), 2, "every player: maps 10 and 20 (30 is spin)");
        let mut got = best_passes(&db, Some("me")).unwrap();
        got.sort_by_key(|b| b.id);
        assert_eq!(got.iter().map(|b| b.id).collect::<Vec<_>>(), vec![10, 20], "fails, spin and other players left out");
        assert_eq!((got[0].rp, got[0].passes), (Some(320.0), 2), "best whole pass; practice from the middle does not count");
        assert_eq!((got[1].accuracy, got[1].mods.clone(), got[1].status.as_deref()), (Some(95.0), vec!["mod_nofail".to_string()], Some("UNRANKED")));
        assert_eq!(newest_try(&db).unwrap(), 7);
        assert_eq!(tries_since(&db, 1_767_312_000).unwrap(), (5, 4), "from 2026-01-02 00:00 UTC");
        let new = tries_after(&db, 5).unwrap();
        assert_eq!(new.iter().map(|t| (t.online_id, t.title.as_str(), t.passed)).collect::<Vec<_>>(), vec![(Some(30), "", true), (Some(20), "b", true)]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
