#!/bin/sh
# Tests the smart scan: after finding one inventory stack, Ferret learns the memory around it, and
# searches for other stacks look in places shaped like it first (the item records are alike): the
# chest's logs and a new stack are the only shaped place holding their number from the first
# scan, and a lone shaped place must follow one change before it counts as found. Run on the host.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
export FERRET_SESSION=shapes-test
live="$here/test-game/live.sh"
d="$here/run/shapes-test-game"
shapes="$HOME/.var/app/io.github.Petru101.Ferret/data/profiles/target.shapes"
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
    rm -f "$shapes"
}
trap cleanup EXIT
# Types the number and waits for the outcome (test writes take a while).
type_n() {
    out="$here/run/$FERRET_SESSION/out"
    before=$(wc -l < "$out")
    printf 'type %s\n' "$1" > "$here/run/$FERRET_SESSION/in"
    until tail -n +"$((before + 1))" "$out" | grep -q "^found it\|^error\|candidates left"; do sleep 0.3; done
    tail -n +"$((before + 1))" "$out"
}
# Finds the number; when stack leftovers (printf's copy of the last values) still match too,
# picks the heap one.
find_n() {
    type_n "$1" > "$d/typed"
    tail -n1 "$d/typed"
    grep -q "^found it" "$d/typed" && return
    loc=$("$live" list | grep '^0x' | grep -v '^0x7ff' | head -n1 | cut -d' ' -f1)
    [ -n "$loc" ] || fail "no heap match"
    "$live" track "$loc" >/dev/null
    type_n "$1" | grep -q "^found it" || fail "$loc isn't it"
}
rm -f "$shapes" "$d/cmd"
"$here/test-game/target" "$d/cmd" "$d/log" &
pid=$!
until [ -s "$d/log" ]; do sleep 0.2; done
"$live" start >/dev/null
sleep 2
"$live" attach "$pid" >/dev/null
echo "--- logs in the inventory: learns the shape"
type_n 33 | tail -n1
game "logs 4"
find_n 37
grep -q . "$shapes" || fail "no shape saved"
cat "$shapes"
echo "--- logs in the chest: one shaped place from the first number, found once it follows"
"$live" reset >/dev/null
type_n 50 | tee "$d/out" | tail -n2
grep -q "shaped like earlier finds" "$d/out" || fail "the scan didn't prefer shaped places"
grep -q "^1 candidates left" "$d/out" || fail "a lone shaped place counted as found before it followed a change"
game "chest 6"
find_n 56
cat "$shapes"
echo "--- a new stack: in a place shaped like the others"
game "stack 71"
"$live" reset >/dev/null
type_n 71 | tee "$d/out"
grep -q "shaped like earlier finds" "$d/out" || fail "the scan didn't prefer shaped places"
grep -q "^1 candidates left" "$d/out" || fail "expected the new stack's place alone"
echo "--- gold, with a logs stack of 1000 in a shaped place: falls back to searching everywhere"
game "stack 1000"
# Undo, not Start Over: clearing a shaped search means it wasn't there, and scans then look
# everywhere until the next find.
"$live" undo >/dev/null
type_n 1000 | tail -n1
game "earn 5"
type_n 1005 | tee "$d/out" | tail -n2
grep -q "searching everywhere" "$d/out" || fail "didn't fall back"
game "earn 7"
find_n 1012
echo PASS
