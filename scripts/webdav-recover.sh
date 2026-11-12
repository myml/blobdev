#!/bin/bash
# Can a WebDAV volume survive losing its metadata disk?
#
# It could not, until the gate on the metadata copy was fixed. Measured
# before: metadata=file, delete the file, "volume has no metadata", no
# recovery. After: metadata=file+store, recovered from the store, data
# identical.
#
# It should: WebDAV has plain PUT and GET, which is all a metadata copy needs.
# The copy is gated on conditional_write, which is about optimistic locking
# and has nothing to do with keeping a backup.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/dr; MNT=/mnt/dr
DAV=http://127.0.0.1:18083/remote.php/dav/files/ncadmin/blobdev
pkill -9 -x blobdev >/dev/null 2>&1; sleep 3
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
V=dr$RANDOM

$BIN run --backend webdav --endpoint $DAV --user ncadmin --password ncpass12345 \
  --timeout 20 --volume $V --meta $W/v.meta --id 108 --create-size 512M --max-age 2 > $W/l 2>&1 &
for _ in $(seq 1 180); do grep -q "created /dev/ublkb108" $W/l 2>/dev/null && break; sleep 1; done
grep -q "created /dev/ublkb108" $W/l || { echo "FAIL: no device"; exit 1; }
echo "  metadata backend: $(grep -oE 'metadata=[a-z+]+' $W/l | head -1)"
mkfs.ext4 -q -F /dev/ublkb108 2>/dev/null
for _ in $(seq 1 90); do mount /dev/ublkb108 $MNT 2>/dev/null && break; sleep 1; done
head -c 33554432 /dev/urandom > $MNT/a.bin
sync; sleep 6
WANT=$(sha256sum $MNT/a.bin | awk '{print $1}')
umount $MNT
$BIN stop --id 108 >/dev/null 2>&1
for _ in $(seq 1 120); do [ "$(ls /sys/class/ublk-char 2>/dev/null|wc -l)" -eq 0 ] && break; sleep 0.5; done
sleep 2

echo "== delete the local metadata, as if that disk died"
rm -f $W/v.meta $W/v.meta-behind
$BIN run --backend webdav --endpoint $DAV --user ncadmin --password ncpass12345 \
  --timeout 20 --volume $V --meta $W/v.meta --id 108 > $W/l2 2>&1 &
for _ in $(seq 1 180); do grep -q "created /dev/ublkb108" $W/l2 2>/dev/null && break; sleep 1; done
if grep -q "created /dev/ublkb108" $W/l2; then
  echo "  recovered: yes"
  for _ in $(seq 1 90); do mount -o ro /dev/ublkb108 $MNT 2>/dev/null && break; sleep 1; done
  GOT=$(sha256sum $MNT/a.bin 2>/dev/null | awk '{print $1}')
  echo "  data: $([ "$WANT" = "$GOT" ] && echo identical || echo DIFFERS)"
  umount $MNT 2>/dev/null
else
  echo "  recovered: no"
  grep -vE "^\[.*INFO" $W/l2 | head -2 | cut -c1-100
fi
pkill -9 -x blobdev 2>/dev/null
