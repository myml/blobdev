# Testing on a shared host

The verification host (`192.168.0.220`) is not a scratch machine. It serves
NFS exports and other services. Every script here runs as root, so a careless
wildcard can take something down.

## Mount points

Never loop over mount points. Only these belong to blobdev tests, and only
these may ever be unmounted by them:

```
/mnt/blobdev        e2e-dir.sh, e2e-webdav.sh, e2e-s3.sh
/mnt/blobdev-stress stress.sh
/mnt/blobdev-crash  crash-test.sh
/mnt/rc             restart-cycles.sh
/mnt/ft /mnt/fs /mnt/rc2 /mnt/vh /mnt/ds    fault-injection.sh and manual runs
```

`/mnt/9kg` and `/mnt/mergerfs` are NFS exports from another host. They are not
test directories. A `for m in $(grep -oE "/mnt/..." /proc/mounts); do umount $m;
done` loop once unmounted both. The data was intact and `mount /mnt/9kg`
restored them, because they come from `/etc/fstab`, but it should not have
happened: unmount them only if you know what they are.

## ublk devices

A `kill -9` leaves the device behind; the kernel keeps it until someone asks
for its removal. They accumulate and consume device ids.

```bash
blobdev status          # which devices have a daemon, which are stale
blobdev reap            # remove the stale ones (skips anything still mounted)
```

`reap` only touches devices whose block node is gone, so a live volume is not
at risk. Clean up after a test run rather than letting ids pile up: the supply
is finite and a test that cannot allocate one looks like a blobdev bug.

## Backends

- S3: RustFS in podman on port 9000, bucket `blobtest`, path-style.
- S3 virtual-host: `scripts/vhost_s3_server.py` on port 18090, which resolves
  the bucket from the Host header. RustFS does not, so it cannot serve this
  test.
- WebDAV: the `davtest` systemd unit on port 18080. Port 8080 belongs to
  qbittorrent.

Do not reuse a volume name across runs. An old metadata file with the same
name points the new run at objects it did not write, and the failure looks
like a data-corruption bug.
