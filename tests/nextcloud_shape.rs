//! Reproduce the shape of the Nextcloud failure without a device.
//!
//! Writing 24 MiB through the device and restarting brings back the first
//! 8 MiB and zeros after it. This drives the same Volume with an object store
//! that behaves like Nextcloud -- whole-object replacement only -- to find out
//! whether the fault is in the volume layer or above it.

use std::sync::Arc;

use blobdev::meta::FileMeta;
use blobdev::store::{Caps, DirStore, ObjectStore};
use blobdev::volume::{Create, Layout, Policy, Volume, BLOCK, DEFAULT_CHUNK};

/// DirStore with S3-like capabilities: no partial writes.
struct WholeObjectOnly(DirStore);
impl ObjectStore for WholeObjectOnly {
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

fn pattern(bytes: u64, seed: u8) -> Vec<u8> {
    // Every 4 KiB block carries a distinct byte, derived from its index, so a
    // block that comes back from the wrong place is obvious.
    let mut v = vec![0u8; bytes as usize];
    for b in 0..(bytes / BLOCK) {
        v[(b * BLOCK) as usize..((b + 1) * BLOCK) as usize]
            .fill((b as u8).wrapping_mul(7).wrapping_add(seed));
    }
    v
}

/// Write `mib` MiB in 1 MiB pieces, flush, reopen, read back.
fn roundtrip(mib: u64, keep_disk_cache: bool) -> Vec<u64> {
    let root = std::env::temp_dir().join(format!(
        "blobdev-nc-{}-{mib}-{keep_disk_cache}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let cache_dir = root.join("cache");
    let bytes = mib << 20;
    let data = pattern(bytes, 3);
    let blocks = (bytes / BLOCK) as usize;

    {
        let store = Arc::new(WholeObjectOnly(DirStore::new(&objects).unwrap()));
        let cache = if keep_disk_cache {
            Some(blobdev::volume::DiskCache::new(&cache_dir, 64 << 20))
        } else {
            None
        };
        let mut v = Volume::open_with_cache(
            store,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(8 * DEFAULT_CHUNK)),
            Policy::default(),
            cache,
        )
        .unwrap();
        let piece = (1 << 20) / BLOCK as usize;
        let mut done = 0usize;
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
    }

    let store = Arc::new(WholeObjectOnly(DirStore::new(&objects).unwrap()));
    let cache = if keep_disk_cache {
        Some(blobdev::volume::DiskCache::new(&cache_dir, 64 << 20))
    } else {
        None
    };
    let mut v = Volume::open_with_cache(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
        cache,
    )
    .unwrap();
    let mut got = vec![0u8; data.len()];
    let piece = (1 << 20) / BLOCK as usize;
    let mut done = 0usize;
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
        }
    }
    std::fs::remove_dir_all(&root).ok();
    bad
}

#[test]
fn whole_object_backend_round_trips_every_size() {
    for mib in [4u64, 8, 12, 16, 20, 24, 32] {
        let bad = roundtrip(mib, false);
        assert!(
            bad.is_empty(),
            "{mib} MiB: {} block(s) came back wrong, first at {:?} (block {})",
            bad.len(),
            bad.first(),
            bad.first().copied().unwrap_or(0) * BLOCK
        );
    }
}

#[test]
fn whole_object_backend_round_trips_with_disk_cache() {
    for mib in [12u64, 24] {
        let bad = roundtrip(mib, true);
        assert!(bad.is_empty(), "{mib} MiB with disk cache: {bad:?}");
    }
}

/// The same thing through a store that starts out claiming partial writes and
/// then refuses them, which is what Nextcloud does.
#[test]
fn a_backend_that_flips_to_whole_object_mid_run() {
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Flaky {
        inner: DirStore,
        patches: AtomicU32,
    }
    impl ObjectStore for Flaky {
        fn caps(&self) -> Caps {
            Caps {
                range_read: true,
                partial_write: self.patches.load(Ordering::SeqCst) == 0,
                conditional_write: false,
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
        fn patch(&self, _k: &str, _o: u64, _d: &[u8]) -> anyhow::Result<()> {
            self.patches.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("partial write not supported")
        }
        fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.inner.delete(key)
        }
        fn list(&self, prefix: &str) -> anyhow::Result<Vec<(String, u64)>> {
            self.inner.list(prefix)
        }
    }

    let root = std::env::temp_dir().join(format!("blobdev-flip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let objects = root.join("objects");
    let meta_path = root.join("meta");
    let bytes = 24u64 << 20;
    let data = pattern(bytes, 11);
    let blocks = (bytes / BLOCK) as usize;

    {
        let store = Arc::new(Flaky {
            inner: DirStore::new(&objects).unwrap(),
            patches: AtomicU32::new(0),
        });
        let mut v = Volume::open(
            store.clone() as Arc<dyn ObjectStore>,
            Box::new(FileMeta::new(&meta_path)),
            Layout::new("vol"),
            Some(Create::new(8 * DEFAULT_CHUNK)),
            Policy::default(),
        )
        .unwrap();
        let piece = (1 << 20) / BLOCK as usize;
        let mut done = 0usize;
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
    }

    let store = Arc::new(Flaky {
        inner: DirStore::new(&objects).unwrap(),
        patches: AtomicU32::new(1), // the new process finds it cannot patch
    });
    let mut v = Volume::open(
        store as Arc<dyn ObjectStore>,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new("vol"),
        None,
        Policy::default(),
    )
    .unwrap();
    let mut got = vec![0u8; data.len()];
    let piece = (1 << 20) / BLOCK as usize;
    let mut done = 0usize;
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
            if bad.len() > 6 {
                break;
            }
        }
    }
    assert!(bad.is_empty(), "blocks wrong after a capability flip: {bad:?}");
    std::fs::remove_dir_all(&root).ok();
}
