//! Atlas' own collections, kept in `collections.json` in Atlas' data folder. A collection can be
//! made from scratch, copied from Rhythia or from the game, and shown in the game as a local game
//! collection (`game_write.rs`).

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::library::{clip, is_hash};

const MAX_COLLECTIONS: usize = 500;
const MAX_MAPS: usize = 10_000;

/// A map in a collection: its online id when it has one, its hash for local maps.
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub online_id: Option<i64>,
    pub hash: Option<String>,
    pub title: String,
    #[serde(default)]
    pub mapper: String,
}

impl Item {
    fn clean(self) -> Option<Item> {
        let item = Item {
            online_id: self.online_id.filter(|n| *n > 0 && *n < 10_000_000_000),
            hash: self.hash.map(|h| h.to_ascii_lowercase()).filter(|h| is_hash(h)),
            title: clip(&self.title, 300),
            mapper: clip(&self.mapper, 100),
        };
        (item.online_id.is_some() || item.hash.is_some()).then_some(item)
    }

    fn same(&self, other: &Item) -> bool {
        match (self.online_id, other.online_id) {
            (Some(a), Some(b)) => a == b,
            _ => self.hash.is_some() && self.hash == other.hash,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Collection {
    pub id: u64,
    pub name: String,
    pub created: u64,
    pub maps: Vec<Item>,
    /// The game collection it is shown as, once written to the game.
    #[serde(default)]
    pub game_id: Option<i64>,
    /// To be shown in the game (written) as soon as Rhythia is closed: "Download as a collection"
    /// installs the maps, which needs the game running, and the game's collections can only be
    /// changed while it is closed.
    #[serde(default)]
    pub want_game: bool,
}

pub struct Mine {
    file: PathBuf,
    list: Mutex<Vec<Collection>>,
}

fn name_ok(name: &str) -> Result<String, String> {
    let name = clip(name, 80);
    if name.is_empty() {
        Err("Give the collection a name.".into())
    } else {
        Ok(name)
    }
}

impl Mine {
    pub fn load(file: PathBuf) -> Self {
        let mut list = crate::store::load::<Vec<Collection>>(&file).unwrap_or_default();
        list.truncate(MAX_COLLECTIONS);
        for c in &mut list {
            c.name = clip(&c.name, 80);
            c.maps = std::mem::take(&mut c.maps).into_iter().filter_map(Item::clean).take(MAX_MAPS).collect();
        }
        Mine { file, list: Mutex::new(list) }
    }

    pub fn all(&self) -> Vec<Collection> {
        self.list.lock().unwrap().clone()
    }

    pub fn get(&self, id: u64) -> Option<Collection> {
        self.list.lock().unwrap().iter().find(|c| c.id == id).cloned()
    }

    fn change<T>(&self, f: impl FnOnce(&mut Vec<Collection>) -> Result<T, String>) -> Result<T, String> {
        let mut list = self.list.lock().unwrap();
        let out = f(&mut list)?;
        crate::store::save(&self.file, &*list)?;
        Ok(out)
    }

    pub fn create(&self, name: &str, maps: Vec<Item>, now: u64) -> Result<Collection, String> {
        let name = name_ok(name)?;
        self.change(|list| {
            if list.len() >= MAX_COLLECTIONS {
                return Err(format!("You can have up to {MAX_COLLECTIONS} collections."));
            }
            let id = list.iter().map(|c| c.id).max().unwrap_or(0) + 1;
            let mut c = Collection { id, name, created: now, maps: Vec::new(), game_id: None, want_game: false };
            add_to(&mut c, maps);
            list.push(c.clone());
            Ok(c)
        })
    }

    pub fn rename(&self, id: u64, name: &str) -> Result<(), String> {
        let name = name_ok(name)?;
        self.change(|list| {
            find(list, id)?.name = name;
            Ok(())
        })
    }

    pub fn delete(&self, id: u64) -> Result<(), String> {
        self.change(|list| {
            list.retain(|c| c.id != id);
            Ok(())
        })
    }

    /// Adds maps it does not have yet; returns how many were added.
    /// Adds maps; a collection shown in the game is written there again once Rhythia is closed.
    pub fn add(&self, id: u64, maps: Vec<Item>) -> Result<usize, String> {
        self.change(|list| {
            let c = find(list, id)?;
            let n = add_to(c, maps);
            c.want_game |= n > 0 && c.game_id.is_some();
            Ok(n)
        })
    }

    /// Removes maps; a collection shown in the game is written there again once Rhythia is closed.
    pub fn remove(&self, id: u64, maps: Vec<Item>) -> Result<(), String> {
        self.change(|list| {
            let c = find(list, id)?;
            let before = c.maps.len();
            c.maps.retain(|m| !maps.iter().any(|x| x.same(m)));
            c.want_game |= c.maps.len() < before && c.game_id.is_some();
            Ok(())
        })
    }

    pub fn set_game_id(&self, id: u64, game_id: i64) -> Result<(), String> {
        self.change(|list| {
            let c = find(list, id)?;
            c.game_id = Some(game_id);
            c.want_game = false;
            Ok(())
        })
    }

    pub fn set_want_game(&self, id: u64, want: bool) -> Result<(), String> {
        self.change(|list| {
            find(list, id)?.want_game = want;
            Ok(())
        })
    }
}

fn find(list: &mut [Collection], id: u64) -> Result<&mut Collection, String> {
    list.iter_mut().find(|c| c.id == id).ok_or_else(|| "That collection is gone.".to_string())
}

fn add_to(c: &mut Collection, maps: Vec<Item>) -> usize {
    let before = c.maps.len();
    for m in maps.into_iter().filter_map(Item::clean) {
        if c.maps.len() >= MAX_MAPS {
            break;
        }
        if !c.maps.iter().any(|x| x.same(&m)) {
            c.maps.push(m);
        }
    }
    c.maps.len() - before
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: Option<i64>, hash: Option<&str>) -> Item {
        Item { online_id: id, hash: hash.map(String::from), title: "t".into(), mapper: String::new() }
    }

    #[test]
    fn adds_once_and_cleans() {
        let dir = std::env::temp_dir().join(format!("atlas-mine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mine = Mine::load(dir.join("c.json"));
        let h = "ab".repeat(32);
        let c = mine.create(" Mine ", vec![item(Some(5), None), item(Some(5), None), item(None, Some(&h)), item(None, None), item(Some(-1), None)], 1).unwrap();
        assert_eq!(c.name, "Mine");
        assert_eq!(c.maps.len(), 2);
        assert_eq!(mine.add(c.id, vec![item(Some(5), None), item(Some(6), None)]).unwrap(), 1);
        mine.remove(c.id, vec![item(None, Some(&h))]).unwrap();
        assert_eq!(Mine::load(dir.join("c.json")).get(c.id).unwrap().maps.len(), 2);
        assert!(mine.create("  ", vec![], 1).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
