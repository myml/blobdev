#!/bin/bash
# Small-write latency, measured two ways, because the two disagree.
#
# fio with libaio reports a 99.99th percentile around 1.4 s against this
# device. The same file, same session, with plain O_DIRECT pwrite: 99.99th
# 29 ms, 50th 0.2 ms, about 2000-4000 writes/s.
#
# The difference is the async block path. libaio submits asynchronously
# through the block layer and the 99.99th picks up whatever that path
# occasionally does; blobdev completes each command synchronously in its
# queue thread, so pwrite measures blobdev and libaio measures blobdev plus
# the async machinery above it.
#
# Use pwrite numbers for blobdev. Keep the fio line as evidence of the gap.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/fv; MNT=/mnt/fv
pkill -9 -x blobdev >/dev/null 2>&1; sleep 3
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
$BIN run --backend dir --dir $W/o --volume fv$RANDOM --meta $W/v.meta --id 104 \
  --create-size 2G --max-age 2 --full-pct 10 > $W/l 2>&1 &
for _ in $(seq 1 120); do grep -q "created /dev/ublkb104" $W/l 2>/dev/null && break; sleep 1; done
mkfs.ext4 -q -F /dev/ublkb104 2>/dev/null
for _ in $(seq 1 90); do mount /dev/ublkb104 $MNT 2>/dev/null && break; sleep 1; done
head -c 536870912 /dev/urandom > $MNT/f.bin
sync; sleep 3

echo "== fio, libaio"
fio --name=r --filename=$MNT/f.bin --direct=1 --rw=randwrite --bs=4k \
    --size=512M --iodepth=1 --numjobs=1 --runtime=15 --time_based \
    --ioengine=libaio --group_reporting 2>/dev/null |
  grep -E "iops|99\.99th|lat \(usec\)" | head -3

echo "== same file, plain pwrite with O_DIRECT"
python3 - "$MNT/f.bin" <<'PY'
import os, sys, time
fd = os.open(sys.argv[1], os.O_RDWR | os.O_DIRECT)
buf = bytearray(4096)
gaps = []
start = time.time()
while time.time() - start < 15:
    off = (int.from_bytes(os.urandom(4), "little") % 131072) * 4096
    t0 = time.time()
    os.pwrite(fd, buf, off)
    gaps.append(time.time() - t0)
os.close(fd)
gaps.sort()
n = len(gaps)
print(f"  {n} writes ({n/15:.0f}/s)  50th={gaps[n//2]*1000:.1f}ms  "
      f"99th={gaps[int(n*0.99)]*1000:.1f}ms  99.99th={gaps[int(n*0.9999)]*1000:.1f}ms")
PY
umount $MNT 2>/dev/null
pkill -9 -x blobdev 2>/dev/null
