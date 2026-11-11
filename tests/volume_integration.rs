//! Integration tests for the volume/backend layering, runnable anywhere.
//!
//! These drive the same [`Volume`] the ublk layer uses, against a real
//! directory backend, so the chunk/upload/restore logic is exercised without
//! needing the ublk kernel driver.

use std::sync::Arc;

use blobdev::meta::{FileMeta, Geometry};
use blobdev::store::DirStore;
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
