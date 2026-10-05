#!/bin/sh
# Runs a game in a headless gamescope, out of the player's way: nothing shows on the desktop,
# keys go in through gamescope's own X display, frames come out of its PipeWire node (which
# Ferret reads with FERRET_PIPEWIRE_NODE, see live.sh). Run on the host.
#   hidden.sh start <name> [WxH] -- <command...>   start in the current folder (environment
#                                                   variables to pass: ENV="A=1 B=2")
#   hidden.sh node <name>            the PipeWire node with the game's picture
#   hidden.sh key <name> <key>...    xdotool keys (Return, Escape, Down, w, ctrl+s...)
#   hidden.sh hold <name> <key> <s>  hold a key for s seconds
#   hidden.sh shot <name> <out.png>  save the current frame
#   hidden.sh stop <name>
# The mouse can't be moved there (gamescope owns the pointer; xdotool's moves do nothing):
# use the keyboard, and bind actions to keys in the game's settings.
set -eu
unit="hidden-${2:-}"
gamescope_pid() { systemctl --user show -p MainPID --value "$unit"; }
display() {
    for p in $(pgrep -P "$(gamescope_pid)") $(pgrep -g "$(gamescope_pid)"); do
        d=$(tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null | sed -n 's/^DISPLAY=//p')
        [ -n "$d" ] && echo "$d" && return
    done
    echo "no display for $unit" >&2
    exit 1
}
node() { journalctl --user -u "$unit" --no-pager -o cat | sed -n 's/.*stream available on node ID: \([0-9]*\).*/\1/p' | tail -1; }
case ${1:-} in
start)
    size=1280x720
    shift 2
    [ "${1:-}" != "--" ] && size=$1 && shift
    shift
    w=${size%x*} h=${size#*x}
    set -- $(for e in ${ENV:-}; do printf -- '-E %s ' "$e"; done) gamescope --backend headless -W "$w" -H "$h" -w "$w" -h "$h" -- "$@"
    systemctl --user stop "$unit" 2>/dev/null || true
    systemd-run --user --quiet --collect --unit="$unit" --working-directory="$PWD" "$@"
    for _ in $(seq 1 40); do
        sleep 0.5
        [ -n "$(node)" ] && break
    done
    echo "started $unit: PipeWire node $(node)"
    ;;
node) node ;;
key)
    d=$(display)
    shift 2
    for k in "$@"; do
        DISPLAY=$d xdotool key "$k"
        sleep 0.3
    done
    ;;
hold)
    d=$(display)
    DISPLAY=$d xdotool keydown "$3"
    sleep "$4"
    DISPLAY=$d xdotool keyup "$3"
    ;;
shot)
    out=$(realpath -m "$3")
    timeout 10 gst-launch-1.0 -q pipewiresrc path="$(node)" num-buffers=2 ! videoconvert ! pngenc snapshot=false ! multifilesink location="$out.%d" >/dev/null
    mv "$out.1" "$out" && rm -f "$out.0"
    echo "saved $out"
    ;;
stop)
    systemctl --user stop "$unit"
    echo "stopped $unit"
    ;;
*)
    sed -n '2,13p' "$0"
    exit 1
    ;;
esac
