//! Map songs for the window's player, through Atlas' own `song` protocol:
//! - `<sha256>`: a file in the game's `cache/audio` (the name in `Maps.AudioPath`), read only;
//! - `o-<map id>`: a preview of a map the game does not have. Rhythia's website plays a song from the map
//!   file itself, and so does Atlas: it downloads the map file once (paced like every request to Rhythia,
//!   at most `PREVIEWS_PER_DAY` a day, apart from the daily download limit since nothing goes into the
//!   game), checks it, keeps only the song in its cache folder (the newest `KEEP_PREVIEWS`) and drops the
//!   rest. A `.sspm` (version 2) names where its song is; a `.rhm` is a ZIP with an `audio` entry.
//!
//! Byte ranges are served so the player can seek. Songs are untrusted files: only MP3, OGG, WAV and FLAC
//! are served, known by their first bytes.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tauri::http::{header, Request, Response, StatusCode};

use crate::game::GameData;
use crate::library::is_hash;

const MAX_SONG_BYTES: u64 = 200 * 1024 * 1024;
/// Previews of maps not in the game, downloaded at most this many a day.
pub const PREVIEWS_PER_DAY: u32 = 300;
/// Preview songs kept in the cache folder; older ones go.
const KEEP_PREVIEWS: usize = 150;

/// Songs of maps the game does not have (`o-<id>`), in Atlas' cache folder.
pub struct Previews {
    dir: PathBuf,
    /// Unix day and how many previews were downloaded on it.
    today: Mutex<(u64, u32)>,
}

impl Previews {
    pub fn new(dir: PathBuf) -> Self {
        Previews { dir, today: Mutex::new((0, 0)) }
    }

    fn path(&self, id: i64) -> PathBuf {
        self.dir.join(format!("{id}.song"))
    }

    /// The preview song of a map, downloaded first when it is not kept yet.
    pub async fn fetch(&self, net: &crate::net::Net, seen: &crate::rhythia::Seen, id: i64) -> Result<(), String> {
        if !(1..10_000_000_000).contains(&id) {
            return Err("Unknown map.".into());
        }
        if self.path(id).is_file() {
            return Ok(());
        }
        {
            let day = crate::now_secs() / 86_400;
            let mut today = self.today.lock().unwrap();
            if today.0 != day {
                *today = (day, 0);
            }
            if today.1 >= PREVIEWS_PER_DAY {
                return Err("That is all the previews for today.".into());
            }
            today.1 += 1;
        }
        let (url, _) = crate::rhythia::map_file(net, seen, id).await?;
        let ext = crate::files::ext_of(&url).ok_or("This map has no song to play.")?;
        let (bytes, sha1) = net.get_file(&url, crate::files::MAX_MAP_BYTES).await?;
        crate::files::check(&bytes, ext, sha1.as_deref())?;
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let audio = match ext {
            "sspm" => sspm_audio(&bytes).ok_or("This map has no song to play.")?.to_vec(),
            _ => {
                let tmp = self.dir.join(format!("{id}.part"));
                std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                let got = crate::files::read_entry(&tmp, "audio", &mut out, MAX_SONG_BYTES);
                let _ = std::fs::remove_file(&tmp);
                got.map_err(|_| "This map has no song to play.")?;
                out
            }
        };
        kind_of(&audio[..audio.len().min(12)]).ok_or("This map's song is in a format Atlas does not play.")?;
        std::fs::write(self.path(id), &audio).map_err(|e| e.to_string())?;
        self.trim();
        Ok(())
    }

    fn trim(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        let mut songs: Vec<(std::time::SystemTime, PathBuf)> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "song"))
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        songs.sort_by_key(|s| std::cmp::Reverse(s.0));
        for (_, old) in songs.into_iter().skip(KEEP_PREVIEWS) {
            let _ = std::fs::remove_file(old);
        }
    }
}

/// The song inside a version 2 `.sspm`: its offset and length are in the header (0x40), after a flag
/// that it has one (0x2D).
fn sspm_audio(bytes: &[u8]) -> Option<&[u8]> {
    if !bytes.starts_with(b"SS+m") || bytes.get(4..6)? != [2, 0] || *bytes.get(0x2D)? != 1 {
        return None;
    }
    let u64_at = |i: usize| Some(u64::from_le_bytes(bytes.get(i..i + 8)?.try_into().ok()?));
    let (at, len) = (usize::try_from(u64_at(0x40)?).ok()?, usize::try_from(u64_at(0x48)?).ok()?);
    bytes.get(at..at.checked_add(len)?).filter(|a| !a.is_empty())
}
/// One answer carries at most this much; the player asks for the rest.
const MAX_PART: u64 = 4 * 1024 * 1024;

pub fn serve(request: &Request<Vec<u8>>, previews: &Previews) -> Response<Vec<u8>> {
    let name = request.uri().path().trim_start_matches('/');
    let range = request.headers().get(header::RANGE).and_then(|v| v.to_str().ok()).map(str::to_string);
    let preview = name.strip_prefix("o-").and_then(|n| n.parse::<i64>().ok()).filter(|n| *n > 0);
    let path = match preview {
        Some(id) => Some(previews.path(id)),
        None => is_hash(name).then(|| GameData::locate().ok()?.file(&format!("cache/audio/{}", name.to_ascii_lowercase())).ok()).flatten(),
    };
    let found = path.and_then(|path| part(&path, range.as_deref()));
    match found {
        Some(p) => {
            let mut answer = Response::builder()
                .header(header::CONTENT_TYPE, p.mime)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CACHE_CONTROL, "no-store");
            if p.partial {
                answer = answer.status(StatusCode::PARTIAL_CONTENT).header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", p.start, p.start + p.bytes.len() as u64 - 1, p.size),
                );
            }
            answer.body(p.bytes).unwrap_or_default()
        }
        None => Response::builder().status(StatusCode::NOT_FOUND).body(Vec::new()).unwrap_or_default(),
    }
}

struct Part {
    bytes: Vec<u8>,
    start: u64,
    size: u64,
    partial: bool,
    mime: &'static str,
}

fn part(path: &Path, range: Option<&str>) -> Option<Part> {
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    if size == 0 || size > MAX_SONG_BYTES {
        return None;
    }
    let mut head = [0u8; 12];
    let got = file.read(&mut head).ok()?;
    let mime = kind_of(&head[..got])?;
    let (start, end) = match range.and_then(|r| parse_range(r, size)) {
        Some(r) => r,
        None if range.is_some() => return None,
        None => (0, size - 1),
    };
    let end = end.min(start + MAX_PART - 1);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = vec![0u8; (end - start + 1) as usize];
    file.read_exact(&mut bytes).ok()?;
    Some(Part { bytes, start, size, partial: range.is_some() || end + 1 < size, mime })
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` (the last n bytes), inside a file of `size` bytes.
fn parse_range(r: &str, size: u64) -> Option<(u64, u64)> {
    let (a, b) = r.trim().strip_prefix("bytes=")?.split(',').next()?.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        (size.checked_sub(n.min(size))?, size - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() { size - 1 } else { b.parse::<u64>().ok()?.min(size - 1) };
        (start, end)
    };
    (start <= end && end < size).then_some((start, end))
}

fn kind_of(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"ID3") || (head.len() > 1 && head[0] == 0xff && head[1] & 0xe0 == 0xe0) {
        Some("audio/mpeg")
    } else if head.starts_with(b"OggS") {
        Some("audio/ogg")
    } else if head.starts_with(b"fLaC") {
        Some("audio/flac")
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WAVE" {
        Some("audio/wav")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range("bytes=90-500", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=5-1", 100), None);
        assert_eq!(parse_range("items=0-1", 100), None);
    }

    #[test]
    fn only_sound_files() {
        assert_eq!(kind_of(b"ID3\x04"), Some("audio/mpeg"));
        assert_eq!(kind_of(&[0xff, 0xfb, 0x90, 0x40]), Some("audio/mpeg"));
        assert_eq!(kind_of(b"OggS\0\x02"), Some("audio/ogg"));
        // A version 2 .sspm names where its song is.
        let mut map = vec![0u8; 0x60];
        map[..4].copy_from_slice(b"SS+m");
        map[4] = 2;
        map[0x2D] = 1;
        map[0x40..0x48].copy_from_slice(&0x60u64.to_le_bytes());
        map[0x48..0x50].copy_from_slice(&4u64.to_le_bytes());
        map.extend_from_slice(b"ID3\x04");
        assert_eq!(sspm_audio(&map), Some(&b"ID3\x04"[..]));
        map[0x48..0x50].copy_from_slice(&40u64.to_le_bytes());
        assert_eq!(sspm_audio(&map), None, "a length past the end is refused");
        assert_eq!(kind_of(b"MZ\x90\0"), None);
        assert_eq!(kind_of(b"<html>"), None);
    }
}
