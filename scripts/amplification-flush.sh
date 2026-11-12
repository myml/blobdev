#!/bin/bash
# The same 100 writes with the default window, to see whether --max-age
# decides the amplification.
#
# It does not, much. 185x at --max-age 1, 133x at the default 30 -- because
# `sync` sends a FLUSH and FLUSH uploads every pending object immediately,
# whatever the window says. What decides amplification is how much is
# written between two flushes.
#
# NOTE: this kills every blobdev process by name, so do not run it beside
# another test on the same host.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/amp3; MNT=/mnt/amp3
S3=(--backend s3 --endpoint http://192.168.0.220:9000 --bucket blobtest \
  --access-key blobdev --secret-key blobdevsecret123 --region us-east-1 --path-style)

# No pkill here: another blobdev may be running (the soak). Stop only the
# daemon this script starts.
rm -rf $W; mkdir -p $W $MNT

V=amp3$RANDOM
# Default policy: max-age 30s, full-pct 50.
BLOBDEV_SUMMARY_SECS=2 $BIN run "${S3[@]}" --volume $V --meta $W/v.meta --id 76 \
  --create-size 512M --max-age 30 --full-pct 50 > $W/l 2>&1 &
for _ in $(seq 1 180); do [ -e /dev/ublkb76 ] && break; sleep 1; done
mkfs.ext4 -q -F /dev/ublkb76 2>/dev/null
for _ in $(seq 1 120); do mount /dev/ublkb76 $MNT 2>/dev/null && break; sleep 1; done

sent() { grep -oE "sent [0-9.]+ MiB" $W/l 2>/dev/null | tail -1 | awk '{printf "%.1f", $2}'; }

head -c 8388608 /dev/urandom > $MNT/base.bin
sync; sleep 40
S0=$(sent)

echo "== 100 x 4 KiB writes inside one object, default window"
for _ in $(seq 1 100); do
  dd if=/dev/urandom of=$MNT/base.bin bs=4096 seek=$((RANDOM % 64)) count=1 conv=notrunc status=none 2>/dev/null
done
sync
sleep 40
S1=$(sent)

echo "  app wrote:  $(awk "BEGIN{printf \"%.2f\", 100*4096/1048576}") MiB"
echo "  store recv: $(awk "BEGIN{printf \"%.1f\", $S1 - $S0}") MiB"
echo "  ratio:      $(awk "BEGIN{d=$S1-$S0; a=100*4096/1048576; if (a>0) printf \"%.0fx\", d/a}")"

umount $MNT 2>/dev/null
$BIN stop --id 76 >/dev/null 2>&1
