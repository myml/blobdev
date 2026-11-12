#!/bin/bash
# Does a kill -9 still recover after the fsync was batched?
#
# The claim is that the rename alone makes the metadata survive a process
# kill -- the page cache is the kernel's, not the daemon's -- and that fsync
# only insures against power loss. Testing the claim means killing the
# daemon mid-write and checking what the next start believes.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/ks; MNT=/mnt/ks

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
for m in $(grep -oE "^/dev/ublkb[0-9]+ [^ ]+" /proc/mounts 2>/dev/null | awk '{print $2}'); do umount -l "$m" 2>/dev/null; done
rm -rf $W; mkdir -p $W $MNT
V=ks$RANDOM

start() {
  $BIN run --backend dir --dir $W/o --volume $V --meta $W/v.meta --id 95 \
    --create-size 1G --max-age 60 --full-pct 100 > $W/$1 2>&1 &
  for _ in $(seq 1 120); do grep -q "created /dev/ublkb95" $W/$1 2>/dev/null && break; sleep 1; done
  grep -q "created /dev/ublkb95" $W/$1 || { echo "FAIL: no device"; return 1; }
}

start l1 || exit 1
mkfs.ext4 -q -F /dev/ublkb95 2>/dev/null
for _ in $(seq 1 90); do mount /dev/ublkb95 $MNT 2>/dev/null && break; sleep 1; done

echo "== 64 MiB written, no sync, then kill -9 immediately"
head -c 67108864 /dev/urandom > $MNT/a.bin
# No sync: this is the case the per-write fsync was protecting against.
sha256sum $MNT/a.bin | awk '{print $1}' > $W/want
PID=$(pgrep -x blobdev | head -1)
kill -9 $PID
# The kernel releases a device id asynchronously; reusing it too soon fails
# with EOPNOTSUPP, which reads like a broken device.
for _ in $(seq 1 120); do [ "$(ls /sys/class/ublk-char 2>/dev/null|wc -l)" -eq 0 ] && break; sleep 0.5; done
$BIN reap >/dev/null 2>&1
sleep 2
umount -l $MNT 2>/dev/null

echo "== restart: what does it think was written?"
start l2 || { echo "FAIL: restart"; exit 1; }
for _ in $(seq 1 90); do mount -o ro /dev/ublkb95 $MNT 2>/dev/null && break; sleep 1; done
if mountpoint -q $MNT; then
  got=$(sha256sum $MNT/a.bin 2>/dev/null | awk '{print $1}')
  want=$(cat $W/want)
  echo "  metadata survived: $([ -n "$got" ] && echo yes || echo no)"
  echo "  bytes after restart: $(stat -c %s $MNT/a.bin 2>/dev/null || echo 0)"
  echo "  data identical: $([ "$want" = "$got" ] && echo yes || echo NO)"
  umount $MNT 2>/dev/null
else
  echo "  FAIL: could not remount"
fi
$BIN stop --id 95 >/dev/null 2>&1
