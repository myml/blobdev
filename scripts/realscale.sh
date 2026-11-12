#!/bin/bash
# A volume at a size someone might use, holding real data.
#
# Everything in the other scripts runs 16-256 MiB volumes. Two problems only
# appear at size: startup cost that scales with the object count, and a device
# that is not released on the way out -- which looked like a five minute
# restart and a failed mount, and was neither.
#
# usage: realscale.sh [dir|s3]
set -u
BACKEND=${1:-dir}
BIN=${BIN:-/usr/local/bin/blobdev}
VOL=scale$RANDOM
W=${W:-/root/bd-test/scale}
MNT=${MNT:-/mnt/scale}
GIB=${GIB:-1}

if [ "$BACKEND" = s3 ]; then
  ARGS=(--backend s3 --endpoint "${S3_ENDPOINT:-http://192.168.0.220:9000}" \
    --bucket "${S3_BUCKET:-blobtest}" --access-key "${S3_KEY:-blobdev}" \
    --secret-key "${S3_SECRET:-blobdevsecret123}" --region us-east-1 --path-style)
  STORE_DIR=""
else
  ARGS=(--backend dir --dir "$W/objs")
  STORE_DIR="$W/objs"
fi

pkill -9 -x blobdev >/dev/null 2>&1
sleep 2
"$BIN" reap >/dev/null 2>&1
for m in $(grep -oE "^/dev/ublkb[0-9]+ [^ ]+" /proc/mounts 2>/dev/null | awk '{print $2}'); do
  umount -l "$m" 2>/dev/null
done
rm -rf "$W"; mkdir -p "$W" "$MNT"

wait_dev() {
  for _ in $(seq 1 180); do [ -e "/dev/ublkb$1" ] && return 0; sleep 1; done
  return 1
}
wait_gone() {
  for _ in $(seq 1 120); do
    [ "$(ls /sys/class/ublk-char 2>/dev/null | wc -l)" -eq 0 ] && return 0
    sleep 0.5
  done
  return 1
}

echo "== start 5 GiB volume on $BACKEND"
T0=$(date +%s)
"$BIN" run "${ARGS[@]}" --volume $VOL --meta "$W/v.meta" --id 50 --create-size 5G \
  --cache-dir "$W/cache" --cache-size 512M > "$W/log1" 2>&1 &
wait_dev 50 || { echo "FAIL: no device"; tail -5 "$W/log1"; exit 1; }
echo "START_SECONDS=$(( $(date +%s) - T0 ))"

mkfs.ext4 -q -F /dev/ublkb50 || { echo "FAIL: mkfs"; exit 1; }
for _ in $(seq 1 120); do mount /dev/ublkb50 "$MNT" 2>/dev/null && break; sleep 1; done
mountpoint -q "$MNT" || { echo "FAIL: mount"; exit 1; }
echo "FS_SIZE=$(df -B1 --output=size "$MNT" | tail -1 | tr -d ' ')"

# 64 MiB per file, 64 of them for 4 GiB, but the default run keeps to GIB.
COUNT=$((GIB / 64))
[ "$COUNT" -ge 1 ] || COUNT=1
# GIB is the total data size in MiB (the name is historical); the label says
# what is actually written.
echo "== write ${GIB} MiB in 64 MiB files"
T0=$(date +%s)
for i in $(seq 1 "$COUNT"); do head -c 67108864 /dev/urandom > "$MNT/f$i.bin"; done
sync
echo "WRITE_SECONDS=$(( $(date +%s) - T0 ))"

echo "== small random writes"
T0=$(date +%s)
for _ in $(seq 1 200); do
  dd if=/dev/urandom of="$MNT/f$((RANDOM % COUNT + 1)).bin" bs=4096 \
     seek=$((RANDOM % 16384)) count=1 conv=notrunc status=none 2>/dev/null
done
sync
echo "SMALL_WRITES_SECONDS=$(( $(date +%s) - T0 ))"

for i in $(seq 1 "$COUNT"); do sha256sum "$MNT/f$i.bin" | awk '{print $1}'; done > "$W/want.txt"
echo "META_BYTES=$(stat -c %s "$W/v.meta" 2>/dev/null)"
umount "$MNT"
pkill -TERM -x blobdev
for _ in $(seq 1 900); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
pgrep -x blobdev >/dev/null && { echo "FAIL: daemon did not exit"; pkill -9 -x blobdev; exit 1; }
wait_gone || { echo "FAIL: device still present after stop"; ls /sys/class/ublk-char; exit 1; }
echo "== device removed cleanly"

T0=$(date +%s)
"$BIN" run "${ARGS[@]}" --volume $VOL --meta "$W/v.meta" --id 50 \
  --cache-dir "$W/cache" --cache-size 512M > "$W/log2" 2>&1 &
wait_dev 50 || { echo "FAIL: restart"; tail -5 "$W/log2"; exit 1; }
echo "RESTART_SECONDS=$(( $(date +%s) - T0 ))"
for _ in $(seq 1 120); do mount -o ro /dev/ublkb50 "$MNT" 2>/dev/null && break; sleep 1; done

BAD=0
if mountpoint -q "$MNT"; then
  n=1
  while read -r want; do
    got=$(sha256sum "$MNT/f$n.bin" 2>/dev/null | awk '{print $1}')
    [ "$want" = "$got" ] || { echo "  f$n: DIFFERS"; BAD=$((BAD + 1)); }
    n=$((n + 1))
  done < "$W/want.txt"
  umount "$MNT" 2>/dev/null
else
  echo "FAIL: remount"
  BAD=99
fi
pkill -9 -x blobdev
[ -n "$STORE_DIR" ] && echo "OBJECTS=$(ls "$STORE_DIR/$VOL/c" 2>/dev/null | wc -l)"
[ "$BAD" -eq 0 ] && echo "PASS: ${GIB} MiB at scale on $BACKEND" || echo "FAIL: $BAD file(s) wrong"
