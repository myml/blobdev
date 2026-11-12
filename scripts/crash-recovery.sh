#!/bin/bash
# Kill the daemon dead, then see what the filesystem thinks of it.
#
# `kill -9` is the crude version of a power cut: the process vanishes with no
# flush, no metadata save, and no chance to finish an upload. What survives is
# whatever reached the object store and whatever the metadata file recorded
# before the signal. The test checks the property that actually matters -- the
# filesystem mounts again and what was fsynced is still there -- rather than
# pretending nothing was lost.
#
# usage: crash-recovery.sh [rounds] [MB]
set -u
ROUNDS=${1:-4}
MB=${2:-16}
BIN=${BIN:-/usr/local/bin/blobdev}
NC=${NC:-http://127.0.0.1:18083/remote.php/dav/files/ncadmin/blobdev}
W=${W:-/root/bd-test/crash}
MNT=${MNT:-/mnt/nc}

pkill -9 -x blobdev >/dev/null 2>&1
sleep 2
$BIN reap >/dev/null 2>&1
umount -l "$MNT" 2>/dev/null
rm -rf "$W"; mkdir -p "$W" "$MNT"

start() {
  local vol=$1 log=$2; shift 2
  $BIN run --backend webdav --endpoint "$NC" --user ncadmin --password ncpass12345 \
    --volume "$vol" --meta "$W/v.meta" --id -1 "$@" > "$W/$log" 2>&1 &
  for i in $(seq 1 120); do
    local d
    d=$(grep -oE "/dev/ublkb[0-9]+" "$W/$log" 2>/dev/null | head -1)
    if [ -n "${d:-}" ] && [ -e "$d" ]; then
      dd if="$d" of=/dev/null bs=4096 count=1 iflag=direct status=none 2>/dev/null && { echo "$d"; return 0; }
    fi
    sleep 0.5
  done
  return 1
}

mount_rw() {
  for _ in $(seq 1 60); do mount "$1" "$MNT" 2>/dev/null && return 0; sleep 1; done
  return 1
}
mount_ro() {
  # Mount read-only *with* journal replay.
  #
  # `noload` looks attractive -- it cannot modify the device -- but it also
  # hides the writes that were in the journal when the process died, which is
  # exactly what this test is checking survived. A machine that reboots
  # replays. Replaying on a read-only mount only writes to the journal area,
  # and the volume is writable, so nothing is at risk here.
  for _ in $(seq 1 120); do mount -o ro "$1" "$MNT" 2>/dev/null && return 0; sleep 1; done
  return 1
}

V=crash$RANDOM
D=$(start "$V" log0 --create-size 512M) || { echo "FAIL: start"; exit 1; }
mkfs.ext4 -q -F "$D" || { echo "FAIL: mkfs"; exit 1; }
mount_rw "$D" || { echo "FAIL: mount"; exit 1; }

PASSED=0
for r in $(seq 1 "$ROUNDS"); do
  # Write something and fsync the file, which is the strongest durability a
  # program can ask for, then die without warning.
  #
  # A plain `sync` is not enough to test this: it pushes data towards the
  # device without the caller learning when each write lands, and the file
  # being read back may not have reached the device at all when the process
  # dies. fsync on the file is the promise that has to hold.
  head -c $((MB * 1048576)) /dev/urandom > "$W/src$r.bin"
  cp "$W/src$r.bin" "$MNT/f$r.bin" 2>/dev/null
  python3 -c "
import os
fd = os.open('$MNT/f$r.bin', os.O_RDONLY)
os.fsync(fd)
os.close(fd)
" || { echo "round $r: FAIL fsync"; }
  pkill -9 -x blobdev
  sleep 2
  umount -l "$MNT" 2>/dev/null
  sleep 1

  # Bring it back from what survived.
  D=$(start "$V" "log$r") || { echo "round $r: FAIL restart"; exit 1; }
  # A read-only mount right after the device appears can fail while the
  # superblock is still being fetched; a few seconds make it dependable.
  sleep 5
  if ! mount_ro "$D"; then
    echo "round $r: FAIL filesystem does not mount after kill -9"
    pkill -9 -x blobdev
    continue
  fi
  # Compare against a digest taken before the kill: the file on disk is the
  # thing being checked.
  #
  # The first read after a restart comes from object storage, so it can take
  # a while; reading immediately and calling an empty result a mismatch
  # reports a data problem that does not exist. Retry the read itself.
  want=$(sha256sum "$W/src$r.bin" | awk '{print $1}')
  got=""
  lasterr=""
  for _ in $(seq 1 60); do
    lasterr=$(sha256sum "$MNT/f$r.bin" 2>&1 >/dev/null)
    got=$(sha256sum "$MNT/f$r.bin" 2>/dev/null | awk '{print $1}')
    [ -n "${got:-}" ] && break
    sleep 2
  done
  [ -n "${lasterr:-}" ] && [ -z "${got:-}" ] && echo "round $r: read error: $lasterr"
  if [ "$want" = "$got" ]; then
    echo "round $r: PASS fsynced file intact after kill -9"
    PASSED=$((PASSED + 1))
  else
    echo "round $r: FAIL fsynced file differs after kill -9 (${want:0:12} vs ${got:0:12})"
  fi
  umount "$MNT" 2>/dev/null
  pkill -9 -x blobdev
  sleep 2

  # Continue with the same volume: the next round must still work.
  D=$(start "$V" "logw$r") || { echo "round $r: FAIL restart for writing"; exit 1; }
  mount_rw "$D" || { echo "round $r: FAIL cannot remount read-write"; exit 1; }
done

echo "PASSED=$PASSED/$ROUNDS"
umount "$MNT" 2>/dev/null
pkill -9 -x blobdev
[ "$PASSED" -eq "$ROUNDS" ] && echo "PASS: crash recovery" || echo "FAIL: crash recovery"
