//! Metadata layer.
//!
//! The volume's *metadata* is deliberately separate from its *data*, the way
//! JuiceFS separates its metadata engine from its object store:
//!
//! * metadata = geometry, plus which chunk is stored under which object key
//!   and which blocks of it are still dirty locally;
//! * data     = the chunk objects themselves.
//!
//! Keeping them apart buys two things. A write only touches local metadata
//! (cheap, never a network round trip), and the metadata backend can be
//! swapped: an in-memory map for tests, a local file for crash safety, or a
//! database / Redis for shared access later.
//!
//! The metadata itself is versioned and CRC-protected so a partially written
//! snapshot is detected instead of trusted.

use anyhow::{Context, Result};
use crate::store::ObjectStore;
use crc32fast::hash as crc32;
use std::collections::BTreeMap;
use std::path::Path;

#[allow(dead_code)]
pub const META_MAGIC: u64 = 0x424c_4f42_4d45_5430; // "BLOBMET0"
#[allow(dead_code)]
/// v1: per-chunk `[id][ndirty][uploaded][dirty...]`
/// v2: adds a 32-bit uid after `uploaded`
/// v3: adds an explicit object size to the geometry
pub const META_VERSION: u32 = 3;

/// Geometry of a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Geometry {
    /// Logical addressing unit. Fixed for the life of the volume; changing it
    /// would move every object boundary.
    pub chunk_size: u64,
    /// Physical object size. Also fixed, for the same reason.
    pub object_size: u64,
    pub size: u64,
}

impl Geometry {
    pub fn chunks(&self) -> u64 {
        self.size / self.chunk_size
    }

    pub fn chunk_blocks(&self) -> u64 {
        self.chunk_size / crate::volume::BLOCK
    }

    /// Number of objects one chunk is stored as.
    pub fn objects_per_chunk(&self) -> u64 {
        self.chunk_size / self.object_size
    }

    /// Object index and byte offset within it for a volume offset.
    pub fn locate(&self, offset: u64) -> (u64, u64) {
        let chunk = offset / self.chunk_size;
        let in_chunk = offset % self.chunk_size;
        let obj_in_chunk = in_chunk / self.object_size;
        (
            chunk * self.objects_per_chunk() + obj_in_chunk,
            in_chunk % self.object_size,
        )
    }

    /// Length of object `obj` in bytes (the last one may be short).
    pub fn object_len(&self, obj: u64) -> u64 {
        let start = obj * self.object_size;
        self.size.saturating_sub(start).min(self.object_size)
    }

    /// Length of chunk `id` in bytes (the last chunk may be short).
    pub fn chunk_len(&self, id: u64) -> u64 {
        let start = id * self.chunk_size;
        self.size.saturating_sub(start).min(self.chunk_size)
    }

    pub fn validate(&self) -> Result<()> {
        if self.chunk_size == 0 || self.size == 0 {
            anyhow::bail!("volume geometry has a zero dimension");
        }
        // The final chunk may be short; it only has to end on a block boundary.
        if self.size % crate::volume::BLOCK != 0 {
            anyhow::bail!(
                "volume size {} is not a multiple of the {} byte block",
                self.size,
                crate::volume::BLOCK
            );
        }
        if self.chunk_size % crate::volume::BLOCK != 0 {
            anyhow::bail!("chunk size must be a multiple of {}", crate::volume::BLOCK);
        }
        if self.object_size == 0 || self.object_size % crate::volume::BLOCK != 0 {
            anyhow::bail!("object size must be a multiple of {}", crate::volume::BLOCK);
        }
        if self.chunk_size % self.object_size != 0 {
            anyhow::bail!(
                "chunk size {} must be a multiple of object size {}",
                self.chunk_size,
                self.object_size
            );
        }
        Ok(())
    }
}

/// Block-level description of one chunk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChunkState {
    /// Blocks modified since the last upload, relative to the chunk start.
    pub dirty: BTreeMap<u64, ()>,
    /// Set once the chunk object exists in the object store.
    pub uploaded: bool,
    /// Which data object currently holds this chunk, as a per-volume object id.
    ///
    /// Chunk objects are written under `<uid>` and never modified, so an
    /// upload is a three step operation: write the new object, point this
    /// field at it (the commit point), then delete the old object.
    pub uid: u32,
}

impl ChunkState {
    pub fn is_fully_dirty(&self, blocks: u64) -> bool {
        self.dirty.len() as u64 >= blocks
    }
}

/// Whole-volume metadata snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub geometry: Geometry,
    pub chunks: BTreeMap<u64, ChunkState>,
}

impl Snapshot {
    pub fn new(geometry: Geometry) -> Snapshot {
        Snapshot {
            geometry,
            chunks: BTreeMap::new(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64);
        b.extend_from_slice(&META_MAGIC.to_le_bytes());
        b.extend_from_slice(&META_VERSION.to_le_bytes());
        b.extend_from_slice(&self.geometry.chunk_size.to_le_bytes());
        b.extend_from_slice(&self.geometry.object_size.to_le_bytes());
        b.extend_from_slice(&self.geometry.size.to_le_bytes());
        b.extend_from_slice(&(self.chunks.len() as u64).to_le_bytes());
        for (id, st) in &self.chunks {
            b.extend_from_slice(&id.to_le_bytes());
            b.extend_from_slice(&(st.dirty.len() as u64).to_le_bytes());
            b.extend_from_slice(&[st.uploaded as u8]);
            b.extend_from_slice(&st.uid.to_le_bytes());
            for blk in st.dirty.keys() {
                b.extend_from_slice(&blk.to_le_bytes());
            }
        }
        let c = crc32(&b);
        b.extend_from_slice(&c.to_le_bytes());
        b
    }

    pub fn decode(bytes: &[u8]) -> Result<Snapshot> {
        if bytes.len() < 36 {
            anyhow::bail!("metadata snapshot too short ({} bytes)", bytes.len());
        }
        let (body, tail) = bytes.split_at(bytes.len() - 4);
        let stored = u32::from_le_bytes(tail.try_into().unwrap());
        if crc32(body) != stored {
            anyhow::bail!("metadata snapshot CRC mismatch (torn write?)");
        }
        if u64::from_le_bytes(body[0..8].try_into().unwrap()) != META_MAGIC {
            anyhow::bail!("bad metadata magic");
        }
        let version = u32::from_le_bytes(body[8..12].try_into().unwrap());
        if !(1..=META_VERSION).contains(&version) {
            anyhow::bail!("unsupported metadata version {version}");
        }
        let chunk_size = u64::from_le_bytes(body[12..20].try_into().unwrap());
        // v1/v2 had no object size: the chunk itself was the object.
        let object_size = if version >= 3 {
            u64::from_le_bytes(body[20..28].try_into().unwrap())
        } else {
            chunk_size
        };
        let size = u64::from_le_bytes(body[28..36].try_into().unwrap());
        let geometry = Geometry {
            chunk_size,
            object_size,
            size,
        };
        geometry.validate()?;

        let count = u64::from_le_bytes(body[36..44].try_into().unwrap());
        let mut chunks = BTreeMap::new();
        let mut off = 44usize;
        for _ in 0..count {
            if off + 17 > body.len() {
                anyhow::bail!("metadata snapshot truncated");
            }
            let id = u64::from_le_bytes(body[off..off + 8].try_into().unwrap());
            let ndirty = u64::from_le_bytes(body[off + 8..off + 16].try_into().unwrap());
            let uploaded = body[off + 16] != 0;
            off += 17;
            // v1 had no uid; such a volume stored chunk <id> directly, which we
            // treat as uid 0 so the legacy key still resolves.
            let uid = if version >= 2 {
                if off + 4 > body.len() {
                    anyhow::bail!("metadata snapshot truncated in chunk {id}");
                }
                let g = u32::from_le_bytes(body[off..off + 4].try_into().unwrap());
                off += 4;
                g
            } else {
                0
            };
            let mut dirty = BTreeMap::new();
            for _ in 0..ndirty {
                if off + 8 > body.len() {
                    anyhow::bail!("metadata snapshot truncated in chunk {id}");
                }
                let blk = u64::from_le_bytes(body[off..off + 8].try_into().unwrap());
                off += 8;
                dirty.insert(blk, ());
            }
            chunks.insert(
                id,
                ChunkState {
                    dirty,
                    uploaded,
                    uid,
                },
            );
        }
        Ok(Snapshot { geometry, chunks })
    }
}

/// Where metadata lives.
pub trait MetaStore: Send {
    fn load(&self) -> Result<Option<Snapshot>>;
    fn save(&mut self, snap: &Snapshot) -> Result<()>;

    fn name(&self) -> &str {
        "metadata"
    }
}

/// Metadata held only in memory. Fast; loses unsent state when the process dies.
#[derive(Default)]
pub struct MemMeta {
    snap: Option<Snapshot>,
}

impl MemMeta {
    pub fn new() -> MemMeta {
        MemMeta::default()
    }
}

impl MetaStore for MemMeta {
    fn load(&self) -> Result<Option<Snapshot>> {
        Ok(self.snap.clone())
    }

    fn save(&mut self, snap: &Snapshot) -> Result<()> {
        self.snap = Some(snap.clone());
        Ok(())
    }

    fn name(&self) -> &str {
        "memory"
    }
}

/// Metadata in a local file, written atomically (temp file + rename).
///
/// This is what makes a crashed process recoverable: the snapshot points at
/// the chunk blocks that still need uploading.
pub struct FileMeta {
    path: std::path::PathBuf,
    /// Held for the lifetime of the volume so two processes cannot both
    /// believe they own it. Never read: dropping it releases the lock.
    #[allow(dead_code)]
    lock: Option<crate::lockfile::FileLock>,
}

impl FileMeta {
    /// Metadata in `path`, without taking the exclusive lock.
    ///
    /// Used by one-shot commands (`gc`, `inspect`) that must be able to run
    /// while nothing else owns the volume; the daemon path uses
    /// [`FileMeta::locked`].
    pub fn new(path: impl Into<std::path::PathBuf>) -> FileMeta {
        FileMeta {
            path: path.into(),
            lock: None,
        }
    }

    /// Metadata in `path`, taking an exclusive lock on `<path>.lock`.
    ///
    /// Two `blobdev run` processes against one volume would each write their
    /// own metadata and their own chunk objects, and the last writer would
    /// silently discard the other's work.
    pub fn locked(path: impl Into<std::path::PathBuf>) -> Result<FileMeta> {
        let path = path.into();
        let lock_path = path.with_extension("lock");
        if let Some(parent) = lock_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let lock = crate::lockfile::FileLock::try_acquire(&lock_path)
            .with_context(|| format!("lock volume metadata {}", path.display()))?;
        Ok(FileMeta {
            path,
            lock: Some(lock),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl MetaStore for FileMeta {
    fn load(&self) -> Result<Option<Snapshot>> {
        match std::fs::read(&self.path) {
            Ok(b) => Ok(Some(
                Snapshot::decode(&b).with_context(|| {
                    format!("parse metadata {}", self.path.display())
                })?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", self.path.display())),
        }
    }

    fn save(&mut self, snap: &Snapshot) -> Result<()> {
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, snap.encode())
            .with_context(|| format!("write {}", tmp.display()))?;
        // fsync before rename so the rename cannot expose an empty file.
        {
            let f = std::fs::File::open(&tmp)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename into {}", self.path.display()))?;
        Ok(())
    }

    fn name(&self) -> &str {
        "file"
    }
}

/// Metadata on local disk, with a copy in the object store.
///
/// A volume's metadata lives next to the daemon, which is one disk away from
/// being unrecoverable: if that disk dies, the objects are still in the store
/// and nothing can name them. Measured before this existed -- deleting the
/// metadata file left a volume that refused to open ("no metadata; pass a
/// size to create it"), and taking that advice built an empty volume over
/// data that was still there.
///
/// Saves go to both; loads prefer the local file and fall back to the store.
/// The snapshot is a few hundred bytes, so the extra put is cheap, and it is
/// written after the local one, so a store outage never loses a local save.
pub struct MirroredMeta {
    local: FileMeta,
    remote: std::sync::Arc<dyn ObjectStore>,
    key: String,
}

impl MirroredMeta {
    pub fn new(
        local: FileMeta,
        remote: std::sync::Arc<dyn ObjectStore>,
        key: String,
    ) -> MirroredMeta {
        MirroredMeta {
            local,
            remote,
            key,
        }
    }

    /// Where the copy is kept, for messages.
    pub fn remote_key(&self) -> &str {
        &self.key
    }
}

impl MetaStore for MirroredMeta {
    fn load(&self) -> Result<Option<Snapshot>> {
        if let Some(snap) = self.local.load()? {
            return Ok(Some(snap));
        }
        // Ask whether the copy exists first: a plain get() reports a missing
        // object and an unreachable store the same way, and one of those is a
        // new volume while the other is an outage.
        match self.remote.stat(&self.key) {
            Ok(None) => Ok(None),
            Ok(Some(_)) => {
                let b = self
                    .remote
                    .get(&self.key, None)
                    .with_context(|| format!("read the metadata copy at {}", self.key))?;
                let snap = Snapshot::decode(&b).with_context(|| {
                    format!("parse the metadata copy at {}", self.key)
                })?;
                log::warn!(
                    "local metadata is missing; recovered it from {} ({} bytes). \
                     The local file will be written again on the next save",
                    self.key,
                    b.len()
                );
                Ok(Some(snap))
            }
            Err(e) => Err(e)
                .with_context(|| format!("look for the metadata copy at {}", self.key)),
        }
    }

    fn save(&mut self, snap: &Snapshot) -> Result<()> {
        self.local.save(snap)?;
        let bytes = snap.encode();
        if let Err(e) = self.remote.put(&self.key, &bytes) {
            // The local save already succeeded, so the volume is consistent;
            // the copy is what protects against losing this disk.
            log::warn!("could not update the metadata copy at {}: {e:#}", self.key);
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "file+store"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom(chunk_size: u64, size: u64) -> Geometry {
        Geometry {
            chunk_size,
            object_size: chunk_size.min(4 << 20),
            size,
        }
    }

    fn sample() -> Snapshot {
        let g = geom(8 << 20, 64 << 20);
        let mut s = Snapshot::new(g);
        let mut st = ChunkState::default();
        st.dirty.insert(0, ());
        st.dirty.insert(1, ());
        st.dirty.insert(9, ());
        s.chunks.insert(3, st);
        let up = ChunkState {
            uploaded: true,
            ..Default::default()
        };
        s.chunks.insert(4, up);
        s
    }

    #[test]
    fn snapshot_roundtrip() {
        let s = sample();
        let b = s.encode();
        let back = Snapshot::decode(&b).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.geometry.chunks(), 8);
        assert_eq!(back.geometry.chunk_blocks(), 2048);
    }

    #[test]
    fn corrupt_snapshot_rejected() {
        let mut b = sample().encode();
        let n = b.len();
        b[n / 2] ^= 0xFF;
        assert!(Snapshot::decode(&b).is_err(), "corruption must be detected");
    }

    #[test]
    fn truncated_snapshot_rejected() {
        let b = sample().encode();
        for cut in [0usize, 10, 40, b.len() - 1] {
            assert!(Snapshot::decode(&b[..cut]).is_err(), "short {cut} accepted");
        }
    }

    #[test]
    fn file_meta_roundtrip() {
        let dir = std::env::temp_dir().join(format!("blobdev-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("meta");
        let mut m = FileMeta::new(&p);
        assert!(m.load().unwrap().is_none());
        m.save(&sample()).unwrap();
        let back = m.load().unwrap().unwrap();
        assert_eq!(back, sample());
        // A stale temp file must not be read back.
        std::fs::write(p.with_extension("tmp"), b"junk").unwrap();
        assert_eq!(m.load().unwrap().unwrap(), sample());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn geometry_validation() {
        // A volume smaller than one chunk is fine: the only chunk is short.
        let small = geom(8 << 20, 1 << 20);
        assert!(small.validate().is_ok());
        assert_eq!(small.chunk_len(0), 1 << 20);
        assert_eq!(small.chunks(), 0, "integer chunk count understates a short tail");

        assert!(geom(0, 0).validate().is_err());
        // Must end on a block boundary.
        assert!(geom(4096, 4096).validate().is_ok());
        assert!(geom(4096, 4097).validate().is_err());
        // Chunk size must be a whole number of blocks.
        assert!(geom(1000, 1000).validate().is_err());

        // A non-multiple tail: 2.5 chunks.
        let odd = geom(8 << 20, (8 << 20) * 2 + (4 << 20));
        assert!(odd.validate().is_ok());
        assert_eq!(odd.chunk_len(2), 4 << 20);
    }
}

#[cfg(test)]
mod size_tests {
    use super::*;

    /// How big does the metadata get for a realistically sized volume?
    ///
    /// It is rewritten whole on every write, so its size is the write
    /// amplification of the metadata path: 1 TiB with every object dirty means
    /// 4.5 MB of file per 4 KiB block written.
    #[test]
    fn metadata_size_for_a_large_volume() {
        for size_gib in [64u64, 256, 1024, 4096] {
            let size = size_gib << 30;
            let object_size = 4u64 << 20;
            let geometry = Geometry {
                chunk_size: 64 << 20,
                object_size,
                size,
            };
            let objects = size.div_ceil(object_size);
            let mut snap = Snapshot::new(geometry);
            for id in 0..objects {
                snap.chunks.insert(
                    id,
                    ChunkState {
                        dirty: BTreeMap::new(),
                        uploaded: true,
                        uid: 1,
                    },
                );
            }
            let clean = snap.encode().len();
            // Worst case: every object has one dirty block.
            for st in snap.chunks.values_mut() {
                st.dirty.insert(0, ());
            }
            let dirty = snap.encode().len();
            println!(
                "{size_gib:>5} GiB: {objects:>7} objects, {} KiB clean, {} KiB with a dirty block",
                clean >> 10,
                dirty >> 10
            );
        }
    }
}
