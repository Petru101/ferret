#!/bin/sh
# Looks after the shared library: lists uploads, shows one, deletes any (as the developer:
# spam, a wrong upload), and refuses games whose values mustn't be shared (online play).
#   library.sh                     the 30 newest uploads
#   library.sh <id>                one upload's text
#   library.sh delete <id>         removes it and its votes
#   library.sh block <game> <why>  refuses uploads for a program (any case) or steam:<app id>
#   library.sh blocked             the refused games
set -eu
cd "$(dirname "$0")"
q() { npx wrangler d1 execute ferret --remote --json --command "$1" 2>/dev/null; }
ref() {
    r=$(printf '%s' "$1" | tr a-z A-Z | tr -d -)
    printf '%s' "$r" | grep -qE '^[0-9A-Z]{8}$' || { echo "not an upload id: $1" >&2; exit 1; }
    printf '%s-%s' "$(printf '%s' "$r" | cut -c1-4)" "$(printf '%s' "$r" | cut -c5-8)"
}
sql_text() { printf '%s' "$1" | sed "s/'/''/g"; }
case ${1:-} in
"")
    q "SELECT ref, uploaded, game, steam, worked, failed, names FROM packs ORDER BY id DESC LIMIT 30" |
        python3 -c 'import json,sys
rows = json.load(sys.stdin)[0]["results"]
print("no uploads yet" if not rows else "")
for r in rows:
    print(f"{r["ref"]}  {r["uploaded"]}  {r["game"]}{" (steam " + r["steam"] + ")" if r["steam"] else ""}  worked {r["worked"]} failed {r["failed"]}\n    {r["names"]}")'
    ;;
delete)
    r=$(ref "${2:-}")
    n=$(q "DELETE FROM votes WHERE pack IN (SELECT id FROM packs WHERE ref = '$r'); DELETE FROM packs WHERE ref = '$r'" |
        python3 -c 'import json,sys; print(json.load(sys.stdin)[-1]["meta"]["changes"])')
    [ "$n" = 1 ] && echo "deleted $r" || echo "no upload $r"
    ;;
block)
    [ $# -ge 3 ] || { echo "usage: library.sh block <game> <why>" >&2; exit 1; }
    game=$(printf '%s' "$2" | tr A-Z a-z)
    q "INSERT INTO blocked (game, why) VALUES ('$(sql_text "$game")', '$(sql_text "$3")') ON CONFLICT (game) DO UPDATE SET why = excluded.why" >/dev/null
    echo "uploads for $game are refused now (existing ones stay: delete them)"
    ;;
blocked)
    q "SELECT game, why FROM blocked ORDER BY game" | python3 -c 'import json,sys
rows = json.load(sys.stdin)[0]["results"]
print("none" if not rows else "\n".join(f"{r["game"]}  {r["why"]}" for r in rows))'
    ;;
*)
    r=$(ref "$1")
    q "SELECT body FROM packs WHERE ref = '$r'" | python3 -c 'import json,sys
rows = json.load(sys.stdin)[0]["results"]
print(rows[0]["body"] if rows else "no such upload", end="")'
    ;;
esac
