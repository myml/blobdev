#!/bin/bash
# Does WebDAV really write only the dirty range, and is the data right?
#
# WebDAV is declared patch-capable and then falls back on the first refusal,
# so both paths need checking: this one runs against Nextcloud, which refuses
# ranged PUTs, and webdav-range.sh against a server that honours them.
#
# Measured against Nextcloud: 100 distinct 4 KiB writes, all present, one
# "partial write not supported" warning, whole-object uploads after that.
# 108 MiB sent for 0.39 MiB written.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/dav; MNT=/mnt/dav
NC=http://127.0.0.1:18083/remote.php/dav/files/ncadmin/blobdev

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
V=dav$RANDOM

BLOBDEV_SUMMARY_SECS=2 $BIN run --backend webdav --endpoint $NC --user ncadmin \
  --password ncpass12345 --timeout 30 --volume $V --meta $W/v.meta --id 80 \
  --create-size 512M --max-age 2 > $W/l 2>&1 &
for _ in $(seq 1 180); do grep -q "created /dev/ublkb80" $W/l 2>/dev/null && break; sleep 1; done
grep -q "created /dev/ublkb80" $W/l || { echo "FAIL: no device"; tail -3 $W/l; exit 1; }
mkfs.ext4 -q -F /dev/ublkb80 2>/dev/null || { echo "FAIL: mkfs"; exit 1; }
for _ in $(seq 1 120); do mount /dev/ublkb80 $MNT 2>/dev/null && break; sleep 1; done

echo "== write 4 MiB, let it settle"
head -c 4194304 /dev/urandom > $MNT/base.bin
sync
sleep 8

echo "== does the backend claim patch support?"
grep -oE "partial_write=[a-z]+" $W/l | head -1

echo "== one 4 KiB write, then read it back"
head -c 4096 /dev/urandom > $W/blk
dd if=$W/blk of=$MNT/base.bin bs=4096 seek=10 count=1 conv=notrunc status=none 2>/dev/null
sync
sleep 5
if dd if=$MNT/base.bin bs=4096 skip=10 count=1 status=none 2>/dev/null | cmp -s - $W/blk; then
  echo "  read back: identical"
else
  echo "  read back: DIFFERS -- the patch did not land"
fi

echo "== 100 scattered writes, read every one back"
# Distinct blocks: with 100 random choices from 1024, a few collide, and the
# later write legitimately overwrites the earlier one -- comparing against
# the first value then reports a mismatch that is the test's own arithmetic.
BAD=0
for i in $(seq 1 100); do
  head -c 4096 /dev/urandom > $W/p$i
  OFF=$(( (i - 1) * 4096 ))
  dd if=$W/p$i of=$MNT/base.bin bs=4096 seek=$((OFF / 4096)) count=1 conv=notrunc status=none 2>/dev/null
  echo "$OFF" >> $W/offs
done
sync
sleep 6
n=1
while read -r OFF; do
  if ! dd if=$MNT/base.bin bs=4096 skip=$((OFF / 4096)) count=1 status=none 2>/dev/null | cmp -s - $W/p$n; then
    BAD=$((BAD + 1))
  fi
  n=$((n + 1))
done < $W/offs
echo "  mismatched blocks: $BAD / 100"

echo "== store traffic"
grep -oE "sent [0-9.]+ MiB in [0-9]+ upload" $W/l 2>/dev/null | tail -1

umount $MNT 2>/dev/null
$BIN stop --id 80 >/dev/null 2>&1
for _ in $(seq 1 300); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
[ "$BAD" -eq 0 ] && echo "PASS: webdav patches land" || echo "FAIL"
