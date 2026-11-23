//! Downloads, in three modes (`Mode`), all from the same daily limit and on the same Downloads list:
//! - Game ("To the game"): one `rhythia://map/<id>` link every 0.8 s, only for maps the game does not
//!   have yet. The game downloads each map itself.
//! - Files: Atlas downloads each map file from Rhythia's file server (paced like every request to
//!   Rhythia), checks it (files.rs) and saves it to the folder the player picked.
//! - Zip: the same, into a ZIP written part by part to that folder (files.rs `Zip`).
//!
//! Maps are reserved from the daily limit before the queue starts; the ones not sent or not
//! downloaded (cancel, error) are given back.
//!
//! Atlas then watches the game's database: a map that shows up there is done, one that does not
//! within `CHECK_FOR` after the last link has failed. Finished maps go to the download history
//! (`downloads.json`, the last `KEEP_HISTORY`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_opener::OpenerExt;

use crate::{game::GameData, library, now_secs, quota, Atlas};

const LINK_GAP: Duration = Duration::from_millis(800);
const CHECK_EVERY: Duration = Duration::from_secs(3);
/// A started game can take a while before it handles links; then each map is 1 to 3 s.
const CHECK_FOR: Duration = Duration::from_secs(90);
const KEEP_HISTORY: usize = 300;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum State {
    Waiting,
    Sent,
    Done,
    Failed,
    Cancelled,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: i64,
    pub title: String,
    pub state: State,
    /// Unix seconds of the last change.
    pub at: u64,
    /// "files" or "zip" for a file download; none for the game.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

/// Where the maps go.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Mode {
    Game,
    Files { dir: String },
    Zip { dir: String, name: String },
}

impl Mode {
    fn tag(&self) -> Option<String> {
        match self {
            Mode::Game => None,
            Mode::Files { .. } => Some("files".into()),
            Mode::Zip { .. } => Some("zip".into()),
        }
    }

    /// The picked folder, which must exist.
    fn folder(&self) -> Result<Option<PathBuf>, String> {
        match self {
            Mode::Game => Ok(None),
            Mode::Files { dir } | Mode::Zip { dir, .. } => {
                let p = PathBuf::from(dir);
                if p.is_absolute() && p.is_dir() {
                    Ok(Some(p))
                } else {
                    Err("Pick a folder for the files first.".into())
                }
            }
        }
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub items: Vec<Item>,
    pub done: bool,
    pub error: Option<String>,
    /// "files" or "zip" for a file download.
    pub to: Option<String>,
    /// The folder the files went to.
    pub folder: Option<String>,
    /// The ZIP parts written.
    pub zips: Vec<String>,
    /// The map files saved (Files), which the player can drag onto the game window.
    pub files: Vec<String>,
}

pub struct Installer {
    running: AtomicBool,
    cancel: AtomicBool,
    job: Mutex<Option<Job>>,
    history_file: PathBuf,
    history: Mutex<Vec<Item>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub queued: usize,
    pub already: usize,
    pub quota: quota::Status,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Downloads {
    pub job: Option<Job>,
    pub history: Vec<Item>,
}

impl Installer {
    /// Maps are being downloaded or sent to the game now.
    pub fn busy(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn load(history_file: PathBuf) -> Self {
        let mut history = crate::store::load::<Vec<Item>>(&history_file).unwrap_or_default();
        history.truncate(KEEP_HISTORY);
        for item in &mut history {
            item.title = library::clip(&item.title, 300);
        }
        Installer {
            running: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            job: Mutex::new(None),
            history_file,
            history: Mutex::new(history),
        }
    }

    pub fn downloads(&self) -> Downloads {
        Downloads { job: self.job.lock().unwrap().clone(), history: self.history.lock().unwrap().clone() }
    }

    pub fn clear_history(&self) {
        let mut history = self.history.lock().unwrap();
        history.clear();
        let _ = crate::store::save(&self.history_file, &*history);
    }

    fn update(&self, app: &AppHandle, f: impl FnOnce(&mut Job)) {
        let snapshot = {
            let mut job = self.job.lock().unwrap();
            let Some(job) = job.as_mut() else { return };
            f(job);
            job.clone()
        };
        let _ = app.emit("install-progress", snapshot);
    }
}

pub fn start(app: &AppHandle, ids: Vec<i64>, mode: Mode) -> Result<Plan, String> {
    let atlas = app.state::<Atlas>();
    let folder = mode.folder()?;
    let mut ids: Vec<i64> = ids.into_iter().filter(|n| *n > 0).collect();
    let mut seen_ids = std::collections::HashSet::new();
    ids.retain(|id| seen_ids.insert(*id));
    let wanted = ids.len();
    // The game skips maps it has; files and ZIPs take every map asked for.
    if mode == Mode::Game {
        let have = GameData::locate().and_then(|g| library::online_ids(&g.database()))?;
        ids.retain(|id| !have.contains(id));
    }
    let already = wanted - ids.len();
    if ids.is_empty() {
        return Ok(Plan { queued: 0, already, quota: atlas.quota.status(now_secs()) });
    }
    if atlas.installer.running.swap(true, Ordering::SeqCst) {
        return Err("Another install is running. Wait for it or cancel it.".into());
    }
    let status = match atlas.quota.take(ids.len() as u32, now_secs()) {
        Ok(s) => s,
        Err(s) => {
            atlas.installer.running.store(false, Ordering::SeqCst);
            return Err(format!("Daily limit: {} of {} maps left.", s.left, s.limit));
        }
    };
    atlas.installer.cancel.store(false, Ordering::SeqCst);
    let now = now_secs();
    let items = ids
        .iter()
        .map(|&id| Item {
            id,
            title: atlas.seen.get(id).map(|m| m.title).filter(|t| !t.is_empty()).unwrap_or_else(|| format!("Map {id}")),
            state: State::Waiting,
            at: now,
            to: mode.tag(),
        })
        .collect();
    *atlas.installer.job.lock().unwrap() = Some(Job {
        items,
        done: false,
        error: None,
        to: mode.tag(),
        folder: folder.as_ref().map(|p| p.display().to_string()),
        zips: Vec::new(),
        files: Vec::new(),
    });
    let queued = ids.len();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match (mode, folder) {
            (Mode::Game, _) => run(app).await,
            (Mode::Zip { name, .. }, Some(dir)) => run_files(app, dir, Some(name)).await,
            (_, Some(dir)) => run_files(app, dir, None).await,
            _ => {}
        }
    });
    Ok(Plan { queued, already, quota: status })
}

/// Marks sent maps that are now in the game as done.
fn check(app: &AppHandle) {
    let Ok(have) = GameData::locate().and_then(|g| library::online_ids(&g.database())) else { return };
    let now = now_secs();
    app.state::<Atlas>().installer.update(app, |job| {
        for item in job.items.iter_mut().filter(|i| i.state == State::Sent && have.contains(&i.id)) {
            item.state = State::Done;
            item.at = now;
        }
    });
}

async fn run(app: AppHandle) {
    let atlas = app.state::<Atlas>();
    let ids: Vec<i64> = atlas.installer.job.lock().unwrap().as_ref().map(|j| j.items.iter().map(|i| i.id).collect()).unwrap_or_default();
    let (mut sent, mut error) = (0usize, None);
    for (n, id) in ids.iter().enumerate() {
        if atlas.installer.cancel.load(Ordering::SeqCst) {
            break;
        }
        if let Err(e) = app.opener().open_url(format!("rhythia://map/{id}"), None::<&str>) {
            error = Some(format!("The game did not take the link: {e}"));
            break;
        }
        sent += 1;
        let now = now_secs();
        atlas.installer.update(&app, |job| {
            job.items[n].state = State::Sent;
            job.items[n].at = now;
        });
        if n % 4 == 3 {
            check(&app);
        }
        tokio::time::sleep(LINK_GAP).await;
    }
    atlas.quota.give_back((ids.len() - sent) as u32, now_secs());
    let now = now_secs();
    atlas.installer.update(&app, |job| {
        for item in job.items.iter_mut().filter(|i| i.state == State::Waiting) {
            item.state = State::Cancelled;
            item.at = now;
        }
    });

    let until = tokio::time::Instant::now() + CHECK_FOR;
    while tokio::time::Instant::now() < until {
        check(&app);
        let waiting = atlas.installer.job.lock().unwrap().as_ref().is_some_and(|j| j.items.iter().any(|i| i.state == State::Sent));
        if !waiting {
            break;
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }

    let now = now_secs();
    let mut finished = Vec::new();
    atlas.installer.update(&app, |job| {
        for item in job.items.iter_mut().filter(|i| i.state == State::Sent) {
            item.state = State::Failed;
            item.at = now;
        }
        job.done = true;
        job.error = error;
        finished = job.items.clone();
    });
    let any_done = finished.iter().any(|i| i.state == State::Done);
    {
        let mut history = atlas.installer.history.lock().unwrap();
        finished.reverse();
        finished.retain(|i| i.state != State::Cancelled);
        history.splice(0..0, finished);
        history.truncate(KEEP_HISTORY);
        let _ = crate::store::save(&atlas.installer.history_file, &*history);
    }
    atlas.installer.running.store(false, Ordering::SeqCst);
    // "A copy after each download": made by show_waiting once the game is closed.
    if any_done && atlas.backup.settings().after_download {
        atlas.backup.set_pending(true);
    }
}

/// Files and ZIP: each map's file downloaded, checked and saved (or added to the ZIP) one after another.
async fn run_files(app: AppHandle, dir: PathBuf, zip_name: Option<String>) {
    let atlas = app.state::<Atlas>();
    let ids: Vec<i64> = atlas.installer.job.lock().unwrap().as_ref().map(|j| j.items.iter().map(|i| i.id).collect()).unwrap_or_default();
    let mut zip = zip_name.map(|n| crate::files::Zip::new(&dir, &n));
    let (mut tried, mut error) = (0usize, None);
    let mut saved = Vec::new();
    for (n, &id) in ids.iter().enumerate() {
        if atlas.installer.cancel.load(Ordering::SeqCst) {
            break;
        }
        tried += 1;
        let now = now_secs();
        atlas.installer.update(&app, |job| {
            job.items[n].state = State::Sent;
            job.items[n].at = now;
        });
        let got = async {
            let (url, title) = crate::rhythia::map_file(&atlas.net, &atlas.seen, id).await?;
            let ext = crate::files::ext_of(&url).ok_or("Unknown map file.")?;
            let (bytes, sha1) = atlas.net.get_file(&url, crate::files::MAX_MAP_BYTES).await?;
            crate::files::check(&bytes, ext, sha1.as_deref())?;
            let name = crate::files::file_name(&title, id, ext);
            match zip.as_mut() {
                Some(z) => z.add(&name, &bytes),
                None => {
                    std::fs::write(dir.join(&name), &bytes).map_err(|e| format!("Could not save {name}: {e}"))?;
                    saved.push(dir.join(&name).display().to_string());
                    Ok(())
                }
            }
        }
        .await;
        let now = now_secs();
        let state = if got.is_ok() { State::Done } else { State::Failed };
        atlas.installer.update(&app, |job| {
            job.items[n].state = state;
            job.items[n].at = now;
        });
        // A full disk or a folder Atlas may not write to stops the rest.
        if let Err(e) = got {
            if e.starts_with("Could not") {
                error = Some(e);
                break;
            }
        }
    }
    let zips = match zip.map(|z| z.finish()) {
        Some(Ok(paths)) => paths.iter().map(|p| p.display().to_string()).collect(),
        Some(Err(e)) => {
            error.get_or_insert(e);
            Vec::new()
        }
        None => Vec::new(),
    };
    atlas.quota.give_back((ids.len() - tried) as u32, now_secs());
    let now = now_secs();
    let mut finished = Vec::new();
    atlas.installer.update(&app, |job| {
        for item in job.items.iter_mut().filter(|i| i.state == State::Waiting) {
            item.state = State::Cancelled;
            item.at = now;
        }
        job.done = true;
        job.error = error;
        job.zips = zips;
        job.files = saved;
        finished = job.items.clone();
    });
    {
        let mut history = atlas.installer.history.lock().unwrap();
        finished.reverse();
        finished.retain(|i| i.state != State::Cancelled);
        history.splice(0..0, finished);
        history.truncate(KEEP_HISTORY);
        let _ = crate::store::save(&atlas.installer.history_file, &*history);
    }
    atlas.installer.running.store(false, Ordering::SeqCst);
}

/// The map files of the last Files download that are still there, at most `MAX_DRAG`.
pub fn saved_files(app: &AppHandle) -> Vec<PathBuf> {
    const MAX_DRAG: usize = 500;
    let atlas = app.state::<Atlas>();
    let job = atlas.installer.job.lock().unwrap();
    job.as_ref()
        .filter(|j| j.done)
        .map(|j| j.files.iter().map(PathBuf::from).filter(|p| p.is_file()).take(MAX_DRAG).collect())
        .unwrap_or_default()
}

pub fn cancel(app: &AppHandle) {
    app.state::<Atlas>().installer.cancel.store(true, Ordering::SeqCst);
}
