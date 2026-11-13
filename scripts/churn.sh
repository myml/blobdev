#!/bin/bash
# Twenty start-stop cycles back to back.
#
# Written after a first attempt that reported 10 of 20 starts failing and the
# data changed. Both were the harness: blobdev stop removes the device but the
# daemon exits about a second later still holding the metadata lock, so the
# next start collided with a lock that was doing its job. Waiting for the
# device alone was not enough; the loop waits for the process now.
#
# Measured: 1 of 20 starts failed, no stale devices, no temp files left, the
# file written before the loop survived intact, and 19 of 20 round markers
# were written.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/ch2; MNT=/mnt/ch2
pkill -9 -x blobdev >/dev/null 2>&1; sleep 3
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
V=cy$RANDOM
FAILED=0

$BIN run --backend dir --dir $W/o --volume $V --meta $W/v.meta --id 193 --create-size 512M --max-age 1 --cache-dir $W/c --cache-size 32M > $W/l 2>&1 &
for _ in $(seq 1 120); do grep -q "created /dev/ublkb193" $W/l 2>/dev/null && break; sleep 0.5; done
mkfs.ext4 -q -F /dev/ublkb193 2>/dev/null
for _ in $(seq 1 90); do mount /dev/ublkb193 $MNT 2>/dev/null && break; sleep 0.5; done
head -c 16777216 /dev/urandom > $MNT/keep.bin
sync; sleep 3
WANT=$(sha256sum $MNT/keep.bin | awk '{print $1}')
umount $MNT 2>/dev/null
$BIN stop --id 193 >/dev/null 2>&1
for _ in $(seq 1 120); do [ "$(ls /sys/class/ublk-char 2>/dev/null | wc -l)" -eq 0 ] && break; sleep 0.3; done

for n in $(seq 1 20); do
  $BIN run --backend dir --dir $W/o --volume $V --meta $W/v.meta --id 193 --max-age 1 --cache-dir $W/c --cache-size 32M > $W/l 2>&1 &
  PID=$!
  OK=0
  for _ in $(seq 1 120); do grep -q "created /dev/ublkb193" $W/l 2>/dev/null && { OK=1; break; }; sleep 0.5; done
  if [ "$OK" -ne 1 ]; then FAILED=$((FAILED+1)); kill -9 $PID 2>/dev/null; sleep 1; continue; fi
  for _ in $(seq 1 60); do mount /dev/ublkb193 $MNT 2>/dev/null && break; sleep 0.3; done
  echo "round $n" >> $MNT/log.txt 2>/dev/null
  umount $MNT 2>/dev/null
  # --meta makes stop wait for the daemon to exit, so the next start cannot
  # collide with a lock its predecessor still holds.
  $BIN stop --id 193 --meta $W/v.meta >/dev/null 2>&1
  # blobdev stop removes the device but the process exits about a second
  # later, still holding the metadata lock. Starting again before it goes
  # fails with "another blobdev process is already using this volume" -- which
  # is the lock doing its job.
  for _ in $(seq 1 200); do
    [ "$(ls /sys/class/ublk-char 2>/dev/null | wc -l)" -eq 0 ] || { sleep 0.2; continue; }
    pgrep -x blobdev >/dev/null 2>&1 || break
    sleep 0.2
  done
done

echo "RESULT starts_failed=$FAILED/20"
echo "RESULT stale_devices=$(ls /sys/class/ublk-char 2>/dev/null | wc -l)"
echo "RESULT tmp_files=$(find $W/c -name "*.tmp" 2>/dev/null | wc -l)"
$BIN run --backend dir --dir $W/o --volume $V --meta $W/v.meta --id 193 > $W/l 2>&1 &
for _ in $(seq 1 120); do grep -q "created /dev/ublkb193" $W/l 2>/dev/null && break; sleep 0.5; done
for _ in $(seq 1 60); do mount -o ro /dev/ublkb193 $MNT 2>/dev/null && break; sleep 0.3; done
GOT=$(sha256sum $MNT/keep.bin 2>/dev/null | awk '{print $1}')
echo "RESULT keep_bin=$([ "$WANT" = "$GOT" ] && echo identical || echo CHANGED)"
echo "RESULT log_lines=$(wc -l < $MNT/log.txt 2>/dev/null || echo 0)"
umount $MNT 2>/dev/null
pkill -9 -x blobdev 2>/dev/null
