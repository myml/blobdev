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
# The unit limits restarts, so a previous failing run has left a start-limit
# record that blocks the next start with "Start request repeated too quickly".
# Clearing it is part of setting up a clean run.
systemctl reset-failed blobdev@deploytest blobdev@deploybad >/dev/null 2>&1
rm -rf /etc/blobdev /var/lib/blobdev /etc/systemd/system/blobdev@.service

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

# The unit runs with ProtectSystem=full and ReadWritePaths=/var/lib/blobdev
# /var/cache/blobdev, so the volume has to live under one of those. Putting it
# anywhere else fails at step NAMESPACE before blobdev runs a line, which
# looks like a broken binary rather than a directory in the wrong place.
install -d /var/lib/blobdev /var/cache/blobdev
cat > /etc/blobdev/deploytest.conf <<CONFEOF
BLOBDEV_ARGS="--backend dir --dir /var/lib/blobdev/o --volume deploytest --meta /var/lib/blobdev/v.meta --id 7 --create-size 256M"
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

# A misconfiguration must stop retrying instead of looping. A misspelled flag
# fails identically every time, and systemd's own default lets it try five
# times first.
cat > /etc/blobdev/deploybad.conf <<CONFEOF
BLOBDEV_ARGS="--backend dir --dir /var/lib/blobdev/o --volume deploybad --meta /var/lib/blobdev/v2.meta --id 8 --create-size 64M --totally-bogus x"
CONFEOF
systemctl start blobdev@deploybad >/dev/null 2>&1
sleep 14
STATE=$(systemctl is-active blobdev@deploybad 2>/dev/null)
N=$(systemctl show blobdev@deploybad -p NRestarts --value 2>/dev/null)
if [ "$STATE" = "failed" ] && [ "${N:-9}" -le 2 ]; then
  ok "a bad configuration stops after $N retries instead of looping"
else
  bad "a bad configuration is still $STATE after $N restarts"
fi
systemctl reset-failed blobdev@deploybad >/dev/null 2>&1
rm -f /etc/blobdev/deploybad.conf

systemctl disable blobdev@deploytest >/dev/null 2>&1
rm -rf /etc/blobdev /var/lib/blobdev /var/cache/blobdev
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && echo "PASS: systemd deploy" || exit 1
