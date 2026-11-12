#!/bin/bash
# The shapes nobody uses, checked so they fail clearly rather than quietly.
#
# Defaults are 64 MiB chunks, 4 MiB objects, one queue of 64 tags, and volumes
# that line up with the grid. Everything below is off that path: one object per
# chunk, sixty-four objects per chunk, one tag, many queues, a volume smaller
# than a chunk. Each either works or refuses with a reason; none of them may
# hang, corrupt data, or fail silently.
#
# usage: boundaries.sh [backend: dir|webdav]
set -u
BACKEND=${1:-dir}
BIN=${BIN:-/usr/local/bin/blobdev}
W=${W:-/root/bd-test/bounds}
MNT=${MNT:-/mnt/nc}
NC=${NC:-http://127.0.0.1:18083/remote.php/dav/files/ncadmin/blobdev}

pkill -9 -x blobdev >/dev/null 2>&1
sleep 2
$BIN reap >/dev/null 2>&1
umount -l "$MNT" 2>/dev/null
rm -rf "$W"; mkdir -p "$W" "$MNT"

if [ "$BACKEND" = webdav ]; then
  BASE=(--backend webdav --endpoint "$NC" --user ncadmin --password ncpass12345 --timeout 20)
else
  BASE=(--backend dir --dir "$W/objs")
fi

PASS=0; FAIL=0
note() { echo "  $*"; }

start_dev() {
  local vol=$1 log=$2; shift 2
  $BIN run "${BASE[@]}" --volume "$vol" --meta "$W/$vol.meta" --id -1 "$@" > "$W/$log" 2>&1 &
  for i in $(seq 1 120); do
    D=$(grep -oE "/dev/ublkb[0-9]+" "$W/$log" 2>/dev/null | head -1)
    [ -n "${D:-}" ] && [ -e "$D" ] && { sleep 1; echo "$D"; return 0; }
    pgrep -x blobdev >/dev/null || return 1
    sleep 0.5
  done
  return 1
}

roundtrip() {
  local label=$1 size=$2; shift 2
  local vol=bd$RANDOM D
  D=$(start_dev "$vol" l1 --create-size "$size" "$@") || {
    note "$label: REFUSED — $(grep -iE 'blobdev:|error' "$W/l1" 2>/dev/null | head -1 | cut -c1-80)"
    FAIL=$((FAIL + 1)); pkill -9 -x blobdev; sleep 1; return
  }
  mkfs.ext4 -q -F "$D" 2>/dev/null || { note "$label: MKFS failed"; FAIL=$((FAIL+1)); pkill -9 -x blobdev; return; }
  local mounted=no
  for _ in $(seq 1 60); do mount "$D" "$MNT" 2>/dev/null && { mounted=yes; break; }; sleep 1; done
  [ "$mounted" = yes ] || { note "$label: MOUNT failed"; FAIL=$((FAIL+1)); pkill -9 -x blobdev; return; }

  head -c 2097152 /dev/urandom > "$W/src.bin"
  # Rewriting repeatedly spreads blocks over the volume, so the tail objects
  # get written without the test having to know where they are.
  for _ in $(seq 1 40); do cp "$W/src.bin" "$MNT/f.bin" 2>/dev/null; done
  dd if=/dev/urandom of="$W/src.bin" bs=4096 count=1 conv=notrunc status=none 2>/dev/null
  cp "$W/src.bin" "$MNT/f.bin" 2>/dev/null
  sync
  local want
  want=$(sha256sum "$W/src.bin" | awk '{print $1}')
  umount "$MNT"
  pkill -TERM -x blobdev
  for _ in $(seq 1 600); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
  pgrep -x blobdev >/dev/null && pkill -9 -x blobdev
  sleep 2

  D=$(start_dev "$vol" l2) || { note "$label: restart failed"; FAIL=$((FAIL+1)); pkill -9 -x blobdev; return; }
  for _ in $(seq 1 120); do mount -o ro "$D" "$MNT" 2>/dev/null && break; sleep 1; done
  local got
  got=$(sha256sum "$MNT/f.bin" 2>/dev/null | awk '{print $1}')
  umount "$MNT" 2>/dev/null
  pkill -9 -x blobdev
  if [ "$want" = "$got" ]; then
    note "$label: PASS"
    PASS=$((PASS + 1))
  else
    note "$label: FAIL (${want:0:10} vs ${got:0:10})"
    FAIL=$((FAIL + 1))
  fi
  sleep 1
}

echo "== geometry"
roundtrip "objects 4 MiB (default)" 256M
roundtrip "object == chunk (1 per chunk)" 256M --object-mib 64
roundtrip "object 1 MiB (64 per chunk)" 256M --object-mib 1
echo "== sizes"
roundtrip "32M half a chunk" 32M
roundtrip "64M exactly one chunk" 64M
roundtrip "96M 1.5 chunks" 96M
roundtrip "100M not a multiple" 100M
roundtrip "16M quarter chunk" 16M
echo "== queues"
roundtrip "depth 1, one queue" 128M --queues 1 --depth 1
roundtrip "depth 8, 4 queues" 128M --queues 4 --depth 8

echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && echo "PASS: boundaries" || echo "FAIL: boundaries"
