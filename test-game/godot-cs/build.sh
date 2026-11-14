#!/bin/sh
# Exports the C# Godot stand-in as a native Linux release build into build/, and with `win` a
# Windows one into build/win/. Needs Godot 4.7.2's .NET editor, the .NET 8 SDK and the two
# release templates in run/godot-cs/tools/ (godot/, dotnet/, templates/: notes godot.md, "C#").
# Everything the tools write (Godot's settings, NuGet's packages, fetched from nuget.org the
# first time) stays in run/godot-cs/tools/home. Runs in the distrobox or on the host.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
tools=$(cd "$here/../../run/godot-cs/tools" && pwd)
export HOME="$tools/home" DOTNET_ROOT="$tools/dotnet" PATH="$tools/dotnet:$PATH"
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1 DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1
unset XDG_DATA_HOME XDG_CONFIG_HOME XDG_CACHE_HOME
t="$HOME/.local/share/godot/export_templates/4.7.2.stable.mono"
mkdir -p "$t" "$here/build"
for f in linux_release.x86_64 windows_release_x86_64.exe version.txt; do [ -e "$t/$f" ] || ln -s "$tools/templates/$f" "$t/$f"; done
godot=$(ls "$tools"/godot/*/Godot_v*_mono_linux.x86_64)
"$godot" --headless --path "$here" --import
"$godot" --headless --path "$here" --export-release Linux "$here/build/standin-cs.x86_64"
if [ "${1:-}" = win ]; then
    mkdir -p "$here/build/win"
    "$godot" --headless --path "$here" --export-release Windows "$here/build/win/standin-cs.exe"
fi
