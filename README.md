# cheat-poc

Proof of concept for a single-player game cheat tool for Linux, packaged as a flatpak.

- The sandboxed frontend (`cheat-poc`) starts a static host helper (`cheat-poc-helper`) through
  `flatpak-spawn --host`, which reads and writes game memory via `/proc/<pid>/mem`.
- Refuses to attach to processes with Easy Anti-Cheat or BattlEye loaded.
- `numbers` / `watch` / `auto`: reads a value off the game window (ScreenCast portal + Tesseract) and
  narrows memory scans every time it changes; `probe` picks the real value out of its copies.
- `save <name>`: finds the code that accesses a value with hardware breakpoints and saves a code
  pattern, so the value is found again automatically after the game restarts.

## Build

```sh
# once, and after changing dependencies (uses the SDK's cargo)
flatpak run --share=network --filesystem=$PWD --command=sh org.freedesktop.Sdk//26.08 \
    -c 'cd '"$PWD"' && /usr/lib/sdk/rust-stable/bin/cargo vendor -q vendor'
flatpak run org.flatpak.Builder --user --install --force-clean --disable-rofiles-fuse \
    build-dir io.github.Petru101.CheatPoc.yml
flatpak run io.github.Petru101.CheatPoc
```

`test-game/` has a stand-in game and scripts: `demo.sh` (scripted runs natively, under Proton and in
the Steam Linux Runtime), `live.sh` (a background session for sending commands between in-game
actions) and `table-check.py` (resolves ParticleFleet.CT's gems pointer read-only).
