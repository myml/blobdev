#!/bin/bash
# A full volume must behave like a full disk.
#
# Writes fail with ENOSPC, the data already there stays readable, the daemon
# stays up, and the filesystem unmounts cleanly. Then, after deleting
# something, the space has to come back.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/vfull; MNT=/mnt/vfull
PASS=0; FAIL=0
ok()  { echo "  ok   $*"; PASS=$((PASS + 1)); }
bad() { echo "  FAIL $*"; FAIL=$((FAIL + 1)); }

pkill -9 -x blobdev >/dev/null 2>&1; sleep 3
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
$BIN run --backend dir --dir $W/o --volume vf$RANDOM --meta $W/v.meta --id 178 \
  --create-size 256M --max-age 1 > $W/l 2>&1 &
for _ in $(seq 1 120); do grep -q "created /dev/ublkb178" $W/l 2>/dev/null && break; sleep 1; done
mkfs.ext4 -q -F /dev/ublkb178 2>/dev/null
for _ in $(seq 1 90); do mount /dev/ublkb178 $MNT 2>/dev/null && break; sleep 1; done

# Something known to read back later.
head -c 16777216 /dev/urandom > $MNT/keep.bin
KEEP=$(sha256sum $MNT/keep.bin | awk '{print $1}')
# Fill it up.
dd if=/dev/zero of=$MNT/fill.bin bs=1M count=400 2>/dev/null
sync

# A write with nowhere to go must be refused.
if dd if=/dev/zero of=$MNT/more.bin bs=1M count=8 2>/dev/null; then
  bad "a write to a full volume was accepted"
else
  ok "a write to a full volume fails"
fi
ERR=$(PROBE="$MNT/probe.bin" python3 - <<'PY' 2>/dev/null
import os
try:
    with open(os.environ["PROBE"], "wb") as f:
        f.write(b"x" * (8 << 20))
    print("accepted")
except OSError as e:
    print(e.errno)
PY
)
[ "$ERR" = "28" ] && ok "the application sees ENOSPC (28)" || bad "the error was '$ERR'"

pgrep -x blobdev >/dev/null && ok "the daemon is still running" || bad "the daemon died"
mountpoint -q $MNT && ok "the filesystem is still mounted" || bad "the filesystem was unmounted"

GOT=$(sha256sum $MNT/keep.bin 2>/dev/null | awk '{print $1}')
[ "$KEEP" = "$GOT" ] && ok "data written before the volume filled is intact" \
                     || bad "data written before the volume filled changed"

# Deleting has to give the space back.
rm -f $MNT/fill.bin
sync
sleep 2
if head -c 8388608 /dev/urandom > $MNT/after.bin 2>/dev/null; then
  ok "space comes back after deleting a file"
else
  bad "could not write after deleting a file"
fi

umount $MNT 2>/dev/null && ok "unmounts cleanly" || bad "unmount failed"
$BIN stop --id 178 >/dev/null 2>&1
pkill -9 -x blobdev >/dev/null 2>&1
rm -rf $W $MNT
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && echo "PASS: full volume" || exit 1
