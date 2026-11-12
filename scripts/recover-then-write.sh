#!/bin/bash
# Recover, then keep using the volume, then lose the disk again.
#
# Recovering the metadata proves the volume can be opened. What an operator
# does next is write to it, restart it, and expect the copy to be current
# again -- and then lose the disk again. None of that had been run.
#
# Also checks that recovery writes the local file back: saving only happens
# on an upload, so before this the log said the file would be written again
# and it never was -- every start went to the store, and would have refused
# to open whenever the store was unreachable.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/rw; MNT=/mnt/rw
S3=(--backend s3 --endpoint http://192.168.0.220:9000 --bucket blobtest \
  --access-key blobdev --secret-key blobdevsecret123 --region us-east-1 --path-style)

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
for m in $(grep -oE "^/dev/ublkb[0-9]+ [^ ]+" /proc/mounts 2>/dev/null | awk '{print $2}'); do umount -l "$m" 2>/dev/null; done
rm -rf $W; mkdir -p $W $MNT
V=rw$RANDOM
DEV=/dev/ublkb64

wait_gone() {
  for _ in $(seq 1 120); do [ "$(ls /sys/class/ublk-char 2>/dev/null|wc -l)" -eq 0 ] && return 0; sleep 0.5; done
  return 1
}
start() {
  local log=$1; shift
  $BIN run "${S3[@]}" --volume $V --meta $W/v.meta --id 64 "$@" > $W/$log 2>&1 &
  for _ in $(seq 1 180); do [ -e $DEV ] && break; sleep 1; done
  [ -e $DEV ] || { echo "FAIL: no device ($log)"; grep -v INFO $W/$log | head -3; return 1; }
  return 0
}
stop() {
  pkill -TERM -x blobdev
  for _ in $(seq 1 900); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
  pkill -9 -x blobdev 2>/dev/null
  wait_gone
}

echo "== first life: write a.bin"
start l1 --create-size 1G || exit 1
mkfs.ext4 -q -F $DEV 2>/dev/null
for _ in $(seq 1 120); do mount $DEV $MNT 2>/dev/null && break; sleep 1; done
head -c 16777216 /dev/urandom > $MNT/a.bin
sync
A=$(sha256sum $MNT/a.bin | awk '{print $1}')
umount $MNT
stop

echo "== lose the metadata"
rm -f $W/v.meta
rm -f $W/v.meta-behind

echo "== second life: recover, then write b.bin"
start l2 || exit 1
grep -oE "recovered it from [^ ]*" $W/l2 | head -1
for _ in $(seq 1 120); do mount $DEV $MNT 2>/dev/null && break; sleep 1; done
GOT_A=$(sha256sum $MNT/a.bin 2>/dev/null | awk '{print $1}')
[ "$A" = "$GOT_A" ] && echo "  a.bin survived: yes" || echo "  a.bin survived: NO"
head -c 16777216 /dev/urandom > $MNT/b.bin
sync
B=$(sha256sum $MNT/b.bin | awk '{print $1}')
umount $MNT
stop

echo "== third life: both files, no local metadata again"
rm -f $W/v.meta
start l3 || exit 1
for _ in $(seq 1 120); do mount -o ro $DEV $MNT 2>/dev/null && break; sleep 1; done
GA=$(sha256sum $MNT/a.bin 2>/dev/null | awk '{print $1}')
GB=$(sha256sum $MNT/b.bin 2>/dev/null | awk '{print $1}')
[ "$A" = "$GA" ] && echo "  a.bin identical" || echo "  a.bin DIFFERS"
[ "$B" = "$GB" ] && echo "  b.bin identical" || echo "  b.bin DIFFERS"
umount $MNT 2>/dev/null
pkill -9 -x blobdev
[ "$A" = "$GA" ] && [ "$B" = "$GB" ] && echo "PASS: recover and keep writing" || echo "FAIL"
