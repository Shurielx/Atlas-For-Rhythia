//! Updates: Atlas asks GitHub for `latest.json` of the newest published release of
//! `Shurielx/Atlas-For-Rhythia` shortly after the start and every hour (Tauri's updater; the address
//! and the public key are in `tauri.conf.json`). Only the request itself goes out: GitHub sees the
//! computer's address and Atlas' version, nothing about the player. A newer version is offered in the
//! window ("update-found"); installing downloads the installer, checks its signature against the
//! public key (a release not signed with Shuriel's key is refused) and runs it, which closes Atlas.
//!
//! This is the one connection outside `net.rs`: the updater has its own client and talks only to the
//! address in the config and the files the signed `latest.json` names.

use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::Atlas;

const FIRST_CHECK: Duration = Duration::from_secs(20);
const CHECK_EVERY: Duration = Duration::from_secs(3600);

#[derive(Default)]
pub struct Updates {
    found: Mutex<Option<Update>>,
}

/// A newer Atlas, for the window.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    pub version: String,
    pub notes: String,
}

fn clip(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control() || *c == '\n').take(max).collect()
}

/// Asks for the newest release; a newer one is kept for `install` and returned.
pub async fn check(app: &AppHandle) -> Result<Option<Found>, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let found = updater.check().await.map_err(|_| "Atlas could not check for updates. Try again later.".to_string())?;
    let info = found.as_ref().map(|u| Found { version: clip(&u.version, 30), notes: clip(u.body.as_deref().unwrap_or(""), 2000) });
    *app.state::<Atlas>().updates.found.lock().unwrap() = found;
    Ok(info)
}

/// Downloads the update found, checks its signature and runs the installer (Atlas closes).
pub async fn install(app: &AppHandle) -> Result<(), String> {
    let atlas = app.state::<Atlas>();
    if atlas.installer.busy() {
        return Err("Atlas is downloading maps. Update when it is done.".into());
    }
    let update = atlas.updates.found.lock().unwrap().clone().ok_or("No update to install. Check again.")?;
    update.download_and_install(|_, _| {}, || {}).await.map_err(|_| "The update could not be installed. Try again later.".to_string())?;
    app.restart();
}

/// Shortly after the start, then every hour; tells the window about a newer version once.
pub async fn keep_checking(app: AppHandle) {
    tokio::time::sleep(FIRST_CHECK).await;
    let mut told = String::new();
    loop {
        if let Ok(Some(found)) = check(&app).await {
            if found.version != told {
                told = found.version.clone();
                let _ = app.emit("update-found", found);
            }
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}
