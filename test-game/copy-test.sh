#!/bin/sh
# Tests that a search ending on a display copy isn't taken as found (it stays listed for the
# player to try, with the reason): food is a 16-bit short (left out of the search here with
# `types`) with a float copy for the HUD that the game refreshes every tick, so typed numbers
# for food end on the copy, which a test write can't change. Gold (a plain int) must still be
# found, and the test write on it undone. Run on the host.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
export FERRET_SESSION=copy-test
live="$here/test-game/live.sh"
d="$here/run/copy-test-game"
rm -rf "$d" && mkdir -p "$d"
fail() { echo "FAIL: $*" >&2; exit 1; }
game() {
    printf '%s\n' "$1" > "$d/cmd.tmp" && mv "$d/cmd.tmp" "$d/cmd"
    sleep 1
    echo "   [game] $1 -> $(tail -n1 "$d/log")"
}
cleanup() {
    printf 'quit 1\n' > "$d/cmd" 2>/dev/null || true
    "$live" stop >/dev/null 2>&1 || true
}
trap cleanup EXIT
# Stack leftovers (printf's copy of the last values) follow the values too: keep the heap match.
keep_heap() {
    loc=$("$live" list | grep '^0x' | grep -v '^0x7ff' | head -n1 | cut -d' ' -f1)
    "$live" keep "${loc%%:*}" >/dev/null
}
# Typed numbers can probe or test-write for a while: wait for the final line.
type_n() {
    out="$here/run/$FERRET_SESSION/out"
    before=$(wc -l < "$out")
    "$live" type "$1" >/dev/null
    until tail -n +"$((before + 1))" "$out" | grep -q "^found it\|^error\|candidates\? left"; do sleep 0.3; done
    tail -n +"$((before + 1))" "$out"
}
rm -f "$d/cmd"
"$here/test-game/target" "$d/cmd" "$d/log" &
pid=$!
until [ -s "$d/log" ]; do sleep 0.2; done
"$live" start >/dev/null
sleep 2
"$live" attach "$pid" >/dev/null
"$live" types i32,f32,f64,xor >/dev/null
echo "--- food: only its display copy can be found"
# Wait for each outcome: food changed during a test write looks like the player eating.
food=500
type_n 500 | tee "$d/out"
if ! grep -q "put a test value back.*Kept: 1 candidate left" "$d/out"; then
    game "eat 7"
    type_n 507
    keep_heap
    game "eat 3"
    food=510
    type_n 510 | tee "$d/out"
fi
grep -q "put a test value back.*Kept: 1 candidate left" "$d/out" || fail "the copy was accepted"
game "show"
tail -n1 "$d/log" | grep -q "food=$food " || fail "food changed"
echo "--- gold: a real value is still found"
"$live" type 1000 >/dev/null
game "earn 5"
"$live" type 1005
keep_heap
game "earn 7"
gold=1012
type_n "$gold" | tee "$d/out"
grep -q "found it" "$d/out" || fail "gold not found"
game "show"
tail -n1 "$d/log" | grep -q "gold=$gold " || fail "the test write on gold wasn't undone"
echo PASS
