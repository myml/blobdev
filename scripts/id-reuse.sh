#!/bin/bash
# Does a device id come back cleanly after a hundred uses?
#
# `--id -1` hands out the lowest free number, so a daemon restarted often
# reuses the same few ids. A device that is not fully released leaves its id
# unusable, and the volume fails to start with EOPNOTSUPP -- which is what
# happened before the signal handler started removing the device.
#
# Measured: 100 cycles, 0 failures, 103 seconds, device table empty at the
# end.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/idr
pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W
N=${N:-100}
FAILS=0
T0=$(date +%s)
for i in $(seq 1 $N); do
  $BIN run --backend dir --dir $W/o --volume idr --meta $W/v.meta --id -1 \
      $([ "$i" -eq 1 ] && echo "--create-size 64M") > $W/l 2>&1 &
  ok=no
  for _ in $(seq 1 120); do
    if grep -q "created /dev/ublkb" $W/l 2>/dev/null; then ok=yes; break; fi
    grep -q "^blobdev:" $W/l 2>/dev/null && break
    sleep 0.2
  done
  if [ "$ok" = no ]; then
    FAILS=$((FAILS+1))
    [ "$FAILS" -le 2 ] && { echo "  cycle $i failed:"; grep -vE "^\[.*INFO" $W/l | head -2; }
  fi
  pkill -TERM -x blobdev 2>/dev/null
  for _ in $(seq 1 400); do pgrep -x blobdev >/dev/null || break; sleep 0.1; done
  pkill -9 -x blobdev 2>/dev/null
  for _ in $(seq 1 60); do [ "$(ls /sys/class/ublk-char 2>/dev/null|wc -l)" -eq 0 ] && break; sleep 0.2; done
done
echo "CYCLES=$N FAILS=$FAILS SECONDS=$(( $(date +%s) - T0 ))"
echo "final_devs=$(ls /sys/class/ublk-char 2>/dev/null|wc -l)"
