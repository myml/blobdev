//! The backend abstraction.
//!
//! A volume lives in an object store: a flat namespace of keys, each holding
//! an immutable-or-mutable blob. S3, WebDAV, a local directory and an
//! in-memory map all fit. They differ in *capabilities*, which the chunk layer
//! uses to choose its write strategy:
//!
//! | backend     | range read | partial write | conditional write |
//! |-------------|-----------|---------------|-------------------|
//! | S3          | yes       | no            | yes (ETag)        |
//! | WebDAV      | yes       | yes           | yes (ETag)        |
//! | local dir   | yes       | yes           | no                |
//! | memory      | yes       | yes           | no                |
//!
//! With partial writes a dirty 4 KiB block costs a 4 KiB request; without
//! them the whole chunk (default 8 MiB) must be uploaded.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What a backend can do beyond the basics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// `get` honours a byte range.
    pub range_read: bool,
    /// `patch` can rewrite part of an existing object.
    pub partial_write: bool,
    /// Writes can be made conditional on the object's current ETag.
    pub conditional_write: bool,
}

impl Caps {
    /// A plain filesystem-like backend.
    pub const FULL: Caps = Caps {
        range_read: true,
        partial_write: true,
        conditional_write: false,
    };

    /// S3: whole-object replacement only, but ETags are available.
    pub const S3: Caps = Caps {
        range_read: true,
        partial_write: false,
        conditional_write: true,
    };
}

/// Opaque version tag for an object, used for conditional writes.
pub type ETag = String;

/// The backend interface.
///
/// All methods block. The ublk queue thread serves one command per tag, so a
/// synchronous backend keeps the code simple; a backend that wants to overlap
/// uploads does so internally with its own worker.
pub trait ObjectStore: Send + Sync {
    /// Capability declaration; the chunk layer adapts to it.
    fn caps(&self) -> Caps;

    /// Size in bytes, or `None` if the key does not exist.
    fn stat(&self, key: &str) -> Result<Option<u64>>;

    /// Fetch a key. With `caps().range_read`, `range` limits the transfer;
    /// without it, implementations may return the whole object.
    fn get(&self, key: &str, range: Option<Range<u64>>) -> Result<Vec<u8>>;

    /// Replace the whole object and return its new ETag if the backend has one.
    fn put(&self, key: &str, data: &[u8]) -> Result<Option<ETag>>;

    /// Rewrite bytes `[offset, offset+data.len())` of an existing object.
    ///
    /// Only called when `caps().partial_write`; the default returns an error.
    fn patch(&self, key: &str, offset: u64, data: &[u8]) -> Result<()> {
        let _ = (key, offset, data);
        anyhow::bail!("backend does not support partial writes")
    }

    fn delete(&self, key: &str) -> Result<()>;

    /// Human-readable backend name, for logs.
    fn name(&self) -> &str {
        "object store"
    }
}

// ---------------------------------------------------------------------------
// Local directory backend (tests and local experiments)
// ---------------------------------------------------------------------------

pub struct DirStore {
    root: PathBuf,
}

impl DirStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<DirStore> {
        let root = root.into();
        fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
        Ok(DirStore { root })
    }

    fn path(&self, key: &str) -> PathBuf {
        debug_assert!(!key.contains(".."), "keys are generated internally");
        self.root.join(key)
    }
}

impl ObjectStore for DirStore {
    fn caps(&self) -> Caps {
        Caps::FULL
    }

    fn stat(&self, key: &str) -> Result<Option<u64>> {
        let p = self.path(key);
        match fs::metadata(&p) {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("stat {}", p.display())),
        }
    }

    fn get(&self, key: &str, range: Option<Range<u64>>) -> Result<Vec<u8>> {
        let p = self.path(key);
        let all = fs::read(&p).with_context(|| format!("read {}", p.display()))?;
        Ok(match range {
            Some(r) => {
                let start = (r.start as usize).min(all.len());
                let end = (r.end as usize).min(all.len());
                all[start..end].to_vec()
            }
            None => all,
        })
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<Option<ETag>> {
        let p = self.path(key);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        // Write-then-rename so a reader never sees a partial object.
        let tmp = p.with_extension("tmp");
        fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &p).with_context(|| format!("rename into {}", p.display()))?;
        Ok(None)
    }

    fn patch(&self, key: &str, offset: u64, data: &[u8]) -> Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let p = self.path(key);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .with_context(|| format!("open {} for patch", p.display()))?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)?;
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<()> {
        match fs::remove_file(self.path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("delete object"),
        }
    }

    fn name(&self) -> &str {
        "directory"
    }
}

// ---------------------------------------------------------------------------
// In-memory backend (unit tests)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct MemStore {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Bytes transferred, so tests can assert on IO volume.
    pub fetched: Mutex<u64>,
    pub stored: Mutex<u64>,
}

impl MemStore {
    pub fn new() -> MemStore {
        MemStore::default()
    }

    pub fn fetched_bytes(&self) -> u64 {
        *self.fetched.lock().unwrap()
    }

    pub fn stored_bytes(&self) -> u64 {
        *self.stored.lock().unwrap()
    }

    pub fn keys(&self) -> Vec<String> {
        self.objects.lock().unwrap().keys().cloned().collect()
    }
}

impl ObjectStore for MemStore {
    fn caps(&self) -> Caps {
        Caps::FULL
    }

    fn stat(&self, key: &str) -> Result<Option<u64>> {
        Ok(self.objects.lock().unwrap().get(key).map(|d| d.len() as u64))
    }

    fn get(&self, key: &str, range: Option<Range<u64>>) -> Result<Vec<u8>> {
        let g = self.objects.lock().unwrap();
        let Some(d) = g.get(key) else {
            anyhow::bail!("key not found: {key}");
        };
        Ok(match range {
            Some(r) => {
                let start = (r.start as usize).min(d.len());
                let end = (r.end as usize).min(d.len());
                let out = d[start..end].to_vec();
                *self.fetched.lock().unwrap() += out.len() as u64;
                out
            }
            None => {
                *self.fetched.lock().unwrap() += d.len() as u64;
                d.clone()
            }
        })
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<Option<ETag>> {
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), data.to_vec());
        *self.stored.lock().unwrap() += data.len() as u64;
        Ok(None)
    }

    fn patch(&self, key: &str, offset: u64, data: &[u8]) -> Result<()> {
        let mut g = self.objects.lock().unwrap();
        let obj = g.get_mut(key).context("patch on missing object")?;
        let start = offset as usize;
        if start + data.len() > obj.len() {
            anyhow::bail!("patch past end of object");
        }
        obj[start..start + data.len()].copy_from_slice(data);
        *self.stored.lock().unwrap() += data.len() as u64;
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.objects.lock().unwrap().remove(key);
        Ok(())
    }

    fn name(&self) -> &str {
        "memory"
    }
}

#[allow(dead_code)]
fn _unused(_: &Path) {}
