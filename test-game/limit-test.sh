#!/bin/sh
# Tests limits end to end on the Linux build of the test game: find gold, save
# it, limit it, then "respawn" (the player moves to a new object and the old
# one is freed) and check the limit finds gold again by itself.
# Runs in its own Ferret session. Run on the host.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
export FERRET_SESSION=limit-test
live="$here/test-game/live.sh"
game_dir="$here/run/limit-test-game"
cmd="$game_dir/cmd"
log="$game_dir/log"
rm -rf "$game_dir" && mkdir -p "$game_dir"
profile="$HOME/.var/app/io.github.Petru101.Ferret/data/profiles/target.profile"
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

"$here/test-game/target" "$cmd" "$log" &
until [ -s "$log" ]; do sleep 0.2; done
pid=$!
"$live" start >/dev/null
sleep 2
"$live" attach "$pid"
"$live" scan 1000
game "earn 37"
"$live" next 1037
game "spend 10"
"$live" next 1027
"$live" save gold
"$live" limit gold 1100
game "earn 500"
game "show"
"$live" values
echo "--- respawn: new player object, old one freed"
game "respawn"
sleep 1
"$live" values
echo "--- waiting for the limit to find gold again"
sleep 8
"$live" values
game "earn 500"
game "show"
"$live" values
