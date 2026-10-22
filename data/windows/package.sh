#!/bin/bash
# Makes Ferret's Windows package from a release build, in MSYS2 UCRT64 (needs ntldd and nsis:
# pacman -S mingw-w64-ucrt-x86_64-ntldd mingw-w64-ucrt-x86_64-nsis), from the repo's root after
# `cargo build --release --workspace` and with the OCR models in target/share/ferret/ocr:
#   bash data/windows/package.sh
# Out: dist/Ferret (bin: the programs and the DLLs they load; share: OCR models, translations,
# GTK's icons and settings schemas, licenses), dist/Ferret-<version>-windows.zip (unzip and run
# bin\ferret.exe) and dist/Ferret-<version>-setup.exe (installs for the current user, no
# administrator needed). GTK finds its files from its DLLs' folder: <package>/share.
set -euo pipefail
export PATH=/ucrt64/bin:$PATH
cd "$(dirname "$0")/../.."
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
out=dist/Ferret
licenses=$out/share/licenses/ferret
rm -rf dist
mkdir -p $out/bin $out/share/ferret $licenses/msys2

cp target/release/ferret.exe target/release/ferret-helper.exe $out/bin/
# gdbus.exe: the session bus GLib starts for one Ferret at a time; gspawn helpers: GLib's way to
# start programs from a windowed one.
cp /ucrt64/bin/gdbus.exe /ucrt64/bin/gspawn-win64-helper.exe /ucrt64/bin/gspawn-win64-helper-console.exe $out/bin/

# Image loaders (SVG for the icons), their list with paths relative to the package.
pb=lib/gdk-pixbuf-2.0/2.10.0
mkdir -p $out/$pb/loaders
cp /ucrt64/$pb/loaders/*.dll $out/$pb/loaders/
cp /ucrt64/$pb/loaders.cache $out/$pb/

# Every MSYS2 DLL the programs and loaders load, and the packages they come from.
dlls=$(ntldd -R $out/bin/*.exe $out/$pb/loaders/*.dll | awk 'tolower($3) ~ /ucrt64/ {print $3}' | sort -u)
for d in $dlls; do cp "$(cygpath -u "$d")" $out/bin/; done
pkgs=$( (for d in $dlls; do pacman -Qqo "$(cygpath -u "$d")"; done; pacman -Qqo /ucrt64/bin/gdbus.exe /ucrt64/$pb/loaders/pixbufloader_svg.dll) | sort -u)
for p in $pkgs; do
    name=${p#mingw-w64-ucrt-x86_64-}
    v=$(pacman -Q "$p" | cut -d' ' -f2)
    lic=$(pacman -Qi "$p" | sed -n 's/^Licenses *: //p')
    echo "$name $v: $lic" >> $licenses/msys2/PACKAGES.txt
    if [ -d "/ucrt64/share/licenses/$name" ]; then cp -r "/ucrt64/share/licenses/$name" $licenses/msys2/; fi
done

# GTK's settings schemas, icons (Adwaita's, and Ferret's own in hicolor).
mkdir -p $out/share/glib-2.0/schemas $out/share/icons
cp /ucrt64/share/glib-2.0/schemas/org.gtk.gtk4.*.gschema.xml $out/share/glib-2.0/schemas/
glib-compile-schemas $out/share/glib-2.0/schemas
cp -r /ucrt64/share/icons/Adwaita $out/share/icons/
mkdir -p $out/share/icons/hicolor/scalable/apps
cp /ucrt64/share/icons/hicolor/index.theme $out/share/icons/hicolor/
cp data/io.github.Petru101.Ferret.svg $out/share/icons/hicolor/scalable/apps/
gtk4-update-icon-cache -q -t -f $out/share/icons/hicolor || true
gtk4-update-icon-cache -q -t -f $out/share/icons/Adwaita || true

# OCR models, translations (Ferret's and GTK's, libadwaita's, GLib's for its languages).
cp -r target/share/ferret/ocr $out/share/ferret/
for lang in $(grep -v '^#' po/LINGUAS); do
    dir=$out/share/locale/$lang/LC_MESSAGES
    mkdir -p "$dir"
    msgfmt -c -o "$dir/ferret.mo" "po/$lang.po"
    for dom in gtk40 libadwaita glib20; do
        if [ -f "/ucrt64/share/locale/$lang/LC_MESSAGES/$dom.mo" ]; then cp "/ucrt64/share/locale/$lang/LC_MESSAGES/$dom.mo" "$dir/"; fi
    done
done

# Licenses: Ferret's, the crates', the OCR models' and the anti-cheat list's.
cp LICENSE $licenses/LICENSE
cp data/third-party/THIRD-PARTY-LICENSES-windows.txt $licenses/THIRD-PARTY-LICENSES.txt
cp data/windows/PaddleOCR-LICENSE $licenses/PaddleOCR-LICENSE
cp data/areweanticheatyet/LICENSE $licenses/AreWeAntiCheatYet-LICENSE
cp data/windows/ferret.ico $out/

# The portable zip and the installer.
powershell -NoProfile -Command "Compress-Archive -Path '$(cygpath -w "$PWD/$out")' -DestinationPath '$(cygpath -w "$PWD/dist/Ferret-$ver-windows.zip")'"
makensis -V2 -DVERSION="$ver" -DSRC="$(cygpath -w "$PWD/$out")" -DOUT="$(cygpath -w "$PWD/dist/Ferret-$ver-setup.exe")" data/windows/ferret.nsi
ls -la dist
