#!/bin/sh
# Tests pointer paths: gems (a double) sits at the end of a chain from a static pointer
# (world -> room -> stats) and is only touched through shared functions, like GameMaker games,
# so save must fall back to a pointer scan. Then the paths must follow gems when the game moves
# its room to new memory, keep a limit on it across that move, and find it again after the game
# restarts. Runs in its own Ferret session. Run on the host.
#   paths-test.sh [target|proton|proton32]
# proton runs target.exe (64-bit), proton32 target32.exe (32-bit, like Forager) under Proton.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
bin=${1:-target}
steam="$HOME/.local/share/Steam"
proton="$steam/steamapps/common/Proton 9.0 (Beta)/proton"
export FERRET_SESSION=paths-test
live="$here/test-game/live.sh"
game_dir="$here/run/paths-test-game"
cmd="$game_dir/cmd"
log="$game_dir/log"
rm -rf "$game_dir" && mkdir -p "$game_dir"
case $bin in
proton) exe=target.exe ;;
proton32) exe=target32.exe ;;
*) exe=$bin ;;
esac
profile="$HOME/.var/app/io.github.Petru101.Ferret/data/profiles/$exe.profile"
rm -f "$profile"

game() {
    printf '%s\n' "$1" > "$cmd.tmp" && mv "$cmd.tmp" "$cmd"
    sleep 1
    echo "   [game] $1 -> $(tail -n1 "$log")"
}
start_game() {
    rm -f "$log"
    if [ "$bin" = target ]; then
        "$here/test-game/$bin" "$cmd" "$log" &
        until [ -s "$log" ]; do sleep 0.2; done
        pid=$!
    else
        mkdir -p "$here/run/prefix"
        export STEAM_COMPAT_DATA_PATH="$here/run/prefix" STEAM_COMPAT_CLIENT_INSTALL_PATH="$steam"
        winpath() { printf 'Z:%s' "$1" | tr / '\\'; }
        "$proton" run "$here/test-game/$exe" "$(winpath "$cmd")" "$(winpath "$log")" >"$game_dir/proton.log" 2>&1 &
        i=0
        until [ -s "$log" ]; do i=$((i + 1)); [ $i -gt 900 ] && { echo "game did not start" >&2; exit 1; }; sleep 0.2; done
        pid=$(pgrep -n -x "$exe")
    fi
}
stop_game() {
    printf 'quit 1\n' > "$cmd" 2>/dev/null || true
    sleep 1.5
}
# Save watches the value, scans and re-checks the paths: wait until it is really done.
save_gems() {
    out="$here/run/$FERRET_SESSION/out"
    done_before=$(grep -c "^saved gems\|^error" "$out" || true)
    "$live" save gems >/dev/null
    until [ "$(grep -c "^saved gems\|^error" "$out" || true)" -gt "$done_before" ]; do sleep 0.5; done
    echo "> save gems"
    awk '/^> save gems/ { buf = ""; next } { buf = buf $0 "\n" } END { printf "%s", buf }' "$out"
}
# The other matches are copies (on the stack); with no screen to read, probe can't tell them
# apart, so ask the game: write a marker to each and see which one it reports.
keep_real_match() {
    for loc in $("$live" list | grep '^0x' | cut -d' ' -f1); do
        "$live" write "$loc" 4242 >/dev/null
        game "show" >/dev/null
        "$live" write "$loc" "$1" >/dev/null
        if tail -n1 "$log" | grep -q "gems=4242"; then
            "$live" keep "${loc%%:*}"
            return
        fi
    done
    echo "no match is the real gems" >&2
    exit 1
}
cleanup() {
    stop_game
    "$live" stop >/dev/null 2>&1 || true
    rm -f "$profile"
}
trap cleanup EXIT

start_game
"$live" start >/dev/null
sleep 2
"$live" attach "$pid"
echo "--- gems 77"
"$live" scan 77
game "gems 5"
"$live" next 82
game "gems 4"
"$live" next 86
game "coins 3"
game "gems -2"
"$live" next 84
"$live" list
keep_real_match 84
save_gems
grep -A3 "^entry gems" "$profile" || true
echo "--- the game moves gems to new memory: the paths follow it"
game "newroom"
game "gems 6"
"$live" values
echo "--- limit gems to at most 95, then go past it, move it, go past it again"
"$live" limit gems 95
game "gems 20"
game "show"
game "newroom"
game "gems 30"
game "show"
"$live" values
echo "--- restart the game: the saved paths find gems again (fresh game: 77)"
stop_game
start_game
"$live" attach "$pid"
game "gems 1"
"$live" values
game "gems 50"
game "show"
"$live" values
echo "--- find gems again and save it again: the saved paths that lead to it are kept"
"$live" limit gems off
game "gems 7"
"$live" scan 102
game "gems 1"
"$live" next 103
keep_real_match 103
save_gems
grep -A3 "^entry gems" "$profile" || true
