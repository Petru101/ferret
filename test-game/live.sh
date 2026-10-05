#!/bin/sh
# Keeps one flatpak session running in the background so commands can be sent
# between in-game actions. Run on the host.
#   live.sh start        start the session
#   live.sh <command>    send a command (e.g. "scan 120") and print the reply
#   live.sh stop         end the session
# FERRET_SESSION=<name> runs a separate session next to the default one.
set -eu
session=${FERRET_SESSION:-live}
dir="$(cd "$(dirname "$0")/.." && pwd)/run/$session"
case ${1:-} in
start)
    # Shapes learned from the stand-in by one test would steer the next test's scans.
    rm -f "$HOME"/.var/app/io.github.Petru101.Ferret/data/profiles/target*.shapes
    rm -rf "$dir" && mkdir -p "$dir" && mkfifo "$dir/in"
    systemctl --user stop "ferret-$session" 2>/dev/null || true
    # FERRET_PIPEWIRE_NODE=<id>: read that PipeWire node instead of a window picked through the
    # portal (a game in a headless gamescope).
    extra=${FERRET_PIPEWIRE_NODE:+--filesystem=xdg-run/pipewire-0 --env=FERRET_PIPEWIRE_NODE=$FERRET_PIPEWIRE_NODE}
    systemd-run --user --quiet --collect --unit="ferret-$session" sh -c \
        "sleep infinity > '$dir/in' & flatpak run $extra io.github.Petru101.Ferret --cli < '$dir/in' > '$dir/out' 2>&1; kill \$!"
    echo "session started"
    ;;
stop)
    systemctl --user stop "ferret-$session"
    echo "session stopped"
    ;;
*)
    before=$(wc -l < "$dir/out")
    printf '%s\n' "$*" > "$dir/in"
    # Wait for the echoed "> command" plus at least one reply line, then for output to settle.
    last=-1
    for _ in $(seq 1 240); do
        sleep 0.25
        now=$(wc -l < "$dir/out")
        [ "$now" -gt "$((before + 1))" ] && [ "$now" -eq "$last" ] && break
        last=$now
    done
    tail -n +"$((before + 1))" "$dir/out"
    ;;
esac
