#!/bin/bash
# Several processes writing the same blocks, and the same object.
#
# The existing concurrency test gives each worker its own file, so nothing
# ever contends for the same block. A filesystem under real load does have
# contention -- journal and data landing in the same region, two processes
# appending to one file -- and that is where a write path that merges dirty
# blocks can lose one.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/cont; MNT=/mnt/cont
S3=(--backend s3 --endpoint http://192.168.0.220:9000 --bucket blobtest \
  --access-key blobdev --secret-key blobdevsecret123 --region us-east-1 --path-style)

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
$BIN run "${S3[@]}" --volume cont$RANDOM --meta $W/v.meta --id 77 --create-size 512M > $W/l 2>&1 &
for _ in $(seq 1 180); do grep -q "created /dev/ublkb77" $W/l 2>/dev/null && break; sleep 1; done
grep -q "created /dev/ublkb77" $W/l || { echo "FAIL: no device"; exit 1; }
mkfs.ext4 -q -F /dev/ublkb77 2>/dev/null || { echo "FAIL: mkfs"; exit 1; }
for _ in $(seq 1 120); do mount /dev/ublkb77 $MNT 2>/dev/null && break; sleep 1; done

# Four processes pound the same 4 MiB region of one file.
head -c 4194304 /dev/zero > $MNT/shared.bin
sync
SECONDS_TO_RUN=${SECONDS_TO_RUN:-60}
end=$(( $(date +%s) + SECONDS_TO_RUN ))
WPIDS=""
for w in 1 2 3 4; do
  (
    bad=0; rounds=0
    while [ "$(date +%s)" -lt "$end" ]; do
      # Each worker owns 1 MiB of the shared file, so writes interleave in
      # the same objects without any of them overwriting another's bytes.
      dd if=/dev/urandom of=$MNT/shared.bin bs=1M seek=$((w - 1)) count=1 \
         conv=notrunc status=none 2>/dev/null || bad=$((bad+1))
      rounds=$((rounds+1))
    done
    echo "worker $w: rounds=$rounds bad=$bad" > $W/w$w.out
  ) &
  WPIDS="$WPIDS $!"
done
for p in $WPIDS; do wait "$p"; done

echo "== after the writes"
for w in 1 2 3 4; do cat $W/w$w.out 2>/dev/null; done
sync
sleep 3

echo "== every worker's region must match what it wrote last"
# Re-run deterministically: write known patterns, one worker per MiB.
for w in 1 2 3 4; do
  head -c 1048576 /dev/urandom > $W/pat$w.bin
done
for w in 1 2 3 4; do
  dd if=$W/pat$w.bin of=$MNT/shared.bin bs=1M seek=$((w - 1)) count=1 \
     conv=notrunc status=none 2>/dev/null
done
sync
sleep 2
BAD=0
for w in 1 2 3 4; do
  if cmp -s <(dd if=$MNT/shared.bin bs=1M skip=$((w - 1)) count=1 status=none 2>/dev/null) $W/pat$w.bin; then
    echo "  worker $w region: identical"
  else
    echo "  worker $w region: DIFFERS"
    BAD=$((BAD + 1))
  fi
done

echo "== and after a restart"
umount $MNT 2>/dev/null
$BIN stop --id 77 >/dev/null 2>&1
# Wait for the process to be gone, not just for the device: the daemon holds
# the metadata lock until it exits, and starting while it does fails with
# "another blobdev process is already using this volume". That refusal is
# correct; the test has to wait for it to stop being true.
for _ in $(seq 1 300); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
for _ in $(seq 1 120); do [ "$(ls /sys/class/ublk-char 2>/dev/null|wc -l)" -eq 0 ] && break; sleep 0.5; done
V=$(grep -oE "cont[0-9]+" $W/l | head -1)
$BIN run "${S3[@]}" --volume $V --meta $W/v.meta --id 77 > $W/l2 2>&1 &
for _ in $(seq 1 180); do grep -q "created /dev/ublkb77" $W/l2 2>/dev/null && break; sleep 1; done
for _ in $(seq 1 120); do mount -o ro /dev/ublkb77 $MNT 2>/dev/null && break; sleep 1; done
for w in 1 2 3 4; do
  if cmp -s <(dd if=$MNT/shared.bin bs=1M skip=$((w - 1)) count=1 status=none 2>/dev/null) $W/pat$w.bin; then
    echo "  worker $w after restart: identical"
  else
    echo "  worker $w after restart: DIFFERS"
    BAD=$((BAD + 1))
  fi
done
umount $MNT 2>/dev/null
$BIN stop --id 77 >/dev/null 2>&1
echo "BAD=$BAD"
[ "$BAD" -eq 0 ] && echo "PASS: contended writes" || echo "FAIL"
