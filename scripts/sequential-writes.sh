#!/bin/bash
# Does --full-pct decide how many times a sequentially written object goes up?
#
# Default 50 fires at 2 MiB of dirty blocks in a 4 MiB object, so a
# sequentially written object is uploaded in two pieces. That is a write
# amplification nobody asked for on the most common workload there is.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/fp; MNT=/mnt/fp

try() {
  local label=$1 pct=$2
  pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
  $BIN reap >/dev/null 2>&1
  rm -rf $W; mkdir -p $W $MNT
  BLOBDEV_SUMMARY_SECS=1 $BIN run --backend dir --dir $W/o --volume fp$RANDOM \
    --meta $W/v.meta --id 93 --create-size 2G --max-age 30 --full-pct $pct > $W/l 2>&1 &
  for _ in $(seq 1 120); do grep -q "created /dev/ublkb93" $W/l 2>/dev/null && break; sleep 1; done
  mkfs.ext4 -q -F /dev/ublkb93 2>/dev/null
  for _ in $(seq 1 90); do mount /dev/ublkb93 $MNT 2>/dev/null && break; sleep 1; done
  sync
  local out
  out=$(dd if=/dev/zero of=$MNT/b.bin bs=1M count=256 oflag=direct 2>&1 | tail -1 | sed 's/.*, //')
  sleep 4
  local sent
  sent=$(grep -oE "sent [0-9.]+ MiB" $W/l | tail -1 | awk '{print $2}')
  local uploaded
  uploaded=$(du -sm $W/o 2>/dev/null | awk '{print $1}')
  echo "  $label: dd=$out  sent=${sent:-0} MiB  on disk=${uploaded} MiB"
  umount $MNT 2>/dev/null
  $BIN stop --id 93 >/dev/null 2>&1
  for _ in $(seq 1 300); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
}

echo "== sequential 256 MiB write, varying --full-pct"
try "full-pct 40" 40
try "full-pct 50 (default)" 50
try "full-pct 90" 90
