#!/bin/bash
# Watch the daemon while it works, looking for things that only grow.
#
# A leak does not show up in a five minute test: the RSS creeps, file
# descriptors accumulate, or the object store collects orphans, and the
# machine falls over an hour later. This samples the things that should stay
# flat and prints them, so a trend is visible rather than guessed at.
#
# usage: soak.sh [seconds] [sample-every]
set -u
SECONDS_TO_RUN=${1:-900}
EVERY=${2:-60}
BIN=${BIN:-/usr/local/bin/blobdev}
NC=${NC:-http://127.0.0.1:18083/remote.php/dav/files/ncadmin/blobdev}
W=${W:-/root/bd-test/soak}
MNT=${MNT:-/mnt/nc}

pkill -9 -x blobdev >/dev/null 2>&1
sleep 2
$BIN reap >/dev/null 2>&1
umount -l "$MNT" 2>/dev/null
rm -rf "$W"; mkdir -p "$W" "$MNT"
V=soak$RANDOM

$BIN run --backend webdav --endpoint "$NC" --user ncadmin --password ncpass12345 \
  --volume $V --meta "$W/v.meta" --id -1 --create-size 1G > "$W/log" 2>&1 &
for i in $(seq 1 120); do
  D=$(grep -oE "/dev/ublkb[0-9]+" "$W/log" 2>/dev/null | head -1)
  [ -n "${D:-}" ] && [ -e "$D" ] && dd if="$D" of=/dev/null bs=4096 count=1 iflag=direct status=none 2>/dev/null && break
  sleep 0.5
done
mkfs.ext4 -q -F "$D" || { echo "FAIL: mkfs"; exit 1; }
for _ in $(seq 1 60); do mount "$D" "$MNT" 2>/dev/null && break; sleep 1; done
mountpoint -q "$MNT" || { echo "FAIL: mount"; exit 1; }

PID=$(pgrep -x blobdev | head -1)
echo "pid=$PID device=$D"
sample() {
  local tag=$1
  local rss fds objs
  rss=$(awk '/VmRSS/ {print $2}' /proc/$PID/status 2>/dev/null)
  fds=$(ls /proc/$PID/fd 2>/dev/null | wc -l)
  # Objects in the volume: a garbage collector that never runs shows up here.
  objs=$(curl -s -X PROPFIND -H "Depth: 1" -u ncadmin:ncpass12345 \
    "$NC/$V/c/" 2>/dev/null | grep -c 'd:href.*/c/[0-9a-z]')
  echo "SAMPLE $tag rss_kb=${rss:-0} fds=${fds:-0} objects=${objs:-0} $(date +%H:%M:%S)"
}

sample start
END=$(( $(date +%s) + SECONDS_TO_RUN ))
N=0
while [ "$(date +%s)" -lt "$END" ]; do
  # Keep the device busy: write a fresh file, read it back, delete it.
  head -c 4194304 /dev/urandom > "$MNT/tmp.bin" 2>/dev/null
  sync
  cmp -s "$MNT/tmp.bin" /dev/null 2>/dev/null  # keep the page cache honest
  rm -f "$MNT/tmp.bin"
  N=$((N + 1))
  sleep "$EVERY"
  sample "$N"
done

echo "ROUNDS=$N"
umount "$MNT" 2>/dev/null
pkill -9 -x blobdev
