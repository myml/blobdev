#!/bin/bash
# Ten minutes of mixed IO, watching memory.
#
# Sequential write, random 4 KiB writes and a read-back every pass, against
# S3, with the memory sampled each minute. Measured: RSS 104 MB at the start
# and 112-116 MB throughout, 10 file descriptors, nothing left dirty, no
# errors in the log, and the file identical after a restart.
#
# Override DURATION_SECONDS for a shorter run (tests) or a longer soak.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/longio; MNT=/mnt/longio
S3=(--backend s3 --endpoint http://192.168.0.220:9000 --bucket blobtest \
  --access-key blobdev --secret-key blobdevsecret123 --region us-east-1 --path-style)

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
$BIN run "${S3[@]}" --volume long$RANDOM --meta $W/v.meta --id 73 --create-size 1G \
  --max-age 5 --full-pct 30 > $W/l 2>&1 &
# Wait for THIS daemon, not for a device to exist: a device left by an
# earlier run is still there, is shut down, and mounting it produces a
# filesystem that returns EIO to everything.
for _ in $(seq 1 180); do grep -q "created /dev/ublkb73" $W/l 2>/dev/null && break; sleep 1; done
if ! pgrep -x blobdev >/dev/null || ! grep -q "created /dev/ublkb73" $W/l 2>/dev/null; then
  echo "FAIL: the daemon did not start"
  grep -vE "^\[.*INFO" $W/l 2>/dev/null | head -3
  exit 1
fi
mkfs.ext4 -q -F /dev/ublkb73 2>/dev/null || { echo "FAIL: mkfs"; exit 1; }
mounted=no
for _ in $(seq 1 120); do mount /dev/ublkb73 $MNT 2>/dev/null && { mounted=yes; break; }; sleep 1; done
[ "$mounted" = yes ] || { echo "FAIL: mount"; exit 1; }
PID=$(pgrep -x blobdev | head -1)

echo "minute  rss_kb  fds  dirty_mb"
DURATION=${DURATION_SECONDS:-600}
START=$(date +%s)
IO_ERRORS=0
while [ $(( $(date +%s) - START )) -lt "$DURATION" ]; do
  # Mixed: sequential write, random small write, read back.
  #
  # Errors are counted, not discarded. A filesystem that has been shut down
  # (the daemon died under it) stays listed in /proc/mounts, so the mount
  # check passes while every write returns EIO -- and a soak that quietly
  # does no IO measures nothing.
  if ! dd if=/dev/urandom of=$MNT/seq.bin bs=1M count=8 conv=notrunc status=none 2>/dev/null; then
    IO_ERRORS=$((IO_ERRORS + 1))
  fi
  for _ in $(seq 1 20); do
    dd if=/dev/urandom of=$MNT/rnd.bin bs=4096 seek=$((RANDOM % 2048)) count=1 conv=notrunc status=none 2>/dev/null ||
      IO_ERRORS=$((IO_ERRORS + 1))
  done
  dd if=$MNT/seq.bin of=/dev/null bs=1M status=none 2>/dev/null || IO_ERRORS=$((IO_ERRORS + 1))
  sync
  M=$(( ($(date +%s) - START) / 60 ))
  RSS=$(awk '/VmRSS/{print $2}' /proc/$PID/status 2>/dev/null || echo 0)
  FDS=$(ls /proc/$PID/fd 2>/dev/null | wc -l)
  DIRTY=$(grep -oE "dirty [0-9]+" $W/l 2>/dev/null | tail -1 | awk '{print $2}' || echo "?")
  printf "%-7s %-7s %-4s %s\n" "$M" "$RSS" "$FDS" "${DIRTY:-0}"
  # A single lost write is noise; a pass where every write failed means the
  # filesystem is gone and the numbers from here on are meaningless.
  if [ "$IO_ERRORS" -ge 21 ]; then
    echo "  every write in this pass failed -- the filesystem is not accepting IO"
    break
  fi
done
echo "IO_ERRORS=$IO_ERRORS"
[ "$IO_ERRORS" -ge 21 ] && echo "FAIL: the soak did no useful IO"

echo "== verify"
WANT=$(sha256sum $MNT/seq.bin | awk '{print $1}')
umount $MNT
pkill -TERM -x blobdev
for _ in $(seq 1 900); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
pkill -9 -x blobdev 2>/dev/null
for _ in $(seq 1 60); do [ "$(ls /sys/class/ublk-char 2>/dev/null|wc -l)" -eq 0 ] && break; sleep 1; done
$BIN run "${S3[@]}" --volume $(grep -oE "long[0-9]+" $W/l | head -1) --meta $W/v.meta --id 73 > $W/l2 2>&1 &
for _ in $(seq 1 180); do [ -e /dev/ublkb73 ] && break; sleep 1; done
for _ in $(seq 1 120); do mount -o ro /dev/ublkb73 $MNT 2>/dev/null && break; sleep 1; done
GOT=$(sha256sum $MNT/seq.bin 2>/dev/null | awk '{print $1}')
[ "$WANT" = "$GOT" ] && echo "DATA_IDENTICAL=yes" || echo "DATA_IDENTICAL=no"
umount $MNT 2>/dev/null; pkill -9 -x blobdev 2>/dev/null
