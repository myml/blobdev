#!/bin/bash
# The object store goes away while a filesystem is mounted, then comes back.
#
# What this checks: the device keeps answering while the store is down (no
# hangs), reports EIO to the block layer, keeps the data it already holds, and
# works again once the store returns.
#
# What it deliberately does not assert: that `sync(2)` returns an error. It
# does not -- the block layer records the write error and reports it on the
# next fsync, which is how every block device behaves. The fsync case is the
# one worth checking, and it does fail as it should.
#
# Needs scripts/vhost_s3_server.py on the target host.
set +e
BIN=/usr/local/bin/blobdev
pkill -9 -f vhost_s3_server.py; pkill -9 -x blobdev; sleep 2; umount /mnt/ft 2>/dev/null
rm -rf /tmp/ftroot /tmp/ftw; mkdir -p /tmp/ftroot/blobtest /tmp/ftw

start_server() {
  setsid python3 /tmp/vhost_s3_server.py 18095 /tmp/ftroot .127.0.0.1 </dev/null >/dev/null 2>&1 &
  sleep 1
}
start_server

S3="--backend s3 --endpoint http://127.0.0.1:18095 --bucket blobtest --access-key k --secret-key s --virtual-host-style --retries 2 --timeout 5"
DEV=""
start_dev() {
  $BIN run $S3 --volume ft --meta /tmp/ftw/v.meta --id -1 "$@" > /tmp/ftw/log 2>&1 &
  DEV=""
  for i in $(seq 1 60); do DEV=$(grep -oE "/dev/ublkb[0-9]+" /tmp/ftw/log | head -1); [ -n "$DEV" ] && break; sleep 0.5; done
  [ -n "$DEV" ]
}
start_dev --create-size 256M || { echo "FAIL: no device"; tail -5 /tmp/ftw/log; exit 1; }
mkdir -p /mnt/ft
mkfs.ext4 -q -F $DEV || { echo "FAIL mkfs"; exit 1; }
mount $DEV /mnt/ft || { echo "FAIL mount"; exit 1; }
dd if=/dev/urandom of=/mnt/ft/keep.bin bs=1M count=16 status=none
sync
KEEP=$(sha256sum /mnt/ft/keep.bin | awk '{print $1}')
echo "baseline written: ${KEEP:0:12}"

echo "== kill the object store"
pkill -9 -f vhost_s3_server.py; sleep 1

echo "-- read while the store is down (data is in page cache, expect ok)"
timeout 20 dd if=/mnt/ft/keep.bin of=/dev/null bs=1M 2>&1 | tail -1
echo "   rc=$?"

echo "-- write while down (page cache absorbs it)"
timeout 20 dd if=/dev/urandom of=/mnt/ft/down.bin bs=1M count=4 status=none 2>&1 | tail -1
echo "   rc=$?"

echo "-- fsync while down: must report the failure"
python3 - <<'PY'
import os
try:
    fd = os.open('/mnt/ft/down.bin', os.O_WRONLY)
    os.fsync(fd)
    print("   fsync: ok (unexpected)")
except OSError as e:
    print(f"   fsync: {e} (expected)")
PY

echo "== bring the store back"
start_server
sleep 1
echo "-- fsync again should now succeed"
python3 - <<'PY'
import os
try:
    fd = os.open('/mnt/ft/down.bin', os.O_WRONLY)
    os.fsync(fd)
    print("   fsync: ok")
except OSError as e:
    print(f"   fsync: {e} (unexpected)")
PY

echo "-- earlier data must still match"
GOT=$(sha256sum /mnt/ft/keep.bin | awk '{print $1}')
[ "$GOT" = "$KEEP" ] && echo "   data intact" || echo "   FAIL: data changed"

umount /mnt/ft 2>/dev/null
pkill -TERM -x blobdev; sleep 3
echo "== restart and verify from the store"
start_dev "" || { echo "FAIL: no device on restart"; tail -5 /tmp/ftw/log; exit 1; }
mount -o ro $DEV /mnt/ft || { echo "FAIL remount"; exit 1; }
GOT2=$(sha256sum /mnt/ft/keep.bin | awk '{print $1}')
umount /mnt/ft 2>/dev/null
[ "$GOT2" = "$KEEP" ] && echo "PASS: volume survived an outage and restarted cleanly" || echo "FAIL: data differs after restart"
pkill -9 -x blobdev; pkill -9 -f vhost_s3_server.py
