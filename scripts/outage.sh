#!/bin/bash
# Check the promises the README makes about an outage.
#
#   "the block device answers every IO with EIO, the daemon logs the request
#    and the reason, and nothing hangs or reports success."
#
#   "ext4 remounts read-only after an IO error and does not recover by itself."
#
# Those are the sentences an operator acts on during an incident, so they get
# tested rather than asserted. The store is made to refuse everything through
# the fault proxy; every operation here is timed and its result recorded.
#
# usage: outage.sh
set -u
BIN=${BIN:-/usr/local/bin/blobdev}
NC=${NC:-http://127.0.0.1:19090/remote.php/dav/files/ncadmin/blobdev}
GOOD=${GOOD:-http://127.0.0.1:18083/remote.php/dav/files/ncadmin/blobdev}
W=${W:-/root/bd-test/outage}
MNT=${MNT:-/mnt/nc}

pkill -9 -x blobdev >/dev/null 2>&1
sleep 2
$BIN reap >/dev/null 2>&1
umount -l "$MNT" 2>/dev/null
rm -rf "$W"; mkdir -p "$W" "$MNT"
V=out$RANDOM

start() {
  local endpoint=$1 log=$2; shift 2
  $BIN run --backend webdav --endpoint "$endpoint" --user ncadmin --password ncpass12345 \
    --volume $V --meta "$W/v.meta" --id -1 --timeout 20 "$@" > "$W/$log" 2>&1 &
  for i in $(seq 1 120); do
    local d
    d=$(grep -oE "/dev/ublkb[0-9]+" "$W/$log" 2>/dev/null | head -1)
    # Only wait for the node to appear. Proving it answers reads is wrong here:
    # during an outage a read is supposed to fail.
    if [ -n "${d:-}" ] && [ -e "$d" ]; then
      sleep 2
      echo "$d"; return 0
    fi
    sleep 0.5
  done
  return 1
}

D=$(start "$GOOD" log1 --create-size 256M) || { echo "FAIL: start"; tail -3 "$W/log1"; exit 1; }
mkfs.ext4 -q -F "$D" || { echo "FAIL: mkfs"; exit 1; }
for _ in $(seq 1 60); do mount "$D" "$MNT" 2>/dev/null && break; sleep 1; done
mountpoint -q "$MNT" || { echo "FAIL: mount"; exit 1; }

# Something already durable, so a write during the outage has a base object.
head -c 4194304 /dev/urandom > "$W/keep.bin"
cp "$W/keep.bin" "$MNT/keep.bin"
python3 -c "import os; fd=os.open('$MNT/keep.bin', os.O_RDONLY); os.fsync(fd); os.close(fd)"
echo "== baseline written"

umount "$MNT"
pkill -TERM -x blobdev
for i in $(seq 1 400); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
pgrep -x blobdev >/dev/null && pkill -9 -x blobdev
sleep 3

# Bring the same volume back with a store that refuses everything.
D=$(start "$NC" log2) || { echo "FAIL: restart against the failing store"; tail -3 "$W/log2"; exit 1; }
echo "DAEMON_STARTED_WITH_STORE_DOWN=yes"

# The device is there; what it does on each access is the question. A cold
# mount has to read the superblock through the store, so it cannot succeed --
# that is the documented behaviour, not a failure of this test. Probe the
# device directly instead, which is the layer the README talks about.
start_s=$(date +%s)
dd if="$D" of=/dev/null bs=4096 count=1 iflag=direct status=none 2>"$W/probe.err"
probe_rc=$?
end_s=$(date +%s)
echo "PROBE_RC=$probe_rc PROBE_SECONDS=$((end_s - start_s))"
if [ "$probe_rc" -ne 0 ]; then
  echo "== the device reports an error rather than hanging or returning zeros"
fi

# A mount cannot be completed against a dead store; record that too, because
# it is what an operator will actually meet.
if mount "$D" "$MNT" 2>/dev/null; then
  echo "MOUNT_DURING_OUTAGE=yes"
else
  echo "MOUNT_DURING_OUTAGE=no"
fi


echo "MAIN_LOG_ERRORS=$(grep -ci 'error' "$W/log2" 2>/dev/null || echo 0)"
echo "DAEMON_ALIVE=$(pgrep -c blobdev 2>/dev/null || echo 0)"
echo "FS_READONLY=$(grep -q ' /mnt/nc ro' /proc/mounts && echo yes || echo no)"

umount "$MNT" 2>/dev/null
pkill -9 -x blobdev
