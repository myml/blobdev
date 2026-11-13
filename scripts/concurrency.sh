#!/bin/bash
# Eight processes racing for one volume.
#
# The lock exists so that one daemon owns a volume. What matters when they
# race is: nobody else gets in, every loser says why, and the winner is not
# disturbed -- its device still formats, mounts and takes writes afterwards.
set -u
BIN=${BIN:-/usr/local/bin/blobdev}
W=/root/bd-test/cc; MNT=/mnt/cc
PASS=0; FAIL=0
ok()  { echo "  ok   $*"; PASS=$((PASS + 1)); }
bad() { echo "  FAIL $*"; FAIL=$((FAIL + 1)); }

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
$BIN run --backend dir --dir $W/o --volume cc --meta $W/v.meta --id 219 --create-size 128M > $W/l0 2>&1 &
for _ in $(seq 1 60); do grep -q "created /dev/ublkb219" $W/l0 2>/dev/null && break; sleep 0.5; done
[ "$(pgrep -x blobdev | wc -l)" -eq 1 ] && ok "one daemon holds the volume" || bad "could not get a holder"

# Eight at once. bare `wait` waited for the sub-shells' children, not the
# sub-shells, and hung until every racer had run a full ten seconds. Collect
# the pids and wait on those.
PIDS=""
for i in $(seq 1 8); do
  timeout 10 $BIN run --backend dir --dir $W/o --volume cc --meta $W/v.meta --id 219 \
    > $W/l$i 2>&1 &
  PIDS="$PIDS $!"
done
for p in $PIDS; do wait "$p" 2>/dev/null; done

GOT=0; REFUSED=0
for i in $(seq 1 8); do
  grep -q "created /dev/ublkb219" $W/l$i 2>/dev/null && GOT=$((GOT + 1))
  grep -qi "already using this volume" $W/l$i 2>/dev/null && REFUSED=$((REFUSED + 1))
done
[ "$GOT" -eq 0 ] && ok "none of the eight took the device" || bad "$GOT of 8 took it"
[ "$REFUSED" -eq 8 ] && ok "all eight explained the lock" \
                     || bad "only $REFUSED of 8 explained it"

OTHER=$(cat $W/l[1-8] 2>/dev/null | grep -vE "INFO|already using|created /dev" | sort -u | head -1)
[ -z "$OTHER" ] && ok "no other kind of failure" || bad "other output: $(echo "$OTHER" | cut -c1-60)"
[ "$(pgrep -x blobdev | wc -l)" -eq 1 ] && ok "the holder is untouched" \
                                        || bad "the holder is gone"
[ -e /dev/ublkb219 ] && ok "its device is still there" || bad "its device vanished"

# And it still works.
mkfs.ext4 -q -F /dev/ublkb219 2>/dev/null && ok "the device still formats" || bad "mkfs failed"
mount /dev/ublkb219 $MNT 2>/dev/null && ok "it still mounts" || bad "mount failed"
head -c 4194304 /dev/urandom > $MNT/x.bin 2>/dev/null
[ "$(stat -c %s $MNT/x.bin 2>/dev/null)" = "4194304" ] && ok "it still takes writes" || bad "write failed"
umount $MNT 2>/dev/null

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
rm -rf $W
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && echo "PASS: concurrency" || { echo "FAIL: concurrency"; exit 1; }
