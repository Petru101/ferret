#!/bin/sh
# Tests named paths on the test game's Unity-style inventory (like Valheim's wood): logs are
# found, saved by name ("Inventory" -> every "$item_logs" stack), set and limited in every stack
# but not in the chest's, followed into a new inventory after "die", and found again by name
# after a restart. Runs in its own Ferret session. Run on the host.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
export FERRET_SESSION=names-test
live="$here/test-game/live.sh"
out="$here/run/$FERRET_SESSION/out"
game_dir="$here/run/names-test-game"
cmd="$game_dir/cmd"
log="$game_dir/log"
rm -rf "$game_dir" && mkdir -p "$game_dir"
profile="$HOME/.var/app/io.github.Petru101.Ferret/data/profiles/target.profile"
rm -f "$profile"
fail() { echo "FAIL: $*" >&2; exit 1; }

game() {
    printf '%s\n' "$1" > "$cmd.tmp" && mv "$cmd.tmp" "$cmd"
    sleep 1
    echo "   [game] $1 -> $(tail -n1 "$log")"
}
expect() {
    tail -n1 "$log" | grep -q "$1" || fail "expected $1 in: $(tail -n1 "$log")"
}
start_game() {
    rm -f "$log" "$cmd"
    "$here/test-game/target" "$cmd" "$log" &
    pid=$!
    until [ -s "$log" ]; do sleep 0.2; done
}
cleanup() {
    printf 'quit 1\n' > "$cmd" 2>/dev/null || true
    "$live" stop >/dev/null 2>&1 || true
    rm -f "$profile"
}
trap cleanup EXIT

start_game
"$live" start >/dev/null
sleep 2
"$live" attach "$pid"
echo "--- logs 33"
"$live" scan 33
game "logs 4"
"$live" next 37
game "logs 1"
"$live" next 38
"$live" save logs >/dev/null
until grep -q "^saved logs\|^error" "$out"; do sleep 0.5; done
grep "named path\|^saved logs\|^error" "$out"
grep -q "^saved logs (by name: every \"\$item_logs\" in \"Inventory\"" "$out" || fail "not saved by name"
grep -A3 "^entry logs" "$profile"

echo "--- a second stack: set writes both, not the chest's"
game "stack 20"
"$live" values | tee "$game_dir/values"
grep -q "in 2 places" "$game_dir/values" || fail "values doesn't show 2 places"
"$live" set logs 45
game "show"
expect "logs=45,45 stone=9 chestlogs=50"

echo "--- limit logs to at most 40"
"$live" limit logs 40
game "logs 10"
game "show"
expect "logs=40,40 stone=9 chestlogs=50"

echo "--- die: the stacks move to a new inventory, the limit follows them"
game "die"
game "logs 30"
sleep 12
game "show"
expect "logs=40,40 stone=9 chestlogs=50"
"$live" values
"$live" limit logs off

echo "--- restart: found again by name (fresh game: one stack of 33)"
printf 'quit 1\n' > "$cmd"
i=0
while kill -0 "$pid" 2>/dev/null; do i=$((i + 1)); [ $i -gt 50 ] && fail "the game didn't quit"; sleep 0.2; done
start_game
"$live" attach "$pid" | tee "$game_dir/attach"
grep -q "logs = 33 (found by name" "$game_dir/attach" || fail "not restored by name"
"$live" set logs 77
game "show"
expect "logs=77 stone=9 chestlogs=50"
echo "PASS"
