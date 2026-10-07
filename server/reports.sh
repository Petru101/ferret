#!/bin/sh
# Lists the newest feedback and bug reports, shows one whole, or deletes one.
#   reports.sh               the 20 newest
#   reports.sh <id>          one report, with its log and saved values
#   reports.sh delete <id>   removes one
set -eu
cd "$(dirname "$0")"
q() { npx wrangler d1 execute ferret --remote --json --command "$1" 2>/dev/null; }
case ${1:-} in
"")
    q "SELECT id, received, version, game, contact, substr(replace(message, char(10), ' '), 1, 70) AS message, length(log) AS log FROM reports ORDER BY id DESC LIMIT 20" |
        python3 -c 'import json,sys
rows = json.load(sys.stdin)[0]["results"]
print("no reports yet" if not rows else "")
for r in rows:
    print(f"#{r["id"]}  {r["received"]}  Ferret {r["version"]}  {r["game"] or "-"}  log {r["log"] or 0} B  contact: {r["contact"] or "-"}\n    {r["message"]}")'
    ;;
delete) q "DELETE FROM reports WHERE id = $(printf '%d' "$2")" >/dev/null && echo "deleted #$2" ;;
*)
    q "SELECT * FROM reports WHERE id = $(printf '%d' "$1")" |
        python3 -c 'import json,sys
rows = json.load(sys.stdin)[0]["results"]
print("no such report") if not rows else [print(f"--- {k}\n{v}") for k, v in rows[0].items()]'
    ;;
esac
