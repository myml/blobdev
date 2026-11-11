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
use crate::readcache::ReadCache;
use crate::store::{Caps, ObjectStore};

pub const BLOCK: u64 = 4096;
pub const DEFAULT_CHUNK: u64 = 8 << 20;

/// Parameters used only when creating a new volume.
#[derive(Debug, Clone, Copy)]
pub struct Create {
    pub size: u64,
    pub chunk_size: Option<u64>,
}

/// Optional on-disk read cache settings.
#[derive(Debug, Clone)]
pub struct DiskCache {
    pub dir: std::path::PathBuf,
    pub max_bytes: u64,
}

impl DiskCache {
    pub fn new(dir: impl Into<std::path::PathBuf>, max_bytes: u64) -> DiskCache {
        DiskCache {
            dir: dir.into(),
            max_bytes,
        }
    }
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
    /// Fetch and cache a whole chunk when a read covers at least this
    /// percentage of it (percent). Smaller reads use a byte-range fetch.
    pub whole_chunk_pct: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            full_pct: 50,
            max_age: std::time::Duration::from_secs(30),
            read_chunks: 64,
            whole_chunk_pct: 25,
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
    /// Optional on-disk cache of clean chunks.
    disk_cache: Option<ReadCache>,
    sent_chunks: u64,
    sent_bytes: u64,
    /// Reads served by a byte-range fetch instead of a whole chunk.
    partial_reads: u64,
}

impl Volume {
    /// Open a volume, creating it when the metadata store is empty.
    ///
    /// `create` supplies the size (and optionally the chunk size) used only
    /// when the volume does not exist yet.
    pub fn open(
        data: Arc<dyn ObjectStore>,
        meta: Box<dyn MetaStore>,
        layout: Layout,
        create: Option<Create>,
        policy: Policy,
    ) -> Result<Volume> {
        Volume::open_with_cache(data, meta, layout, create, policy, None)
    }

    /// Like [`Volume::open`], with an optional on-disk read cache.
    pub fn open_with_cache(
        data: Arc<dyn ObjectStore>,
        mut meta: Box<dyn MetaStore>,
        layout: Layout,
        create: Option<Create>,
        policy: Policy,
        disk: Option<DiskCache>,
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
        let disk_cache = match disk {
            Some(d) if d.max_bytes > 0 => {
                match ReadCache::open(&d.dir, &layout.root, d.max_bytes) {
                    Ok(c) => {
                        log::info!(
                            "read cache {}: {} of {} MiB used, {} chunks",
                            d.dir.display(),
                            c.used_bytes() >> 20,
                            d.max_bytes >> 20,
                            c.entry_count()
                        );
                        Some(c)
                    }
                    Err(e) => {
                        log::warn!("read cache disabled: {e:#}");
                        None
                    }
                }
            }
            _ => None,
        };
        let mut vol = Volume {
            data,
            meta,
            layout,
            caps,
            policy,
            geometry,
            working: HashMap::new(),
            read_cache,
            disk_cache,
            sent_chunks: 0,
            sent_bytes: 0,
            partial_reads: 0,
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

    /// Fetch a whole chunk from the object store; a missing object is a hole.
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

    /// Fetch only `[start, start+want)` of a chunk; a missing object or a
    /// range past its end is a hole (zeros).
    ///
    /// Backends without `range_read` are asked for the whole object and the
    /// slice is cut locally.
    fn fetch_range(&self, id: u64, start: u64, want: u64) -> Result<Vec<u8>> {
        let key = self.layout.chunk_key(id);
        let chunk_len = self.geometry.chunk_len(id);
        if start >= chunk_len {
            return Ok(vec![0u8; want as usize]);
        }
        let end = (start + want).min(chunk_len);
        let n = (end - start) as usize;

        if !self.caps.range_read {
            let mut whole = match self.data.get(&key, None) {
                Ok(v) => v,
                Err(e) => {
                    if self.data.stat(&key)?.is_none() {
                        return Ok(vec![0u8; want as usize]);
                    }
                    return Err(e).with_context(|| format!("fetch chunk {id}"));
                }
            };
            whole.resize(chunk_len as usize, 0);
            let mut out = vec![0u8; want as usize];
            let avail = (whole.len() as u64).saturating_sub(start).min(n as u64) as usize;
            out[..avail].copy_from_slice(&whole[start as usize..start as usize + avail]);
            return Ok(out);
        }

        let got = match self.data.get(&key, Some(start..end)) {
            Ok(v) => v,
            Err(e) => {
                if self.data.stat(&key)?.is_none() {
                    return Ok(vec![0u8; want as usize]);
                }
                return Err(e).with_context(|| format!("fetch chunk {id} range {start}..{end}"));
            }
        };
        let mut out = vec![0u8; want as usize];
        let avail = got.len().min(n);
        out[..avail].copy_from_slice(&got[..avail]);
        Ok(out)
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
    ///
    /// A read large enough to cover a good part of a chunk fetches and caches
    /// the whole chunk, which is what a sequential reader wants. A small read
    /// fetches only its own bytes: on S3, pulling 8 MiB to serve 4 KiB is a
    /// 2048x amplification that also evicts useful cache entries.
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

            // Working copies and cached chunks are already in memory.
            if let Some(w) = self.working.get(&id) {
                if w.data.len() < local as usize + n {
                    anyhow::bail!("chunk {id} shorter than expected");
                }
                out[done..done + n].copy_from_slice(&w.data[local as usize..local as usize + n]);
                done += n;
                off += n as u64;
                continue;
            }
            if let Some(d) = self.read_cache.get(id) {
                if d.len() < local as usize + n {
                    anyhow::bail!("chunk {id} shorter than expected");
                }
                out[done..done + n].copy_from_slice(&d[local as usize..local as usize + n]);
                done += n;
                off += n as u64;
                continue;
            }

            let chunk_len = self.geometry.chunk_len(id);

            // A clean chunk may already be on disk from an earlier run.
            if let Some(mut c) = self.disk_cache.as_mut().and_then(|dc| dc.get(id, chunk_len)) {
                if c.len() as u64 != chunk_len {
                    c.resize(chunk_len as usize, 0);
                }
                out[done..done + n].copy_from_slice(&c[local as usize..local as usize + n]);
                self.read_cache.insert(id, c);
                done += n;
                off += n as u64;
                continue;
            }

            if self.should_cache_whole(chunk_len, n as u64) {
                let c = self.fetch_chunk(id, chunk_len)?;
                self.read_cache.insert(id, c.clone());
                if let Some(dc) = self.disk_cache.as_mut() {
                    if let Err(e) = dc.put(id, &c) {
                        log::debug!("read cache store of chunk {id} failed: {e:#}");
                    }
                }
                out[done..done + n].copy_from_slice(&c[local as usize..local as usize + n]);
            } else {
                let piece = self.fetch_range(id, local, n as u64)?;
                out[done..done + n].copy_from_slice(&piece[..n]);
                self.partial_reads += 1;
            }
            done += n;
            off += n as u64;
        }
        Ok(())
    }

    /// Whether a read of `want` bytes inside a `chunk_len` chunk is big enough
    /// to justify pulling and caching the whole chunk.
    fn should_cache_whole(&self, chunk_len: u64, want: u64) -> bool {
        if chunk_len == 0 {
            return false;
        }
        // Treat it as sequential IO once the request covers this much of the
        // chunk; below that we save bandwidth at the cost of a cache miss.
        want.saturating_mul(100) >= chunk_len * self.policy.whole_chunk_pct
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
            if let Some(dc) = self.disk_cache.as_mut() {
                dc.remove(id);
            }
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
        if let Some(dc) = self.disk_cache.as_mut() {
            // The uploaded bytes are authoritative; keep them cached.
            if let Err(e) = dc.put(id, &w.data) {
                log::debug!("read cache store of chunk {id} failed: {e:#}");
            }
        }
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

    /// How many reads used a byte-range fetch rather than a whole chunk.
    pub fn partial_reads(&self) -> u64 {
        self.partial_reads
    }

    /// On-disk read cache usage as `(used_bytes, max_bytes)`; `None` when the
    /// disk cache is disabled.
    pub fn disk_cache_usage(&self) -> Option<(u64, u64)> {
        self.disk_cache.as_ref().map(|c| (c.used_bytes(), c.max_bytes()))
    }

    /// Disk read cache hit/miss counters.
    pub fn disk_cache_stats(&self) -> Option<(u64, u64)> {
        self.disk_cache.as_ref().map(|c| (c.hits(), c.misses()))
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

    /// Populate a volume, flush, then reopen it with empty caches so the next
    /// read has to come from the object store.
    fn populate_and_reopen(
        store: Arc<MemStore>,
        dir: &std::path::Path,
        size: u64,
        fill: impl FnOnce(&mut Volume),
    ) -> Volume {
        let meta = dir.join("meta");
        {
            let mut v = Volume::open(
                store.clone(),
                Box::new(FileMeta::new(&meta)),
                Layout::new("v"),
                Some(Create::new(size)),
                Policy::default(),
            )
            .unwrap();
            fill(&mut v);
            v.flush().unwrap();
        }
        Volume::open(
            store,
            Box::new(FileMeta::new(&meta)),
            Layout::new("v"),
            None,
            Policy::default(),
        )
        .unwrap()
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
            ..Policy::default()
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
            ..Policy::default()
        };
        let mut v = mem_volume(DEFAULT_CHUNK, policy);
        v.write(0, 1, &blocks(1, 3)).unwrap();
        v.tick().unwrap();
        assert_eq!(v.dirty_chunks(), 1, "one dirty block should not upload");
    }

    /// A small read must not pull a whole chunk: on S3 that is 8 MiB of
    /// traffic and cache churn for one 4 KiB block.
    #[test]
    fn small_read_uses_range_fetch() {
        let dir = std::env::temp_dir().join(format!("blobdev-rr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(MemStore::new());
        let mut v = populate_and_reopen(store.clone(), &dir, DEFAULT_CHUNK, |v| {
            v.write(0, 1, &blocks(1, 0x33)).unwrap();
        });
        let before = store.fetched_bytes();
        let mut buf = vec![0u8; BLOCK as usize];
        v.read(5, 1, &mut buf).unwrap();
        let fetched = store.fetched_bytes() - before;
        assert_eq!(buf[0], 0, "block 5 was never written");
        assert!(
            fetched <= BLOCK,
            "a 4 KiB read fetched {fetched} bytes; expected a range fetch"
        );
        assert_eq!(v.partial_reads(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A large (sequential) read still pulls and caches the whole chunk, so a
    /// second read of the same chunk needs no network at all.
    #[test]
    fn large_read_caches_whole_chunk() {
        let dir = std::env::temp_dir().join(format!("blobdev-lr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(MemStore::new());
        let half = (DEFAULT_CHUNK / BLOCK / 2) as usize;
        let mut v = populate_and_reopen(store.clone(), &dir, DEFAULT_CHUNK, |v| {
            v.write(0, half, &blocks(half, 0x44)).unwrap();
        });

        let mut buf = vec![0u8; half * BLOCK as usize];
        v.read(0, half, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x44));
        let first = store.fetched_bytes();
        assert!(first >= DEFAULT_CHUNK, "sequential read should pull the chunk");

        v.read(0, half, &mut buf).unwrap();
        assert_eq!(store.fetched_bytes(), first, "second read caused IO");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Range fetches must not read past the end of a short final chunk.
    #[test]
    fn range_read_past_chunk_end_is_zero() {
        let dir = std::env::temp_dir().join(format!("blobdev-re-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(MemStore::new());
        let size = DEFAULT_CHUNK + 4 * BLOCK; // final chunk is 4 blocks
        let mut v = populate_and_reopen(store, &dir, size, |_| {});
        let mut buf = vec![0xAAu8; 4 * BLOCK as usize];
        v.read(DEFAULT_CHUNK / BLOCK, 4, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The on-disk cache makes a clean chunk survive a restart.
    #[test]
    fn disk_cache_serves_after_reopen() {
        let dir = std::env::temp_dir().join(format!("blobdev-dc-{}", std::process::id()));
        let cache_dir = dir.join("cache");
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(MemStore::new());
        let meta = dir.join("meta");

        {
            let mut v = Volume::open(
                store.clone(),
                Box::new(FileMeta::new(&meta)),
                Layout::new("v"),
                Some(Create::new(DEFAULT_CHUNK)),
                Policy::default(),
            )
            .unwrap();
            v.write(0, 1, &blocks(1, 0x66)).unwrap();
            v.flush().unwrap();
            drop(v);
        }
        // First run with the disk cache: a sequential read caches the chunk.
        {
            let mut v = Volume::open_with_cache(
                store.clone(),
                Box::new(FileMeta::new(&meta)),
                Layout::new("v"),
                None,
                Policy::default(),
                Some(DiskCache::new(&cache_dir, 64 << 20)),
            )
            .unwrap();
            let n = (DEFAULT_CHUNK / BLOCK) as usize;
            let mut buf = vec![0u8; n * BLOCK as usize];
            v.read(0, n, &mut buf).unwrap();
            assert_eq!(buf[0], 0x66);
        }
        // Second run: the chunk must come from disk, not the store.
        let mut v = Volume::open_with_cache(
            store.clone(),
            Box::new(FileMeta::new(&meta)),
            Layout::new("v"),
            None,
            Policy::default(),
            Some(DiskCache::new(&cache_dir, 64 << 20)),
        )
        .unwrap();
        let before = store.fetched_bytes();
        let n = (DEFAULT_CHUNK / BLOCK) as usize;
        let mut buf = vec![0u8; n * BLOCK as usize];
        v.read(0, n, &mut buf).unwrap();
        assert_eq!(buf[0], 0x66);
        assert_eq!(
            store.fetched_bytes(),
            before,
            "a restart with a warm disk cache still hit the object store"
        );
        std::fs::remove_dir_all(&dir).ok();
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
