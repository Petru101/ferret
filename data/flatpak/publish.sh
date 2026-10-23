#!/bin/sh
# Builds Ferret into its own flatpak repo (signed, with update deltas) and copies the repo into
# the update site, a checkout of Petru101/ferret-updates served by GitHub Pages at
# https://petru101.github.io/ferret-updates/. Players add it once (ferret.flatpakref) and get
# every later build through `flatpak update` or their software centre. Run on the host:
#   data/flatpak/publish.sh          build, sign, update the site's flatpak/ and run/ferret.flatpak
#   data/flatpak/publish.sh push     then publish the site (one fresh commit, history dropped);
#                                    run this one in the distrobox, where gh's login is (on the
#                                    host git has no credentials and KDE asked for a password)
# The signing key lives outside the repo (FERRET_GNUPG, default ~/.local/share/ferret-signing/
# gnupg); its public half is data/flatpak/ferret-updates.gpg. The bundle (run/ferret.flatpak,
# for the GitHub release) points at the site too, so bundle installs get updates as well.
set -eu
cd "$(dirname "$0")/../.."
KEY=C51EB562EB46BFD6A7C8D44B9BE8966D9AF9B957
GNUPG=${FERRET_GNUPG:-$HOME/.local/share/ferret-signing/gnupg}
SITE=${FERRET_SITE:-$HOME/Projects/ferret-updates}
URL=https://petru101.github.io/ferret-updates/flatpak/
APP=io.github.Petru101.Ferret
BRANCH=stable
# The repo's history stays here between builds (deltas are made from the previous build).
REPO=flatpak-repo

if [ "${1:-}" != push ]; then
    flatpak run --user org.flatpak.Builder --user --force-clean --disable-rofiles-fuse \
        --default-branch=$BRANCH --repo=$REPO build-dir $APP.yml
    flatpak build-sign --gpg-sign=$KEY --gpg-homedir="$GNUPG" $REPO $APP $BRANCH
    # Only the newest builds are kept (GitHub Pages serves about 1 GB).
    flatpak build-update-repo --title=Ferret --default-branch=$BRANCH --gpg-sign=$KEY \
        --gpg-homedir="$GNUPG" --generate-static-deltas --prune --prune-depth=4 $REPO
    flatpak build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
        --repo-url=$URL --gpg-keys=data/flatpak/ferret-updates.gpg $REPO run/ferret.flatpak $APP $BRANCH
    mkdir -p "$SITE/flatpak"
    rsync -a --delete $REPO/ "$SITE/flatpak/"
    du -sh "$SITE" "$SITE/flatpak"
    exit 0
fi

cd "$SITE"
git checkout -q --orphan publish
git add -A
git commit -q -m "Publish $(date -u +%Y-%m-%dT%H:%MZ)"
git branch -D main >/dev/null 2>&1 || true
git branch -m main
# Never ask for a password: without gh's credentials the push just fails.
GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=true SSH_ASKPASS=true git push -q -f origin main
# The dropped commits' files leave this checkout too.
git reflog expire --expire=now --all
git gc -q --prune=now
echo "published: https://petru101.github.io/ferret-updates/"
