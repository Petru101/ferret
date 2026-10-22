# Ferret on Windows: test report (2026-10-22)

Tested on Windows 11 Pro (build 26300) from a git checkout at `C:\dev\ferret`, built with MSYS2
UCRT64's Rust and GTK 4 (`cargo --offline build --release --workspace`). Everything below is on
GitHub `master`.

## Where it stands

| Check | Result |
|---|---|
| `cargo test --release --workspace` | ✅ ferret 47/47, ferret-helper 34/34 |
| Stand-in feature tests, `test-game/win/run-all.ps1` | ✅ 11/11 |
| Helper on the stand-in, 64-bit and 32-bit (`run/win/stand-in.ps1`) | ✅ |
| CLI restart and exit checks (`run/win/cli-tests.ps1 both`) | ✅ 5/5 |
| GUI monkey (`FERRET_MONKEY`, random clicks on the stand-in) | ✅ no crash or stall after the fixes below |
| Unreal Engine 4.27 (Hell Pie Demo, GOG) | ✅ see below |
| Linux after these changes | ❓ not run yet: pull, build, run `test-game/*.sh` |

## Fixed

| Commit | What |
|---|---|
| `37bda7e` | Ids came from `/dev/urandom`, so every Windows install had the all-zero install id and `0000-0000` report ids. Now `getrandom`; an all-zero id is rejected. Uploads already made under the zero id may be on the server. |
| `399b73e` | `settings` and `install-id` landed in the root of `%LOCALAPPDATA%`; now in `%LOCALAPPDATA%\Ferret`, moved from the old place once (the install id stays the same). |
| `feda76c` | Feedback logs kept the Windows user name: scrubbing read `HOME`, which Windows doesn't set. Now `USERPROFILE`, either slash, any case. |
| `6264731` | A limit set just before the helper died waited for the next CLI command; the CLI now checks for a dead helper after each command too. |
| `e055458` | Electron apps (the Claude app) and WinUI/GTK apps showed up in the games list. |
| `a9ad48d` | A prompt read in Ferret's window was sent again as a notification when switching to the game. GTK's focus tracking was fine (`FERRET_FOCUS_LOG`, `cdc639c`); it was the reminder. |
| `b7477f6` | The typed-number box lost digits while it came and went; a new number is taken only once it holds still. |
| `a6653d3` | The instruction bar's spinner only turned during the first scan (now `adw::Spinner`). |
| `cfee7f1`, `aadf6b3` | Found by the monkey: the game page opened twice when its row was clicked again; "Java game" was said when setting a value with no game open. |
| `e59a7d4` | Unreal: when the possessed pawn is a camera rig, the character it steers is listed first. |
| `9cb9223` | Pointer paths followed a moved value's freed old copy (see "Windows' heap"). |
| `fafa24f` | Learned shapes kept the next heap block's encoded header (see "Windows' heap"). |

## Unreal Engine (Hell Pie Demo, GOG, UE 4.27)

- The Mammoth is UE 4.22, older than Ferret reads (4.25+): "no Unreal name pool found" is expected,
  on Linux too.
- Hell Pie: object list and name pool found (66,940 objects, ~0.1 s). Its controller possesses a
  camera rig whose `ControlledPawn` is the demon, so the demon wasn't listed (218 numbers); after
  `e59a7d4`, 802 numbers: walk speed, jump, gravity...
- Saved by name and found again after a game restart and a level reload, at new addresses:
  health, hearts (`CurrentLifes`), candy (`Basic_Collected`), jump, walk speed.
- Writes reach the game. The HUD keeps its own counts (hearts, candy) until the level reloads; the
  demon restores jump and speed from `StdJumpZVelocity`/`StdMaxWalkSpeed` after a few seconds, so
  setting those too makes it last.
- Keep It: jump kept at least 1500 stays up while the game pulls it back to 480. "At least 500"
  looked broken only because the game's own standard value had been raised to 1500, which "at
  least" allows; turning off "No maximum" sets an upper bound too. Works as designed.

## Windows' heap

Two of Ferret's heuristics assumed glibc's heap. Both showed up only in the Windows feature tests.

**Freed memory stays readable.** When the game moves an object and frees the old one, pointer
paths through the old one keep working. After the stand-in's `newroom`, 2866 of 2998 saved paths
still led to the freed old gems (through ntdll's, KernelBase's, the C runtime's and the game's own
leftovers) and 125 to the new ones, so the vote picked the dead copy, and the limit wrote there.
Dropping paths that start in allocator modules wasn't enough (951 dead against 79 live would
remain; msvcrt carried 46 live ones). Now, for Windows programs (ntdll.dll loaded), the vote knows
where the value was last found and where it moved away from in this run: a new place wins when its
paths clearly agree, start in more than one module and are at least 1 in 32 of those still at the
old place. Ferret's refresh and the helper's limiter each keep those places. Elsewhere voting is
as before.

Risk: paths through something like "the last enemy hit" could look like a move if they start in
two modules and are numerous enough. Ferret already drops paths that don't keep leading to the
value (Steady), which should keep that rare. Untested on real games.

**Block headers are encoded.** A shape (the memory around a found value) reached past the record
into the next heap block's header: XORed with the heap's key and holding the neighbour's sizes,
so it matched neighbouring records but not the next stack along. On 64-bit Windows games the
shape now stops at the value's own block, decoded with the key of its heap (found from the heap
segment at the start of its memory reservation: the stand-in's C runtime heap isn't in the PEB's
list). The low-fragmentation heap and the newer segment heap are left as they were; their headers
differ per block, so narrowing drops them anyway. Untested on real games.

## Test tools added

- `test-game/win/*.ps1`: the stand-in feature tests in PowerShell; `run-all.ps1 [names]` runs them
  one after another, transcripts in `run\win-tests\`. The player's own `target.exe` profile and
  shapes are kept aside meanwhile. Each command is followed by a `sandbox` marker so its reply is
  known complete; output is read on a thread of its own (PowerShell's events came out of order).
- `FERRET_MONKEY=<seed>` (`e47ef06`): the window clicks its own buttons, toggles and entries at
  random (`FERRET_MONKEY_STEPS`, `FERRET_MONKEY_GAME` limits it to one game), skipping network,
  files and Quit; panics and stalls go to `monkey.log` in the cache folder.
- `FERRET_FOCUS_LOG=1` (`cdc639c`): logs GTK's focus next to Windows' foreground window.

PowerShell 5.1 note: in a UTF-8 console .NET writes a BOM before the first line to
`ferret.exe --cli`; set `[Console]::InputEncoding = New-Object System.Text.UTF8Encoding $false`
first (common.ps1 does).

## Not tested on Windows yet

- The two heap fixes on real games: a game that moves a value on a level change (pointer paths),
  and finding two or three inventory items in a row (shapes).
- 32-bit games on real titles (only the 32-bit stand-in so far), Unity/Mono, GameMaker, Ren'Py,
  RPG Maker, Java games.
- The installer and portable zip (`e6a54e1`).
