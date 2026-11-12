#!/bin/bash
# Bytes sent to the store for bytes written by the application.
#
# Sends them via the daemon's own accounting: the periodic summary line
# reports "sent N MiB in M uploads", which is the only number that matters
# for a capacity plan.
#
# Measured on RustFS with 100 scattered 4 KiB writes inside one 4 MiB
# object: 0.39 MiB written, 72 MiB sent, 42 uploads -- 185x. The README
# said 1024x, which is the per-upload worst case; batching makes the
# average much lower and the difference matters when sizing a link.
#
# NOTE: this kills every blobdev process by name, so do not run it beside
# another test on the same host.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/amp2; MNT=/mnt/amp2
S3=(--backend s3 --endpoint http://192.168.0.220:9000 --bucket blobtest \
  --access-key blobdev --secret-key blobdevsecret123 --region us-east-1 --path-style)

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
V=amp$RANDOM

# A one-second summary interval, so the accounting is visible immediately.
BLOBDEV_SUMMARY_SECS=1 $BIN run "${S3[@]}" --volume $V --meta $W/v.meta --id 75 \
  --create-size 512M --max-age 1 --full-pct 1 > $W/l 2>&1 &
for _ in $(seq 1 180); do [ -e /dev/ublkb75 ] && break; sleep 1; done
mkfs.ext4 -q -F /dev/ublkb75 2>/dev/null
for _ in $(seq 1 120); do mount /dev/ublkb75 $MNT 2>/dev/null && break; sleep 1; done

sent() {
  grep -oE "sent [0-9.]+ MiB" $W/l 2>/dev/null | tail -1 | awk '{printf "%.1f", $2}'
}

head -c 8388608 /dev/urandom > $MNT/base.bin
sync; sleep 5
S0=$(sent)

echo "== 100 x 4 KiB writes inside one object"
for _ in $(seq 1 100); do
  dd if=/dev/urandom of=$MNT/base.bin bs=4096 seek=$((RANDOM % 64)) count=1 conv=notrunc status=none 2>/dev/null
done
sync; sleep 8
S1=$(sent)

APP_MIB=$(awk "BEGIN{printf \"%.2f\", 100*4096/1048576}")
echo "  app wrote:   ${APP_MIB} MiB"
echo "  store recv:  $(awk "BEGIN{printf \"%.1f\", $S1 - $S0}") MiB"
echo "  summary:     $(grep -oE 'sent [0-9.]+ MiB in [0-9]+ upload' $W/l | tail -1)"

umount $MNT 2>/dev/null
pkill -TERM -x blobdev
for _ in $(seq 1 900); do pgrep -x blobdev >/dev/null || break; sleep 0.2; done
pkill -9 -x blobdev 2>/dev/null
