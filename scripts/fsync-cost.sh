#!/bin/bash
# What would the write path look like without the per-write fsync?
#
# Measured by lying about the cost: the same write pattern against a
# metadata file on tmpfs (no fsync cost) versus on the real disk.
set -u
echo "== metadata save cost by filesystem"
for dir in /dev/shm /root/bd-test; do
  mkdir -p $dir 2>/dev/null
  python3 - "$dir" <<'PY'
import os, sys, time
d = sys.argv[1]
p = os.path.join(d, "fs-probe")
t0 = time.time()
n = 300
for _ in range(n):
    tmp = p + ".tmp"
    with open(tmp, "wb") as f:
        f.write(b"x" * 1024); f.flush(); os.fsync(f.fileno())
    os.rename(tmp, p)
el = time.time() - t0
print(f"  {d:20s} with fsync:    {el/n*1000:.3f} ms/save ({n/el:.0f}/s)")

t0 = time.time()
for _ in range(n):
    tmp = p + ".tmp"
    with open(tmp, "wb") as f:
        f.write(b"x" * 1024)
    os.rename(tmp, p)
el = time.time() - t0
print(f"  {d:20s} without fsync: {el/n*1000:.3f} ms/save ({n/el:.0f}/s)")
os.remove(p)
PY
done
