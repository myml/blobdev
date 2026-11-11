//! The volume: chunked data over an object store, state over a metadata store.
//!
//! This mirrors JuiceFS's split between a *metadata engine* and an *object
//! store*, scaled down to a block device:
//!
//! ```text
//!   read/write ──► Volume ──► ObjectStore (chunk objects: the data)
//!                    │
//!                    └────► MetaStore   (geometry + dirty blocks: the metadata)
//! ```
//!
//! A write updates the local working copy of a chunk and records the touched
//! blocks in the metadata. Nothing goes to the object store until the chunk is
//! flushed, so the write path never waits on the network.
//!
//! Upload strategy follows the object store's capabilities:
//!
//! * `partial_write` (WebDAV, local directory): only the dirty block ranges
//!   are rewritten.
//! * otherwise (S3): the whole chunk is uploaded, since S3 objects can only be
//!   replaced wholesale.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use crate::meta::{ChunkState, Geometry, MetaStore, Snapshot};
use crate::store::{Caps, ObjectStore};

pub const BLOCK: u64 = 4096;
pub const DEFAULT_CHUNK: u64 = 8 << 20;

/// Parameters used only when creating a new volume.
#[derive(Debug, Clone, Copy)]
pub struct Create {
    pub size: u64,
    pub chunk_size: Option<u64>,
}

impl Create {
    pub fn new(size: u64) -> Create {
        Create {
            size,
            chunk_size: None,
        }
    }

    pub fn with_chunk_size(mut self, chunk_size: u64) -> Create {
        self.chunk_size = Some(chunk_size);
        self
    }
}

/// Tunables for when dirty chunks are uploaded.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Upload when this fraction of a chunk's blocks are dirty (percent).
    pub full_pct: u64,
    /// Upload a chunk that has been dirty for at least this long.
    pub max_age: std::time::Duration,
    /// How many clean chunks to keep resident for reads.
    pub read_chunks: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            full_pct: 50,
            max_age: std::time::Duration::from_secs(30),
            read_chunks: 64,
        }
    }
}

/// Object key layout.
#[derive(Clone, Debug)]
pub struct Layout {
    pub root: String,
}

impl Layout {
    pub fn new(root: impl Into<String>) -> Layout {
        Layout { root: root.into() }
    }

    pub fn chunk_key(&self, id: u64) -> String {
        format!("{}/c/{}", self.root, id)
    }
}

/// A chunk with local modifications.
struct Working {
    /// Full chunk contents; a whole-chunk upload needs them all.
    data: Vec<u8>,
    dirty: BTreeMap<u64, ()>,
    since: Instant,
}

/// Small LRU of clean chunks.
struct LruChunks {
    map: HashMap<u64, (Vec<u8>, u64)>,
    capacity: usize,
    clock: u64,
}

impl LruChunks {
    fn new(capacity: usize) -> LruChunks {
        LruChunks {
            map: HashMap::new(),
            capacity,
            clock: 0,
        }
    }

    fn get(&mut self, id: u64) -> Option<&Vec<u8>> {
        self.clock += 1;
        let now = self.clock;
        match self.map.get_mut(&id) {
            Some(e) => {
                e.1 = now;
                Some(&e.0)
            }
            None => None,
        }
    }

    fn insert(&mut self, id: u64, data: Vec<u8>) {
        if self.capacity == 0 {
            return;
        }
        self.clock += 1;
        let now = self.clock;
        self.map.insert(id, (data, now));
        while self.map.len() > self.capacity {
            let victim = self
                .map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| *k);
            match victim {
                Some(v) => {
                    self.map.remove(&v);
                }
                None => break,
            }
        }
    }

    fn remove(&mut self, id: u64) {
        self.map.remove(&id);
    }

    fn len(&self) -> usize {
        self.map.len()
    }
}

pub struct Volume {
    data: Arc<dyn ObjectStore>,
    meta: Box<dyn MetaStore>,
    layout: Layout,
    caps: Caps,
    policy: Policy,
    geometry: Geometry,
    working: HashMap<u64, Working>,
    read_cache: LruChunks,
    sent_chunks: u64,
    sent_bytes: u64,
}

impl Volume {
    /// Open a volume, creating it when the metadata store is empty.
    ///
    /// `create` supplies the size (and optionally the chunk size) used only
    /// when the volume does not exist yet.
    pub fn open(
        data: Arc<dyn ObjectStore>,
        mut meta: Box<dyn MetaStore>,
        layout: Layout,
        create: Option<Create>,
        policy: Policy,
    ) -> Result<Volume> {
        let caps = data.caps();
        let snapshot = match meta.load()? {
            Some(s) => s,
            None => {
                let Some(create) = create else {
                    anyhow::bail!(
                        "volume {} has no metadata; pass a size to create it",
                        layout.root
                    );
                };
                let size = create.size;
                let geometry = Geometry {
                    chunk_size: create.chunk_size.unwrap_or(DEFAULT_CHUNK),
                    size,
                };
                geometry.validate()?;
                let snap = Snapshot::new(geometry.clone());
                meta.save(&snap)?;
                log::info!(
                    "created volume {} ({} MiB, {} MiB chunks, metadata in {})",
                    layout.root,
                    size >> 20,
                    geometry.chunk_size >> 20,
                    meta.name()
                );
                snap
            }
        };

        let geometry = snapshot.geometry.clone();
        let read_cache = LruChunks::new(policy.read_chunks);
        let mut vol = Volume {
            data,
            meta,
            layout,
            caps,
            policy,
            geometry,
            working: HashMap::new(),
            read_cache,
            sent_chunks: 0,
            sent_bytes: 0,
        };
        vol.restore(snapshot)?;
        Ok(vol)
    }

    /// Rebuild working copies from metadata left by an earlier run, so dirty
    /// blocks recorded there get uploaded on the next flush.
    fn restore(&mut self, snap: Snapshot) -> Result<()> {
        let nchunks = snap.chunks.values().filter(|s| !s.dirty.is_empty()).count();
        let nblocks: usize = snap.chunks.values().map(|s| s.dirty.len()).sum();
        if nchunks > 0 {
            log::info!(
                "restoring {nchunks} modified chunk(s), {nblocks} block(s) pending upload"
            );
            for (id, st) in snap.chunks {
                if st.dirty.is_empty() {
                    continue;
                }
                let len = self.geometry.chunk_len(id);
                let data = self.fetch_chunk(id, len)?;
                self.working.insert(
                    id,
                    Working {
                        data,
                        dirty: st.dirty,
                        since: Instant::now(),
                    },
                );
            }
        }
        log::info!(
            "volume {}: data={} (range_read={} partial_write={}) metadata={}",
            self.layout.root,
            self.data.name(),
            self.caps.range_read,
            self.caps.partial_write,
            self.meta.name()
        );
        Ok(())
    }

    pub fn size(&self) -> u64 {
        self.geometry.size
    }

    pub fn chunk_size(&self) -> u64 {
        self.geometry.chunk_size
    }

    pub fn geometry(&self) -> &Geometry {
        &self.geometry
    }

    fn chunk_of(&self, offset: u64) -> u64 {
        offset / self.geometry.chunk_size
    }

    /// Fetch a chunk from the object store; a missing object is a hole.
    fn fetch_chunk(&self, id: u64, len: u64) -> Result<Vec<u8>> {
        let key = self.layout.chunk_key(id);
        let mut v = match self.data.get(&key, None) {
            Ok(v) => v,
            Err(e) => {
                if self.data.stat(&key)?.is_none() {
                    vec![0u8; len as usize]
                } else {
                    return Err(e).with_context(|| format!("fetch chunk {id}"));
                }
            }
        };
        v.resize(len as usize, 0);
        v.truncate(len as usize);
        Ok(v)
    }

    /// Current contents of a chunk: working copy, clean cache, or the store.
    fn load_chunk(&mut self, id: u64) -> Result<Vec<u8>> {
        if let Some(w) = self.working.get(&id) {
            return Ok(w.data.clone());
        }
        if let Some(d) = self.read_cache.get(id) {
            return Ok(d.clone());
        }
        let len = self.geometry.chunk_len(id);
        let data = self.fetch_chunk(id, len)?;
        self.read_cache.insert(id, data.clone());
        Ok(data)
    }

    /// Read `blocks` blocks starting at block offset `blk`.
    pub fn read(&mut self, blk: u64, blocks: usize, out: &mut [u8]) -> Result<()> {
        let want = blocks as u64 * BLOCK;
        if out.len() as u64 != want {
            anyhow::bail!("read buffer is {} bytes, expected {want}", out.len());
        }
        let end = blk * BLOCK + want;
        if end > self.geometry.size {
            anyhow::bail!("read past end of volume");
        }
        let mut off = blk * BLOCK;
        let mut done = 0usize;
        while off < end {
            let id = self.chunk_of(off);
            let local = off - id * self.geometry.chunk_size;
            let n = ((self.geometry.chunk_len(id) - local).min(end - off)) as usize;
            let c = self.load_chunk(id)?;
            if c.len() < local as usize + n {
                anyhow::bail!("chunk {id} shorter than expected");
            }
            out[done..done + n].copy_from_slice(&c[local as usize..local as usize + n]);
            done += n;
            off += n as u64;
        }
        Ok(())
    }

    /// Write `blocks` blocks starting at block offset `blk`.
    pub fn write(&mut self, blk: u64, blocks: usize, data: &[u8]) -> Result<()> {
        let want = blocks as u64 * BLOCK;
        if data.len() as u64 != want {
            anyhow::bail!("write buffer is {} bytes, expected {want}", data.len());
        }
        let start = blk * BLOCK;
        if start + want > self.geometry.size {
            anyhow::bail!("write past end of volume");
        }
        let mut off = start;
        let end = start + want;
        let mut done = 0usize;
        while off < end {
            let id = self.chunk_of(off);
            let local = off - id * self.geometry.chunk_size;
            let n = ((self.geometry.chunk_len(id) - local).min(end - off)) as usize;
            self.write_into_chunk(id, local, &data[done..done + n])?;
            done += n;
            off += n as u64;
        }
        self.save_meta()
    }

    fn write_into_chunk(&mut self, id: u64, local: u64, data: &[u8]) -> Result<()> {
        if !self.working.contains_key(&id) {
            let contents = self.load_chunk(id)?;
            self.working.insert(
                id,
                Working {
                    data: contents,
                    dirty: BTreeMap::new(),
                    since: Instant::now(),
                },
            );
            self.read_cache.remove(id);
        }
        let base = local as usize;
        let w = self.working.get_mut(&id).unwrap();
        if base + data.len() > w.data.len() {
            anyhow::bail!(
                "chunk {id}: write [{base}, {}) beyond its {} bytes",
                base + data.len(),
                w.data.len()
            );
        }
        w.data[base..base + data.len()].copy_from_slice(data);

        // Record every block this write touched.
        let first = local / BLOCK;
        let last = (local + data.len() as u64).div_ceil(BLOCK);
        for b in first..last {
            w.dirty.insert(b, ());
        }
        Ok(())
    }

    /// Persist metadata describing the modified chunks.
    fn save_meta(&mut self) -> Result<()> {
        let mut chunks: BTreeMap<u64, ChunkState> = BTreeMap::new();
        for (id, w) in &self.working {
            chunks.insert(
                *id,
                ChunkState {
                    dirty: w.dirty.clone(),
                    uploaded: false,
                },
            );
        }
        let snap = Snapshot {
            geometry: self.geometry.clone(),
            chunks,
        };
        self.meta.save(&snap)
    }

    /// Upload one chunk. Returns the number of bytes sent.
    fn upload(&mut self, id: u64) -> Result<u64> {
        let Some(w) = self.working.get(&id) else {
            return Ok(0);
        };
        let key = self.layout.chunk_key(id);

        let sent = if self.caps.partial_write && !w.dirty.is_empty() {
            let mut sent = 0u64;
            if self.data.stat(&key)?.is_none() {
                // Nothing to patch yet: create the object in one go.
                self.data.put(&key, &w.data)?;
                sent += w.data.len() as u64;
            } else {
                for (start, end) in contiguous_ranges(&w.dirty) {
                    let slice = &w.data[start..end];
                    if let Err(e) = self.data.patch(&key, start as u64, slice) {
                        // Never leave the object half-updated.
                        log::warn!("patch {key} failed ({e}); uploading the whole chunk");
                        self.data.put(&key, &w.data)?;
                        sent += w.data.len() as u64;
                        break;
                    }
                    sent += slice.len() as u64;
                }
            }
            sent
        } else {
            self.data.put(&key, &w.data)?;
            w.data.len() as u64
        };

        let w = self.working.remove(&id).unwrap();
        self.read_cache.insert(id, w.data);
        self.sent_chunks += 1;
        self.sent_bytes += sent;
        Ok(sent)
    }

    /// Upload everything; this is what a FLUSH request maps to.
    pub fn flush(&mut self) -> Result<()> {
        let ids: Vec<u64> = self.working.keys().copied().collect();
        for id in ids {
            self.upload(id)?;
        }
        self.save_meta()
    }

    /// Upload chunks that are full enough or have been dirty long enough.
    pub fn tick(&mut self) -> Result<()> {
        if self.working.is_empty() {
            return Ok(());
        }
        let mut ready = Vec::new();
        for (id, w) in &self.working {
            let real_blocks = (self.geometry.chunk_len(*id) / BLOCK).max(1);
            let threshold = (real_blocks * self.policy.full_pct / 100).max(1);
            if w.dirty.len() as u64 >= threshold || w.since.elapsed() >= self.policy.max_age {
                ready.push(*id);
            }
        }
        if ready.is_empty() {
            return Ok(());
        }
        for id in ready {
            self.upload(id)?;
        }
        self.save_meta()
    }

    pub fn dirty_chunks(&self) -> usize {
        self.working.len()
    }

    pub fn dirty_blocks(&self) -> usize {
        self.working.values().map(|w| w.dirty.len()).sum()
    }

    pub fn cached_chunks(&self) -> usize {
        self.read_cache.len()
    }

    /// Maximum number of clean chunks held in memory.
    pub fn cache_capacity(&self) -> usize {
        self.read_cache.capacity
    }

    pub fn sent_chunks(&self) -> u64 {
        self.sent_chunks
    }

    pub fn sent_bytes(&self) -> u64 {
        self.sent_bytes
    }

    /// Byte range of a block request, validated against the volume size.
    pub fn check_range(&self, blk: u64, blocks: usize) -> Result<Range<u64>> {
        let start = blk * BLOCK;
        let end = start + blocks as u64 * BLOCK;
        if end > self.geometry.size {
            anyhow::bail!(
                "IO past end of volume: {start}..{end} > {}",
                self.geometry.size
            );
        }
        Ok(start..end)
    }
}

/// Merge a sorted set of dirty block indices into byte ranges.
fn contiguous_ranges(dirty: &BTreeMap<u64, ()>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut iter = dirty.keys().copied().peekable();
    while let Some(first) = iter.next() {
        let mut last = first;
        while let Some(next) = iter.peek().copied() {
            if next == last + 1 {
                last = next;
                iter.next();
            } else {
                break;
            }
        }
        out.push((
            first as usize * BLOCK as usize,
            (last as usize + 1) * BLOCK as usize,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{FileMeta, MemMeta};
    use crate::store::MemStore;

    fn mem_volume(size: u64, policy: Policy) -> Volume {
        Volume::open(
            Arc::new(MemStore::new()),
            Box::new(MemMeta::new()),
            Layout::new("v"),
            Some(Create::new(size)),
            policy,
        )
        .unwrap()
    }

    fn blocks(n: usize, v: u8) -> Vec<u8> {
        vec![v; n * BLOCK as usize]
    }

    #[test]
    fn fresh_volume_reads_zeros() {
        let mut v = mem_volume(4 * DEFAULT_CHUNK, Policy::default());
        assert_eq!(v.size(), 4 * DEFAULT_CHUNK);
        let mut buf = vec![0xAAu8; 2 * BLOCK as usize];
        v.read(10, 2, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn write_then_read_before_upload() {
        let store = Arc::new(MemStore::new());
        let mut v = Volume::open(
            store.clone(),
            Box::new(MemMeta::new()),
            Layout::new("v"),
            Some(Create::new(2 * DEFAULT_CHUNK)),
            Policy::default(),
        )
        .unwrap();
        v.write(5, 3, &blocks(3, 0x42)).unwrap();
        let mut got = vec![0u8; 3 * BLOCK as usize];
        v.read(5, 3, &mut got).unwrap();
        assert!(got.iter().all(|&b| b == 0x42));
        assert_eq!(store.stored_bytes(), 0, "nothing uploaded before flush");
        assert_eq!(v.dirty_chunks(), 1);
        assert_eq!(v.dirty_blocks(), 3);
    }

    #[test]
    fn flush_uploads_and_reopens() {
        let dir = std::env::temp_dir().join(format!("blobdev-reopen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let meta_path = dir.join("meta");
        let store = Arc::new(MemStore::new());
        let mut v = Volume::open(
            store.clone(),
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("v"),
            Some(Create::new(2 * DEFAULT_CHUNK)),
            Policy::default(),
        )
        .unwrap();
        v.write(5, 2, &blocks(2, 0x11)).unwrap();
        v.flush().unwrap();
        assert_eq!(v.dirty_chunks(), 0);
        assert!(store.stored_bytes() >= 2 * BLOCK);

        let mut v2 = Volume::open(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("v"),
            None,
            Policy::default(),
        )
        .unwrap();
        let mut got = vec![0u8; 2 * BLOCK as usize];
        v2.read(5, 2, &mut got).unwrap();
        assert!(got.iter().all(|&b| b == 0x11));
        assert_eq!(v2.dirty_chunks(), 0, "flushed chunks must reopen clean");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_straddling_chunk_boundary() {
        let mut v = mem_volume(2 * DEFAULT_CHUNK, Policy::default());
        let cblocks = DEFAULT_CHUNK / BLOCK;
        v.write(cblocks - 1, 3, &blocks(3, 0x7E)).unwrap();
        assert_eq!(v.dirty_chunks(), 2);
        v.flush().unwrap();
        let mut got = vec![0u8; 3 * BLOCK as usize];
        v.read(cblocks - 1, 3, &mut got).unwrap();
        assert!(got.iter().all(|&b| b == 0x7E));
    }

    #[test]
    fn partial_write_only_sends_dirty_blocks() {
        let store = Arc::new(MemStore::new());
        let mut v = Volume::open(
            store.clone(),
            Box::new(MemMeta::new()),
            Layout::new("v"),
            Some(Create::new(DEFAULT_CHUNK)),
            Policy::default(),
        )
        .unwrap();
        v.write(0, 1, &blocks(1, 1)).unwrap();
        v.flush().unwrap();
        let after_create = store.stored_bytes();
        assert!(after_create >= DEFAULT_CHUNK, "first upload creates the object");

        v.write(7, 1, &blocks(1, 2)).unwrap();
        v.flush().unwrap();
        let second = store.stored_bytes() - after_create;
        assert!(
            second <= 2 * BLOCK,
            "partial write sent {second} bytes for one block"
        );
    }

    #[test]
    fn metadata_survives_crash() {
        let dir = std::env::temp_dir().join(format!("blobdev-vol-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let meta_path = dir.join("meta");
        let store = Arc::new(MemStore::new());

        {
            let mut v = Volume::open(
                store.clone(),
                Box::new(FileMeta::new(&meta_path)),
                Layout::new("v"),
                Some(Create::new(DEFAULT_CHUNK)),
                Policy::default(),
            )
            .unwrap();
            v.write(3, 1, &blocks(1, 0x55)).unwrap();
            // Crash: metadata is saved, the upload never happens.
        }

        let mut v = Volume::open(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("v"),
            None,
            Policy::default(),
        )
        .unwrap();
        assert_eq!(v.dirty_chunks(), 1, "pending modification not restored");
        assert_eq!(v.dirty_blocks(), 1);
        v.flush().unwrap();
        assert_eq!(v.dirty_chunks(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tick_uploads_full_chunks() {
        let policy = Policy {
            full_pct: 50,
            max_age: std::time::Duration::from_secs(3600),
            read_chunks: 4,
        };
        let mut v = mem_volume(DEFAULT_CHUNK, policy);
        let half = (DEFAULT_CHUNK / BLOCK / 2) as usize;
        v.write(0, half, &blocks(half, 9)).unwrap();
        assert_eq!(v.dirty_chunks(), 1);
        v.tick().unwrap();
        assert_eq!(v.dirty_chunks(), 0);
        assert_eq!(v.sent_chunks(), 1);
    }

    #[test]
    fn tick_skips_mostly_clean_chunks() {
        let policy = Policy {
            full_pct: 50,
            max_age: std::time::Duration::from_secs(3600),
            read_chunks: 4,
        };
        let mut v = mem_volume(DEFAULT_CHUNK, policy);
        v.write(0, 1, &blocks(1, 3)).unwrap();
        v.tick().unwrap();
        assert_eq!(v.dirty_chunks(), 1, "one dirty block should not upload");
    }

    #[test]
    fn ranges_are_merged() {
        let mut d = BTreeMap::new();
        for b in [0u64, 1, 2, 5, 7, 8] {
            d.insert(b, ());
        }
        let r = contiguous_ranges(&d);
        assert_eq!(
            r,
            vec![
                (0, 3 * 4096),
                (5 * 4096, 6 * 4096),
                (7 * 4096, 9 * 4096)
            ]
        );
    }

    #[test]
    fn rejects_bad_size() {
        let r = Volume::open(
            Arc::new(MemStore::new()),
            Box::new(MemMeta::new()),
            Layout::new("v"),
            Some(Create::new(DEFAULT_CHUNK + 1)),
            Policy::default(),
        );
        assert!(r.is_err());
    }

    #[test]
    fn rejects_out_of_range_io() {
        let mut v = mem_volume(DEFAULT_CHUNK, Policy::default());
        assert!(v.write(DEFAULT_CHUNK / BLOCK, 1, &blocks(1, 0)).is_err());
        let mut buf = vec![0u8; BLOCK as usize];
        assert!(v.read(DEFAULT_CHUNK / BLOCK, 1, &mut buf).is_err());
    }
}
