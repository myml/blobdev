#!/bin/bash
# blobdev against a real Nextcloud instance, including a restart.
#
# Nextcloud differs from a plain WebDAV server in ways that broke blobdev:
#   - MKCOL on a missing parent answers 409 (a plain server would 404);
#   - MKCOL on an existing collection answers 405;
#   - a ranged PUT answers 400, so writes fall back to whole objects.
#
# The overwrite is deliberate: creating an object needs no patch, and the
# 400/fallback path only runs when an existing object is modified.
#
# Start one with:
#   podman run -d --name nextcloud -p 18083:80 \
#     -e SQLITE_DATABASE=nc -e NEXTCLOUD_ADMIN_USER=ncadmin \
#     -e NEXTCLOUD_ADMIN_PASSWORD=ncpass12345 \
#     -v /srv/nc/data:/var/www/html:Z nextcloud:stable-apache
#
# usage: NC_URL=http://127.0.0.1:18083 nextcloud-e2e.sh
set +e
SPACE_DIR=$(dirname "${W:-${ROOT:-/var/tmp}}")
mkdir -p "$SPACE_DIR" 2>/dev/null
AVAIL_KB=$(df -Pk "$SPACE_DIR" 2>/dev/null | awk 'NR==2 {print $4}')
if [ -n "${AVAIL_KB:-}" ] && [ "$AVAIL_KB" -lt $((1024 * 1024)) ]; then
  echo "FAIL: less than 1 GiB free in $SPACE_DIR" >&2
  df -h "$SPACE_DIR" >&2
  exit 1
fi
BIN=${BIN:-/usr/local/bin/blobdev}
NC_URL=${NC_URL:-http://127.0.0.1:18083}
NC_USER=${NC_USER:-ncadmin}
NC_PASS=${NC_PASS:-ncpass12345}
NC="$NC_URL/remote.php/dav/files/$NC_USER/blobdev"
pkill -9 -x blobdev; sleep 2; umount /mnt/nc 2>/dev/null
# Disk, not /tmp: on the test host /tmp is a tmpfs shared with other work, and
# a run needs a few hundred megabytes.
W=${W:-/var/tmp/bd-nextcloud}
rm -rf "$W"; mkdir -p "$W"
V=ncr$RANDOM
start() {
  local log=$1; shift
  $BIN run --backend webdav --endpoint "$NC" --user "$NC_USER" --password "$NC_PASS" \
    --volume $V --meta $W/v.meta --id -1 "$@" > "$W/$log" 2>&1 &
  DEV=""
  for i in $(seq 1 80); do
    DEV=$(grep -oE "/dev/ublkb[0-9]+" "$W/$log" | head -1)
    [ -n "$DEV" ] && [ -e "$DEV" ] && break
    sleep 0.5
  done
  [ -n "$DEV" ] && [ -e "$DEV" ]
}
start log1 --create-size 256M || { echo "FAIL: no device"; tail -5 $W/log1; exit 1; }
echo "device=$DEV"
mkdir -p /mnt/nc
mkfs.ext4 -q -F $DEV || { echo "FAIL mkfs"; exit 1; }
mount $DEV /mnt/nc || { echo "FAIL mount"; exit 1; }
dd if=/dev/urandom of=/mnt/nc/a.bin bs=1M count=24 status=none
dd if=/dev/urandom of=/mnt/nc/a.bin bs=4096 seek=50 count=1 conv=notrunc status=none
sync
S1=$(sha256sum /mnt/nc/a.bin | awk '{print $1}')
echo "written=${S1:0:16}"
umount /mnt/nc || { echo "FAIL umount"; exit 1; }
pkill -TERM -x blobdev
for i in $(seq 1 200); do pgrep -x blobdev >/dev/null || break; sleep 0.5; done
pgrep -x blobdev >/dev/null && { echo "FAIL: daemon still running"; pkill -9 -x blobdev; exit 1; }
echo "daemon exited"
start log2 || { echo "FAIL: no device on restart"; tail -5 $W/log2; exit 1; }
echo "restart device=$DEV"
mount -o ro $DEV /mnt/nc || { echo "FAIL remount"; exit 1; }
S2=$(sha256sum /mnt/nc/a.bin | awk '{print $1}')
umount /mnt/nc 2>/dev/null
echo "readback=${S2:0:16}"
[ "$S1" = "$S2" ] && echo "PASS: clean restart round trip" || echo "FAIL: differs"
pkill -9 -x blobdev
