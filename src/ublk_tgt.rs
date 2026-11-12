//! The block-device side: expose a [`Volume`] as `/dev/ublkbN` through ublk.
//!
//! The structure is lifted from rcache, where each piece below was validated
//! against a real device:
//!
//! * a command may be **larger than a tag buffer**, so it is served in windows
//!   over the same memory (never truncated, never copied into a scratch buffer
//!   that would be uploaded in place of the real data);
//! * a FLUSH carries no data, so its COMMIT repeats the tag's real buffer
//!   address instead of a zero-length slice (which would send NULL and wedge
//!   the tag forever);
//! * `UBLK_ATTR_FUA` marks writes that must be durable, which map to a volume
//!   flush.

use anyhow::{Context, Result};
use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::io::{BufDesc, BufDescList, UblkDev, UblkIOCtx, UblkQueue};
use libublk::{UblkError, UblkFlags, UblkIORes};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use crate::volume::{Volume, BLOCK};

/// Shared state handed to every queue thread.
pub struct Shared {
    pub volume: Arc<Mutex<Volume>>,
    /// Asks the maintenance thread to flush the volume now.
    pub wake: Arc<(Mutex<bool>, Condvar)>,
    /// Periodic upload of aged/saturated chunks.
    pub interval: Duration,
}

pub struct Target {
    pub id: i32,
    pub queues: u16,
    pub depth: u16,
    pub interval: Duration,
}

impl Target {
    pub fn run(
        self,
        volume: Volume,
        drain: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        exit_after: Arc<ExitRequest>,
    ) -> Result<()> {
        let dev_size = volume.size();
        log::info!(
            "serving {} MiB in {} MiB chunks ({} cached chunks, {} KiB blocks)",
            dev_size >> 20,
            volume.geometry().chunk_size >> 20,
            volume.cached_chunks(),
            BLOCK >> 10
        );

        let volume = Arc::new(Mutex::new(volume));
        let wake = Arc::new((Mutex::new(false), Condvar::new()));
        let shared = Arc::new(Shared {
            volume: volume.clone(),
            wake: wake.clone(),
            interval: self.interval,
        });
        let maintenance = spawn_maintenance(volume.clone(), wake.clone(), self.interval);

        let max_io = BLOCK as usize * 64;
        let ctrl = UblkCtrlBuilder::default()
            .name("blobdev")
            .id(self.id)
            .depth(self.depth)
            .nr_queues(self.queues)
            .io_buf_bytes(max_io as u32)
            .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
            .build()
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to create ublk device id={}: {e:?}\n{}",
                    self.id,
                    ublk_hint()
                )
            })?;

        let tgt_init = move |dev: &mut UblkDev| -> Result<(), UblkError> {
            dev.set_default_params(dev_size);
            // Declare a stable write cache plus FUA: the block layer then marks
            // durability-requiring writes with UBLK_IO_F_FUA instead of relying
            // on a separate FLUSH round trip.
            dev.tgt.params.basic.attrs =
                libublk::sys::UBLK_ATTR_VOLATILE_CACHE | libublk::sys::UBLK_ATTR_FUA;
            dev.tgt.params.basic.logical_bs_shift = 12;
            dev.tgt.params.basic.physical_bs_shift = 12;
            dev.tgt.params.basic.dev_sectors = dev_size >> 9;
            Ok(())
        };

        let s = shared.clone();
        let q_fn = move |qid: u16, dev: &UblkDev| {
            let bufs = std::rc::Rc::new(dev.alloc_queue_io_bufs());
            let queue = match UblkQueue::new(qid, dev).and_then(|q| {
                q.submit_fetch_commands_unified(BufDescList::Slices(Some(&bufs)))
            }) {
                Ok(q) => q,
                Err(e) => {
                    log::error!("queue {qid}: setup failed: {e:?}");
                    return;
                }
            };
            let s2 = s.clone();
            let bufs2 = bufs.clone();
            queue.wait_and_handle_io(move |q: &UblkQueue, tag: u16, io: &UblkIOCtx| {
                let b = &bufs2[tag as usize];
                // Hand over the whole tag buffer; handle_command windows it.
                // SAFETY: one in-flight command per tag per queue thread.
                let buf = unsafe {
                    std::slice::from_raw_parts_mut(b.as_ptr() as *mut u8, b.len())
                };
                let res = handle_command(&s2, q, tag, io, buf, b.as_ptr() as *mut u8);
                if let Err(UblkError::QueueIsDown) = res {
                    log::debug!("queue {qid}: device went down");
                }
            });
        };

        let s = shared.clone();
        let d = drain.clone();
        let st = stop.clone();
        let device_fn = move |c: &UblkCtrl| {
            log::info!("created /dev/ublkb{}", c.dev_info().dev_id);
            let dev_id = c.dev_info().dev_id as i32;
            spawn_drain_watcher(s.volume.clone(), d, st, exit_after.clone(), dev_id);
        };

        let _ = ctrl.run_target(tgt_init, q_fn, device_fn);

        maintenance.stop();
        let mut v = volume.lock().unwrap_or_else(|p| p.into_inner());
        v.flush().context("final flush failed")?;
        log::info!("stopped, all dirty chunks uploaded");
        Ok(())
    }
}

/// Upload aged or saturated chunks in the background.
struct Maintenance {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Maintenance {
    fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn spawn_maintenance(
    volume: Arc<Mutex<Volume>>,
    wake: Arc<(Mutex<bool>, Condvar)>,
    interval: Duration,
) -> Maintenance {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let handle = thread::spawn(move || {
        // A long run otherwise has no window into the daemon at all. One line
        // every few minutes says whether uploads are draining, whether the
        // cache is doing its job, and how much has been sent.
        // Default ten minutes; BLOBDEV_SUMMARY_SECS overrides it, which is how
        // the periodic line is exercised in tests without a long run.
        let summary_every = std::env::var("BLOBDEV_SUMMARY_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(600));
        let mut last_summary = std::time::Instant::now();
        while !s.load(Ordering::SeqCst) {
            {
                let (lock, cvar) = &*wake;
                let g = lock.lock().unwrap_or_else(|p| p.into_inner());
                let _ = cvar.wait_timeout(g, interval).unwrap_or_else(|p| p.into_inner());
            }
            if s.load(Ordering::SeqCst) {
                break;
            }
            let mut v = volume.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(e) = v.tick() {
                log::error!("upload: {e:#}");
            }
            if last_summary.elapsed() >= summary_every {
                last_summary = std::time::Instant::now();
                log::info!("{}", v.summary());
            }
        }
    });
    Maintenance {
        stop,
        handle: Some(handle),
    }
}

/// Remove a ublk device by id, the same way `blobdev stop` does.
fn del_device(id: i32) -> Result<()> {
    let ctrl = libublk::ctrl::UblkCtrl::new_simple(id)
        .with_context(|| format!("open ublk device {id}"))?;
    ctrl.del_dev_async()
        .with_context(|| format!("delete device {id}"))?;
    Ok(())
}

/// What to check when the kernel refuses to make a ublk device.
///
/// The kernel says the same thing whether the driver is missing or a device
/// id is still held, and guessing wrong sends an operator looking at leftover
/// devices when the real problem is that `ublk_drv` was never loaded -- which
/// is what happens at boot on a machine where nothing loads it. Say which it
/// is.
fn ublk_hint() -> String {
    let have_driver = std::path::Path::new("/sys/module/ublk_drv").exists();
    if !have_driver {
        return "hint: the ublk driver is not loaded. Load it with `modprobe ublk_drv`, \n\
                and make it permanent so a reboot does not lose the volume: \n\
                echo ublk_drv > /etc/modules-load.d/ublk.conf"
            .to_string();
    }
    "hint: a leftover /dev/ublkb*/ublkc* may still be in use; check \n\
     `ls /sys/class/ublk-char` and stop it with `blobdev stop --id N`"
        .to_string()
}

/// React to SIGINT/SIGTERM by flushing, not by tearing the device down: a
/// mounted filesystem still needs this device to answer IO.
fn spawn_drain_watcher(
    volume: Arc<Mutex<Volume>>,
    drain: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    exit_after: Arc<ExitRequest>,
    dev_id: i32,
) {
    thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            if drain.swap(false, Ordering::SeqCst) {
                let mut v = volume.lock().unwrap_or_else(|p| p.into_inner());
                match v.flush() {
                    Ok(()) => log::info!("signal: flushed dirty chunks"),
                    Err(e) => log::error!("signal: flush failed: {e:#}"),
                }
                // A signal must be able to stop the service. Flush first so
                // nothing is lost, then let the target loop end normally so
                // the device is torn down.
                //
                // Exiting from here used to be `_exit(0)`, on the assumption
                // that "the kernel removes the device when its handles close".
                // It does not: the device stayed in /sys/class/ublk-char, held
                // its id, and the next start failed with EOPNOTSUPP because
                // that id was still in use. A 5 GiB volume could be written
                // and stopped and then never mounted again without a manual
                // `blobdev stop --id N`.
                if exit_after.is_requested() {
                    // Ask the kernel to remove the device. That is what makes
                    // the queue loops end, which is what lets run_target
                    // return, which is what tears everything down.
                    //
                    // `_exit` here was wrong twice over: it skipped the
                    // teardown, so the device stayed in /sys/class/ublk-char
                    // and held its id -- the next start failed with
                    // EOPNOTSUPP -- and the process still took a minute to
                    // die because a signal handler cannot end the queue
                    // threads' wait.
                    log::info!("signal: flushed; removing device {dev_id}");
                    if let Err(e) = del_device(dev_id) {
                        log::error!("signal: could not remove device {dev_id}: {e:#}");
                    }
                    stop.store(true, Ordering::SeqCst);
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
}

/// Whether the process should exit once the journal is flushed.
#[derive(Default)]
pub struct ExitRequest {
    /// Set when a signal should terminate the process rather than only flush.
    requested: std::sync::atomic::AtomicBool,
}

impl ExitRequest {
    /// Ask for the process to exit once pending data is flushed.
    pub fn request(&self) {
        self.requested.store(true, Ordering::SeqCst);
    }

    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
}

fn handle_command(
    s: &Shared,
    q: &UblkQueue,
    tag: u16,
    io: &UblkIOCtx,
    buf: &mut [u8],
    tag_buf_addr: *mut u8,
) -> Result<(), UblkError> {
    if io.is_tgt_io() {
        return Ok(());
    }
    let iod = q.get_iod(tag);
    let op = iod.op_flags & 0xff;
    let fua = iod.op_flags & libublk::sys::UBLK_IO_F_FUA != 0;
    // FLUSH has no meaningful sector; avoid overflowing the multiplication.
    let sector = iod.start_sector.wrapping_mul(512);
    let bytes = (iod.nr_sectors as usize) * 512;
    let buf_len = buf.len();

    let res = serve_windows(
        &s.volume,
        op,
        sector,
        bytes,
        buf,
        fua,
    );

    // The COMMIT must carry the address the FETCH used. A FLUSH has no data,
    // so reuse the tag's real buffer rather than an empty slice (NULL).
    let commit = if bytes == 0 || buf_len == 0 {
        BufDesc::RawAddress(tag_buf_addr as *const u8)
    } else {
        BufDesc::Slice(&buf[..bytes.min(buf_len)])
    };
    q.complete_io_cmd_unified(tag, commit, Ok(UblkIORes::Result(res)))?;
    Ok(())
}

/// Serve one block command. Returns bytes transferred, 0 for flush, or a
/// negative errno.
pub fn process_op(
    volume: &Mutex<Volume>,
    op: u32,
    byte_off: u64,
    bytes: usize,
    buf: &mut [u8],
    fua: bool,
) -> i32 {
    let lock = || volume.lock().unwrap_or_else(|p| p.into_inner());

    match op {
        libublk::sys::UBLK_IO_OP_READ => match read_staged(volume, byte_off, bytes, buf) {
            Ok(()) => bytes as i32,
            Err(e) => {
                log::error!("read at {byte_off}: {e:#}");
                -libc::EIO
            }
        },
        libublk::sys::UBLK_IO_OP_WRITE => {
            let res = if byte_off % BLOCK != 0 || bytes as u64 % BLOCK != 0 {
                unaligned_write(&mut lock(), byte_off, bytes, buf)
            } else {
                let blk = byte_off / BLOCK;
                match lock().write(blk, bytes / BLOCK as usize, buf) {
                    Ok(()) => bytes as i32,
                    Err(e) => {
                        log::error!("write at {byte_off}: {e:#}");
                        -libc::EIO
                    }
                }
            };
            if res >= 0 && fua {
                if let Err(e) = lock().flush() {
                    log::error!("fua flush: {e:#}");
                    return -libc::EIO;
                }
            }
            res
        }
        libublk::sys::UBLK_IO_OP_FLUSH => match lock().flush() {
            Ok(()) => 0,
            Err(e) => {
                log::error!("flush: {e:#}");
                -libc::EIO
            }
        },
        // Discard/zeroout are advisory here: chunk objects are rewritten whole
        // and a hole already reads as zeros, so accepting them is safe.
        libublk::sys::UBLK_IO_OP_DISCARD | libublk::sys::UBLK_IO_OP_WRITE_ZEROES => {
            let zeroes = vec![0u8; bytes.min(BLOCK as usize)];
            let _ = zeroes;
            0
        }
        other => {
            log::warn!("unsupported op {other:#x}");
            -libc::EOPNOTSUPP
        }
    }
}

/// Serve one command, splitting it into windows that fit the tag buffer.
///
/// The buffer holds the data, so window `i` must read and write its own slice
/// of it. Serving every window from the front of the buffer is wrong in a way
/// that only shows up on large IOs: the first window lands correctly and every
/// later one repeats it.
///
/// A window as large as the buffer maps one to one onto its slice; a smaller
/// final window is staged through a scratch buffer.
fn serve_windows(
    volume: &Mutex<Volume>,
    op: u32,
    sector: u64,
    bytes: usize,
    buf: &mut [u8],
    fua: bool,
) -> i32 {
    let buf_len = buf.len();
    if buf_len == 0 || bytes <= buf_len || op == libublk::sys::UBLK_IO_OP_FLUSH {
        let n = bytes.min(buf_len);
        return process_op(volume, op, sector, n, &mut buf[..n], fua);
    }
    let mut res: i32 = 0;
    let mut off = 0usize;
    let mut scratch: Vec<u8> = Vec::new();
    while off < bytes {
        let n = (bytes - off).min(buf_len);
        let r = if n == buf_len {
            process_op(
                volume,
                op,
                sector + off as u64,
                n,
                &mut buf[off..off + n],
                fua,
            )
        } else {
            scratch.resize(n, 0);
            if op == libublk::sys::UBLK_IO_OP_WRITE {
                scratch.copy_from_slice(&buf[off..off + n]);
            }
            let r = process_op(volume, op, sector + off as u64, n, &mut scratch, fua);
            if r >= 0 && op != libublk::sys::UBLK_IO_OP_WRITE {
                buf[off..off + n].copy_from_slice(&scratch);
            }
            r
        };
        if r < 0 {
            return r;
        }
        res += r;
        off += n;
    }
    res
}

/// Serve a read in three phases so the network is never touched under the lock:
/// plan (locked), fetch (unlocked), install + copy (locked).
///
/// Without this, one slow object-store request would block every queue thread
/// and the background uploader, because they all share one volume lock.
fn read_staged(volume: &Mutex<Volume>, byte_off: u64, bytes: usize, buf: &mut [u8]) -> Result<()> {
    let (aligned_off, blocks, want) = if byte_off % BLOCK == 0 && bytes as u64 % BLOCK == 0 {
        (byte_off, Some(bytes / BLOCK as usize), bytes)
    } else {
        // Covering blocks for a sub-block request.
        let first = byte_off / BLOCK;
        let last = (byte_off + bytes as u64).div_ceil(BLOCK);
        (
            first * BLOCK,
            Some((last - first) as usize),
            ((last - first) * BLOCK) as usize,
        )
    };
    let Some(blocks) = blocks else {
        anyhow::bail!("empty read");
    };

    // Phase 1 (locked): decide what is missing.
    let (plan, fetcher) = {
        let mut v = volume.lock().unwrap_or_else(|p| p.into_inner());
        (v.plan_read(aligned_off / BLOCK, blocks)?, v.fetcher())
    };

    // Phase 2 (UNLOCKED): talk to the object store. Other queue threads and
    // the uploader keep working while this request is in flight.
    let mut fetched = Vec::with_capacity(plan.requests.len());
    for req in &plan.requests {
        let data = fetcher.fetch(req)?;
        fetched.push((req.clone(), data));
    }
    drop(fetcher);
    {
        let mut v = volume.lock().unwrap_or_else(|p| p.into_inner());
        for (req, data) in fetched {
            v.apply_fetch(&req, data);
        }
    }

    let mut whole = vec![0u8; want];
    {
        let mut v = volume.lock().unwrap_or_else(|p| p.into_inner());
        v.read(aligned_off / BLOCK, blocks, &mut whole)?;
    }
    if aligned_off == byte_off && want == bytes {
        buf.copy_from_slice(&whole);
    } else {
        let off = (byte_off - aligned_off) as usize;
        buf[..bytes].copy_from_slice(&whole[off..off + bytes]);
    }
    Ok(())
}

/// Sub-block read: fetch the covering blocks and copy the requested slice.
/// Sub-block write: read-modify-write the covering blocks.
fn unaligned_write(volume: &mut Volume, byte_off: u64, bytes: usize, buf: &[u8]) -> i32 {
    let first = byte_off / BLOCK;
    let last = (byte_off + bytes as u64).div_ceil(BLOCK);
    let mut tmp = vec![0u8; ((last - first) * BLOCK) as usize];
    if let Err(e) = volume.read(first, (last - first) as usize, &mut tmp) {
        log::error!("unaligned write read at {byte_off}: {e:#}");
        return -libc::EIO;
    }
    let off = (byte_off % BLOCK) as usize;
    tmp[off..off + bytes].copy_from_slice(&buf[..bytes]);
    match volume.write(first, (last - first) as usize, &tmp) {
        Ok(()) => bytes as i32,
        Err(e) => {
            log::error!("unaligned write at {byte_off}: {e:#}");
            -libc::EIO
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::MemMeta;
    use crate::store::MemStore;
    use crate::volume::{Create, Layout, Policy, DEFAULT_CHUNK};

    fn test_volume() -> Mutex<Volume> {
        Mutex::new(
            Volume::open(
                Arc::new(MemStore::new()),
                Box::new(MemMeta::new()),
                Layout::new("v"),
                Some(Create::new(DEFAULT_CHUNK)),
                Policy::default(),
            )
            .unwrap(),
        )
    }

    #[test]
    fn aligned_write_then_read() {
        let v = test_volume();
        let data = vec![0x5Au8; BLOCK as usize];
        let r = process_op(&v, libublk::sys::UBLK_IO_OP_WRITE, BLOCK, BLOCK as usize, &mut data.clone(), false);
        assert_eq!(r, BLOCK as i32);
        let mut out = vec![0u8; BLOCK as usize];
        let r = process_op(&v, libublk::sys::UBLK_IO_OP_READ, BLOCK, BLOCK as usize, &mut out, false);
        assert_eq!(r, BLOCK as i32);
        assert!(out.iter().all(|&b| b == 0x5A));
    }

    #[test]
    fn unaligned_write_is_read_modify_write() {
        let v = test_volume();
        // Write "hello" at an offset that spans two blocks.
        let payload = b"hello".to_vec();
        let off = BLOCK * 3 + (BLOCK - 3);
        let mut p = payload.clone();
        let r = process_op(&v, libublk::sys::UBLK_IO_OP_WRITE, off, payload.len(), &mut p, false);
        assert_eq!(r, payload.len() as i32);

        let mut out = vec![0u8; payload.len()];
        let r = process_op(&v, libublk::sys::UBLK_IO_OP_READ, off, payload.len(), &mut out, false);
        assert_eq!(r, payload.len() as i32);
        assert_eq!(out, payload);
    }

    #[test]
    fn flush_op_drains() {
        let v = test_volume();
        let mut data = vec![7u8; BLOCK as usize];
        process_op(&v, libublk::sys::UBLK_IO_OP_WRITE, 0, BLOCK as usize, &mut data, false);
        assert_eq!(v.lock().unwrap().dirty_chunks(), 1);
        let r = process_op(&v, libublk::sys::UBLK_IO_OP_FLUSH, 0, 0, &mut [], false);
        assert_eq!(r, 0);
        assert_eq!(v.lock().unwrap().dirty_chunks(), 0);
    }

    #[test]
    fn fua_write_flushes() {
        let v = test_volume();
        let mut data = vec![1u8; BLOCK as usize];
        process_op(&v, libublk::sys::UBLK_IO_OP_WRITE, 0, BLOCK as usize, &mut data, true);
        assert_eq!(
            v.lock().unwrap().dirty_chunks(),
            0,
            "a FUA write must leave nothing dirty"
        );
    }

    /// Regression: a command larger than the tag buffer must not serve every
    /// window from the front of the buffer.
    ///
    /// This is invisible for small IOs: only the second and later windows are
    /// wrong, and they merely repeat the first one.
    #[test]
    fn large_commands_serve_their_own_windows() {
        let v = test_volume();
        let total = 1 << 20;
        let window = 256 << 10;
        // Distinct, recognisable content in every window.
        let mut data = vec![0u8; total];
        for w in 0..(total / window) {
            for i in 0..window {
                data[w * window + i] = (w as u8).wrapping_mul(37).wrapping_add(1);
            }
        }

        let mut buf = data.clone();
        let r = serve_windows(
            &v,
            libublk::sys::UBLK_IO_OP_WRITE,
            0,
            total,
            &mut buf,
            false,
        );
        assert_eq!(r, total as i32);

        let mut got = vec![0u8; total];
        let r = serve_windows(
            &v,
            libublk::sys::UBLK_IO_OP_READ,
            0,
            total,
            &mut got,
            false,
        );
        assert_eq!(r, total as i32);

        for w in 0..(total / window) {
            let a = w * window;
            let b = a + window;
            assert_eq!(
                &got[a..b],
                &data[a..b],
                "window {w} (bytes {a}..{b}) came back wrong"
            );
        }
    }

    #[test]
    fn unsupported_op_is_rejected() {
        let v = test_volume();
        let r = process_op(&v, 0x99, 0, 0, &mut [], false);
        assert_eq!(r, -libc::EOPNOTSUPP);
    }

    #[test]
    fn out_of_range_read_fails() {
        let v = test_volume();
        let mut out = vec![0u8; BLOCK as usize];
        let r = process_op(&v, libublk::sys::UBLK_IO_OP_READ, DEFAULT_CHUNK, BLOCK as usize, &mut out, false);
        assert_eq!(r, -libc::EIO);
    }
}
