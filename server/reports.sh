#!/bin/sh
# Lists the newest feedback and bug reports, or shows one whole.
#   reports.sh            the 20 newest
#   reports.sh <id>       one report, with its log and saved values
set -eu
cd "$(dirname "$0")"
if [ $# -eq 0 ]; then
    npx wrangler d1 execute ferret --remote --command \
        "SELECT id, received, version, game, contact, substr(replace(message, char(10), ' '), 1, 80) AS message, length(log) AS log FROM reports ORDER BY id DESC LIMIT 20"
else
    npx wrangler d1 execute ferret --remote --json --command "SELECT * FROM reports WHERE id = $(printf '%d' "$1")" |
        python3 -c 'import json,sys; r=json.load(sys.stdin)[0]["results"][0]; [print(f"--- {k}\n{v}") for k,v in r.items()]'
fi
