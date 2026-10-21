#!/bin/sh
# Exports the Godot 4 stand-in as a native Linux release build (binary + .pck) into build/,
# and with `win` a Windows one into build/win/ (the template windows_release_x86_64.exe).
# Needs the Flathub Godot and its export templates. Run on the host.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$here/build"
flatpak run org.godotengine.Godot --headless --path "$here" --import
flatpak run org.godotengine.Godot --headless --path "$here" --export-release Linux "$here/build/standin.x86_64"
if [ "${1:-}" = win ]; then
    mkdir -p "$here/build/win"
    flatpak run org.godotengine.Godot --headless --path "$here" --export-release Windows "$here/build/win/standin.exe"
fi
