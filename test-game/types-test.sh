#!/bin/sh
# Tests the value types besides plain integers end to end on a Linux build of
# the test game: energy (a float that grows by fractions and shows cut off to a
# whole number) and scrap (an XOR-encoded integer) are found, saved and limited;
# after a respawn the limits must find both again. Then shield (a double) is
# found and set. Runs in its own Ferret session. Run on the host.
#   types-test.sh [target|target-x87]   (target-x87 reads floats with x87 code)
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
bin=${1:-target}
export FERRET_SESSION=types-test
live="$here/test-game/live.sh"
game_dir="$here/run/types-test-game"
cmd="$game_dir/cmd"
log="$game_dir/log"
rm -rf "$game_dir" && mkdir -p "$game_dir"
profile="$HOME/.var/app/io.github.Petru101.Ferret/data/profiles/$bin.profile"
rm -f "$profile"

game() {
    printf '%s\n' "$1" > "$cmd.tmp" && mv "$cmd.tmp" "$cmd"
    sleep 1
    echo "   [game] $1 -> $(tail -n1 "$log")"
}
cleanup() {
    printf 'quit 1\n' > "$cmd" 2>/dev/null || true
    "$live" stop >/dev/null 2>&1 || true
    rm -f "$profile"
}
trap cleanup EXIT

"$here/test-game/$bin" "$cmd" "$log" &
until [ -s "$log" ]; do sleep 0.2; done
pid=$!
"$live" start >/dev/null
sleep 2
"$live" attach "$pid"
echo "--- energy 1.5 shows as 1"
"$live" scan 1
game "gain 0.9"
"$live" next 2
game "gain 1.3"
"$live" next 3
game "gain 0.6"
"$live" next 4
game "gain 2.2"
"$live" next 6
echo "--- going down leaves the game's highest-energy copy behind"
game "gain -1.4"
"$live" next 5
"$live" list
"$live" save energy
"$live" limit energy 7
game "gain 3.1"
echo "--- scrap 50, XOR-encoded"
"$live" scan 50
game "scrap 17"
"$live" next 67
game "scrap -30"
"$live" next 37
"$live" list
"$live" save scrap
"$live" limit scrap 40
game "scrap 100"
game "show"
"$live" values
echo "--- respawn: new player object, old one freed"
game "respawn"
echo "--- waiting for the limits to find energy and scrap again"
sleep 8
game "gain 9"
game "scrap 100"
game "show"
"$live" values
echo "--- shield 250.25 (a double)"
"$live" scan 250
game "shield 11.5"
"$live" next 261
game "shield -30.2"
"$live" next 231
"$live" list
"$live" set 5000
game "show"
