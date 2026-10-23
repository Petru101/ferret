#!/bin/bash
# Makes Ferret's Windows package from a release build, in MSYS2 UCRT64 (needs ntldd:
# pacman -S mingw-w64-ucrt-x86_64-ntldd), from the repo's root after
# `cargo build --release --workspace` and with the OCR models in target/share/ferret/ocr, and
# Velopack's vpk in C:\dev\vpk with .NET in C:\dev\dotnet (run/win/vpk-setup.ps1):
#   bash data/windows/package.sh
# Out: dist/Ferret (bin: the programs and the DLLs they load; share: OCR models, translations,
# GTK's icons and settings schemas, licenses), dist/Ferret-<version>-setup.exe (installs for
# the current user, no administrator needed, and updates itself), dist/Ferret-<version>-
# portable.zip (unzip anywhere; updates itself too) and the update feed in VPK_RELEASES. GTK
# finds its files from its DLLs' folder: <package>/share.
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

# The installer, the portable zip and the update feed, by Velopack's vpk (update.rs): installs
# per user into %LOCALAPPDATA%\Petru101.Ferret (not Ferret's data folder: an uninstall deletes
# its own), a Start menu shortcut. The feed's folder keeps earlier releases (VPK_RELEASES), so
# each pack adds a delta from the one before; it is copied to the update site's windows/.
releases=${VPK_RELEASES:-/c/dev/velopack-releases}
mkdir -p "$releases"
DOTNET_ROOT='C:\dev\dotnet' /c/dev/vpk/vpk.exe pack --packId Petru101.Ferret --packVersion "$ver" \
    --runtime win-x64 --packDir "$(cygpath -w "$PWD/$out")" --mainExe 'bin\ferret.exe' --packTitle Ferret --packAuthors 'Petru Mare' \
    --icon "$(cygpath -w "$PWD/data/windows/ferret.ico")" --shortcuts StartMenuRoot --outputDir "$(cygpath -w "$releases")"
cp "$releases/Petru101.Ferret-win-Setup.exe" "dist/Ferret-$ver-setup.exe"
cp "$releases/Petru101.Ferret-win-Portable.zip" "dist/Ferret-$ver-portable.zip"
ls -la dist "$releases"
