#!/bin/bash
# Does the shipped unit actually run a volume, and survive a restart?
#
# The unit is what an operator will use, and it had never been run: the
# sandbox settings stopped the device from being created at all, with nothing
# in the log to say so. This installs the unit, starts a volume through it,
# puts real data through, restarts the service, and checks the data is still
# there under the same device name.
#
# Must run as root on the host. Writes to /etc/blobdev and /var/lib/blobdev.
#
# usage: systemd-check.sh [device-id]
set -u
ID=${1:-3}
VOL=sdcheck
BIN=${BIN:-/usr/local/bin/blobdev}
HERE=$(cd "$(dirname "$0")" && pwd)
MNT=/mnt/sdcheck
W=/var/lib/blobdev

[ "$(id -u)" -eq 0 ] || { echo "FAIL: needs root"; exit 1; }

cleanup() {
  umount "$MNT" 2>/dev/null
  systemctl stop "blobdev@$VOL" 2>/dev/null
  sleep 2
  pkill -9 -x blobdev 2>/dev/null
  "$BIN" reap >/dev/null 2>&1
}
trap cleanup EXIT

cleanup
install -m 0644 "$HERE/../packaging/blobdev@.service" /etc/systemd/system/ 2>/dev/null ||
  cp "$HERE/../packaging/blobdev@.service" /etc/systemd/system/
mkdir -p "$W" /var/cache/blobdev /etc/blobdev "$MNT"
rm -rf "$W/objs" "$W/$VOL.meta" /var/cache/blobdev/*

cat > "/etc/blobdev/$VOL.conf" <<CONF
BLOBDEV_ARGS="--backend dir --dir $W/objs --volume $VOL --meta $W/$VOL.meta \
  --cache-dir /var/cache/blobdev --cache-size 64M --id $ID --create-size 256M"
CONF
chmod 0600 "/etc/blobdev/$VOL.conf"
systemctl daemon-reload

echo "== start"
systemctl start "blobdev@$VOL" || { echo "FAIL: systemctl start"; exit 1; }
sleep 6
[ "$(systemctl is-active "blobdev@$VOL")" = active ] || {
  echo "FAIL: service did not stay active"
  journalctl -u "blobdev@$VOL" --no-pager 2>/dev/null | tail -5
  exit 1
}
DEV=/dev/ublkb$ID
[ -e "$DEV" ] || { echo "FAIL: $DEV was not created"; exit 1; }
echo "  device=$DEV active=yes"

echo "== use it"
mkfs.ext4 -q -F "$DEV" || { echo "FAIL: mkfs"; exit 1; }
for _ in $(seq 1 60); do mount "$DEV" "$MNT" 2>/dev/null && break; sleep 1; done
mountpoint -q "$MNT" || { echo "FAIL: mount"; exit 1; }
head -c 8388608 /dev/urandom > "$MNT/f.bin"
sync
WANT=$(sha256sum "$MNT/f.bin" | awk '{print $1}')
umount "$MNT"
echo "  wrote 8 MiB"

echo "== restart the service"
systemctl restart "blobdev@$VOL"
sleep 6
[ "$(systemctl is-active "blobdev@$VOL")" = active ] || { echo "FAIL: not active after restart"; exit 1; }
[ -e "$DEV" ] || { echo "FAIL: $DEV missing after restart"; exit 1; }
for _ in $(seq 1 60); do mount -o ro "$DEV" "$MNT" 2>/dev/null && break; sleep 1; done
mountpoint -q "$MNT" || { echo "FAIL: remount"; exit 1; }
GOT=$(sha256sum "$MNT/f.bin" 2>/dev/null | awk '{print $1}')
umount "$MNT" 2>/dev/null
[ "$WANT" = "$GOT" ] || { echo "FAIL: data differs after restart (${WANT:0:12} vs ${GOT:0:12})"; exit 1; }
echo "  same device, data intact"

echo "== stop"
systemctl stop "blobdev@$VOL"
sleep 3
[ "$(systemctl is-active "blobdev@$VOL")" = inactive ] && echo "  stopped cleanly"
echo "PASS: unit runs a volume and survives a restart"
