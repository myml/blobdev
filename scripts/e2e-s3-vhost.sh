#!/bin/bash
# Virtual-host addressing end to end.
#
# AWS S3 and any deployment with wildcard DNS address the bucket in the host
# (`bucket.endpoint`) rather than the path. That changes both the canonical
# URI and the signed Host header, so it needs its own run: a signature that is
# only correct for path-style fails here with SignatureDoesNotMatch.
#
# Needs DNS: either wildcard DNS for the endpoint, or an /etc/hosts entry such
#   as `127.0.0.1 blobtest.127.0.0.1` for a local server. It also needs a
#   server that resolves the bucket from the Host header; RustFS does not, so
#   scripts/vhost_s3_server.py exists for exactly this case:
#
#     python3 scripts/vhost_s3_server.py 18090 /tmp/vhostroot .127.0.0.1 &
#     EP=http://127.0.0.1:18090 e2e-s3-vhost.sh
#
# usage: EP=http://127.0.0.1:9000 e2e-s3-vhost.sh
set +e
BIN=/usr/local/bin/blobdev
EP=${EP:-http://127.0.0.1:9000}
AK=blobdev; SK=blobdevsecret123; BKT=blobtest
V=vh$RANDOM
pkill -9 -x blobdev; sleep 2; umount /mnt/vh 2>/dev/null
rm -rf /tmp/vhwork; mkdir -p /tmp/vhwork
DEV=""
start() {
  $BIN run --backend s3 --endpoint "$EP" --bucket $BKT --access-key $AK --secret-key $SK \
    --virtual-host-style \
    --volume $V --meta /tmp/vhwork/v.meta --id -1 "$@" > /tmp/vhwork/log 2>&1 &
  DEV=""
  for i in $(seq 1 60); do
    DEV=$(grep -oE "/dev/ublkb[0-9]+" /tmp/vhwork/log | head -1); [ -n "$DEV" ] && break; sleep 0.5
  done
  [ -n "$DEV" ] || { echo "FAIL: no device"; tail -5 /tmp/vhwork/log; exit 1; }
}
start --create-size 512M
mkdir -p /mnt/vh
mkfs.ext4 -q -F $DEV || { echo "FAIL mkfs"; exit 1; }
mount $DEV /mnt/vh || { echo "FAIL mount"; exit 1; }
# 120 MiB spans 30 objects of 4 MiB.
dd if=/dev/urandom of=/mnt/vh/big.bin bs=1M count=120 status=none
S1=$(sha256sum /mnt/vh/big.bin | awk '{print $1}')
sync
umount /mnt/vh || { echo "FAIL umount"; exit 1; }
pkill -TERM -x blobdev; sleep 3
start
mount -o ro $DEV /mnt/vh || { echo "FAIL remount"; exit 1; }
S2=$(sha256sum /mnt/vh/big.bin | awk '{print $1}')
umount /mnt/vh 2>/dev/null
[ "$S1" = "$S2" ] || { echo "FAIL: $S1 vs $S2"; exit 1; }
N=$($BIN ls --backend s3 --endpoint "$EP" --bucket $BKT --access-key $AK --secret-key $SK \
  --virtual-host-style --volume $V 2>/dev/null | tail -1)
echo "   objects: $N"
echo "PASS: virtual-host addressing, 120 MiB across objects"
