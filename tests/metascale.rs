// How expensive is a metadata save once a volume has touched many objects?
//
// Every write saves the whole snapshot: it is the price of surviving kill -9.
// The cost that matters is not the volume size but the number of objects the
// volume has ever touched -- mkfs on a 1 TiB volume leaves hundreds.
//
// Run with: cargo test --release --test metascale -- --nocapture
use std::time::Instant;

use blobdev::meta::{FileMeta, MetaStore, ChunkState, Snapshot, Geometry};

#[test]
fn metadata_save_cost_at_realistic_object_counts() {
    let root = std::env::temp_dir().join(format!("blobdev-scale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("meta");

    let geometry = Geometry {
        chunk_size: 64 << 20,
        object_size: 4 << 20,
        size: 1 << 40,
    };

    println!("{:>10} {:>12} {:>12}", "objects", "snapshot", "save ms");
    for n in [100usize, 801, 5000, 20000] {
        let mut chunks = std::collections::BTreeMap::new();
        for i in 0..n {
            let mut dirty = std::collections::BTreeMap::new();
            // A realistic sample: some objects have a few dirty blocks.
            for b in 0..3u64 {
                dirty.insert(b, ());
            }
            chunks.insert(
                i as u64,
                ChunkState {
                    dirty,
                    uploaded: true,
                    uid: 0,
                },
            );
        }
        let snap = Snapshot {
            geometry: geometry.clone(),
            chunks,
        };
        let mut store = FileMeta::new(&path);
        // Warm up, then measure.
        store.save(&snap).unwrap();
        let t = Instant::now();
        let rounds = 20;
        for _ in 0..rounds {
            store.save(&snap).unwrap();
        }
        let per = t.elapsed().as_secs_f64() * 1000.0 / rounds as f64;
        println!(
            "{:>10} {:>12} {:>12.3}",
            n,
            snap.encode().len(),
            per
        );
    }
    std::fs::remove_dir_all(&root).ok();
}
