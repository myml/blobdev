//! Uploading must not corrupt chunks.
//!
//! Reproduces the S3 path end to end at the volume level: write a
//! multi-chunk file, flush, drop all caches and read it back. Any mismatch is
//! an upload bug, not a caching one.

use std::sync::Arc;

use blobdev::meta::{FileMeta, MemMeta};
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
