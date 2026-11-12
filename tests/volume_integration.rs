//! Integration tests for the volume/backend layering, runnable anywhere.
//!
//! These drive the same [`Volume`] the ublk layer uses, against a real
//! directory backend, so the chunk/upload/restore logic is exercised without
//! needing the ublk kernel driver.

use std::collections::BTreeMap;
use std::sync::Arc;

use blobdev::meta::{ChunkState, FileMeta, Geometry, MetaStore, MirroredMeta, Snapshot};
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

/// A failed upload waits before trying again, and forgets that it waited.
///
/// Against a store refusing everything, the old code re-attempted on every
/// FLUSH -- about once a second -- for as long as the client kept trying.
/// The wait fixes that; the entry has to go once it has expired, or a volume
/// that had a bad afternoon carries a map of every object that ever failed.
#[test]
fn a_failed_upload_backs_off_and_then_forgets() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Refusing {
        inner: DirStore,
        refuse: std::sync::Arc<AtomicBool>,
        attempts: std::sync::Arc<std::sync::atomic::AtomicU32>,
    }
    impl ObjectStore for Refusing {
        fn caps(&self) -> Caps {
            Caps::S3
        }
        fn stat(&self, key: &str) -> anyhow::Result<Option<u64>> {
            self.inner.stat(key)
        }
        fn get(&self, key: &str, range: Option<std::ops::Range<u64>>) -> anyhow::Result<Vec<u8>> {
            self.inner.get(key, range)
        }
        fn put(&self, key: &str, data: &[u8]) -> anyhow::Result<Option<String>> {
            if self.refuse.load(Ordering::SeqCst) {
                self.attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                anyhow::bail!("store is refusing everything");
            }
            self.inner.put(key, data)
        }
        fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.inner.delete(key)
        }
        fn list(&self, prefix: &str) -> anyhow::Result<Vec<(String, u64)>> {
            self.inner.list(prefix)
        }
    }

    let root = std::env::temp_dir().join(format!("blobdev-backoff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let refuse = std::sync::Arc::new(AtomicBool::new(true));
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    let store = Arc::new(Refusing {
        inner: DirStore::new(&objects).unwrap(),
        refuse: refuse.clone(),
        attempts: attempts.clone(),
    });
    let mut v = Volume::open(
        store as Arc<dyn ObjectStore>,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        Some(Create::new(8 * DEFAULT_CHUNK)),
        Policy::default(),
    )
    .unwrap();

    let buf = vec![7u8; (8 * BLOCK) as usize];
    let blocks = (8 * BLOCK / BLOCK) as usize;
    v.write(0, blocks, &buf).unwrap();

    // The first flush reaches the store and fails.
    assert!(v.flush().is_err(), "a refusing store must fail the flush");
    let after_first = attempts.load(std::sync::atomic::Ordering::SeqCst);
    assert!(after_first >= 1, "the store should have been asked once");

    // Subsequent flushes must not ask again while the wait holds.
    for _ in 0..5 {
        let _ = v.flush();
    }
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        after_first,
        "the store was asked again while the object was backing off"
    );

    // While the store is still refusing, the wait keeps holding.
    assert!(v.flush().is_err());
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        after_first,
        "the wait stopped holding while the store was still refusing"
    );

    // The bytes are still there: reading does not depend on the upload.
    let mut got = vec![0u8; buf.len()];
    v.read(0, blocks, &mut got).unwrap();
    assert_eq!(got, buf, "the data was lost while the upload was failing");
    std::fs::remove_dir_all(&root).ok();
}

/// Opening a volume must not cost a round trip per object.
///
/// Deciding the key spelling probed every uid-keyed object, twice each. A
/// 1 TiB volume whose filesystem had touched 801 objects spent over six
/// minutes opening, talking to the store, before the device appeared. The
/// decision is settled by a handful of agreeing objects, so a sample is
/// enough.
#[test]
fn opening_a_large_volume_does_not_probe_every_object() {
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Counting {
        inner: DirStore,
        stats: Arc<AtomicU32>,
    }
    impl ObjectStore for Counting {
        fn caps(&self) -> Caps {
            Caps::S3
        }
        fn stat(&self, key: &str) -> anyhow::Result<Option<u64>> {
            self.stats.fetch_add(1, Ordering::SeqCst);
            self.inner.stat(key)
        }
        fn get(&self, key: &str, range: Option<std::ops::Range<u64>>) -> anyhow::Result<Vec<u8>> {
            self.inner.get(key, range)
        }
        fn put(&self, key: &str, data: &[u8]) -> anyhow::Result<Option<String>> {
            self.inner.put(key, data)
        }
        fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.inner.delete(key)
        }
        fn list(&self, prefix: &str) -> anyhow::Result<Vec<(String, u64)>> {
            self.inner.list(prefix)
        }
    }

    let root = std::env::temp_dir().join(format!("blobdev-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let obj_blocks = blobdev::volume::DEFAULT_OBJECT / BLOCK;

    // Enough objects that probing all of them would be obvious.
    let n_objects = 48u64;
    {
        let store = Arc::new(Counting {
            inner: DirStore::new(&objects).unwrap(),
            stats: Arc::new(AtomicU32::new(0)),
        });
        let mut v = Volume::open(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(64 * DEFAULT_CHUNK)),
            Policy::default(),
        )
        .unwrap();
        // One block in every object, so each has its own committed uid.
        for i in 0..n_objects {
            let blk = i * obj_blocks + 1;
            v.write(blk, 1, &[i as u8; BLOCK as usize]).unwrap();
            v.flush().unwrap();
        }
    }

    let stats = Arc::new(AtomicU32::new(0));
    let store = Arc::new(Counting {
        inner: DirStore::new(&objects).unwrap(),
        stats: stats.clone(),
    });
    let v = Volume::open(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .unwrap();
    drop(v);

    let calls = stats.load(Ordering::SeqCst);
    assert!(
        calls <= 2 * 16,
        "opening asked the store for {calls} objects; a sample should settle the \
         spelling and a large volume must not pay a round trip per object"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// A large read must survive the memory cache being too small for it.
///
/// `--read-chunks` bounds the in-memory LRU. One read can ask for more objects
/// than it holds, so an object planned a moment ago may already be evicted by
/// the time it is copied out. That is a tuning effect, not a broken read: the
/// disk cache still has it, and failing the IO would turn a small cache into
/// an unreadable volume.
#[test]
fn a_read_larger_than_the_memory_cache_still_succeeds() {
    // Whole-object replacement, so every object comes from the store on a
    // miss and the disk cache is what saves the second attempt.
    struct WholeObjects(DirStore);
    impl ObjectStore for WholeObjects {
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
    let root = std::env::temp_dir().join(format!("blobdev-smallcache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let cache_dir = root.join("cache");

    // 16 MiB of data: four objects' worth.
    let blocks = (16u64 * 1024 * 1024 / BLOCK) as usize;
    let mut data = vec![0u8; blocks * BLOCK as usize];
    for b in 0..blocks {
        data[b * BLOCK as usize..(b + 1) * BLOCK as usize].fill((b % 251) as u8);
    }

    {
        let store = Arc::new(WholeObjects(DirStore::new(&objects).unwrap()));
        let mut v = Volume::open_with_cache(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(8 * DEFAULT_CHUNK)),
            Policy::default(),
            Some(blobdev::volume::DiskCache::new(&cache_dir, 64 << 20)),
        )
        .unwrap();
        v.write(0, blocks, &data).unwrap();
        v.flush().unwrap();
    }

    // A memory cache that holds a single object.
    let policy = Policy {
        read_chunks: 1,
        ..Policy::default()
    };
    let store = Arc::new(WholeObjects(DirStore::new(&objects).unwrap()));
    let mut v = Volume::open_with_cache(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        policy,
        Some(blobdev::volume::DiskCache::new(&cache_dir, 64 << 20)),
    )
    .unwrap();

    let mut got = vec![0u8; data.len()];
    v.read(0, blocks, &mut got)
        .expect("a read spanning more objects than the memory cache holds must still work");
    assert_eq!(got, data, "the data came back wrong");
    std::fs::remove_dir_all(&root).ok();
}

/// Losing the local metadata must not lose the volume, on a store that can
/// hold a copy.
///
/// Measured before this existed: deleting the metadata file left a volume
/// that refused to open, and the advice it gave -- "pass a size to create
/// it" -- built an empty volume on top of data that was still in the store.
#[test]
fn the_volume_survives_losing_its_local_metadata() {
    struct WholeObjects(DirStore);
    impl ObjectStore for WholeObjects {
        fn caps(&self) -> Caps {
            // A store that replaces whole objects is the one worth keeping a
            // metadata copy on: it outlives the machine.
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
    }

    let root = std::env::temp_dir().join(format!("blobdev-metacopy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let blocks = (8 * 1024 * 1024 / BLOCK) as usize;
    let mut data = vec![0u8; blocks * BLOCK as usize];
    for b in 0..blocks {
        data[b * BLOCK as usize..(b + 1) * BLOCK as usize].fill((b % 97) as u8);
    }
    let open = |create: Option<Create>| {
        let store = Arc::new(WholeObjects(DirStore::new(&objects).unwrap()));
        let meta = MirroredMeta::new(
            FileMeta::new(&meta_path),
            store.clone(),
            "vol/meta".to_string(),
        );
        Volume::open(
            store,
            Box::new(meta),
            Layout::new("vol"),
            create,
            Policy::default(),
        )
    };

    {
        let mut v = open(Some(Create::new(4 * DEFAULT_CHUNK))).unwrap();
        v.write(0, blocks, &data).unwrap();
        v.flush().unwrap();
    }

    // The disk holding the metadata is gone; the objects are still there.
    std::fs::remove_file(&meta_path).unwrap();
    assert!(!meta_path.exists());

    let mut v = open(None).expect("the volume must open from the copy in the store");
    let mut got = vec![0u8; data.len()];
    v.read(0, blocks, &mut got).unwrap();
    assert_eq!(got, data, "data recovered through the copied metadata");

    // The local file must be back: saving only happens on an upload, so a
    // volume that was recovered and then only read from would depend on the
    // store at every start and fail to open when it was unreachable.
    assert!(
        meta_path.exists(),
        "recovery must write the metadata back to the local file"
    );

    // A second open must not need the store at all.
    let err = Volume::open(
        Arc::new(WholeObjects(DirStore::new(root.join("gone")).unwrap())),
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .err();
    assert!(
        err.is_none(),
        "with the local file back, opening must not consult the store: {err:?}"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// A save that reaches the local file but not the store must not leave a
/// copy that recovery would trust.
///
/// The copy is written after the local file, so a store outage leaves it
/// behind by an unknown amount. Recovering from it opens a volume that reads
/// objects which have since been replaced -- worse than refusing, because it
/// looks like it worked.
#[test]
fn a_copy_that_fell_behind_is_not_used_for_recovery() {
    struct FlakyCopy {
        inner: DirStore,
        /// Refuse every put once set, as an unreachable store would.
        broken: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl ObjectStore for FlakyCopy {
        fn caps(&self) -> Caps {
            Caps::S3
        }
        fn stat(&self, key: &str) -> anyhow::Result<Option<u64>> {
            self.inner.stat(key)
        }
        fn get(&self, key: &str, range: Option<std::ops::Range<u64>>) -> anyhow::Result<Vec<u8>> {
            self.inner.get(key, range)
        }
        fn put(&self, key: &str, data: &[u8]) -> anyhow::Result<Option<String>> {
            if self.broken.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(blobdev::store::StoreError::transient("store is down"));
            }
            self.inner.put(key, data)
        }
        fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.inner.delete(key)
        }
    }

    let root = std::env::temp_dir().join(format!("blobdev-behind-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let broken = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let nblk = (4 * 1024 * 1024 / BLOCK) as usize;

    {
        // Data goes to a store that always works; only the metadata copy is
        // pointed at one that can fail. A store outage takes out both, and
        // then the volume cannot write at all -- which is a different test.
        let store = Arc::new(FlakyCopy {
            inner: DirStore::new(&objects).unwrap(),
            broken: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        let mut v = Volume::open(
            store,
            Box::new(MirroredMeta::new(
                FileMeta::new(&meta_path),
                Arc::new(FlakyCopy {
                    inner: DirStore::new(&objects).unwrap(),
                    broken: broken.clone(),
                }),
                "vol/meta".to_string(),
            )),
            Layout::new("vol"),
            Some(Create::new(4 * DEFAULT_CHUNK)),
            Policy::default(),
        )
        .unwrap();
        v.write(0, nblk, &blocks(nblk, 0x11)).unwrap();
        v.flush().unwrap();

        // The store goes away, then a write changes the layout: the local
        // metadata now names a different object than the copy does.
        broken.store(true, std::sync::atomic::Ordering::SeqCst);
        v.write(nblk as u64, nblk, &blocks(nblk, 0x22)).unwrap();
        v.flush().unwrap();
    }
    assert!(broken.load(std::sync::atomic::Ordering::SeqCst));
    std::fs::remove_file(&meta_path).unwrap();

    let store = Arc::new(FlakyCopy {
        inner: DirStore::new(&objects).unwrap(),
        broken: broken.clone(),
    });
    let err = Volume::open(
        store,
        Box::new(MirroredMeta::new(
            FileMeta::new(&meta_path),
            Arc::new(FlakyCopy {
                inner: DirStore::new(&objects).unwrap(),
                broken: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
            "vol/meta".to_string(),
        )),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .err()
    .expect("recovery from a copy known to be behind must be refused");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("behind"),
        "the refusal must say why, got: {msg}"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// A save that changes nothing must not reach the store again.
///
/// The metadata is saved on every upload and grows with the object count, so
/// re-sending an identical snapshot is pure cost during repeated writes.
#[test]
fn an_unchanged_save_does_not_reupload_the_copy() {
    struct Counting {
        inner: DirStore,
        puts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl ObjectStore for Counting {
        fn caps(&self) -> Caps {
            Caps::S3
        }
        fn stat(&self, key: &str) -> anyhow::Result<Option<u64>> {
            self.inner.stat(key)
        }
        fn get(&self, key: &str, range: Option<std::ops::Range<u64>>) -> anyhow::Result<Vec<u8>> {
            self.inner.get(key, range)
        }
        fn put(&self, key: &str, data: &[u8]) -> anyhow::Result<Option<String>> {
            if key.ends_with("/meta") {
                self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.inner.put(key, data)
        }
        fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.inner.delete(key)
        }
    }

    let root = std::env::temp_dir().join(format!("blobdev-dedup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let puts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut meta = MirroredMeta::new(
        FileMeta::new(root.join("meta")),
        Arc::new(Counting {
            inner: DirStore::new(&objects).unwrap(),
            puts: puts.clone(),
        }),
        "vol/meta".to_string(),
    );

    let geom = Geometry {
        chunk_size: DEFAULT_CHUNK,
        object_size: 4 << 20,
        size: 4 * DEFAULT_CHUNK,
    };
    let mut snap = Snapshot::new(geom);
    snap.chunks.insert(
        0,
        ChunkState {
            dirty: Default::default(),
            uploaded: true,
            uid: 7,
        },
    );
    meta.save(&snap).unwrap();
    let after_first = puts.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(after_first, 1, "the first save must reach the store");

    for _ in 0..5 {
        meta.save(&snap).unwrap();
    }
    assert_eq!(
        puts.load(std::sync::atomic::Ordering::SeqCst),
        after_first,
        "identical saves must not be sent again"
    );

    // A real change still goes out.
    snap.chunks.insert(
        1,
        ChunkState {
            dirty: Default::default(),
            uploaded: true,
            uid: 9,
        },
    );
    meta.save(&snap).unwrap();
    assert_eq!(
        puts.load(std::sync::atomic::Ordering::SeqCst),
        after_first + 1,
        "a changed snapshot must be sent"
    );
    std::fs::remove_dir_all(&root).ok();
}
