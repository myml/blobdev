#!/bin/bash
# blobdev against a real Nextcloud instance.
#
# Nextcloud is the WebDAV deployment people actually run, and it differs from
# a plain WebDAV server in ways that broke blobdev:
#   - MKCOL on a missing parent answers 409, not 404;
#   - MKCOL on an existing collection answers 405;
#   - a ranged PUT answers 400, so writes fall back to whole objects.
#
# Start one with:
#   podman run -d --name nextcloud -p 18083:80 \
#     -e SQLITE_DATABASE=nc -e NEXTCLOUD_ADMIN_USER=ncadmin \
#     -e NEXTCLOUD_ADMIN_PASSWORD=ncpass12345 \
#     -v /srv/nc/data:/var/www/html:Z nextcloud:stable-apache
#
# usage: NC_URL=http://127.0.0.1:18083 nextcloud-e2e.sh
set +e
BIN=/usr/local/bin/blobdev
NC_URL=${NC_URL:-http://127.0.0.1:18083}
NC_USER=${NC_USER:-ncadmin}
NC_PASS=${NC_PASS:-ncpass12345}
NC="$NC_URL/remote.php/dav/files/$NC_USER/blobdev"
pkill -9 -x blobdev; sleep 2; umount /mnt/nc 2>/dev/null
# /tmp is often a tmpfs on test hosts.
AVAIL_KB=$(df -Pk /tmp 2>/dev/null | awk 'NR==2 {print $4}')
if [ -n "${AVAIL_KB:-}" ] && [ "$AVAIL_KB" -lt $((1024 * 1024)) ]; then
  echo "FAIL: less than 1 GiB free in /tmp" >&2
  exit 1
fi
rm -rf /tmp/ncw; mkdir -p /tmp/ncw
V=nc$RANDOM
$BIN run --backend webdav --endpoint "$NC" --user "$NC_USER" --password "$NC_PASS" \
  --volume $V --create-size 256M --meta /tmp/ncw/v.meta --id -1 > /tmp/ncw/log 2>&1 &
DEV=""
for i in $(seq 1 60); do DEV=$(grep -oE "/dev/ublkb[0-9]+" /tmp/ncw/log | head -1); [ -n "$DEV" ] && break; sleep 0.5; done
if [ -z "$DEV" ]; then echo "FAIL: no device"; tail -6 /tmp/ncw/log; exit 1; fi
echo "device=$DEV"
mkdir -p /mnt/nc
mkfs.ext4 -q -F $DEV || { echo "FAIL mkfs"; exit 1; }
mount $DEV /mnt/nc || { echo "FAIL mount"; exit 1; }
dd if=/dev/urandom of=/mnt/nc/a.bin bs=1M count=32 status=none
S1=$(sha256sum /mnt/nc/a.bin | awk '{print $1}')
sync
sync
cmp /mnt/nc/a.bin <(dd if=/dev/zero bs=1M count=32 status=none) >/dev/null 2>&1
GOT=$(sha256sum /mnt/nc/a.bin | awk '{print $1}')
echo "written=${S1:0:16} read=${GOT:0:16}"
[ "$S1" = "$GOT" ] || { echo "FAIL: immediate readback differs"; }
echo "== 改写已有数据（触发 patch → 400 → 整传回退）"
dd if=/dev/urandom of=/mnt/nc/a.bin bs=4096 seek=100 count=1 conv=notrunc status=none
sync; sync
S2=$(sha256sum /mnt/nc/a.bin | awk '{print $1}')
umount /mnt/nc || { echo "FAIL umount"; exit 1; }
pkill -TERM -x blobdev; sleep 3
$BIN run --backend webdav --endpoint "$NC" --user "$NC_USER" --password "$NC_PASS" \
  --volume $V --meta /tmp/ncw/v.meta --id -1 > /tmp/ncw/log2 2>&1 &
DEV=""
for i in $(seq 1 60); do DEV=$(grep -oE "/dev/ublkb[0-9]+" /tmp/ncw/log2 | head -1); [ -n "$DEV" ] && break; sleep 0.5; done
mount -o ro $DEV /mnt/nc || { echo "FAIL remount"; exit 1; }
S3=$(sha256sum /mnt/nc/a.bin | awk '{print $1}')
umount /mnt/nc 2>/dev/null
echo "expected=${S2:0:16} after-restart=${S3:0:16}"
[ "$S2" = "$S3" ] && echo "PASS: Nextcloud round trip including overwrite" || echo "FAIL: data differs after restart"
echo "== 400 回退次数: $(grep -c "does not support\|not supported" /tmp/ncw/log /tmp/ncw/log2 2>/dev/null | awk -F: '{s+=$2} END {print s}')"
pkill -9 -x blobdev
