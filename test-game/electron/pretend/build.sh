#!/bin/sh
# Bundles and minifies the pretend games that have a src/ folder into their dist/ (gitignored),
# like a Vite production build. Three Knights is one inline script and needs no build.
# Needs `npm install` in test-game/electron first.
set -e
cd "$(dirname "$0")"
esb=../node_modules/.bin/esbuild
opts="--bundle --minify --format=esm --log-level=warning"
$esb moss-farm/src/main.js $opts --outfile=moss-farm/dist/game.js
$esb pixel-bloom/src/main.js $opts --outfile=pixel-bloom/dist/game.js
$esb ant-colony/src/main.js $opts --outfile=ant-colony/dist/game.js
$esb ant-colony/src/worker.js $opts --outfile=ant-colony/dist/worker.js
$esb tiny-tavern/src/main.jsx $opts --jsx=automatic --define:process.env.NODE_ENV='"production"' --outfile=tiny-tavern/dist/game.js
echo built
