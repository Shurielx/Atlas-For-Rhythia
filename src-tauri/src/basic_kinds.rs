//! Map types worked out by Atlas from the game's own map files, for maps the Atlas server has no type
//! for (Unranked, Approved, local). The same public rules as Reimagined's "map types without online
//! history" (`extension/shared/map-kinds-basic.js`): the map is read in pieces of 16 moves; Jump or
//! Stream needs 55% of the notes and a lead of 25 points, else Varied; Quantum is 20% of the notes in
//! off-grid pieces. Tech is never told here (only the server can). Always shown as approximate.
//!
//! The game keeps maps in `cache/maps/<sha256>.rhc`: `RHYCACHE`, then u32 version, u32 JSON length,
//! u32 note count, u32 note bytes, the JSON (map details), then the notes: u32 ms, f32 x, f32 y (the
//! grid is 0..2). Version 1: 12 bytes a note. Version 2 (maps with their own hit sounds): 103 bits a
//! note, packed with no gaps, lowest bit first; the 7 bits after x and y name the hit sound. Other
//! versions are not read.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;

use crate::game::GameData;
use crate::library::is_hash;

const MAX_FILE: u64 = 64 * 1024 * 1024;
const MAX_NOTES: usize = 500_000;

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Basic {
    pub kind: &'static str,
    pub quantum: bool,
    pub approximate: bool,
}

/// Types already worked out (by map file name); `None`: the file could not be read.
#[derive(Default)]
pub struct BasicKinds(Mutex<HashMap<String, Option<Basic>>>);

impl BasicKinds {
    /// Types for these map files (`<sha256>.rhc`), read from the game's `cache/maps`.
    pub fn of(&self, files: &[String]) -> HashMap<String, Basic> {
        let game = GameData::locate().ok();
        let mut out = HashMap::new();
        for name in files {
            let stem = name.strip_suffix(".rhc").unwrap_or("");
            if !is_hash(stem) || name.bytes().any(|b| b.is_ascii_uppercase()) {
                continue;
            }
            let known = self.0.lock().unwrap().get(name).cloned();
            let found = match known {
                Some(k) => k,
                None => {
                    let k = game.as_ref().and_then(|g| g.file(&format!("cache/maps/{name}")).ok()).and_then(|p| read(&p));
                    self.0.lock().unwrap().insert(name.clone(), k.clone());
                    k
                }
            };
            if let Some(k) = found {
                out.insert(name.clone(), k);
            }
        }
        out
    }
}

fn read(path: &std::path::Path) -> Option<Basic> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE {
        return None;
    }
    let notes = notes_of(&std::fs::read(path).ok()?)?;
    let a = analyse(&notes);
    Some(Basic { kind: kind(&a)?, quantum: a.notes > 0 && a.quantum >= QUANTUM, approximate: true })
}

/// Bits of one note in a version 2 `.rhc`.
const V2_BITS: usize = 103;

/// The notes ([ms, x, y], by time) of a version 1 or 2 `.rhc` file.
pub fn notes_of(b: &[u8]) -> Option<Vec<[f64; 3]>> {
    let u32_at = |o: usize| -> Option<usize> { Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?) as usize) };
    if b.get(..8)? != b"RHYCACHE" {
        return None;
    }
    let (version, json, count, bytes) = (u32_at(8)?, u32_at(12)?, u32_at(16)?, u32_at(20)?);
    let size = match version {
        1 => count * 12,
        2 => (count * V2_BITS).div_ceil(8),
        _ => return None,
    };
    if count > MAX_NOTES || bytes != size || 24 + json + bytes > b.len() {
        return None;
    }
    let body = &b[24 + json..24 + json + bytes];
    // `width` bits (at most 32) from bit `at`, lowest bit first.
    let bits = |at: usize, width: usize| -> Option<u32> {
        let mut v: u64 = 0;
        for k in 0..(width + at % 8).div_ceil(8) {
            v |= (*body.get(at / 8 + k)? as u64) << (8 * k);
        }
        Some(((v >> (at % 8)) & ((1u64 << width) - 1)) as u32)
    };
    let mut notes = Vec::with_capacity(count);
    for i in 0..count {
        let at = if version == 1 { i * 96 } else { i * V2_BITS };
        let (ms, x, y) = (bits(at, 32)?, f32::from_bits(bits(at + 32, 32)?) as f64, f32::from_bits(bits(at + 64, 32)?) as f64);
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        notes.push([ms as f64, x, y]);
    }
    notes.sort_by(|a, b| a[0].total_cmp(&b[0]));
    Some(notes)
}

const LEVEL_SHARE: f64 = 0.03;
const PIECE: usize = 16;
const PIECE_MIN: usize = 6;
const FAR: f64 = 1.3;
const UNEVEN: f64 = 0.1;
const OFF_PIECE: f64 = 0.5;
const SLOW_NPS: f64 = 5.0;
const MAIN: f64 = 0.55;
const LEAD: f64 = 0.25;
const UNEVEN_LEADS: f64 = 0.4;
const QUANTUM: f64 = 0.2;

fn r2(v: f64) -> f64 {
    // As JavaScript's Math.round: halves go up.
    (v * 100.0 + 0.5).floor() / 100.0
}

fn median(mut a: Vec<f64>) -> f64 {
    if a.is_empty() {
        return 0.0;
    }
    a.sort_by(f64::total_cmp);
    a[a.len() >> 1]
}

/// The three positions per axis the map uses most (with at least 3% of the notes each).
fn levels(notes: &[[f64; 3]], k: usize) -> Vec<f64> {
    let mut count: Vec<(f64, usize)> = Vec::new();
    for n in notes {
        let v = r2(n[k]);
        match count.iter_mut().find(|(x, _)| *x == v) {
            Some(c) => c.1 += 1,
            None => count.push((v, 1)),
        }
    }
    let min = notes.len() as f64 * LEVEL_SHARE;
    let mut kept: Vec<(f64, usize)> = count.into_iter().filter(|(_, c)| *c as f64 >= min).collect();
    kept.sort_by_key(|k| std::cmp::Reverse(k.1)); // stable, as JavaScript's sort
    kept.into_iter().take(3).map(|(v, _)| v).collect()
}

fn spacing(xs: &[f64], ys: &[f64]) -> f64 {
    let mut gaps = Vec::new();
    for set in [xs, ys] {
        let mut v = set.to_vec();
        v.sort_by(f64::total_cmp);
        for i in 1..v.len() {
            if v[i] - v[i - 1] >= 0.5 {
                gaps.push(v[i] - v[i - 1]);
            }
        }
    }
    if gaps.is_empty() {
        return 1.0;
    }
    gaps.sort_by(f64::total_cmp);
    let cell = gaps[gaps.len() >> 1];
    if (0.9..=1.2).contains(&cell) { cell } else { 1.0 }
}

#[derive(Debug)]
pub struct Analysis {
    pub notes: usize,
    /// Shares of Stream, Jump and Uneven.
    pub mix: [f64; 3],
    pub quantum: f64,
}

pub fn analyse(notes: &[[f64; 3]]) -> Analysis {
    if notes.len() < 2 {
        return Analysis { notes: notes.len(), mix: [0.0; 3], quantum: 0.0 };
    }
    struct Ev { t: f64, x: f64, y: f64, n: usize }
    let mut ev: Vec<Ev> = Vec::new();
    for n in notes {
        match ev.last_mut() {
            Some(last) if n[0] - last.t < 2.0 => last.n += 1,
            _ => ev.push(Ev { t: n[0], x: n[1], y: n[2], n: 1 }),
        }
    }
    let (xs, ys) = (levels(notes, 1), levels(notes, 2));
    let cell = spacing(&xs, &ys);
    struct Step { d: f64, v: f64, off: bool, n: usize }
    let step: Vec<Step> = (1..ev.len())
        .map(|i| {
            let (p, e) = (&ev[i - 1], &ev[i]);
            let d = (e.x - p.x).hypot(e.y - p.y) / cell;
            Step { d, v: d / (e.t - p.t).max(10.0), off: !xs.contains(&r2(e.x)) || !ys.contains(&r2(e.y)), n: e.n }
        })
        .collect();
    let (mut mix, mut all) = ([0.0f64; 3], [0.0f64; 3]);
    let (mut total, mut fast, mut off_fast, mut off_all) = (0.0, 0.0, 0.0, 0.0);
    let mut i = 0;
    while i < step.len() {
        let mut j = step.len().min(i + PIECE);
        if step.len() - j < PIECE_MIN {
            j = step.len();
        }
        let c = &step[i..j];
        let dist = median(c.iter().filter(|s| s.d > 0.3).map(|s| s.d).collect());
        let speed: Vec<f64> = (1..c.len()).filter(|&k| c[k].d > 0.3 && c[k - 1].d > 0.3).map(|k| (c[k].v / c[k - 1].v).ln().abs()).collect();
        let kind = if dist >= FAR { 1 } else if median(speed) >= UNEVEN { 2 } else { 0 };
        let n = c.iter().map(|s| s.n).sum::<usize>() as f64;
        let off = c.iter().filter(|s| s.off).count() as f64 / c.len() as f64 >= OFF_PIECE;
        let slow = (j - i) as f64 / ((ev[j].t - ev[i].t) / 1000.0).max(0.001) < SLOW_NPS;
        all[kind] += n;
        total += n;
        if off {
            off_all += n;
        }
        if !slow {
            mix[kind] += n;
            fast += n;
            if off {
                off_fast += n;
            }
        }
        i = j;
    }
    let use_all = fast < total * 0.3;
    let counted = if use_all { total } else { fast };
    for k in 0..3 {
        mix[k] = (if use_all { all[k] } else { mix[k] }) / counted.max(1.0);
    }
    Analysis { notes: notes.len(), mix, quantum: (if use_all { off_all } else { off_fast }) / counted.max(1.0) }
}

pub fn kind(a: &Analysis) -> Option<&'static str> {
    if a.notes == 0 {
        return None;
    }
    // Jump, Stream, Uneven, largest first (ties keep this order, as in Reimagined).
    let share = |k: usize| a.mix[[1, 0, 2][k]];
    let mut order = [0usize, 1, 2];
    order.sort_by(|x, y| share(*y).total_cmp(&share(*x)));
    let name = ["jump", "stream", "uneven"];
    if order[0] == 2 && share(2) >= UNEVEN_LEADS {
        return Some("varied");
    }
    Some(if order[0] != 2 && share(order[0]) >= MAIN && share(order[0]) - share(order[1]) >= LEAD { name[order[0]] } else { "varied" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(notes: &[(u32, f32, f32)]) -> Vec<u8> {
        let json = b"{}";
        let mut b = b"RHYCACHE".to_vec();
        for n in [1u32, json.len() as u32, notes.len() as u32, notes.len() as u32 * 12] {
            b.extend(n.to_le_bytes());
        }
        b.extend(json);
        for (t, x, y) in notes {
            b.extend(t.to_le_bytes());
            b.extend(x.to_le_bytes());
            b.extend(y.to_le_bytes());
        }
        b
    }

    #[test]
    fn jumps_and_streams() {
        // Corner to corner every 150 ms: jumps.
        let jumps: Vec<_> = (0..200).map(|i| (1000 + i * 150, if i % 2 == 0 { 0.0 } else { 2.0 }, if i % 2 == 0 { 0.0 } else { 2.0 })).collect();
        let a = analyse(&notes_of(&file(&jumps)).unwrap());
        assert_eq!(kind(&a), Some("jump"));
        // Neighbouring cells, evenly every 100 ms: a stream.
        let stream: Vec<_> = (0..200).map(|i| (1000 + i * 100, (i % 2) as f32, 1.0)).collect();
        let a = analyse(&notes_of(&file(&stream)).unwrap());
        assert_eq!(kind(&a), Some("stream"));
        assert!(a.quantum < QUANTUM);
    }

    /// A version 2 file: 103 bits a note, lowest bit first, hit sound 1 after each note.
    fn file_v2(notes: &[(u32, f32, f32)]) -> Vec<u8> {
        let json = b"{}";
        let bytes = (notes.len() * V2_BITS).div_ceil(8);
        let mut b = b"RHYCACHE".to_vec();
        for n in [2u32, json.len() as u32, notes.len() as u32, bytes as u32] {
            b.extend(n.to_le_bytes());
        }
        b.extend(json);
        let mut body = vec![0u8; bytes];
        let mut put = |at: usize, width: usize, v: u64| {
            for k in 0..width {
                if v >> k & 1 == 1 {
                    body[(at + k) / 8] |= 1 << ((at + k) % 8);
                }
            }
        };
        for (i, (t, x, y)) in notes.iter().enumerate() {
            let at = i * V2_BITS;
            put(at, 32, *t as u64);
            put(at + 32, 32, x.to_bits() as u64);
            put(at + 64, 32, y.to_bits() as u64);
            put(at + 96, 7, 1);
        }
        b.extend(body);
        b
    }

    #[test]
    fn reads_version_2() {
        let notes = [(247, 2.0, 0.0), (326, 2.0, 1.0), (445, 0.0, 2.0), (563, 1.5, 0.25)];
        let got = notes_of(&file_v2(&notes)).unwrap();
        assert_eq!(got, notes.iter().map(|(t, x, y)| [*t as f64, *x as f64, *y as f64]).collect::<Vec<_>>());
        let jumps: Vec<_> = (0..200).map(|i| (1000 + i * 150, if i % 2 == 0 { 0.0 } else { 2.0 }, if i % 2 == 0 { 0.0 } else { 2.0 })).collect();
        assert_eq!(kind(&analyse(&notes_of(&file_v2(&jumps)).unwrap())), Some("jump"));
        let mut cut = file_v2(&notes);
        cut.pop();
        assert!(notes_of(&cut).is_none());
    }

    #[test]
    fn refuses_other_files() {
        assert!(notes_of(b"RHYCACHE\x03\0\0\0").is_none());
        assert!(notes_of(b"PK\x03\x04").is_none());
        let mut cut = file(&[(1, 0.0, 0.0), (2, 1.0, 1.0)]);
        cut.truncate(cut.len() - 3);
        assert!(notes_of(&cut).is_none());
    }

    /// `cargo test print_game_kinds -- --ignored --nocapture`: every map in the game's cache with its
    /// type, to compare with Reimagined's own code (tools/compare_kinds.mjs).
    #[test]
    #[ignore]
    fn print_game_kinds() {
        let dir = GameData::locate().unwrap().file("cache/maps").unwrap();
        let mut out = serde_json::Map::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if let Some(k) = read(&e.path()) {
                out.insert(e.file_name().to_string_lossy().to_string(), serde_json::json!([k.kind, k.quantum]));
            }
        }
        println!("KINDS {}", serde_json::Value::Object(out));
    }
}
