#!/bin/bash
# What does the device actually deliver, at one stream and at many?
#
# Every performance number so far came from one dd. Production is not one dd.
# Reads and writes, 1 stream then 4, on both a local directory backend (what
# the storage layer costs) and S3 (what the store costs on top).
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/tp; MNT=/mnt/tp
S3=(--backend s3 --endpoint http://192.168.0.220:9000 --bucket blobtest \
  --access-key blobdev --secret-key blobdevsecret123 --region us-east-1 --path-style)

run_case() {
  local backend=$1 size_mb=$2 streams=$3 op=$4
  pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
  $BIN reap >/dev/null 2>&1
  rm -rf $W; mkdir -p $W $MNT
  local V=tp$RANDOM
  local ARGS
  if [ "$backend" = s3 ]; then
    ARGS=("${S3[@]}")
  else
    ARGS=(--backend dir --dir $W/objs)
  fi
  $BIN run "${ARGS[@]}" --volume $V --meta $W/v.meta --id 87 --create-size 4G \
    --max-age 5 --full-pct 40 > $W/l 2>&1 &
  for _ in $(seq 1 180); do grep -q "created /dev/ublkb87" $W/l 2>/dev/null && break; sleep 1; done
  grep -q "created /dev/ublkb87" $W/l || { echo "  FAIL: no device"; return; }
  mkfs.ext4 -q -F /dev/ublkb87 2>/dev/null
  for _ in $(seq 1 120); do mount /dev/ublkb87 $MNT 2>/dev/null && break; sleep 1; done

  # Lay down data for the read case.
  if [ "$op" = read ]; then
    head -c $((size_mb * 1048576)) /dev/urandom > $MNT/data.bin
    sync; sleep 6
    # Drop the page cache so reads go to the device.
    echo 3 > /proc/sys/vm/drop_caches 2>/dev/null
  fi

  local per=$((size_mb / streams))
  local T0 T1 pids=""
  T0=$(date +%s%N)
  for i in $(seq 1 "$streams"); do
    if [ "$op" = write ]; then
      ( dd if=/dev/zero of=$MNT/w$i.bin bs=1M count=$per oflag=direct status=none 2>/dev/null ) &
    else
      ( dd if=$MNT/data.bin of=/dev/null bs=1M count=$per skip=$(( (i-1) * per )) iflag=direct status=none 2>/dev/null ) &
    fi
    pids="$pids $!"
  done
  for p in $pids; do wait "$p"; done
  T1=$(date +%s%N)
  local ms=$(( (T1 - T0) / 1000000 ))
  [ "$ms" -gt 0 ] && echo "  $backend $op ${streams} stream(s), ${size_mb} MiB: $(( size_mb * 1000 / ms )) MiB/s"

  umount $MNT 2>/dev/null
  $BIN stop --id 87 >/dev/null 2>&1
  for _ in $(seq 1 300); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
  for _ in $(seq 1 120); do [ "$(ls /sys/class/ublk-char 2>/dev/null|wc -l)" -eq 0 ] && break; sleep 0.5; done
}

echo "== local directory backend"
run_case dir 512 1 write
run_case dir 512 4 write
run_case dir 512 1 read
run_case dir 512 4 read

echo "== S3 (RustFS)"
run_case s3 128 1 write
run_case s3 128 4 write
run_case s3 128 1 read
run_case s3 128 4 read
