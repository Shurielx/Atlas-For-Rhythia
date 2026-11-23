mod account;
mod atlas_link;
mod backup;
mod basic_kinds;
mod bridge;
mod covers;
mod files;
mod foryou;
mod game;
mod game_write;
mod install;
mod library;
mod mine;
mod near;
mod net;
mod quota;
mod rhythia;
mod secret;
mod songs;
mod store;
mod updates;
mod vps;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_opener::OpenerExt;

pub struct Atlas {
    net: net::Net,
    quota: quota::Quota,
    vps: vps::Vps,
    installer: install::Installer,
    seen: rhythia::Seen,
    covers: covers::Covers,
    mine: mine::Mine,
    backups: PathBuf,
    /// Copies of maps and replays (Settings → Data).
    backup: backup::Backup,
    /// Map types Atlas worked out from the game's map files.
    basic: basic_kinds::BasicKinds,
    /// The game folder picked in Settings (`game.json`).
    game_file: PathBuf,
    bridge: bridge::Bridge,
    /// A link that arrived before the window could listen (Atlas started by the link).
    pending_link: Mutex<Option<atlas_link::AtlasLink>>,
    /// The player's Rhythia sign-in, and the session tokens the Rhythia window hands over.
    account: account::Account,
    account_tx: tokio::sync::mpsc::Sender<String>,
    account_rx: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<String>>,
    /// A newer Atlas found on GitHub (updates.rs).
    updates: updates::Updates,
    /// Songs of maps the game does not have (songs.rs).
    previews: songs::Previews,
    /// The players ranked near the player (near.rs).
    near: near::Near,
    /// "At which speed" answers from our server (foryou.rs).
    for_you: foryou::Kept,
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    version: &'static str,
    game_data: Option<String>,
    game_error: Option<String>,
    /// The folder was picked in Settings, not found by Atlas.
    game_chosen: bool,
    rhythia_per_second: u32,
    maps_per_day: u32,
}

#[tauri::command]
fn app_info() -> AppInfo {
    let (game_data, game_error) = match game::GameData::locate() {
        Ok(g) => (Some(g.dir().display().to_string()), None),
        Err(e) => (None, Some(e)),
    };
    AppInfo {
        version: env!("CARGO_PKG_VERSION"),
        game_data,
        game_error,
        game_chosen: game::chosen().is_some(),
        rhythia_per_second: net::RHYTHIA_PER_SECOND,
        maps_per_day: quota::MAPS_PER_WINDOW,
    }
}

#[derive(Serialize, serde::Deserialize, Default)]
struct GameChoice {
    dir: Option<PathBuf>,
}

/// Lets the player point Atlas at the game's data folder (the one with `rhythia.db`).
#[tauri::command]
async fn game_folder_pick(app: AppHandle) -> Result<AppInfo, String> {
    use tauri_plugin_dialog::DialogExt;
    let asker = app.clone();
    let picked = blocking(move || {
        let mut dialog = asker.dialog().file().set_title("Rhythia's data folder (it has rhythia.db)");
        if let Some(w) = asker.get_webview_window("main") {
            dialog = dialog.set_parent(&w);
        }
        Ok(dialog.blocking_pick_folder())
    })
    .await?;
    let Some(picked) = picked else { return Ok(app_info()) };
    let path = picked.into_path().map_err(|e| e.to_string())?;
    let dir = game::data_folder_in(&path).ok_or("No Rhythia data there: the folder has no rhythia.db.")?;
    let state = app.state::<Atlas>();
    store::save(&state.game_file, &GameChoice { dir: Some(dir.clone()) })?;
    game::choose(Some(dir));
    Ok(app_info())
}

/// Forgets a picked folder and looks in the usual places again.
#[tauri::command]
fn game_folder_auto(state: State<'_, Atlas>) -> Result<AppInfo, String> {
    store::save(&state.game_file, &GameChoice::default())?;
    game::choose(None);
    Ok(app_info())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DataInfo {
    folder: String,
    cover_files: u64,
    cover_bytes: u64,
}

#[tauri::command]
fn data_info(app: AppHandle, state: State<'_, Atlas>) -> Result<DataInfo, String> {
    let (cover_files, cover_bytes) = covers::size(&state.covers);
    let folder = app.path().app_data_dir().map_err(|e| e.to_string())?.display().to_string();
    Ok(DataInfo { folder, cover_files, cover_bytes })
}

#[tauri::command]
fn clear_covers(state: State<'_, Atlas>) {
    covers::clear(&state.covers);
}

#[tauri::command]
fn open_data_folder(app: AppHandle) -> Result<(), String> {
    explore(&app.path().app_data_dir().map_err(|e| e.to_string())?)
}

/// Settings → Data → Uninstall: whether this Atlas was installed (its uninstaller is next to it; a dev build has
/// none), Atlas' data folder and the copies folder, and whether the copies go with the data.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UninstallInfo {
    installed: bool,
    data_dir: String,
    copies_dir: String,
    copies_in_data: bool,
}

fn uninstaller() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("uninstall.exe")).filter(|p| p.is_file())
}

#[tauri::command]
fn uninstall_info(app: AppHandle) -> Result<UninstallInfo, String> {
    let data = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let copies = app.state::<Atlas>().backup.dir();
    Ok(UninstallInfo {
        installed: uninstaller().is_some(),
        copies_in_data: copies.starts_with(&data),
        data_dir: data.display().to_string(),
        copies_dir: copies.display().to_string(),
    })
}

/// Uninstalls Atlas: Windows' own uninstaller in its quick mode (a progress window, no questions), and Atlas
/// closes. With `delete_data` it signs out of Rhythia first (the password store and the Rhythia window's
/// profile) and the uninstaller deletes Atlas' data folders (`/DELETEDATA`, windows/hooks.nsh). The game's
/// maps, scores and collections stay.
#[tauri::command]
async fn uninstall_atlas(app: AppHandle, delete_data: bool) -> Result<(), String> {
    let exe = uninstaller().ok_or("Only an installed Atlas can uninstall itself. Use Windows Settings → Apps.")?;
    if delete_data {
        account::sign_out(&app).await;
    }
    let mut run = std::process::Command::new(exe);
    run.arg("/P");
    if delete_data {
        run.arg("/DELETEDATA");
    }
    run.spawn().map_err(|e| format!("The uninstaller did not start: {e}"))?;
    app.exit(0);
    Ok(())
}

/// Opens a folder in Explorer (Explorer itself, which also takes folders the shell's "open" refused).
fn explore(dir: &std::path::Path) -> Result<(), String> {
    if !dir.is_absolute() || !dir.is_dir() {
        return Err("That folder is gone.".into());
    }
    std::process::Command::new("explorer.exe").arg(dir).spawn().map(|_| ()).map_err(|e| format!("Explorer did not open: {e}"))
}

/// Size of everything in the window (Settings → Look).
#[tauri::command]
fn set_zoom(window: tauri::WebviewWindow, scale: f64) -> Result<(), String> {
    if !(0.75..=2.0).contains(&scale) {
        return Err("Unknown size.".into());
    }
    window.set_zoom(scale).map_err(|e| e.to_string())
}

fn database() -> Result<PathBuf, String> {
    Ok(game::GameData::locate()?.database())
}

/// Runs blocking work (database, files) off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

fn installed() -> HashSet<i64> {
    database().and_then(|db| library::online_ids(&db)).unwrap_or_default()
}

#[tauri::command]
async fn library() -> Result<library::Library, String> {
    blocking(|| library::read(&database()?)).await
}

/// Your game: tries and passes per day, the best pass of every map, and when the running game started.
#[derive(serde::Serialize)]
struct Picture {
    days: Vec<library::DayPlays>,
    bests: Vec<library::BestPass>,
    /// The running game's session; none while the game is closed.
    session: Option<Session>,
}

#[derive(serde::Serialize)]
struct Session {
    /// When the game was started (Unix seconds).
    since: u64,
    tries: i64,
    passes: i64,
    /// The newest attempt.
    last: Option<library::NewTry>,
}

#[tauri::command]
async fn game_picture() -> Result<Picture, String> {
    blocking(|| {
        let db = database()?;
        let running = game_write::game_running().unwrap_or(false);
        let since = running.then(|| game::GameData::locate().ok()?.started()).flatten();
        let session = match since {
            Some(since) => {
                let (tries, passes) = library::tries_since(&db, since)?;
                let newest = library::newest_try(&db)?;
                let last = if tries > 0 { library::tries_after(&db, newest - 1)?.pop() } else { None };
                Some(Session { since, tries, passes, last })
            }
            None => None,
        };
        Ok(Picture { days: library::days(&db)?, bests: library::best_passes(&db, None)?, session })
    })
    .await
}

#[tauri::command]
async fn map_attempts(online_id: Option<i64>, hash: Option<String>) -> Result<Vec<library::Attempt>, String> {
    blocking(move || library::attempts(&database()?, online_id, hash.as_deref())).await
}

/// Your whole tries of every map at each speed (library.rs `speed_tries`), for At which speed.
#[tauri::command]
async fn speed_tries() -> Result<Vec<library::SpeedTries>, String> {
    blocking(|| library::speed_tries(&database()?)).await
}

#[tauri::command]
async fn catalog(state: State<'_, Atlas>, query: rhythia::CatalogQuery) -> Result<rhythia::CatalogPage, String> {
    let mut result = rhythia::beatmaps(&state.net, &state.seen, &query).await?;
    let have = installed();
    for m in &mut result.maps {
        m.installed = have.contains(&m.id);
    }
    Ok(result)
}

/// Downloads maps: to the game (default), as files or as a ZIP in a picked folder (install.rs).
#[tauri::command]
fn install_maps(app: AppHandle, ids: Vec<i64>, mode: Option<install::Mode>) -> Result<install::Plan, String> {
    if ids.len() > quota::MAPS_PER_WINDOW as usize {
        return Err("Too many maps at once.".into());
    }
    install::start(&app, ids, mode.unwrap_or(install::Mode::Game))
}

/// The folder for file and ZIP downloads, picked in Windows' own dialog.
#[tauri::command]
async fn download_pick_folder(app: AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let asker = app.clone();
    let picked = blocking(move || {
        let mut dialog = asker.dialog().file().set_title("Where Atlas saves map files and ZIPs");
        if let Some(w) = asker.get_webview_window("main") {
            dialog = dialog.set_parent(&w);
        }
        Ok(dialog.blocking_pick_folder())
    })
    .await?;
    Ok(picked.and_then(|p| p.into_path().ok()).map(|p| p.display().to_string()))
}

/// Opens a folder in Explorer (where files or ZIPs went).
#[tauri::command]
fn open_folder(path: String) -> Result<(), String> {
    explore(&PathBuf::from(path))
}

#[tauri::command]
fn cancel_install(app: AppHandle) {
    install::cancel(&app);
}

#[tauri::command]
fn downloads(state: State<'_, Atlas>) -> install::Downloads {
    state.installer.downloads()
}

#[tauri::command]
fn clear_downloads(state: State<'_, Atlas>) {
    state.installer.clear_history();
}

/// Public collections on Rhythia: newest, by search, or the player's own (their id from the
/// game's log).
#[tauri::command]
async fn online_collections(
    state: State<'_, Atlas>,
    page: u32,
    search: String,
    mine: bool,
    min_maps: Option<u32>,
) -> Result<Vec<rhythia::ListedCollection>, String> {
    let owner = if mine {
        Some(me(&state).ok_or("Sign in to Rhythia first (Settings → Account).")?)
    } else {
        None
    };
    rhythia::collections(&state.net, page, &search, owner, min_maps.unwrap_or(1)).await
}

#[tauri::command]
async fn online_collection(state: State<'_, Atlas>, id: i64, first_page: Option<bool>) -> Result<rhythia::OnlineCollection, String> {
    if id <= 0 || id >= 10_000_000_000 {
        return Err("Unknown collection.".into());
    }
    let mut found = rhythia::collection(&state.net, &state.seen, id, first_page.unwrap_or(false)).await?;
    let have = installed();
    for m in &mut found.maps {
        m.installed = have.contains(&m.id);
    }
    Ok(found)
}

/// A map's top 50 on Rhythia (of everyone or of one country) and the player's own best there (their public
/// id from the game's log).
#[tauri::command]
async fn map_board(state: State<'_, Atlas>, id: i64, country: Option<String>) -> Result<rhythia::Board, String> {
    if id <= 0 || id >= 10_000_000_000 {
        return Err("Unknown map.".into());
    }
    let player = game::GameData::locate().ok().and_then(|g| g.player_id());
    rhythia::board(&state.net, id, player, country.as_deref().and_then(rhythia::country)).await
}

/// One page of Rhythia's ranking of players (of everyone or of one country).
#[tauri::command]
async fn top_players(state: State<'_, Atlas>, page: i64, country: Option<String>) -> Result<rhythia::PlayersPage, String> {
    rhythia::players(&state.net, page, country.as_deref().and_then(rhythia::country)).await
}

/// The player's own places on Rhythia (public profile), none when Atlas does not know their account.
#[tauri::command]
async fn my_rank(state: State<'_, Atlas>) -> Result<Option<rhythia::Rank>, String> {
    match me(&state) {
        Some(id) => rhythia::rank(&state.net, id).await.map(Some),
        None => Ok(None),
    }
}

/// The player's best plays on Rhythia (near.rs `my_top`), none when Atlas does not know their account.
#[tauri::command]
async fn my_top(state: State<'_, Atlas>) -> Result<Option<Vec<near::TopPlay>>, String> {
    match me(&state) {
        Some(id) => near::my_top(&state.net, id).await.map(Some),
        None => Ok(None),
    }
}

#[tauri::command]
async fn game_collections() -> Result<Vec<library::GameCollection>, String> {
    blocking(|| library::game_collections(&database()?)).await
}

#[tauri::command]
fn my_collections(state: State<'_, Atlas>) -> Vec<mine::Collection> {
    state.mine.all()
}

#[tauri::command]
fn collection_create(state: State<'_, Atlas>, name: String, maps: Vec<mine::Item>) -> Result<mine::Collection, String> {
    state.mine.create(&name, maps, now_secs())
}

#[tauri::command]
fn collection_rename(state: State<'_, Atlas>, id: u64, name: String) -> Result<(), String> {
    state.mine.rename(id, &name)
}

#[tauri::command]
fn collection_delete(state: State<'_, Atlas>, id: u64) -> Result<(), String> {
    state.mine.delete(id)
}

#[tauri::command]
fn collection_add(state: State<'_, Atlas>, id: u64, maps: Vec<mine::Item>) -> Result<usize, String> {
    state.mine.add(id, maps)
}

#[tauri::command]
fn collection_remove(state: State<'_, Atlas>, id: u64, maps: Vec<mine::Item>) -> Result<(), String> {
    state.mine.remove(id, maps)
}

/// Shows an Atlas collection in the game (game closed, database backed up first).
#[tauri::command]
async fn collection_to_game(app: AppHandle, id: u64) -> Result<game_write::Written, String> {
    let atlas = app.state::<Atlas>();
    let c = atlas.mine.get(id).ok_or("That collection is gone.")?;
    let backups = atlas.backups.clone();
    let written = blocking(move || game_write::write(&database()?, &backups, &c, now_secs())).await?;
    atlas.mine.set_game_id(id, written.game_id)?;
    Ok(written)
}

/// Removes maps from the game (game closed, database backed up first). With "copy before removing"
/// on, a copy of maps and replays is made first, so the maps can come back.
#[tauri::command]
async fn uninstall_maps(app: AppHandle, ids: Vec<i64>) -> Result<game_write::Removed, String> {
    if ids.is_empty() || ids.len() > 20_000 {
        return Err("Pick the maps to remove.".into());
    }
    if blocking(game_write::game_running).await? {
        return Err("Close Rhythia first. Atlas removes maps only while the game is closed.".into());
    }
    let handle = app.clone();
    blocking(move || {
        let atlas = handle.state::<Atlas>();
        if atlas.backup.settings().after_download {
            atlas.backup.run(now_secs()).map_err(|e| format!("The copy before removing failed, nothing was removed: {e}"))?;
        }
        game_write::uninstall(&game::GameData::locate()?, &atlas.backups, &ids, now_secs())
    })
    .await
}

/// "Download as a collection": the collection goes into the game by itself once Rhythia is closed
/// (show_waiting), or `false` takes that back.
#[tauri::command]
fn collection_want_game(state: State<'_, Atlas>, id: u64, want: bool) -> Result<(), String> {
    state.mine.set_want_game(id, want)
}

/// Every 20 seconds: Atlas collections waiting for the game are written once Rhythia is closed
/// (after a backup, like "Show in game"). The window hears `game-collection-written`. Copies of maps
/// and replays that are due are made here too, also only while the game is closed (`backup-done`).
async fn show_waiting(app: AppHandle) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(20));
    let mut ticks = 0u32;
    loop {
        tick.tick().await;
        ticks = ticks.wrapping_add(1);
        let atlas = app.state::<Atlas>();
        let waiting: Vec<_> = atlas.mine.all().into_iter().filter(|c| c.want_game).collect();
        // A copy asked for is looked at every time, a timed one every 10 minutes.
        let copy_soon = atlas.backup.settings().pending || ticks % 30 == 1;
        if (waiting.is_empty() && !copy_soon) || blocking(game_write::game_running).await.unwrap_or(true) {
            continue;
        }
        if copy_soon {
            let s = atlas.backup.settings();
            let big = s.last_at == 0 && !s.pending && blocking(|| Ok(backup::game_bytes() > backup::NUDGE_BYTES)).await.unwrap_or(true);
            if atlas.backup.due(now_secs(), big) {
                let handle = app.clone();
                let made = blocking(move || handle.state::<Atlas>().backup.run(now_secs())).await;
                let _ = app.emit("backup-done", match made {
                    Ok(m) => serde_json::json!({ "made": m }),
                    Err(e) => serde_json::json!({ "error": e }),
                });
            }
        }
        for c in waiting {
            let (backups, id, name) = (atlas.backups.clone(), c.id, c.name.clone());
            match blocking(move || game_write::write(&database()?, &backups, &c, now_secs())).await {
                Ok(w) => {
                    let _ = atlas.mine.set_game_id(id, w.game_id);
                    let _ = app.emit("game-collection-written", serde_json::json!({ "name": name, "maps": w.maps, "missing": w.missing }));
                }
                Err(e) => {
                    let _ = app.emit("game-collection-written", serde_json::json!({ "name": name, "error": e }));
                    let _ = atlas.mine.set_want_game(id, false);
                }
            }
        }
    }
}

/// Every 4 seconds: when the game's database changed (its size or time), the window hears
/// `game-changed` with the attempts that came in, so lists stay current while the player plays.
/// Only the file's details are looked at until it changes; then it is read like any time.
async fn watch_game(app: AppHandle) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(4));
    let (mut seen, mut newest) = (None, None::<i64>);
    loop {
        tick.tick().await;
        let Ok(db) = database() else { continue };
        let stamp = std::fs::metadata(&db).ok().map(|m| (m.len(), m.modified().ok()));
        if stamp.is_none() || stamp == seen {
            continue;
        }
        let first = seen.is_none();
        seen = stamp;
        let (db2, after) = (db.clone(), newest);
        let Ok((top, tries)) = blocking(move || {
            let top = library::newest_try(&db2)?;
            let tries = match after { Some(a) if top > a => library::tries_after(&db2, a)?, _ => Vec::new() };
            Ok((top, tries))
        })
        .await
        else {
            continue;
        };
        newest = Some(top);
        if !first {
            let _ = app.emit("game-changed", serde_json::json!({ "tries": tries }));
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BackupInfo {
    settings: backup::Settings,
    dir: String,
    copies: Vec<backup::Copy>,
}

fn backup_info(state: &Atlas) -> BackupInfo {
    let dir = state.backup.dir();
    BackupInfo { settings: state.backup.settings(), dir: dir.display().to_string(), copies: backup::list(&dir) }
}

#[tauri::command]
fn backup_status(state: State<'_, Atlas>) -> BackupInfo {
    backup_info(&state)
}

#[tauri::command]
fn backup_set(state: State<'_, Atlas>, settings: backup::Settings) -> Result<BackupInfo, String> {
    let mut settings = settings;
    settings.dir = state.backup.settings().dir; // the folder changes only through the picker
    state.backup.set(settings)?;
    Ok(backup_info(&state))
}

/// What the game's files and replays take, and the copy folder.
#[tauri::command]
async fn game_sizes(state: State<'_, Atlas>) -> Result<backup::Sizes, String> {
    let dir = state.backup.dir();
    blocking(move || backup::sizes(Some(&dir))).await
}

/// "Back up now": made at once if the game is closed, else once it closes.
#[tauri::command]
async fn backup_now(app: AppHandle) -> Result<Option<backup::Made>, String> {
    if blocking(game_write::game_running).await? {
        app.state::<Atlas>().backup.set_pending(true);
        return Ok(None);
    }
    let handle = app.clone();
    blocking(move || handle.state::<Atlas>().backup.run(now_secs())).await.map(Some)
}

#[tauri::command]
fn backup_cancel_pending(state: State<'_, Atlas>) {
    state.backup.set_pending(false);
}

#[tauri::command]
async fn backup_pick_folder(app: AppHandle) -> Result<BackupInfo, String> {
    use tauri_plugin_dialog::DialogExt;
    let asker = app.clone();
    let picked = blocking(move || {
        let mut dialog = asker.dialog().file().set_title("Where Atlas keeps copies of your maps and replays");
        if let Some(w) = asker.get_webview_window("main") {
            dialog = dialog.set_parent(&w);
        }
        Ok(dialog.blocking_pick_folder())
    })
    .await?;
    let state = app.state::<Atlas>();
    if let Some(p) = picked {
        let dir = p.into_path().map_err(|e| e.to_string())?;
        let game = game::GameData::locate().ok().map(|g| g.dir().to_path_buf());
        if game.is_some_and(|g| dir.starts_with(&g)) {
            return Err("Pick a folder outside the game's data folder.".into());
        }
        let mut s = state.backup.settings();
        s.dir = Some(dir);
        state.backup.set(s)?;
    }
    Ok(backup_info(&state))
}

#[tauri::command]
fn open_backup_folder(app: AppHandle) -> Result<(), String> {
    let dir = app.state::<Atlas>().backup.dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    explore(&dir)
}

/// Deletes one copy, or all of them (`name` none).
#[tauri::command]
async fn backup_delete(app: AppHandle, name: Option<String>) -> Result<(), String> {
    let dir = app.state::<Atlas>().backup.dir();
    blocking(move || backup::remove(&dir, name.as_deref())).await
}

/// Puts a database copy back (game closed; the current database is saved first).
#[tauri::command]
async fn backup_restore(app: AppHandle, name: String) -> Result<backup::Restored, String> {
    let atlas = app.state::<Atlas>();
    let (dir, saves) = (atlas.backup.dir(), atlas.backups.clone());
    blocking(move || backup::restore(&dir, &name, &saves, now_secs())).await
}

/// One copy as a ZIP (or ZIP parts) in a folder the player picked: database and map files together.
#[tauri::command]
async fn backup_export(app: AppHandle, name: String, dir: String) -> Result<backup::Exported, String> {
    let to = PathBuf::from(&dir);
    if !to.is_absolute() || !to.is_dir() {
        return Err("Pick a folder first.".into());
    }
    let from = app.state::<Atlas>().backup.dir();
    blocking(move || backup::export(&from, &name, &to)).await
}

#[tauri::command]
fn quota_status(state: State<'_, Atlas>) -> quota::Status {
    state.quota.status(now_secs())
}

/// Who is signed in to Rhythia in Atlas (no token leaves this command).
#[tauri::command]
fn account_status(state: State<'_, Atlas>) -> account::Status {
    state.account.status(now_secs())
}

/// Opens the Rhythia window to sign in; `account-changed` follows once signed in.
#[tauri::command]
fn rhythia_sign_in(app: AppHandle) {
    tauri::async_runtime::spawn(account::sign_in_with_window(app));
}

/// "Check" in Settings: renews the sign-in when it has ended and asks Rhythia who it belongs to.
#[tauri::command]
async fn account_check(app: AppHandle, state: State<'_, Atlas>) -> Result<account::Status, String> {
    state.account.check(&app, &state.net).await?;
    Ok(state.account.status(now_secs()))
}

/// "Sign in with Reimagined": for two minutes the paired extension may hand the sign-in over.
#[tauri::command]
fn rhythia_sign_in_reimagined(state: State<'_, Atlas>) -> Result<account::Status, String> {
    state.account.wait_for_reimagined(now_secs());
    if !state.bridge.ask_session() {
        return Err(if state.bridge.status().has_pairing() {
            "Reimagined is not connected right now. Open your browser with a rhythia.com tab, then try again."
        } else {
            "Reimagined is not connected. In its popup: Features → Extras → Atlas → Connect."
        }
        .into());
    }
    Ok(state.account.status(now_secs()))
}

/// Maps changed in a Rhythia collection: done, already that way before, not done (and Rhythia's last refusal).
#[derive(Serialize)]
struct Sent {
    id: i64,
    added: usize,
    already: usize,
    failed: usize,
    reason: Option<String>,
}

// The player's own collections on Rhythia, changed from Atlas (Collections → My Rhythia). Rhythia
// checks that the collection is theirs; the session goes only to Rhythia (account.rs).

#[tauri::command]
async fn rhythia_collection_create(app: AppHandle, state: State<'_, Atlas>, title: String) -> Result<i64, String> {
    let token = state.account.fresh_token(&app, &state.net).await?;
    rhythia::create_collection(&state.net, &token, &title).await
}

#[tauri::command]
async fn rhythia_collection_edit(app: AppHandle, state: State<'_, Atlas>, id: i64, title: String, description: String, is_list: bool) -> Result<(), String> {
    let token = state.account.fresh_token(&app, &state.net).await?;
    rhythia::edit_collection(&state.net, &token, id, &title, &description, is_list).await
}

#[tauri::command]
async fn rhythia_collection_delete(app: AppHandle, state: State<'_, Atlas>, id: i64) -> Result<(), String> {
    let token = state.account.fresh_token(&app, &state.net).await?;
    rhythia::delete_collection(&state.net, &token, id).await
}

/// Adds maps to one of the player's Rhythia collections, or removes them (at most 1000 at once). Rhythia does not
/// say when a map did not go in, so Atlas reads the collection before (maps already in it, or already out, are
/// not sent) and after (what really changed). The window follows with `collection-progress` ({done, of}).
#[tauri::command]
async fn rhythia_collection_maps(app: AppHandle, state: State<'_, Atlas>, id: i64, maps: Vec<i64>, add: bool) -> Result<Sent, String> {
    let mut maps: Vec<i64> = maps.into_iter().filter(|n| (1..10_000_000_000).contains(n)).collect();
    let mut seen = std::collections::HashSet::new();
    maps.retain(|n| seen.insert(*n));
    if maps.is_empty() || maps.len() > 1000 {
        return Err("Pick between 1 and 1000 maps.".into());
    }
    let token = state.account.fresh_token(&app, &state.net).await?;
    let ids_in = |c: rhythia::OnlineCollection| c.maps.into_iter().map(|m| m.id).collect::<std::collections::HashSet<i64>>();
    let before = ids_in(rhythia::collection(&state.net, &state.seen, id, false).await?);
    let todo: Vec<i64> = maps.iter().copied().filter(|m| before.contains(m) != add).collect();
    let mut reason = None;
    for (i, &map) in todo.iter().enumerate() {
        match rhythia::collection_map(&state.net, &token, id, map, add).await {
            Ok(()) => {}
            Err(e) if e.contains("already in collection") => {}
            Err(e) => reason = Some(e),
        }
        let _ = app.emit("collection-progress", serde_json::json!({ "done": i + 1, "of": todo.len() }));
    }
    let after = if todo.is_empty() { before } else { ids_in(rhythia::collection(&state.net, &state.seen, id, false).await?) };
    let done = todo.iter().filter(|m| after.contains(m) == add).count();
    let failed = todo.len() - done;
    Ok(Sent { id, added: done, already: maps.len() - todo.len(), failed, reason: if failed > 0 { reason } else { None } })
}

#[tauri::command]
async fn rhythia_sign_out(app: AppHandle) -> Result<(), String> {
    account::sign_out(&app).await;
    Ok(())
}

/// Map types from the list Atlas keeps (vps.rs): no request to the server.
#[tauri::command]
async fn map_kinds(state: State<'_, Atlas>, hashes: Vec<String>) -> Result<HashMap<String, vps::Kind>, String> {
    if hashes.len() > 20_000 {
        return Err("Too many maps at once.".into());
    }
    Ok(state.vps.kinds(&hashes).await)
}

/// The version of the Terms the player accepted (`terms.json`), shown at the first start.
#[tauri::command]
fn terms_accepted(app: AppHandle) -> Option<String> {
    let file = app.path().app_data_dir().ok()?.join("terms.json");
    store::load::<String>(&file)
}

#[tauri::command]
fn terms_accept(app: AppHandle, version: String) -> Result<(), String> {
    let version: String = version.chars().take(40).collect();
    let file = app.path().app_data_dir().map_err(|e| e.to_string())?.join("terms.json");
    store::save(&file, &version)
}

/// The player's public Rhythia number: the signed-in account, else the player the game knows.
fn me(state: &Atlas) -> Option<i64> {
    let signed = state.account.status(now_secs()).player.map(|p| p.id);
    signed.or_else(|| game::GameData::locate().ok().and_then(|g| g.player_id()))
}

/// "At which speed" for these maps ([Rhythia id or 0, stars, notes or 0]; foryou.rs): none when the server knows
/// no plays of the player.
#[tauri::command]
async fn for_you(app: AppHandle, state: State<'_, Atlas>, maps: Vec<foryou::Ask>) -> Result<Option<foryou::ForYou>, String> {
    let me = me(&state).ok_or("Atlas does not know your Rhythia account. Sign in (Settings → Account) or play once.")?;
    let file = app.path().app_data_dir().map_err(|e| e.to_string())?.join("for-you.json");
    state.for_you.ask(&state.net, &state.vps, me, &maps, now_secs(), &file).await
}

/// The players ranked near the player and their top plays (near.rs; kept 6 hours, `force` asks again). The
/// window follows the requests with `near-progress` ({done, of}).
#[tauri::command]
async fn near_group(app: AppHandle, state: State<'_, Atlas>, force: Option<bool>) -> Result<near::Group, String> {
    let me = me(&state).ok_or("Atlas does not know your Rhythia account. Sign in (Settings → Account) or play once.")?;
    let file = app.path().app_data_dir().map_err(|e| e.to_string())?.join("near.json");
    let step = |done: usize, of: usize| {
        let _ = app.emit("near-progress", serde_json::json!({ "done": done, "of": of }));
    };
    state.near.group(&state.net, &state.vps, me, force.unwrap_or(false), now_secs(), &file, &step).await
}

#[derive(Serialize)]
struct FoundSong {
    song: String,
    map: rhythia::OnlineMap,
}

/// Maps by their song keys (at most 50 at once), for "Played near your rank".
#[tauri::command]
async fn near_maps_found(state: State<'_, Atlas>, songs: Vec<String>) -> Result<Vec<FoundSong>, String> {
    if songs.len() > 50 {
        return Err("Too many maps at once.".into());
    }
    use futures_util::StreamExt;
    let have = installed();
    // Four at a time, in order (the pace of every request to Rhythia still applies).
    let found: Vec<_> = futures_util::stream::iter(songs.into_iter().map(|song| async {
        let map = rhythia::map_by_song(&state.net, &state.seen, &song).await;
        (song, map)
    }))
    .buffered(4)
    .collect()
    .await;
    Ok(found
        .into_iter()
        .filter_map(|(song, map)| {
            let mut map = map.ok().flatten()?;
            map.installed = have.contains(&map.id);
            Some(FoundSong { song, map })
        })
        .collect())
}

/// Downloads a map's song for the player when the game does not have the map; the name for the song
/// protocol (`o-<id>`).
#[tauri::command]
async fn preview_song(state: State<'_, Atlas>, id: i64) -> Result<String, String> {
    state.previews.fetch(&state.net, &state.seen, id).await?;
    Ok(format!("o-{id}"))
}

/// Asks GitHub for a newer Atlas (Settings → About, and by itself every hour: updates.rs).
#[tauri::command]
async fn update_check(app: AppHandle) -> Result<Option<updates::Found>, String> {
    updates::check(&app).await
}

/// Installs the update found (Atlas closes and starts again).
#[tauri::command]
async fn update_install(app: AppHandle) -> Result<(), String> {
    updates::install(&app).await
}

/// Keeps the list of map types current: shortly after the start, then checked every hour (the server is
/// asked once a day, or an hour after a failed try). The window redraws types when the list changed.
async fn keep_map_types(app: AppHandle) {
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
    loop {
        tick.tick().await;
        let state = app.state::<Atlas>();
        if state.vps.due(now_secs()).await && state.vps.refresh(&state.net, now_secs()).await == Ok(true) {
            let _ = app.emit("kinds-changed", ());
        }
    }
}

/// Approximate types of maps in the game the server has no type for, from their files
/// (`<sha256>.rhc` names, from the library).
#[tauri::command]
async fn basic_kinds(app: AppHandle, files: Vec<String>) -> Result<HashMap<String, basic_kinds::Basic>, String> {
    if files.len() > 20_000 {
        return Err("Too many maps at once.".into());
    }
    blocking(move || Ok(app.state::<Atlas>().basic.of(&files))).await
}

/// Opens a map, collection or player page on rhythia.com in the player's browser (where Reimagined,
/// if installed, takes over the page).
#[tauri::command]
fn open_on_rhythia(app: AppHandle, kind: String, id: i64) -> Result<(), String> {
    let path = match kind.as_str() {
        "map" => "maps",
        "collection" => "collections",
        "player" => "player",
        _ => return Err("Unknown page.".into()),
    };
    if id <= 0 {
        return Err("Unknown page.".into());
    }
    app.opener()
        .open_url(format!("https://www.rhythia.com/{path}/{id}"), None::<&str>)
        .map_err(|e| e.to_string())
}

/// Starts dragging the map files of the last Files download (only those: the window names no paths), for
/// the player to drop onto the game window, which adds them. A plain Windows drag, as from Explorer.
#[tauri::command]
fn drag_saved_maps(app: AppHandle, window: tauri::WebviewWindow) -> Result<(), String> {
    let files = install::saved_files(&app);
    if files.is_empty() {
        return Err("No map files to drag. Download some as files first.".into());
    }
    let w = window.clone();
    window
        .run_on_main_thread(move || {
            let icon = drag::Image::Raw(include_bytes!("../icons/32x32.png").to_vec());
            let _ = drag::start_drag(&w, drag::DragItem::Files(files), icon, |_, _| {}, drag::Options::default());
        })
        .map_err(|e| e.to_string())
}

/// Rhythia's map preview (its player with auto play) in a window of its own. Async: a window made in a
/// plain command blocks the window thread on Windows.
#[tauri::command]
async fn map_preview(app: AppHandle, id: i64) -> Result<(), String> {
    account::open_preview(&app, id)
}

/// Near your rank from Reimagined (memory only), by map hash.
#[tauri::command]
fn near_maps(state: State<'_, Atlas>) -> HashMap<String, bridge::Near> {
    state.bridge.near()
}

#[tauri::command]
fn bridge_status(state: State<'_, Atlas>) -> bridge::Status {
    state.bridge.status()
}

/// "Refresh connection": listens again when no port could be opened, and asks each connected browser to
/// answer now (one that does not is dropped; Reimagined connects again by itself).
#[tauri::command]
async fn bridge_check(app: AppHandle) -> bridge::Status {
    bridge::check(&app).await
}

#[tauri::command]
fn bridge_answer(state: State<'_, Atlas>, yes: bool) {
    state.bridge.answer(yes);
}

#[tauri::command]
fn bridge_forget(app: AppHandle, id: u64) {
    app.state::<Atlas>().bridge.forget(id);
    let _ = app.emit("bridge-changed", app.state::<Atlas>().bridge.status());
}

#[tauri::command]
fn take_pending_link(state: State<'_, Atlas>) -> Option<atlas_link::AtlasLink> {
    state.pending_link.lock().unwrap().take()
}

fn got_link(app: &AppHandle, raw: &str) {
    if let Some(link) = atlas_link::parse(raw) {
        show_link(app, link);
    }
}

/// Brings the window up and hands it a link (from `atlas://` or from Reimagined).
pub fn show_link(app: &AppHandle, link: atlas_link::AtlasLink) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
    *app.state::<Atlas>().pending_link.lock().unwrap() = Some(link.clone());
    let _ = app.emit("atlas-link", link);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // First: a second Atlas (e.g. opened by a link) hands its link to this one and closes.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .register_asynchronous_uri_scheme_protocol("cover", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            tauri::async_runtime::spawn(async move { responder.respond(covers::serve(app, request).await) });
        })
        .register_asynchronous_uri_scheme_protocol("song", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            tauri::async_runtime::spawn_blocking(move || responder.respond(songs::serve(&request, &app.state::<Atlas>().previews)));
        })
        .setup(|app| {
            let data = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data)?;
            // Answers per map from versions before the list of map types (vps.rs); nothing reads them now.
            let _ = std::fs::remove_file(data.join("kinds.json"));
            game::choose(store::load::<GameChoice>(&data.join("game.json")).and_then(|c| c.dir));
            let (account_tx, account_rx) = tokio::sync::mpsc::channel(4);
            app.manage(Atlas {
                net: net::Net::new()?,
                quota: quota::Quota::load(data.join("limits.json")),
                vps: vps::Vps::load(data.join("server-key.json"), data.join("map-types.json")),
                installer: install::Installer::load(data.join("downloads.json")),
                seen: rhythia::Seen::default(),
                covers: covers::Covers::new(app.path().app_cache_dir()?.join("covers")),
                mine: mine::Mine::load(data.join("collections.json")),
                // Not "backups": Windows folder names ignore case, so that is the copies' folder (Backups) below,
                // whose copies have the same names (rhythia-<time>.db) and each side would delete the other's.
                backups: data.join("game-saves"),
                backup: backup::Backup::load(data.join("backup.json"), data.join("Backups")),
                basic: basic_kinds::BasicKinds::default(),
                game_file: data.join("game.json"),
                bridge: bridge::Bridge::load(data.join("bridge.json")),
                pending_link: Mutex::new(None),
                account: account::Account::load(data.join("account.json")),
                account_tx,
                account_rx: tokio::sync::Mutex::new(account_rx),
                updates: updates::Updates::default(),
                previews: songs::Previews::new(app.path().app_cache_dir()?.join("previews")),
                near: near::Near::default(),
                for_you: foryou::Kept::default(),
            });

            tauri::async_runtime::spawn(bridge::serve(app.handle().clone()));
            tauri::async_runtime::spawn(show_waiting(app.handle().clone()));
            tauri::async_runtime::spawn(watch_game(app.handle().clone()));
            tauri::async_runtime::spawn(keep_map_types(app.handle().clone()));
            tauri::async_runtime::spawn(updates::keep_checking(app.handle().clone()));
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<Atlas>();
                let _ = state.account.check(&handle, &state.net).await;
            });

            // The installer registers atlas:// for release builds; a dev build registers itself.
            #[cfg(debug_assertions)]
            app.deep_link().register_all()?;

            let handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    got_link(&handle, url.as_str());
                }
            });
            if let Some(urls) = app.deep_link().get_current()? {
                for url in urls {
                    got_link(app.handle(), url.as_str());
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            update_check,
            preview_song,
            near_group,
            for_you,
            near_maps_found,
            backup_export,
            backup_delete,
            terms_accepted,
            terms_accept,
            update_install,
            game_folder_pick,
            game_folder_auto,
            set_zoom,
            data_info,
            clear_covers,
            open_data_folder,
            library,
            map_attempts,
            speed_tries,
            catalog,
            install_maps,
            cancel_install,
            download_pick_folder,
            open_folder,
            downloads,
            clear_downloads,
            online_collections,
            online_collection,
            map_board,
            backup_status,
            backup_set,
            game_sizes,
            backup_now,
            backup_cancel_pending,
            backup_pick_folder,
            open_backup_folder,
            backup_restore,
            game_collections,
            my_collections,
            collection_create,
            collection_rename,
            collection_delete,
            collection_add,
            collection_remove,
            collection_to_game,
            collection_want_game,
            uninstall_maps,
            quota_status,
            map_kinds,
            account_status,
            account_check,
            uninstall_info,
            uninstall_atlas,
            rhythia_sign_in,
            drag_saved_maps,
            top_players,
            my_rank,
            my_top,
            game_picture,
            map_preview,
            rhythia_sign_in_reimagined,
            rhythia_sign_out,
            rhythia_collection_create,
            rhythia_collection_edit,
            rhythia_collection_delete,
            rhythia_collection_maps,
            basic_kinds,
            open_on_rhythia,
            bridge_status,
            bridge_check,
            near_maps,
            bridge_answer,
            bridge_forget,
            take_pending_link
        ])
        .run(tauri::generate_context!())
        .expect("Atlas could not start");
}
