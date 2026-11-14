#!/bin/sh
# play.sh <game>                 opens a pretend game on your desktop, to play by hand
# play.sh <game> --hidden [:N]   runs it on a virtual display (Xvfb, default :81, software GL),
#                                commands in run/electron/pretend/<game>/cmd, state lines in
#                                .../log (shell.js lists the commands), its main process ID in
#                                .../pid; `echo quit > .../cmd` ends it
# Games: moss-farm, three-knights, pixel-bloom, ant-colony, tiny-tavern. Each is its own app
# folder (its own package.json and name), so several can run at once and each is listed apart.
# Run in the distrobox; build first (build.sh).
game=${1:?game name}; mode=$2
here=$(cd "$(dirname "$0")" && pwd)
app=$here/$game
[ -f "$app/package.json" ] || { echo "no game $game"; exit 1; }
# Electron's own program, not npm's launcher script, so the saved ID is the game's.
electron=$here/../node_modules/electron/dist/electron
# --no-sandbox: npm's Electron has no setuid chrome-sandbox.
if [ "$mode" != --hidden ]; then
  exec "$electron" --no-sandbox --ozone-platform-hint=auto "$app"
fi
disp=${3:-:81}; n=${disp#:}
d=$here/../../../run/electron/pretend/$game; mkdir -p "$d"; d=$(cd "$d" && pwd)
[ -e /tmp/.X11-unix/X$n ] || { setsid Xvfb $disp -screen 0 1280x800x24 >/dev/null 2>&1 & sleep 2; }
if [ -s "$d/pid" ] && kill -0 "$(cat "$d/pid")" 2>/dev/null; then echo quit > "$d/cmd"; sleep 1.5; fi
rm -f "$d/cmd"; : > "$d/log"
env -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE DISPLAY=$disp FERRET_CMD="$d/cmd" FERRET_LOG="$d/log" \
  setsid "$electron" --no-sandbox --ozone-platform=x11 --use-gl=angle --use-angle=swiftshader --enable-unsafe-swiftshader "$app" > "$d/out.txt" 2>&1 &
echo $! > "$d/pid"
sleep 5; tail -1 "$d/log"
