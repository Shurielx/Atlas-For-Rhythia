//! Copies of the player's maps and replays (Settings → Data). The game is only read: its database
//! through a read-only connection (`VACUUM INTO` a new file), its map, song and cover files by
//! plain copy. Copies are made only while the game is closed, so the game never waits for Atlas.
//!
//! The copy folder holds:
//! - `files/maps|audio|covers/<name>`: each game file once (its name is its hash), added as new maps
//!   come; files no kept copy needs any more are deleted;
//! - `rhythia-<time>.zip`: the game's database (maps, collections, every attempt, `rhythia.db`,
//!   compressed) with replays kept as chosen (all, passes only, the best pass of each map, none), and
//!   `files.txt`, the map files that copy needs; the newest few are kept. Copies from before 0.4
//!   (`rhythia-<time>.db`, not compressed) are zipped at the next copy.
//!
//! Map files stay outside the ZIPs, once each: they are compressed already, and one ZIP per copy would
//! hold every map again.
//!
//! Restoring (game closed) puts a database copy back after saving the current one, then copies back
//! the map files that copy needs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::game::GameData;
use crate::library::is_hash;

const GB: u64 = 1024 * 1024 * 1024;
/// Left free on the disk after a copy, whatever the copy needs.
const KEEP_FREE: u64 = 2 * GB;
const KINDS: [&str; 3] = ["maps", "audio", "covers"];
/// Past this much game data, the first copy is offered instead of made unasked.
pub const NUDGE_BYTES: u64 = GB;

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// None: `Backups` in Atlas' data folder.
    pub dir: Option<PathBuf>,
    /// A copy every this many days; 0: only by hand.
    pub every_days: u32,
    /// "all", "ranked", "unranked" or "none".
    pub maps: String,
    /// "all", "passes", "best" or "none".
    pub replays: String,
    /// Database copies kept.
    pub keep: u32,
    /// The copy folder never grows past this.
    pub max_gb: u32,
    /// A copy after each finished download.
    pub after_download: bool,
    /// Offer a first copy when the game's maps and replays pass 1 GB.
    pub nudge: bool,
    pub last_at: u64,
    pub last_error: Option<String>,
    /// Asked for while the game was open: made once it closes.
    pub pending: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            dir: None,
            every_days: 7,
            maps: "all".into(),
            replays: "all".into(),
            keep: 3,
            max_gb: 10,
            after_download: false,
            nudge: true,
            last_at: 0,
            last_error: None,
            pending: false,
        }
    }
}

impl Settings {
    fn checked(mut self) -> Settings {
        if !["all", "ranked", "unranked", "none"].contains(&self.maps.as_str()) {
            self.maps = "all".into();
        }
        if !["all", "passes", "best", "none"].contains(&self.replays.as_str()) {
            self.replays = "all".into();
        }
        self.every_days = self.every_days.min(90);
        self.keep = self.keep.clamp(1, 20);
        self.max_gb = self.max_gb.clamp(1, 2000);
        self
    }
}

pub struct Backup {
    file: PathBuf,
    default_dir: PathBuf,
    settings: Mutex<Settings>,
    running: AtomicBool,
}

impl Backup {
    pub fn load(file: PathBuf, default_dir: PathBuf) -> Self {
        let settings = crate::store::load::<Settings>(&file).unwrap_or_default().checked();
        Backup { file, default_dir, settings: Mutex::new(settings), running: AtomicBool::new(false) }
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    pub fn dir(&self) -> PathBuf {
        self.settings().dir.unwrap_or_else(|| self.default_dir.clone())
    }

    /// The player's choices; times and errors stay as Atlas has them.
    pub fn set(&self, new: Settings) -> Result<Settings, String> {
        let mut s = self.settings.lock().unwrap();
        let keep = (s.last_at, s.last_error.clone(), s.pending);
        *s = new.checked();
        (s.last_at, s.last_error, s.pending) = keep;
        crate::store::save(&self.file, &*s)?;
        Ok(s.clone())
    }

    pub fn set_pending(&self, on: bool) {
        let mut s = self.settings.lock().unwrap();
        s.pending = on;
        let _ = crate::store::save(&self.file, &*s);
    }

    fn finished(&self, at: u64, error: Option<String>) {
        let mut s = self.settings.lock().unwrap();
        if error.is_none() {
            s.last_at = at;
        }
        s.last_error = error;
        s.pending = false;
        let _ = crate::store::save(&self.file, &*s);
    }

    /// A copy is due: asked for, or the days have passed. The very first automatic copy waits for
    /// the player when the game's data is large (`big`: the window offers it).
    pub fn due(&self, now: u64, big: bool) -> bool {
        let s = self.settings();
        s.pending || (s.every_days > 0 && (s.last_at > 0 || !big) && now.saturating_sub(s.last_at) >= u64::from(s.every_days) * 86_400)
    }

    /// Makes a copy now (blocking). Only one at a time.
    pub fn run(&self, now: u64) -> Result<Made, String> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Err("A copy is being made already.".into());
        }
        let s = self.settings();
        let result = make(&self.dir(), &s, now);
        self.running.store(false, Ordering::SeqCst);
        self.finished(now, result.as_ref().err().cloned());
        result
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Made {
    pub new_files: u64,
    pub new_bytes: u64,
    pub database_bytes: u64,
}

/// What the game keeps and how much it takes (Settings → Data).
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Sizes {
    pub maps: u64,
    pub audio: u64,
    pub covers: u64,
    pub replays: u64,
    pub replay_count: u64,
    pub database: u64,
    pub copies: u64,
}

pub fn sizes(copies: Option<&Path>) -> Result<Sizes, String> {
    let game = GameData::locate()?;
    let mut out = Sizes {
        maps: folder_size(&game.file("cache/maps")?),
        audio: folder_size(&game.file("cache/audio")?),
        covers: folder_size(&game.file("cache/covers")?),
        database: std::fs::metadata(game.database()).map(|m| m.len()).unwrap_or(0),
        copies: copies.map(folder_size).unwrap_or(0),
        ..Sizes::default()
    };
    let c = read_only(&game.database())?;
    if let Ok((n, b)) = c.query_row("SELECT COUNT(Replay), COALESCE(SUM(length(Replay)), 0) FROM Scores", [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))) {
        (out.replay_count, out.replays) = (n.max(0) as u64, b.max(0) as u64);
    }
    Ok(out)
}

/// The game's maps and replays together, to know when to offer a first copy.
pub fn game_bytes() -> u64 {
    sizes(None).map(|s| s.maps + s.audio + s.covers + s.replays).unwrap_or(0)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Copy {
    pub name: String,
    pub at: u64,
    pub bytes: u64,
}

/// Database copies in the folder, newest first.
pub fn list(dir: &Path) -> Vec<Copy> {
    let mut out: Vec<Copy> = copies_in(dir)
        .into_iter()
        .filter_map(|(at, path)| {
            Some(Copy { name: path.file_name()?.to_str()?.to_string(), at, bytes: std::fs::metadata(&path).ok()?.len() })
        })
        .collect();
    out.sort_by_key(|c| std::cmp::Reverse(c.at));
    out
}

/// The time in a copy's name: `rhythia-<unix time>.zip`, or `.db` from before 0.4.
fn copy_time(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("rhythia-")?;
    rest.strip_suffix(".zip").or_else(|| rest.strip_suffix(".db"))?.parse().ok()
}

/// The copies in `dir`, with their time.
fn copies_in(dir: &Path) -> Vec<(u64, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| Some((copy_time(&e.file_name().into_string().ok()?)?, e.path())))
        .collect()
}

/// The largest database Atlas puts back from a copy.
const MAX_DB_BYTES: u64 = 3 * GB;

/// Zips a database copy (`db`, removed after) with the list of map files it needs, as
/// `rhythia-<at>.zip`.
fn zip_copy(dir: &Path, db: &Path, at: u64) -> Result<PathBuf, String> {
    let files = files_of(&read_only(db)?, "all")?;
    let list: String = files.iter().map(|(k, n)| format!("{k}/{n}\n")).collect();
    let mut z = crate::files::Zip::new(dir, &format!("rhythia-{at}"));
    z.add_path_deflated("rhythia.db", db)?;
    z.add("files.txt", list.as_bytes())?;
    let made = z.finish()?.remove(0);
    let _ = std::fs::remove_file(db);
    Ok(made)
}

/// The map files (kind, name) a copy needs: its `files.txt`, or read from an old `.db` copy.
fn copy_files(copy: &Path) -> Result<Vec<(&'static str, String)>, String> {
    if copy.extension().is_some_and(|e| e == "db") {
        return files_of(&read_only(copy)?, "all");
    }
    let mut list = Vec::new();
    crate::files::read_entry(copy, "files.txt", &mut list, 64 * 1024 * 1024)?;
    let list = String::from_utf8(list).map_err(|_| "The copy is damaged.")?;
    Ok(list
        .lines()
        .filter_map(|l| {
            let (kind, name) = l.split_once('/')?;
            let kind = KINDS.into_iter().find(|k| *k == kind)?;
            plain_name(kind, name).then(|| (kind, name.to_string()))
        })
        .collect())
}

fn read_only(db: &Path) -> Result<Connection, String> {
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|_| "The game's database could not be opened.")?;
    c.busy_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
    Ok(c)
}

/// A game file name Atlas copies: a sha256, for maps with `.rhc` or `.sspm` after it.
fn plain_name(kind: &str, name: &str) -> bool {
    let stem = if kind == "maps" { name.strip_suffix(".rhc").or_else(|| name.strip_suffix(".sspm")) } else { Some(name) };
    stem.is_some_and(is_hash) && !name.bytes().any(|b| b.is_ascii_uppercase())
}

/// The files (kind, name) the maps in `db` use, for the chosen maps.
fn files_of(c: &Connection, which: &str) -> Result<Vec<(&'static str, String)>, String> {
    let filter = match which {
        "ranked" => "WHERE OnlineStatus = 'RANKED'",
        "unranked" => "WHERE OnlineStatus IS NOT 'RANKED'",
        "none" => return Ok(Vec::new()),
        _ => "",
    };
    let mut stmt = c.prepare(&format!("SELECT Path, AudioPath, ImagePath FROM Maps {filter} LIMIT 200000")).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok([r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?]))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows.flatten() {
        for (kind, path) in KINDS.iter().zip(row) {
            let name = path.and_then(|p| p.strip_prefix(&format!("/cache/{kind}/")).map(str::to_string));
            if let Some(name) = name.filter(|n| plain_name(kind, n)) {
                out.push((*kind, name));
            }
        }
    }
    Ok(out)
}

fn make(dir: &Path, s: &Settings, now: u64) -> Result<Made, String> {
    if crate::game_write::game_running()? {
        return Err("Close Rhythia first. Copies are made while the game is closed.".into());
    }
    let game = GameData::locate()?;
    let db = game.database();
    std::fs::create_dir_all(dir).map_err(|e| format!("Atlas could not make the copy folder: {e}"))?;

    // Copies from before 0.4 are zipped first (a failure leaves them as they are).
    for (at, old) in copies_in(dir) {
        if old.extension().is_some_and(|e| e == "db") {
            let _ = zip_copy(dir, &old, at);
        }
    }
    let source = read_only(&db)?;
    let mut wanted: Vec<(&str, String, PathBuf, u64)> = Vec::new();
    for (kind, name) in files_of(&source, &s.maps)? {
        let to = dir.join("files").join(kind).join(&name);
        if to.is_file() {
            continue;
        }
        let from = game.file(&format!("cache/{kind}/{name}"))?;
        if let Ok(m) = std::fs::metadata(&from) {
            wanted.push((kind, name, from, m.len()));
        }
    }
    let db_bytes = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    let need: u64 = wanted.iter().map(|w| w.3).sum::<u64>() + db_bytes;
    let used = folder_size(dir);
    if used + need > u64::from(s.max_gb) * GB {
        return Err(format!("The copy would pass the {} GB limit.", s.max_gb));
    }
    if let Some(free) = free_space(dir) {
        if free < need + KEEP_FREE {
            return Err(format!("Not enough free space: the copy needs {} and Atlas leaves 2 GB free.", size_text(need)));
        }
    }

    let (mut new_files, mut new_bytes) = (0, 0);
    for (kind, _, from, len) in &wanted {
        let folder = dir.join("files").join(kind);
        std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        let to = folder.join(from.file_name().unwrap_or_default());
        let part = to.with_extension("part");
        if let Err(e) = std::fs::copy(from, &part).and_then(|_| std::fs::rename(&part, &to)) {
            let _ = std::fs::remove_file(&part);
            return Err(format!("Copying the maps stopped: {e}"));
        }
        new_files += 1;
        new_bytes += len;
    }

    // The database: read through the read-only connection into a new file, then replays not chosen
    // are dropped from that new file (attempts stay; only their replays go).
    let part = dir.join(format!("rhythia-{now}.db.part"));
    let _ = std::fs::remove_file(&part);
    let done = (|| -> Result<PathBuf, String> {
        source.execute("VACUUM INTO ?1", [part.to_string_lossy()]).map_err(|e| format!("Copying the database failed: {e}"))?;
        drop(source);
        let drop_sql = match s.replays.as_str() {
            "none" => Some("UPDATE Scores SET Replay = NULL"),
            "passes" => Some("UPDATE Scores SET Replay = NULL WHERE Passed IS NOT 1"),
            "best" => Some(
                "UPDATE Scores SET Replay = NULL WHERE Id NOT IN (SELECT Id FROM (SELECT Id, ROW_NUMBER() OVER \
                 (PARTITION BY COALESCE(MapId, BeatmapHash) ORDER BY Points DESC, Accuracy DESC) k FROM Scores WHERE Passed = 1) WHERE k = 1)",
            ),
            _ => None,
        };
        if let Some(sql) = drop_sql {
            let c = Connection::open(&part).map_err(|e| e.to_string())?;
            c.execute_batch(&format!("{sql}; VACUUM;")).map_err(|e| format!("Leaving out replays failed: {e}"))?;
        }
        zip_copy(dir, &part, now)
    })();
    let made = match done {
        Ok(made) => made,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            let _ = std::fs::remove_file(dir.join(format!("rhythia-{now}-part-1.zip")));
            return Err(e);
        }
    };
    let database_bytes = std::fs::metadata(made).map(|m| m.len()).unwrap_or(0);
    trim(dir, s.keep as usize);
    Ok(Made { new_files, new_bytes, database_bytes })
}

/// Deletes one copy (and the map files no other copy needs), or every copy and every kept file.
pub fn remove(dir: &Path, name: Option<&str>) -> Result<(), String> {
    match name {
        Some(name) => {
            let at = copy_time(name).ok_or("Unknown copy.")?;
            if name != format!("rhythia-{at}.zip") && name != format!("rhythia-{at}.db") {
                return Err("Unknown copy.".into());
            }
            std::fs::remove_file(dir.join(name)).map_err(|e| format!("Could not delete the copy: {e}"))?;
            trim(dir, usize::MAX);
        }
        None => {
            for (_, path) in copies_in(dir) {
                std::fs::remove_file(&path).map_err(|e| format!("Could not delete the copies: {e}"))?;
            }
            let files = dir.join("files");
            if files.is_dir() {
                std::fs::remove_dir_all(&files).map_err(|e| format!("Could not delete the copies: {e}"))?;
            }
        }
    }
    Ok(())
}

/// Keeps the newest `keep` database copies and the files they still need.
fn trim(dir: &Path, keep: usize) {
    let mut copies = copies_in(dir);
    copies.sort_by_key(|c| std::cmp::Reverse(c.0));
    for (_, old) in copies.iter().skip(keep) {
        let _ = std::fs::remove_file(old);
    }
    let mut needed = HashSet::new();
    for (_, path) in copies.iter().take(keep) {
        let Ok(files) = copy_files(path) else { return }; // unsure what is needed: keep every file
        needed.extend(files.into_iter().map(|(k, n)| format!("{k}/{n}")));
    }
    for kind in KINDS {
        let Ok(entries) = std::fs::read_dir(dir.join("files").join(kind)) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !needed.contains(&format!("{kind}/{name}")) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Restored {
    pub files: u64,
    pub missing: u64,
}

/// Puts the copy `name` back as the game's database (game closed; the current database is saved in
/// `saves` first), then copies back the map files it needs that the game no longer has.
pub fn restore(dir: &Path, name: &str, saves: &Path, now: u64) -> Result<Restored, String> {
    let at = copy_time(name).ok_or("Unknown copy.")?;
    let copy = dir.join(name);
    if !copy.is_file() || name != format!("rhythia-{at}.zip") && name != format!("rhythia-{at}.db") {
        return Err("That copy is gone.".into());
    }
    if crate::game_write::game_running()? {
        return Err("Close Rhythia first.".into());
    }
    let game = GameData::locate()?;
    let db = game.database();
    // The copy's database next to the game's, checked before anything of the game's changes.
    let part = db.with_extension("db.atlas-part");
    let unpacked = (|| -> Result<(), String> {
        if copy.extension().is_some_and(|e| e == "db") {
            std::fs::copy(&copy, &part).map_err(|e| format!("Atlas could not put the copy back: {e}"))?;
        } else {
            let mut out = std::io::BufWriter::new(std::fs::File::create(&part).map_err(|e| format!("Atlas could not put the copy back: {e}"))?);
            crate::files::read_entry(&copy, "rhythia.db", &mut out, MAX_DB_BYTES).map_err(|_| "That copy is damaged. Pick another one.")?;
            std::io::Write::flush(&mut out).map_err(|e| format!("Atlas could not put the copy back: {e}"))?;
        }
        let check = read_only(&part)?;
        let ok: String = check.query_row("PRAGMA quick_check", [], |r| r.get(0)).map_err(|e| e.to_string())?;
        if ok != "ok" {
            return Err("That copy is damaged. Pick another one.".into());
        }
        Ok(())
    })();
    let files = unpacked.and_then(|_| copy_files(&copy));
    let files = match files {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
    };
    if let Err(e) = crate::game_write::backup(&db, saves, now) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&part, &db) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("Atlas could not put the copy back: {e}"));
    }
    let (mut put, mut missing) = (0, 0);
    for (kind, name) in files {
        let to = game.file(&format!("cache/{kind}/{name}"))?;
        if to.is_file() {
            continue;
        }
        let from = dir.join("files").join(kind).join(&name);
        if from.is_file() && std::fs::copy(&from, &to).is_ok() {
            put += 1;
        } else {
            missing += 1;
        }
    }
    Ok(Restored { files: put, missing })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Exported {
    pub parts: Vec<String>,
    pub files: u64,
    pub missing: u64,
}

/// `2026-11-21` for a Unix time (UTC).
fn day_of(at: u64) -> String {
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = (at / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

const EXPORT_README: &str = "A copy of Rhythia's maps, scores and replays, made by Atlas for Rhythia.\r\n\r\n\
rhythia.db: the game's database (maps, collections, every attempt, the replays kept in the copy).\r\n\
files/maps, files/audio, files/covers: the map files the game keeps in its cache folder of the same names.\r\n\
\r\nTo use it with Atlas on another computer, put the files back with Atlas closed and Rhythia closed, or keep it as a backup.\r\n";

/// One copy as a ZIP in `to`: its database (compressed), the list of files and every map file it needs
/// (stored), in parts of up to 1.5 GB. Named `Atlas copy <day>`.
pub fn export(dir: &Path, name: &str, to: &Path) -> Result<Exported, String> {
    let at = copy_time(name).ok_or("Unknown copy.")?;
    let copy = dir.join(name);
    if !copy.is_file() || name != format!("rhythia-{at}.zip") && name != format!("rhythia-{at}.db") {
        return Err("That copy is gone.".into());
    }
    let files = copy_files(&copy)?;
    let mut z = crate::files::Zip::new(to, &format!("Atlas copy {}", day_of(at)));
    z.add("README.txt", EXPORT_README.as_bytes())?;
    if copy.extension().is_some_and(|e| e == "db") {
        z.add_path_deflated("rhythia.db", &copy)?;
    } else {
        let tmp = to.join(format!(".atlas-export-{at}.db"));
        let unpacked = (|| -> Result<(), String> {
            let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp).map_err(|e| format!("Could not write to the folder: {e}"))?);
            crate::files::read_entry(&copy, "rhythia.db", &mut out, MAX_DB_BYTES)?;
            std::io::Write::flush(&mut out).map_err(|e| format!("Could not write to the folder: {e}"))?;
            z.add_path_deflated("rhythia.db", &tmp)
        })();
        let _ = std::fs::remove_file(&tmp);
        unpacked?;
    }
    let list: String = files.iter().map(|(k, n)| format!("{k}/{n}\n")).collect();
    z.add("files.txt", list.as_bytes())?;
    let (mut put, mut missing) = (0, 0);
    for (kind, file) in &files {
        match std::fs::read(dir.join("files").join(kind).join(file)) {
            Ok(bytes) if bytes.len() <= crate::files::MAX_MAP_BYTES => {
                z.add(&format!("files/{kind}/{file}"), &bytes)?;
                put += 1;
            }
            _ => missing += 1,
        }
    }
    let parts = z.finish()?.iter().map(|p| p.display().to_string()).collect();
    Ok(Exported { parts, files: put, missing })
}

fn folder_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some(if m.is_dir() { folder_size(&e.path()) } else { m.len() })
        })
        .sum()
}

fn size_text(b: u64) -> String {
    if b >= GB {
        format!("{:.1} GB", b as f64 / GB as f64)
    } else {
        format!("{} MB", b / (1024 * 1024))
    }
}

#[cfg(windows)]
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    // SAFETY: a NUL-ended path and a valid out pointer; the other outs may be null.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(free)
}

#[cfg(not(windows))]
fn free_space(_dir: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_due() {
        let h = "a".repeat(64);
        assert!(plain_name("maps", &format!("{h}.rhc")));
        assert!(plain_name("audio", &h));
        assert!(!plain_name("audio", &format!("{h}.rhc")));
        assert!(!plain_name("maps", "../rhythia.db"));
        assert!(!plain_name("covers", &"A".repeat(64)));

        let dir = std::env::temp_dir().join(format!("atlas-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let b = Backup::load(dir.join("backup.json"), dir.join("Backups"));
        assert!(b.due(1_000_000, false), "first automatic copy when the game's data is small");
        assert!(!b.due(1_000_000, true), "a large first copy waits for the player");
        b.finished(1_000_000, None);
        assert!(!b.due(1_000_000 + 86_400, true));
        assert!(b.due(1_000_000 + 7 * 86_400, true));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn copies_database_and_drops_replays() {
        let dir = std::env::temp_dir().join(format!("atlas-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("game.db");
        let c = Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE Maps (Id INTEGER PRIMARY KEY, Path TEXT, AudioPath TEXT, ImagePath TEXT, OnlineStatus TEXT);
             CREATE TABLE Scores (Id INTEGER PRIMARY KEY, MapId INTEGER, BeatmapHash TEXT, Passed INTEGER, Points REAL, Accuracy REAL, Replay BLOB);
             INSERT INTO Scores VALUES (1, 10, NULL, 1, 300, 97, x'01'), (2, 10, NULL, 1, 320, 98, x'02'), (3, 10, NULL, 0, 0, 50, x'03'), (4, 20, NULL, 1, 10, 90, x'04');",
        )
        .unwrap();
        drop(c);
        let out = dir.join("copy.db");
        read_only(&db).unwrap().execute("VACUUM INTO ?1", [out.to_string_lossy()]).unwrap();
        let c = Connection::open(&out).unwrap();
        c.execute_batch(
            "UPDATE Scores SET Replay = NULL WHERE Id NOT IN (SELECT Id FROM (SELECT Id, ROW_NUMBER() OVER \
             (PARTITION BY COALESCE(MapId, BeatmapHash) ORDER BY Points DESC, Accuracy DESC) k FROM Scores WHERE Passed = 1) WHERE k = 1)",
        )
        .unwrap();
        let kept: Vec<i64> = c.prepare("SELECT Id FROM Scores WHERE Replay IS NOT NULL ORDER BY Id").unwrap().query_map([], |r| r.get(0)).unwrap().flatten().collect();
        assert_eq!(kept, vec![2, 4], "the best pass of each map keeps its replay");
        let rows: i64 = c.query_row("SELECT COUNT(*) FROM Scores", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 4, "attempts stay");
        // Zipped with the list of files it needs, and read back whole.
        let h = "b".repeat(64);
        c.execute("INSERT INTO Maps VALUES (10, ?1, ?2, NULL, 'RANKED')", [format!("/cache/maps/{h}.rhc"), format!("/cache/audio/{h}")]).unwrap();
        drop(c);
        let zip = zip_copy(&dir, &out, 1234).unwrap();
        assert_eq!(zip, dir.join("rhythia-1234.zip"));
        assert!(!out.exists(), "the plain database is gone");
        assert_eq!(copies_in(&dir).len(), 1);
        assert_eq!(copy_files(&zip).unwrap(), vec![("maps", format!("{h}.rhc")), ("audio", h.clone())]);
        let back = dir.join("back.db");
        crate::files::read_entry(&zip, "rhythia.db", &mut std::fs::File::create(&back).unwrap(), MAX_DB_BYTES).unwrap();
        let n: i64 = read_only(&back).unwrap().query_row("SELECT COUNT(*) FROM Scores", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 4);
        // Exported with its map files (one there, one missing).
        std::fs::create_dir_all(dir.join("files/maps")).unwrap();
        std::fs::write(dir.join(format!("files/maps/{h}.rhc")), b"RHYCACHE").unwrap();
        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let ex = export(&dir, "rhythia-1234.zip", &out_dir).unwrap();
        assert_eq!((ex.files, ex.missing), (1, 1));
        let part = PathBuf::from(&ex.parts[0]);
        assert_eq!(part, out_dir.join("Atlas copy 1970-01-01.zip"));
        let mut map = Vec::new();
        crate::files::read_entry(&part, &format!("files/maps/{h}.rhc"), &mut map, 100).unwrap();
        assert_eq!(map, b"RHYCACHE");
        crate::files::read_entry(&part, "rhythia.db", &mut Vec::new(), MAX_DB_BYTES).unwrap();
        assert!(!out_dir.join(".atlas-export-1234.db").exists(), "the unpacked database is gone");
        assert_eq!(day_of(1_795_296_111), "2026-11-21");
        let _ = std::fs::remove_dir_all(dir);
    }
}
