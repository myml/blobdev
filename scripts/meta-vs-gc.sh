#!/bin/bash
# The maintenance commands share the store with a metadata copy.
#
# `gc` deletes objects no chunk points at. The copy lives at <volume>/meta,
# outside the <volume>/c/ prefix it scans -- but that is a claim about the
# code, and `gc` deleting the copy would be silent data loss for the next
# recovery. `ls` must not count it as data either, and `inspect` and `gc`
# must still be able to answer when the local file is gone, since that is
# when they are worth running.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/mo; MNT=/mnt/mo
S3=(--backend s3 --endpoint http://192.168.0.220:9000 --bucket blobtest \
  --access-key blobdev --secret-key blobdevsecret123 --region us-east-1 --path-style)

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
V=mo$RANDOM

$BIN run "${S3[@]}" --volume $V --meta $W/v.meta --id 71 --create-size 1G > $W/l1 2>&1 &
for _ in $(seq 1 120); do [ -e /dev/ublkb71 ] && break; sleep 1; done
mkfs.ext4 -q -F /dev/ublkb71 2>/dev/null
for _ in $(seq 1 120); do mount /dev/ublkb71 $MNT 2>/dev/null && break; sleep 1; done
head -c 16777216 /dev/urandom > $MNT/a.bin
sync
A=$(sha256sum $MNT/a.bin | awk '{print $1}')
umount $MNT
pkill -TERM -x blobdev
for _ in $(seq 1 900); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
pkill -9 -x blobdev 2>/dev/null
for _ in $(seq 1 60); do [ "$(ls /sys/class/ublk-char 2>/dev/null|wc -l)" -eq 0 ] && break; sleep 1; done
$BIN reap >/dev/null 2>&1

echo "== ls"
$BIN ls "${S3[@]}" --volume $V 2>&1 | tail -2
echo "== does ls count the copy?"
$BIN ls "${S3[@]}" --volume $V 2>&1 | grep -c "/meta" 

echo "== gc, twice"
$BIN gc "${S3[@]}" --volume $V --meta $W/v.meta 2>&1 | head -2
$BIN gc "${S3[@]}" --volume $V --meta $W/v.meta 2>&1 | head -2

echo "== the copy survived gc?"
rm -f $W/v.meta
$BIN run "${S3[@]}" --volume $V --meta $W/v.meta --id 71 > $W/l2 2>&1 &
for _ in $(seq 1 120); do [ -e /dev/ublkb71 ] && break; sleep 1; done
if [ -e /dev/ublkb71 ]; then
  echo "  recovery: $(grep -oE "recovered it from [^ ]*" $W/l2 | head -1)"
  for _ in $(seq 1 120); do mount -o ro /dev/ublkb71 $MNT 2>/dev/null && break; sleep 1; done
  GOT=$(sha256sum $MNT/a.bin 2>/dev/null | awk '{print $1}')
  [ "$A" = "$GOT" ] && echo "  data identical after gc" || echo "  DATA LOST after gc"
  umount $MNT 2>/dev/null
else
  echo "  RECOVERY FAILED: the copy is gone"
  grep -v INFO $W/l2 | head -2
fi
pkill -9 -x blobdev 2>/dev/null
