#!/bin/bash
# What the disk cache buys across a restart, and is it the right data.
#
# Zero store requests only proves the store was not asked; the checksum
# proves the answer was correct. Measured on Nextcloud with a 32 MiB file:
#
#   warm cache   0 store requests   data identical
#   cold cache   8 store requests   data identical
#
# --read-chunks 1 keeps the memory cache to one object, so anything served
# without a request came from the disk cache. Needs the fault proxy on
# 19090 pointing at Nextcloud, because that is what counts the requests:
# a proxy cannot forward S3's SigV4 signature.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/dcv; MNT=/mnt/dcv
NC=http://127.0.0.1:19090/remote.php/dav/files/ncadmin/blobdev
stats() { curl -s --max-time 5 http://127.0.0.1:19090/stats 2>/dev/null | grep -oE "^requests=[0-9]+" | cut -d= -f2; }

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
V=dcv$RANDOM

start() {
  local log=$1; shift
  $BIN run --backend webdav --endpoint $NC --user ncadmin --password ncpass12345 \
    --timeout 20 --volume $V --meta $W/v.meta --id 86 --cache-dir $W/cache \
    --cache-size 128M --read-chunks 1 --max-age 2 "$@" > $W/$log 2>&1 &
  for _ in $(seq 1 180); do grep -q "created /dev/ublkb86" $W/$log 2>/dev/null && break; sleep 1; done
  grep -q "created /dev/ublkb86" $W/$log || { echo "FAIL: no device ($log)"; tail -3 $W/$log; return 1; }
}
stopit() {
  umount $MNT 2>/dev/null
  $BIN stop --id 86 >/dev/null 2>&1
  for _ in $(seq 1 300); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
  for _ in $(seq 1 120); do [ "$(ls /sys/class/ublk-char 2>/dev/null|wc -l)" -eq 0 ] && break; sleep 0.5; done
}

start l1 --create-size 256M || exit 1
mkfs.ext4 -q -F /dev/ublkb86 2>/dev/null
for _ in $(seq 1 120); do mount /dev/ublkb86 $MNT 2>/dev/null && break; sleep 1; done
head -c 33554432 /dev/urandom > $MNT/big.bin
sync
WANT=$(sha256sum $MNT/big.bin | awk '{print $1}')
echo "WANT=$WANT"
# Fill the disk cache.
cat $MNT/big.bin > /dev/null
sync
stopit

echo "== warm cache, verified"
start l2 || exit 1
for _ in $(seq 1 120); do mount -o ro /dev/ublkb86 $MNT 2>/dev/null && break; sleep 1; done
R0=$(stats)
cat $MNT/big.bin > /dev/null
R1=$(stats)
got=$(sha256sum $MNT/big.bin | awk '{print $1}')
echo "  requests: $((R1 - R0))"
echo "  data: $([ "$WANT" = "$got" ] && echo identical || echo DIFFERS)"
stopit

echo "== deleting the cache must change nothing but the traffic"
rm -rf $W/cache
start l3 || exit 1
for _ in $(seq 1 120); do mount -o ro /dev/ublkb86 $MNT 2>/dev/null && break; sleep 1; done
R0=$(stats)
cat $MNT/big.bin > /dev/null
R1=$(stats)
got=$(sha256sum $MNT/big.bin | awk '{print $1}')
echo "  requests: $((R1 - R0))"
echo "  data: $([ "$WANT" = "$got" ] && echo identical || echo DIFFERS)"
stopit
