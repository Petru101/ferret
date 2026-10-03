#!/bin/sh
# Refreshes the bundled AreWeAntiCheatYet list (MIT, see LICENSE here): keeps the fields the
# helper reads (name, storeIds, anticheats), one game per line. Run before a release.
set -e
cd "$(dirname "$0")"
repo=AreWeAntiCheatYet/AreWeAntiCheatYet
commit=$(gh api "repos/$repo/commits/master" --jq .sha)
curl -fsSL "https://raw.githubusercontent.com/$repo/$commit/LICENSE" -o LICENSE
curl -fsSL "https://raw.githubusercontent.com/$repo/$commit/games.json" | python3 -c '
import json, sys
games = [{k: g[k] for k in ("name", "storeIds", "anticheats")} for g in json.load(sys.stdin) if g.get("anticheats")]
games.sort(key=lambda g: g["name"].lower())
print("[\n" + ",\n".join(json.dumps(g, ensure_ascii=False, separators=(",", ":")) for g in games) + "\n]")
' > games.json
echo "https://github.com/$repo/tree/$commit" > SOURCE
echo "$(grep -c '"name"' games.json) games from $commit"
