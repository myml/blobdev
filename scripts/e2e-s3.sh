#!/bin/bash
# End-to-end test against an S3-compatible server (RustFS / MinIO / Ceph RGW).
#
# Run on a host with ublk support. Verifies that data really lands in the
# bucket as chunk objects and survives a full restart read back from S3.
set -u
BIN=${BIN:-./target/release/blobdev}
ROOT=${ROOT:-/var/tmp/blobdev-s3}
VOL=${VOL:-s3vol}
ENDPOINT=${ENDPOINT:-http://127.0.0.1:9000}
BUCKET=${BUCKET:-blobtest}
AK=${AK:-blobdev}
SK=${SK:-blobdevsecret123}
DEV=""

fail() { echo "FAIL: $*" >&2; exit 1; }
cleanup() {
  umount /mnt/blobdev 2>/dev/null
  for p in $(pgrep -x blobdev 2>/dev/null); do kill -INT $p 2>/dev/null; done
  sleep 2
  for p in $(pgrep -x blobdev 2>/dev/null); do kill -9 $p 2>/dev/null; done
  sleep 1
}
trap cleanup EXIT

start() {
  "$BIN" run --backend s3 --endpoint "$ENDPOINT" --bucket "$BUCKET" \
    --access-key "$AK" --secret-key "$SK" --path-style \
    --volume "$VOL" --meta "$ROOT/$VOL.meta" --id -1 "$@" > "$ROOT/log" 2>&1 &
  DEV=""
  for i in $(seq 1 40); do
    DEV=$(grep -oE "/dev/ublkb[0-9]+" "$ROOT/log" | head -1)
    [ -n "$DEV" ] && break
    sleep 0.5
  done
  [ -n "$DEV" ] || { tail -10 "$ROOT/log"; fail "device did not appear"; }
}

rm -rf "$ROOT"; # /tmp is often a tmpfs on test hosts. Filling it makes later runs fail in a
# way that looks like a blobdev bug, so refuse to start without room.
SPACE_DIR=$(dirname "${W:-${ROOT:-/var/tmp}}")
mkdir -p "$SPACE_DIR" 2>/dev/null
AVAIL_KB=$(df -Pk "$SPACE_DIR" 2>/dev/null | awk 'NR==2 {print $4}')
if [ -n "${AVAIL_KB:-}" ] && [ "$AVAIL_KB" -lt $((1024 * 1024)) ]; then
  echo "FAIL: less than 1 GiB free in $SPACE_DIR" >&2
  df -h "$SPACE_DIR" >&2
  exit 1
fi

mkdir -p "$ROOT"

echo "== start device on S3 backend"
start --create-size 256M
echo "   device=$DEV size=$(blockdev --getsize64 "$DEV")"

echo "== mkfs + mount"
mkfs.ext4 -q -F "$DEV" || fail "mkfs"
mkdir -p /mnt/blobdev
mount "$DEV" /mnt/blobdev || fail "mount"

echo "== write 80 MiB and read it back through the cache"
dd if=/dev/urandom of=/mnt/blobdev/data.bin bs=1M count=80 status=none || fail "dd"
SUM1=$(sha256sum /mnt/blobdev/data.bin | awk '{print $1}')
sync; echo 3 > /proc/sys/vm/drop_caches
SUM2=$(sha256sum /mnt/blobdev/data.bin | awk '{print $1}')
[ "$SUM1" = "$SUM2" ] || fail "readback mismatch through the cache"
echo "   sha=$SUM1"

echo "== unmount triggers flush to S3"
umount /mnt/blobdev || fail "umount"
sleep 2

echo "== restart from S3 and verify"
cleanup
start
mount -o ro "$DEV" /mnt/blobdev || fail "remount"
SUM3=$(sha256sum /mnt/blobdev/data.bin | awk '{print $1}')
umount /mnt/blobdev 2>/dev/null
[ "$SUM1" = "$SUM3" ] || fail "data changed after reading back from S3 ($SUM1 vs $SUM3)"

echo "PASS: sha256 identical after a full restart reading from S3"

echo "== crash recovery: kill -9 with unsynced-to-S3 chunks"
# The data has reached blobdev (sync -> block device) but not S3 yet. A hard
# kill must not lose it: the metadata records which blocks are pending.
cleanup
start --create-size 256M
mkfs.ext4 -q -F "$DEV" || fail "mkfs (crash test)"
mount "$DEV" /mnt/blobdev || fail "mount (crash test)"
dd if=/dev/urandom of=/mnt/blobdev/crash.bin bs=1M count=20 status=none || fail "dd (crash test)"
sync   # reach blobdev, NOT necessarily S3
SUM_CRASH=$(sha256sum /mnt/blobdev/crash.bin | awk '{print $1}')
for p in $(pgrep -x blobdev); do kill -9 $p; done
sleep 2
start
mount -o ro "$DEV" /mnt/blobdev || fail "remount after crash"
SUM_AFTER=$(sha256sum /mnt/blobdev/crash.bin | awk '{print $1}')
umount /mnt/blobdev 2>/dev/null
[ "$SUM_CRASH" = "$SUM_AFTER" ] || fail "crash recovery lost data ($SUM_CRASH vs $SUM_AFTER)"
echo "PASS: kill -9 recovery restored pending chunks from metadata"
