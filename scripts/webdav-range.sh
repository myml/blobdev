#!/bin/bash
# A WebDAV server that honours Content-Range: does the ranged path hold up?
#
# Measured: 100 distinct 4 KiB writes, all present, 0 patch refusals,
# 2.40 MiB sent for 0.39 MiB written -- 6x, against 277x on Nextcloud,
# which refuses ranged PUTs. Whether the server supports Content-Range
# is the single biggest factor in what a WebDAV volume costs to write.
#
# Nextcloud refuses ranged PUTs, so the fallback is what gets exercised there.
# This runs the same measurement against a server that honours them, which is
# the case the ranged path exists for.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/davr; MNT=/mnt/davr
DAV=http://127.0.0.1:18080

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
V=davr$RANDOM

BLOBDEV_SUMMARY_SECS=2 $BIN run --backend webdav --endpoint $DAV --timeout 20 \
  --volume $V --meta $W/v.meta --id 81 --create-size 512M --max-age 2 > $W/l 2>&1 &
for _ in $(seq 1 180); do grep -q "created /dev/ublkb81" $W/l 2>/dev/null && break; sleep 1; done
grep -q "created /dev/ublkb81" $W/l || { echo "FAIL: no device"; tail -3 $W/l; exit 1; }
mkfs.ext4 -q -F /dev/ublkb81 2>/dev/null || { echo "FAIL: mkfs"; exit 1; }
for _ in $(seq 1 120); do mount /dev/ublkb81 $MNT 2>/dev/null && break; sleep 1; done

head -c 4194304 /dev/urandom > $MNT/base.bin
sync; sleep 6
S0=$(grep -oE "sent [0-9.]+ MiB" $W/l 2>/dev/null | tail -1 | awk '{printf "%.1f", $2}')

BAD=0
for i in $(seq 1 100); do
  head -c 4096 /dev/urandom > $W/p$i
  OFF=$(( (i - 1) * 4096 ))
  dd if=$W/p$i of=$MNT/base.bin bs=4096 seek=$((OFF / 4096)) count=1 conv=notrunc status=none 2>/dev/null
  echo "$OFF" >> $W/offs
done
sync; sleep 8
S1=$(grep -oE "sent [0-9.]+ MiB" $W/l 2>/dev/null | tail -1 | awk '{printf "%.1f", $2}')

n=1
while read -r OFF; do
  dd if=$MNT/base.bin bs=4096 skip=$((OFF / 4096)) count=1 status=none 2>/dev/null | cmp -s - $W/p$n || BAD=$((BAD + 1))
  n=$((n + 1))
done < $W/offs

echo "  mismatched blocks: $BAD / 100"
echo "  app wrote:  $(awk "BEGIN{printf \"%.2f\", 100*4096/1048576}") MiB"
echo "  store recv: $(awk "BEGIN{printf \"%.2f\", $S1 - $S0}") MiB"
echo "  patch refusals: $(grep -c "partial write not supported" $W/l 2>/dev/null)"
umount $MNT 2>/dev/null
$BIN stop --id 81 >/dev/null 2>&1
[ "$BAD" -eq 0 ] && echo "PASS: ranged writes land" || echo "FAIL"
