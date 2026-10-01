#!/bin/sh
# Tests that Ferret doesn't write through pointer paths that disagree: gems is saved through
# pointer paths, then its profile is changed to one real path plus two copies of a wrong one
# (gems' path with the last offset +8 = ore). The vote then points at ore, 2 to 1: not clear.
# Set must refuse, and a limit must pause instead of writing ore (or gems). Then with the real
# path given three times the vote is clear again and both work. Run on the host.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
export FERRET_SESSION=doubt-test
live="$here/test-game/live.sh"
game_dir="$here/run/doubt-test-game"
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
cleanup() {
    printf 'quit 1\n' > "$cmd" 2>/dev/null || true
    "$live" stop >/dev/null 2>&1 || true
    rm -f "$profile"
}
trap cleanup EXIT

"$here/test-game/target" "$cmd" "$log" &
pid=$!
until [ -s "$log" ]; do sleep 0.2; done
"$live" start >/dev/null
sleep 2
"$live" attach "$pid" >/dev/null
"$live" scan 77 >/dev/null
game "gems 5"
"$live" next 82 >/dev/null
game "gems 4"
"$live" next 86 >/dev/null
for loc in $("$live" list | grep '^0x' | cut -d' ' -f1); do
    "$live" write "$loc" 4242 >/dev/null
    game "show" >/dev/null
    "$live" write "$loc" 86 >/dev/null
    if tail -n1 "$log" | grep -q " gems=4242"; then "$live" keep "${loc%%:*}" >/dev/null; break; fi
done
out="$here/run/$FERRET_SESSION/out"
"$live" save gems >/dev/null
until grep -q "^saved gems\|^error" "$out"; do sleep 0.5; done
grep "^saved gems\|^error" "$out"
real=$(grep -m1 '^candidate ' "$profile" | cut -d' ' -f2-)
last=${real##* }
wrong="${real% *} $(printf '%x' $((0x$last + 8)))"
echo "real path: $real   wrong path: $wrong"
write_profile() {
    { echo "entry gems"; echo "type f64"; echo "run 1"; for p in "$@"; do echo "candidate $p"; done; } > "$profile"
}

echo "--- paths disagree (1 real, 2 wrong)"
write_profile "$real" "$wrong" "$wrong"
"$live" restore
"$live" values | tee "$game_dir/values"
grep -q "not written" "$game_dir/values" || fail "values doesn't say it won't write"
"$live" set gems 500 | tee "$game_dir/set"
grep -q "^error: not written: its pointer paths disagree" "$game_dir/set" || fail "set wrote"
"$live" limit gems 95 || true
game "gems 20"
"$live" values | tee "$game_dir/values"
grep -q "don't agree" "$game_dir/values" || fail "limit not paused"
game "show"
tail -n1 "$log" | grep -q "gems=106.* ore=" || fail "gems changed"
tail -n1 "$log" | grep -q "ore=95\b" && fail "the limit wrote ore"
"$live" limit gems off >/dev/null

echo "--- paths agree (3 real)"
write_profile "$real" "$real" "$real"
"$live" restore
"$live" set gems 90 | tee "$game_dir/set"
grep -q "^error" "$game_dir/set" && fail "set refused"
game "show"
tail -n1 "$log" | grep -q "gems=90" || fail "gems not 90"
"$live" limit gems 95 >/dev/null
game "gems 20"
game "show"
tail -n1 "$log" | grep -q "gems=95" || fail "limit didn't hold gems at 95"
echo "PASS"
