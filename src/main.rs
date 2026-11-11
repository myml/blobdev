//! blobdev: serve objects from S3, WebDAV or a directory as a block device,
//! with a local cache in front.
//!
//! ```text
//! blobdev run --backend s3 --endpoint http://127.0.0.1:9000 --bucket blob \
//!             --volume vol1 --create-size 1G [--id N]
//! blobdev stop  --id N
//! blobdev status --id N
//! ```

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use blobdev::meta::FileMeta;
use blobdev::s3::{S3Config, S3Store};
use blobdev::store::{DirStore, ObjectStore, Retry};
use blobdev::webdav::{WebDavConfig, WebDavStore};
use blobdev::volume::{Create, DiskCache, Layout, Policy, Volume};

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn usage() -> ! {
    eprintln!(
        "usage:
  blobdev run --volume NAME --create-size SIZE [options]
  blobdev stop --id N
  blobdev status --id N

backends:
  --backend dir     --dir PATH
  --backend s3      --endpoint URL --bucket NAME --access-key K --secret-key S
                    [--region R] [--path-style] [--key-prefix P]
  --backend webdav  --endpoint URL [--user U] [--password P] [--key-prefix P]

options:
  --meta PATH        metadata file (default: ./<volume>.meta)
  --chunk-mib N      chunk size in MiB (default 8)
  --create-size S    create the volume, e.g. 1G, 500M (only when new)
  --id N             ublk device id (-1 = allocate)
  --queues N         queue count (default 1)
  --depth N          queue depth (default 64)
  --full-pct N       upload a chunk when N% of its blocks are dirty (default 50)
  --max-age S        upload a chunk dirty for S seconds (default 30)
  --read-chunks N    clean chunks kept in memory (default 64)
  --whole-chunk-pct N  fetch a whole chunk for reads covering N%% of it (default 25)
  --cache-dir PATH   keep clean chunks on local disk; enables read caching
  --cache-size SIZE  disk read cache budget, e.g. 20G (default 64M, 0 disables)
  --retries N        attempts per request on transient failures (default 4)"
    );
    std::process::exit(2)
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// Parse sizes like `1G`, `500M`, `4096`.
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1 << 10),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1 << 20),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1 << 30),
        Some('T') | Some('t') => (&s[..s.len() - 1], 1u64 << 40),
        _ => (s, 1),
    };
    let n: u64 = num.parse().with_context(|| format!("bad size {s}"))?;
    Ok(n * mult)
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_target(false)
        .format_timestamp_secs()
        .init();
    if let Err(e) = real_main() {
        eprintln!("blobdev: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else { usage() };
    match cmd.as_str() {
        "run" => run(&args),
        "stop" => stop(&args),
        "status" => status(&args),
        _ => usage(),
    }
}

/// A sensible read cache budget when `--cache-size` is not given: a slice of
/// the volume size, so a small volume is cached whole and a large one keeps a
/// working fraction. Never smaller than 64 MiB.
fn default_cache_budget() -> u64 {
    const MIN: u64 = 64 << 20;
    MIN
}

fn build_store(args: &[String]) -> Result<Arc<dyn ObjectStore>> {
    let backend = arg(args, "--backend").unwrap_or_else(|| "dir".to_string());
    match backend.as_str() {
        "dir" => {
            let dir = arg(args, "--dir").context("--dir is required for the dir backend")?;
            Ok(Arc::new(DirStore::new(dir)?))
        }
        "s3" => {
            let endpoint = arg(args, "--endpoint").context("--endpoint is required for s3")?;
            let bucket = arg(args, "--bucket").context("--bucket is required for s3")?;
            let access = arg(args, "--access-key")
                .or_else(|| std::env::var("AWS_ACCESS_KEY_ID").ok())
                .context("--access-key (or AWS_ACCESS_KEY_ID) is required for s3")?;
            let secret = arg(args, "--secret-key")
                .or_else(|| std::env::var("AWS_SECRET_ACCESS_KEY").ok())
                .context("--secret-key (or AWS_SECRET_ACCESS_KEY) is required for s3")?;
            let region = arg(args, "--region").unwrap_or_else(|| "us-east-1".to_string());
            let mut cfg = S3Config::new(endpoint, region, bucket, access, secret);
            if let Some(p) = arg(args, "--key-prefix") {
                cfg = cfg.with_prefix(p);
            }
            // MinIO/RustFS and most self-hosted endpoints need path-style.
            if flag(args, "--path-style") {
                cfg = cfg.with_path_style(true);
            }
            // Session tokens are for temporary credentials; wire them once the
            // backend exposes a setter.
            let mut store = S3Store::new(cfg)?;
            if let Some(n) = arg(args, "--retries") {
                let attempts: u32 = n.parse().context("bad --retries")?;
                store = store.with_retry(Retry {
                    attempts: attempts.max(1),
                    ..Retry::default()
                });
            }
            store.check_bucket()?;
            Ok(Arc::new(store))
        }
        "webdav" => {
            let endpoint = arg(args, "--endpoint").context("--endpoint is required for webdav")?;
            let mut cfg = WebDavConfig::new(endpoint);
            if let (Some(u), Some(pw)) = (arg(args, "--user"), arg(args, "--password")) {
                cfg = cfg.with_auth(u, pw);
            }
            if let Some(p) = arg(args, "--key-prefix") {
                cfg = cfg.with_prefix(p);
            }
            let mut store = WebDavStore::new(cfg)?;
            if let Some(n) = arg(args, "--retries") {
                let attempts: u32 = n.parse().context("bad --retries")?;
                store = store.with_retry(Retry {
                    attempts: attempts.max(1),
                    ..Retry::default()
                });
            }
            Ok(Arc::new(store))
        }
        other => anyhow::bail!("unknown backend {other}"),
    }
}

fn run(args: &[String]) -> Result<()> {
    let backend = arg(args, "--backend").unwrap_or_else(|| "dir".to_string());
    if !matches!(backend.as_str(), "dir" | "s3" | "webdav") {
        anyhow::bail!("unknown backend {backend} (expected dir, s3 or webdav)");
    }
    let volume_name = arg(args, "--volume").context("--volume is required")?;
    let create = match arg(args, "--create-size") {
        Some(s) => {
            let mut c = Create::new(parse_size(&s)?);
            if let Some(mib) = arg(args, "--chunk-mib") {
                let mib: u64 = mib.parse().context("bad --chunk-mib")?;
                if mib == 0 {
                    anyhow::bail!("--chunk-mib must be at least 1");
                }
                c = c.with_chunk_size(mib << 20);
            }
            Some(c)
        }
        None => None,
    };
    let meta_path = arg(args, "--meta")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("{volume_name}.meta")));
    let policy = Policy {
        full_pct: arg(args, "--full-pct")
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(50),
        max_age: std::time::Duration::from_secs(
            arg(args, "--max-age")
                .map(|s| s.parse())
                .transpose()?
                .unwrap_or(30),
        ),
        read_chunks: arg(args, "--read-chunks")
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(64),
        whole_chunk_pct: arg(args, "--whole-chunk-pct")
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(25),
    };

    // Data written through the device is already on local disk on its way out;
    // keeping it is what makes a restart start warm. So a cache directory also
    // implies a cache, and the budget defaults rather than being mandatory.
    let disk_cache = match (arg(args, "--cache-dir"), arg(args, "--cache-size")) {
        (Some(dir), Some(sz)) => {
            let bytes = parse_size(&sz)?;
            if bytes == 0 {
                None
            } else {
                Some(DiskCache::new(dir, bytes))
            }
        }
        (Some(dir), None) => {
            let budget = default_cache_budget();
            log::info!(
                "read cache: {} with the default budget of {} MiB (set --cache-size)",
                dir,
                budget >> 20
            );
            Some(DiskCache::new(dir, budget))
        }
        (None, Some(_)) => anyhow::bail!("--cache-size needs --cache-dir"),
        (None, None) => None,
    };

    let store = build_store(args)?;
    let volume = Volume::open_with_cache(
        store,
        Box::new(FileMeta::new(&meta_path)),
        Layout::new(&volume_name),
        create,
        policy,
        disk_cache,
    )?;

    // Block signals before any thread starts, so every thread inherits the mask
    // and only the waiter below receives them.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
    let drain = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    {
        let drain = drain.clone();
        std::thread::spawn(move || loop {
            let mut sig: libc::c_int = 0;
            let rc = unsafe { libc::sigwait(&set, &mut sig) };
            if rc == 0 {
                log::info!("signal {sig}: flushing");
                drain.store(true, Ordering::SeqCst);
            }
        });
    }

    let target = blobdev::ublk_tgt::Target {
        id: arg(args, "--id")
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(-1),
        queues: arg(args, "--queues")
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(1),
        depth: arg(args, "--depth")
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(64),
        interval: std::time::Duration::from_secs(1),
    };
    target.run(volume, drain, stop)
}

fn stop(args: &[String]) -> Result<()> {
    let id: i32 = arg(args, "--id")
        .context("--id is required")?
        .parse()
        .context("bad --id")?;
    let ctrl = libublk::ctrl::UblkCtrl::new_simple(id)
        .with_context(|| format!("open ublk device {id}"))?;
    ctrl.del_dev_async()
        .with_context(|| format!("stop device {id}"))?;
    log::info!("device {id} removed");
    Ok(())
}

fn status(_args: &[String]) -> Result<()> {
    let dir = std::path::Path::new("/sys/class/ublk-char");
    if !dir.exists() {
        println!("ublk driver not loaded");
        return Ok(());
    }
    let mut found = false;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name();
        let dev = std::fs::read_to_string(e.path().join("dev_id"))
            .unwrap_or_default()
            .trim()
            .to_string();
        println!("{} dev_id={dev}", name.to_string_lossy());
        found = true;
    }
    if !found {
        println!("no ublk devices");
    }
    Ok(())
}
