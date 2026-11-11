//! Reads in flight must not block each other.
//!
//! The volume is shared behind one mutex. If a read held that mutex while it
//! talked to the object store, a slow backend would serialise every queue
//! thread and the background uploader. This test uses a deliberately slow
//! backend to prove the fetch phase runs unlocked.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use blobdev::store::{Caps, ObjectStore};
use blobdev::volume::{Create, Layout, Policy, Volume, BLOCK, DEFAULT_CHUNK};

/// A store where every GET sleeps, recording how many were in flight at once.
struct SlowStore {
    inner: blobdev::store::MemStore,
    delay: Duration,
    in_flight: AtomicU64,
    max_in_flight: AtomicU64,
}

impl SlowStore {
    fn new(delay: Duration) -> Arc<SlowStore> {
        Arc::new(SlowStore {
            inner: blobdev::store::MemStore::new(),
            delay,
            in_flight: AtomicU64::new(0),
            max_in_flight: AtomicU64::new(0),
        })
    }
}

impl ObjectStore for SlowStore {
    fn caps(&self) -> Caps {
        Caps::FULL
    }
    fn stat(&self, key: &str) -> Result<Option<u64>> {
        self.inner.stat(key)
    }
    fn get(&self, key: &str, range: Option<std::ops::Range<u64>>) -> Result<Vec<u8>> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        let out = self.inner.get(key, range);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        out
    }
    fn put(&self, key: &str, data: &[u8]) -> Result<Option<String>> {
        self.inner.put(key, data)
    }
    fn delete(&self, key: &str) -> Result<()> {
        self.inner.delete(key)
    }
}

fn blocks(n: usize, v: u8) -> Vec<u8> {
    vec![v; n * BLOCK as usize]
}

#[test]
fn concurrent_reads_overlap_instead_of_serialising() {
    let delay = Duration::from_millis(150);
    let store = SlowStore::new(delay);

    // Populate a few chunks, then flush so reads must come from the store.
    let dir = std::env::temp_dir().join(format!("blobdev-conc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let meta = dir.join("meta");
    let size = 8 * DEFAULT_CHUNK;
    {
        let mut v = Volume::open(
            store.clone() as Arc<dyn ObjectStore>,
            Box::new(blobdev::meta::FileMeta::new(&meta)),
            Layout::new("v"),
            Some(Create::new(size)),
            Policy::default(),
        )
        .unwrap();
        // One block in each of 4 chunks.
        for c in 0..4u64 {
            v.write(c * (DEFAULT_CHUNK / BLOCK), 1, &blocks(1, 0x10 + c as u8))
                .unwrap();
        }
        v.flush().unwrap();
    }

    let volume = Arc::new(Mutex::new(
        Volume::open(
            store.clone() as Arc<dyn ObjectStore>,
            Box::new(blobdev::meta::FileMeta::new(&meta)),
            Layout::new("v"),
            None,
            Policy::default(),
        )
        .unwrap(),
    ));

    // Four threads read four different chunks at once.
    let threads = 4u64;
    let started = Instant::now();
    let mut handles = Vec::new();
    for c in 0..threads {
        let volume = volume.clone();
        handles.push(std::thread::spawn(move || {
            let blk = c * (DEFAULT_CHUNK / BLOCK);
            // Plan under the lock, fetch outside it, exactly like the ublk path.
            let (plan, fetcher) = {
                let mut v = volume.lock().unwrap();
                (v.plan_read(blk, 1).unwrap(), v.fetcher())
            };
            let mut fetched = Vec::new();
            for req in &plan.requests {
                fetched.push((req.clone(), fetcher.fetch(req).unwrap()));
            }
            {
                let mut v = volume.lock().unwrap();
                for (req, data) in fetched {
                    v.apply_fetch(&req, data);
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let elapsed = started.elapsed();

    let max = store.max_in_flight.load(Ordering::SeqCst);
    assert!(
        max >= 2,
        "expected overlapping fetches, but at most {max} were in flight"
    );
    // Serialised would be at least 4 * delay.
    assert!(
        elapsed < delay * 3,
        "four {delay:?} fetches took {elapsed:?}; they were serialised"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn read_plan_reports_what_is_missing() {
    let store = Arc::new(blobdev::store::MemStore::new());
    let dir = std::env::temp_dir().join(format!("blobdev-plan-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let meta = dir.join("meta");
    let mut v = Volume::open(
        store,
        Box::new(blobdev::meta::FileMeta::new(&meta)),
        Layout::new("v"),
        Some(Create::new(DEFAULT_CHUNK)),
        Policy::default(),
    )
    .unwrap();

    // Nothing cached yet: one request for the addressed chunk. A single block
    // read is small relative to the chunk, so it must be a range fetch.
    let plan = v.plan_read(0, 1).unwrap();
    assert_eq!(plan.requests.len(), 1);
    assert_eq!(plan.requests[0].chunk, 0);
    assert!(
        !plan.requests[0].whole,
        "a one-block read should use a range fetch, not pull the chunk"
    );
    assert_eq!(plan.requests[0].len, BLOCK);

    // A range fetch is not cached as a chunk, so the next plan asks again;
    // that is the intended trade-off (a miss costs one block, not a chunk).
    let fetcher = v.fetcher();
    for req in &plan.requests {
        let data = fetcher.fetch(req).unwrap();
        v.apply_fetch(req, data);
    }
    assert_eq!(v.partial_reads(), 1);
    assert_eq!(v.plan_read(0, 1).unwrap().requests.len(), 1);

    // A read covering most of the chunk pulls and caches the whole chunk, so
    // the next plan is satisfied from cache.
    let big = v.plan_read(0, (DEFAULT_CHUNK / BLOCK / 2) as usize).unwrap();
    assert!(big.requests.iter().all(|r| r.whole));
    let fetcher = v.fetcher();
    for req in &big.requests {
        let data = fetcher.fetch(req).unwrap();
        v.apply_fetch(req, data);
    }
    assert!(
        v.plan_read(0, 1).unwrap().is_empty(),
        "a whole-chunk fetch should satisfy subsequent reads from cache"
    );

    // A working copy also needs no fetch.
    v.write(10, 1, &blocks(1, 7)).unwrap();
    assert!(v.plan_read(10, 1).unwrap().is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn writes_do_not_wait_for_the_object_store() -> Result<()> {
    let store = SlowStore::new(Duration::from_millis(200));
    let dir = std::env::temp_dir().join(format!("blobdev-wconc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let meta = dir.join("meta");
    let mut v = Volume::open(
        store.clone() as Arc<dyn ObjectStore>,
        Box::new(blobdev::meta::FileMeta::new(&meta)),
        Layout::new("v"),
        Some(Create::new(DEFAULT_CHUNK)),
        Policy::default(),
    )?;

    let started = Instant::now();
    for b in 0..50u64 {
        v.write(b, 1, &blocks(1, b as u8))?;
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(200),
        "50 writes took {elapsed:?}; a write must not wait on the backend"
    );
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
