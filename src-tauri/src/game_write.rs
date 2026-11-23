//! The only place Atlas writes to the game's `rhythia.db`: an Atlas collection shown in the game
//! as a local game collection, and maps the player removes. Only while the game
//! is closed, and only after a copy of the database is saved in Atlas' `game-saves` folder (the last
//! five are kept). Putting a copy back (backup.rs) follows the same rules.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;

use crate::mine::Collection;

const KEEP_BACKUPS: usize = 5;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Written {
    pub game_id: i64,
    pub maps: usize,
    /// Maps of the collection the game does not have (they are left out).
    pub missing: usize,
}

/// Whether `rhythia.exe` is running, from Windows' own task list.
pub fn game_running() -> Result<bool, String> {
    let out = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq rhythia.exe", "/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|_| "Atlas could not check whether the game is running.")?;
    Ok(String::from_utf8_lossy(&out.stdout).to_ascii_lowercase().contains("\"rhythia.exe\""))
}

pub(crate) fn backup(db: &Path, dir: &Path, now: u64) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let copy = |ext: &str| -> std::io::Result<()> {
        let from = db.with_extension(ext);
        if from.is_file() {
            std::fs::copy(&from, dir.join(format!("rhythia-{now}.{ext}")))?;
        }
        Ok(())
    };
    copy("db").map_err(|e| format!("Atlas could not back up the game's database: {e}"))?;
    let _ = copy("db-wal");
    let mut olds: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "db"))
        .collect();
    olds.sort();
    for old in olds.iter().take(olds.len().saturating_sub(KEEP_BACKUPS)) {
        let _ = std::fs::remove_file(old);
        let _ = std::fs::remove_file(old.with_extension("db-wal"));
    }
    Ok(())
}

/// Writes `c` to the game: a new game collection, or the one it was written as before (its maps
/// replaced). Maps the game does not have are left out.
pub fn write(db: &Path, backups: &Path, c: &Collection, now: u64) -> Result<Written, String> {
    if game_running()? {
        return Err("Close Rhythia first.".into());
    }
    write_closed(db, backups, c, now)
}

fn write_closed(db: &Path, backups: &Path, c: &Collection, now: u64) -> Result<Written, String> {
    backup(db, backups, now)?;
    let mut conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|_| "The game's database could not be opened.")?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let has = |table: &str, cols: &[&str]| -> bool {
        let Ok(mut s) = tx.prepare(&format!("PRAGMA table_info({table})")) else { return false };
        let names: Vec<String> = s.query_map([], |r| r.get(1)).map(|r| r.flatten().collect()).unwrap_or_default();
        cols.iter().all(|c| names.iter().any(|n| n == c))
    };
    if !has("MapCollections", &["Id", "Name", "ColorIndex", "IsPinned"])
        || !has("MapMapCollection", &["FavouritesId", "MapsId"])
        || !has("Maps", &["Id", "OnlineId", "MapHash"])
    {
        return Err("This game version keeps collections differently. Atlas needs an update.".into());
    }
    let existing: Option<i64> = match c.game_id {
        Some(id) => tx.query_row("SELECT Id FROM MapCollections WHERE Id = ?1", [id], |r| r.get(0)).optional().map_err(|e| e.to_string())?,
        None => None,
    };
    let game_id = match existing {
        Some(id) => {
            tx.execute("UPDATE MapCollections SET Name = ?1 WHERE Id = ?2", params![c.name, id]).map_err(|e| e.to_string())?;
            tx.execute("DELETE FROM MapMapCollection WHERE FavouritesId = ?1", [id]).map_err(|e| e.to_string())?;
            id
        }
        None => {
            tx.execute("INSERT INTO MapCollections (Name, ColorIndex, IsPinned) VALUES (?1, 0, 0)", [&c.name]).map_err(|e| e.to_string())?;
            tx.last_insert_rowid()
        }
    };
    let (mut maps, mut missing) = (0, 0);
    for item in &c.maps {
        let local: Option<i64> = tx
            .query_row(
                "SELECT Id FROM Maps WHERE (?1 IS NOT NULL AND OnlineId = ?1) OR (?2 IS NOT NULL AND lower(MapHash) = ?2) LIMIT 1",
                params![item.online_id, item.hash],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        match local {
            Some(map) => {
                maps += tx
                    .execute("INSERT OR IGNORE INTO MapMapCollection (FavouritesId, MapsId) VALUES (?1, ?2)", params![game_id, map])
                    .map_err(|e| e.to_string())?;
            }
            None => missing += 1,
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(Written { game_id, maps, missing })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Removed {
    pub maps: usize,
    pub files: usize,
}

/// Removes maps (local `Maps.Id`) from the game: their rows, their places in the game's collections
/// and their files in `cache/maps`, plus songs and covers no other map uses. Attempts and replays
/// stay (`Scores` names maps by their online id, not by row).
pub fn uninstall(game: &crate::game::GameData, backups: &Path, ids: &[i64], now: u64) -> Result<Removed, String> {
    if game_running()? {
        return Err("Close Rhythia first. Atlas removes maps only while the game is closed.".into());
    }
    uninstall_closed(game, backups, ids, now)
}

fn uninstall_closed(game: &crate::game::GameData, backups: &Path, ids: &[i64], now: u64) -> Result<Removed, String> {
    let db = game.database();
    backup(&db, backups, now)?;
    let mut conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|_| "The game's database could not be opened.")?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let mut files: Vec<(&str, String)> = Vec::new();
    let mut maps = 0;
    for &id in ids {
        let row: Option<[Option<String>; 3]> = tx
            .query_row("SELECT Path, AudioPath, ImagePath FROM Maps WHERE Id = ?1", [id], |r| Ok([r.get(0)?, r.get(1)?, r.get(2)?]))
            .optional()
            .map_err(|_| "This game version keeps maps differently. Atlas needs an update.")?;
        let Some(row) = row else { continue };
        tx.execute("DELETE FROM MapMapCollection WHERE MapsId = ?1", [id]).map_err(|e| e.to_string())?;
        maps += tx.execute("DELETE FROM Maps WHERE Id = ?1", [id]).map_err(|e| e.to_string())?;
        for (kind, path) in ["maps", "audio", "covers"].into_iter().zip(row) {
            if let Some(name) = path.and_then(|p| p.strip_prefix(&format!("/cache/{kind}/")).map(str::to_string)) {
                files.push((kind, name));
            }
        }
    }
    // A song or cover another map still uses stays.
    let mut gone = Vec::new();
    for (kind, name) in files {
        let column = match kind { "maps" => "Path", "audio" => "AudioPath", _ => "ImagePath" };
        let used: bool = tx
            .query_row(&format!("SELECT EXISTS(SELECT 1 FROM Maps WHERE {column} = ?1)"), [format!("/cache/{kind}/{name}")], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if !used {
            gone.push(format!("cache/{kind}/{name}"));
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    let mut removed = 0;
    for rel in gone {
        let plain = rel.rsplit('/').next().is_some_and(|n| {
            let stem = n.strip_suffix(".rhc").or_else(|| n.strip_suffix(".sspm")).unwrap_or(n);
            crate::library::is_hash(stem)
        });
        if plain && game.file(&rel).is_ok_and(|p| std::fs::remove_file(p).is_ok()) {
            removed += 1;
        }
    }
    Ok(Removed { maps, files: removed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mine::Item;

    #[test]
    fn writes_and_rewrites() {
        let dir = std::env::temp_dir().join(format!("atlas-gw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("rhythia.db");
        let c = Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE Maps (Id INTEGER PRIMARY KEY, OnlineId INTEGER, MapHash TEXT);
             CREATE TABLE MapCollections (Id INTEGER PRIMARY KEY AUTOINCREMENT, Name TEXT, ColorIndex INTEGER, IsPinned INTEGER);
             CREATE TABLE MapMapCollection (FavouritesId INTEGER, MapsId INTEGER, PRIMARY KEY (FavouritesId, MapsId));
             INSERT INTO Maps VALUES (1, 100, NULL), (2, NULL, 'ABAB');",
        )
        .unwrap();
        drop(c);
        let item = |id: Option<i64>, hash: Option<&str>| Item { online_id: id, hash: hash.map(String::from), title: String::new(), mapper: String::new() };
        let mut col = Collection { id: 1, name: "Mine".into(), created: 0, maps: vec![item(Some(100), None), item(None, Some("abab")), item(Some(7), None)], game_id: None, want_game: false };
        let w = write_closed(&db, &dir.join("b"), &col, 1).unwrap();
        assert_eq!((w.maps, w.missing), (2, 1));
        col.game_id = Some(w.game_id);
        col.maps.truncate(1);
        col.name = "Renamed".into();
        let w2 = write_closed(&db, &dir.join("b"), &col, 2).unwrap();
        assert_eq!((w2.game_id, w2.maps), (w.game_id, 1));
        let c = Connection::open(&db).unwrap();
        let n: i64 = c.query_row("SELECT COUNT(*) FROM MapMapCollection", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        assert!(dir.join("b").join("rhythia-1.db").is_file());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn uninstall_keeps_shared_files_and_scores() {
        let dir = std::env::temp_dir().join(format!("atlas-un-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for k in ["maps", "audio", "covers"] {
            std::fs::create_dir_all(dir.join("cache").join(k)).unwrap();
        }
        let (a, b, song) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        for f in [format!("maps/{a}.rhc"), format!("maps/{b}.rhc"), format!("audio/{song}"), format!("covers/{a}")] {
            std::fs::write(dir.join("cache").join(f), b"x").unwrap();
        }
        let db = dir.join("rhythia.db");
        let c = Connection::open(&db).unwrap();
        c.execute_batch(&format!(
            "CREATE TABLE Maps (Id INTEGER PRIMARY KEY, Path TEXT, AudioPath TEXT, ImagePath TEXT);
             CREATE TABLE MapMapCollection (FavouritesId INTEGER, MapsId INTEGER);
             CREATE TABLE Scores (Id INTEGER PRIMARY KEY, MapId INTEGER);
             INSERT INTO Maps VALUES (1, '/cache/maps/{a}.rhc', '/cache/audio/{song}', '/cache/covers/{a}'), (2, '/cache/maps/{b}.rhc', '/cache/audio/{song}', NULL);
             INSERT INTO MapMapCollection VALUES (9, 1), (9, 2);
             INSERT INTO Scores VALUES (1, 100);"
        ))
        .unwrap();
        drop(c);
        let game = crate::game::GameData::at(dir.clone());
        let r = uninstall_closed(&game, &dir.join("b"), &[1, 77], 5).unwrap();
        assert_eq!((r.maps, r.files), (1, 2), "the map file and its own cover go; the shared song stays");
        assert!(dir.join("cache/audio").join(&song).is_file());
        assert!(!dir.join("cache/maps").join(format!("{a}.rhc")).exists());
        let c = Connection::open(&db).unwrap();
        let (links, scores): (i64, i64) = c.query_row("SELECT (SELECT COUNT(*) FROM MapMapCollection), (SELECT COUNT(*) FROM Scores)", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((links, scores), (1, 1));
        let _ = std::fs::remove_dir_all(dir);
    }
}
