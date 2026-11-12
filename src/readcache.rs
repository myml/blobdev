//! On-disk read cache.
//!
//! Clean chunks fetched from the object store are kept as files under a
//! directory, so a restart does not begin with an empty cache and the cache
//! can be much larger than RAM.
//!
//! Layout:
//!
//! ```text
//! <dir>/<volume>/<chunk-id>   [32-byte header][chunk bytes]
//! ```
//!
//! The header carries a magic, the chunk length, a generation counter (for
//! LRU eviction) and a CRC of the payload. A file that fails its checksum is
//! treated as absent and rewritten: a corrupt cache must never serve data,
//! it may only cost a refetch.
//!
//! Writes go to a temporary file that is renamed into place, so a crash
//! mid-write cannot leave a half-written chunk visible.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAGIC: u64 = 0x424c_4f42_5243_4143; // "BLOB RCAC"
const HEADER: usize = 32;

/// Header of a cached chunk file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryHeader {
    len: u64,
    generation: u64,
    crc: u32,
}

impl EntryHeader {
    fn encode(&self) -> [u8; HEADER] {
        let mut b = [0u8; HEADER];
        b[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        b[8..16].copy_from_slice(&self.len.to_le_bytes());
        b[16..24].copy_from_slice(&self.generation.to_le_bytes());
        b[24..28].copy_from_slice(&self.crc.to_le_bytes());
        b
    }

    fn decode(b: &[u8; HEADER]) -> Option<EntryHeader> {
        if u64::from_le_bytes(b[0..8].try_into().unwrap()) != MAGIC {
            return None;
        }
        Some(EntryHeader {
            len: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            generation: u64::from_le_bytes(b[16..24].try_into().unwrap()),
            crc: u32::from_le_bytes(b[24..28].try_into().unwrap()),
        })
    }
}

/// A read cache backed by a directory.
pub struct ReadCache {
    dir: PathBuf,
    max_bytes: u64,
    /// Bytes currently held, tracked incrementally once scanned.
    used_bytes: u64,
    generation: u64,
    hits: u64,
    misses: u64,
    enabled: bool,
    /// Every entry on disk: id -> (generation, bytes).
    ///
    /// Eviction needs the least recently used entries. It used to get them by
    /// reading the whole directory and the header of every file, which at
    /// --cache-size 16G is 4096 files and measured 59 ms -- spent in `put`,
    /// under the volume lock, every time the cache was full. The index is
    /// built once at open and kept current on put/get/evict, so eviction is a
    /// sort over memory.
    entries: HashMap<u64, (u64, u64)>,
}

impl ReadCache {
    /// Open (and measure) the cache for `volume` inside `dir`.
    pub fn open(dir: &Path, volume: &str, max_bytes: u64) -> Result<ReadCache> {
        let safe_volume: String = volume
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let dir = dir.join(safe_volume);
        let enabled = max_bytes > 0;
        let mut c = ReadCache {
            dir,
            max_bytes,
            used_bytes: 0,
            generation: 0,
            hits: 0,
            misses: 0,
            enabled,
            entries: HashMap::new(),
        };
        if enabled {
            fs::create_dir_all(&c.dir)
                .with_context(|| format!("create read cache {}", c.dir.display()))?;
            c.rescan()?;
        }
        Ok(c)
    }

    /// Measure the cache directory and find the highest generation in use.
    fn rescan(&mut self) -> Result<()> {
        self.used_bytes = 0;
        self.generation = 0;
        self.entries.clear();
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).context("scan read cache"),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            // Drop leftovers from an interrupted write.
            if path.extension().is_some_and(|e| e == "tmp") {
                let _ = fs::remove_file(&path);
                continue;
            }
            // Only files this cache wrote count. Anything else -- a stray
            // file, an editor backup, a half-created name -- used to be added
            // to used_bytes and then skipped, which is the worst combination:
            // the cache reports itself full, eviction cannot free the space
            // because the file is not in the index, and every later put pays
            // for a scan that cannot help. Leave it alone on disk; just do not
            // count it.
            let Ok(id) = entry.file_name().to_string_lossy().parse::<u64>() else {
                continue;
            };
            self.used_bytes += meta.len();
            let mut gen = 0;
            if let Ok(mut f) = fs::File::open(&path) {
                let mut h = [0u8; HEADER];
                if f.read_exact(&mut h).is_ok() {
                    if let Some(hdr) = EntryHeader::decode(&h) {
                        gen = hdr.generation;
                        self.generation = self.generation.max(hdr.generation);
                    }
                }
            }
            self.entries.insert(id, (gen, meta.len()));
        }
        Ok(())
    }

    /// Drop an entry from the index and from the byte count, after its file
    /// has been removed.
    fn forget(&mut self, id: u64) {
        if let Some((_, size)) = self.entries.remove(&id) {
            self.used_bytes = self.used_bytes.saturating_sub(size);
        }
    }

    fn path(&self, id: u64) -> PathBuf {
        self.dir.join(id.to_string())
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn used_bytes(&self) -> u64 {
        self.used_bytes
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    pub fn hits(&self) -> u64 {
        self.hits
    }

    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Look up a chunk. A checksum mismatch is treated as a miss.
    pub fn get(&mut self, id: u64, expect_len: u64) -> Option<Vec<u8>> {
        if !self.enabled {
            return None;
        }
        let path = self.path(id);
        let mut f = match fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => {
                self.misses += 1;
                return None;
            }
        };
        let mut h = [0u8; HEADER];
        if f.read_exact(&mut h).is_err() {
            let _ = fs::remove_file(&path);
            self.forget(id);
            self.misses += 1;
            return None;
        }
        let Some(hdr) = EntryHeader::decode(&h) else {
            let _ = fs::remove_file(&path);
            self.forget(id);
            self.misses += 1;
            return None;
        };
        if hdr.len != expect_len {
            // The volume geometry or the object changed under us.
            self.used_bytes = self.used_bytes.saturating_sub(hdr.len + HEADER as u64);
            let _ = fs::remove_file(&path);
            self.forget(id);
            self.misses += 1;
            return None;
        }
        let mut data = vec![0u8; hdr.len as usize];
        if f.read_exact(&mut data).is_err() {
            let _ = fs::remove_file(&path);
            self.forget(id);
            self.misses += 1;
            return None;
        }
        if crc32fast::hash(&data) != hdr.crc {
            self.used_bytes = self.used_bytes.saturating_sub(hdr.len + HEADER as u64);
            let _ = fs::remove_file(&path);
            self.forget(id);
            self.misses += 1;
            return None;
        }
        // Refresh the generation so this entry becomes the most recent.
        self.generation += 1;
        let refreshed = EntryHeader {
            generation: self.generation,
            ..hdr
        };
        if let Ok(mut wf) = fs::OpenOptions::new().write(true).open(&path) {
            let _ = wf.seek(SeekFrom::Start(0));
            let _ = wf.write_all(&refreshed.encode());
        }
        if let Some(e) = self.entries.get_mut(&id) {
            e.0 = self.generation;
        }
        self.hits += 1;
        Some(data)
    }

    /// Store a chunk, evicting the least recently used entries if needed.
    ///
    /// A chunk larger than the whole budget is simply not cached.
    pub fn put(&mut self, id: u64, data: &[u8]) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let need = data.len() as u64 + HEADER as u64;
        if need > self.max_bytes {
            return Ok(());
        }
        self.evict_until_fits(need)?;

        self.generation += 1;
        let hdr = EntryHeader {
            len: data.len() as u64,
            generation: self.generation,
            crc: crc32fast::hash(data),
        };
        let path = self.path(id);
        let tmp = self.dir.join(format!("{id}.tmp"));
        {
            let mut f = fs::File::create(&tmp)
                .with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(&hdr.encode())?;
            f.write_all(data)?;
            f.flush()?;
        }
        fs::rename(&tmp, &path)
            .with_context(|| format!("rename into {}", path.display()))?;
        // Replacing an entry that is already indexed must not count its bytes
        // twice.
        self.forget(id);
        self.entries.insert(id, (self.generation, need));
        self.used_bytes += need;
        Ok(())
    }

    /// Remove a chunk (it went dirty or was uploaded again).
    pub fn remove(&mut self, id: u64) {
        if !self.enabled {
            return;
        }
        let path = self.path(id);
        if let Ok(meta) = fs::metadata(&path) {
            self.used_bytes = self.used_bytes.saturating_sub(meta.len());
            let _ = fs::remove_file(&path);
        }
    }

    /// Evict least-recently-used entries until `need` more bytes fit.
    fn evict_until_fits(&mut self, need: u64) -> Result<()> {
        if self.used_bytes + need <= self.max_bytes {
            return Ok(());
        }
        // Least recently used first. The index is in memory, so this is a
        // sort over a few thousand pairs rather than a directory walk.
        let mut order: Vec<(u64, u64, u64)> = self
            .entries
            .iter()
            .map(|(id, (gen, size))| (*gen, *id, *size))
            .collect();
        order.sort_unstable_by_key(|(gen, _, _)| *gen);
        for (_, id, size) in order {
            if self.used_bytes + need <= self.max_bytes {
                break;
            }
            if fs::remove_file(self.path(id)).is_ok() {
                self.used_bytes = self.used_bytes.saturating_sub(size);
                self.entries.remove(&id);
            }
        }
        Ok(())
    }

    /// Drop everything (used by tests and by a cache flush command).
    pub fn clear(&mut self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return Ok(());
        };
        for entry in rd {
            let entry = entry?;
            if entry.path().is_file() {
                let _ = fs::remove_file(entry.path());
            }
        }
        self.used_bytes = 0;
        Ok(())
    }

    /// Per-chunk sizes, for diagnostics and tests.
    pub fn entry_count(&self) -> usize {
        if !self.enabled {
            return 0;
        }
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return 0;
        };
        rd.filter_map(|e| e.ok())
            .filter(|e| e.path().is_file() && !e.path().extension().is_some_and(|x| x == "tmp"))
            .count()
    }

    /// Map of chunk id to size, for tests.
    pub fn listing(&self) -> BTreeMap<u64, u64> {
        let mut out = BTreeMap::new();
        if !self.enabled {
            return out;
        }
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().is_some_and(|x| x == "tmp") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Ok(id) = name.parse::<u64>() else { continue };
            if let Ok(meta) = entry.metadata() {
                out.insert(id, meta.len());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("blobdev-rc-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn put_then_get() {
        let dir = tmpdir("basic");
        let mut c = ReadCache::open(&dir, "vol", 1 << 20).unwrap();
        c.put(7, b"hello chunk").unwrap();
        assert_eq!(c.get(7, 11).unwrap(), b"hello chunk");
        assert_eq!(c.hits(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn survives_reopen() {
        let dir = tmpdir("reopen");
        {
            let mut c = ReadCache::open(&dir, "vol", 1 << 20).unwrap();
            c.put(3, b"persisted").unwrap();
        }
        let mut c = ReadCache::open(&dir, "vol", 1 << 20).unwrap();
        assert_eq!(c.get(3, 9).unwrap(), b"persisted");
        assert!(c.used_bytes() > 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_entry_is_a_miss() {
        let dir = tmpdir("corrupt");
        let mut c = ReadCache::open(&dir, "vol", 1 << 20).unwrap();
        c.put(1, b"0123456789").unwrap();
        // Flip a payload byte on disk.
        let p = dir.join("vol").join("1");
        let mut raw = fs::read(&p).unwrap();
        let n = raw.len();
        raw[n - 1] ^= 0xFF;
        fs::write(&p, &raw).unwrap();
        assert!(c.get(1, 10).is_none(), "corrupt cache must not be served");
        assert_eq!(c.misses(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn length_mismatch_is_a_miss() {
        let dir = tmpdir("len");
        let mut c = ReadCache::open(&dir, "vol", 1 << 20).unwrap();
        c.put(2, b"0123456789").unwrap();
        assert!(c.get(2, 99).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn evicts_least_recently_used() {
        let dir = tmpdir("evict");
        let chunk = 1000usize;
        // Budget fits two chunks plus headers.
        let budget = (chunk as u64 + HEADER as u64) * 2 + 16;
        let mut c = ReadCache::open(&dir, "vol", budget).unwrap();
        c.put(1, &vec![1u8; chunk]).unwrap();
        c.put(2, &vec![2u8; chunk]).unwrap();
        // Touch 1 so 2 is now the least recently used.
        assert!(c.get(1, chunk as u64).is_some());
        c.put(3, &vec![3u8; chunk]).unwrap();
        assert!(c.get(1, chunk as u64).is_some(), "recently used entry went away");
        assert!(c.get(2, chunk as u64).is_none(), "LRU entry should be gone");
        assert!(c.get(3, chunk as u64).is_some());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversized_chunk_is_skipped() {
        let dir = tmpdir("oversize");
        let mut c = ReadCache::open(&dir, "vol", 100).unwrap();
        c.put(1, &vec![0u8; 500]).unwrap();
        assert_eq!(c.entry_count(), 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn disabled_cache_stores_nothing() {
        let dir = tmpdir("disabled");
        let mut c = ReadCache::open(&dir, "vol", 0).unwrap();
        c.put(1, b"x").unwrap();
        assert!(c.get(1, 1).is_none());
        assert_eq!(c.entry_count(), 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_tmp_files_are_dropped() {
        let dir = tmpdir("tmp");
        fs::create_dir_all(dir.join("vol")).unwrap();
        fs::write(dir.join("vol").join("5.tmp"), b"junk").unwrap();
        let c = ReadCache::open(&dir, "vol", 1 << 20).unwrap();
        assert_eq!(c.entry_count(), 0);
        assert_eq!(c.used_bytes(), 0);
        fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod index_tests {
    use super::*;

    /// A file the cache did not write must not make it think it is full.
    ///
    /// Rescan used to add every file's size to used_bytes and then skip the
    /// ones whose names were not numbers -- so a stray 1 MB file against a
    /// 64 KB budget left the cache permanently over budget, with eviction
    /// unable to recover anything because the file was never in the index.
    #[test]
    fn a_stray_file_does_not_fill_the_cache() {
        let dir = std::env::temp_dir().join(format!("blobdev-rcstray-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("vol")).unwrap();
        fs::write(dir.join("vol").join("not-an-entry"), vec![0u8; 1_000_000]).unwrap();
        let mut c = ReadCache::open(&dir, "vol", 64 * 1024).unwrap();
        assert_eq!(c.used_bytes, 0, "a file the cache did not write was counted");
        for id in 0..40u64 {
            c.put(id, &vec![1u8; 4096]).unwrap();
        }
        assert!(
            c.used_bytes <= c.max_bytes,
            "cache is over its budget: {} > {}",
            c.used_bytes,
            c.max_bytes
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The index must match what is on disk after eviction.
    ///
    /// Eviction used to walk the directory every time; it now sorts an
    /// in-memory index, so the index has to be right or entries leak and the
    /// byte count drifts.
    #[test]
    fn eviction_keeps_the_index_and_the_disk_in_step() {
        let dir = std::env::temp_dir().join(format!("blobdev-rcidx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Room for five 4 KiB entries.
        let mut c = ReadCache::open(&dir, "vol", 5 * (4096 + HEADER as u64)).unwrap();
        for id in 0..20u64 {
            c.put(id, &vec![id as u8; 4096]).unwrap();
            // Touch the lower ids so the higher ones are least recent.
            if id >= 4 {
                let _ = c.get(id - 4, 4096);
            }
        }
        let on_disk: Vec<u64> = fs::read_dir(c.dir.clone())
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().to_string_lossy().parse::<u64>().ok())
            .collect();
        assert_eq!(
            on_disk.len(),
            c.entries.len(),
            "index has {} entries, disk has {}",
            c.entries.len(),
            on_disk.len()
        );
        let counted: u64 = c.entries.values().map(|(_, size)| *size).sum();
        assert_eq!(counted, c.used_bytes, "byte count disagrees with the index");
        assert!(c.used_bytes <= c.max_bytes, "cache is over its budget");
        std::fs::remove_dir_all(&dir).ok();
    }
}
