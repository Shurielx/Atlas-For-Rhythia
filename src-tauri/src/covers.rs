//! Map covers for the window, through Atlas' own `cover` protocol (the window still has no
//! network: CSP `img-src` allows only this protocol):
//! - `l-<sha256>`: a cover from the game's `cache/covers`, shrunk to a small JPEG (game covers
//!   can be 10 MB and more);
//! - `o-<map id>`: Rhythia's small preview of a map Atlas has seen in an answer (`Seen`),
//!   downloaded once through `Net` (paced like every request to Rhythia).
//!
//! Both are kept in Atlas' cache folder. Covers are untrusted files: the decoder has size
//! limits and only PNG, JPEG and WebP are accepted.

use std::path::{Path, PathBuf};

use image::ImageReader;
use tauri::http::{header, Request, Response, StatusCode};
use tauri::{AppHandle, Manager};
use tokio::sync::Semaphore;

use crate::game::GameData;
use crate::library::is_hash;
use crate::Atlas;

const THUMB_PX: u32 = 256;
const MAX_LOCAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ONLINE_BYTES: usize = 3 * 1024 * 1024;
const MAX_PIXELS_SIDE: u32 = 12_000;
/// Cached covers kept; past this the oldest half goes at start.
const MAX_CACHED: usize = 8000;

pub struct Covers {
    dir: PathBuf,
    /// Shrinking a large PNG is heavy: two at a time.
    work: Semaphore,
}

impl Covers {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        trim(&dir);
        Covers { dir, work: Semaphore::new(2) }
    }
}

/// Files and bytes in the cover cache (Settings → Data).
pub fn size(covers: &Covers) -> (u64, u64) {
    let Ok(entries) = std::fs::read_dir(&covers.dir) else { return (0, 0) };
    entries.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).fold((0, 0), |(n, b), m| (n + 1, b + m.len()))
}

/// Deletes every cached cover; they come back when shown again.
pub fn clear(covers: &Covers) {
    if let Ok(entries) = std::fs::read_dir(&covers.dir) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

pub async fn serve(app: AppHandle, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let name = request.uri().path().trim_start_matches('/').to_string();
    let found = if let Some(sha) = name.strip_prefix("l-").filter(|s| is_hash(s) && !s.bytes().any(|b| b.is_ascii_uppercase())) {
        local(&app, sha).await
    } else if let Some(id) = name.strip_prefix("o-").and_then(|s| s.parse::<i64>().ok()).filter(|n| *n > 0 && *n < 10_000_000_000) {
        online(&app, id).await
    } else {
        None
    };
    match found {
        Some((bytes, mime)) => Response::builder()
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, "max-age=604800")
            .body(bytes)
            .unwrap_or_default(),
        None => Response::builder().status(StatusCode::NOT_FOUND).body(Vec::new()).unwrap_or_default(),
    }
}

async fn local(app: &AppHandle, sha: &str) -> Option<(Vec<u8>, &'static str)> {
    let atlas = app.state::<Atlas>();
    let cached = atlas.covers.dir.join(format!("l-{sha}.jpg"));
    if let Ok(bytes) = tokio::fs::read(&cached).await {
        return Some((bytes, "image/jpeg"));
    }
    let source = GameData::locate().ok()?.file(&format!("cache/covers/{sha}")).ok()?;
    let _permit = atlas.covers.work.acquire().await.ok()?;
    let bytes = tauri::async_runtime::spawn_blocking(move || shrink(&source)).await.ok()??;
    let _ = tokio::fs::write(&cached, &bytes).await;
    Some((bytes, "image/jpeg"))
}

async fn online(app: &AppHandle, id: i64) -> Option<(Vec<u8>, &'static str)> {
    let atlas = app.state::<Atlas>();
    let cached = atlas.covers.dir.join(format!("o-{id}.img"));
    if let Ok(bytes) = tokio::fs::read(&cached).await {
        return kind_of(&bytes).map(|mime| (bytes, mime));
    }
    let url = atlas.seen.get(id)?.cover?;
    let bytes = atlas.net.get_image(&url, MAX_ONLINE_BYTES).await.ok()?;
    let mime = kind_of(&bytes)?;
    let _ = tokio::fs::write(&cached, &bytes).await;
    Some((bytes, mime))
}

/// PNG, JPEG or WebP by their first bytes; anything else is refused.
fn kind_of(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn shrink(source: &Path) -> Option<Vec<u8>> {
    if std::fs::metadata(source).ok()?.len() > MAX_LOCAL_BYTES {
        return None;
    }
    let bytes = std::fs::read(source).ok()?;
    kind_of(&bytes)?;
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_PIXELS_SIDE);
    limits.max_image_height = Some(MAX_PIXELS_SIDE);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let thumb = reader.decode().ok()?.thumbnail(THUMB_PX, THUMB_PX).into_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(&thumb).ok()?;
    Some(out)
}

fn trim(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<_> = entries
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if files.len() <= MAX_CACHED {
        return;
    }
    files.sort();
    for (_, path) in files.iter().take(files.len() / 2) {
        let _ = std::fs::remove_file(path);
    }
}
