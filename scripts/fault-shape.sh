#!/bin/bash
# How does blobdev behave when the store refuses requests permanently?
#
# A 400 means the request is wrong; repeating it cannot help. The daemon must
# give up at once, report the error, and -- importantly -- must not spin: the
# previous generation of this code asked the store again on every tick for as
# long as it ran.
#
# usage: fault-shape.sh <profile> <every> [seconds-to-watch]
set -u
PROFILE=${1:?profile}
EVERY=${2:-2}
WATCH=${3:-60}
BIN=${BIN:-/usr/local/bin/blobdev}
W=/root/bd-test/shape
MNT=/mnt/nc
NC="http://127.0.0.1:19090/remote.php/dav/files/ncadmin/blobdev"

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
umount -l $MNT 2>/dev/null
rm -rf $W; mkdir -p $W $MNT
V=shape$RANDOM

$BIN run --backend webdav --endpoint "$NC" --user ncadmin --password ncpass12345 \
  --volume $V --meta $W/v.meta --id -1 --retries 4 --timeout 10 \
  --create-size 128M > $W/log 2>&1 &
for i in $(seq 1 120); do
  D=$(grep -oE "/dev/ublkb[0-9]+" $W/log 2>/dev/null | head -1)
  if [ -n "${D:-}" ] && [ -e "$D" ]; then
    dd if="$D" of=/dev/null bs=4096 count=1 iflag=direct status=none 2>/dev/null && break
  fi
  sleep 0.5
done

BEFORE=$(curl -s --max-time 5 http://127.0.0.1:19090/stats 2>/dev/null | grep -E '^requests=' | cut -d= -f2)
START=$(date +%s)
mkfs.ext4 -q -F "$D" > $W/mkfs.out 2>&1
MKFS_RC=$?
END=$(date +%s)
sleep 2
AFTER=$(curl -s --max-time 5 http://127.0.0.1:19090/stats 2>/dev/null | grep -E '^requests=' | cut -d= -f2)

echo "MKFS_RC=$MKFS_RC"
echo "MKFS_SECONDS=$((END - START))"
echo "REQUESTS=$(( ${AFTER:-0} - ${BEFORE:-0} ))"
echo "RETRY_LINES=$(grep -ci 'retrying' $W/log 2>/dev/null || echo 0)"
echo "ERROR_LINES=$(grep -ci 'error' $W/log 2>/dev/null || echo 0)"

# Watch for a while: a daemon that keeps asking is a daemon that will fill
# the log and the network with doomed requests.
W1=$(curl -s --max-time 5 http://127.0.0.1:19090/stats 2>/dev/null | grep -E '^requests=' | cut -d= -f2)
sleep "$WATCH"
W2=$(curl -s --max-time 5 http://127.0.0.1:19090/stats 2>/dev/null | grep -E '^requests=' | cut -d= -f2)
echo "IDLE_REQUESTS=$(( ${W2:-0} - ${W1:-0} ))"
echo "IDLE_SECONDS=$WATCH"
pkill -9 -x blobdev
