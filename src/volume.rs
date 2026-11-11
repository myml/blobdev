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

/// Smallest addressable unit, matching the block device's logical block.
pub const BLOCK: u64 = 4096;

/// Logical addressing unit, the way JuiceFS uses its chunk: a write is located
/// by `offset / DEFAULT_CHUNK`, and the chunk never changes size or
/// boundaries. It is *not* a storage object; see [`DEFAULT_OBJECT`].
pub const DEFAULT_CHUNK: u64 = 64 << 20;

/// Physical storage unit: one object holds this many bytes.
///
/// This is the number that sets the write amplification. Changing 4 KiB of a
/// chunk rewrites only the object that covers it, so the cost is
/// `DEFAULT_OBJECT / BLOCK` = 1024, not the whole chunk.
pub const DEFAULT_OBJECT: u64 = 4 << 20;

/// Parameters used only when creating a new volume.
#[derive(Debug, Clone, Copy)]
pub struct Create {
    pub size: u64,
    pub chunk_size: Option<u64>,
    pub object_size: Option<u64>,
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
            object_size: None,
        }
    }

    pub fn with_chunk_size(mut self, chunk_size: u64) -> Create {
        self.chunk_size = Some(chunk_size);
        self
    }

    pub fn with_object_size(mut self, object_size: u64) -> Create {
        self.object_size = Some(object_size);
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

/// Fetches chunks without needing the volume lock.
#[derive(Clone)]
pub struct Fetcher {
    data: Arc<dyn ObjectStore>,
    layout: Layout,
    geometry: Geometry,
    caps: Caps,
    uids: HashMap<u64, u32>,
}

impl Fetcher {
    fn object_key(&self, id: u64) -> String {
        match self.uids.get(&id) {
            Some(uid) => self.layout.chunk_key(*uid),
            None => self.layout.legacy_chunk_key(id),
        }
    }

    /// Execute one planned fetch.
    pub fn fetch(&self, req: &FetchRequest) -> Result<Vec<u8>> {
        if req.whole {
            self.fetch_whole(req.chunk, req.len)
        } else {
            self.fetch_range(req.chunk, req.start, req.len)
        }
    }

    fn fetch_whole(&self, id: u64, len: u64) -> Result<Vec<u8>> {
        let key = self.object_key(id);
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

    fn fetch_range(&self, id: u64, start: u64, want: u64) -> Result<Vec<u8>> {
        let key = self.object_key(id);
        let chunk_len = self.geometry.object_len(id);
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
                return Err(e)
                    .with_context(|| format!("fetch chunk {id} range {start}..{end}"));
            }
        };
        let mut out = vec![0u8; want as usize];
        let avail = got.len().min(n);
        out[..avail].copy_from_slice(&got[..avail]);
        Ok(out)
    }
}

/// What a garbage collection found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Objects still referenced by the metadata.
    pub kept: u64,
    /// Objects nothing references.
    pub orphaned: u64,
    /// Orphans actually removed (0 for a dry run).
    pub deleted: u64,
    /// Bytes held by the orphans.
    pub bytes: u64,
}

/// One fetch the object store has to perform for a planned read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    pub chunk: u64,
    /// Byte offset inside the chunk.
    pub start: u64,
    /// Bytes to fetch from `start`.
    pub len: u64,
    /// Fetch the whole chunk (and cache it) rather than a range.
    pub whole: bool,
}

/// Fetches a read needs before it can be served, plus any range bytes the
/// plan can serve directly.
#[derive(Debug, Clone, Default)]
pub struct ReadPlan {
    pub requests: Vec<FetchRequest>,
}

impl ReadPlan {
    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
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

    /// Object key for one version of a chunk.
    ///
    /// The uid is unique per written object, and an object is never modified
    /// after it is written, so a new version can be uploaded while the old one
    /// is still being served.
    /// Key of one physical object.
    pub fn chunk_key(&self, uid: u32) -> String {
        format!("{}/c/{}", self.root, uid)
    }

    /// The pre-uid layout, where the chunk index doubled as the object name.
    pub fn legacy_chunk_key(&self, id: u64) -> String {
        format!("{}/c/{}", self.root, id)
    }
}

/// A chunk with local modifications.
struct Working {
    /// Chunk contents. Blocks absent from `known` were never read from the
    /// object store, so a whole-object upload must fill them in first.
    data: Vec<u8>,
    /// Block indices whose `data` is valid.
    known: std::collections::BTreeSet<u64>,
    dirty: BTreeMap<u64, ()>,
    since: Instant,
}

impl Working {
    fn full(data: Vec<u8>) -> Working {
        let blocks = data.len().div_ceil(BLOCK as usize) as u64;
        Working {
            data,
            known: (0..blocks).collect(),
            dirty: BTreeMap::new(),
            since: Instant::now(),
        }
    }

    fn partial(len: u64) -> Working {
        Working {
            data: vec![0u8; len as usize],
            known: std::collections::BTreeSet::new(),
            dirty: BTreeMap::new(),
            since: Instant::now(),
        }
    }
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
    /// Object uid currently holding each chunk that exists in the store.
    uids: HashMap<u64, u32>,
    /// Next uid to hand out.
    next_uid: u32,
    /// Objects superseded by a committed upload, awaiting deletion.
    garbage: Vec<String>,
    /// Objects deleted because a newer version replaced them.
    reclaimed: u64,
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
                    object_size: create.object_size.unwrap_or(DEFAULT_OBJECT),
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
            uids: HashMap::new(),
            next_uid: 1,
            garbage: Vec::new(),
            reclaimed: 0,
        };
        vol.restore(snapshot)?;
        Ok(vol)
    }

    /// Rebuild working copies from metadata left by an earlier run, so dirty
    /// blocks recorded there get uploaded on the next flush.
    fn restore(&mut self, snap: Snapshot) -> Result<()> {
        let nchunks = snap.chunks.values().filter(|s| !s.dirty.is_empty()).count();
        let nblocks: usize = snap.chunks.values().map(|s| s.dirty.len()).sum();
        // Every chunk present in the store has a committed object uid.
        for (id, st) in &snap.chunks {
            if st.uploaded {
                self.uids.insert(*id, st.uid);
                self.next_uid = self.next_uid.max(st.uid.saturating_add(1));
            }
        }
        if nchunks > 0 {
            log::info!(
                "restoring {nchunks} modified chunk(s), {nblocks} block(s) pending upload"
            );
            for (id, st) in snap.chunks {
                if st.dirty.is_empty() {
                    continue;
                }
                let len = self.geometry.object_len(id);
                let data = self.fetcher().fetch(&FetchRequest {
                    chunk: id,
                    start: 0,
                    len,
                    whole: true,
                })?;
                let mut w = Working::full(data);
                w.dirty = st.dirty;
                w.since = Instant::now();
                self.working.insert(id, w);
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

    /// Physical object covering a volume offset.
    fn object_of(&self, offset: u64) -> u64 {
        self.geometry.locate(offset).0
    }

    /// Bytes from `offset` to the end of its object.
    fn room_in_object(&self, offset: u64) -> u64 {
        let (obj, within) = self.geometry.locate(offset);
        self.geometry.object_len(obj) - within
    }

    /// A fetcher that runs without holding the volume lock.
    pub fn fetcher(&self) -> Fetcher {
        Fetcher {
            data: self.data.clone(),
            layout: self.layout.clone(),
            geometry: self.geometry.clone(),
            caps: self.caps,
            uids: self.uids.clone(),
        }
    }

    /// Read `blocks` blocks starting at block offset `blk`.
    ///
    /// Anything missing is fetched synchronously through a [`Fetcher`]. The
    /// ublk path prefers [`Volume::plan_read`] + [`Volume::apply_fetch`] so the
    /// network happens outside the volume lock.
    pub fn read(&mut self, blk: u64, blocks: usize, out: &mut [u8]) -> Result<()> {
        let want = blocks as u64 * BLOCK;
        if out.len() as u64 != want {
            anyhow::bail!("read buffer is {} bytes, expected {want}", out.len());
        }
        let end = blk * BLOCK + want;
        if end > self.geometry.size {
            anyhow::bail!("read past end of volume");
        }

        let plan = self.plan_read(blk, blocks)?;
        let mut ranges: Vec<(FetchRequest, Vec<u8>)> = Vec::new();
        if !plan.is_empty() {
            let fetcher = self.fetcher();
            for req in &plan.requests {
                let data = fetcher.fetch(req)?;
                if req.whole {
                    self.apply_fetch(req, data);
                } else {
                    // A range covers part of a chunk; keep it aside and copy it
                    // straight out rather than caching it as a whole chunk.
                    self.partial_reads += 1;
                    ranges.push((req.clone(), data));
                }
            }
        }
        self.copy_out(blk, blocks, out, &ranges)
    }

    /// Copy blocks into `out` from the local copies, which must be complete.
    fn copy_out(
        &mut self,
        blk: u64,
        blocks: usize,
        out: &mut [u8],
        ranges: &[(FetchRequest, Vec<u8>)],
    ) -> Result<()> {
        let end = blk * BLOCK + blocks as u64 * BLOCK;
        let mut off = blk * BLOCK;
        let mut done = 0usize;
        while off < end {
            let id = self.object_of(off);
            let local = self.geometry.locate(off).1 as usize;
            let n = (self.room_in_object(off).min(end - off)) as usize;

            // A freshly fetched range wins over anything cached.
            let covering = ranges.iter().find(|(r, _)| {
                r.chunk == id
                    && r.start <= local as u64
                    && r.start + r.len >= (local + n) as u64
            });
            if let Some((r, data)) = covering {
                let start = local - r.start as usize;
                out[done..done + n].copy_from_slice(&data[start..start + n]);
                done += n;
                off += n as u64;
                continue;
            }

            let src: &[u8] = if let Some(w) = self.working.get(&id) {
                &w.data
            } else if let Some(d) = self.read_cache.get(id) {
                d
            } else {
                anyhow::bail!("object {id} missing from cache after planning a read");
            };
            if src.len() < local + n {
                anyhow::bail!("object {id} shorter than expected");
            }
            out[done..done + n].copy_from_slice(&src[local..local + n]);
            done += n;
            off += n as u64;
        }
        Ok(())
    }

    /// Decide what a read needs from the object store. Takes only local state,
    /// so the caller can drop the lock before fetching.
    pub fn plan_read(&mut self, blk: u64, blocks: usize) -> Result<ReadPlan> {
        let want = blocks as u64 * BLOCK;
        let end = blk * BLOCK + want;
        if end > self.geometry.size {
            anyhow::bail!("read past end of volume");
        }
        let mut plan = ReadPlan::default();
        let mut off = blk * BLOCK;
        while off < end {
            let id = self.object_of(off);
            let local = self.geometry.locate(off).1;
            let n = (self.room_in_object(off).min(end - off)) as usize;

            if self.working.contains_key(&id) || self.read_cache.get(id).is_some() {
                off += n as u64;
                continue;
            }
            let obj_len = self.geometry.object_len(id);
            if let Some(mut c) = self.disk_cache.as_mut().and_then(|dc| dc.get(id, obj_len)) {
                if c.len() as u64 != obj_len {
                    c.resize(obj_len as usize, 0);
                }
                self.read_cache.insert(id, c);
                off += n as u64;
                continue;
            }
            if !plan.requests.iter().any(|r: &FetchRequest| r.chunk == id) {
                let whole = self.should_cache_whole(obj_len, n as u64);
                plan.requests.push(FetchRequest {
                    chunk: id,
                    start: if whole { 0 } else { local },
                    len: if whole { obj_len } else { n as u64 },
                    whole,
                });
            }
            off += n as u64;
        }
        Ok(plan)
    }

    /// Install a completed fetch.
    ///
    /// A whole-chunk fetch populates the chunk cache. A range fetch does not:
    /// it covers only part of the chunk, so caching it would serve zeros for
    /// the rest. Use [`Volume::read`] (or the ublk path) for range reads,
    /// which copies them straight into the caller's buffer.
    ///
    /// The fetch ran without the lock, so the chunk may have gained a working
    /// copy in the meantime. That copy is newer than whatever was just
    /// fetched, so it wins: caching the fetched bytes would resurrect the
    /// previous contents of a chunk that has just been written.
    pub fn apply_fetch(&mut self, req: &FetchRequest, data: Vec<u8>) {
        if self.working.contains_key(&req.chunk) {
            return;
        }
        if !req.whole {
            self.partial_reads += 1;
            return;
        }
        if let Some(dc) = self.disk_cache.as_mut() {
            if let Err(e) = dc.put(req.chunk, &data) {
                log::debug!("read cache store of chunk {} failed: {e:#}", req.chunk);
            }
        }
        self.read_cache.insert(req.chunk, data);
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
            let id = self.object_of(off);
            let local = self.geometry.locate(off).1;
            let n = (self.room_in_object(off).min(end - off)) as usize;
            self.write_into_chunk(id, local, &data[done..done + n])?;
            done += n;
            off += n as u64;
        }
        self.save_meta()
    }

    fn write_into_chunk(&mut self, id: u64, local: u64, data: &[u8]) -> Result<()> {
        if !self.working.contains_key(&id) {
            // A copy already in memory or on disk is the cheapest possible
            // base: reuse it instead of starting from nothing, otherwise a
            // chunk that was just uploaded loses the bytes nobody is
            // rewriting and the next full upload publishes zeros for them.
            let len = self.geometry.object_len(id);
            let cached = {
                let from_mem = self.read_cache.get(id).cloned();
                match from_mem {
                    Some(c) => Some(c),
                    None => self
                        .disk_cache
                        .as_mut()
                        .and_then(|dc| dc.get(id, len)),
                }
            };
            match cached {
                Some(mut c) => {
                    c.resize(len as usize, 0);
                    c.truncate(len as usize);
                    self.working.insert(id, Working::full(c));
                }
                None => {
                    // Nothing cached: start partial. A 4 KiB write must not
                    // pull 4 MiB just to modify it; the untouched blocks are
                    // fetched only when an upload or a read needs them.
                    self.working.insert(id, Working::partial(len));
                }
            }
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
            w.known.insert(b);
        }
        Ok(())
    }

    /// Persist the metadata: the committed object uid of every chunk, plus the
    /// dirty blocks of the chunks still in flight.
    ///
    /// Writing this file is the commit point of an upload: until it lands, the
    /// previous object is still the one readers use.
    fn save_meta(&mut self) -> Result<()> {
        let mut chunks: BTreeMap<u64, ChunkState> = BTreeMap::new();
        for (id, uid) in &self.uids {
            chunks.insert(
                *id,
                ChunkState {
                    dirty: BTreeMap::new(),
                    uploaded: true,
                    uid: *uid,
                },
            );
        }
        for (id, w) in &self.working {
            let committed = self.uids.get(id).copied();
            chunks.insert(
                *id,
                ChunkState {
                    dirty: w.dirty.clone(),
                    uploaded: committed.is_some(),
                    uid: committed.unwrap_or(0),
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
    ///
    /// On a backend that can rewrite part of an object (WebDAV, local dir) the
    /// dirty ranges are patched in place. Otherwise (S3) the chunk is written
    /// as a brand new object under a fresh uid, the metadata is pointed at it,
    /// and only then is the superseded object deleted. That ordering is what
    /// makes an interrupted upload harmless: the old object stays untouched
    /// until the new one is committed.
    fn upload(&mut self, id: u64) -> Result<u64> {
        if !self.working.contains_key(&id) {
            return Ok(0);
        }

        if self.caps.partial_write {
            // Patching needs a base object. If there is none, or if this write
            // covers most of the chunk, the whole object has to be written --
            // and a whole-object write must not publish zeros for blocks this
            // write never touched, so fill them in from the store first.
            let needs_full_put = {
                let key = match self.uids.get(&id) {
                    Some(uid) => self.layout.chunk_key(*uid),
                    None => self.layout.legacy_chunk_key(id),
                };
                let w = self.working.get(&id).unwrap();
                let nearly_all = w.dirty.len() as u64 * BLOCK * 100
                    >= (w.data.len() as u64).max(1) * 90;
                self.data.stat(&key)?.is_none() || nearly_all
            };
            // Either way the bytes come from the working copy, so every block
            // being written has to be present. A partial copy only holds the
            // blocks this session wrote; the rest must come from the store.
            if needs_full_put || self.needs_completion(id) {
                self.complete_working_copy(id)?;
            }
            let key = match self.uids.get(&id) {
                Some(uid) => self.layout.chunk_key(*uid),
                None => self.layout.legacy_chunk_key(id),
            };
            let mut sent = 0u64;
            {
                let w = self.working.get(&id).unwrap();
                let nearly_all = w.dirty.len() as u64 * BLOCK * 100
                    >= (w.data.len() as u64).max(1) * 90;
                if self.data.stat(&key)?.is_none() || nearly_all {
                    self.data.put(&key, &w.data)?;
                    sent += w.data.len() as u64;
                } else {
                    for (start, end) in contiguous_ranges(&w.dirty) {
                        let slice = &w.data[start..end];
                        if let Err(e) = self.data.patch(&key, start as u64, slice) {
                            log::warn!("patch {key} failed ({e}); writing the whole object");
                            self.data.put(&key, &w.data)?;
                            sent += w.data.len() as u64;
                            break;
                        }
                        sent += slice.len() as u64;
                    }
                }
            }
            let w = self.working.remove(&id).unwrap();
            self.cache_uploaded(id, w.data);
            self.save_meta()?;
            self.sent_chunks += 1;
            self.sent_bytes += sent;
            return Ok(sent);
        }

        // S3-style: new object, commit the pointer, then delete the old one.
        self.complete_working_copy(id)?;
        let uid = self.next_uid;
        self.next_uid = self.next_uid.wrapping_add(1).max(1);
        let new_key = self.layout.chunk_key(uid);
        let sent = {
            let w = self.working.get(&id).unwrap();
            self.data.put(&new_key, &w.data)?;
            w.data.len() as u64
        };

        let previous = self.uids.insert(id, uid);
        self.save_meta()?;

        let w = self.working.remove(&id).unwrap();
        self.cache_uploaded(id, w.data);
        if let Some(old) = previous {
            self.garbage.push(if old == 0 {
                self.layout.legacy_chunk_key(id)
            } else {
                self.layout.chunk_key(old)
            });
        }
        self.sent_chunks += 1;
        self.sent_bytes += sent;
        self.collect_garbage();
        Ok(sent)
    }

    /// Fill in blocks of a partial working copy that have never been read, so a
    /// whole-object upload does not write zeros over them.
    /// Whether the working copy is missing blocks it must fill in.
    fn needs_completion(&self, id: u64) -> bool {
        match self.working.get(&id) {
            Some(w) => {
                let blocks = w.data.len().div_ceil(BLOCK as usize) as u64;
                (0..blocks).any(|b| !w.known.contains(&b))
            }
            None => false,
        }
    }

    fn complete_working_copy(&mut self, id: u64) -> Result<()> {
        let Some(w) = self.working.get(&id) else {
            return Ok(());
        };
        log::debug!(
            "object {id}: filling in {} of {} blocks from the store",
            w.known.len(),
            w.data.len().div_ceil(BLOCK as usize)
        );
        let blocks = w.data.len().div_ceil(BLOCK as usize) as u64;
        let missing: Vec<u64> = (0..blocks).filter(|b| !w.known.contains(b)).collect();
        if missing.is_empty() {
            return Ok(());
        }
        let len = w.data.len() as u64;
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for b in missing {
            let start = b * BLOCK;
            let n = (len - start).min(BLOCK);
            match ranges.last_mut() {
                Some((s, l)) if *s + *l == start => *l += n,
                _ => ranges.push((start, n)),
            }
        }
        let fetcher = self.fetcher();
        let mut fetched = Vec::with_capacity(ranges.len());
        for (start, n) in ranges {
            let req = FetchRequest {
                chunk: id,
                start,
                len: n,
                whole: false,
            };
            fetched.push((start, fetcher.fetch(&req)?));
        }
        let w = self.working.get_mut(&id).unwrap();
        for (start, bytes) in fetched {
            let off = start as usize;
            let n = bytes.len().min(w.data.len() - off);
            w.data[off..off + n].copy_from_slice(&bytes[..n]);
            let first = start / BLOCK;
            let last = (start + n as u64).div_ceil(BLOCK);
            for b in first..last {
                w.known.insert(b);
            }
        }
        Ok(())
    }

    /// Move an uploaded working copy into the read caches.
    fn cache_uploaded(&mut self, id: u64, data: Vec<u8>) {
        if let Some(dc) = self.disk_cache.as_mut() {
            if let Err(e) = dc.put(id, &data) {
                log::debug!("read cache store of chunk {id} failed: {e:#}");
            }
        }
        self.read_cache.insert(id, data);
    }

    /// Delete objects superseded by a committed upload.
    ///
    /// A failure is not fatal: the object is unreachable but wastes space.
    fn collect_garbage(&mut self) {
        if self.garbage.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.garbage);
        for key in pending {
            match self.data.delete(&key) {
                Ok(()) => self.reclaimed += 1,
                Err(e) => log::warn!("could not delete superseded object {key}: {e:#}"),
            }
        }
    }

    /// Upload everything; this is what a FLUSH request maps to.
    pub fn flush(&mut self) -> Result<()> {
        let ids: Vec<u64> = self.working.keys().copied().collect();
        for id in ids {
            self.upload(id)?;
        }
        self.save_meta()?;
        self.collect_garbage();
        Ok(())
    }

    /// Upload chunks that are full enough or have been dirty long enough.
    pub fn tick(&mut self) -> Result<()> {
        if self.working.is_empty() {
            return Ok(());
        }
        let mut ready = Vec::new();
        for (id, w) in &self.working {
            let real_blocks = (self.geometry.object_len(*id) / BLOCK).max(1);
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
        self.save_meta()?;
        self.collect_garbage();
        Ok(())
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

    /// Delete chunk objects that the metadata no longer references.
    ///
    /// Writing a new object and committing the pointer is two steps; a crash
    /// between them leaves the new object orphaned. Those objects are
    /// unreachable but still billed, so this reports what it found.
    ///
    /// Only runs when nothing is dirty: an in-flight upload owns objects the
    /// metadata does not point at yet.
    pub fn gc(&mut self, dry_run: bool) -> Result<GcReport> {
        if !self.working.is_empty() {
            anyhow::bail!(
                "refusing to collect garbage with {} chunk(s) still pending upload",
                self.working.len()
            );
        }
        if self.caps.partial_write {
            // Objects are patched in place and named by chunk index, so there
            // is nothing orphaned to reclaim.
            return Ok(GcReport::default());
        }

        let prefix = format!("{}/c/", self.layout.root);
        let live: std::collections::BTreeSet<String> = self
            .uids
            .values()
            .map(|uid| self.layout.chunk_key(*uid))
            .collect();

        let listing = self.data.list(&prefix)?;
        let present: std::collections::BTreeSet<&str> =
            listing.iter().map(|(k, _)| k.as_str()).collect();

        // If the metadata points at an object that is not there, the volume is
        // already inconsistent (restored metadata, manual deletion, a backend
        // that lost data). Deleting anything now would only make it worse, and
        // it would remove the evidence.
        let mut missing: Vec<String> = live
            .iter()
            .filter(|k| !present.contains(k.as_str()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            missing.sort();
            anyhow::bail!(
                "metadata references {} object(s) that are missing, refusing to delete anything: {}",
                missing.len(),
                missing.join(", ")
            );
        }

        let mut report = GcReport::default();
        for (key, size) in listing {
            if live.contains(&key) {
                report.kept += 1;
                continue;
            }
            report.orphaned += 1;
            report.bytes += size;
            if !dry_run {
                self.data.delete(&key)?;
                report.deleted += 1;
            }
        }
        Ok(report)
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
        assert!(
            after_create >= DEFAULT_OBJECT,
            "first upload creates the object ({after_create} bytes)"
        );

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
        // Half of one 4 MiB object, so exactly that object is ready to go.
        let half = (DEFAULT_OBJECT / BLOCK / 2) as usize;
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
        let half = (DEFAULT_OBJECT / BLOCK / 2) as usize;
        let mut v = populate_and_reopen(store.clone(), &dir, DEFAULT_CHUNK, |v| {
            v.write(0, half, &blocks(half, 0x44)).unwrap();
        });

        let mut buf = vec![0u8; half * BLOCK as usize];
        v.read(0, half, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x44));
        let first = store.fetched_bytes();
        assert!(
            first >= DEFAULT_OBJECT,
            "sequential read should pull the object ({first} bytes)"
        );

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
            let n = (DEFAULT_OBJECT / BLOCK) as usize;
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
        let n = (DEFAULT_OBJECT / BLOCK) as usize;
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
