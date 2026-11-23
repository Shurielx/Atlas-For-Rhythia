//! Atlas' own small JSON files in its data folder (limits, server key, map types, collections,
//! download history). Written whole through a temporary file, so a crash never leaves half a file.

use std::path::Path;

use serde::{de::DeserializeOwned, Serialize};

/// No file of Atlas' own is larger; a bigger one is not ours and is ignored.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// The file's value, or none when it is missing, too large or not what Atlas wrote.
pub fn load<T: DeserializeOwned>(file: &Path) -> Option<T> {
    if std::fs::metadata(file).ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    serde_json::from_slice(&std::fs::read(file).ok()?).ok()
}

pub fn save<T: Serialize>(file: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, file)).map_err(|e| format!("Atlas could not save its data: {e}"))
}
