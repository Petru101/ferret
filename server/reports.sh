#!/bin/sh
# Lists the newest feedback and bug reports, shows one whole, or deletes one. Reports go by
# the id Ferret showed the player ("K7Q2-9XMB"; any case, the dash optional).
#   reports.sh               the 20 newest
#   reports.sh <id>          one report, with its log and saved values
#   reports.sh delete <id>   removes one
set -eu
cd "$(dirname "$0")"
q() { npx wrangler d1 execute ferret --remote --json --command "$1" 2>/dev/null; }
# "k7q29xmb" -> "K7Q2-9XMB"; anything else stops here (it goes into SQL).
ref() {
    r=$(printf '%s' "$1" | tr a-z A-Z | tr -d -)
    printf '%s' "$r" | grep -qE '^[0-9A-Z]{8}$' || { echo "not a report id: $1" >&2; exit 1; }
    printf '%s-%s' "$(printf '%s' "$r" | cut -c1-4)" "$(printf '%s' "$r" | cut -c5-8)"
}
case ${1:-} in
"")
    q "SELECT ref, received, version, game, contact, substr(replace(message, char(10), ' '), 1, 70) AS message, length(log) AS log FROM reports ORDER BY id DESC LIMIT 20" |
        python3 -c 'import json,sys
rows = json.load(sys.stdin)[0]["results"]
print("no reports yet" if not rows else "")
for r in rows:
    print(f"{r["ref"]}  {r["received"]}  Ferret {r["version"]}  {r["game"] or "-"}  log {r["log"] or 0} B  contact: {r["contact"] or "-"}\n    {r["message"]}")'
    ;;
delete)
    r=$(ref "${2:-}")
    n=$(q "DELETE FROM reports WHERE ref = '$r'" | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["meta"]["changes"])')
    [ "$n" = 1 ] && echo "deleted $r" || echo "no report $r"
    ;;
*)
    r=$(ref "$1")
    q "SELECT * FROM reports WHERE ref = '$r'" |
        python3 -c 'import json,sys
rows = json.load(sys.stdin)[0]["results"]
print("no such report") if not rows else [print(f"--- {k}\n{v}") for k, v in rows[0].items() if k != "id"]'
    ;;
esac
