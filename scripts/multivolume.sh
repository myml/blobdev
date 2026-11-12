#!/bin/bash
# Several volumes at once, and managing them while they run.
#
# One volume at a time has been tested throughout. What an operator does is
# run several and then ask which is which, stop one, and clean up the rest --
# and the commands that do that (status, stop, reap) had never been run
# against more than one.
#
# Two things this had to learn about the kernel:
#   - a device id is released asynchronously; reusing it before the table is
#     empty fails with EOPNOTSUPP, which reads like a broken device
#   - `wait` with no arguments waits for the daemons too, and they never exit
#
# usage: multivolume.sh
set -u
BIN=${BIN:-/usr/local/bin/blobdev}
W=${W:-/root/bd-test/mv}
MNT=${MNT:-/mnt/mv}
IDS=${IDS:-"40 41 42 43"}

pkill -9 -x blobdev >/dev/null 2>&1
sleep 2
for m in $(grep -oE "^/dev/ublkb[0-9]+ [^ ]+" /proc/mounts 2>/dev/null | awk '{print $2}'); do
  umount -l "$m" 2>/dev/null
done
"$BIN" reap >/dev/null 2>&1
rm -rf "$W"; mkdir -p "$W" "$MNT"

for id in $IDS; do
  $BIN run --backend dir --dir "$W/o$id" --volume mv$id --meta "$W/mv$id.meta" \
    --id "$id" --create-size 256M > "$W/log$id" 2>&1 &
done
for id in $IDS; do
  for _ in $(seq 1 60); do [ -e "/dev/ublkb$id" ] && break; sleep 1; done
  [ -e "/dev/ublkb$id" ] || { echo "FAIL: /dev/ublkb$id never appeared"; exit 1; }
done
sleep 2
echo "== started $IDS"

RUNNING=$($BIN status 2>&1 | grep -c running)
echo "STATUS_RUNNING=$RUNNING"
[ "$RUNNING" -eq 4 ] || { echo "FAIL: status reported $RUNNING running, expected 4"; $BIN status; exit 1; }

for id in $IDS; do
  mkfs.ext4 -q -F "/dev/ublkb$id" 2>/dev/null || { echo "FAIL: mkfs $id"; exit 1; }
  mkdir -p "$MNT/$id"
  for _ in $(seq 1 60); do mount "/dev/ublkb$id" "$MNT/$id" 2>/dev/null && break; sleep 1; done
  mountpoint -q "$MNT/$id" || { echo "FAIL: mount $id"; exit 1; }
done
echo "== all mounted"

# status must name the mount points, not just the devices.
MOUNTED=$($BIN status 2>&1 | grep -c "$MNT/")
echo "STATUS_MOUNTED=$MOUNTED"
[ "$MOUNTED" -ge 4 ] || { echo "FAIL: status did not report the mount points"; $BIN status; exit 1; }

WPIDS=""
for id in $IDS; do
  ( head -c 16777216 /dev/urandom > "$W/src$id.bin"
    cp "$W/src$id.bin" "$MNT/$id/f.bin"
    sync
    cmp -s "$MNT/$id/f.bin" "$W/src$id.bin" && echo ok > "$W/r$id" || echo bad > "$W/r$id" ) &
  WPIDS="$WPIDS $!"
done
for p in $WPIDS; do wait "$p"; done
BAD=0
for id in $IDS; do [ "$(cat "$W/r$id" 2>/dev/null)" = ok ] || BAD=$((BAD + 1)); done
echo "WRITE_BAD=$BAD"

for id in $IDS; do umount "$MNT/$id" 2>/dev/null; done
# Stop one while the others run: they must keep working.
FIRST=$(echo $IDS | awk '{print $1}')
$BIN stop --id "$FIRST" >/dev/null 2>&1
sleep 4
LEFT=$($BIN status 2>&1 | grep -c running)
echo "RUNNING_AFTER_STOP=$LEFT"

pkill -TERM -x blobdev
for _ in $(seq 1 600); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
pgrep -x blobdev >/dev/null && pkill -9 -x blobdev
for _ in $(seq 1 60); do
  [ "$(ls /sys/class/ublk-char 2>/dev/null | wc -l)" -eq 0 ] && break
  sleep 1
done
"$BIN" reap >/dev/null 2>&1
sleep 2

MISMATCH=0
for id in $IDS; do
  $BIN run --backend dir --dir "$W/o$id" --volume mv$id --meta "$W/mv$id.meta" --id "$id" > "$W/rl$id" 2>&1 &
  for _ in $(seq 1 60); do [ -e "/dev/ublkb$id" ] && break; sleep 1; done
  sleep 2
  mkdir -p "$MNT/$id"
  for _ in $(seq 1 60); do mount -o ro "/dev/ublkb$id" "$MNT/$id" 2>/dev/null && break; sleep 1; done
  if mountpoint -q "$MNT/$id" && cmp -s "$MNT/$id/f.bin" "$W/src$id.bin"; then
    echo "  mv$id: identical"
  else
    echo "  mv$id: FAILED"
    MISMATCH=$((MISMATCH + 1))
  fi
  umount "$MNT/$id" 2>/dev/null
done
pkill -9 -x blobdev
sleep 2
"$BIN" reap >/dev/null 2>&1

[ "$BAD" -eq 0 ] && [ "$MISMATCH" -eq 0 ] && [ "$LEFT" -eq 3 ] &&
  echo "PASS: four volumes under load" || echo "FAIL: bad=$BAD mismatch=$MISMATCH left=$LEFT"
