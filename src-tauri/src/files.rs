//! Map files on disk, for the ZIP and Files download modes (install.rs): each file checked (it starts
//! like a Rhythia map and, when the file server states one, has its SHA-1), named after the map with
//! the extension of its address, written to the folder the player picked.
//!
//! ZIP: map files stored, not compressed (they are compressed already), names in UTF-8, written straight
//! to disk part by part (a new part past `PART_BYTES`), so a large collection never sits in memory. One
//! part is `<name>.zip`, more are `<name>-part-N.zip`. No ZIP64: a part stays far below 4 GB. Large
//! files that do compress (the game's database in a copy, backup.rs) go in deflated, streamed from disk;
//! `read_entry` reads one entry back.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// A part closes once it passes this size (like Reimagined's Download all).
pub const PART_BYTES: u64 = 1_500_000_000;
/// The largest map file Atlas takes.
pub const MAX_MAP_BYTES: usize = 200 * 1024 * 1024;

/// Checks a downloaded map file: a `.rhm` is a ZIP (`PK`), a `.sspm` starts with `SS+m`; with a SHA-1
/// from the server, the file must match it.
pub fn check(bytes: &[u8], ext: &str, sha1: Option<&str>) -> Result<(), String> {
    let looks = match ext {
        "rhm" => bytes.starts_with(b"PK"),
        "sspm" => bytes.starts_with(b"SS+m"),
        _ => false,
    };
    if !looks {
        return Err("The file is not a Rhythia map.".into());
    }
    if let Some(expected) = sha1 {
        if sha1_smol::Sha1::from(bytes).digest().to_string() != expected {
            return Err("The file did not arrive whole.".into());
        }
    }
    Ok(())
}

/// `rhm` or `sspm`, from the file's address.
pub fn ext_of(url: &str) -> Option<&'static str> {
    if url.ends_with(".rhm") {
        Some("rhm")
    } else if url.ends_with(".sspm") {
        Some("sspm")
    } else {
        None
    }
}

/// A file name Windows takes: the map's title and number, without characters Windows refuses.
pub fn file_name(title: &str, id: i64, ext: &str) -> String {
    let clean: String = title
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { ' ' } else { c })
        .collect();
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean: String = clean.chars().take(120).collect();
    let clean = clean.trim_end_matches(['.', ' ']);
    if clean.is_empty() {
        format!("{id}.{ext}")
    } else {
        format!("{clean} [{id}].{ext}")
    }
}

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

pub fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0u32, |c, &b| CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8))
}

struct Entry {
    name: Vec<u8>,
    /// 0 stored, 8 deflated.
    method: u16,
    crc: u32,
    packed: u32,
    size: u32,
    offset: u32,
}

/// A local file header; the sizes and CRC of a deflated entry are written over once known.
fn local_header(name: &[u8], method: u16, crc: u32, packed: u32, size: u32) -> Vec<u8> {
    let mut head = Vec::with_capacity(30 + name.len());
    head.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    head.extend_from_slice(&20u16.to_le_bytes()); // version needed
    head.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
    head.extend_from_slice(&method.to_le_bytes());
    head.extend_from_slice(&0u32.to_le_bytes()); // time and date
    head.extend_from_slice(&crc.to_le_bytes());
    head.extend_from_slice(&packed.to_le_bytes());
    head.extend_from_slice(&size.to_le_bytes());
    head.extend_from_slice(&(name.len() as u16).to_le_bytes());
    head.extend_from_slice(&0u16.to_le_bytes());
    head.extend_from_slice(name);
    head
}

/// Counts and CRCs what passes through, for the deflated entry's header.
struct Counted<R> {
    inner: R,
    crc: u32,
    size: u64,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.crc = !buf[..n].iter().fold(!self.crc, |c, &b| CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8));
        self.size += n as u64;
        Ok(n)
    }
}

/// A ZIP written part by part into a folder.
pub struct Zip {
    dir: PathBuf,
    name: String,
    part: u32,
    out: Option<(BufWriter<File>, u64)>,
    entries: Vec<Entry>,
    names: HashSet<String>,
    written: Vec<PathBuf>,
}

impl Zip {
    pub fn new(dir: &Path, name: &str) -> Zip {
        let base = file_name(name, 0, "zip");
        let base = base.trim_end_matches(" [0].zip").trim_end_matches(".zip").to_string();
        Zip { dir: dir.to_path_buf(), name: if base == "0" { "maps".into() } else { base }, part: 0, out: None, entries: Vec::new(), names: HashSet::new(), written: Vec::new() }
    }

    fn part_path(&self, n: u32) -> PathBuf {
        self.dir.join(format!("{}-part-{n}.zip", self.name))
    }

    fn open_part(&mut self) -> Result<(), String> {
        if self.out.is_none() {
            self.part += 1;
            let path = self.part_path(self.part);
            let f = File::create(&path).map_err(|e| format!("Could not write {}: {e}", path.display()))?;
            self.written.push(path);
            self.out = Some((BufWriter::new(f), 0));
            self.entries.clear();
            self.names.clear();
        }
        Ok(())
    }

    /// The name in this part; the same name twice gets a number.
    fn unique(&mut self, file: &str) -> Vec<u8> {
        let mut name = file.to_string();
        let mut n = 2;
        while !self.names.insert(name.to_lowercase()) {
            name = match file.rsplit_once('.') {
                Some((stem, ext)) => format!("{stem} ({n}).{ext}"),
                None => format!("{file} ({n})"),
            };
            n += 1;
        }
        name.into_bytes()
    }

    /// Adds one file, stored; a new part starts when the current one is full.
    pub fn add(&mut self, file: &str, bytes: &[u8]) -> Result<(), String> {
        let size = u32::try_from(bytes.len()).map_err(|_| "The file is too large for a ZIP part.")?;
        if self.out.as_ref().is_some_and(|(_, n)| *n + bytes.len() as u64 > PART_BYTES) {
            self.close_part()?;
        }
        self.open_part()?;
        let name = self.unique(file);
        let crc = crc32(bytes);
        let head = local_header(&name, 0, crc, size, size);
        let (w, at) = self.out.as_mut().expect("a part is open");
        let offset = u32::try_from(*at).map_err(|_| "A ZIP part grew too large.")?;
        let fail = |e: std::io::Error| format!("Could not write the ZIP: {e}");
        w.write_all(&head).map_err(fail)?;
        w.write_all(bytes).map_err(fail)?;
        *at += (head.len() + bytes.len()) as u64;
        self.entries.push(Entry { name, method: 0, crc, packed: size, size, offset });
        Ok(())
    }

    /// Adds a file from disk, deflated and streamed (never whole in memory), into the current part.
    pub fn add_path_deflated(&mut self, file: &str, path: &Path) -> Result<(), String> {
        self.open_part()?;
        let name = self.unique(file);
        let fail = |e: std::io::Error| format!("Could not write the ZIP: {e}");
        let source = File::open(path).map_err(|e| format!("Could not read {}: {e}", path.display()))?;
        let (w, at) = self.out.as_mut().expect("a part is open");
        let offset = u32::try_from(*at).map_err(|_| "A ZIP part grew too large.")?;
        let head = local_header(&name, 8, 0, 0, 0);
        w.write_all(&head).map_err(fail)?;
        let mut input = Counted { inner: BufReader::new(source), crc: 0, size: 0 };
        let mut enc = flate2::write::DeflateEncoder::new(&mut *w, flate2::Compression::default());
        std::io::copy(&mut input, &mut enc).map_err(fail)?;
        enc.finish().map_err(fail)?;
        let end = w.stream_position().map_err(fail)?;
        let packed = end - u64::from(offset) - head.len() as u64;
        let (crc, size) = (input.crc, input.size);
        let too_big = |_| "The file is too large for a ZIP part.".to_string();
        let (packed, size) = (u32::try_from(packed).map_err(too_big)?, u32::try_from(size).map_err(too_big)?);
        // CRC and sizes over the zeros written before.
        w.seek(SeekFrom::Start(u64::from(offset) + 14)).map_err(fail)?;
        w.write_all(&[crc.to_le_bytes(), packed.to_le_bytes(), size.to_le_bytes()].concat()).map_err(fail)?;
        w.seek(SeekFrom::Start(end)).map_err(fail)?;
        *at = end;
        self.entries.push(Entry { name, method: 8, crc, packed, size, offset });
        Ok(())
    }

    fn close_part(&mut self) -> Result<(), String> {
        let Some((mut w, at)) = self.out.take() else { return Ok(()) };
        let start = u32::try_from(at).map_err(|_| "A ZIP part grew too large.")?;
        let mut dir = Vec::new();
        for e in &self.entries {
            dir.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            dir.extend_from_slice(&20u16.to_le_bytes()); // made by
            dir.extend_from_slice(&20u16.to_le_bytes()); // needed
            dir.extend_from_slice(&0x0800u16.to_le_bytes());
            dir.extend_from_slice(&e.method.to_le_bytes());
            dir.extend_from_slice(&0u32.to_le_bytes());
            dir.extend_from_slice(&e.crc.to_le_bytes());
            dir.extend_from_slice(&e.packed.to_le_bytes());
            dir.extend_from_slice(&e.size.to_le_bytes());
            dir.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            dir.extend_from_slice(&[0u8; 8]); // extra, comment, disk, internal attributes
            dir.extend_from_slice(&0u32.to_le_bytes()); // external attributes
            dir.extend_from_slice(&e.offset.to_le_bytes());
            dir.extend_from_slice(&e.name);
        }
        let count = self.entries.len() as u16;
        let mut end = Vec::with_capacity(22);
        end.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        end.extend_from_slice(&[0u8; 4]);
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&(dir.len() as u32).to_le_bytes());
        end.extend_from_slice(&start.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        let fail = |e: std::io::Error| format!("Could not write the ZIP: {e}");
        w.write_all(&dir).map_err(fail)?;
        w.write_all(&end).map_err(fail)?;
        w.flush().map_err(fail)?;
        Ok(())
    }

    /// Closes the last part; a single part loses its `-part-1`. The files written.
    pub fn finish(mut self) -> Result<Vec<PathBuf>, String> {
        self.close_part()?;
        if self.written.len() == 1 {
            let one = self.dir.join(format!("{}.zip", self.name));
            if !one.exists() && std::fs::rename(&self.written[0], &one).is_ok() {
                self.written[0] = one;
            }
        }
        Ok(self.written)
    }
}

/// Reads the entry `name` of a ZIP (stored or deflated) into `out`, checking its CRC; at most `max`
/// bytes. Its size.
pub fn read_entry(zip: &Path, name: &str, out: &mut impl Write, max: u64) -> Result<u64, String> {
    let bad = || "The ZIP is damaged.".to_string();
    let io = |e: std::io::Error| format!("Could not read the ZIP: {e}");
    let mut f = File::open(zip).map_err(io)?;
    let len = f.metadata().map_err(io)?.len();
    if len < 22 {
        return Err(bad());
    }
    // The end record: the last 22 bytes, or further back when the ZIP has a comment (at most 64 KB).
    let tail_len = len.min(22 + 65_535);
    let mut tail = vec![0u8; tail_len as usize];
    f.seek(SeekFrom::Start(len - tail_len)).map_err(io)?;
    f.read_exact(&mut tail).map_err(io)?;
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let at = (0..=tail.len() - 22).rev().find(|&i| u32_at(&tail, i) == 0x0605_4b50).ok_or_else(bad)?;
    let end = &tail[at..at + 22];
    let (dir_len, dir_at) = (u32_at(end, 12) as u64, u32_at(end, 16) as u64);
    if dir_at + dir_len > len || dir_len > 16 * 1024 * 1024 {
        return Err(bad());
    }
    let mut dir = vec![0u8; dir_len as usize];
    f.seek(SeekFrom::Start(dir_at)).map_err(io)?;
    f.read_exact(&mut dir).map_err(io)?;
    let mut i = 0;
    while i + 46 <= dir.len() && u32_at(&dir, i) == 0x0201_4b50 {
        let (method, crc, packed, size) = (u16_at(&dir, i + 10), u32_at(&dir, i + 16), u32_at(&dir, i + 20) as u64, u32_at(&dir, i + 24) as u64);
        let (n, extra, comment) = (u16_at(&dir, i + 28) as usize, u16_at(&dir, i + 30) as usize, u16_at(&dir, i + 32) as usize);
        let offset = u32_at(&dir, i + 42) as u64;
        let entry = dir.get(i + 46..i + 46 + n).ok_or_else(bad)?;
        i += 46 + n + extra + comment;
        if entry != name.as_bytes() {
            continue;
        }
        if size > max {
            return Err("A file in the ZIP is too large.".into());
        }
        let mut head = [0u8; 30];
        f.seek(SeekFrom::Start(offset)).map_err(io)?;
        f.read_exact(&mut head).map_err(io)?;
        if u32_at(&head, 0) != 0x0403_4b50 {
            return Err(bad());
        }
        let skip = u16_at(&head, 26) as i64 + u16_at(&head, 28) as i64;
        f.seek(SeekFrom::Current(skip)).map_err(io)?;
        let data = BufReader::new(Read::by_ref(&mut f).take(packed));
        let mut input: Counted<Box<dyn Read>> = Counted {
            inner: match method {
                0 => Box::new(data),
                8 => Box::new(flate2::read::DeflateDecoder::new(data)),
                _ => return Err(bad()),
            },
            crc: 0,
            size: 0,
        };
        let copied = std::io::copy(&mut Read::by_ref(&mut input).take(max + 1), out).map_err(|_| bad())?;
        if copied != size || input.crc != crc {
            return Err(bad());
        }
        return Ok(copied);
    }
    Err(format!("{name} is not in the ZIP."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_windows_takes() {
        assert_eq!(file_name("Camellia - ナイト of Nights?", 42, "sspm"), "Camellia - ナイト of Nights [42].sspm");
        assert_eq!(file_name(" ..<>  ", 7, "rhm"), "7.rhm");
        assert_eq!(ext_of("https://static.rhythia.com/a.rhm"), Some("rhm"));
        assert_eq!(ext_of("https://static.rhythia.com/a.zip"), None);
    }

    #[test]
    fn checks_files() {
        assert!(check(b"SS+m....", "sspm", None).is_ok());
        assert!(check(b"PK\x03\x04", "sspm", None).is_err());
        let sha = sha1_smol::Sha1::from(b"PK\x03\x04xyz").digest().to_string();
        assert!(check(b"PK\x03\x04xyz", "rhm", Some(&sha)).is_ok());
        assert!(check(b"PK\x03\x04xyZ", "rhm", Some(&sha)).is_err());
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn zip_parts_open_and_read_back() {
        let dir = std::env::temp_dir().join(format!("atlas-zip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut z = Zip::new(&dir, "My: collection");
        z.add("a [1].sspm", b"SS+m first").unwrap();
        z.add("a [1].sspm", b"SS+m second").unwrap();
        let files = z.finish().unwrap();
        assert_eq!(files, vec![dir.join("My collection.zip")]);
        let bytes = std::fs::read(&files[0]).unwrap();
        // End record: two entries; the second name got a number.
        let end = &bytes[bytes.len() - 22..];
        assert_eq!(u32::from_le_bytes(end[0..4].try_into().unwrap()), 0x0605_4b50);
        assert_eq!(u16::from_le_bytes(end[10..12].try_into().unwrap()), 2);
        assert!(bytes.windows(14).any(|w| w == b"a [1] (2).sspm"));
        // Each local entry's data is where its header says, with the right CRC.
        assert_eq!(&bytes[30 + 10..30 + 10 + 10], b"SS+m first");
        assert_eq!(u32::from_le_bytes(bytes[14..18].try_into().unwrap()), crc32(b"SS+m first"));
        let mut back = Vec::new();
        read_entry(&files[0], "a [1] (2).sspm", &mut back, 100).unwrap();
        assert_eq!(back, b"SS+m second");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn deflated_files_read_back() {
        let dir = std::env::temp_dir().join(format!("atlas-zip8-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let big: Vec<u8> = (0..300_000u32).flat_map(|n| (n % 251).to_le_bytes()).collect();
        std::fs::write(dir.join("in.db"), &big).unwrap();
        let mut z = Zip::new(&dir, "copy");
        z.add_path_deflated("rhythia.db", &dir.join("in.db")).unwrap();
        z.add("files.txt", b"maps/x").unwrap();
        let zip = z.finish().unwrap().remove(0);
        assert!(std::fs::metadata(&zip).unwrap().len() < big.len() as u64 / 4, "it is compressed");
        let mut back = Vec::new();
        assert_eq!(read_entry(&zip, "rhythia.db", &mut back, 10_000_000).unwrap(), big.len() as u64);
        assert!(back == big);
        let mut list = Vec::new();
        read_entry(&zip, "files.txt", &mut list, 100).unwrap();
        assert_eq!(list, b"maps/x");
        assert!(read_entry(&zip, "rhythia.db", &mut Vec::new(), 1000).is_err(), "the size cap holds");
        // A changed byte in the packed data is caught.
        let mut bytes = std::fs::read(&zip).unwrap();
        bytes[60] ^= 0xFF;
        std::fs::write(&zip, &bytes).unwrap();
        assert!(read_entry(&zip, "rhythia.db", &mut Vec::new(), 10_000_000).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
