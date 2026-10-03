#!/bin/sh
# Tests telling the real value from copies when the screen never shows a test write (Lumencraft
# only redraws a number when it changes it itself): every candidate gets its own test value, the
# game changes the number, and the one that carried on from its test value is the real one. The
# CLI watches no screen, so the first check can't tell. Gold must be found and keep what was earned meanwhile. Run on the host.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
export FERRET_SESSION=probe-test
live="$here/test-game/live.sh"
d="$here/run/probe-test-game"
out="$here/run/$FERRET_SESSION/out"
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
rm -f "$d/cmd"
"$here/test-game/target" "$d/cmd" "$d/log" &
pid=$!
until [ -s "$d/log" ]; do sleep 0.2; done
"$live" start >/dev/null
sleep 2
"$live" attach "$pid" >/dev/null
"$live" type 1000 | tail -n1
game "earn 5"
"$live" type 1005 | tail -n1
game "earn 7"
"$live" type 1012 | tail -n1
# Gold's stack copies follow a test write, which already settles it: pair gold with a decoy
# holding the same number that nothing follows (hp, the int before it).
gold=$("$live" list | grep '^0x' | grep -v '^0x7ff' | head -n1 | cut -d' ' -f1)
gold=${gold%%:*}
hp=$(printf '%x' $((gold - 4)))
"$live" write "$hp" 1012 | tail -n1
"$live" track "$gold" "$hp"
# The same places left a few times in a row: the check runs, and waits for the game.
for _ in 1 2 3 4; do
    before=$(wc -l < "$out")
    printf 'type 1012\n' > "$here/run/$FERRET_SESSION/in"
    until tail -n +"$((before + 1))" "$out" | grep -q "waiting up to\|^found it\|^error\|candidates left"; do sleep 0.3; done
    tail -n +"$((before + 1))" "$out" | grep -q "waiting up to" && break
    tail -n +"$((before + 1))" "$out" | grep -q "^found it\|^error" && { tail -n +"$((before + 1))" "$out"; fail "settled without the in-game check"; }
done
game "earn 3"
until tail -n +"$((before + 1))" "$out" | grep -q "^found it\|^error\|candidates left"; do sleep 0.3; done
tail -n +"$((before + 1))" "$out" | tee "$d/out"
grep -q "^found it" "$d/out" || fail "gold not found"
game "show"
tail -n1 "$d/log" | grep -q "gold=1015 " || fail "the test write on gold wasn't undone (or the 3 earned were lost)"
echo PASS
