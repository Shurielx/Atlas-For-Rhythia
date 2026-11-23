//! Hard limit on map downloads: 3000 maps in 24 hours, counted from the first download of the
//! window, for every way of getting maps (to the game, ZIP, files) together. A constant, not a
//! setting (see TERMS.md). Kept in `limits.json` in Atlas' data folder.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

pub const MAPS_PER_WINDOW: u32 = 3000;
pub const WINDOW_SECS: u64 = 24 * 3600;

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Saved {
    /// Unix seconds of the first download in this window; none until the first one.
    window_start: Option<u64>,
    used: u32,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub used: u32,
    pub limit: u32,
    pub left: u32,
    /// Unix seconds when the window ends; none while no window is open.
    pub resets_at: Option<u64>,
}

pub struct Quota {
    file: Option<PathBuf>,
    saved: Mutex<Saved>,
}

impl Quota {
    pub fn load(file: PathBuf) -> Self {
        // A missing or broken file starts a fresh window: the limit protects the player,
        // and deleting the file was always possible anyway.
        let saved = crate::store::load::<Saved>(&file)
            .map(|s| Saved { used: s.used.min(MAPS_PER_WINDOW), ..s })
            .unwrap_or_default();
        Quota { file: Some(file), saved: Mutex::new(saved) }
    }

    #[cfg(test)]
    fn in_memory() -> Self {
        Quota { file: None, saved: Mutex::new(Saved::default()) }
    }

    pub fn status(&self, now: u64) -> Status {
        let mut saved = self.saved.lock().unwrap();
        roll(&mut saved, now);
        status_of(&saved)
    }

    /// Reserves `n` maps, all or none. On refusal gives the current status back.
    pub fn take(&self, n: u32, now: u64) -> Result<Status, Status> {
        let mut saved = self.saved.lock().unwrap();
        roll(&mut saved, now);
        if n == 0 {
            return Ok(status_of(&saved));
        }
        if saved.used.saturating_add(n) > MAPS_PER_WINDOW {
            return Err(status_of(&saved));
        }
        saved.window_start.get_or_insert(now);
        saved.used += n;
        self.write(&saved);
        Ok(status_of(&saved))
    }

    /// Returns maps reserved but not downloaded (cancelled or failed queue).
    pub fn give_back(&self, n: u32, now: u64) {
        let mut saved = self.saved.lock().unwrap();
        roll(&mut saved, now);
        saved.used = saved.used.saturating_sub(n);
        self.write(&saved);
    }

    fn write(&self, saved: &Saved) {
        if let Some(file) = &self.file {
            let _ = crate::store::save(file, saved);
        }
    }
}

/// Ends the window once 24 hours have passed. A start in the future (clock moved back)
/// becomes now, so the window cannot be stretched past 24 hours.
fn roll(saved: &mut Saved, now: u64) {
    if let Some(start) = saved.window_start {
        if start > now {
            saved.window_start = Some(now);
        } else if now - start >= WINDOW_SECS {
            *saved = Saved::default();
        }
    }
}

fn status_of(saved: &Saved) -> Status {
    Status {
        used: saved.used,
        limit: MAPS_PER_WINDOW,
        left: MAPS_PER_WINDOW - saved.used,
        resets_at: saved.window_start.map(|s| s + WINDOW_SECS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_starts_at_first_download() {
        let q = Quota::in_memory();
        assert_eq!(q.status(100).resets_at, None);
        assert_eq!(q.take(10, 100).unwrap().resets_at, Some(100 + WINDOW_SECS));
        assert_eq!(q.take(10, 5000).unwrap().resets_at, Some(100 + WINDOW_SECS));
    }

    #[test]
    fn all_or_nothing_and_reset() {
        let q = Quota::in_memory();
        assert!(q.take(2999, 0).is_ok());
        assert_eq!(q.take(2, 10).unwrap_err().left, 1);
        assert!(q.take(1, 10).is_ok());
        assert!(q.take(1, WINDOW_SECS - 1).is_err());
        let after = q.take(1, WINDOW_SECS).unwrap();
        assert_eq!((after.used, after.resets_at), (1, Some(2 * WINDOW_SECS)));
    }

    #[test]
    fn give_back_and_clock_back() {
        let q = Quota::in_memory();
        q.take(100, 1000).unwrap();
        q.give_back(40, 1001);
        assert_eq!(q.status(1001).used, 60);
        // Clock moved back: the window does not stretch, it restarts its end from now.
        assert_eq!(q.status(10).resets_at, Some(10 + WINDOW_SECS));
    }
}
