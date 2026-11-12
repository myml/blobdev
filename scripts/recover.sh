#!/bin/bash
# The disk holding the metadata is gone. Can the volume be read again?
#
# This is what an object-store backend is for: the objects outlive the
# machine. It had never been tested, because every other test kept the
# metadata next to the daemon. Measured before the copy existed: deleting
# the metadata file left a volume that refused to open, and its advice --
# "pass a size to create it" -- built an empty volume on top of objects
# that were still in the store.
#
# Writes a file, records its checksum, deletes the metadata, opens the
# volume from nothing but the store, and compares.
#
# usage: recover.sh
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/rec2; MNT=/mnt/rec2
S3=(--backend s3 --endpoint http://192.168.0.220:9000 --bucket blobtest \
  --access-key blobdev --secret-key blobdevsecret123 --region us-east-1 --path-style)

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
V=rc$RANDOM

$BIN run "${S3[@]}" --volume $V --meta $W/v.meta --id 61 --create-size 1G > $W/l1 2>&1 &
for _ in $(seq 1 120); do [ -e /dev/ublkb61 ] && break; sleep 1; done
mkfs.ext4 -q -F /dev/ublkb61 2>/dev/null
for _ in $(seq 1 120); do mount /dev/ublkb61 $MNT 2>/dev/null && break; sleep 1; done
head -c 33554432 /dev/urandom > $MNT/f.bin
sync
WANT=$(sha256sum $MNT/f.bin | awk '{print $1}')
echo "WANT=$WANT"
umount $MNT
pkill -TERM -x blobdev
for _ in $(seq 1 900); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
pkill -9 -x blobdev 2>/dev/null
for _ in $(seq 1 60); do [ "$(ls /sys/class/ublk-char 2>/dev/null|wc -l)" -eq 0 ] && break; sleep 1; done
$BIN reap >/dev/null 2>&1

echo "== delete the local metadata, as if this disk died"
rm -f $W/v.meta

echo "== open using only what the store holds"
$BIN run "${S3[@]}" --volume $V --meta $W/v.meta --id 61 > $W/l2 2>&1 &
for _ in $(seq 1 180); do [ -e /dev/ublkb61 ] && break; sleep 1; done
[ -e /dev/ublkb61 ] || { echo "FAIL: no device"; grep -v INFO $W/l2 | head -3; exit 1; }
grep -oE "recovered it from [^ ]*" $W/l2 | head -1
echo "RECOVERED_META_LOCAL=$(stat -c %s $W/v.meta 2>/dev/null || echo 0)"

for _ in $(seq 1 120); do mount -o ro /dev/ublkb61 $MNT 2>/dev/null && break; sleep 1; done
if mountpoint -q $MNT; then
  GOT=$(sha256sum $MNT/f.bin 2>/dev/null | awk '{print $1}')
  echo "GOT=$GOT"
  if [ "$WANT" = "$GOT" ]; then echo "DATA_IDENTICAL=yes"; else echo "DATA_IDENTICAL=no"; fi
  umount $MNT 2>/dev/null
else
  echo "DATA_IDENTICAL=no (mount failed)"
fi
pkill -9 -x blobdev 2>/dev/null
