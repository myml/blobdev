//! Uploading must not corrupt chunks.
//!
//! Reproduces the S3 path end to end at the volume level: write a
//! multi-chunk file, flush, drop all caches and read it back. Any mismatch is
//! an upload bug, not a caching one.

use std::sync::Arc;

use blobdev::meta::FileMeta;
use blobdev::store::{Caps, DirStore, ObjectStore};
use blobdev::volume::{Create, Layout, Policy, Volume, BLOCK, DEFAULT_CHUNK};

/// DirStore with S3 semantics (no partial writes, whole-object uploads).
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

fn pattern(blocks: u64) -> Vec<u8> {
    // Deterministic bytes, different in every block, so a misplaced block is
    // visible rather than being masked by repeats.
    let mut v = vec![0u8; (blocks * BLOCK) as usize];
    for b in 0..blocks {
        for i in 0..BLOCK as usize {
            v[(b * BLOCK) as usize + i] = (b.wrapping_mul(31).wrapping_add(i as u64) % 251) as u8;
        }
    }
    v
}

#[test]
fn multi_chunk_upload_reads_back_identically() {
    let root = std::env::temp_dir().join(format!("blobdev-up-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = 10 * DEFAULT_CHUNK;
    let blocks: u64 = 2000; // spans several chunks
    let data = pattern(blocks);

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
        v.write(0, blocks as usize, &data).unwrap();
        v.flush().unwrap();
    }

    // Fresh process view: nothing cached, everything from the store.
    let store = Arc::new(S3Like(DirStore::new(&objects).unwrap()));
    let mut v = Volume::open(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .unwrap();
    let mut got = vec![0u8; data.len()];
    v.read(0, blocks as usize, &mut got).unwrap();
    for b in 0..blocks {
        let off = (b * BLOCK) as usize;
        assert_eq!(
            &got[off..off + BLOCK as usize],
            &data[off..off + BLOCK as usize],
            "block {b} differs after upload"
        );
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn reupload_after_partial_overwrite_keeps_old_blocks() {
    let root = std::env::temp_dir().join(format!("blobdev-up2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = 4 * DEFAULT_CHUNK;

    let first = pattern(500);
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
        v.write(0, 500, &first).unwrap();
        v.flush().unwrap();
    }

    // Overwrite a few blocks in a fresh process, then flush again.
    let patch = pattern(3);
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
        v.write(100, 3, &patch).unwrap();
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
    let mut got = vec![0u8; first.len()];
    v.read(0, 500, &mut got).unwrap();
    for b in 0..500u64 {
        let off = (b * BLOCK) as usize;
        let want = if (100..103).contains(&b) {
            &patch[((b - 100) * BLOCK) as usize..((b - 100 + 1) * BLOCK) as usize]
        } else {
            &first[off..off + BLOCK as usize]
        };
        assert_eq!(
            &got[off..off + BLOCK as usize],
            want,
            "block {b} is wrong after a partial overwrite"
        );
    }
    std::fs::remove_dir_all(&root).ok();
}
/// Reproduces the e2e failure: write a whole chunk, force an upload, then read
/// it back with every cache dropped.
#[test]
fn whole_chunk_roundtrip_after_upload() {
    let root = std::env::temp_dir().join(format!("blobdev-wc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let cache_dir = root.join("cache");
    let size = 4 * DEFAULT_CHUNK;

    let chunk_blocks = (DEFAULT_CHUNK / BLOCK) as usize;
    let data = pattern(chunk_blocks as u64);

    {
        let store = Arc::new(blobdev::store::DirStore::new(&objects).unwrap());
        let mut v = Volume::open_with_cache(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(size)),
            Policy::default(),
            Some(blobdev::volume::DiskCache::new(&cache_dir, 64 << 20)),
        )
        .unwrap();
        v.write(0, chunk_blocks, &data).unwrap();
        v.flush().unwrap();
    }

    // Fresh volume: only the object store and the disk cache are available.
    let store = Arc::new(blobdev::store::DirStore::new(&objects).unwrap());
    let mut v = Volume::open_with_cache(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
        Some(blobdev::volume::DiskCache::new(&cache_dir, 64 << 20)),
    )
    .unwrap();
    let mut got = vec![0u8; data.len()];
    v.read(0, chunk_blocks, &mut got).unwrap();
    for b in 0..chunk_blocks as u64 {
        let off = (b * BLOCK) as usize;
        assert_eq!(
            &got[off..off + BLOCK as usize],
            &data[off..off + BLOCK as usize],
            "block {b} differs after upload + reopen"
        );
    }
    std::fs::remove_dir_all(&root).ok();
}

/// A command larger than the tag buffer is served in windows, and each window
/// must address its own part of that buffer.
///
/// Getting this wrong is invisible in small tests: the first window is
/// correct, and every later one repeats it, so a 1 MiB write silently becomes
/// 256 KiB of data written four times.
#[test]
fn large_command_windows_carry_their_own_bytes() {
    use blobdev::ublk_tgt::process_op;
    use std::sync::Mutex;

    let root = std::env::temp_dir().join(format!("blobdev-win-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = 4 * DEFAULT_CHUNK;
    let volume = Mutex::new(
        Volume::open(
            Arc::new(S3Like(DirStore::new(&objects).unwrap())),
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(size)),
            Policy::default(),
        )
        .unwrap(),
    );

    // 1 MiB command through a 256 KiB window, which is the ublk buffer size.
    let total = 1 << 20;
    let window = 256 << 10;
    let data = pattern((total / BLOCK as usize) as u64);
    assert_eq!(data.len(), total);

    // Serve it the way handle_command does.
    let mut buf = data.clone();
    let mut off = 0usize;
    while off < total {
        let n = (total - off).min(window);
        let r = process_op(
            &volume,
            libublk::sys::UBLK_IO_OP_WRITE,
            off as u64,
            n,
            &mut buf[off..off + n],
            false,
        );
        assert_eq!(r, n as i32);
        off += n;
    }

    let mut got = vec![0u8; total];
    off = 0;
    while off < total {
        let n = (total - off).min(window);
        let r = process_op(
            &volume,
            libublk::sys::UBLK_IO_OP_READ,
            off as u64,
            n,
            &mut got[off..off + n],
            false,
        );
        assert_eq!(r, n as i32);
        off += n;
    }
    for w in 0..(total / window) {
        let a = w * window;
        let b = a + window;
        assert_eq!(
            &got[a..b],
            &data[a..b],
            "window {w} (bytes {a}..{b}) differs"
        );
    }
    std::fs::remove_dir_all(&root).ok();
}

/// 120 MiB through many chunks, flushed, then read back in one go.
///
/// This is the shape of the e2e failure: it only shows up at this size, and
/// only when the whole file is read back from the store rather than from the
/// writer's own caches.
#[test]
fn many_chunks_roundtrip_large() {
    let root = std::env::temp_dir().join(format!("blobdev-big-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = 64 * DEFAULT_CHUNK;

    let total: u64 = 120 << 20; // 120 MiB
    let blocks = total / BLOCK;
    let data = pattern(blocks);

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
        // Write it in 1 MiB pieces, the way the block layer delivers it.
        let piece = (1 << 20) / BLOCK as usize;
        let mut done = 0usize;
        while done < blocks as usize {
            let n = piece.min(blocks as usize - done);
            v.write(done as u64, n, &data[done * BLOCK as usize..(done + n) * BLOCK as usize])
                .unwrap();
            done += n;
        }
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

    // Read back in 1 MiB pieces too.
    let piece = (1 << 20) / BLOCK as usize;
    let mut got = vec![0u8; data.len()];
    let mut done = 0usize;
    while done < blocks as usize {
        let n = piece.min(blocks as usize - done);
        v.read(
            done as u64,
            n,
            &mut got[done * BLOCK as usize..(done + n) * BLOCK as usize],
        )
        .unwrap();
        done += n;
    }
    let mut bad = Vec::new();
    for b in 0..blocks {
        let off = (b * BLOCK) as usize;
        if got[off..off + BLOCK as usize] != data[off..off + BLOCK as usize] {
            bad.push(b);
            if bad.len() > 8 {
                break;
            }
        }
    }
    assert!(bad.is_empty(), "blocks differ after roundtrip: {bad:?}");
    std::fs::remove_dir_all(&root).ok();
}

/// Rewriting part of an object that was just uploaded must not lose the rest.
///
/// The upload moves the chunk out of the working set and into the caches. A
/// later write to the same object used to start from an empty partial copy,
/// ignoring the cached bytes, so the next full upload published zeros for
/// every block that write did not itself touch. On a filesystem that shows up
/// as data that reads correctly until the page cache is dropped.
#[test]
fn rewrite_after_upload_keeps_cached_bytes() {
    let root = std::env::temp_dir().join(format!("blobdev-rw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = 4 * DEFAULT_CHUNK;

    let obj_blocks = (blobdev::volume::DEFAULT_OBJECT / BLOCK) as usize;
    let data = pattern(obj_blocks as u64);
    let store = Arc::new(S3Like(DirStore::new(&objects).unwrap()));

    let mut v = Volume::open(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        Some(Create::new(size)),
        Policy::default(),
    )
    .unwrap();

    // Upload the whole first object.
    v.write(0, obj_blocks, &data).unwrap();
    v.flush().unwrap();

    // Change one block in it.
    let changed = vec![0xEEu8; BLOCK as usize];
    v.write(7, 1, &changed).unwrap();
    v.flush().unwrap();

    // Everything except block 7 must still be there.
    let mut got = vec![0u8; data.len()];
    v.read(0, obj_blocks, &mut got).unwrap();
    for b in 0..obj_blocks as u64 {
        let off = (b * BLOCK) as usize;
        let want = if b == 7 {
            &changed[..]
        } else {
            &data[off..off + BLOCK as usize]
        };
        assert_eq!(
            &got[off..off + BLOCK as usize],
            want,
            "block {b} was lost when an uploaded object was rewritten"
        );
    }
    std::fs::remove_dir_all(&root).ok();
}

/// A restart must see the data a partial-write backend uploaded.
///
/// The patched path updates objects in place and used to leave no entry in
/// the metadata, so the saved state claimed the volume was empty. Everything
/// read back as zeros afterwards even though the objects were intact.
#[test]
fn restart_sees_objects_uploaded_by_a_partial_write_backend() {
    let root = std::env::temp_dir().join(format!("blobdev-pwm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = 4 * DEFAULT_CHUNK;
    let data = pattern(2000);

    {
        let store = Arc::new(DirStore::new(&objects).unwrap());
        assert!(store.caps().partial_write, "test needs the patched path");
        let mut v = Volume::open(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(size)),
            Policy::default(),
        )
        .unwrap();
        v.write(0, 2000, &data).unwrap();
        v.flush().unwrap();
    }

    // A fresh process must still know those objects exist.
    let store = Arc::new(DirStore::new(&objects).unwrap());
    let mut v = Volume::open(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .unwrap();
    let mut got = vec![0u8; data.len()];
    v.read(0, 2000, &mut got).unwrap();
    let mut bad = Vec::new();
    for b in 0..2000u64 {
        let off = (b * BLOCK) as usize;
        if got[off..off + BLOCK as usize] != data[off..off + BLOCK as usize] {
            bad.push((b, got[off], data[off]));
            if bad.len() >= 3 {
                break;
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a restart lost data the patched path had uploaded: (idx, got, want) {bad:?}"
    );
    std::fs::remove_dir_all(&root).ok();
}


/// Objects patched in place are keyed by index, and that must not collide
/// with a real uid of 0.
///
/// `chunk_key(0)` and the index key of object 0 are the same string. Treating
/// the "patched in place" marker as a real uid made every object resolve to
/// object 0's bytes, so a volume read back the first 4 MiB over and over.
#[test]
fn patched_objects_do_not_alias_object_zero() {
    let root = std::env::temp_dir().join(format!("blobdev-alias-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let size = 4 * DEFAULT_CHUNK;
    let obj_blocks = blobdev::volume::DEFAULT_OBJECT / BLOCK;
    let n = (obj_blocks * 3) as usize;
    // Each block carries its own index, so a shifted read is obvious.
    let mut data = vec![0u8; n * BLOCK as usize];
    for b in 0..n {
        data[b * BLOCK as usize..(b + 1) * BLOCK as usize].fill((b % 251) as u8);
    }

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
        v.write(0, n, &data).unwrap();
        v.flush().unwrap();
    }

    let store = Arc::new(DirStore::new(&objects).unwrap());
    let mut v = Volume::open(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .unwrap();
    let mut got = vec![0u8; data.len()];
    v.read(0, n, &mut got).unwrap();
    let mut bad = Vec::new();
    for b in 0..n {
        if got[b * BLOCK as usize] != (b % 251) as u8 {
            bad.push((b, got[b * BLOCK as usize], (b % 251) as u8));
            if bad.len() >= 3 {
                break;
            }
        }
    }
    assert!(
        bad.is_empty(),
        "read back the wrong object's bytes: (block, got, want) {bad:?}"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// A backend that discovers it cannot patch must not be asked again.
///
/// Real servers do this: Nextcloud answers a ranged PUT with 400. Falling
/// back to a whole-object upload is correct, but repeating the rejected
/// attempt for every object wastes a request each time -- 19 of them in one
/// run against Nextcloud, before the store started remembering.
#[test]
fn a_backend_that_cannot_patch_is_asked_only_once() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    struct NoRangedPatch {
        inner: DirStore,
        patches: AtomicU32,
        refuses: AtomicBool,
    }
    impl ObjectStore for NoRangedPatch {
        fn caps(&self) -> Caps {
            Caps {
                range_read: true,
                partial_write: !self.refuses.load(Ordering::SeqCst),
                conditional_write: true,
                expensive_range_read: false,
            }
        }
        fn stat(&self, key: &str) -> anyhow::Result<Option<u64>> {
            self.inner.stat(key)
        }
        fn get(&self, key: &str, range: Option<std::ops::Range<u64>>) -> anyhow::Result<Vec<u8>> {
            self.inner.get(key, range)
        }
        fn put(&self, key: &str, data: &[u8]) -> anyhow::Result<Option<String>> {
            self.inner.put(key, data)
        }
        fn patch(&self, _key: &str, _offset: u64, _data: &[u8]) -> anyhow::Result<()> {
            self.patches.fetch_add(1, Ordering::SeqCst);
            self.refuses.store(true, Ordering::SeqCst);
            anyhow::bail!("partial write not supported by this server")
        }
        fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.inner.delete(key)
        }
        fn list(&self, prefix: &str) -> anyhow::Result<Vec<(String, u64)>> {
            self.inner.list(prefix)
        }
    }

    let root = std::env::temp_dir().join(format!("blobdev-once-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let store = Arc::new(NoRangedPatch {
        inner: DirStore::new(&objects).unwrap(),
        patches: AtomicU32::new(0),
        refuses: AtomicBool::new(false),
    });

    let obj_blocks = blobdev::volume::DEFAULT_OBJECT / BLOCK;
    let mut v = Volume::open(
        store.clone() as Arc<dyn ObjectStore>,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        Some(Create::new(4 * DEFAULT_CHUNK)),
        Policy::default(),
    )
    .unwrap();

    // Write and upload several objects whole, so later writes have something
    // to patch rather than create.
    for round in 0..4u64 {
        let base = round * obj_blocks;
        let mut data = vec![0u8; obj_blocks as usize * BLOCK as usize];
        data.fill(round as u8 + 1);
        v.write(base, obj_blocks as usize, &data).unwrap();
        v.flush().unwrap();
    }
    // Now a small write to each existing object, which is what a patch is for.
    for round in 0..4u64 {
        let base = round * obj_blocks;
        let mut data = vec![0u8; BLOCK as usize];
        data.fill(0xF0 + round as u8);
        v.write(base + 2, 1, &data).unwrap();
        v.flush().unwrap();
    }

    assert_eq!(
        store.patches.load(Ordering::SeqCst),
        1,
        "the server was asked to patch again after refusing once"
    );

    // Every object must still read back: the fallbacks wrote them whole.
    let mut buf = vec![0u8; BLOCK as usize];
    for round in 0..4u64 {
        v.read(round * obj_blocks, 1, &mut buf).unwrap();
        assert_eq!(buf[0], round as u8 + 1, "object {round} was not written");
        v.read(round * obj_blocks + 2, 1, &mut buf).unwrap();
        assert_eq!(buf[0], 0xF0 + round as u8, "patch {round} was lost");
    }
    std::fs::remove_dir_all(&root).ok();
}

/// A uid and an object index must never name the same key.
///
/// Patched objects are stored under their index (`c/<id>`) and uid-keyed ones
/// under `c/u<uid>`. Without the prefix, uid 4 and object 4 are the same
/// string: superseding the patched object 4 deleted the uid-keyed object 4,
/// and the volume lost that data while its metadata kept pointing at it.
///
/// This is reachable on any backend that starts by patching (WebDAV) and then
/// switches to whole-object writes, which is exactly what Nextcloud makes it
/// do by rejecting a ranged PUT.
#[test]
fn a_uid_never_collides_with_an_object_index() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    struct Switches {
        inner: DirStore,
        patches: AtomicU32,
        refuses: AtomicBool,
        keys: std::sync::Mutex<Vec<String>>,
    }
    impl ObjectStore for Switches {
        fn caps(&self) -> Caps {
            Caps {
                range_read: true,
                partial_write: !self.refuses.load(Ordering::SeqCst),
                conditional_write: false,
                expensive_range_read: true,
            }
        }
        fn stat(&self, key: &str) -> anyhow::Result<Option<u64>> {
            self.inner.stat(key)
        }
        fn get(&self, key: &str, range: Option<std::ops::Range<u64>>) -> anyhow::Result<Vec<u8>> {
            self.inner.get(key, range)
        }
        fn put(&self, key: &str, data: &[u8]) -> anyhow::Result<Option<String>> {
            self.keys.lock().unwrap().push(key.to_string());
            self.inner.put(key, data)
        }
        fn patch(&self, k: &str, o: u64, d: &[u8]) -> anyhow::Result<()> {
            // The first patch succeeds so the store gets index-keyed objects;
            // later ones are refused, which pushes the volume onto the
            // uid-keyed path. The collision only exists when both are present.
            if self.patches.fetch_add(1, Ordering::SeqCst) == 0 {
                return self.inner.patch(k, o, d);
            }
            self.refuses.store(true, Ordering::SeqCst);
            anyhow::bail!("partial write not supported")
        }
        fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.inner.delete(key)
        }
        fn list(&self, prefix: &str) -> anyhow::Result<Vec<(String, u64)>> {
            self.inner.list(prefix)
        }
    }

    let root = std::env::temp_dir().join(format!("blobdev-collide-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let obj_blocks = blobdev::volume::DEFAULT_OBJECT / BLOCK;
    let bytes = obj_blocks * BLOCK * 24; // enough objects for the indices to matter
    let mut data = vec![0u8; bytes as usize];
    for b in 0..(bytes / BLOCK) {
        data[(b * BLOCK) as usize..((b + 1) * BLOCK) as usize]
            .fill((b as u8).wrapping_mul(13).wrapping_add(5));
    }

    let store = Arc::new(Switches {
        inner: DirStore::new(&objects).unwrap(),
        patches: AtomicU32::new(0),
        refuses: AtomicBool::new(false),
        keys: std::sync::Mutex::new(Vec::new()),
    });
    {
        let mut v = Volume::open(
            store.clone() as Arc<dyn ObjectStore>,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(8 * DEFAULT_CHUNK)),
            Policy::default(),
        )
        .unwrap();
        let piece = (1 << 20) / BLOCK as usize;
        let blocks = (bytes / BLOCK) as usize;
        let mut done = 0;
        while done < blocks {
            let n = piece.min(blocks - done);
            v.write(
                done as u64,
                n,
                &data[done * BLOCK as usize..(done + n) * BLOCK as usize],
            )
            .unwrap();
            done += n;
        }
        v.flush().unwrap();
        // A second pass over the same objects: the first patch succeeds (index
        // key), the rest are refused (uid keys).
        done = 0;
        while done < blocks {
            let n = piece.min(blocks - done);
            v.write(
                done as u64,
                n,
                &data[done * BLOCK as usize..(done + n) * BLOCK as usize],
            )
            .unwrap();
            done += n;
            v.flush().unwrap();
        }
    }

    // The two namespaces must not overlap as strings. A small volume has uids
    // in the same numeric range as object indices, so the prefix is what keeps
    // them apart -- without it uid 4 and object 4 are both `c/4`.
    let keys = store.keys.lock().unwrap().clone();
    assert!(!keys.is_empty(), "nothing was written");
    for k in &keys {
        let name = k.rsplit('/').next().unwrap_or_default();
        let numeric = name.parse::<u64>().is_ok();
        let prefixed = name.starts_with('u');
        assert!(
            numeric || prefixed,
            "unexpected key spelling: {k} (index-keyed and uid-keyed objects \
             must be distinguishable by name)"
        );
    }
    let both_kinds = keys.iter().any(|k| {
        let n = k.rsplit('/').next().unwrap_or_default();
        n.starts_with('u')
    }) && keys.iter().any(|k| {
        let n = k.rsplit('/').next().unwrap_or_default();
        n.parse::<u64>().is_ok()
    });
    assert!(both_kinds, "the test did not produce both key kinds: {keys:?}");

    let store = Arc::new(Switches {
        inner: DirStore::new(&objects).unwrap(),
        patches: AtomicU32::new(1),
        refuses: AtomicBool::new(true),
        keys: std::sync::Mutex::new(Vec::new()),
    });
    let mut v = Volume::open(
        store as Arc<dyn ObjectStore>,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .unwrap();
    let blocks = (bytes / BLOCK) as usize;
    let mut got = vec![0u8; data.len()];
    let piece = (1 << 20) / BLOCK as usize;
    let mut done = 0;
    while done < blocks {
        let n = piece.min(blocks - done);
        v.read(
            done as u64,
            n,
            &mut got[done * BLOCK as usize..(done + n) * BLOCK as usize],
        )
        .unwrap();
        done += n;
    }
    let mut bad = Vec::new();
    for b in 0..blocks {
        let off = b * BLOCK as usize;
        if got[off] != data[off] {
            bad.push(b as u64);
            if bad.len() > 5 {
                break;
            }
        }
    }
    assert!(bad.is_empty(), "blocks lost to a key collision: {bad:?}");
    std::fs::remove_dir_all(&root).ok();
}
