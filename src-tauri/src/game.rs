//! Where the game keeps its data, and the one gate to its files. Atlas never touches the game's
//! install folder (exe, dll) and never reads `userconfig.json` (session token, login, password).

use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;

/// Files in the data folder Atlas must never open, whatever asks for them.
const NEVER_READ: [&str; 1] = ["userconfig.json"];

/// A data folder the player picked in Settings, used before the usual places.
static CHOSEN: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Sets (or with `None` clears) the folder the player picked.
pub fn choose(dir: Option<PathBuf>) {
    *CHOSEN.write().unwrap() = dir;
}

pub fn chosen() -> Option<PathBuf> {
    CHOSEN.read().unwrap().clone()
}

/// The game's data folder at or right under `dir` (people often pick the folder above it).
pub fn data_folder_in(dir: &Path) -> Option<PathBuf> {
    [dir.to_path_buf(), dir.join("CapoRhythia")].into_iter().find(|d| d.join("rhythia.db").is_file())
}

pub struct GameData {
    dir: PathBuf,
}

impl GameData {
    /// The folder picked in Settings; else `%APPDATA%\CapoRhythia`, or the folder its
    /// `framework.json` names in `DataPath`.
    pub fn locate() -> Result<GameData, String> {
        if let Some(dir) = chosen().filter(|d| d.join("rhythia.db").is_file()) {
            return Ok(GameData { dir });
        }
        let appdata = std::env::var_os("APPDATA").ok_or("Windows did not say where app data is.")?;
        let default = PathBuf::from(appdata).join("CapoRhythia");
        let dir = data_path_from(&default.join("framework.json")).unwrap_or(default);
        if !dir.join("rhythia.db").is_file() {
            return Err("Rhythia's data was not found. Start the game once, or pick its folder in Settings.".into());
        }
        Ok(GameData { dir })
    }

    /// A data folder as it is, for tests (no global choice involved).
    #[cfg(test)]
    pub fn at(dir: PathBuf) -> GameData {
        GameData { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A file inside the data folder, for reading. Refuses `..`, absolute paths and the
    /// files in `NEVER_READ`, at any depth and in any letter case.
    pub fn file(&self, relative: &str) -> Result<PathBuf, String> {
        let rel = Path::new(relative);
        let plain = rel.components().all(|c| matches!(c, Component::Normal(_)));
        let forbidden = rel.components().any(|c| {
            c.as_os_str().to_str().is_some_and(|s| NEVER_READ.iter().any(|n| s.eq_ignore_ascii_case(n)))
        });
        if relative.is_empty() || !plain || forbidden {
            return Err("Blocked: Atlas does not open that file.".into());
        }
        Ok(self.dir.join(rel))
    }

    pub fn database(&self) -> PathBuf {
        self.file("rhythia.db").expect("a fixed, allowed name")
    }

    /// When the game was started, from the name of its newest runtime log (`runtime-<unix seconds>.log`).
    pub fn started(&self) -> Option<u64> {
        let logs = self.file("logs").ok()?;
        std::fs::read_dir(logs)
            .ok()?
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_prefix("runtime-")?.strip_suffix(".log")?.parse::<u64>().ok())
            .filter(|n| (1_500_000_000..10_000_000_000).contains(n))
            .max()
    }

    /// The player's Rhythia user id, from the game's newest online log (`Profile loaded:
    /// userId=<n>`). Only that number is taken from the log; nothing else in it is kept.
    pub fn player_id(&self) -> Option<i64> {
        const MARK: &str = "Profile loaded: userId=";
        const TAIL: u64 = 1024 * 1024;
        let logs = self.file("logs").ok()?;
        let newest = std::fs::read_dir(logs)
            .ok()?
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("online-") && n.ends_with(".log")))
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .max()?
            .1;
        let mut file = std::fs::File::open(newest).ok()?;
        let len = file.metadata().ok()?.len();
        use std::io::{Read, Seek, SeekFrom};
        file.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
        let mut tail = Vec::new();
        file.take(TAIL).read_to_end(&mut tail).ok()?;
        let text = String::from_utf8_lossy(&tail);
        let at = text.rfind(MARK)? + MARK.len();
        let digits: String = text[at..].chars().take_while(char::is_ascii_digit).take(12).collect();
        digits.parse().ok().filter(|n| *n > 0)
    }
}

/// `{"DataPath":{"Value":"D:\\Games\\Rhythia"}}`: an existing folder, or none when empty or odd.
fn data_path_from(framework: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(framework).ok()?;
    if bytes.len() > 256 * 1024 {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let value = json.get("DataPath")?.get("Value")?.as_str()?.trim();
    let path = PathBuf::from(value);
    (!value.is_empty() && path.is_absolute() && path.is_dir()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_userconfig() {
        let g = GameData { dir: PathBuf::from(r"C:\data") };
        assert!(g.file("rhythia.db").is_ok());
        assert!(g.file("logs/runtime-1.log").is_ok());
        assert!(g.file("userconfig.json").is_err());
        assert!(g.file("UserConfig.JSON").is_err());
        assert!(g.file("x/userconfig.json").is_err());
        assert!(g.file("../rhythia/userconfig.json").is_err());
        assert!(g.file(r"C:\other\file").is_err());
        assert!(g.file("").is_err());
    }
}
