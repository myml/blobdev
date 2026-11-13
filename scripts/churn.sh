#!/bin/bash
# Twenty stop-start cycles back to back, each writing a line into the volume.
#
# This started as a way to chase one failure in twenty. The failure was real
# and is fixed: stop --meta used to wait for the daemon's pid to vanish, but a
# process leaves /proc before it releases its flock, so one start in twenty
# collided with a lock its predecessor had not yet let go. stop takes the lock
# now, and this is 0 in 20.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/ch5; MNT=/mnt/ch5
pkill -9 -x blobdev >/dev/null 2>&1; sleep 3
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
V=c5$RANDOM
FAILED=0

$BIN run --backend dir --dir $W/o --volume $V --meta $W/v.meta --id 201 --create-size 512M --max-age 1 > $W/l 2>&1 &
for _ in $(seq 1 120); do grep -q "created /dev/ublkb201" $W/l 2>/dev/null && break; sleep 0.5; done
mkfs.ext4 -q -F /dev/ublkb201 2>/dev/null
for _ in $(seq 1 90); do mount /dev/ublkb201 $MNT 2>/dev/null && break; sleep 0.5; done
head -c 16777216 /dev/urandom > $MNT/keep.bin
sync; sleep 3
umount $MNT 2>/dev/null
$BIN stop --id 201 --meta $W/v.meta >/dev/null 2>&1

for n in $(seq 1 20); do
  $BIN run --backend dir --dir $W/o --volume $V --meta $W/v.meta --id 201 --max-age 1 > $W/l$n 2>&1 &
  OK=0
  for _ in $(seq 1 120); do grep -q "created /dev/ublkb201" $W/l$n 2>/dev/null && { OK=1; break; }; sleep 0.5; done
  if [ "$OK" -ne 1 ]; then
    FAILED=$((FAILED+1))
    echo "FAILED round $n:"
    grep -vE "INFO" $W/l$n 2>/dev/null | head -2 | sed 's/^/    /' | cut -c1-110
    sleep 1
  fi
  for _ in $(seq 1 60); do mount /dev/ublkb201 $MNT 2>/dev/null && break; sleep 0.3; done
  echo "round $n" >> $MNT/log.txt 2>/dev/null
  umount $MNT 2>/dev/null
  $BIN stop --id 201 --meta $W/v.meta >/dev/null 2>&1
done
echo "RESULT failed=$FAILED/20"
pkill -9 -x blobdev >/dev/null 2>&1
# Exit on the result, not on whatever the last command happened to return:
# pkill exits 1 when there was nothing to kill, which made this step fail in
# the regression while printing a clean 0/20.
[ "$FAILED" -eq 0 ] && echo "PASS: start-stop cycles" || { echo "FAIL: $FAILED of 20 starts"; exit 1; }
