#!/bin/sh
# Tests searching for numbers shown with decimals and as times: energy (a float, 1.50 at start)
# is found from typed "1.5", "1.7" (1.75 cut off), "2.0" (2.05 rounded), 2.1, 2.2, then saved,
# set to 3.5 and kept between 1.25 and 4.5; gold (an int) typed as if shown in tenths ("100.0"
# for 1000) is saved with its decimal, shown, set and limited in tenths; a time "0:20" is a
# search for 20 seconds, which finds wood (a double, 12 at start + 8). Run on the host.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
export FERRET_SESSION=decimal-test
live="$here/test-game/live.sh"
d="$here/run/decimal-test-game"
profile=~/.var/app/io.github.Petru101.Ferret/data/profiles/target.profile
rm -f "$profile"
rm -rf "$d" && mkdir -p "$d"
fail() { echo "FAIL: $*" >&2; exit 1; }
game() {
    printf '%s\n' "$1" > "$d/cmd.tmp" && mv "$d/cmd.tmp" "$d/cmd"
    sleep 1
    echo "   [game] $1 -> $(tail -n1 "$d/log")"
}
type_n() {
    out="$here/run/$FERRET_SESSION/out"
    before=$(wc -l < "$out")
    "$live" type "$1" >/dev/null
    until tail -n +"$((before + 1))" "$out" | grep -q "^found it\|^error\|candidates left"; do sleep 0.3; done
    tail -n +"$((before + 1))" "$out"
}
cleanup() {
    printf 'quit 1\n' > "$d/cmd" 2>/dev/null || true
    "$live" stop >/dev/null 2>&1 || true
    rm -f "$profile"
}
trap cleanup EXIT
"$here/test-game/target" "$d/cmd" "$d/log" &
pid=$!
until [ -s "$d/log" ]; do sleep 0.2; done
"$live" start >/dev/null
sleep 2
"$live" attach "$pid" >/dev/null
echo "--- energy shown as 1.5, 1.7, 2.0"
type_n 1.5
game "gain 0.25"
type_n 1.7
game "gain 0.3"
type_n 2.0
# The game's running maximum of energy follows it too: two more numbers, then the probe
# tells them apart.
for e in 2.1 2.2; do
    game "gain 0.1"
    type_n $e | tee "$d/out"
    grep -q "found it" "$d/out" && break
done
grep -q "found it" "$d/out" || fail "energy not found"
loc=$(grep "found it" "$d/out" | sed 's/found it at \(0x[0-9a-f]*\).*/\1/')
energy=$(tail -n1 "$d/log" | sed 's/.*energy=\([0-9.]*\).*/\1/')
got=$("$live" peek "$loc:f32")
v=$(echo "$got" | sed -n 's/.* = //p')
awk -v a="$v" -v b="$energy" 'BEGIN { exit (a - b < 0.001 && b - a < 0.001) ? 0 : 1 }' || fail "found the wrong value (game: $energy, $got)"
echo "--- energy: save, set 3.5, keep between 1.25 and 4.5"
"$live" save energy | tail -n1
"$live" set energy 3.5 >/dev/null
game show
tail -n1 "$d/log" | grep -q "energy=3.50" || fail "energy not set to 3.5"
"$live" limit energy 1.25 4.5 | tail -n1
game "gain 5"
sleep 1
game show
tail -n1 "$d/log" | grep -q "energy=4.50" || fail "energy not kept at 4.5 at most"
"$live" values | grep "^energy" | tee "$d/out"
grep -q "^energy *4.5 .*kept between 1.25 and 4.5" "$d/out" || fail "values doesn't show energy as 4.5"
echo "--- gold shown in tenths: 100.0 is 1000"
"$live" reset >/dev/null
type_n 100.0 | tail -n1
game "earn 37"
type_n 103.7 | tail -n1
game "spend 10"
type_n 102.7 | tee "$d/out" | tail -n1
grep -q "found it" "$d/out" || fail "gold not found from tenths"
"$live" save gold | tail -n1
grep -q "^decimals 1" "$profile" || fail "gold saved without its decimal"
"$live" values | grep "^gold" | tee "$d/out"
grep -q "^gold *102.7 " "$d/out" || fail "values doesn't show gold as 102.7"
"$live" set gold 120.3 >/dev/null
game show
tail -n1 "$d/log" | grep -q "gold=1203 " || fail "gold not set to 120.3"
"$live" set gold 1.25 | tee "$d/out"
grep -q "one decimal at most" "$d/out" || fail "gold took two decimals"
"$live" limit gold - 130 | tail -n1
game "earn 500"
sleep 1
game show
tail -n1 "$d/log" | grep -q "gold=1300 " || fail "gold not kept at 130.0 at most"
"$live" values | grep "^gold"
echo "--- a time: 0:20 is 20 seconds"
"$live" reset >/dev/null
game "wood 8"
type_n 0:20 | tee "$d/out"
grep -q "typed 0:20" "$d/out" || fail "time not taken"
echo PASS
