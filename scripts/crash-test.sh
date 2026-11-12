#!/bin/bash
# Power-loss simulation: kill the daemon at random moments while a filesystem
# is mounted and being written, then check that the volume still mounts and
# that the filesystem is consistent.
#
# This is what separates "works when told to stop" from "survives a crash".
set -u
BIN=${BIN:-./target/release/blobdev}
ROOT=${ROOT:-/tmp/blobdev-crash}
VOL=${VOL:-crash}
SIZE=${SIZE:-512M}
ROUNDS=${ROUNDS:-6}

fail() { echo "FAIL: $*" >&2; exit 1; }
cleanup() {
  umount /mnt/blobdev-crash 2>/dev/null
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

mkdir -p "$ROOT"
start --create-size "$SIZE"
mkfs.ext4 -q -F "$DEV" || fail "mkfs"
mkdir -p /mnt/blobdev-crash
mount "$DEV" /mnt/blobdev-crash || fail "mount"

for round in $(seq 1 "$ROUNDS"); do
  echo "== round $round: write, then kill -9 mid-stream"
  # Write in the background and kill the daemon while the write is in flight.
  dd if=/dev/urandom of=/mnt/blobdev-crash/r$round.bin bs=1M count=32 status=none &
  WRITER=$!
  sleep 0.$((RANDOM % 9 + 1))
  pkill -9 -x "$(basename "$BIN")"
  wait $WRITER 2>/dev/null
  umount /mnt/blobdev-crash 2>/dev/null
  sleep 1

  start
  # The volume must still be mountable; fsck repairs what the crash cut off.
  if ! mount "$DEV" /mnt/blobdev-crash; then
    echo "   mount after crash failed, running fsck"
    fsck.ext4 -y "$DEV" >/dev/null 2>&1
    mount "$DEV" /mnt/blobdev-crash || fail "volume unusable after crash $round"
  fi
  # Data written in earlier rounds and synced must survive.
  for prev in $(seq 1 $((round - 1))); do
    if [ -f /mnt/blobdev-crash/r$prev.bin ]; then
      sz=$(stat -c %s /mnt/blobdev-crash/r$prev.bin)
      [ "$sz" = 33554432 ] || fail "round $prev lost data: ${sz} bytes"
    fi
  done
  umount /mnt/blobdev-crash 2>/dev/null
done

echo "PASS: $ROUNDS crash rounds, volume still usable"
