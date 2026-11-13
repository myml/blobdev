#!/bin/bash
# Do the README's install steps produce a working service?
#
# Written because the five-line quick install in the README did not: it copied
# the example config into /etc/blobdev before creating that directory, and cp
# failed. Verifying the commands a reader will paste is worth more than
# reading them.
set -u
UNIT=/tmp/blobdev@.service
CONF=/tmp/blobdev.conf.example
PASS=0; FAIL=0
ok()  { echo "  ok   $*"; PASS=$((PASS + 1)); }
bad() { echo "  FAIL $*"; FAIL=$((FAIL + 1)); }

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
systemctl stop blobdev@deploytest >/dev/null 2>&1
rm -rf /etc/blobdev /var/lib/blobdev-deploy /etc/systemd/system/blobdev@.service

# The steps as the README now writes them.
install -m 0644 "$UNIT" /etc/systemd/system/ || bad "unit install"
install -d /etc/blobdev
if cp "$CONF" /etc/blobdev/deploytest.conf 2>/dev/null; then
  ok "example config copies once the directory exists"
else
  bad "could not copy the example config"
fi
chmod 600 /etc/blobdev/deploytest.conf
[ "$(stat -c %a /etc/blobdev/deploytest.conf)" = "600" ] \
  && ok "config is 0600" || bad "config permissions are $(stat -c %a /etc/blobdev/deploytest.conf)"

install -d /var/lib/blobdev-deploy
cat > /etc/blobdev/deploytest.conf <<CONFEOF
BLOBDEV_ARGS="--backend dir --dir /var/lib/blobdev-deploy/o --volume deploytest --meta /var/lib/blobdev-deploy/v.meta --id 7 --create-size 256M"
CONFEOF
chmod 600 /etc/blobdev/deploytest.conf
systemctl daemon-reload
if systemctl start blobdev@deploytest 2>/dev/null && sleep 6 && systemctl is-active -q blobdev@deploytest; then
  ok "service starts"
else
  bad "service did not start"
fi
[ -e /dev/ublkb7 ] && ok "device node exists" || bad "no /dev/ublkb7"

# The stop timeout has to cover one object's worst case, or a slow store turns
# every stop into a SIGKILL mid-upload.
SHOW=$(systemctl show blobdev@deploytest -p TimeoutStopUSec --value 2>/dev/null)
case "$SHOW" in
  5min|300000000) ok "TimeoutStopSec covers a 241 s worst-case object upload" ;;
  *)              bad "TimeoutStopSec is $SHOW, short of the 241 s worst case" ;;
esac

systemctl restart blobdev@deploytest
sleep 6
systemctl is-active -q blobdev@deploytest && [ -e /dev/ublkb7 ] \
  && ok "restart brings the device back" || bad "restart left the service down"

systemctl stop blobdev@deploytest
sleep 4
[ "$(ls /sys/class/ublk-char 2>/dev/null | wc -l)" -eq 0 ] \
  && ok "stop leaves no stale device" || bad "stale devices after stop"

systemctl disable blobdev@deploytest >/dev/null 2>&1
rm -rf /etc/blobdev /var/lib/blobdev-deploy /var/cache/blobdev
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && echo "PASS: systemd deploy" || exit 1
