# Installing blobdev

## Requirements

- Linux with `ublk_drv` (5.19+, device creation is stable from 6.8)
- root, or `CAP_SYS_ADMIN`
- a local disk for the chunk cache (optional but strongly recommended)

```bash
modprobe ublk_drv
ls /dev/ublk-control          # must exist
```

To load it at boot:

```bash
echo ublk_drv > /etc/modules-load.d/blobdev.conf
```

## Build and install

```bash
cargo build --release
install -m 0755 target/release/blobdev /usr/local/bin/blobdev
install -d /etc/blobdev /var/lib/blobdev /var/cache/blobdev
install -m 0644 packaging/blobdev@.service /etc/systemd/system/
```

## Configure a volume

```bash
cp packaging/blobdev.conf.example /etc/blobdev/data.conf
$EDITOR /etc/blobdev/data.conf          # set the backend and credentials
chown root:root /etc/blobdev/data.conf
chmod 0600 /etc/blobdev/data.conf       # it holds the secret key
```

The first start creates the device but not the volume: add `--create-size 100G`
the first time only, then remove it. Afterwards the geometry comes from the
metadata file.

## Start

```bash
systemctl daemon-reload
systemctl enable --now blobdev@data
journalctl -u blobdev@data -f
```

The device appears as `/dev/ublkbN`; the number is in the log.

```bash
mkfs.ext4 /dev/ublkbN
mount /dev/ublkbN /srv/data
```

## Operations

| Task | Command |
|---|---|
| Check the metadata against the object store | `blobdev inspect --volume data ...` |
| List chunk objects | `blobdev ls --volume data ...` |
| Reclaim objects a crash orphaned | `blobdev gc --volume data ...` (add `--dry-run` first) |
| See what the store holds | `blobdev status` |

`gc` and `inspect` take the same backend flags as `run`; they are safe to run
while the daemon is idle, and they refuse to act while chunks are still
waiting to be uploaded.

## Removing

```bash
systemctl stop blobdev@data     # uploads every dirty chunk before exiting
umount /srv/data
```

The volume's data stays in the bucket; the metadata file is the only local
state worth keeping.

## Multiple volumes

One process per volume, each locked by its own `.lock` file next to the
metadata. Two processes on one volume refuse to start rather than corrupt it.
