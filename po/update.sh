#!/bin/sh
# Refreshes po/ferret.pot from the code, and every translation in po/LINGUAS from it. Runs the
# GNOME SDK's gettext on the host (from the distrobox; inside the SDK it runs as it is).
cd "$(dirname "$0")/.." || exit 1
run() {
    if command -v xgettext >/dev/null && xgettext --version | grep -q ' 0\.2[4-9]'; then "$@"
    else host-spawn flatpak run --filesystem="$PWD" --command=sh org.gnome.Sdk//50 -c 'cd "$0" && exec "$@"' "$PWD" "$@" | tr -d '\r'
    fi
}
# Only Ferret's own macros (i18n.rs) and the desktop file's Comment and Keywords (not its Name).
run xgettext --from-code=UTF-8 --add-comments=Translators: --package-name=Ferret \
    -k --keyword=tr! --keyword=ntr!:1,2 --keyword=n_! --keyword=Comment --keyword=Keywords \
    -f po/POTFILES -o po/ferret.pot || exit 1
for lang in $(cat po/LINGUAS); do
    run msgmerge --quiet --update --backup=none "po/$lang.po" po/ferret.pot || exit 1
done
