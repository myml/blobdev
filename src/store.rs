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
    /// A ranged read costs about as much as reading the whole object.
    ///
    /// True for servers that pay a fixed price per request regardless of how
    /// many bytes come back. Nextcloud is one: a 4 KiB range takes the same
    /// 250 ms as the whole 4 MiB object, because PHP bootstraps per request.
    /// Asking for tiny ranges there turns a sequential read into thousands of
    /// round trips -- 24 MiB took 52 seconds, 0.5 MB/s, until this was known.
    pub expensive_range_read: bool,
}

impl Caps {
    /// A plain filesystem-like backend.
    pub const FULL: Caps = Caps {
        range_read: true,
        partial_write: true,
        conditional_write: false,
        expensive_range_read: false,
    };

    /// S3: whole-object replacement only, but ETags are available.
    pub const S3: Caps = Caps {
        range_read: true,
        partial_write: false,
        conditional_write: true,
        expensive_range_read: false,
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
    /// Whether this store can hold a second copy of a volume's metadata.
    ///
    /// Only object stores: they outlive the machine. A directory store is the
    /// machine, so a copy there would sit on the same disk that is going to
    /// fail, which is the case the copy is for.
    fn has_metadata_copy(&self) -> bool {
        self.caps().conditional_write
    }

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

    /// List keys under `prefix`.
    ///
    /// Used to find objects nothing references any more (a crash between
    /// writing a new chunk and committing the metadata leaves one behind).
    /// Backends that cannot list return an empty vector.
    fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let _ = prefix;
        Ok(Vec::new())
    }

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

    fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        fn walk(
            root: &Path,
            dir: &Path,
            prefix: &str,
            out: &mut Vec<(String, u64)>,
        ) -> Result<()> {
            let Ok(rd) = fs::read_dir(dir) else {
                return Ok(());
            };
            for entry in rd {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    walk(root, &path, prefix, out)?;
                    continue;
                }
                let Ok(rel) = path.strip_prefix(root) else {
                    continue;
                };
                let key = rel.to_string_lossy().replace('\\', "/");
                if !key.starts_with(prefix) {
                    continue;
                }
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                out.push((key, size));
            }
            Ok(())
        }
        walk(&self.root, &self.root, prefix, &mut out)?;
        Ok(out)
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

    fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        Ok(self
            .objects
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.clone(), v.len() as u64))
            .collect())
    }

    fn name(&self) -> &str {
        "memory"
    }
}

#[allow(dead_code)]
fn _unused(_: &Path) {}

// ---------------------------------------------------------------------------
// Retries
// ---------------------------------------------------------------------------

/// How aggressively to retry a failed request.
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    /// Total attempts, including the first one. 1 disables retrying.
    pub attempts: u32,
    /// First backoff; doubles on each further retry.
    pub base_delay: std::time::Duration,
    /// Cap on a single backoff.
    pub max_delay: std::time::Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Retry {
            attempts: 4,
            base_delay: std::time::Duration::from_millis(200),
            max_delay: std::time::Duration::from_secs(5),
        }
    }
}

impl Retry {
    pub fn none() -> Retry {
        Retry {
            attempts: 1,
            ..Retry::default()
        }
    }

    /// Delay before attempt `n` (1-based); `None` when out of attempts.
    pub fn delay(&self, attempt: u32) -> Option<std::time::Duration> {
        if attempt >= self.attempts {
            return None;
        }
        let factor = 1u32 << (attempt.saturating_sub(1)).min(10);
        let d = self.base_delay.saturating_mul(factor);
        Some(d.min(self.max_delay))
    }

    /// Run `f` until it succeeds, it fails with a permanent error, or the
    /// attempts run out.
    ///
    /// Only operations that are safe to repeat should go through this: a PUT
    /// of a fixed object and a GET are, while anything with a side effect that
    /// is not idempotent must not be retried blindly. In this code base every
    /// object write publishes a whole new object, so repeats are harmless.
    pub fn run<T>(&self, what: &str, mut f: impl FnMut() -> Result<T>) -> Result<T> {
        let mut attempt = 1u32;
        loop {
            match f() {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let retryable = is_transient(&e);
                    match self.delay(attempt) {
                        Some(d) if retryable => {
                            log::warn!(
                                "{what}: attempt {attempt}/{} failed ({e:#}); retrying in {d:?}",
                                self.attempts
                            );
                            std::thread::sleep(d);
                            attempt += 1;
                        }
                        _ => {
                            if retryable && attempt > 1 {
                                return Err(e).with_context(|| {
                                    format!("{what}: gave up after {attempt} attempts")
                                });
                            }
                            return Err(e);
                        }
                    }
                }
            }
        }
    }
}

/// A backend failure, classified so the retry loop can decide.
#[derive(Debug)]
pub struct StoreError {
    pub message: String,
    /// True when the request may succeed if repeated (network hiccup, 5xx,
    /// throttling, timeout).
    pub transient: bool,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StoreError {}

impl StoreError {
    /// A failure worth retrying.
    pub fn transient(message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(StoreError {
            message: message.into(),
            transient: true,
        })
    }

    /// A failure that will not fix itself (bad credentials, missing bucket).
    pub fn permanent(message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(StoreError {
            message: message.into(),
            transient: false,
        })
    }
}

/// Whether an error, or any error in its context chain, is worth retrying.
pub fn is_transient(e: &anyhow::Error) -> bool {
    if e.chain().any(|c| {
        c.downcast_ref::<StoreError>()
            .map(|s| s.transient)
            .unwrap_or(false)
    }) {
        return true;
    }
    // The HTTP client reports its own timeouts and resets, which carry no
    // StoreError. They mean the link stalled, not that the request was wrong.
    e.chain().any(|c| {
        if let Some(io) = c.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::{
                BrokenPipe, ConnectionAborted, ConnectionReset, TimedOut, UnexpectedEof, WouldBlock,
            };
            // BrokenPipe and UnexpectedEof belong here for the same reason as a
            // reset: the peer went away mid-request. A store that answers 503
            // and hangs up produces exactly that.
            return matches!(
                io.kind(),
                TimedOut | WouldBlock | ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof
            );
        }
        let msg = c.to_string().to_ascii_lowercase();
        msg.contains("timeout")
            || msg.contains("timed out")
            || msg.contains("broken pipe")
            || msg.contains("connection reset")
            || msg.contains("unexpected eof")
            // ureq reports a connection that died before a reply as this.
            || msg.contains("peer disconnected")
            || msg.contains("peer closed")
            || msg.contains("connection closed")
    })
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn retries_transient_failures() {
        let r = Retry {
            attempts: 4,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        };
        let n = AtomicU32::new(0);
        let out: Result<u32> = r.run("op", || {
            let c = n.fetch_add(1, Ordering::SeqCst) + 1;
            if c < 3 {
                Err(StoreError::transient("boom"))
            } else {
                Ok(c)
            }
        });
        assert_eq!(out.unwrap(), 3);
        assert_eq!(n.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn permanent_failures_are_not_retried() {
        let r = Retry {
            attempts: 8,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        };
        let n = AtomicU32::new(0);
        let out: Result<u32> = r.run("op", || {
            n.fetch_add(1, Ordering::SeqCst);
            Err(StoreError::permanent("403 Forbidden"))
        });
        assert!(out.is_err());
        assert_eq!(
            n.load(Ordering::SeqCst),
            1,
            "a permanent error must fail on the first attempt"
        );
    }

    #[test]
    fn retries_stop_after_the_budget() {
        let r = Retry {
            attempts: 3,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        };
        let n = AtomicU32::new(0);
        let out: Result<u32> = r.run("op", || {
            n.fetch_add(1, Ordering::SeqCst);
            Err(StoreError::transient("still down"))
        });
        let msg = format!("{:#}", out.unwrap_err());
        assert_eq!(n.load(Ordering::SeqCst), 3);
        assert!(msg.contains("gave up"), "unhelpful error: {msg}");
    }

    #[test]
    fn transient_survives_context() {
        let e = StoreError::transient("network");
        let wrapped = e.context("uploading chunk 3");
        assert!(is_transient(&wrapped));
        assert!(!is_transient(&anyhow::anyhow!("plain error")));
        assert!(!is_transient(&StoreError::permanent("nope")));
    }

    #[test]
    fn backoff_grows_and_caps() {
        let r = Retry {
            attempts: 10,
            base_delay: std::time::Duration::from_millis(100),
            max_delay: std::time::Duration::from_millis(500),
        };
        assert_eq!(r.delay(1), Some(std::time::Duration::from_millis(100)));
        assert_eq!(r.delay(2), Some(std::time::Duration::from_millis(200)));
        assert_eq!(r.delay(3), Some(std::time::Duration::from_millis(400)));
        assert_eq!(
            r.delay(4),
            Some(std::time::Duration::from_millis(500)),
            "must cap at max_delay"
        );
        assert_eq!(r.delay(10), None, "no delay once attempts are exhausted");
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;

    /// A timeout is the one error class that says nothing about the object
    /// store's health: a flaky link and a wedged server look identical. It is
    /// worth one retry, so it must be classified as transient.
    #[test]
    fn a_timeout_is_worth_retrying() {
        let e = anyhow::anyhow!("timeout: global");
        assert!(is_transient(&e), "a timeout should be retried");
    }

    #[test]
    fn a_refusal_is_not_retried_by_message_alone() {
        // Connection refused is handled by the caller's error type, not by
        // string matching, so a bare message must not be called transient.
        let e = anyhow::anyhow!("connection refused");
        assert!(!is_transient(&e));
    }
}

#[cfg(test)]
mod transient_tests {
    use super::*;

    fn err_of(e: anyhow::Error) -> bool {
        is_transient(&e)
    }

    /// A server that answers 503 or hangs up mid-request is saying "not now".
    ///
    /// WebDAV built these as plain strings, which carried no transient flag,
    /// so the retry policy never ran and one blip failed the whole upload.
    #[test]
    fn a_server_that_says_not_now_is_retried() {
        for status in [408u16, 429, 500, 502, 503, 504] {
            let e = anyhow::Error::new(StoreError {
                transient: true,
                message: format!("HTTP {status}"),
            });
            assert!(err_of(e), "HTTP {status} should be retried");
        }
        for status in [400u16, 401, 403, 404, 409, 501] {
            let e = anyhow::Error::new(StoreError {
                transient: false,
                message: format!("HTTP {status}"),
            });
            assert!(!err_of(e), "HTTP {status} should not be retried");
        }
    }

    /// A connection that dies mid-exchange is retried too.
    #[test]
    fn a_dropped_connection_is_retried() {
        use std::io::{Error, ErrorKind};
        for kind in [
            ErrorKind::BrokenPipe,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::TimedOut,
            ErrorKind::UnexpectedEof,
        ] {
            let e = anyhow::Error::new(Error::from(kind));
            assert!(err_of(e), "{kind:?} should be retried");
        }
        // A request the server rejected as wrong will not improve.
        let e = anyhow::Error::new(Error::from(ErrorKind::InvalidInput));
        assert!(!err_of(e));
    }

    /// The HTTP client has its own words for a vanished peer.
    ///
    /// ureq says "Peer disconnected" rather than raising an io error with a
    /// kind, so matching on ErrorKind alone left those failures final.
    #[test]
    fn the_clients_own_disconnect_wording_is_retried() {
        for msg in [
            "io: Peer disconnected",
            "Peer closed connection without sending TLS close_notify",
            "connection closed before message completed",
        ] {
            let e = anyhow::anyhow!("{msg}");
            assert!(err_of(e), "{msg:?} should be retried");
        }
    }
}
