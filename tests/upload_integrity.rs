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
