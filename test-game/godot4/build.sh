#!/bin/sh
# Exports the Godot 4 stand-in as a native Linux release build (binary + .pck) into build/.
# Needs the Flathub Godot and its export templates. Run on the host.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$here/build"
flatpak run org.godotengine.Godot --headless --path "$here" --import
flatpak run org.godotengine.Godot --headless --path "$here" --export-release Linux "$here/build/standin.x86_64"
