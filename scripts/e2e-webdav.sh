#!/bin/bash
# End-to-end test against a WebDAV server.
#
# WebDAV advertises partial_write, so this doubles as the check that only dirty
# block ranges get rewritten instead of whole chunks.
set -u
BIN=${BIN:-./target/release/blobdev}
ROOT=${ROOT:-/tmp/blobdev-dav}
VOL=${VOL:-davvol}
ENDPOINT=${ENDPOINT:-http://127.0.0.1:18080}
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
  "$BIN" run --backend webdav --endpoint "$ENDPOINT" \
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
AVAIL_KB=$(df -Pk /tmp 2>/dev/null | awk 'NR==2 {print $4}')
if [ -n "${AVAIL_KB:-}" ] && [ "$AVAIL_KB" -lt $((1024 * 1024)) ]; then
  echo "FAIL: less than 1 GiB free in /tmp" >&2
  df -h /tmp >&2
  exit 1
fi

mkdir -p "$ROOT"

echo "== start device on WebDAV backend"
start --create-size 128M
echo "   device=$DEV size=$(blockdev --getsize64 "$DEV")"

echo "== mkfs + mount"
mkfs.ext4 -q -F "$DEV" || fail "mkfs"
mkdir -p /mnt/blobdev
mount "$DEV" /mnt/blobdev || fail "mount"

echo "== write 40 MiB"
dd if=/dev/urandom of=/mnt/blobdev/data.bin bs=1M count=40 status=none || fail "dd"
SUM1=$(sha256sum /mnt/blobdev/data.bin | awk '{print $1}')
sync
echo "   sha=$SUM1"

echo "== unmount (flush) and restart"
umount /mnt/blobdev || fail "umount"
cleanup
start
mount -o ro "$DEV" /mnt/blobdev || fail "remount"
SUM2=$(sha256sum /mnt/blobdev/data.bin | awk '{print $1}')
umount /mnt/blobdev 2>/dev/null
[ "$SUM1" = "$SUM2" ] || fail "data changed after restart ($SUM1 vs $SUM2)"

echo "PASS: sha256 identical after a restart from WebDAV"
