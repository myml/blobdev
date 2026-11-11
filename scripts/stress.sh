#!/bin/bash
# Long-running stress: keep writing while verifying data, then restart and
# verify again from the object store.
#
# usage: stress.sh  (env: BIN, BACKEND_ARGS, ROOT, VOL, SIZE, MB, SECONDS)
set -u
BIN=${BIN:-./target/release/blobdev}
ROOT=${ROOT:-/tmp/blobdev-stress}
VOL=${VOL:-stress}
SIZE=${SIZE:-1G}
MB=${MB:-200}
SECONDS_TO_RUN=${SECONDS_TO_RUN:-120}
DEV=""

fail() { echo "FAIL: $*" >&2; exit 1; }
cleanup() {
  umount /mnt/blobdev-stress 2>/dev/null
  pkill -INT -x "$(basename "$BIN")" 2>/dev/null
  sleep 2
  pkill -9 -x "$(basename "$BIN")" 2>/dev/null
  sleep 1
}
trap cleanup EXIT

start() {
  # shellcheck disable=SC2086
  "$BIN" run $BACKEND_ARGS --volume "$VOL" --meta "$ROOT/$VOL.meta" --id -1 "$@" \
    > "$ROOT/log" 2>&1 &
  DEV=""
  for _ in $(seq 1 60); do
    DEV=$(grep -oE "/dev/ublkb[0-9]+" "$ROOT/log" | head -1)
    [ -n "$DEV" ] && break
    sleep 0.5
  done
  [ -n "$DEV" ] || { tail -5 "$ROOT/log"; fail "device did not appear"; }
}

mkdir -p "$ROOT"

echo "== start"
start --create-size "$SIZE"
mkfs.ext4 -q -F "$DEV" || fail "mkfs"
mkdir -p /mnt/blobdev-stress
mount "$DEV" /mnt/blobdev-stress || fail "mount"

echo "== write ${MB} MiB while verifying"
deadline=$((SECONDS + SECONDS_TO_RUN))
i=0
while [ $SECONDS -lt $deadline ]; do
  i=$((i + 1))
  f=/mnt/blobdev-stress/part-$i.bin
  dd if=/dev/urandom of=$f bs=1M count=8 status=none || fail "write $i"
  # Read it back through the device immediately.
  a=$(sha256sum $f | awk '{print $1}')
  echo 3 > /proc/sys/vm/drop_caches 2>/dev/null
  b=$(sha256sum $f | awk '{print $1}')
  [ "$a" = "$b" ] || fail "readback mismatch on part $i"
  rm -f $f
done
echo "   $i rounds completed"

echo "== keep a final file and verify end to end"
dd if=/dev/urandom of=/mnt/blobdev-stress/final.bin bs=1M count=64 status=none
SUM=$(sha256sum /mnt/blobdev-stress/final.bin | awk '{print $1}')
sync
umount /mnt/blobdev-stress || fail "umount"
cleanup

echo "== restart and verify from the object store"
start
mount -o ro "$DEV" /mnt/blobdev-stress || fail "remount"
AGAIN=$(sha256sum /mnt/blobdev-stress/final.bin | awk '{print $1}')
umount /mnt/blobdev-stress 2>/dev/null
[ "$SUM" = "$AGAIN" ] || fail "data changed across restart ($SUM vs $AGAIN)"
echo "PASS: $i rounds, restart verified"
