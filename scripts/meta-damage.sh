#!/bin/bash
# What happens when the metadata file is damaged or missing?
#
# The metadata is the only record of which objects make up the volume. A
# truncated, corrupted, or deleted file is the worst thing that can happen
# to it, and the response should be to refuse, not to start empty.
set -u
BIN=/usr/local/bin/blobdev
W=/root/bd-test/mc
PASS=0; FAIL=0
ok()  { echo "  ok   $*"; PASS=$((PASS + 1)); }
bad() { echo "  FAIL $*"; FAIL=$((FAIL + 1)); }

pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
$BIN reap >/dev/null 2>&1
rm -rf $W; mkdir -p $W

# The plain start, as an operator would run it after a restart. It must not
# carry --create-size: that is a request to make an empty volume, and the
# questions below are all about what happens without it.
boot() { timeout 6 $BIN run --backend dir --dir $W/o --volume mc --meta $W/v.meta --id 217 "$@" 2>&1 | grep -vE "INFO" | head -2; }
create() { timeout 6 $BIN run --backend dir --dir $W/o --volume mc --meta $W/v.meta --id 217 --create-size 256M 2>&1 | grep -vE "INFO" | head -2; }
create >/dev/null 2>&1
pkill -9 -x blobdev >/dev/null 2>&1; sleep 2
ORIG=$(sha256sum $W/v.meta | awk '{print $1}')

echo "== truncated metadata"
head -c 20 $W/v.meta > $W/t && mv $W/t $W/v.meta
OUT=$(boot)
if [ -n "$OUT" ]; then ok "truncated metadata is refused: $(echo "$OUT" | head -1 | cut -c1-60)"
else bad "a truncated metadata file was accepted"; fi
pkill -9 -x blobdev >/dev/null 2>&1; sleep 2

echo "== corrupted metadata (byte flipped)"
cp $W/v.meta $W/bad 2>/dev/null
printf 'X' | dd of=$W/bad bs=1 seek=40 conv=notrunc 2>/dev/null
mv $W/bad $W/v.meta
OUT=$(boot)
if [ -n "$OUT" ]; then ok "corrupted metadata is refused: $(echo "$OUT" | head -1 | cut -c1-60)"
else bad "corrupted metadata was accepted"; fi
pkill -9 -x blobdev >/dev/null 2>&1; sleep 2

echo "== deleted metadata is refused, and says how to recover"
# Not "always refused": passing --create-size is a deliberate request for an
# empty volume, and that still works. Without it, a missing metadata file must
# not turn into a fresh volume -- the objects in the store are still the old
# volume's data, and a silent recreate orphans them.
rm -f $W/v.meta
OUT=$(boot)
case "$OUT" in
  *"has no metadata"*"recover"*) ok "a deleted metadata file is refused with recovery advice" ;;
  "") bad "a deleted metadata file was silently replaced with a new volume" ;;
  *)  bad "refused, but without recovery advice: $(echo "$OUT" | head -1 | cut -c1-60)" ;;
esac
[ -f $W/v.meta ] && bad "a metadata file was created anyway" || ok "nothing was created"
pkill -9 -x blobdev >/dev/null 2>&1; sleep 2

echo "== empty metadata"
: > $W/v.meta
OUT=$(boot)
if [ -n "$OUT" ]; then ok "empty metadata is refused: $(echo "$OUT" | head -1 | cut -c1-60)"
else bad "an empty metadata file was accepted"; fi
pkill -9 -x blobdev >/dev/null 2>&1

echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && echo "PASS: metadata damage" || exit 1
