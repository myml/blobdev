#!/bin/bash
# One Nextcloud round trip, fully serialised, with every step checked.
#
# The same volume is written, unmounted, the daemon stopped, restarted and read
# back. Every wait is explicit: the device must be gone before the next one is
# created, and the daemon must have exited before its replacement starts,
# because a device id still held by the kernel makes the *next* mount fail in a
# way that looks like data loss.
#
# usage: nextcloud-one.sh [MiB] [volume-size]
set -u
BIN=${BIN:-/usr/local/bin/blobdev}
NC_URL=${NC_URL:-http://127.0.0.1:18083}
NC_USER=${NC_USER:-ncadmin}
NC_PASS=${NC_PASS:-ncpass12345}
NC="$NC_URL/remote.php/dav/files/$NC_USER/blobdev"
MIB=${1:-24}
VSIZE=${2:-256M}
# Disk, not /tmp: these runs write hundreds of megabytes and /tmp is a tmpfs on
# the test host, shared with other work.
W=${W:-/root/bd-test/one}
MNT=${MNT:-/mnt/nc}

step() { echo "  $*"; }
die() { echo "FAIL: $*" >&2; exit 1; }

wait_for() {
  # wait_for <tries> <cmd...>: succeed as soon as cmd does.
  local tries=$1; shift
  for _ in $(seq 1 "$tries"); do "$@" && return 0; sleep 0.5; done
  return 1
}

device_node() {
  # The block node for the device mentioned in a log, once it exists.
  local dev
  dev=$(grep -oE "/dev/ublkb[0-9]+" "$1" 2>/dev/null | head -1)
  [ -n "$dev" ] && [ -e "$dev" ] && { echo "$dev"; return 0; }
  return 1
}

clean() {
  umount -l "$MNT" 2>/dev/null
  pkill -9 -x blobdev 2>/dev/null
  sleep 2
  "$BIN" reap >/dev/null 2>&1
}

AVAIL_KB=$(df -Pk "$(dirname "$W")" | awk 'NR==2 {print $4}')
[ "$AVAIL_KB" -gt $((1024 * 1024)) ] || die "less than 1 GiB free under $(dirname "$W")"

clean
rm -rf "$W"; mkdir -p "$W" "$MNT"
V=one$MIB$RANDOM
step "volume=$V size=$VSIZE data=${MIB}MiB"

start() {
  local log=$1; shift
  "$BIN" run --backend webdav --endpoint "$NC" \
      --user "$NC_USER" --password "$NC_PASS" --volume "$V" --meta "$W/v.meta" \
      --id -1 "$@" > "$W/$log" 2>&1 &
  local pid=$!
  local dev=""
  for _ in $(seq 1 120); do
    dev=$(grep -oE "/dev/ublkb[0-9]+" "$W/$log" 2>/dev/null | head -1)
    if [ -n "$dev" ] && [ -e "$dev" ]; then
      # The node exists before the device answers: probing it with a read is
      # what makes the difference between "appeared" and "ready".
      if dd if="$dev" of=/dev/null bs=4096 count=1 iflag=direct status=none 2>/dev/null; then
        echo "$dev"; return 0
      fi
    fi
    kill -0 "$pid" 2>/dev/null || { echo "daemon exited during startup" >&2; tail -5 "$W/$log" >&2; return 1; }
    sleep 0.5
  done
  echo "device never appeared" >&2
  tail -5 "$W/$log" >&2
  return 1
}

DEV1=$(start log1 --create-size "$VSIZE") || die "start 1"
step "device 1: $DEV1"

mkfs.ext4 -q -F "$DEV1" || die "mkfs"
mount "$DEV1" "$MNT" || die "mount"

dd if=/dev/urandom of="$W/src.bin" bs=1M count="$MIB" status=none || die "generate"
cp "$W/src.bin" "$MNT/a.bin" || die "copy in"
sync
step "written"

cmp -s "$MNT/a.bin" "$W/src.bin" || die "read back inside the mount differs"
step "in-mount comparison: identical"

SRC=$(sha256sum "$W/src.bin" | awk '{print $1}')

umount "$MNT" || die "umount"
mountpoint -q "$MNT" && die "mount point still busy"
step "unmounted"

pkill -TERM -x blobdev
for _ in $(seq 1 400); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
pgrep -x blobdev >/dev/null && { pkill -9 -x blobdev; die "daemon did not exit"; }
step "daemon exited"
sleep 3

DEV2=$(start log2) || die "start 2"
step "device 2: $DEV2"

# The device answers reads as soon as it appears, but a read-only mount also
# has to fetch the superblock through the cache; give it room rather than
# reporting a data problem when the store is simply slow.
sleep 3
for _ in $(seq 1 120); do mount -o ro "$DEV2" "$MNT" 2>/dev/null && break; sleep 0.5; done
mountpoint -q "$MNT" || { tail -5 "$W/log2" >&2; die "remount"; }

GOT=$(sha256sum "$MNT/a.bin" | awk '{print $1}')
umount "$MNT" 2>/dev/null
step "expected=${SRC:0:16} got=${GOT:0:16}"
[ "$SRC" = "$GOT" ] || die "data differs after restart"
echo "PASS: ${MIB} MiB round trip through Nextcloud"
