#!/bin/sh
# Starts a game installed through Heroic (flatpak) the way Heroic does: its gogdl/legendary,
# umu, GE-Proton, the shared prefix and Heroic's environment (Ferret's launcher check reads
# HEROIC_APP_*). Meant as hidden.sh's command. Run on the host.
#   heroic.sh gog <id> <install folder>
#   heroic.sh epic <app name> <install folder>
#   heroic.sh <gog|epic> <id> <install folder> <exe>   run the exe with umu directly (no store
#                                                    login; for games that run offline)
# Extra environment for the game: GAME_ENV="A=1 B=2".
set -eu
H=/home/Petru/.var/app/com.heroicgameslauncher.hgl/config/heroic
B=/app/bin/heroic/resources/app.asar.unpacked/build/bin/x64/linux
store=$1 id=$2 dir=$3 EXE=${4:-}
runner=$store
[ "$store" = epic ] && runner=legendary
proton=$(ls -d /var/home/Petru/.local/share/Steam/compatibilitytools.d/GE-Proton* | sort -V | tail -1)
set -- --env=HEROIC_APP_NAME="$id" --env=HEROIC_APP_RUNNER="$runner" --env=HEROIC_APP_SOURCE="$runner" \
    --env=GAMEID=umu-0 --env=STORE="$store" --env=STEAM_COMPAT_INSTALL_PATH="$dir" \
    --env=STEAM_COMPAT_CLIENT_INSTALL_PATH=/home/Petru/.var/app/com.heroicgameslauncher.hgl/.steam/steam \
    --env=WINEPREFIX=/home/Petru/Games/Heroic/Prefixes/shared --env=STEAM_COMPAT_DATA_PATH=/home/Petru/Games/Heroic/Prefixes/shared \
    --env=PROTONPATH="$proton" --env=PROTON_ENABLE_NVAPI=1 --env=DXVK_NVAPI_ALLOW_OTHER_DRIVERS=1 \
    --env=STEAM_COMPAT_APP_ID=0 --env=SteamAppId=0 --env=SteamGameId=heroic- \
    $(for e in ${GAME_ENV:-}; do printf -- '--env=%s ' "$e"; done)
wrapper=$H/tools/runtimes/umu/umu_run.py
if [ -n "${EXE:-}" ]; then
    exec flatpak run "$@" --command=$wrapper com.heroicgameslauncher.hgl "$EXE"
fi
case $store in
gog)
    exec flatpak run "$@" --env=GOGDL_CONFIG_PATH=$H/gogdlConfig --command=$B/gogdl com.heroicgameslauncher.hgl \
        --auth-config-path $H/gog_store/auth.json launch "$dir" "$id" --no-wine --wrapper "$wrapper" --platform windows
    ;;
epic)
    exec flatpak run "$@" --env=LEGENDARY_CONFIG_PATH=$H/legendaryConfig/legendary --command=$B/legendary com.heroicgameslauncher.hgl \
        launch "$id" --skip-version-check --no-wine --wrapper "$wrapper"
    ;;
esac
