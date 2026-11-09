#!/bin/sh
# Takes the builds GitHub made for a tag, checks GitHub really made them from that tag, signs them
# with the keys on this computer and readies the update site and the draft release. Publishes
# nothing: that's `data/flatpak/publish.sh push` and publishing the draft, after a look. Run in the
# distrobox, where gh's login is:
#   data/release/from-github.sh v0.1.5
# A branch name instead of a tag (release-test/*) checks and signs the same way without touching
# the release; FERRET_SITE, FERRET_REPO, FERRET_GNUPG, FERRET_KEY and FERRET_UPDATE_SECRET name
# throwaway ones to try this.
set -eu
cd "$(dirname "$0")/../.."
name=${1:?usage: from-github.sh <tag or branch>}
repo=Petru101/ferret
site=${FERRET_SITE:-$HOME/Projects/ferret-updates}
secret=${FERRET_UPDATE_SECRET:-$HOME/.local/share/ferret-signing/windows-updates.key}
public=${FERRET_UPDATE_PUBLIC:-data/windows/ferret-updates.pub}
case $name in
    v[0-9]*) ref=refs/tags/$name; release=yes ;;
    *) ref=refs/heads/$name; release= ;;
esac
[ -f "$secret" ] || { echo "no update key at $secret" >&2; exit 1; }
# Apt's gh can be too old to have `gh attestation`.
checker=$HOME/.config/lobby-release/gh; [ -x "$checker" ] || checker=gh
"$checker" attestation verify --help >/dev/null 2>&1 || { echo "$checker can't check attestations: it needs gh 2.49 or newer" >&2; exit 1; }

run=$(gh run list -R "$repo" -w release.yml -b "$name" -s success -L 1 --json databaseId --jq '.[0].databaseId')
[ -n "$run" ] || { echo "GitHub has no finished build for $name" >&2; exit 1; }
work=$PWD/run/release/from-github
rm -rf "$work"
mkdir -p "$work"
gh run download "$run" -R "$repo" -n flatpak -D "$work/flatpak"
gh run download "$run" -R "$repo" -n windows -D "$work/windows"
for file in "$work"/flatpak/* "$work"/windows/Ferret-* "$work"/windows/feed/*; do
    "$checker" attestation verify "$file" --repo "$repo" --signer-workflow "$repo/.github/workflows/release.yml" --source-ref "$ref" >/dev/null
done
echo "GitHub built all of them from $ref (run $run)"

# Linux: signed, with update deltas, into the site's flatpak/.
host-spawn sh -c "cd '$PWD' && FERRET_SITE='$site' ${FERRET_REPO:+FERRET_REPO='$FERRET_REPO'} ${FERRET_GNUPG:+FERRET_GNUPG='$FERRET_GNUPG'} ${FERRET_KEY:+FERRET_KEY='$FERRET_KEY'} data/flatpak/publish.sh import '$work/flatpak/ferret-flatpak-repo.tar.zst'" </dev/null | tr -d '\r'

# Windows: the list of updates signed (update.rs refuses it otherwise; it holds each package's
# hash), and the packages it no longer lists gone from the site.
mkdir -p "$site/windows"
cp "$work"/windows/feed/* "$site/windows/"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
minisign -S -s "$secret" -m "$site/windows/releases.win.json" -x "$site/windows/releases.win.json.minisig" -t "Ferret $version" >/dev/null
minisign -V -p "$public" -m "$site/windows/releases.win.json" -x "$site/windows/releases.win.json.minisig" >/dev/null
for package in "$site"/windows/*.nupkg; do
    grep -q "\"$(basename "$package")\"" "$site/windows/releases.win.json" || rm -f "$package"
done
du -sh "$site" "$site/windows"
echo "signed for Ferret $version"

if [ -n "$release" ]; then
    bundle=run/release/ferret-${name#v}-x86_64.flatpak
    cp run/ferret.flatpak "$bundle"
    gh release upload "$name" "$bundle" -R "$repo" --clobber
    cat <<NEXT
Ready. Nothing has reached anyone yet. To publish:
  data/flatpak/publish.sh push
  gh release edit $name -R $repo --draft=false --notes-file run/release/notes-${name#v}.md
NEXT
fi
