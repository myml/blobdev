#!/bin/bash
# Walk the README's outage-recovery steps for real, and check the data.
#
# The steps were "umount, optionally inspect, mount again". That does not
# work: when the store goes away ext4 aborts its journal, and after the
# umount the /dev/ublkbN node is gone while /sys/class/ublk-char still lists
# ublkcN. mount fails with "Can't lookup blockdev" and says nothing about
# why. Restarting the daemon recreates the device and the mount succeeds.
#
# Measured on Nextcloud through the fault proxy, 32 MiB written:
#
#   sync while the store was down    mount turned emergency_ro, reads served
#   umount + mount (the old steps)   "Can't lookup blockdev"
#   umount, stop, run, mount         mounted, sha256 identical, writes fine
set -u
BIN=/usr/local/bin/blobdev
W=${W:-/root/bd-test/outage-rec}; MNT=/mnt/outage-rec
DAV=http://127.0.0.1:19090/remote.php/dav/files/ncadmin/blobdev
PASS=0; FAIL=0
ok()   { echo "  ok   $*"; PASS=$((PASS + 1)); }
bad()  { echo "  FAIL $*"; FAIL=$((FAIL + 1)); }

set_profile() {
  systemctl stop faultproxy >/dev/null 2>&1
  sed -i "s|^ExecStart=.*|ExecStart=/usr/bin/python3 /tmp/fault_proxy.py 19090 http://127.0.0.1:18083 $1 1|" \
    /etc/systemd/system/faultproxy.service
  systemctl daemon-reload >/dev/null 2>&1
  systemctl start faultproxy >/dev/null 2>&1
  sleep 2
}

start_dev() {
  $BIN run --backend webdav --endpoint $DAV --user ncadmin --password ncpass12345 \
    --timeout 5 --retries 2 --volume "$V" --meta $W/v.meta --id 170 "$@" > $W/$2.log 2>&1 &
  for _ in $(seq 1 180); do grep -q "created /dev/ublkb170" $W/$2.log 2>/dev/null && break; sleep 1; done
  grep -q "created /dev/ublkb170" $W/$2.log
}
stop_dev() {
  umount $MNT 2>/dev/null || umount -l $MNT 2>/dev/null
  $BIN stop --id 170 >/dev/null 2>&1
  for _ in $(seq 1 120); do [ "$(ls /sys/class/ublk-char 2>/dev/null|wc -l)" -eq 0 ] && break; sleep 0.5; done
  pkill -9 -x blobdev >/dev/null 2>&1
  sleep 2
}

pkill -9 -x blobdev >/dev/null 2>&1; sleep 3
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W $MNT
V=orc$RANDOM

set_profile none
if ! start_dev --create-size 512M --max-age 2 first; then
  echo "could not start; SKIP"; exit 1
fi
mkfs.ext4 -q -F /dev/ublkb170 2>/dev/null
for _ in $(seq 1 90); do mount /dev/ublkb170 $MNT 2>/dev/null && break; sleep 1; done
head -c 33554432 /dev/urandom > $MNT/a.bin
sync; sleep 6
WANT=$(sha256sum $MNT/a.bin | awk '{print $1}')

# The store goes away and something syncs.
set_profile reset
head -c 8388608 /dev/urandom > $MNT/b.bin 2>/dev/null
sync 2>/dev/null
sleep 3
# ext4 does not switch the mount to read-only when the journal aborts; it
# stays "rw" and the failure surfaces on the next write. What matters is that
# the write fails rather than being reported as done.
if dd if=/dev/zero of=$MNT/probe.bin bs=1M count=1 oflag=direct 2>/dev/null; then
  bad "a write during the outage was reported as successful"
else
  ok "writes fail during the outage instead of being acknowledged"
fi
if dmesg 2>/dev/null | grep -q "Aborting journal on device ublkb170"; then
  ok "ext4 aborted the journal, which is why the remount needs a restart"
else
  bad "no journal abort in dmesg; the flaky-store profile may not have bitten"
fi

# The stale procedure: umount then mount, without restarting the daemon.
set_profile none
sleep 3
umount $MNT 2>/dev/null || umount -l $MNT 2>/dev/null
if mount /dev/ublkb170 $MNT 2>/dev/null; then
  umount $MNT 2>/dev/null
  bad "mounting without restarting the daemon worked; the README could be simpler"
else
  ok "mounting without restarting the daemon fails, as the README now says"
fi

# The documented procedure.
stop_dev
if start_dev "" again; then
  if mount /dev/ublkb170 $MNT 2>/dev/null; then
    GOT=$(sha256sum $MNT/a.bin 2>/dev/null | awk '{print $1}')
    [ "$WANT" = "$GOT" ] && ok "remounted after a restart, data identical" \
                         || bad "data changed across the outage"
    head -c 4194304 /dev/urandom > $MNT/z.bin 2>/dev/null \
      && ok "writes work again" || bad "writes still fail"
    umount $MNT 2>/dev/null
  else
    bad "could not mount after restarting the daemon"
  fi
else
  bad "could not restart the daemon"
fi
stop_dev
set_profile none
rm -rf $W $MNT

echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && echo "PASS: outage recovery" || exit 1
