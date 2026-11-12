#!/bin/bash
# Every flag refuses what it cannot mean, and says why.
#
# Found by feeding the CLI values nobody would type on purpose. Three of them
# used to get through:
#
#   --create-size 18446744073709551615T  wrapped to a 16 PiB volume
#   --id 1048576                         reached the kernel, which said
#                                        EOPNOTSUPP and the daemon blamed a
#                                        leftover device
#   --queues 0 / --depth 0               same unhelpful kernel error
#   --full-pct 101                       accepted; 0 already means "never"
#
# None of these are data loss. All of them send the reader somewhere else.
#
# usage: args.sh
set -u
BIN=${BIN:-/usr/local/bin/blobdev}
W=${W:-/root/bd-test/args}
rm -rf "$W"; mkdir -p "$W"
PASS=0
FAIL=0

# expect_reject <label> <args...>: must exit non-zero and mention <expect>
expect_reject() {
  local label=$1 expect=$2; shift 2
  local out rc
  out=$(timeout 10 "$BIN" "$@" 2>&1)
  rc=$?
  # Keep the command's own status: a pipe would report the last stage's.
  if [ "$rc" -eq 0 ]; then
    echo "  FAIL $label: accepted (expected a refusal)"
    FAIL=$((FAIL + 1))
  elif ! grep -qF -e "$expect" <<<"$out"; then
    echo "  FAIL $label: refused, but said: $(grep -vE '^\[' <<<"$out" | head -1 | cut -c1-60)"
    FAIL=$((FAIL + 1))
  else
    echo "  ok   $label"
    PASS=$((PASS + 1))
  fi
}

# No --create-size here: the size cases add their own, and the parser takes
# the first value it sees, so a base one would mask every case below it.
base=(run --backend dir --dir "$W/o" --volume a$RANDOM --meta "$W/a.meta")

echo "== sizes"
expect_reject "overflow with T" "is too large" \
  "${base[@]}" --create-size 18446744073709551615T
expect_reject "overflow with G" "is too large" \
  "${base[@]}" --create-size 17179869184G
for bad in abc 1.5G -1 ""; do
  expect_reject "size [$bad]" "bad size" "${base[@]}" --create-size "$bad"
done
expect_reject "size 0" "zero dimension" "${base[@]}" --create-size 0

echo "== device id"
expect_reject "id -2" "must be -1" "${base[@]}" --create-size 64M --id -2
expect_reject "id 1048576" "must be -1" "${base[@]}" --create-size 64M --id 1048576
expect_reject "id 4294967295" "must be -1" "${base[@]}" --create-size 64M --id 4294967295
expect_reject "id not a number" "bad --id" "${base[@]}" --create-size 64M --id x

echo "== queue shape"
expect_reject "queues 0" "at least 1" "${base[@]}" --create-size 64M --queues 0
expect_reject "depth 0" "at least 1" "${base[@]}" --create-size 64M --depth 0

echo "== policy"
expect_reject "full-pct 101" "0..=100" "${base[@]}" --create-size 64M --full-pct 101
expect_reject "unknown backend" "unknown backend" \
  run --backend nope --volume x --create-size 1G

echo "== unknown options"
# An option nobody reads used to be ignored without a word: --cache-sise 128M
# ran with the cache at its default. Every misspelling below had to be found
# by reading the source.
expect_reject "cache-sise" "did you mean --cache-size" \
  "${base[@]}" --create-size 64M --cache-sise 128M
expect_reject "volum" "did you mean --volume" \
  "${base[@]}" --create-size 64M --volum x
expect_reject "backnd" "did you mean --backend" \
  "${base[@]}" --create-size 64M --backnd dir
expect_reject "totally bogus" "unknown option --totally-bogus" \
  "${base[@]}" --create-size 64M --totally-bogus x

echo "== repeated flags"
# `arg` takes the first match, so a duplicate used to be ignored silently --
# and silently ignoring a --create-size is how a test of bad sizes passed.
expect_reject "create-size twice" "more than once" \
  "${base[@]}" --create-size 64M --create-size 128M
expect_reject "id twice" "more than once" \
  "${base[@]}" --create-size 64M --id 3 --id 4

echo "== every option is documented"
# Eight options lived only in --help until someone looked. A new flag that
# nobody documents is invisible to anyone reading the README.
HELP=$("$BIN" --help 2>&1)
UNDOC=""
for f in $(echo "$HELP" | grep -oE "^  --[a-z-]+" | tr -d ' '); do
  grep -q -- "$f" /tmp/README.md 2>/dev/null || UNDOC="$UNDOC $f"
done
if [ -z "$UNDOC" ]; then
  echo "  ok   all help options appear in the README"; PASS=$((PASS + 1))
else
  echo "  FAIL undocumented:$UNDOC"; FAIL=$((FAIL + 1))
fi

echo "== environment"
# A bad value used to fall back silently: BLOBDEV_SUMMARY_SECS=abc looked the
# same as the setting working, and the person watching for the periodic line
# waited for one that was never coming.
BAD=$(BLOBDEV_SUMMARY_SECS=abc timeout 5 "$BIN" run --backend dir --dir "$W/env" \
  --volume env$RANDOM --meta "$W/env.meta" --id -1 --create-size 64M 2>&1 | grep -c "BLOBDEV_SUMMARY_SECS=abc is not a number")
if [ "$BAD" -ge 1 ]; then
  echo "  ok   bad BLOBDEV_SUMMARY_SECS is reported"; PASS=$((PASS + 1))
else
  echo "  FAIL bad BLOBDEV_SUMMARY_SECS was ignored silently"; FAIL=$((FAIL + 1))
fi

echo "== required flags"
expect_reject "no volume" "--volume is required" run --create-size 1G
expect_reject "cache-size without cache-dir" "--cache-size needs --cache-dir" \
  "${base[@]}" --create-size 64M --cache-size 64M

echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && echo "PASS: argument handling" || echo "FAIL: $FAIL case(s)"
