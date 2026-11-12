#!/bin/bash
# Several processes writing and reading at once, then a restart.
#
# The single-writer stress run proves one client is handled. This proves the
# device holds up when the kernel has many requests in flight from unrelated
# processes, and that what they wrote is still there after a restart.
#
# Each worker owns one file and writes it, reads it back and compares, in a
# loop. A worker that sees its own data change has caught a real corruption;
# one that only fails to write has caught a busy device.
#
# usage: concurrent.sh [seconds] [workers]
set -u
SECONDS_TO_RUN=${1:-120}
WORKERS=${2:-4}
BIN=${BIN:-/usr/local/bin/blobdev}
NC=${NC:-http://127.0.0.1:18083/remote.php/dav/files/ncadmin/blobdev}
W=${W:-/root/bd-test/conc}
MNT=${MNT:-/mnt/nc}

pkill -9 -x blobdev >/dev/null 2>&1
sleep 2
$BIN reap >/dev/null 2>&1
umount -l "$MNT" 2>/dev/null
rm -rf "$W"; mkdir -p "$W" "$MNT"
V=conc$RANDOM

start() {
  local log=$1; shift
  $BIN run --backend webdav --endpoint "$NC" --user ncadmin --password ncpass12345 \
    --volume $V --meta "$W/v.meta" --id -1 --queues 2 --depth 32 "$@" > "$W/$log" 2>&1 &
  for i in $(seq 1 120); do
    local d
    d=$(grep -oE "/dev/ublkb[0-9]+" "$W/$log" 2>/dev/null | head -1)
    if [ -n "${d:-}" ] && [ -e "$d" ]; then
      dd if="$d" of=/dev/null bs=4096 count=1 iflag=direct status=none 2>/dev/null && { echo "$d"; return 0; }
    fi
    sleep 0.5
  done
  return 1
}

D=$(start log1 --create-size 512M) || { echo "FAIL: start"; tail -3 "$W/log1"; exit 1; }
mkfs.ext4 -q -F "$D" || { echo "FAIL: mkfs"; exit 1; }
# mkfs leaves the daemon uploading what it just wrote; opening the device for a
# mount during that window fails with "Can't lookup blockdev", which is a
# timing artefact rather than a broken device.
for _ in $(seq 1 60); do
  mount "$D" "$MNT" 2>/dev/null && break
  sleep 1
done
mountpoint -q "$MNT" || { echo "FAIL: mount"; pkill -9 -x blobdev; exit 1; }

worker() {
  local id=$1
  local file=$MNT/w$id.bin
  local src=$W/src$id.bin
  local seed=$((id * 7919 + 13))
  local bad=0 rounds=0
  end=$(( $(date +%s) + SECONDS_TO_RUN ))
  while [ "$(date +%s)" -lt "$end" ]; do
    head -c 1048576 /dev/urandom > "$src" 2>/dev/null
    if ! cp "$src" "$file" 2>/dev/null; then
      # A failing write is a reported error, not a silent one.
      echo "worker $id: write failed" >> "$W/worker$id.out"
      bad=$((bad + 1))
      continue
    fi
    sync
    if ! cmp -s "$file" "$src"; then
      echo "worker $id: READ BACK DIFFERS at round $rounds" >> "$W/worker$id.out"
      bad=$((bad + 1))
    fi
    rounds=$((rounds + 1))
  done
  echo "worker $id: rounds=$rounds bad=$bad (seed $seed)" >> "$W/worker$id.out"
}

for i in $(seq 1 "$WORKERS"); do
  worker "$i" &
done
wait

echo "== workers"
cat "$W"/worker*.out 2>/dev/null
BADC=$(cat "$W"/worker*.out 2>/dev/null | grep -oE "bad=[0-9]+" | cut -d= -f2 | awk '{s+=$1} END {print s+0}')
echo "TOTAL_BAD=$BADC"

echo "== restart"
umount "$MNT" || { echo "FAIL: umount"; pkill -9 -x blobdev; exit 1; }
pkill -TERM -x blobdev
for i in $(seq 1 600); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
pgrep -x blobdev >/dev/null && pkill -9 -x blobdev
sleep 3

D2=$(start log2) || { echo "FAIL: restart"; tail -3 "$W/log2"; exit 1; }
sleep 3
for _ in $(seq 1 120); do mount -o ro "$D2" "$MNT" 2>/dev/null && break; sleep 1; done
if ! mountpoint -q "$MNT"; then echo "FAIL: remount"; pkill -9 -x blobdev; exit 1; fi

MISMATCH=0
for i in $(seq 1 "$WORKERS"); do
  if [ -f "$W/src$i.bin" ] && [ -f "$MNT/w$i.bin" ]; then
    cmp -s "$MNT/w$i.bin" "$W/src$i.bin" || { echo "worker $i: final file differs after restart"; MISMATCH=$((MISMATCH + 1)); }
  fi
done
echo "RESTART_MISMATCH=$MISMATCH"
umount "$MNT" 2>/dev/null
pkill -9 -x blobdev
[ "$BADC" -eq 0 ] && [ "$MISMATCH" -eq 0 ] && echo "PASS: $WORKERS workers, ${SECONDS_TO_RUN}s, data intact" || echo "FAIL"
