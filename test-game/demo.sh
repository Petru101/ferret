#!/bin/sh
# Starts the test game and drives a scripted cheat session through the
# installed flatpak. Run on the host (not in a container).
#   native  - Linux build of the game
#   eac     - Linux build with a fake easyanticheat_x64.so loaded (must be refused)
#   proton  - Windows build under "proton run", like a non-Steam launch
#   sniper  - Windows build under Proton inside the Steam Linux Runtime container,
#             the way Steam itself starts Proton games
set -eu
mode=${1:-native}
here=$(cd "$(dirname "$0")/.." && pwd)
steam="$HOME/.local/share/Steam"
proton="$steam/steamapps/common/Proton 9.0 (Beta)/proton"
sniper="$steam/steamapps/common/SteamLinuxRuntime_sniper/_v2-entry-point"
run="$here/run/$mode"
cmd="$run/cmd"
log="$run/log"
rm -rf "$run"
mkdir -p "$run" "$here/run/prefix"

winpath() { printf 'Z:%s' "$1" | tr / '\\'; }
game() {
    printf '%s\n' "$1" > "$cmd.tmp" && mv "$cmd.tmp" "$cmd"
    sleep 1
    echo "   [game] $1 -> $(tail -n1 "$log")" >&2
}
cleanup() { [ -e "$log" ] && printf 'quit 1\n' > "$cmd"; }
trap cleanup EXIT

export STEAM_COMPAT_DATA_PATH="$here/run/prefix" STEAM_COMPAT_CLIENT_INSTALL_PATH="$steam"
export SteamAppId=480 SteamGameId=480
case $mode in
native) comm=target; "$here/test-game/target" "$cmd" "$log" & ;;
eac)
    comm=target
    cp "$(ldd /bin/sh | awk '/libc\.so/ {print $3}')" "$run/easyanticheat_x64.so"
    LD_PRELOAD="$run/easyanticheat_x64.so" "$here/test-game/target" "$cmd" "$log" &
    ;;
proton) comm=target.exe; "$proton" run "$here/test-game/target.exe" "$(winpath "$cmd")" "$(winpath "$log")" >"$run/proton.log" 2>&1 & ;;
sniper)
    comm=target.exe
    "$sniper" --verb=waitforexitandrun -- "$proton" waitforexitandrun \
        "$here/test-game/target.exe" "$(winpath "$cmd")" "$(winpath "$log")" >"$run/proton.log" 2>&1 &
    ;;
*) echo "unknown mode $mode" >&2; exit 1 ;;
esac

i=0
until [ -s "$log" ]; do
    i=$((i + 1)); [ $i -gt 180 ] && { echo "game did not start" >&2; exit 1; }
    sleep 1
done
pid=$(pgrep -n -x "$comm")
echo "game running as host pid $pid ($comm): $(cat "$log")" >&2

{
    echo sandbox
    echo info
    echo "ps target"
    echo "attach $pid"
    echo "scan 1000"; sleep 3
    game "earn 37"; echo "next 1037"; sleep 1
    game "spend 500"; echo "next 537"; sleep 1
    echo list
    echo "set 999999"; sleep 1
    game "show"
    echo "scan 100"; sleep 3
    game "hit 7"; echo "next -"; sleep 1
    game "hit 3"; echo "next -"; sleep 1
    game "show"; echo "next ="; sleep 1
    echo list
    echo quit
} | flatpak run io.github.Petru101.Ferret
