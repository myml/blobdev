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
use crc32fast::hash as crc32;
use std::collections::BTreeMap;
use std::path::Path;

#[allow(dead_code)]
pub const META_MAGIC: u64 = 0x424c_4f42_4d45_5430; // "BLOBMET0"
#[allow(dead_code)]
/// v1: per-chunk `[id][ndirty][uploaded][dirty...]`
/// v2: adds a 32-bit uid after `uploaded`
pub const META_VERSION: u32 = 2;

/// Geometry of a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Geometry {
    pub chunk_size: u64,
    pub size: u64,
}

impl Geometry {
    pub fn chunks(&self) -> u64 {
        self.size / self.chunk_size
    }

    pub fn chunk_blocks(&self) -> u64 {
        self.chunk_size / crate::volume::BLOCK
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
        if version != 1 && version != META_VERSION {
            anyhow::bail!("unsupported metadata version {version}");
        }
        let chunk_size = u64::from_le_bytes(body[12..20].try_into().unwrap());
        let size = u64::from_le_bytes(body[20..28].try_into().unwrap());
        let geometry = Geometry { chunk_size, size };
        geometry.validate()?;

        let count = u64::from_le_bytes(body[28..36].try_into().unwrap());
        let mut chunks = BTreeMap::new();
        let mut off = 36usize;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        let g = Geometry {
            chunk_size: 8 << 20,
            size: 64 << 20,
        };
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
        let small = Geometry {
            chunk_size: 8 << 20,
            size: 1 << 20,
        };
        assert!(small.validate().is_ok());
        assert_eq!(small.chunk_len(0), 1 << 20);
        assert_eq!(small.chunks(), 0, "integer chunk count understates a short tail");

        assert!(Geometry { chunk_size: 0, size: 0 }.validate().is_err());
        // Must end on a block boundary.
        assert!(Geometry { chunk_size: 4096, size: 4096 }.validate().is_ok());
        assert!(Geometry { chunk_size: 4096, size: 4097 }.validate().is_err());
        // Chunk size must be a whole number of blocks.
        assert!(Geometry { chunk_size: 1000, size: 1000 }.validate().is_err());

        // A non-multiple tail: 2.5 chunks.
        let odd = Geometry {
            chunk_size: 8 << 20,
            size: (8 << 20) * 2 + (4 << 20),
        };
        assert!(odd.validate().is_ok());
        assert_eq!(odd.chunk_len(2), 4 << 20);
    }
}
