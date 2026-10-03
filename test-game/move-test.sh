#!/bin/sh
# Tests that saved values follow the game when it replaces their objects but keeps the old ones
# readable (like Particle Fleet on a new map): crystals (read from a static pointer) must follow
# at once, gold (no static pointer) within the 10 s re-check, also with limits.
#   move-test.sh [target|proton|proton32]
# proton runs target.exe (64-bit), proton32 target32.exe (32-bit, like Particle Fleet) under Proton.
# Runs in its own Ferret session. Run on the host.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
bin=${1:-target}
steam="$HOME/.local/share/Steam"
proton="$steam/steamapps/common/Proton 9.0 (Beta)/proton"
export FERRET_SESSION=move-test
live="$here/test-game/live.sh"
game_dir="$here/run/move-test-game"
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
# Other matches are copies (on the stack): write a marker to each and see which one the game reports.
keep_real_match() {
    for loc in $("$live" list | grep '^0x' | cut -d' ' -f1); do
        "$live" write "$loc" 4242 >/dev/null
        game "show" >/dev/null
        "$live" write "$loc" "$2" >/dev/null
        if tail -n1 "$log" | grep -Eq "(^| )$1=4242"; then
            "$live" keep "${loc%%:*}"
            return
        fi
    done
    echo "no match is the real $1" >&2
    exit 1
}
cleanup() {
    printf 'quit 1\n' > "$cmd" 2>/dev/null || true
    sleep 1.5
    "$live" stop >/dev/null 2>&1 || true
    rm -f "$profile"
}
trap cleanup EXIT

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
"$live" start >/dev/null
sleep 2
"$live" attach "$pid"
"$live" scan 300
game "crystals 7"
"$live" next 307
game "crystals -5"
"$live" next 302
keep_real_match crystals 302
"$live" save crystals
"$live" scan 1000
game "earn 37"
"$live" next 1037
game "spend 10"
"$live" next 1027
keep_real_match gold 1027
"$live" save gold
echo "--- attach again (as after a restart): restore"
"$live" attach "$pid"
"$live" values
echo "--- newmap: new objects, old ones stay readable"
game "newmap"
game "crystals 4"
game "earn 3"
"$live" values
echo "--- after the 10 s re-check, gold too"
sleep 11
"$live" values
echo "--- limits across another newmap"
"$live" limit crystals - 320
"$live" limit gold - 1100
game "newmap"
game "crystals 50"
sleep 11
game "earn 500"
game "show"
"$live" values
