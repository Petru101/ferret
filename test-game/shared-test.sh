#!/bin/sh
# Tests saving a value that is only in its own heap block and read through shared code, like
# GameMaker games read every variable: coins (a double) is read by one function that also reads
# wood, and directly by the main loop ([register] with no offset; r12 in the 64-bit builds).
# Save must skip the shared function, keep the direct read, and the saved pattern must find
# coins again. Runs in its own Ferret session. Run on the host.
#   shared-test.sh [target|target-x87|proton32]
# proton32 runs target32.exe (i686-w64-mingw32-gcc -O2 -msse2 -mfpmath=sse) under Proton: 32-bit
# code like Forager's, reading coins with movsd xmm2,[edi].
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
bin=${1:-target}
steam="$HOME/.local/share/Steam"
proton="$steam/steamapps/common/Proton 9.0 (Beta)/proton"
export FERRET_SESSION=shared-test
live="$here/test-game/live.sh"
game_dir="$here/run/shared-test-game"
cmd="$game_dir/cmd"
log="$game_dir/log"
rm -rf "$game_dir" && mkdir -p "$game_dir"
exe=$bin
[ "$bin" = proton32 ] && exe=target32.exe
profile="$HOME/.var/app/io.github.Petru101.Ferret/data/profiles/$exe.profile"
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

if [ "$bin" = proton32 ]; then
    mkdir -p "$here/run/prefix"
    export STEAM_COMPAT_DATA_PATH="$here/run/prefix" STEAM_COMPAT_CLIENT_INSTALL_PATH="$steam"
    winpath() { printf 'Z:%s' "$1" | tr / '\\'; }
    "$proton" run "$here/test-game/target32.exe" "$(winpath "$cmd")" "$(winpath "$log")" >"$game_dir/proton.log" 2>&1 &
    i=0
    until [ -s "$log" ]; do i=$((i + 1)); [ $i -gt 900 ] && { echo "game did not start" >&2; exit 1; }; sleep 0.2; done
    pid=$(pgrep -n -x target32.exe)
else
    "$here/test-game/$bin" "$cmd" "$log" &
    until [ -s "$log" ]; do sleep 0.2; done
    pid=$!
fi
"$live" start >/dev/null
sleep 2
"$live" attach "$pid"
echo "--- coins 30"
"$live" scan 30
game "coins 7"
"$live" next 37
game "coins 5"
"$live" next 42
game "wood 30"
game "coins -4"
"$live" next 38
"$live" list
# With no screen to read, the probe waits for the game to change the number.
"$live" probe 38 &
probing=$!
sleep 6
game "coins 2"
wait $probing
"$live" save coins
echo "--- the saved pattern finds coins again"
"$live" restore
"$live" values
