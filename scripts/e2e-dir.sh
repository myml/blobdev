#!/bin/bash
# End-to-end test against the local `dir` backend (no network needed).
#
# Covers the path a real deployment uses: a ublk block device backed by chunk
# objects, a filesystem on top, and the object store verified after detaching.
set -u
BIN=${BIN:-./target/release/blobdev}
ROOT=${ROOT:-/tmp/blobdev-e2e}
VOL=vol1
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

# /tmp is often a tmpfs on test hosts. Filling it makes later runs fail in a
# way that looks like a blobdev bug, so refuse to start without room.
AVAIL_KB=$(df -Pk /tmp 2>/dev/null | awk 'NR==2 {print $4}')
if [ -n "${AVAIL_KB:-}" ] && [ "$AVAIL_KB" -lt $((1024 * 1024)) ]; then
  echo "FAIL: less than 1 GiB free in /tmp" >&2
  df -h /tmp >&2
  exit 1
fi

rm -rf "$ROOT"; mkdir -p "$ROOT/objects" "$ROOT/cache"

echo "== start the device"
"$BIN" run --backend dir --dir "$ROOT/objects" --volume $VOL \
  --create-size 512M --meta "$ROOT/cache/$VOL.meta" --id -1 \
  > "$ROOT/log" 2>&1 &
for i in $(seq 1 40); do
  DEV=$(grep -oE "/dev/ublkb[0-9]+" "$ROOT/log" | head -1)
  [ -n "$DEV" ] && break
  sleep 0.5
done
[ -n "$DEV" ] || { tail -5 "$ROOT/log"; fail "device did not appear"; }
echo "   device=$DEV size=$(blockdev --getsize64 "$DEV")"
[ "$(blockdev --getsize64 "$DEV")" = 536870912 ] || fail "wrong device size"

echo "== mkfs + mount"
mkfs.ext4 -q -F "$DEV" || fail "mkfs"
mkdir -p /mnt/blobdev
mount "$DEV" /mnt/blobdev || fail "mount"

echo "== write 120 MiB through the cache"
dd if=/dev/urandom of=/mnt/blobdev/data.bin bs=1M count=120 status=none || fail "dd"
SUM1=$(sha256sum /mnt/blobdev/data.bin | awk '{print $1}')
sync; echo 3 > /proc/sys/vm/drop_caches
SUM2=$(sha256sum /mnt/blobdev/data.bin | awk '{print $1}')
[ "$SUM1" = "$SUM2" ] || fail "readback mismatch through the cache"
echo "   sha=$SUM1"

echo "== unmount (flush) and inspect the object store"
umount /mnt/blobdev || fail "umount"
sleep 1
CHUNKS=$(ls "$ROOT/objects/$VOL/c" 2>/dev/null | wc -l)
echo "   chunk objects after flush: $CHUNKS"
[ "$CHUNKS" -gt 0 ] || fail "no chunk objects uploaded by the flush"

cleanup

echo "== restart from the object store and verify"
"$BIN" run --backend dir --dir "$ROOT/objects" --volume $VOL \
  --meta "$ROOT/cache/$VOL.meta" --id -1 > "$ROOT/log2" 2>&1 &
DEV=""
for i in $(seq 1 40); do
  DEV=$(grep -oE "/dev/ublkb[0-9]+" "$ROOT/log2" | head -1)
  [ -n "$DEV" ] && break
  sleep 0.5
done
[ -n "$DEV" ] || { tail -5 "$ROOT/log2"; fail "device did not reappear"; }
mount -o ro "$DEV" /mnt/blobdev || fail "remount"
SUM3=$(sha256sum /mnt/blobdev/data.bin | awk '{print $1}')
umount /mnt/blobdev 2>/dev/null
[ "$SUM1" = "$SUM3" ] || fail "data changed after reopen ($SUM1 vs $SUM3)"

echo "PASS: sha256 identical across a full restart from the object store"
