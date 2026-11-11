#!/bin/bash
# Long run: keep the volume busy for a while and watch that nothing grows.
#
# A short stress run proves the data path works. It does not prove the daemon
# can run for days: a cache that never evicts, a file descriptor that is never
# closed, or a counter that only goes up all look fine for a minute and show
# up as an OOM kill or EBADF hours later.
#
# usage: BLOBDEV_ARGS="--backend ..." long-run.sh [seconds]
set -u
BIN=${BIN:-/usr/local/bin/blobdev}
ROOT=${ROOT:-/tmp/blobdev-long}
VOL=${VOL:-long$RANDOM}
SIZE=${SIZE:-2G}
DURATION=${1:-${DURATION:-900}}
SAMPLE_EVERY=${SAMPLE_EVERY:-30}

mkdir -p "$ROOT"
pkill -9 -x blobdev 2>/dev/null
sleep 1
umount "$ROOT/mnt" 2>/dev/null
rm -rf "$ROOT"
mkdir -p "$ROOT/mnt"

fail() { echo "FAIL: $*" >&2; exit 1; }
cleanup() {
  umount "$ROOT/mnt" 2>/dev/null
  pkill -TERM -x blobdev 2>/dev/null
  sleep 2
  pkill -9 -x blobdev 2>/dev/null
}
trap cleanup EXIT

# shellcheck disable=SC2086
BLOBDEV_SUMMARY_SECS=60 "$BIN" run $BLOBDEV_ARGS --volume "$VOL" --meta "$ROOT/v.meta" \
  --create-size "$SIZE" --id -1 > "$ROOT/log" 2>&1 &
DEV=""
for _ in $(seq 1 60); do
  DEV=$(grep -oE "/dev/ublkb[0-9]+" "$ROOT/log" | head -1)
  [ -n "$DEV" ] && break
  sleep 0.5
done
[ -n "$DEV" ] || { tail -5 "$ROOT/log"; fail "device did not appear"; }
PID=$(pgrep -x blobdev | head -1)
echo "device=$DEV pid=$PID duration=${DURATION}s"

mkfs.ext4 -q -F "$DEV" || fail "mkfs"
mount "$DEV" "$ROOT/mnt" || fail "mount"

# First sample is the baseline, taken once the device is serving.
sample() {
  local rss fds
  rss=$(awk '/VmRSS/ {print $2}' "/proc/$PID/status" 2>/dev/null)
  fds=$(ls "/proc/$PID/fd" 2>/dev/null | wc -l)
  echo "${rss:-0} ${fds:-0}"
}

read -r BASE_RSS BASE_FDS <<< "$(sample)"
echo "baseline: rss=${BASE_RSS}kB fds=$BASE_FDS"

START=$(date +%s)
ROUND=0
LAST_SAMPLE=$START
MAX_RSS=$BASE_RSS
MAX_FDS=$BASE_FDS
ERRORS=0

while [ $(( $(date +%s) - START )) -lt "$DURATION" ]; do
  ROUND=$((ROUND + 1))
  # Mixed profile: a big sequential write, a few random overwrites of blocks
  # already on disk, and a read-back that cannot be served from the page cache.
  dd if=/dev/urandom of="$ROOT/mnt/seq.bin" bs=1M count=32 status=none || fail "write $ROUND"
  for _ in 1 2 3; do
    off=$(( (RANDOM * 4096) % (32 * 1024 * 1024) ))
    dd if=/dev/urandom of="$ROOT/mnt/seq.bin" bs=4096 seek=$((off / 4096)) count=1 \
      conv=notrunc status=none || fail "overwrite $ROUND"
  done
  sync
  sum=$(sha256sum "$ROOT/mnt/seq.bin" | awk '{print $1}')
  echo 3 > /proc/sys/vm/drop_caches 2>/dev/null
  sum2=$(sha256sum "$ROOT/mnt/seq.bin" | awk '{print $1}')
  [ "$sum" = "$sum2" ] || { ERRORS=$((ERRORS + 1)); echo "  round $ROUND: readback differs"; }

  NOW=$(date +%s)
  if [ $((NOW - LAST_SAMPLE)) -ge "$SAMPLE_EVERY" ]; then
    LAST_SAMPLE=$NOW
    read -r RSS FDS <<< "$(sample)"
    [ "${RSS:-0}" -gt "$MAX_RSS" ] && MAX_RSS=$RSS
    [ "${FDS:-0}" -gt "$MAX_FDS" ] && MAX_FDS=$FDS
    echo "  t=$((NOW - START))s round=$ROUND rss=${RSS}kB fds=$FDS"
  fi
done

read -r END_RSS END_FDS <<< "$(sample)"
ELAPSED=$(( $(date +%s) - START ))

echo "== results: $ROUND rounds in ${ELAPSED}s, $ERRORS readback error(s)"
echo "   rss  ${BASE_RSS}kB -> ${END_RSS}kB (peak ${MAX_RSS}kB)"
echo "   fds  $BASE_FDS -> $END_FDS (peak $MAX_FDS)"

umount "$ROOT/mnt" || fail "umount"
pkill -TERM -x blobdev
sleep 3

# Growth limits: some is expected (the read cache fills, allocators keep
# arenas). Doubling over the whole run, or leaking descriptors, is not.
[ "$ERRORS" -eq 0 ] || fail "$ERRORS readback error(s)"
if [ "$BASE_RSS" -gt 0 ] && [ "$MAX_RSS" -gt $(( BASE_RSS * 3 )) ]; then
  fail "rss grew from ${BASE_RSS}kB to ${MAX_RSS}kB (more than 3x)"
fi
if [ "$MAX_FDS" -gt $(( BASE_FDS + 64 )) ]; then
  fail "file descriptors grew from $BASE_FDS to $MAX_FDS"
fi

# The periodic summary must have been written.
grep -q "dirty .* chunk(s)" "$ROOT/log" || fail "no periodic summary in the log"
echo "PASS: $ROUND rounds, stable memory and descriptors"
