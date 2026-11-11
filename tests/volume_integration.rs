//! Integration tests for the volume/backend layering, runnable anywhere.
//!
//! These drive the same [`Volume`] the ublk layer uses, against a real
//! directory backend, so the chunk/upload/restore logic is exercised without
//! needing the ublk kernel driver.

use std::collections::BTreeMap;
use std::sync::Arc;

use blobdev::meta::{ChunkState, FileMeta, Geometry, MetaStore, Snapshot};
use blobdev::store::{Caps, ObjectStore};
use blobdev::store::{DirStore, MemStore};
use blobdev::volume::{Create, Layout, Policy, Volume, BLOCK, DEFAULT_CHUNK};

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("blobdev-it-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn blocks(n: usize, v: u8) -> Vec<u8> {
    vec![v; n * BLOCK as usize]
}

#[test]
fn data_survives_a_fresh_process() {
    let root = tmpdir("restart");
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = 4 * DEFAULT_CHUNK;

    {
        let store = Arc::new(DirStore::new(&objects).unwrap());
        let mut v = Volume::open(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(size)),
            Policy::default(),
        )
        .unwrap();

        // Write scattered blocks across several chunks.
        for (i, blk) in [0u64, 7, 2048, 4095, 6000].iter().enumerate() {
            v.write(*blk, 1, &blocks(1, 0x10 + i as u8)).unwrap();
        }
        let mut all = vec![0u8; 5 * BLOCK as usize];
        for (i, blk) in [0u64, 7, 2048, 4095, 6000].iter().enumerate() {
            v.read(*blk, 1, &mut all[i * BLOCK as usize..(i + 1) * BLOCK as usize])
                .unwrap();
        }
        let _ = all;
        v.flush().unwrap();
    }

    // Fresh volume object, same object store and metadata file.
    {
        let store = Arc::new(DirStore::new(&objects).unwrap());
        let mut v = Volume::open(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            None,
            Policy::default(),
        )
        .unwrap();
        for (i, blk) in [0u64, 7, 2048, 4095, 6000].iter().enumerate() {
            let mut got = vec![0u8; BLOCK as usize];
            v.read(*blk, 1, &mut got).unwrap();
            assert_eq!(
                got,
                blocks(1, 0x10 + i as u8),
                "block {blk} lost its data across a restart"
            );
        }
    }

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn unsent_changes_are_recovered_from_metadata() {
    let root = tmpdir("crash");
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = DEFAULT_CHUNK;

    {
        let store = Arc::new(DirStore::new(&objects).unwrap());
        let mut v = Volume::open(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(size)),
            Policy::default(),
        )
        .unwrap();
        v.write(2, 1, &blocks(1, 0xAA)).unwrap();
        // Dropped without flush: simulates a crash after the metadata write.
    }

    let store = Arc::new(DirStore::new(&objects).unwrap());
    let mut v = Volume::open(
        store.clone(),
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .unwrap();
    assert!(
        v.dirty_chunks() > 0,
        "the pending modification should have been restored from metadata"
    );
    v.flush().unwrap();

    // The store must now hold the object.
    assert!(objects.join("vol/c/0").exists(), "chunk object was not created");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn geometry_is_persisted_and_reused() {
    let root = tmpdir("geometry");
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = 2 * DEFAULT_CHUNK;

    {
        let store = Arc::new(DirStore::new(&objects).unwrap());
        let _v = Volume::open(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(size)),
            Policy::default(),
        )
        .unwrap();
    }

    let store = Arc::new(DirStore::new(&objects).unwrap());
    let v = Volume::open(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .unwrap();
    assert_eq!(
        v.geometry(),
        &Geometry {
            chunk_size: DEFAULT_CHUNK,
            object_size: blobdev::volume::DEFAULT_OBJECT,
            size
        }
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn opening_without_metadata_and_size_fails_clearly() {
    let root = tmpdir("nometa");
    let objects = root.join("objects");
    let store = Arc::new(DirStore::new(&objects).unwrap());
    let err = match Volume::open(
        store,
        Box::new(FileMeta::new(root.join("meta"))),
        Layout::new("vol"),
        None,
        Policy::default(),
    ) {
        Ok(_) => panic!("creating a volume without a size must fail"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        err.contains("no metadata"),
        "unhelpful error when the volume does not exist: {err}"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// Regression: writing one block into an existing chunk must not upload
/// zeros over the blocks it never read.
///
/// A write starts from a partial working copy (no fetch), so a whole-chunk
/// upload on S3 has to fill the untouched blocks first. If that step were
/// skipped, every 4 KiB write would blank the other 2047 blocks of the chunk.
#[test]
fn writing_one_block_does_not_clobber_the_rest_of_the_chunk() {
    let root = tmpdir("clobber");
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = DEFAULT_CHUNK;

    // Establish a chunk with two distinct regions.
    {
        let store = Arc::new(DirStore::new(&objects).unwrap());
        let mut v = Volume::open(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(size)),
            Policy::default(),
        )
        .unwrap();
        v.write(0, 4, &blocks(4, 0xA1)).unwrap();
        v.write(100, 4, &blocks(4, 0xB2)).unwrap();
        v.flush().unwrap();
    }

    // Fresh process: write a single unrelated block, then flush.
    {
        let store = Arc::new(DirStore::new(&objects).unwrap());
        let mut v = Volume::open(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            None,
            Policy::default(),
        )
        .unwrap();
        v.write(500, 1, &blocks(1, 0xC3)).unwrap();
        v.flush().unwrap();
    }

    // Everything written earlier must still be there.
    let store = Arc::new(DirStore::new(&objects).unwrap());
    let mut v = Volume::open(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .unwrap();
    for b in 0..4u64 {
        let mut buf = vec![0u8; BLOCK as usize];
        v.read(b, 1, &mut buf).unwrap();
        assert_eq!(buf[0], 0xA1, "block {b} was clobbered by a later write");
    }
    for b in 100..104u64 {
        let mut buf = vec![0u8; BLOCK as usize];
        v.read(b, 1, &mut buf).unwrap();
        assert_eq!(buf[0], 0xB2, "block {b} was clobbered by a later write");
    }
    let mut buf = vec![0u8; BLOCK as usize];
    v.read(500, 1, &mut buf).unwrap();
    assert_eq!(buf[0], 0xC3);
    std::fs::remove_dir_all(&root).ok();
}

/// The dangerous case: a backend without partial writes (S3) rewrites the
/// whole chunk on upload, so an incomplete working copy would destroy data.
#[test]
fn whole_chunk_upload_on_s3_like_backend_preserves_untouched_blocks() {
    use blobdev::store::{Caps, ObjectStore};

    /// DirStore with partial writes disabled, i.e. S3 semantics.
    struct S3Like(DirStore);
    impl ObjectStore for S3Like {
        fn caps(&self) -> Caps {
            Caps::S3
        }
        fn stat(&self, key: &str) -> anyhow::Result<Option<u64>> {
            self.0.stat(key)
        }
        fn get(
            &self,
            key: &str,
            range: Option<std::ops::Range<u64>>,
        ) -> anyhow::Result<Vec<u8>> {
            self.0.get(key, range)
        }
        fn put(&self, key: &str, data: &[u8]) -> anyhow::Result<Option<String>> {
            self.0.put(key, data)
        }
        fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.0.delete(key)
        }
    }

    let root = tmpdir("s3like");
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = DEFAULT_CHUNK;

    {
        let store = Arc::new(S3Like(DirStore::new(&objects).unwrap()));
        assert!(!store.caps().partial_write);
        let mut v = Volume::open(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(size)),
            Policy::default(),
        )
        .unwrap();
        v.write(0, 4, &blocks(4, 0x11)).unwrap();
        v.write(100, 4, &blocks(4, 0x22)).unwrap();
        v.flush().unwrap();
    }

    // Write one block in a new process; the upload rewrites the whole chunk.
    {
        let store = Arc::new(S3Like(DirStore::new(&objects).unwrap()));
        let mut v = Volume::open(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            None,
            Policy::default(),
        )
        .unwrap();
        v.write(500, 1, &blocks(1, 0x33)).unwrap();
        v.flush().unwrap();
    }

    let store = Arc::new(S3Like(DirStore::new(&objects).unwrap()));
    let mut v = Volume::open(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .unwrap();
    for (range, want) in [(0..4u64, 0x11u8), (100..104, 0x22)] {
        for b in range {
            let mut buf = vec![0u8; BLOCK as usize];
            v.read(b, 1, &mut buf).unwrap();
            assert_eq!(
                buf[0], want,
                "block {b} lost its data in a whole-chunk upload"
            );
        }
    }
    let mut buf = vec![0u8; BLOCK as usize];
    v.read(500, 1, &mut buf).unwrap();
    assert_eq!(buf[0], 0x33);
    std::fs::remove_dir_all(&root).ok();
}

/// The point of copy-on-write: an upload that fails part way must leave the
/// volume readable at its previous content, not half-written.
#[test]
fn failed_upload_keeps_the_previous_version_readable() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    /// S3-like store whose PUTs start failing after `fail_after` successes.
    struct FlakyStore {
        inner: DirStore,
        puts: AtomicU64,
        fail_after: AtomicU64,
        fail: Mutex<bool>,
    }
    impl ObjectStore for FlakyStore {
        fn caps(&self) -> Caps {
            Caps::S3
        }
        fn stat(&self, key: &str) -> anyhow::Result<Option<u64>> {
            self.inner.stat(key)
        }
        fn get(
            &self,
            key: &str,
            range: Option<std::ops::Range<u64>>,
        ) -> anyhow::Result<Vec<u8>> {
            self.inner.get(key, range)
        }
        fn put(&self, key: &str, data: &[u8]) -> anyhow::Result<Option<String>> {
            if *self.fail.lock().unwrap() && self.puts.load(Ordering::SeqCst) >= self.fail_after.load(Ordering::SeqCst) {
                anyhow::bail!("simulated S3 outage");
            }
            self.puts.fetch_add(1, Ordering::SeqCst);
            self.inner.put(key, data)
        }
        fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.inner.delete(key)
        }
    }
    let root = tmpdir("cowfail");
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = DEFAULT_CHUNK;

    let store = Arc::new(FlakyStore {
        inner: DirStore::new(&objects).unwrap(),
        puts: AtomicU64::new(0),
        fail_after: AtomicU64::new(u64::MAX),
        fail: Mutex::new(false),
    });

    // Version 1 committed.
    {
        let mut v = Volume::open(
            store.clone() as Arc<dyn ObjectStore>,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(size)),
            Policy::default(),
        )
        .unwrap();
        v.write(0, 2, &blocks(2, 0x01)).unwrap();
        v.flush().unwrap();
    }

    // Version 2 begins uploading and the store dies.
    {
        let mut v = Volume::open(
            store.clone() as Arc<dyn ObjectStore>,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            None,
            Policy::default(),
        )
        .unwrap();
        v.write(0, 2, &blocks(2, 0x02)).unwrap();
        *store.fail.lock().unwrap() = true;
        store.fail_after.store(0, Ordering::SeqCst);
        let err = v.flush();
        assert!(err.is_err(), "the flush should have reported the outage");
    }

    // The store recovers; the volume must still read version 1.
    *store.fail.lock().unwrap() = false;
    store.fail_after.store(u64::MAX, Ordering::SeqCst);
    let mut v = Volume::open(
        store.clone() as Arc<dyn ObjectStore>,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .unwrap();
    for b in 0..2u64 {
        let mut buf = vec![0u8; BLOCK as usize];
        v.read(b, 1, &mut buf).unwrap();
        assert_eq!(
            buf[0], 0x01,
            "block {b} should still hold the last committed version"
        );
    }
    std::fs::remove_dir_all(&root).ok();
}

/// A successful copy-on-write upload must reclaim the superseded object.
#[test]
fn superseded_objects_are_deleted() {
    let root = tmpdir("cowgc");
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = DEFAULT_CHUNK;
    let store = Arc::new(DirStore::new(&objects).unwrap());

    let mut v = Volume::open(
        store as Arc<dyn ObjectStore>,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        Some(Create::new(size)),
        Policy::default(),
    )
    .unwrap();
    // Three overwrites: each leaves one generation behind.
    for round in 0..3u8 {
        v.write(0, 1, &blocks(1, round + 1)).unwrap();
        v.flush().unwrap();
    }

    let chunk_dir = objects.join("vol").join("c");
    let files: Vec<String> = std::fs::read_dir(&chunk_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        files.len(),
        1,
        "expected exactly one live chunk object, found {files:?}"
    );
    // The dir backend supports partial writes, so it patches the object in
    // place and keeps the original key; only S3-style backends roll
    // generations. Either way exactly one object survives.
    assert!(
        files[0] == "0" || files[0].starts_with("0."),
        "unexpected key {}",
        files[0]
    );

    // And the data is the newest version.
    let mut buf = vec![0u8; BLOCK as usize];
    v.read(0, 1, &mut buf).unwrap();
    assert_eq!(buf[0], 3);
    std::fs::remove_dir_all(&root).ok();
}

/// Garbage collection must remove orphaned chunk objects and nothing else.
#[test]
fn gc_removes_only_orphans() {
    /// DirStore with partial writes disabled: S3 semantics, which is the only
    /// case that can leave orphaned objects behind.
    struct S3Like(DirStore);
    impl ObjectStore for S3Like {
        fn caps(&self) -> Caps {
            Caps::S3
        }
        fn stat(&self, key: &str) -> anyhow::Result<Option<u64>> {
            self.0.stat(key)
        }
        fn get(
            &self,
            key: &str,
            range: Option<std::ops::Range<u64>>,
        ) -> anyhow::Result<Vec<u8>> {
            self.0.get(key, range)
        }
        fn put(&self, key: &str, data: &[u8]) -> anyhow::Result<Option<String>> {
            self.0.put(key, data)
        }
        fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.0.delete(key)
        }
        fn list(&self, prefix: &str) -> anyhow::Result<Vec<(String, u64)>> {
            self.0.list(prefix)
        }
    }

    let root = tmpdir("gc");
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = 2 * DEFAULT_CHUNK;

    // Two chunks committed normally.
    {
        let store = Arc::new(S3Like(DirStore::new(&objects).unwrap()));
        let mut v = Volume::open(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(size)),
            Policy::default(),
        )
        .unwrap();
        v.write(0, 1, &blocks(1, 0x01)).unwrap();
        v.write((DEFAULT_CHUNK / BLOCK) + 5, 1, &blocks(1, 0x02))
            .unwrap();
        v.flush().unwrap();
    }

    let chunk_dir = objects.join("vol").join("c");
    let live_before: std::collections::BTreeSet<String> = std::fs::read_dir(&chunk_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(live_before.len(), 2, "expected two live objects");

    // Simulate a crash between writing a new object and committing it.
    std::fs::write(chunk_dir.join("9999"), vec![0u8; 4096]).unwrap();

    let store = Arc::new(S3Like(DirStore::new(&objects).unwrap()));
    let mut v = Volume::open(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .unwrap();

    // A dry run reports without touching anything.
    let dry = v.gc(true).unwrap();
    assert_eq!(dry.orphaned, 1, "the stray object should be found");
    assert_eq!(dry.deleted, 0);
    assert_eq!(dry.kept, 2);
    assert!(chunk_dir.join("9999").exists(), "dry run deleted something");

    // A real run removes only the orphan.
    let real = v.gc(false).unwrap();
    assert_eq!(real.orphaned, 1);
    assert_eq!(real.deleted, 1);
    assert!(!chunk_dir.join("9999").exists());

    let after: std::collections::BTreeSet<String> = std::fs::read_dir(&chunk_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(after, live_before, "gc must not touch live objects");

    // Data is still intact after the collection.
    drop(v);
    let store = Arc::new(S3Like(DirStore::new(&objects).unwrap()));
    let mut v = Volume::open(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .unwrap();
    let mut buf = vec![0u8; BLOCK as usize];
    v.read(0, 1, &mut buf).unwrap();
    assert_eq!(buf[0], 0x01);
    std::fs::remove_dir_all(&root).ok();
}

/// Collection is refused while chunks are still waiting to be uploaded,
/// because those own objects the metadata does not reference yet.
#[test]
fn gc_refuses_with_pending_uploads() {
    let root = tmpdir("gcpending");
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let store = Arc::new(DirStore::new(&objects).unwrap());
    let mut v = Volume::open(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        Some(Create::new(DEFAULT_CHUNK)),
        Policy::default(),
    )
    .unwrap();
    v.write(0, 1, &blocks(1, 0x09)).unwrap();
    let err = match v.gc(false) {
        Ok(_) => panic!("gc should refuse while uploads are pending"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("pending"), "unhelpful error: {err}");
    std::fs::remove_dir_all(&root).ok();
}

/// Collection must refuse to run when the metadata points at a missing
/// object: deleting the "unreferenced" ones would then destroy the only
/// remaining evidence and any data that is still reachable.
#[test]
fn gc_refuses_when_metadata_references_a_missing_object() {
    struct S3Like(DirStore);
    impl ObjectStore for S3Like {
        fn caps(&self) -> Caps {
            Caps::S3
        }
        fn stat(&self, key: &str) -> anyhow::Result<Option<u64>> {
            self.0.stat(key)
        }
        fn get(&self, key: &str, range: Option<std::ops::Range<u64>>) -> anyhow::Result<Vec<u8>> {
            self.0.get(key, range)
        }
        fn put(&self, key: &str, data: &[u8]) -> anyhow::Result<Option<String>> {
            self.0.put(key, data)
        }
        fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.0.delete(key)
        }
        fn list(&self, prefix: &str) -> anyhow::Result<Vec<(String, u64)>> {
            self.0.list(prefix)
        }
    }

    let root = tmpdir("gcmiss");
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = 2 * DEFAULT_CHUNK;

    {
        let store = Arc::new(S3Like(DirStore::new(&objects).unwrap()));
        let mut v = Volume::open(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(size)),
            Policy::default(),
        )
        .unwrap();
        v.write(0, 1, &blocks(1, 0x77)).unwrap();
        v.flush().unwrap();
    }

    // Take one referenced object away behind the volume's back.
    let chunk_dir = objects.join("vol").join("c");
    let victim = std::fs::read_dir(&chunk_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_file())
        .expect("a chunk object");
    std::fs::remove_file(&victim).unwrap();

    let store = Arc::new(S3Like(DirStore::new(&objects).unwrap()));
    let mut v = Volume::open(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .unwrap();
    let err = match v.gc(false) {
        Ok(r) => panic!("gc should have refused, but reported {r:?}"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        err.contains("missing"),
        "the error should name the problem: {err}"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// The partial-write path must not publish zeros either.
///
/// When the base object does not exist yet, the backend needs a whole-object
/// PUT. Doing that from a partial working copy would write zeros over every
/// block this write never touched, which is exactly how an ext4 filesystem
/// ended up unreadable right after mkfs.
#[test]
fn partial_write_backend_creates_missing_object_without_losing_blocks() {
    let root = tmpdir("pwcreate");
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = DEFAULT_CHUNK;
    let store = Arc::new(DirStore::new(&objects).unwrap());
    assert!(store.caps().partial_write);

    let mut v = Volume::open(
        store as Arc<dyn ObjectStore>,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        Some(Create::new(size)),
        Policy::default(),
    )
    .unwrap();

    // Several separate writes inside one chunk, each flush creating the object
    // for the first time from a partial working copy.
    v.write(1, 1, &blocks(1, 0xA1)).unwrap();
    v.flush().unwrap();
    v.write(3, 1, &blocks(1, 0xB2)).unwrap();
    v.flush().unwrap();
    v.write(500, 1, &blocks(1, 0xC3)).unwrap();
    v.flush().unwrap();

    // Everything written must read back, and everything else must read zero
    // rather than garbage or earlier data.
    let mut buf = vec![0u8; BLOCK as usize];
    for (blk, want) in [(1u64, 0xA1u8), (3, 0xB2), (500, 0xC3)] {
        v.read(blk, 1, &mut buf).unwrap();
        assert_eq!(buf[0], want, "block {blk} was lost");
    }
    for blk in [2u64, 4, 499] {
        v.read(blk, 1, &mut buf).unwrap();
        assert_eq!(buf[0], 0, "block {blk} should be zero");
    }
    std::fs::remove_dir_all(&root).ok();
}

/// Metadata that disagrees with the geometry must be rejected, not acted on.
///
/// A dirty block past the end of its object would index out of bounds, and an
/// object past the end of the volume addresses storage that does not exist.
/// Both come from a truncated file or a geometry change, and both are much
/// easier to diagnose as a clear error at open time than as an out-of-range
/// write later.
#[test]
fn metadata_with_out_of_range_indices_is_rejected() {
    let root = tmpdir("metabad");
    let meta_path = root.join("meta");
    let geometry = blobdev::meta::Geometry {
        chunk_size: DEFAULT_CHUNK,
        object_size: blobdev::volume::DEFAULT_OBJECT,
        size: DEFAULT_CHUNK,
    };

    let mut bad_block = Snapshot::new(geometry.clone());
    bad_block.chunks.insert(
        0,
        ChunkState {
            dirty: [(999_999u64, ())].into_iter().collect(),
            uploaded: true,
            uid: 7,
        },
    );
    FileMeta::new(&meta_path)
        .save(&bad_block)
        .unwrap();
    let err = match Volume::open(
        Arc::new(MemStore::new()),
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    ) {
        Ok(_) => panic!("out-of-range block index was accepted"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("corrupt"), "unhelpful error: {err}");

    let mut bad_object = Snapshot::new(geometry);
    bad_object.chunks.insert(
        5000,
        ChunkState {
            dirty: BTreeMap::new(),
            uploaded: true,
            uid: 9,
        },
    );
    FileMeta::new(&meta_path)
        .save(&bad_object)
        .unwrap();
    let err = match Volume::open(
        Arc::new(MemStore::new()),
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    ) {
        Ok(_) => panic!("out-of-range object index was accepted"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        err.contains("geometry"),
        "the error should mention the geometry: {err}"
    );
    std::fs::remove_dir_all(&root).ok();
}
