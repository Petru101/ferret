# Ferret

![Ferret following SuperTux's coin count: 21 → 2 places match, the player's turn to change it](data/screenshots/find-supertux.png)

<sub>The game in the screenshot is [SuperTux](https://www.supertux.org/), free and open source;
its artwork is under its own free licenses.</sub>

![The coins saved in the Values tab: set them, or keep them in a range](data/screenshots/values-supertux.png)

Find and change values in your games. Proof of concept for a single-player game cheat tool for
Linux, packaged as a flatpak.

- The sandboxed frontend (`ferret`) starts a static host helper (`ferret-helper`) through
  `flatpak-spawn --host`, which reads and writes game memory via `/proc/<pid>/mem`.
- Never for online games: refuses games with anti-cheat (loaded, shipped in the game's folder, VAC
  per Steam, or listed by [AreWeAntiCheatYet](https://github.com/AreWeAntiCheatYet/AreWeAntiCheatYet))
  and games their store (Steam, Heroic) lists as online only.
- `numbers` / `watch` / `auto`: reads a value off the game window (ScreenCast portal; the number
  inside the rectangle the player picks, read with PaddleOCR and the game's own digits once
  learned) and narrows memory scans every time it changes; `probe` picks the real value out of
  its copies.
- `save <name>`: finds the code that accesses a value with hardware breakpoints and saves a code
  pattern, so the value is found again automatically after the game restarts.

## Build

```sh
# once, and after changing dependencies (uses the SDK's cargo)
flatpak run --share=network --filesystem=$PWD --command=sh org.freedesktop.Sdk//26.08 \
    -c 'cd '"$PWD"' && /usr/lib/sdk/rust-stable/bin/cargo vendor -q vendor'
flatpak run org.flatpak.Builder --user --install --force-clean --disable-rofiles-fuse \
    build-dir io.github.Petru101.Ferret.yml
flatpak run io.github.Petru101.Ferret
```

`test-game/` has a stand-in game and scripts: `demo.sh` (scripted runs natively, under Proton and in
the Steam Linux Runtime), `live.sh` (a background session for sending commands between in-game
actions) and `table-check.py` (resolves ParticleFleet.CT's gems pointer read-only).

## Third-party data

`data/areweanticheatyet/games.json` is a trimmed copy of AreWeAntiCheatYet's `games.json`
(MIT License, Copyright © 2021 Starz0r, Curve; see `data/areweanticheatyet/LICENSE` and `SOURCE`).
Refresh it with `data/areweanticheatyet/update.sh` before a release.

Numbers are read with PaddlePaddle's PP-OCRv6 small text detection and recognition models
([PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR), Apache License 2.0), downloaded from
PaddlePaddle's Hugging Face repositories at build time and run with
[rten](https://github.com/robertknight/rten).

`bench/` compares OCR engines on saved game frames (its own cargo workspace; see `bench/run.sh`).
