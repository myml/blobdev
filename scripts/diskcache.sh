#!/bin/bash
# Does the disk read cache actually work, and does it stay within its budget?
#
# The cache exists so a restart does not have to fetch everything from the
# store again. Two properties matter, and neither had been measured against a
# real backend:
#
#   1. after a restart, reads are served from disk instead of the store
#   2. the directory never grows past --cache-size, whatever it is asked to hold
#
# usage: diskcache.sh [backend: webdav|dir]
set -u
BACKEND=${1:-webdav}
BIN=${BIN:-/usr/local/bin/blobdev}
W=${W:-/root/bd-test/dc}
MNT=/mnt/nc
CACHE=$W/cache
NC="http://127.0.0.1:18083/remote.php/dav/files/ncadmin/blobdev"

pkill -9 -x blobdev >/dev/null 2>&1
sleep 2
$BIN reap >/dev/null 2>&1
umount -l "$MNT" 2>/dev/null
rm -rf "$W"; mkdir -p "$W" "$MNT"

if [ "$BACKEND" = dir ]; then
  ARGS=(--backend dir --dir "$W/objs")
else
  ARGS=(--backend webdav --endpoint "$NC" --user ncadmin --password ncpass12345
        --timeout 20 --retries 3)
fi

V=dc$RANDOM
start() {
  local log=$1; shift
  $BIN run "${ARGS[@]}" --volume "$V" --meta "$W/v.meta" --id -1 \
    --cache-dir "$CACHE" --cache-size "${CACHE_SIZE:-8M}" "$@" > "$W/$log" 2>&1 &
  for i in $(seq 1 120); do
    D=$(grep -oE "/dev/ublkb[0-9]+" "$W/$log" 2>/dev/null | head -1)
    [ -n "${D:-}" ] && [ -e "$D" ] && { sleep 2; echo "$D"; return 0; }
    sleep 0.5
  done
  return 1
}
cache_bytes() { du -sb "$CACHE" 2>/dev/null | awk '{print $1}'; }
reqs() { curl -s --max-time 5 http://127.0.0.1:19090/stats 2>/dev/null | grep '^requests=' | cut -d= -f2; }

D=$(start log1 --create-size 512M) || { echo "FAIL: start"; tail -3 "$W/log1"; exit 1; }
mkfs.ext4 -q -F "$D" || { echo "FAIL: mkfs"; exit 1; }
for _ in $(seq 1 60); do mount "$D" "$MNT" 2>/dev/null && break; sleep 1; done
mountpoint -q "$MNT" || { echo "FAIL: mount"; exit 1; }

# 32 MiB, which is four times the 8 MiB budget: the cache must evict.
head -c 33554432 /dev/urandom > "$W/src.bin"
cp "$W/src.bin" "$MNT/a.bin"
sync
echo "CACHE_AFTER_WRITE=$(cache_bytes) bytes"
umount "$MNT"
pkill -TERM -x blobdev
for i in $(seq 1 600); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
pgrep -x blobdev >/dev/null && pkill -9 -x blobdev
sleep 3
echo "CACHE_ON_DISK=$(cache_bytes) bytes"
echo "CACHE_BUDGET=$(( 8 * 1024 * 1024 )) bytes"

# Read it all back. With a warm cache the store is not needed for the chunks
# that fit; with a cold one every chunk is fetched.
D=$(start log2) || { echo "FAIL: restart"; exit 1; }
for _ in $(seq 1 120); do mount -o ro "$D" "$MNT" 2>/dev/null && break; sleep 1; done
mountpoint -q "$MNT" || { echo "FAIL: remount"; exit 1; }
T0=$(date +%s)
if cmp -s "$MNT/a.bin" "$W/src.bin"; then echo "READBACK=identical"; else echo "READBACK=differs"; fi
T1=$(date +%s)
echo "READ_SECONDS=$((T1 - T0))"
echo "CACHE_HITS=$(grep -oE 'hits [0-9]+' "$W/log2" 2>/dev/null | tail -1)"
echo "CACHE_AFTER_READ=$(cache_bytes) bytes"
umount "$MNT" 2>/dev/null
pkill -9 -x blobdev
