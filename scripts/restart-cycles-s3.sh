#!/bin/bash
# Twenty start/stop cycles on one volume, against S3.
#
# Restarting has been tested; what accumulates over many restarts has not.
# Every cycle creates and removes a ublk device, opens the metadata, opens a
# socket to the store, and starts threads. A leak of any of those shows up
# here and nowhere else.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/cyc; MNT=/mnt/cyc
S3=(--backend s3 --endpoint http://192.168.0.220:9000 --bucket blobtest \
  --access-key blobdev --secret-key blobdevsecret123 --region us-east-1 --path-style)

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
for m in $(grep -oE "^/dev/ublkb[0-9]+ [^ ]+" /proc/mounts 2>/dev/null | awk '{print $2}'); do umount -l "$m" 2>/dev/null; done
rm -rf $W; mkdir -p $W $MNT
V=cyc$RANDOM
N=${N:-20}

wait_free() {
  for _ in $(seq 1 120); do
    [ "$(ls /sys/class/ublk-char 2>/dev/null | wc -l)" -eq 0 ] && return 0
    sleep 0.5
  done
  return 1
}

echo "cycle  rss_kb  fds  devs  mounts  threads"
FIRST_RSS=""
FAILS=0
for i in $(seq 1 $N); do
  create=""
  [ "$i" -eq 1 ] && create="--create-size 512M"
  # shellcheck disable=SC2086
  $BIN run "${S3[@]}" --volume $V --meta $W/v.meta --id 72 $create > $W/l$i 2>&1 &
  for _ in $(seq 1 180); do [ -e /dev/ublkb72 ] && break; sleep 0.5; done
  if [ ! -e /dev/ublkb72 ]; then
    echo "  cycle $i: FAILED to start"
    grep -vE "^\[.*INFO" $W/l$i | head -2
    FAILS=$((FAILS+1))
    pkill -9 -x blobdev 2>/dev/null; sleep 2; $BIN reap >/dev/null 2>&1
    continue
  fi
  # A little IO every cycle, so the read and write paths are exercised too.
  if [ "$i" -eq 1 ]; then
    mkfs.ext4 -q -F /dev/ublkb72 2>/dev/null
  fi
  for _ in $(seq 1 60); do mount /dev/ublkb72 $MNT 2>/dev/null && break; sleep 1; done
  if mountpoint -q $MNT; then
    dd if=/dev/urandom of=$MNT/f$i.bin bs=1M count=4 conv=notrunc status=none 2>/dev/null
    sync
    umount $MNT
  fi
  PID=$(pgrep -x blobdev | head -1)
  RSS=$(( $(awk '/VmRSS/{print $2}' /proc/$PID/status 2>/dev/null || echo 0) ))
  FDS=$(ls /proc/$PID/fd 2>/dev/null | wc -l)
  THR=$(ls /proc/$PID/task 2>/dev/null | wc -l)
  DEVN=$(ls /sys/class/ublk-char 2>/dev/null | wc -l)
  MNTN=$(grep -c ublkb /proc/mounts)
  [ -z "$FIRST_RSS" ] && FIRST_RSS=$RSS
  printf "%-6s %-7s %-4s %-5s %-7s %s\n" "$i" "$RSS" "$FDS" "$DEVN" "$MNTN" "$THR"
  pkill -TERM -x blobdev
  for _ in $(seq 1 900); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
  pkill -9 -x blobdev 2>/dev/null
  wait_free || { echo "  cycle $i: device table did not clear"; FAILS=$((FAILS+1)); $BIN reap >/dev/null 2>&1; }
done

echo "FIRST_RSS=$FIRST_RSS"
echo "FAILS=$FAILS"
echo "final_devs=$(ls /sys/class/ublk-char 2>/dev/null|wc -l) final_mounts=$(grep -c ublkb /proc/mounts)"
