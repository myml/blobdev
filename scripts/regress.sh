#!/bin/bash
# Run the whole suite in order, on a clean host, and report one verdict.
#
# Nineteen rounds of changes have gone in one at a time, each verified on its
# own. This runs them together: the backends, the failure modes, the boundary
# shapes, crash recovery. Serially, because two of these running at once is
# how earlier rounds produced failures that were the harness rather than the
# code.
#
# Every step gets a clean device table and mount point first, so one step
# cannot poison the next.
#
# usage: regress.sh [--quick]
set -u
QUICK=${1:-}
BIN=${BIN:-/usr/local/bin/blobdev}
MNT=${MNT:-/mnt/nc}
W=/root/bd-test/regress
mkdir -p "$W"
RESULTS=$W/results.txt
: > "$RESULTS"

clean() {
  pkill -9 -x blobdev >/dev/null 2>&1
  sleep 2
  umount -l "$MNT" 2>/dev/null
  umount -l /mnt/blobdev 2>/dev/null
  "$BIN" reap >/dev/null 2>&1
  sleep 1
}

record() {
  local name=$1 status=$2 detail=${3:-}
  printf '%-34s %-6s %s\n' "$name" "$status" "$detail" | tee -a "$RESULTS"
}

run_step() {
  local name=$1; shift
  clean
  # The scripts default BIN to a relative path; from here that would be wrong.
  export BIN
  local log=$W/$(echo "$name" | tr ' /' '__').log
  local t0 t1
  t0=$(date +%s)
  if timeout 900 bash "$@" > "$log" 2>&1; then
    t1=$(date +%s)
    record "$name" PASS "$((t1 - t0))s"
  else
    t1=$(date +%s)
    local why
    why=$(grep -iE "^FAIL|^blobdev:|error" "$log" 2>/dev/null | head -1 | cut -c1-70)
    record "$name" FAIL "$((t1 - t0))s ${why:-}"
  fi
  clean
}

echo "== backends"
run_step "e2e dir" /tmp/e2e-dir.sh
run_step "e2e s3" /tmp/e2e-s3.sh
run_step "e2e webdav (simple server)" /tmp/e2e-webdav.sh
run_step "e2e nextcloud" /tmp/nextcloud-e2e.sh

echo "== failure modes"
run_step "outage handling" /tmp/outage.sh
run_step "crash recovery" /tmp/crash-recovery.sh 2 8
run_step "boundaries" /tmp/boundaries.sh dir
# The unit an operator will actually use, run the way they will use it.
run_step "systemd unit" /tmp/systemd-check.sh 3

if [ "$QUICK" != "--quick" ]; then
  # A step that needs environment of its own.
run_step_env() {
  local name=$1 envs=$2 backend=$3 script=$4
  clean
  export BIN
  local log=$W/$(echo "$name" | tr ' /' '__').log
  local t0 t1
  t0=$(date +%s)
  if timeout 900 env $envs BACKEND_ARGS="$backend" bash "$script" > "$log" 2>&1; then
    t1=$(date +%s); record "$name" PASS "$((t1 - t0))s"
  else
    t1=$(date +%s)
    local why
    why=$(grep -iE "^FAIL|^blobdev:|error" "$log" 2>/dev/null | head -1 | cut -c1-70)
    record "$name" FAIL "$((t1 - t0))s ${why:-}"
  fi
  clean
}

echo "== load"
  run_step "concurrency 4 writers" /tmp/concurrent.sh 60 4
  run_step_env "stress 2 min" \
    "ROOT=/root/bd-test/regress/stress SIZE=512M MB=64 SECONDS_TO_RUN=120" \
    "--backend dir --dir /root/bd-test/regress/objs" /tmp/stress.sh
fi

echo
echo "== summary"
cat "$RESULTS"
FAILS=$(grep -c ' FAIL ' "$RESULTS" 2>/dev/null | head -1)
TOTAL=$(grep -c . "$RESULTS" 2>/dev/null | head -1)
FAILS=${FAILS:-0}
TOTAL=${TOTAL:-0}
echo "TOTAL=$TOTAL FAILED=$FAILS"
if [ "$FAILS" -eq 0 ]; then echo "PASS: full regression"; else echo "FAIL: $FAILS step(s) failed"; fi
