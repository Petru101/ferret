#!/bin/sh
# Keeps one flatpak session running in the background so commands can be sent
# between in-game actions. Run on the host.
#   live.sh start        start the session
#   live.sh <command>    send a command (e.g. "scan 120") and print the reply
#   live.sh stop         end the session
set -eu
dir="$(cd "$(dirname "$0")/.." && pwd)/run/live"
case ${1:-} in
start)
    rm -rf "$dir" && mkdir -p "$dir" && mkfifo "$dir/in"
    systemctl --user stop cheat-poc-live 2>/dev/null || true
    systemd-run --user --quiet --collect --unit=cheat-poc-live sh -c \
        "sleep infinity > '$dir/in' & flatpak run io.github.Petru101.CheatPoc < '$dir/in' > '$dir/out' 2>&1; kill \$!"
    echo "session started"
    ;;
stop)
    systemctl --user stop cheat-poc-live
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
