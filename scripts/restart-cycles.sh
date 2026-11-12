#!/bin/bash
# Restart the same volume repeatedly, accumulating data and verifying that
# every earlier file survives each round.
#
# This is the ordinary service-restart pattern, and it exercises the paths
# that a single write-then-restart cycle can miss: the metadata has to be
# rewritten each time, and objects written in earlier rounds have to stay
# readable while new ones are added.
#
# usage: S3ARGS="..." restart-cycles.sh
set +e
BIN=${BIN:-/usr/local/bin/blobdev}
ROOT=${ROOT:-/tmp/blobdev-rc}
VOL=${VOL:-rcvol$RANDOM}
SIZE=${SIZE:-256M}
ROUNDS=${ROUNDS:-4}
FILE_MIB=${FILE_MIB:-12}
pkill -9 -x blobdev; sleep 2; umount /mnt/rc 2>/dev/null
# /tmp is often a tmpfs on test hosts. Filling it makes later runs fail in a
# way that looks like a blobdev bug, so refuse to start without room.
SPACE_DIR=$(dirname "${ROOT:-${W:-/var/tmp}}")
mkdir -p "$SPACE_DIR" 2>/dev/null
AVAIL_KB=$(df -Pk "$SPACE_DIR" 2>/dev/null | awk 'NR==2 {print $4}')
if [ -n "${AVAIL_KB:-}" ] && [ "$AVAIL_KB" -lt $((1024 * 1024)) ]; then
  echo "FAIL: less than 1 GiB free in $SPACE_DIR" >&2
  df -h "$SPACE_DIR" >&2
  exit 1
fi

rm -rf $ROOT; mkdir -p $ROOT
DEV=""

start() {
  local extra="$1"
  $BIN run $S3ARGS --volume $VOL --meta $ROOT/v.meta --id -1 $extra > $ROOT/log 2>&1 &
  DEV=""
  for i in $(seq 1 60); do
    DEV=$(grep -oE "/dev/ublkb[0-9]+" $ROOT/log | head -1); [ -n "$DEV" ] && break; sleep 0.5
  done
  [ -n "$DEV" ] || { echo "FAIL: no device"; tail -5 $ROOT/log; exit 1; }
}

start "--create-size $SIZE"
mkfs.ext4 -q -F $DEV >/dev/null 2>&1 || { echo "FAIL mkfs"; exit 1; }
mkdir -p /mnt/rc; mount $DEV /mnt/rc || { echo "FAIL mount"; exit 1; }

for round in $(seq 1 $ROUNDS); do
  dd if=/dev/urandom of=/mnt/rc/r$round.bin bs=1M count=$FILE_MIB status=none
  sync
  umount /mnt/rc || { echo "FAIL umount round $round"; exit 1; }
  pkill -TERM -x blobdev; sleep 2
  start ""
  mount $DEV /mnt/rc || { echo "FAIL remount round $round"; exit 1; }
  for prev in $(seq 1 $round); do
    [ -f /mnt/rc/r$prev.bin ] || { echo "FAIL round $round: r$prev.bin missing"; exit 1; }
    sz=$(stat -c %s /mnt/rc/r$prev.bin)
    [ "$sz" = $((FILE_MIB * 1048576)) ] || { echo "FAIL round $round: r$prev.bin is $sz bytes"; exit 1; }
  done
  echo "  round $round ok ($round file(s))"
  umount /mnt/rc
  pkill -TERM -x blobdev; sleep 2
  start ""
  mount $DEV /mnt/rc || { echo "FAIL final remount"; exit 1; }
done
umount /mnt/rc 2>/dev/null
echo "PASS: $ROUNDS restart cycles, all files intact"
