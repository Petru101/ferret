// The part that runs on the host (started by the frontend through
// `flatpak-spawn --host`). Reads and writes game memory through `Mem` (mem.rs); what it asks
// the OS about other programs goes through `os` (linux.rs).
// Protocol: one command per line on stdin; each reply ends with a line "end".

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::anticheat;
use crate::ue::{self, UePath};
use crate::gdscript::{self, ScriptPath};
use crate::godot::{self, DictPath};
use crate::names::{self, Heap, NamedPath};
use crate::pointers::{self, Module, Pointers, PtrPath};
use crate::launchers;
use crate::mono::{self, MonoPath};
use crate::js::{self, JsPath};
use crate::gml::{self, GmlPath};
use crate::lua::{self, LuaPath};
use crate::air::{self, AirPath};
use crate::java::{self, JavaPath};
use crate::python::{self, PyPath};
use crate::trace;
use crate::mem::{FileAt, Mem};
use crate::os;

pub struct Region {
    pub start: u64,
    pub end: u64,
    pub perms: String,
    pub path: String,
}

pub use crate::os::maps;

/// A region mapped from a file: Linux paths start with '/', Windows ones with a drive ("C:/").
pub fn from_file(path: &str) -> bool {
    path.starts_with('/') || path.as_bytes().get(1..3) == Some(b":/")
}
use crate::os::environ;

/// The program as started (argv[0]); Wine paths have spaces ("E:\\Mass Effect Andromeda\\...").
fn argv0(pid: u32) -> String {
    os::argv(pid).into_iter().next().unwrap_or_default()
}

/// The program's arguments (without the program itself).
fn args(pid: u32) -> Vec<String> {
    os::argv(pid).into_iter().skip(1).filter(|a| !a.is_empty()).collect()
}

fn cmdline(pid: u32) -> String {
    os::argv(pid).into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")
}

/// File name of the program, also for Windows programs under Wine.
pub fn exe_name(pid: u32) -> String {
    argv0(pid).rsplit(['/', '\\']).next().unwrap_or_default().to_owned()
}

/// The name a game goes by (the games list, its profile): the program's, except for players
/// that run many games, named after the game file they were started with. Ruffle runs every
/// Flash game: all of them shared one name and one profile (Age of War's saved values were
/// restored into The Binding of Isaac); Solarus's `solarus-run` every Solarus quest. Started
/// without a file (opened from its menu), it keeps the player's name.
pub fn game_name(pid: u32) -> String {
    if let Some(name) = nwjs_app(pid).map(|dir| nwjs_name(pid, &dir)) {
        return name;
    }
    let exe = exe_name(pid);
    let lower = exe.to_ascii_lowercase();
    if DOSBOXES.contains(&lower.trim_end_matches(".exe")) {
        let home = std::env::var("HOME").unwrap_or_default();
        return dosbox_game(&argv0(pid), &os::cwd(pid), &home).map_or(exe, |g| format!("{g} (DOSBox)"));
    }
    if lower == "udk.exe" {
        if let Some(game) = udk_game(&argv0(pid)) {
            return game;
        }
    }
    let args: Vec<String> = os::argv(pid).into_iter().skip(1).collect();
    run_game(&lower, &args, || os::cwd(pid)).unwrap_or(exe)
}

/// A UDK game (Unreal Engine 3's free kit): every one runs `<game>\Binaries\Win32\UDK.exe`,
/// with no arguments, so it goes by the folder that holds Binaries: "retrobikepark (UDK)".
fn udk_game(argv0: &str) -> Option<String> {
    let parts: Vec<&str> = argv0.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    let bin = parts.iter().rposition(|p| p.eq_ignore_ascii_case("Binaries"))?;
    let game = parts.get(bin.checked_sub(1)?)?;
    (!game.ends_with(':')).then(|| format!("{game} (UDK)"))
}

/// The program Unreal Engine 4 and 5 games without code of their own run (UE4Game.exe,
/// UE4Game-Win64-Shipping.exe, UnrealGame-...): every one of them shared one name and profile.
fn unreal_runner(program: &str) -> bool {
    program.starts_with("ue4game") || program.starts_with("unrealgame")
}

/// The game a player program was started with (`game_name`): the game file it was given, a
/// LÖVE game's folder, a Python game's script (its folder when the script has a name every game
/// uses, like main.py). LÖVE, Java and Python run many games each with the same code: values
/// saved by code pattern in one would be found in another.
fn run_game(program: &str, args: &[String], cwd: impl Fn() -> String) -> Option<String> {
    let program = program.trim_end_matches(".exe");
    let endings: &[&str] = match program {
        "ruffle" => &[".swf"],
        "solarus-run" => &[".solarus"],
        "love" | "lovec" => &[".love"],
        "java" | "javaw" => return java_game(args),
        "mono" | "mono-sgen" => &[".exe"],
        p if is_python(p) => &[".py", ".pyc", ".pyw", ".pyz"],
        // Content-only Unreal games run the engine's own program, started by the game's launcher
        // with its project: `..\..\..\Zombielicious\Zombielicious.uproject`.
        p if unreal_runner(p) => &[".uproject"],
        _ => return None,
    };
    // A path or a URL; a URL's query left out.
    let file = |a: &str| a.split(['?', '#']).next().unwrap_or_default().rsplit(['/', '\\']).next().unwrap_or_default().to_owned();
    let full = |a: &str| if a.starts_with('/') { a.to_owned() } else { format!("{}/{a}", cwd()) };
    if let Some(a) = args.iter().find(|a| endings.iter().any(|e| file(a).to_ascii_lowercase().ends_with(e))) {
        let name = file(a);
        if unreal_runner(program) {
            return Some(format!("{} (Unreal)", name.trim_end_matches(".uproject")));
        }
        let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s).to_ascii_lowercase();
        if !is_python(program) || !["main", "__main__", "game", "run", "run_game", "start", "play", "launcher"].contains(&stem.as_str()) {
            return Some(name);
        }
        let path = full(a);
        let folder = path.split('/').rev().skip(1).find(|d| !d.is_empty() && *d != "." && !["src", "game", "bin", "data"].contains(d))?;
        return Some(format!("{folder} (Python)"));
    }
    // `python -m <module>`, `love <folder>` (a game kept as a folder; "." is the one it runs in).
    if is_python(program) {
        return args.iter().position(|a| a == "-m").and_then(|i| args.get(i + 1)).cloned();
    }
    if program.starts_with("love") {
        let path = full(args.iter().find(|a| !a.starts_with('-'))?);
        return path.split('/').rev().find(|d| !d.is_empty() && *d != ".").map(|d| format!("{d} (LÖVE)"));
    }
    None
}

/// The game Java runs: the jar of `-jar <file>`, else the main class (`-cp <jars> <class>`), or
/// the module of `-m <module>/<class>`.
fn java_game(args: &[String]) -> Option<String> {
    let mut args = args.iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "-jar" => return args.next().map(|f| f.rsplit(['/', '\\']).next().unwrap_or_default().to_owned()),
            "-m" | "--module" => return args.next().cloned(),
            "-cp" | "-classpath" | "--class-path" | "-p" | "--module-path" | "--add-modules" | "--add-opens" | "--add-exports"
            | "--add-reads" | "--patch-module" | "--limit-modules" | "--upgrade-module-path" => {
                args.next();
            }
            a if !a.starts_with('-') => return Some(a.to_owned()),
            _ => {}
        }
    }
    None
}

/// Python, as a program: python, python3, python3.12, python2.7, pythonw.exe.
fn is_python(program: &str) -> bool {
    let p = program.trim_end_matches(".exe").trim_end_matches('w');
    p.strip_prefix("python").is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

/// Python running a Wine launcher (Proton's `proton` script, GE-Proton's too, or `umu-run`)
/// rather than a game. Steam injects its overlay into every process of a launch and the overlay
/// needs libGL, so Proton's launcher loads the GPU driver and looks like a pygame game.
fn wine_launcher(args: &[String]) -> bool {
    let script = args.iter().find(|a| !a.starts_with('-'));
    script.is_some_and(|a| ["proton", "umu-run", "umu_run.py"].contains(&a.rsplit('/').next().unwrap_or_default()))
}

/// DOSBox and its forks, as programs (".exe" left off).
const DOSBOXES: &[&str] = &["dosbox", "dosbox-x", "dosbox-staging"];

/// The DOS game a DOSBox runs, by its folder: every DOSBox game is `dosbox.exe`, and all of them
/// shared one profile (GOG and Steam ship DOS games each with its own DOSBox: Tyrian 2000's is
/// `Tyrian 2000\DOSBOX\dosbox.exe`). The program's folder, or the one above it when that is
/// DOSBox's own; for a DOSBox installed system-wide (Lutris, a flatpak), the folder it runs in.
fn dosbox_game(argv0: &str, cwd: &str, home: &str) -> Option<String> {
    let parts = |p: &str| -> Vec<String> { p.split(['/', '\\']).filter(|s| !s.is_empty()).map(str::to_owned).collect() };
    let game = |mut dirs: Vec<String>| {
        // DOSBox's own folder, and GOG's Linux layout (<game>/dosbox, <game>/data).
        while dirs.last().is_some_and(|d| {
            let d = d.to_ascii_lowercase();
            d.contains("dosbox") || ["game", "data", "bin"].contains(&d.as_str())
        }) {
            dirs.pop();
        }
        // A drive letter alone ("S:") isn't a game.
        dirs.pop().filter(|d| !d.ends_with(':'))
    };
    // A relative path ("dosbox/dosbox", as GOG's Linux start.sh runs it) is from the folder it
    // runs in; a Windows one starts with a drive.
    let absolute = argv0.starts_with('/') || argv0.get(1..2) == Some(":");
    let mut exe_dirs = if absolute { parts(argv0) } else { parts(&format!("{cwd}/{argv0}")) };
    exe_dirs.pop();
    let system = ["usr", "app", "bin", "sbin", "snap"].contains(&exe_dirs.first().map_or("", String::as_str));
    if let Some(g) = (argv0.contains(['/', '\\']) && !system).then(|| game(exe_dirs)).flatten() {
        return Some(g);
    }
    let cwd = cwd.trim_end_matches('/');
    if cwd.is_empty() || cwd == home.trim_end_matches('/') {
        return None;
    }
    game(parts(cwd))
}

/// The folder of the NW.js app a process belongs to (RPG Maker MV and MZ, Construct and other
/// HTML5 games): its program sits next to package.json or package.nw and NW.js's own files.
/// Under Wine the program is the .exe it maps (RPG Maker MZ games ship for Windows only).
fn nwjs_app(pid: u32) -> Option<PathBuf> {
    let windows = exe_name(pid).to_ascii_lowercase().ends_with(".exe");
    let program = if windows { program_of(pid)? } else { os::exe(pid)? };
    let dir = program.parent()?;
    let inside = os::inside(pid, dir);
    let has = |f: &str| inside.join(f).exists();
    let nw = has("nw.pak") || has("resources.pak") || has("lib/libnw.so") || has("nw.dll");
    ((has("package.json") || has("package.nw")) && nw).then(|| dir.to_path_buf())
}

/// An NW.js game runs its JavaScript, and keeps its values, in a Chromium renderer process
/// (`--type=renderer`; NW.js 0.12's renderers are forked from its zygote and keep the zygote's
/// command line); the window's own process (GTK) and the GPU process hold none of them.
fn nwjs_renderer(pid: u32) -> bool {
    let cmd = cmdline(pid);
    let parent = os::parent(pid);
    cmd.contains(" --type=renderer") || cmd.contains(" --type=zygote") && parent.is_some_and(|p| cmdline(p).contains(" --type=zygote"))
}

/// An NW.js game's name: an RPG Maker game's title (`www/data/System.json`, MZ's
/// `data/System.json`), else its folder's.
fn nwjs_name(pid: u32, dir: &Path) -> String {
    let inside = os::inside(pid, dir);
    let title = ["www/data/System.json", "data/System.json"]
        .iter()
        .find_map(|f| fs::read_to_string(inside.join(f)).ok())
        .and_then(|t| crate::json::parse(&t))
        .and_then(|v| v.get("gameTitle").and_then(|t| t.str()).map(|t| t.trim().replace('\t', " ")))
        .filter(|t| !t.is_empty());
    title.unwrap_or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "NW.js game".into()))
}

/// GameMaker games keep every number as a double; they ship their assets as data.win (or
/// game.unx on Linux) next to the program, or packed into it (`gml::is_runner`).
fn is_gamemaker(pid: u32, exe: &str) -> bool {
    let Ok(regions) = maps(pid) else { return false };
    let program = regions.iter().find(|r| r.path.rsplit('/').next().is_some_and(|n| n.eq_ignore_ascii_case(exe)));
    program.is_some_and(|r| gml::is_runner(pid, Some(std::path::Path::new(&r.path))))
}

/// Java games run on a JVM (HotSpot's libjvm.so, or jvm.dll under Proton). Its garbage collector
/// moves objects around, and one instruction of its bytecode interpreter writes the int fields
/// of every object: a code pattern found there led a limit into another object's reference field
/// and crashed Shattered Pixel Dungeon.
fn is_java(pid: u32) -> bool {
    let Ok(regions) = maps(pid) else { return false };
    regions.iter().any(|r| {
        let name = r.path.rsplit(['/', '\\']).next().unwrap_or_default();
        name == "libjvm.so" || name.eq_ignore_ascii_case("jvm.dll")
    })
}

/// Memory scramblers: Unity games using CodeStage Anti-Cheat Toolkit's obscured types keep their
/// numbers XORed with a per-value key, next to a plain decoy the game rewrites (Moonlighter's
/// health). Found by its namespace in the game's assemblies (Mono) or IL2CPP's metadata, in
/// <Game>_Data (from the maps, else next to the program).
fn scrambler(pid: u32, regions: &[Region]) -> Option<&'static str> {
    let mapped = regions.iter().find_map(|r| r.path.find("_Data/").map(|i| PathBuf::from(&r.path[..i + 5])));
    let beside = || {
        let program = program_path(pid, regions)?;
        Some(program.with_file_name(format!("{}_Data", program.file_stem()?.to_string_lossy())))
    };
    let data = mapped.or_else(beside)?;
    let data = os::inside(pid, &data);
    let assembly = |p: &PathBuf| {
        let name = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        name.ends_with(".dll") && (name.starts_with("assembly-csharp") || name.starts_with("actk"))
    };
    let managed = fs::read_dir(data.join("Managed")).into_iter().flatten().flatten().map(|e| e.path()).filter(assembly);
    let metadata = data.join("il2cpp_data/Metadata/global-metadata.dat");
    let needle = b"CodeStage.AntiCheat.ObscuredTypes";
    let uses = |p: PathBuf| fs::read(p).is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle));
    managed.chain([metadata]).any(uses).then_some("CodeStage Anti-Cheat Toolkit")
}

/// The program a process runs (`program_path`).
fn program_of(pid: u32) -> Option<PathBuf> {
    maps(pid).ok().and_then(|m| program_path(pid, &m))
}

/// Adobe AIR games (air.rs): the AIR runtime mapped.
fn is_air(pid: u32) -> bool {
    maps(pid).is_ok_and(|r| air::runs_air(&r))
}

/// Python games (python.rs): CPython mapped (libpython, Ren'Py's librenpython) or running.
fn is_python_game(pid: u32, exe: &str) -> bool {
    maps(pid).is_ok_and(|r| python::runs_python(&r, exe))
}

/// Lua games (lua.rs): a Lua library mapped (LÖVE's LuaJIT), or Defold's engine.
fn is_lua(pid: u32) -> bool {
    let Ok(regions) = maps(pid) else { return false };
    lua::runs_lua(pid, &regions, program_path(pid, &regions).as_deref())
}

/// RPG Maker MV and MZ games (js.rs): NW.js running `www/js/rpg_core.js` (MV) or
/// `js/rmmz_core.js` (MZ) from next to the program.
fn is_rpgmaker(pid: u32) -> bool {
    let program = maps(pid).ok().and_then(|m| program_path(pid, &m));
    js::is_rpgmaker(pid, program.as_deref())
}

/// Godot games load <program>.pck from next to the program, or carry it at the end of the
/// program (which then ends with its magic, "GDPC").
fn is_godot(pid: u32) -> bool {
    let Some(program) = maps(pid).ok().and_then(|m| program_path(pid, &m)) else { return false };
    let inside = |p: &Path| os::inside(pid, p);
    if inside(&program.with_extension("pck")).exists() || program.with_extension("pck").exists() {
        return true;
    }
    let ends_with_magic = |p: &Path| {
        let Ok(f) = File::open(p) else { return false };
        let Ok(len) = f.metadata().map(|m| m.len()) else { return false };
        let mut b = [0u8; 4];
        len >= 4 && f.read_exact_at(&mut b, len - 4).is_ok() && &b == b"GDPC"
    };
    ends_with_magic(&inside(&program)) || ends_with_magic(&program)
}

/// Why Ferret won't touch a game: the line is no cheating where other players or the player's
/// account can be reached.
enum Refusal {
    /// Loaded in the game, or running beside it in its Wine prefix.
    AntiCheat(String),
    /// Running elsewhere on the computer (Windows: anti-cheat runs as services and kernel
    /// drivers that see every program, not only its game's).
    #[cfg_attr(not(windows), allow(dead_code))]
    AntiCheatElsewhere(String),
    /// Its store lists it as played only with others (or as an MMO or a battle royale).
    Online,
    /// Steam lists Valve Anti-Cheat; it can't be seen running (it's in the Steam client).
    Vac,
    /// Its characters get flagged online for edits made offline (`launchers::FLAGS_EDITS`).
    FlagsEdits,
}

impl Refusal {
    /// The games list's word for it.
    fn code(&self) -> String {
        match self {
            Refusal::AntiCheat(ac) => format!("anti-cheat {ac}"),
            Refusal::AntiCheatElsewhere(ac) => format!("anti-cheat-elsewhere {ac}"),
            Refusal::Online => "online".into(),
            Refusal::Vac => "vac".into(),
            Refusal::FlagsEdits => "edits".into(),
        }
    }

    fn why(&self) -> String {
        match self {
            Refusal::AntiCheat(ac) => format!("{ac} is running with the game"),
            Refusal::AntiCheatElsewhere(ac) => format!("{ac} is running on the computer"),
            Refusal::Online => "the game is played online only".into(),
            Refusal::Vac => "the game uses Valve Anti-Cheat (Source games started with -insecure are allowed)".into(),
            Refusal::FlagsEdits => "the game flags edited characters when they go online".into(),
        }
    }
}

struct Verdict {
    refused: Option<Refusal>,
    /// Anti-cheat shipped in its folder or listed for it but not running: only a warning
    /// (most such games have a single-player mode that runs without it).
    ships: Option<String>,
}

/// Anti-cheat running now outside the games: `programs` (running in a Wine prefix: the prefix
/// and its name; on Windows anywhere on the computer, prefix empty) and `resident`, anti-cheat
/// that is always on (Windows: `anticheat::RESIDENT`), a warning only.
#[derive(Default)]
struct OnComputer {
    programs: Vec<(String, &'static str)>,
    resident: Option<&'static str>,
}

/// Blocked: anti-cheat running, online only, VAC (unless it can't reach secured servers), or a
/// game that flags edited characters. Warned: anti-cheat shipped or listed but not running.
/// `running` = `on_computer()`.
fn verdict(pid: u32, running: &OnComputer) -> Verdict {
    let maps = maps(pid).unwrap_or_default();
    let env = environ(pid);
    let about = about(&env);
    let refused = if let Some(r) = running_anti_cheat(&maps, &env, &running.programs) {
        Some(r)
    } else if about.online_only {
        Some(Refusal::Online)
    } else if about.flags_edits {
        Some(Refusal::FlagsEdits)
    } else if about.vac && !vac_safe(pid, &maps) {
        Some(Refusal::Vac)
    } else {
        None
    };
    let ships = if refused.is_some() { None } else { shipped_anti_cheat(pid, &maps, &env, about.listed) };
    Verdict { refused, ships }
}

/// The Wine prefix a program runs in (Proton's, Heroic's, Lutris's, umu's).
#[cfg(not(windows))]
fn wine_prefix(env: &HashMap<String, String>) -> Option<&String> {
    env.get("WINEPREFIX").or_else(|| env.get("STEAM_COMPAT_DATA_PATH")).filter(|p| !p.is_empty())
}

/// Anti-cheat running now outside games. Linux: anti-cheat programs and the Wine prefix each
/// runs in (under Proton EasyAntiCheat_EOS.exe, BEService.exe and start_protected_game.exe run
/// beside the game). Windows: anti-cheat programs, services and drivers anywhere on the
/// computer (they watch every program there), and resident ones apart.
fn on_computer() -> OnComputer {
    #[cfg(windows)]
    {
        let names = os::running_programs();
        let mut programs: Vec<(String, &'static str)> = Vec::new();
        for ac in names.iter().filter(|n| !anticheat::resident(n)).filter_map(|n| anticheat::named(n)) {
            if !programs.iter().any(|(_, a)| *a == ac) {
                programs.push((String::new(), ac));
            }
        }
        let resident = names.iter().filter(|n| anticheat::resident(n)).find_map(|n| anticheat::named(n));
        OnComputer { programs, resident }
    }
    #[cfg(not(windows))]
    OnComputer {
        programs: os::own_pids()
            .into_iter()
            .filter_map(|pid| {
                let ac = anticheat::named(&exe_name(pid))?;
                Some((wine_prefix(&environ(pid))?.clone(), ac))
            })
            .collect(),
        resident: None,
    }
}

/// Anti-cheat loaded in the game, running in its Wine prefix, or (Windows) on the computer.
fn running_anti_cheat(maps: &[Region], env: &HashMap<String, String>, programs: &[(String, &'static str)]) -> Option<Refusal> {
    if let Some(ac) = maps.iter().find_map(|r| anticheat::in_path(&r.path)) {
        return Some(Refusal::AntiCheat(ac.to_owned()));
    }
    #[cfg(windows)]
    {
        let _ = env;
        programs.first().map(|(_, ac)| Refusal::AntiCheatElsewhere((*ac).to_owned()))
    }
    #[cfg(not(windows))]
    {
        let prefix = wine_prefix(env)?;
        programs.iter().find(|(p, _)| p == prefix).map(|(_, ac)| Refusal::AntiCheat((*ac).to_owned()))
    }
}

/// Anti-cheat that started in the attached game (or, on Windows, on the computer) since the
/// last look.
fn started_anti_cheat(pid: u32) -> Option<Refusal> {
    let maps = maps(pid).ok()?;
    running_anti_cheat(&maps, &environ(pid), &on_computer().programs)
}

/// Call of Duty's single-player programs, shipped beside the multiplayer ones of the same Steam
/// app (which Steam lists with VAC). Not checked on the games yet.
const SINGLE_PLAYER_PROGRAMS: &[&str] = &["iw3sp.exe", "iw4sp.exe", "iw5sp.exe", "t6sp.exe"];

/// A VAC game that can't reach VAC-secured servers: a single-player program, or a Source or
/// GoldSrc game started with -insecure (Valve: it can't join secured servers then). Only for
/// those engines: other games ignore the option.
fn vac_safe(pid: u32, maps: &[Region]) -> bool {
    if SINGLE_PLAYER_PROGRAMS.contains(&exe_name(pid).to_ascii_lowercase().as_str()) {
        return true;
    }
    let insecure = os::argv(pid).iter().any(|a| a.eq_ignore_ascii_case("-insecure"));
    let engine = |r: &Region| {
        let name = r.path.rsplit(['/', '\\']).next().unwrap_or_default().to_ascii_lowercase();
        ["tier0.dll", "libtier0.so", "libtier0_client.so", "hw.dll", "hw.so", "sw.dll"].contains(&name.as_str())
    };
    insecure && maps.iter().any(engine)
}

/// Anti-cheat shipped in the game's folder, or listed by AreWeAntiCheatYet for it (`listed`:
/// what its launcher's IDs found there).
fn shipped_anti_cheat(pid: u32, maps: &[Region], env: &HashMap<String, String>, listed: Option<String>) -> Option<String> {
    if listed.is_some() {
        return listed;
    }
    // Seen from inside the game's sandbox (pressure-vessel, flatpak).
    let inside = |p: &Path| os::inside(pid, p);
    let folder = match env.get("STEAM_COMPAT_INSTALL_PATH") {
        Some(dir) => PathBuf::from(dir),
        None => anticheat::game_folder(&program_path(pid, maps)?),
    };
    if let Some(ac) = anticheat::in_folder(&inside(&folder)) {
        return Some(ac.to_owned());
    }
    // Any launcher (or none): the folders and program named like a listed game.
    let exe = exe_name(pid);
    // Unreal: <game>/<project>/Binaries/Win64, so two folders up as well.
    let folders = folder.ancestors().take(3).filter_map(Path::file_name);
    let names: Vec<String> =
        folders.chain(Path::new(&exe).file_stem()).map(|n| n.to_string_lossy().into_owned()).collect();
    launchers::listed_as(&env, &names)
}

/// The program's file, as the game's sandbox sees it: Wine maps the .exe by its Linux path,
/// native programs are their own (`os::exe`).
fn program_path(pid: u32, maps: &[Region]) -> Option<PathBuf> {
    let exe = exe_name(pid);
    let mapped = maps.iter().map(|r| Path::new(&r.path)).find(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(&*exe)));
    match mapped {
        Some(p) => Some(p.to_path_buf()),
        None => os::exe(pid),
    }
}

/// Unreal games start as <game>/<Project>.exe, a stub that only starts
/// <game>/<Project>/Binaries/Win64/<Project>-Win64-Shipping.exe and waits for it: the stub's
/// folder when `program` is such a game.
fn unreal_stub_folder(program: &Path) -> Option<&Path> {
    let binaries = program.parent()?.parent()?;
    if !binaries.file_name()?.eq_ignore_ascii_case("binaries") {
        return None;
    }
    binaries.parent()?.parent()
}

/// The Steam app ID a game was started with (umu sets 0 or "default" for games not on Steam).
fn steam_id(env: &HashMap<String, String>) -> Option<&String> {
    ["SteamAppId", "SteamGameId"]
        .iter()
        .filter_map(|k| env.get(*k))
        .find(|id| *id != "0" && !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

fn about(env: &HashMap<String, String>) -> launchers::About {
    launchers::about(env, steam_id(env).and_then(|id| id.parse().ok()))
}

// --- How a value is stored. Addresses in commands are "<hex>[:type]", i32 when left out.

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Kind {
    I32,
    F32,
    F64,
    /// A 4-byte integer hidden from cheat tools: the word at the address XOR the next word
    /// (the key). The game re-encodes it with the same key on every change.
    Xor,
    /// A 2-byte whole number, unsigned: Frostbite keeps ammo so (Andromeda: loaded and reserve
    /// side by side, read with movzx); read as 4 bytes, the two together never match.
    U16,
    /// A GameMaker value (gml.rs): 16 bytes {value, flags, kind}, read as the kind of number it
    /// holds now (a double, an int32 or an int64: the game turns one into another as it likes),
    /// written as a double. Never searched for: values saved by a GameMaker path get it.
    Gm,
    /// A Lua value (lua.rs): 8 bytes, a double while it holds a number; never written once it
    /// holds something else (a table that grew moved its entries: the old place is freed).
    /// Values saved by a Lua path get it.
    Lua,
    /// A Python value (python.rs): the slot pointing to a number object (Python's numbers are
    /// immutable objects), read as the number it points to, written in place or by pointing
    /// the slot to a cached small int (`python::write_slot`). Values saved by a Python path.
    Py,
    /// A small integer of 32-bit V8 or of V8 8.x (js.rs, RPG Maker MV on 32-bit NW.js, MZ): 4
    /// bytes holding the number times two (bit 0 clear; set, it's a pointer: not a number now).
    /// Values saved by a JavaScript path on such a game.
    Smi,
}

impl Kind {
    /// The kinds searches look for (all but `Gm` and `Lua`).
    const ALL: [Kind; 5] = [Kind::I32, Kind::F32, Kind::F64, Kind::Xor, Kind::U16];

    fn name(self) -> &'static str {
        match self {
            Kind::I32 => "i32",
            Kind::F32 => "f32",
            Kind::F64 => "f64",
            Kind::Xor => "xor",
            Kind::U16 => "u16",
            Kind::Gm => "gm",
            Kind::Lua => "lua",
            Kind::Py => "py",
            Kind::Smi => "smi",
        }
    }

    fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().chain([Kind::Gm, Kind::Lua, Kind::Py, Kind::Smi]).find(|k| k.name() == s)
    }

    fn size(self) -> usize {
        match self {
            Kind::U16 => 2,
            Kind::I32 | Kind::F32 | Kind::Smi => 4,
            Kind::F64 | Kind::Xor | Kind::Lua | Kind::Py => 8,
            Kind::Gm => 16,
        }
    }

    fn decode(self, b: &[u8]) -> f64 {
        match self {
            Kind::I32 => i32::from_le_bytes(b[..4].try_into().unwrap()) as f64,
            Kind::F32 => f32::from_le_bytes(b[..4].try_into().unwrap()) as f64,
            Kind::F64 => f64::from_le_bytes(b[..8].try_into().unwrap()),
            Kind::U16 => u16::from_le_bytes(b[..2].try_into().unwrap()) as f64,
            Kind::Xor => {
                (i32::from_le_bytes(b[..4].try_into().unwrap()) ^ i32::from_le_bytes(b[4..8].try_into().unwrap())) as f64
            }
            Kind::Gm => gml::rvalue_number(b).unwrap_or(f64::NAN),
            Kind::Lua => lua::number(b).unwrap_or(f64::NAN),
            // Read through the slot (`read_value`).
            Kind::Py => f64::NAN,
            Kind::Smi => match i32::from_le_bytes(b[..4].try_into().unwrap()) {
                w if w & 1 == 0 => (w >> 1) as f64,
                _ => f64::NAN,
            },
        }
    }

    /// Whether a value that went from `lo` to `hi` (either way round) meanwhile can be what the
    /// screen showed as `n`. Games show fractions rounded, cut off or rounded up (countdowns:
    /// Lumencraft's wave timer shows 6:37 with 396.4 s left); a decimal like 1.2 may be kept as
    /// a whole number of tenths (12).
    fn fits(self, lo: f64, hi: f64, n: Shown) -> bool {
        let (lo, hi) = (lo.min(hi), lo.max(hi));
        match self {
            Kind::I32 | Kind::Xor | Kind::U16 | Kind::Smi => lo <= n.scaled && n.scaled <= hi,
            Kind::F32 | Kind::F64 | Kind::Gm | Kind::Lua | Kind::Py => lo < n.value + n.step && hi > n.value - n.step,
        }
    }

    fn show(self, v: f64) -> String {
        match self {
            Kind::F32 => (v as f32).to_string(),
            _ => v.to_string(),
        }
    }
}

/// A number read off the screen: "1250", "1.2", "-3".
#[derive(Clone, Copy)]
struct Shown {
    value: f64,
    /// The last shown digit's worth: 1, 0.1, 0.01...
    step: f64,
    /// All digits as one whole number: 1.2 -> 12.
    scaled: f64,
}

impl Shown {
    fn parse(s: &str) -> Option<Shown> {
        let value: f64 = s.parse().ok().filter(|v: &f64| v.is_finite())?;
        if !s.bytes().all(|c| c.is_ascii_digit() || c == b'.' || c == b'-') {
            return None;
        }
        let decimals = s.split_once('.').map_or(0, |(_, b)| b.len() as i32);
        let scaled = s.replace('.', "").parse::<f64>().ok()?;
        Some(Shown { value, step: 10f64.powi(-decimals), scaled })
    }
}

fn read_value(mem: &Mem, addr: u64, kind: Kind) -> Option<f64> {
    if kind == Kind::Py {
        return python::read_slot(mem, addr);
    }
    let mut b = [0u8; 16];
    mem.read_exact_at(&mut b[..kind.size()], addr).ok()?;
    Some(kind.decode(&b)).filter(|v| !matches!(kind, Kind::Gm | Kind::Lua | Kind::Smi) || !v.is_nan())
}

fn write_value(mem: &Mem, addr: u64, kind: Kind, v: f64) -> io::Result<()> {
    match kind {
        Kind::I32 => mem.write_all_at(&(v.round() as i32).to_le_bytes(), addr),
        Kind::F32 => mem.write_all_at(&(v as f32).to_le_bytes(), addr),
        Kind::F64 => mem.write_all_at(&v.to_le_bytes(), addr),
        Kind::U16 => mem.write_all_at(&(v.round().clamp(0.0, 65535.0) as u16).to_le_bytes(), addr),
        Kind::Xor => {
            let mut key = [0u8; 4];
            mem.read_exact_at(&mut key, addr + 4)?;
            mem.write_all_at(&((v.round() as i32) ^ i32::from_le_bytes(key)).to_le_bytes(), addr)
        }
        // A number of any kind becomes a double (kind 0); the flags stay.
        Kind::Gm => {
            let mut b = [0u8; 16];
            mem.read_exact_at(&mut b, addr)?;
            if gml::rvalue_number(&b).is_none() {
                return Err(io::Error::other("not a GameMaker number now"));
            }
            mem.write_all_at(&v.to_le_bytes(), addr)?;
            mem.write_all_at(&0u32.to_le_bytes(), addr + 12)
        }
        Kind::Lua => {
            let mut b = [0u8; 8];
            mem.read_exact_at(&mut b, addr)?;
            if lua::number(&b).is_none() {
                return Err(io::Error::other("not a Lua number now"));
            }
            mem.write_all_at(&v.to_le_bytes(), addr)
        }
        Kind::Py => python::write_slot(mem, addr, v),
        Kind::Smi => {
            let mut b = [0u8; 4];
            mem.read_exact_at(&mut b, addr)?;
            if b[0] & 1 != 0 {
                return Err(io::Error::other("not a small integer now"));
            }
            let n = v.round().clamp(-(1 << 30) as f64, ((1 << 30) - 1) as f64) as i32;
            mem.write_all_at(&(n << 1).to_le_bytes(), addr)
        }
    }
}

/// "<hex>[:type]"
fn parse_loc(a: &str) -> Option<(u64, Kind)> {
    let (addr, kind) = match a.split_once(':') {
        Some((addr, kind)) => (addr, Kind::parse(kind)?),
        None => (a, Kind::I32),
    };
    Some((parse_addr(addr)?, kind))
}

/// Writable memory that can hold the game's values. Not the GPU driver's mappings
/// (`/dev/nvidia0`, `/dev/dri/*`, dma-bufs): video memory reads very slowly through
/// `/proc/<pid>/mem` (on Creeper World 4, 157 MB of them took 3.7 s of a 4 s pass; the game's
/// own 1.2 GB took 0.3 s).
pub fn scannable(r: &Region) -> bool {
    let device = r.path.starts_with("/dev/") && !r.path.starts_with("/dev/shm/") && !r.path.starts_with("/dev/zero");
    r.perms.starts_with("rw")
        && !device
        && !r.path.contains("dmabuf")
        && !matches!(r.path.as_str(), "[vvar]" | "[vvar_vclock]" | "[vsyscall]")
}

// --- Limits: keep a value within a range, writing only when the game moves it out.

struct Limit {
    name: String,
    addr: u64,
    kind: Kind,
    min: Option<f64>,
    max: Option<f64>,
    sites: Vec<Site>,
    /// Pointer paths to the value; when there are any, the value is found through them on
    /// every check instead of through `sites` and the guard.
    paths: Vec<PtrPath>,
    /// The places `paths` led to before the value moved (see `pointers::vote`).
    left: Vec<u64>,
    /// Which of `paths` led to `addr` when last followed.
    came: Vec<bool>,
    /// A named path to every place the value is kept (all of an item's stacks): when there is
    /// one, all of them are found through it on every check and kept in range.
    named: Option<NamedLimit>,
    // First bytes of the object holding the value (its type pointer). If they
    // change, the object is gone and writing would corrupt unrelated memory. 0: no object (see
    // `guard_of`).
    guard_addr: u64,
    guard: [u8; 4],
    fixes: u64,
    restores: u64,
    /// Found through pointer paths: whether the game has changed the value at the place they
    /// lead to since they first led there, and the value seen there until it does. Before the
    /// value exists (a game's menu) paths may agree on another place (Age of War's title menu:
    /// 154 of 247 on a place holding 0); the game changing it shows it's the value.
    changed: bool,
    unchanged: Option<f64>,
    paused: Option<&'static str>,
    /// Turned off by the player (`switch`, a hotkey): still followed, never written.
    off: bool,
    retry_at: Instant,
    /// When its code patterns last found it (the object may be replaced while the old one stays
    /// readable, which the guard can't see).
    checked_at: Instant,
    /// Whether all its code patterns were searched for a static pointer to the object.
    searched_code: bool,
    /// A thread is finding it by its code patterns now (`limiter_loop`): not started again.
    finding: bool,
    /// How long until its code patterns are checked again: `RECHECK_EVERY`, doubled every time
    /// they find it where it was, up to `RECHECK_MOST` (the check stops every game thread for
    /// a moment); back to `RECHECK_EVERY` once it moved.
    recheck_every: Duration,
}

/// A named path of any kind: through objects the game names (Unity, names.rs), to an entry of
/// the game's dictionaries (Godot, godot.rs), through its scripts' variables (Godot,
/// gdscript.rs), through its Unreal objects (ue.rs), to a field of a Mono class's live
/// objects (Unity, Mono or IL2CPP; mono.rs), from an RPG Maker MV or MZ game's globals (js.rs) or
/// to a GameMaker instance's or the global object's variable (gml.rs).
#[derive(Clone)]
enum Named {
    Objects(NamedPath),
    Dict(DictPath),
    Script(ScriptPath),
    Unreal(UePath),
    Mono(MonoPath),
    Js(JsPath),
    Gml(GmlPath),
    Lua(LuaPath),
    Java(JavaPath),
    Py(PyPath),
    Air(AirPath),
}

impl Named {
    fn parse(text: &str) -> Option<Named> {
        if text.starts_with("ue:") {
            return UePath::parse(text).map(Named::Unreal);
        }
        if text.starts_with("gd:") {
            return ScriptPath::parse(text).map(Named::Script);
        }
        if text.starts_with("mono:") || text.starts_with("il2cpp:") {
            return MonoPath::parse(text).map(Named::Mono);
        }
        if text.starts_with("js:") {
            return JsPath::parse(text).map(Named::Js);
        }
        if text.starts_with("gml:") {
            return GmlPath::parse(text).map(Named::Gml);
        }
        if text.starts_with("lua:") {
            return LuaPath::parse(text).map(Named::Lua);
        }
        if text.starts_with("java:") {
            return JavaPath::parse(text).map(Named::Java);
        }
        if text.starts_with("py:") {
            return PyPath::parse(text).map(Named::Py);
        }
        if text.starts_with("air:") {
            return AirPath::parse(text).map(Named::Air);
        }
        match text.starts_with('{') {
            true => DictPath::parse(text).map(Named::Dict),
            false => NamedPath::parse(text).map(Named::Objects),
        }
    }

    fn is_named(text: &str) -> bool {
        text.starts_with('"') || text.starts_with('{') || text.starts_with("ue:") || text.starts_with("gd:") || text.starts_with("mono:") || text.starts_with("il2cpp:") || text.starts_with("js:") || text.starts_with("gml:") || text.starts_with("lua:") || text.starts_with("java:") || text.starts_with("py:") || text.starts_with("air:")
    }

    /// Walked again right before every write (not only every `LIMIT_FIND_EVERY`): Java and
    /// V8 (RPG Maker) move their objects when they compact memory, and the old place is
    /// reused; Python makes a new number object at every change (`hp -= 1`) and frees the old
    /// one.
    fn fresh(&self) -> bool {
        matches!(self, Named::Java(_) | Named::Py(_) | Named::Js(_))
    }

    /// Walked again before a write only when one is due (the number at its last place is out
    /// of range): a walk of an RPG Maker path 20 times a second took ~3% of a core (reading
    /// the game's memory map each time). A number that moved and stayed in range waits for
    /// the next `LIMIT_FIND_EVERY` walk; a write always follows a walk.
    fn walk_when_due(&self) -> bool {
        matches!(self, Named::Js(_))
    }

    /// Where it leads from the roots found before (no search).
    fn walk(&self, heap: &Heap, roots: &[u64]) -> Vec<u64> {
        match self {
            Named::Objects(p) => heap.walk(roots, p),
            Named::Dict(p) => godot::walk(heap, roots, p),
            Named::Script(p) => gdscript::walk(heap, roots, p),
            Named::Mono(p) => mono::walk(heap, roots, p),
            Named::Js(p) => js::walk(heap, roots, p),
            Named::Gml(p) => gml::walk(heap, program_of(heap.pid).as_deref(), roots, p),
            Named::Lua(p) => lua::walk(heap, roots, p),
            Named::Java(p) => java::walk(heap, roots, p),
            Named::Py(p) => python::walk(heap, roots, p),
            Named::Air(p) => air::walk(heap, roots, p),
            Named::Unreal(p) => unreal(heap.pid, heap.mem()).map(|ue| ue.walk(heap.mem(), roots, p)).unwrap_or_default(),
        }
    }

    /// Searches the game's memory for its roots (seconds; Unreal's object list: a fraction of one).
    fn find_roots(&self, heap: &Heap) -> Vec<u64> {
        match self {
            Named::Objects(p) => heap.find_roots(p),
            Named::Dict(p) => godot::find_roots(heap, p),
            Named::Script(p) => gdscript::find_roots(heap, p),
            Named::Mono(p) => mono::find_roots(heap, p),
            Named::Js(p) => js::find_roots(heap, p),
            Named::Gml(p) => gml::find_roots(heap, program_of(heap.pid).as_deref(), p),
            Named::Lua(p) => lua::find_roots(heap, p),
            Named::Java(p) => java::find_roots(heap, p),
            Named::Py(p) => python::find_roots(heap, p),
            Named::Air(p) => air::find_roots(heap, p),
            Named::Unreal(p) => unreal(heap.pid, heap.mem()).map(|ue| ue.roots(heap.mem(), p)).unwrap_or_default(),
        }
    }
}

/// The game's Unreal objects, when it's an Unreal game.
fn unreal(pid: u32, mem: &Mem) -> Result<std::sync::Arc<ue::Ue>, String> {
    let exe = exe_name(pid);
    ue::shared(pid, mem, &exe, || pointers::modules(pid, mem))
}

/// Places one value is kept in at most when found by name (stacks of one item).
const MAX_NAMED_PLACES: usize = 32;

/// Why the places a named path leads to aren't one value kept in several places, if they
/// aren't: a path through bytes that only look like a name leads all over memory (Forager has
/// no named objects; a path through two random characters led to 95 places, and a write went
/// to all of them). Places of one value are the same field of objects of one kind: few, lined
/// up alike, holding plausible numbers.
fn named_doubt(heap: &Heap, leads: &[u64], kind: Kind, path: &Named) -> Option<String> {
    let mem = heap.mem();
    if leads.len() > MAX_NAMED_PLACES {
        return Some(format!("it leads to {} places, more than one value is kept in", leads.len()));
    }
    // An Unreal path's places are the properties the game itself describes (a stack and a
    // tally of it may sit at different alignments).
    if !matches!(path, Named::Unreal(_)) && leads.iter().any(|a| a % 8 != leads[0] % 8) {
        return Some("its places don't line up like the same field of objects".into());
    }
    // An RPG Maker whole number is a plain i32 in 64-bit V8 and doubled ("smi") in 32-bit V8
    // and V8 8.x (MZ): written as the other, half or twice the number lands there, an odd
    // word a pointer to nowhere (the game crashed: an MZ game's HP limit saved as i32).
    if let Named::Js(_) = path {
        let ints = js::int_kind(heap);
        if matches!(kind, Kind::I32 | Kind::Smi) && ints.is_some_and(|k| k != kind.name()) {
            return Some(format!("this game keeps whole numbers as {}, not {}", ints.unwrap_or_default(), kind.name()));
        }
    }
    let plausible = |v: f64| v.is_finite() && v.abs() < 1e9 && (v == 0.0 || v.abs() >= 1e-6);
    if !leads.iter().all(|&a| read_value(mem, a, kind).is_some_and(plausible)) {
        return Some("some of its places don't hold a number".into());
    }
    None
}

struct NamedLimit {
    path: Named,
    roots: Vec<u64>,
    found_at: Option<Instant>,
    addrs: Vec<u64>,
    /// A search for its objects is running (in a thread of its own: it takes seconds, and the
    /// value may be limited meanwhile at the address its code patterns found).
    searching: bool,
}

#[derive(Default)]
struct Limiter {
    pid: u32,
    mem: Option<Mem>,
    width: usize,
    modules: Vec<Module>,
    limits: Vec<Limit>,
}

type SharedLimiter = Arc<Mutex<Limiter>>;

const RETRY_EVERY: Duration = Duration::from_secs(5);
const RECHECK_EVERY: Duration = Duration::from_secs(10);
const RECHECK_MOST: Duration = Duration::from_secs(160);
const RESOLVE_WAIT: Duration = Duration::from_secs(2);

fn read_guard(mem: &Mem, addr: u64) -> Option<[u8; 4]> {
    let mut g = [0u8; 4];
    mem.read_exact_at(&mut g, addr).ok().map(|_| g)
}

/// The guard for a value whose code patterns found `object` in their base register. Code that
/// addresses the value itself (a global array indexed by player: Jazz Jackrabbit 2's
/// `mov ecx,[esi+eax*4+0x5c8960]` with esi 0) has no object: the value sits at a fixed address
/// and can't be replaced, so it gets none (address 0). Before, its limits failed with "cannot
/// read the object at 0x0" and a restore never found it in the background.
fn guard_of(mem: &Mem, object: u64) -> Option<(u64, [u8; 4])> {
    if object < 0x10000 {
        return Some((0, [0; 4]));
    }
    read_guard(mem, object).map(|g| (object, g))
}

/// How often limits check their values (and write the ones out of range), and how often they
/// find where the values are (pointer paths, named paths). Lumencraft's lumen took a moment to
/// come back after a purchase.
const LIMIT_WRITE_EVERY: Duration = Duration::from_millis(50);
/// A named Python limit's state while its number is one the game shares (python.rs,
/// `write_slot`): still tried on every check.
const PY_SHARED: &str = "the game shares this number with its code, not written until the game changes it";
/// How often the attached game is checked for anti-cheat that started after attaching (some
/// games start it only when going online).
const ANTI_CHEAT_EVERY: Duration = Duration::from_secs(3);

/// Anti-cheat that started in the attached game, or on the computer (its pid and the refusal):
/// the limiter thread lets go of the game, and the next command drops the session.
static CAUGHT: Mutex<Option<(u32, Refusal)>> = Mutex::new(None);
const LIMIT_FIND_EVERY: Duration = Duration::from_millis(250);

/// Keeps limited values in range; paused limits are found again through their
/// saved code patterns and then resume. Values found through code patterns are also found
/// again every 10 s, and through the static pointer their object comes from (when there is
/// one) on every check. A limit with no bounds only keeps the address current.
fn limiter_loop(shared: SharedLimiter) {
    let mut found_at = Instant::now() - LIMIT_FIND_EVERY;
    let mut checked_at = Instant::now();
    loop {
        std::thread::sleep(LIMIT_WRITE_EVERY);
        if checked_at.elapsed() >= ANTI_CHEAT_EVERY {
            checked_at = Instant::now();
            let pid = shared.lock().unwrap().pid;
            if let Some(r) = (pid != 0).then(|| started_anti_cheat(pid)).flatten() {
                *shared.lock().unwrap() = Limiter::default();
                *CAUGHT.lock().unwrap() = Some((pid, r));
            }
        }
        let due: Vec<(String, Vec<Site>, bool)>;
        let named_due: Vec<(String, Named)>;
        let (pid, mem, width) = {
            let mut guard = shared.lock().unwrap();
            let Limiter { pid, mem, width, modules, limits } = &mut *guard;
            let Some(mem) = mem.as_ref() else { continue };
            let now = Instant::now();
            // Where the values are: every `LIMIT_FIND_EVERY`; in between, only their values
            // are checked.
            let find = found_at.elapsed() >= LIMIT_FIND_EVERY;
            if find {
                found_at = now;
            }
            // Pointer paths are cheap to follow: re-find the value on every check.
            if find && limits.iter().any(|l| !l.paths.is_empty() && l.paused.is_some() && l.retry_at <= now) {
                *modules = pointers::modules(*pid, mem);
            }
            for l in limits.iter_mut().filter(|l| find && !l.paths.is_empty() && (l.paused.is_none() || l.retry_at <= now)) {
                match pointers::vote(mem, modules, *width, &l.paths, l.addr, &l.left, &l.came) {
                    Some(v) if !v.clear() => {
                        l.paused = Some("its pointer paths don't agree on where it is, not writing");
                        l.retry_at = now + Duration::from_secs(1);
                    }
                    Some(v) => {
                        if v.addr != l.addr || l.paused.is_some() {
                            l.restores += 1;
                        }
                        if v.addr != l.addr {
                            (l.changed, l.unchanged) = (false, None);
                            pointers::moved_away(&mut l.left, l.addr, v.addr);
                        }
                        l.addr = v.addr;
                        l.came = v.leads;
                        l.paused = None;
                    }
                    None => {
                        l.paused = Some("its pointer paths lead nowhere right now, waiting");
                        l.retry_at = now + Duration::from_secs(1);
                    }
                }
            }
            // A write is due: the number at its places is out of range, or unreadable.
            let write_due = |l: &Limit, n: &NamedLimit| {
                let interim = [l.addr];
                let addrs = if n.addrs.is_empty() { &interim[..] } else { &n.addrs[..] };
                !l.off
                    && addrs.iter().any(|&a| match read_value(mem, a, l.kind) {
                        Some(v) => l.min.is_some_and(|min| v < min) || l.max.is_some_and(|max| v > max),
                        None => true,
                    })
            };
            let fresh_now = |l: &Limit| l.named.as_ref().is_some_and(|n| n.path.fresh() && (!n.path.walk_when_due() || write_due(l, n)));
            let fresh: Vec<bool> = limits.iter().map(fresh_now).collect();
            let heap = ((find || fresh.contains(&true)) && limits.iter().any(|l| l.named.is_some())).then(|| Heap::new(*pid, mem, *width).ok()).flatten();
            for (l, fresh) in limits.iter_mut().zip(fresh) {
                let (Some(n), Some(heap)) = (l.named.as_mut(), heap.as_ref()) else { continue };
                if !find && !fresh {
                    continue;
                }
                // Searching memory for its objects again takes seconds: done without the lock.
                let leads = n.path.walk(heap, &n.roots);
                if leads.is_empty() && n.found_at.is_none() && !l.sites.is_empty() && l.addr != 0 {
                    // Found by its code patterns (fast) while the first search by name runs.
                    l.paused = None;
                } else if leads.is_empty() {
                    l.paused = Some(match n.found_at {
                        None => "looking for it by name",
                        Some(_) => "it isn't anywhere right now (none in the game?), waiting",
                    });
                } else if named_doubt(heap, &leads, l.kind, &n.path).is_some() {
                    l.paused = Some("its name leads to places that aren't one value, not written");
                } else {
                    if leads != n.addrs || l.paused.is_some_and(|p| p != PY_SHARED) {
                        l.restores += 1;
                    }
                    l.addr = leads[0];
                    l.paused = l.paused.filter(|&p| p == PY_SHARED);
                }
                n.addrs = leads;
            }
            named_due = limits
                .iter_mut()
                .filter(|_| find)
                .filter_map(|l| {
                    let n = l.named.as_mut()?;
                    let due = !n.searching && n.addrs.is_empty() && n.found_at.is_none_or(|t| t.elapsed() >= NAMED_REFIND);
                    n.searching |= due;
                    due.then(|| (l.name.clone(), n.path.clone()))
                })
                .collect();
            for l in limits.iter_mut().filter(|l| l.paused.is_none() || l.paused == Some(PY_SHARED)) {
                if let Some(n) = &l.named {
                    let interim = [l.addr];
                    let addrs = if n.addrs.is_empty() { &interim[..] } else { &n.addrs[..] };
                    for &addr in addrs {
                        let Some(v) = read_value(mem, addr, l.kind).filter(|_| !l.off) else { continue };
                        let target = match (l.min, l.max) {
                            (_, Some(max)) if v > max => max,
                            (Some(min), _) if v < min => min,
                            _ => continue,
                        };
                        match write_value(mem, addr, l.kind, target) {
                            Ok(()) => {
                                l.fixes += 1;
                                l.paused = None;
                            }
                            // A Python number the game shares (a constant of its code): said, and
                            // tried again on each check (the game makes its own when it changes it).
                            Err(_) if l.kind == Kind::Py => l.paused = Some(PY_SHARED),
                            Err(_) => {}
                        }
                    }
                    continue;
                }
                // Its key gone: the slot holds another power or unit now.
                let key_gone = l.paths.is_empty() && !key_holds(mem, l.addr, l.sites.first().and_then(|s| s.key));
                if key_gone || (l.paths.is_empty() && l.guard_addr != 0 && read_guard(mem, l.guard_addr) != Some(l.guard)) {
                    l.paused = Some("the object holding it changed, finding it again");
                    l.retry_at = Instant::now();
                    continue;
                }
                let Some(v) = read_value(mem, l.addr, l.kind) else {
                    l.paused = Some("unreadable, finding it again");
                    l.retry_at = Instant::now();
                    continue;
                };
                if !l.changed {
                    match l.unchanged {
                        Some(first) if first != v => l.changed = true,
                        _ => {
                            l.unchanged = Some(v);
                            continue;
                        }
                    }
                }
                if l.off {
                    continue;
                }
                let target = match (l.min, l.max) {
                    (_, Some(max)) if v > max => max,
                    (Some(min), _) if v < min => min,
                    _ => continue,
                };
                if write_value(mem, l.addr, l.kind, target).is_ok() {
                    l.fixes += 1;
                }
            }
            let now = Instant::now();
            due = limits
                .iter_mut()
                .filter(|l| find && !l.finding && l.paths.is_empty() && l.named.is_none())
                .filter(|l| if l.paused.is_some() { l.retry_at <= now } else { l.checked_at + l.recheck_every <= now })
                .map(|l| {
                    l.finding = true;
                    (l.name.clone(), l.sites.clone(), !l.searched_code)
                })
                .collect();
            let Ok(mem) = mem.try_clone() else { continue };
            (*pid, mem, *width)
        };
        for (name, path) in named_due {
            let (Ok(mem), shared) = (mem.try_clone(), shared.clone()) else { continue };
            std::thread::spawn(move || {
                let roots = Heap::new(pid, &mem, width).map(|h| path.find_roots(&h)).unwrap_or_default();
                let mut guard = shared.lock().unwrap();
                if guard.pid != pid {
                    return;
                }
                if let Some(n) = guard.limits.iter_mut().find(|l| l.name == name).and_then(|l| l.named.as_mut()) {
                    n.found_at = Some(named_found_at(&n.roots, &roots));
                    n.roots = roots;
                    n.searching = false;
                }
            });
        }
        // Resolving traces the game for seconds (2 s wait, attaching every thread): on its own
        // thread, so the values are still written meanwhile. On this one, From Dust's Jellify
        // timer held at most 0 ran for 4 s of every 14 (the ring jumped back to 59). Values
        // found through the same code are looked for in one trace.
        let mut groups: Vec<Vec<(String, Vec<Site>, bool)>> = Vec::new();
        for d in due {
            match groups.iter_mut().find(|g| same_code(&g[0].1, &d.1)) {
                Some(g) => g.push(d),
                None => groups.push(vec![d]),
            }
        }
        for group in groups {
            let (Ok(mem), shared) = (mem.try_clone(), shared.clone()) else { continue };
            std::thread::spawn(move || refind(&shared, pid, &mem, width, &group));
        }
    }
}

/// The same code patterns, whatever their keys (others like a value: `cmd_others`).
fn same_code(a: &[Site], b: &[Site]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.pat == b.pat && a.off == b.off && a.base == b.base && a.disp == b.disp)
}

/// Finds limited values by their code patterns again (`limiter_loop`, on a thread of its own):
/// values going through the same code (From Dust's four power timers) in one trace, else
/// waiting ones kept the game traced nearly all the time.
fn refind(shared: &SharedLimiter, pid: u32, mem: &Mem, width: usize, due: &[(String, Vec<Site>, bool)]) {
    let all: Vec<Vec<Site>> = due.iter().map(|(_, sites, _)| sites.clone()).collect();
    let results = resolve_values(pid, mem, &all, width, RESOLVE_WAIT);
    for ((name, sites, search_code), r) in due.iter().zip(results) {
        refound(shared, pid, mem, width, name, sites, *search_code, r.ok());
    }
}

/// What `refind` found for one value (`None`: its code didn't run).
#[allow(clippy::too_many_arguments)]
fn refound(shared: &SharedLimiter, pid: u32, mem: &Mem, width: usize, name: &str, sites: &[Site], search_code: bool, r: Option<Resolved>) {
    let mut found = r.map(|r| {
        let disp = r.disp;
        (r, disp)
    });
    // The code that ran may not show where the object comes from; another site's may
    // (searching the code for every site is slow: once per value).
    if let Some((r, _)) = found.as_mut().filter(|(r, _)| r.holder.is_none() && search_code) {
        r.holder = holder_in_code(pid, mem, sites, r.is_64bit, width, r.addr);
    }
    let mut guard = shared.lock().unwrap();
    if guard.pid != pid {
        return;
    }
    let Some(l) = guard.limits.iter_mut().find(|l| l.name == name) else { return };
    l.finding = false;
    l.checked_at = Instant::now();
    l.searched_code |= search_code && found.is_some();
    let object = found.and_then(|(r, disp)| {
        let (object, g) = guard_of(mem, r.addr.wrapping_sub(disp as u64))?;
        Some((r, object, g))
    });
    match object {
        Some((r, object, g)) => {
            l.recheck_every = match r.addr == l.addr && l.paused.is_none() {
                true => (l.recheck_every * 2).min(RECHECK_MOST),
                false => RECHECK_EVERY,
            };
            if r.addr != l.addr || l.paused.is_some() {
                l.restores += 1;
            }
            l.addr = r.addr;
            l.guard_addr = object;
            l.guard = g;
            l.paused = None;
            // Through the static pointer it is found on every check, without tracing.
            l.paths.extend(r.holder);
        }
        None if l.paused.is_some() => l.retry_at = Instant::now() + RETRY_EVERY,
        // The code didn't run meanwhile (a menu?): nothing says the value moved.
        None => {}
    }
}

fn bound(v: Option<&&str>) -> Result<Option<f64>, ()> {
    match v {
        Some(&"-") => Ok(None),
        Some(v) => v.parse().map(Some).map_err(drop),
        None => Err(()),
    }
}

/// limit <name> <hex addr[:type]> <min|-> <max|-> <site or path>... where a site is
/// "pattern:offset:register:displacement" as saved in the profile, a path
/// "module+offset,offset,..." ("+static,disp" from resolve), and a named path starts with a
/// quote (see names.rs; it keeps every place it leads to in range); `key=<offset>:<number>`
/// goes with the sites (see `Site.key`), `left=<addr>,...` with the paths (see `Limit.left`). No
/// bounds: only keeps the address current (see `limiter_loop`).
fn cmd_limit(out: &mut impl Write, limiter: &SharedLimiter, arg: &str) -> io::Result<()> {
    let (f, left): (Vec<&str>, Vec<&str>) = arg.split_whitespace().partition(|s| !s.starts_with("left="));
    let mut left: Vec<u64> = left.iter().flat_map(|s| s["left=".len()..].split(',')).filter_map(parse_addr).collect();
    let (key, rest) = take_key(f.get(4..).unwrap_or_default());
    let is_named = |s: &&&str| Named::is_named(s);
    let named: Option<Vec<Named>> = rest.iter().filter(is_named).map(|s| Named::parse(s)).collect();
    let rest: Vec<&str> = rest.iter().filter(|s| !is_named(s)).copied().collect();
    let paths: Option<Vec<PtrPath>> = rest.iter().filter(|s| s.contains(',')).map(|s| PtrPath::parse(s)).collect();
    let sites: Option<Vec<Site>> = rest
        .iter()
        .filter(|s| !s.contains(','))
        .map(|s| Site::parse(&s.split(':').collect::<Vec<_>>()).map(|s| Site { key, ..s }))
        .collect();
    let Some(mut named) = named else {
        return writeln!(out, "error: not a named path");
    };
    let (Some(name), Some(addr), Ok(min), Ok(max), Some(sites), Some(paths)) =
        (f.first(), f.get(1).and_then(|a| parse_loc(a)), bound(f.get(2)), bound(f.get(3)), sites, paths)
    else {
        return writeln!(out, "error: usage: limit <name> <hex addr[:type]> <min|-> <max|-> <pattern:offset:register:displacement | module+offset,offset,...>...");
    };
    if sites.is_empty() && paths.is_empty() && named.is_empty() {
        return writeln!(out, "error: a limit needs at least one saved code pattern, pointer path or named path");
    }
    let mut l = limiter.lock().unwrap();
    let Some(mem) = l.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let (addr, kind) = addr;
    let object = addr.wrapping_sub(sites.first().map_or(0, |s| s.disp) as u64);
    // Address 0: not found yet (the game hasn't run its code); found in the background like a
    // value the game moved, instead of holding up the restore.
    let waiting = addr == 0 && !sites.is_empty();
    // Paths are followed on every check, so they need no guard (and the value may not exist yet).
    let (guard_addr, guard) = match guard_of(mem, object).filter(|_| !waiting) {
        Some(g) => g,
        None if waiting || !paths.is_empty() || !named.is_empty() => (object, [0; 4]),
        None => return writeln!(out, "error: cannot read the object at 0x{object:x}"),
    };
    // Found again or given new bounds: a limit the player turned off stays off.
    let off = l.limits.iter().any(|l| l.name == *name && l.off);
    for a in l.limits.iter().filter(|l| l.name == *name).flat_map(|l| l.left.clone()) {
        if a != addr && !left.contains(&a) {
            left.push(a);
        }
    }
    l.limits.retain(|l| l.name != *name);
    let changed = paths.is_empty();
    l.limits.push(Limit {
        name: name.to_string(),
        addr,
        kind,
        min,
        max,
        sites,
        paths,
        left,
        came: Vec::new(),
        named: named.pop().map(|path| NamedLimit { path, roots: Vec::new(), found_at: None, addrs: Vec::new(), searching: false }),
        guard_addr,
        guard,
        fixes: 0,
        restores: 0,
        changed,
        unchanged: None,
        paused: waiting.then_some("not found yet (the game hasn't run the code that uses it), waiting"),
        off,
        retry_at: Instant::now(),
        checked_at: Instant::now(),
        searched_code: false,
        finding: false,
        recheck_every: RECHECK_EVERY,
    });
    writeln!(out, "limiting {name} at 0x{addr:x}")
}

fn cmd_unlimit(out: &mut impl Write, limiter: &SharedLimiter, name: &str) -> io::Result<()> {
    limiter.lock().unwrap().limits.retain(|l| l.name != name);
    writeln!(out, "no longer limiting {name}")
}

/// switch <name|*> <on|off|toggle>: turns limits off (followed, never written) or on again.
/// `*`: every limit with bounds; toggle turns them all off when any is on. One line per limit
/// switched: <name> on|off.
fn cmd_switch(out: &mut impl Write, limiter: &SharedLimiter, arg: &str) -> io::Result<()> {
    let (Some(name), Some(how)) = (arg.split_whitespace().next(), arg.split_whitespace().nth(1)) else {
        return writeln!(out, "error: usage: switch <name|*> <on|off|toggle>");
    };
    let mut l = limiter.lock().unwrap();
    let mut chosen: Vec<&mut Limit> =
        l.limits.iter_mut().filter(|l| if name == "*" { l.min.is_some() || l.max.is_some() } else { l.name == name }).collect();
    if chosen.is_empty() {
        return writeln!(out, "error: no limit called {name}");
    }
    let off = match how {
        "on" => false,
        "off" => true,
        "toggle" => chosen.iter().any(|l| !l.off),
        _ => return writeln!(out, "error: usage: switch <name|*> <on|off|toggle>"),
    };
    for l in chosen.iter_mut() {
        l.off = off;
        writeln!(out, "{} {}", l.name, if off { "off" } else { "on" })?;
    }
    Ok(())
}

/// One line per limit: <name> <hex addr> <min|-> <max|-> fixed N times, restored M times, <state>
fn cmd_limits(out: &mut impl Write, limiter: &SharedLimiter) -> io::Result<()> {
    let show = |b: Option<f64>| b.map_or("-".to_owned(), |b| b.to_string());
    for l in &limiter.lock().unwrap().limits {
        writeln!(
            out,
            "{} 0x{:012x} {} {} fixed {} times, restored {} times, {}",
            l.name,
            l.addr,
            show(l.min),
            show(l.max),
            l.fixes,
            l.restores,
            match (l.off, l.paused) {
                (true, _) => "turned off",
                (false, Some(why)) => why,
                (false, None) if l.changed => "active",
                (false, None) => "waiting for the game to change it once before writing",
            }
        )?;
    }
    Ok(())
}

/// A scan can keep millions of these: address, type and the flags (`MIRROR`, `MISSED`,
/// `MOVED`, `MOVES`) share one word (addresses fit in 56 bits), next to the value at the last scan,
/// mark or next.
#[derive(Clone, Copy)]
struct Candidate {
    tagged: u64,
    value: f64,
}

/// `phase`: the place moved down in this moving phase (else up).
const MIRROR: u64 = 1 << 60;
/// `phase`: the place broke the rule once in this phase (forgiven once: a press a little late
/// or early puts the game's jump, a power starting or ending, inside the phase).
const MISSED: u64 = 1 << 59;
/// `phase`: the place moved in this moving phase; `MIRROR` then says which way.
const MOVED: u64 = 1 << 58;
/// `phase`: how many times it moved in this moving phase, up to 3 (two bits).
const MOVES: u64 = 3 << 56;

impl Candidate {
    fn new(addr: u64, kind: Kind, value: f64) -> Self {
        Candidate { tagged: addr | (kind as u64) << 61, value }
    }

    fn addr(&self) -> u64 {
        self.tagged & ((1 << 56) - 1)
    }

    fn moves(&self) -> u64 {
        (self.tagged & MOVES) >> 56
    }

    fn moved_once_more(self) -> Self {
        Candidate { tagged: (self.tagged & !MOVES) | ((self.moves() + 1).min(3) << 56), ..self }
    }

    fn has(&self, flag: u64) -> bool {
        self.tagged & flag != 0
    }

    fn with(self, flag: u64, on: bool) -> Self {
        Candidate { tagged: if on { self.tagged | flag } else { self.tagged & !flag }, ..self }
    }

    fn mirror(&self) -> bool {
        self.has(MIRROR)
    }

    fn kind(&self) -> Kind {
        Kind::ALL[(self.tagged >> 61) as usize]
    }
}

/// Current values of candidates sorted by address, reading memory a block at a time.
fn read_all(mem: &Mem, cands: &[Candidate]) -> Vec<Option<f64>> {
    const BLOCK: u64 = 1 << 16;
    let mut buf = vec![0u8; BLOCK as usize + 8];
    let (mut start, mut len) = (u64::MAX, 0u64);
    cands
        .iter()
        .map(|c| {
            let (addr, kind) = (c.addr(), c.kind());
            let size = kind.size() as u64;
            if addr < start || addr + size > start + len {
                start = addr & !(BLOCK - 1);
                len = mem.read_at(&mut buf, start).unwrap_or(0) as u64;
                if addr + size > start + len {
                    return read_value(mem, addr, kind);
                }
            }
            let off = (addr - start) as usize;
            Some(kind.decode(&buf[off..off + size as usize]))
        })
        .collect()
}

#[derive(Default)]
struct Session {
    pid: u32,
    mem: Option<Mem>,
    exe: String,
    /// Pointer size in the game: 4 or 8 bytes.
    width: usize,
    /// The game stores numbers only as doubles: scans look for nothing else.
    doubles_only: bool,
    /// A Java game: code patterns are refused (see `is_java`).
    java: bool,
    candidates: Vec<Candidate>,
    /// The matches before each step of the search (newest last) and the command that took it,
    /// for `undo`.
    history: Vec<(String, Vec<Candidate>)>,
    /// Older steps were let go (too many, or too many matches to keep).
    history_cut: bool,
    /// Steps taken back by `undo` (the last one undone last), for `redo`; a new step forgets them.
    undone: Vec<(String, Vec<Candidate>)>,
    /// Every pointer in the game's memory, from a `names` that found nothing, for the pointer
    /// scan that comes next (collecting them takes seconds in big games).
    pointer_map: Option<(Instant, (Pointers, u64))>,
    /// Objects found by name for each named path, and when they were looked for.
    named_roots: HashMap<String, (Vec<u64>, Option<Instant>)>,
    /// What `about` said about places (an Unreal place takes a pass over every object).
    abouts: HashMap<u64, Option<String>>,
    /// The memory around values found earlier in this game: scans keep the matches in places
    /// shaped like one of them, when there are any.
    shapes: Vec<Shape>,
    /// Test values the frontend hasn't put back yet (place, type, value before, test value):
    /// put back when it goes away mid-test (`put_back_tests`).
    tests: Vec<(u64, Kind, f64, f64)>,
    /// A copy of the game's memory for a search with no number to look for (a bar): the first
    /// `next bar` compares memory with it.
    snap: Option<Snap>,
    /// The matches' values right before the screen was read (`mark bar`), for the next `next
    /// bar`: a value that changes again while the screen is read fits the bar then, not later
    /// (Isaac: a heart picked up and a hit right after; the real value was dropped and only a
    /// copy the game redraws the hearts from, a frame behind, was left).
    bar_marks: Option<Vec<f64>>,
    /// `phase`'s phase now: Some(true) moving, Some(false) still, None before its first step;
    /// and how many steps it took so far.
    phase_now: Option<bool>,
    phase_steps: usize,
    /// When `phase` last read the matches, and how fast each moved then (a second; NaN: it
    /// didn't), in their order; another step that changes the matches leaves it unmatched.
    phase_rates: Option<(Instant, Vec<f32>)>,
}

/// How far back `undo` goes, and how many matches it keeps in all (16 bytes each).
const UNDO_STEPS: usize = 32;
const UNDO_KEEP: usize = 16 << 20;

impl Session {
    /// A search step's new matches; the ones before stay for `undo` unless they're the same
    /// places (only their values moved on).
    fn replace(&mut self, step: &str, new: Vec<Candidate>) {
        let same = new.len() == self.candidates.len() && new.iter().zip(&self.candidates).all(|(a, b)| a.tagged == b.tagged);
        let old = std::mem::replace(&mut self.candidates, new);
        self.snap = None;
        self.bar_marks = None;
        if same {
            return;
        }
        self.undone.clear();
        if old.len() > UNDO_KEEP {
            self.history.clear();
            self.history_cut = true;
            return;
        }
        self.history.push((step.to_owned(), old));
        while self.history.len() > UNDO_STEPS || self.history.iter().map(|(_, c)| c.len()).sum::<usize>() > UNDO_KEEP {
            self.history.remove(0);
            self.history_cut = true;
        }
    }

    fn read(&self, addr: u64, kind: Kind) -> Option<f64> {
        read_value(self.mem.as_ref()?, addr, kind)
    }

    fn read_all(&self) -> Vec<Option<f64>> {
        self.mem.as_ref().map_or_else(|| vec![None; self.candidates.len()], |m| read_all(m, &self.candidates))
    }

    /// "0x<addr>:<type> = <value>"
    fn describe(&self, addr: u64, kind: Kind) -> String {
        let v = self.read(addr, kind).map_or("??".to_owned(), |v| kind.show(v));
        format!("0x{addr:012x}:{} = {v}", kind.name())
    }
}

fn cmd_ps(out: &mut impl Write, filter: &str) -> io::Result<()> {
    let filter = filter.to_ascii_lowercase();
    let running = on_computer();
    for pid in os::own_pids() {
        let cmd = cmdline(pid);
        let env = environ(pid);
        let app_id = env.get("SteamAppId").or_else(|| env.get("SteamGameId"));
        let is_game = app_id.is_some() || cmd.to_ascii_lowercase().contains(".exe") || draws_like_a_game(pid);
        let shown = if filter.is_empty() { is_game } else { cmd.to_ascii_lowercase().contains(&filter) };
        if !shown {
            continue;
        }
        let mut short: String = cmd.chars().take(90).collect();
        if cmd.chars().count() > 90 {
            short.push('…');
        }
        write!(out, "{pid:>7}  {short}")?;
        if let Some(id) = app_id {
            write!(out, "  [SteamAppId={id}]")?;
        }
        let v = verdict(pid, &running);
        if let Some(r) = v.refused.as_ref().map(Refusal::code).or(v.ships) {
            write!(out, "  [{r}]")?;
        }
        writeln!(out)?;
    }
    Ok(())
}

/// Processes that come with Wine, Proton or Steam rather than with a game.
const NOT_GAMES: &[&str] = &[
    "steam.exe", "services.exe", "winedevice.exe", "explorer.exe", "plugplay.exe", "rpcss.exe", "svchost.exe",
    "conhost.exe", "tabtip.exe", "start.exe", "wineboot.exe", "winemenubuilder.exe", "rundll32.exe",
    "steamwebhelper.exe", "mscorsvw.exe", "ngen.exe", "reaper", "pressure-vessel-wrap", "pv-adverb",
    "srt-bwrap", "bwrap", "wineserver", "sh",
    "bash", "steam", "gameoverlayui", "fossilize_replay", "timeout", "sleep", "umu.exe", "xalia.exe",
    "xwayland", "gamescope", "gamescope-wl", "gamescopereaper", "gpu-screen-recorder", "mpv", "ffplay",
    // Windows itself: launchers, browsers, consoles and Ferret (most of Windows' own programs
    // run from C:\Windows, `windows_non_game`).
    "epicgameslauncher.exe", "epicwebhelper.exe", "galaxyclient.exe", "galaxyclient helper.exe", "eadesktop.exe",
    "upc.exe", "battle.net.exe", "msedge.exe", "msedgewebview2.exe", "chrome.exe", "firefox.exe", "discord.exe",
    "obs64.exe", "windowsterminal.exe", "openconsole.exe", "powershell.exe", "pwsh.exe", "cmd.exe", "ferret.exe",
    "ferret-helper.exe",
];

/// Native games outside Steam: a GPU driver is loaded (the program draws with OpenGL or Vulkan;
/// the plain libGL/libEGL/libvulkan front ends also load in programs that never draw), or one
/// of the ways games draw without the GPU: SDL (pygame), Java's AWT (Java2D games), or any
/// connection to the display from a program outside the system's folders (old Adventure Game
/// Studio games draw through plain X11); and it isn't a desktop app (GTK, Qt, Chromium/Electron),
/// unless a library only games load is there too: Allegro's dialog add-on brings GTK 3 into
/// every game using it (Darkness Looming).
fn draws_like_a_game(pid: u32) -> bool {
    if cmdline(pid).contains(" --type=") {
        return false;
    }
    let Ok(regions) = maps(pid) else { return false };
    let (mut driver, mut display, mut toolkit, mut game) = (false, false, false, false);
    for r in &regions {
        let name = r.path.rsplit('/').next().unwrap_or_default();
        toolkit |= desktop_toolkit(name);
        game |= ["liballeg", "libsfml-graphics", "libraylib", "UnityPlayer.so"].iter().any(|l| name.starts_with(l));
        // Desktop apps load SDL for gamepads (Steam's web helper, QEMU, GTK 4 apps): with a
        // toolkit it says nothing.
        driver |= gpu_driver(name)
            || ["libSDL-1.2.", "libSDL2", "libSDL3", "libawt_xawt.", "libawt_wlawt."].iter().any(|l| name.starts_with(l));
        display |= ["libX11.", "libxcb.", "libwayland-client."].iter().any(|l| name.starts_with(l));
    }
    let system = || {
        let exe = os::exe(pid).unwrap_or_default();
        ["/usr/", "/bin/", "/sbin/", "/lib", "/app/", "/nix/", "/snap/"].iter().any(|d| exe.to_string_lossy().starts_with(d))
    };
    (driver || game || display && !system()) && (game || !toolkit)
}

/// A GPU driver's library: loaded once a program draws with the GPU.
fn gpu_driver(name: &str) -> bool {
    ["libGLX_", "libEGL_", "libvulkan_", "libnvidia-glcore.", "libnvidia-eglcore.", "libgallium", "amdvlk"]
        .iter()
        .any(|d| name.starts_with(d))
        || name.ends_with("_dri.so")
        // On Windows itself: Direct3D, OpenGL or Vulkan loaded (only telling a launch's game from
        // its helpers apart: most Windows apps load Direct3D).
        || cfg!(windows) && ["d3d8.dll", "d3d9.dll", "d3d11.dll", "d3d12.dll", "opengl32.dll", "vulkan-1.dll"].contains(&name.to_ascii_lowercase().as_str())
}

/// A launcher's install: its folder or the one above ships Qt or Chromium Embedded DLLs (seen
/// through the process's root, for sandboxed launchers).
fn launcher_folder(pid: u32, program: &Path) -> bool {
    let ships_toolkit = |dir: &Path| {
        fs::read_dir(os::inside(pid, dir)).is_ok_and(|entries| {
            entries.flatten().any(|e| e.file_name().to_str().is_some_and(|n| n.to_ascii_lowercase().ends_with(".dll") && desktop_toolkit(n)))
        })
    };
    program.ancestors().skip(1).take(2).any(ships_toolkit)
}

/// A library only desktop apps load: GTK, Qt, Chromium Embedded, WinUI (Linux names and Windows
/// DLLs: GTK's are libgtk-4-1.dll there). GDK 3 alone too: GNOME's ibus-x11 loads it and Mesa
/// without GTK, and showed up as a game; so did Windows 11's Notepad (WinUI).
fn desktop_toolkit(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "libgtk-3.", "libgdk-3.", "libgtk-4.", "libgtk-3-", "libgtk-4-", "libqt5core.", "libqt6core.", "libcef.", "qt5core.dll", "qt6core.dll",
        "chrome_elf.dll", "msedge_elf.dll", "microsoft.ui.xaml.dll",
    ]
    .iter()
    .any(|t| name.starts_with(t))
}

/// An Electron app (Discord, VS Code, the Claude app): Chromium is built into its own .exe, so
/// no toolkit DLL tells it (the Claude app showed up as a game on Windows), but Chromium's V8
/// snapshot sits beside it and the app in resources\app.asar (or resources\app). An Electron
/// game's values are in its renderers, which aren't listed either (`--type=`).
fn electron_app(pid: u32, program: &Path) -> bool {
    let Some(dir) = program.parent() else { return false };
    let has = |p: &str| os::inside(pid, &dir.join(p)).exists();
    has("v8_context_snapshot.bin") && (has("resources/app.asar") || has("resources/app"))
}

/// Windows programs under Wine that aren't games: Wine's own (csrss.exe, lsass.exe, ... from
/// its lib/wine folder), Chromium helpers, and launchers' windows and services (the EA app's
/// use Chromium Embedded and Qt; it showed up as ten games), also their helpers that load
/// neither and don't draw (the EA app starts compatibility32\EADesktop.exe next to every game),
/// and Electron apps.
fn windows_non_game(pid: u32) -> bool {
    if cmdline(pid).contains(" --type=") {
        return true;
    }
    // On Windows itself a game has a window, and doesn't come with Windows.
    #[cfg(windows)]
    if !os::has_window(pid) || os::exe(pid).is_some_and(|p| p.to_string_lossy().to_ascii_lowercase().starts_with("c:/windows/")) {
        return true;
    }
    let Ok(regions) = maps(pid) else { return true };
    let names = || regions.iter().map(|r| r.path.rsplit('/').next().unwrap_or_default());
    if names().any(desktop_toolkit) {
        return true;
    }
    let Some(program) = program_path(pid, &regions) else { return false };
    let lower = program.to_string_lossy().to_ascii_lowercase();
    ["/lib/wine/", "/lib64/wine/", "/drive_c/windows/"].iter().any(|d| lower.contains(d))
        || !names().any(gpu_driver) && launcher_folder(pid, &program)
        || electron_app(pid, &program)
}

/// Running games, one per line: pid, program name, Steam app ID, why it's refused (`Refusal::code`),
/// the name its launcher gives it, "online" (online only) or "multiplayer", anti-cheat it ships
/// but isn't running, and what else is online about it, comma-separated: "marks" (accounts with
/// edited saves get marked, `launchers::MARKS_ACCOUNTS`), "leaderboards", and anti-cheat that is
/// always on on the computer (Windows: Riot Vanguard's driver; tab-separated, "-" when unknown
/// or not).
fn cmd_games(out: &mut impl Write) -> io::Result<()> {
    let running = on_computer();
    let mut rows = Vec::new();
    for pid in os::own_pids() {
        let exe = exe_name(pid);
        let lower = exe.to_ascii_lowercase();
        let name_now = game_name(pid);
        let env = environ(pid);
        let app_id = steam_id(&env);
        let windows = lower.ends_with(".exe");
        // Steam's runtime starts a launcher service next to Proton games, with the game's app ID
        // (steam-runtime-launcher-service, now <arch>-srt-launcher-service).
        let not_game = NOT_GAMES.contains(&lower.as_str()) || lower.ends_with("-launcher-service");
        // Anti-cheat's own programs (EasyAntiCheat_EOS.exe, BEService.exe) refuse the game they
        // run beside; they aren't games themselves.
        let anti_cheat = anticheat::named(&exe).is_some();
        if not_game || anti_cheat || lower.contains("crashhandler") || lower.contains("crashreport") {
            continue;
        }
        // An NW.js game: only its renderer, where its values are.
        let nwjs = nwjs_app(pid).is_some();
        if nwjs && !nwjs_renderer(pid) {
            continue;
        }
        if !nwjs && (!(windows || app_id.is_some() || draws_like_a_game(pid)) || windows && windows_non_game(pid)) {
            continue;
        }
        // Python also runs Steam's and umu's tools (with the game's app ID): only a Python that
        // draws is a game (pygame), and never Proton's launcher (it draws through Steam's overlay).
        if is_python(&lower) && (!draws_like_a_game(pid) || wine_launcher(&args(pid))) {
            continue;
        }
        let dash = |s: Option<&str>| s.filter(|s| !s.is_empty()).unwrap_or("-").to_owned();
        let about = about(&env);
        let name = about.name.as_ref().map(|n| n.replace('\t', " "));
        let play = if about.online_only { Some("online") } else { about.multiplayer.then_some("multiplayer") };
        let id = dash(app_id.map(String::as_str));
        let v = verdict(pid, &running);
        let refused = dash(v.refused.as_ref().map(Refusal::code).as_deref());
        let ships = dash(v.ships.as_deref());
        let extras: Vec<&str> = [(about.marks_accounts, "marks"), (about.leaderboards, "leaderboards")]
            .into_iter()
            .filter_map(|(on, word)| on.then_some(word))
            .collect();
        let extras = dash(Some(extras.join(",").as_str()));
        let regions = maps(pid).unwrap_or_default();
        let program = program_path(pid, &regions);
        let name = dash(name.as_deref());
        let draws = !nwjs && (regions.iter().any(|r| gpu_driver(r.path.rsplit('/').next().unwrap_or_default())) || !windows && draws_like_a_game(pid));
        let size: u64 = regions.iter().filter(|r| scannable(r)).map(|r| r.end - r.start).sum();
        let resident = dash(running.resident.filter(|_| v.refused.is_none()));
        let row = format!("{pid}\t{name_now}\t{id}\t{refused}\t{name}\t{}\t{ships}\t{extras}\t{resident}", dash(play));
        // An NW.js game's renderers (NW.js 0.12 forks a second one from its zygote): the one
        // holding the most memory, as for a launch.
        let launch = match nwjs.then(|| nwjs_app(pid)).flatten() {
            Some(dir) => format!("nwjs {}", dir.display()),
            None => launch_of(pid, &env),
        };
        rows.push(Listed { program, launch, draws, size, row });
    }
    // One game per launch: everything a launcher starts for a game carries its ID (Steam gives
    // its app ID to every process of a launch: Bazzite's tail and grep watching Ubisoft
    // Connect's log beside From Dust were listed as the game, and so was Ubisoft's osi.exe).
    // The ones that draw with the GPU are the game; when none does yet (or an old game draws
    // without it), the one using the most memory.
    let keep = |c: &Listed| {
        let launch: Vec<&Listed> = rows.iter().filter(|o| o.launch == c.launch).collect();
        match launch.iter().any(|o| o.draws) {
            true => c.draws,
            false => launch.iter().all(|o| o.size <= c.size),
        }
    };
    // An Unreal stub holds none of the game's values (Astro Colony: two entries, a search in
    // the stub found nothing).
    let stub_folders: Vec<&Path> = rows.iter().filter_map(|c| unreal_stub_folder(c.program.as_deref()?)).collect();
    for c in rows.iter().filter(|c| keep(c)) {
        let stub = c.program.as_deref().is_some_and(|p| p.parent().is_some_and(|dir| stub_folders.contains(&dir)));
        if !stub {
            writeln!(out, "{}", c.row)?;
        }
    }
    Ok(())
}

/// A process in the games list before it's picked out of its launch.
struct Listed {
    program: Option<PathBuf>,
    /// Which launch it belongs to (`launch_of`).
    launch: String,
    /// It draws with the GPU (or, outside Wine, the ways games draw without it).
    draws: bool,
    /// Its writable memory, in bytes.
    size: u64,
    row: String,
}

/// Which launch a process belongs to: what Steam, Heroic or Lutris start for a game carries
/// the game's ID, and a Wine game's programs share its prefix; anything else is its own.
fn launch_of(pid: u32, env: &HashMap<String, String>) -> String {
    if let Some(id) = steam_id(env) {
        return format!("steam {id}");
    }
    let given = |k: &str| env.get(k).map(|v| v.trim_end_matches('/')).filter(|v| !v.is_empty());
    if let Some(v) = given("HEROIC_APP_NAME").or_else(|| given("GAME_NAME")) {
        return format!("game {v}");
    }
    match given("WINEPREFIX").or_else(|| given("STEAM_COMPAT_DATA_PATH")) {
        Some(prefix) => format!("prefix {}", prefix.trim_end_matches("/pfx")),
        None => format!("pid {pid}"),
    }
}

fn cmd_attach(out: &mut impl Write, s: &mut Session, limiter: &SharedLimiter, arg: &str) -> io::Result<()> {
    let Ok(pid) = arg.parse::<u32>() else {
        return writeln!(out, "error: usage: attach <pid>");
    };
    let running = on_computer();
    if let Some(r) = verdict(pid, &running).refused {
        return writeln!(out, "error: refusing to attach, {}", r.why());
    }
    match os::open_mem(pid) {
        Ok(f) => {
            let exe = exe_name(pid);
            let modules = pointers::modules(pid, &f);
            let width = pointers::pointer_width(pid, &f, &modules, &exe);
            put_back_tests(s);
            *limiter.lock().unwrap() = Limiter { pid, mem: f.try_clone().ok(), width, modules, limits: Vec::new() };
            let doubles_only = is_gamemaker(pid, &exe);
            let java = is_java(pid);
            *s = Session { pid, mem: Some(f), exe: exe.clone(), width, doubles_only, java, ..Session::default() };
            let regions = maps(pid)?;
            let rw: u64 = regions.iter().filter(|r| scannable(r)).map(|r| r.end - r.start).sum();
            // Only the program: launchers pass login tokens as arguments (Heroic's Epic games get
            // -AUTH_PASSWORD=<code>), and the log is kept.
            writeln!(out, "attached to {pid}: {}", argv0(pid))?;
            match trace::clear_left(pid) {
                0 => {}
                n => writeln!(out, "took off breakpoints a stopped helper left on {n} of the game's threads")?,
            }
            writeln!(out, "exe: {}", game_name(pid))?;
            writeln!(out, "{} mappings, {} MiB writable, {}-bit", regions.len(), rw >> 20, width * 8)?;
            if doubles_only {
                writeln!(out, "GameMaker game: it keeps numbers as doubles, searching only those")?;
            }
            if java {
                writeln!(out, "{JAVA}")?;
            }
            if let Some(name) = scrambler(pid, &regions) {
                writeln!(out, "memory scrambler: {name}")?;
            }
            if let Some(ac) = running.resident {
                writeln!(out, "resident anti-cheat: {ac}")?;
            }
            Ok(())
        }
        Err(e) => writeln!(out, "error: {e}"),
    }
}

/// scan <n> [types]: every value that can be what the screen shows as n, stored as a 4-byte
/// integer (plain or XOR-encoded) or as a float/double (fractions are cut off or rounded on
/// screen). `types` (comma-separated, e.g. `i32,xor`) narrows it to the ones the player picked.
fn cmd_scan(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let mut words = arg.split_whitespace();
    let n = words.next().and_then(Shown::parse);
    let mut picked = Some(Vec::new());
    let mut use_shapes = !s.shapes.is_empty();
    for w in words {
        match w {
            "all" => use_shapes = false,
            w => picked = w.split(',').map(Kind::parse).collect(),
        }
    }
    let (Some(n), Some(picked)) = (n, picked) else {
        return writeln!(out, "error: usage: scan <number, e.g. 1250 or 1.5> [i32,f32,f64,xor,u16] [all]");
    };
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let t = Instant::now();
    let kinds: Vec<Kind> = if picked.is_empty() {
        Kind::ALL.into_iter().filter(|k| !s.doubles_only || *k == Kind::F64).collect()
    } else {
        picked
    };
    let regions = maps(s.pid)?;
    let mapped = Mapped::new(&regions);
    let shapes: &[Shape] = if use_shapes { &s.shapes } else { &[] };
    let into = MemScan { mem, regions: &regions, mapped: &mapped, width: s.width, n, shapes };
    // Places shaped like an earlier find, looking only at the types those had (with one type, a
    // third of the work of all four); everything when none of them holds the number. The
    // caller searches everywhere ("all") when the value isn't among them after all.
    let shape_kinds: Vec<Kind> = kinds.iter().copied().filter(|k| shapes.iter().any(|sh| sh.kind == *k)).collect();
    let mut scan = None;
    if !shape_kinds.is_empty() && shape_kinds.len() < kinds.len() {
        scan = Some(into.run(&shape_kinds, out)).filter(|r| !r.shaped.is_empty());
    }
    let r = match scan {
        Some(r) => r,
        None => into.run(&kinds, out),
    };
    if r.over {
        // The search so far is kept: nothing replaced it.
        return writeln!(out, "error: too many matches (over {MAX_MATCHES})");
    }
    let all = r.found.len();
    let step = format!("scan {arg}");
    let within = if r.shaped.is_empty() {
        s.replace(&step, r.found);
        String::new()
    } else {
        s.replace(&step, r.shaped);
        format!(" in places shaped like earlier finds (of {all})")
    };
    writeln!(
        out,
        "{} matches ({}){within} in {} MiB ({} MiB unreadable) in {} ms",
        s.candidates.len(),
        kinds_text(&s.candidates),
        r.bytes >> 20,
        r.unreadable >> 20,
        t.elapsed().as_millis()
    )
}

/// One pass of `scan` over the game's memory.
struct MemScan<'a> {
    mem: &'a Mem,
    regions: &'a [Region],
    mapped: &'a Mapped,
    width: usize,
    n: Shown,
    shapes: &'a [Shape],
}

struct MemScanned {
    found: Vec<Candidate>,
    /// The ones in places shaped like an earlier find.
    shaped: Vec<Candidate>,
    bytes: u64,
    unreadable: u64,
    /// More than `MAX_MATCHES`: given up (0 holds hundreds of millions of places, gigabytes to keep).
    over: bool,
}

/// The most matches a scan keeps (16 bytes each).
const MAX_MATCHES: usize = 16 << 20;

impl MemScan<'_> {
    /// Writes "progress <bytes done> <bytes in all>" lines as it goes (at most ten a second), so
    /// the interface can show how far the scan is. Chunks are checked on several threads (the
    /// checks, not the reading, took most of a scan: 5.7 s for 1.2 GB on one); the matches come
    /// out in address order all the same.
    fn run(&self, kinds: &[Kind], out: &mut impl Write) -> MemScanned {
        const CHUNK: usize = 4 << 20;
        let chunks: Vec<(u64, usize)> = self
            .regions
            .iter()
            .filter(|r| scannable(r))
            .flat_map(|r| (r.start..r.end).step_by(CHUNK).map(move |a| (a, ((r.end - a) as usize).min(CHUNK))))
            .collect();
        let seen = AtomicUsize::new(0);
        let parts = parallel(chunks.len(), |i| chunks[i].1 as u64, CHUNK, out, |i, buf| {
            let (addr, len) = chunks[i];
            if seen.load(Ordering::Relaxed) > MAX_MATCHES {
                return MemScanned { found: Vec::new(), shaped: Vec::new(), bytes: 0, unreadable: 0, over: true };
            }
            let r = self.chunk(kinds, &mut buf[..len], addr);
            seen.fetch_add(r.found.len(), Ordering::Relaxed);
            r
        });
        let mut r = MemScanned { found: Vec::new(), shaped: Vec::new(), bytes: 0, unreadable: 0, over: false };
        if seen.load(Ordering::Relaxed) > MAX_MATCHES {
            r.over = true;
            return r;
        }
        for p in parts {
            r.found.extend(p.found);
            r.shaped.extend(p.shaped);
            r.bytes += p.bytes;
            r.unreadable += p.unreadable;
        }
        r
    }

    /// The matches in one chunk of memory, read into `buf`. Pages that can't be read are
    /// stepped over: glibc 2.43 puts thread stacks' guard pages inside their mappings
    /// (MADV_GUARD_INSTALL), and a read stopping at one used to lose the rest of the chunk (on
    /// 32-bit games about 100 MiB of a scan).
    fn chunk(&self, kinds: &[Kind], buf: &mut [u8], addr: u64) -> MemScanned {
        let mut r = MemScanned { found: Vec::new(), shaped: Vec::new(), bytes: 0, unreadable: 0, over: false };
        let mut at = 0;
        while at < buf.len() {
            let here = addr + at as u64;
            match self.mem.read_at(&mut buf[at..], here) {
                Ok(len) if len > 0 => {
                    r.bytes += len as u64;
                    self.matches_in(kinds, &buf[at..at + len], here, &mut r);
                    at += len;
                }
                _ => {
                    let next = ((((here | 0xfff) + 1) - addr) as usize).min(buf.len());
                    r.unreadable += (next - at) as u64;
                    at = next;
                }
            }
        }
        r
    }

    /// The matches in `buf`, read from `addr`.
    fn matches_in(&self, kinds: &[Kind], buf: &[u8], addr: u64, r: &mut MemScanned) {
        let (n, len) = (self.n, buf.len());
        let with_i32 = kinds.contains(&Kind::I32);
        // The 2-byte number's bytes (none: the number can't be one).
        let short = (n.scaled.fract() == 0.0 && (0.0..65536.0).contains(&n.scaled)).then(|| (n.scaled as u16).to_le_bytes());
        for off in (0..len.saturating_sub(1)).step_by(4) {
            // Doubles are 8-aligned, 2-byte numbers 2-aligned, the rest 4-aligned.
            // Integers and pointers read as floats come out as zero or next to nothing
            // (1e-40, 1e-300), never a number on screen.
            for &kind in kinds {
                if kind == Kind::U16 {
                    let Some(short) = short else { continue };
                    for o in [off, off + 2] {
                        if buf.get(o..o + 2) != Some(&short[..]) {
                            continue;
                        }
                        // Followed by two zero bytes it's the 4-byte one's match (`next`
                        // turns that into a 2-byte one if the bytes after it change).
                        if with_i32 && o == off && buf.get(off + 2..off + 4) == Some(&[0, 0]) {
                            continue;
                        }
                        self.hit(r, buf, len, addr, o, kind, n.scaled);
                    }
                    continue;
                }
                let b = &buf[off..(off + kind.size()).min(len)];
                if b.len() < kind.size() || (kind == Kind::F64 && off % 8 != 0) {
                    continue;
                }
                // A real key is never 0; skipping those also leaves plain integers
                // next to a zero to the i32 check.
                if kind == Kind::Xor && (b[..4] == [0; 4] || b[4..] == [0; 4]) {
                    continue;
                }
                let value = kind.decode(b);
                if matches!(kind, Kind::F32 | Kind::F64) && !(value.abs() >= 1e-3) {
                    continue;
                }
                // The scan runs after the screen was read: allow for a fraction
                // that has moved on by up to 1 since.
                let hit = match kind {
                    Kind::I32 | Kind::Xor | Kind::U16 | Kind::Smi => value == n.scaled,
                    Kind::F32 | Kind::F64 | Kind::Gm | Kind::Lua | Kind::Py => kind.fits(value - n.step, value + n.step, n),
                };
                if hit {
                    self.hit(r, buf, len, addr, off, kind, value);
                }
            }
        }
    }

    /// Keeps a match found in `buf` (read from `addr`, `len` bytes) at `off`.
    #[allow(clippy::too_many_arguments)]
    fn hit(&self, r: &mut MemScanned, buf: &[u8], len: usize, addr: u64, off: usize, kind: Kind, value: f64) {
        let at = addr + off as u64;
        let c = Candidate::new(at, kind, value);
        let around = |o: i64, n: usize| le_at(&buf[..len], off as i64 + o, n).or_else(|| read_le(self.mem, at.wrapping_add_signed(o), n));
        if self.shapes.iter().any(|sh| sh.kind == kind && sh.fits(around, self.mapped, self.width)) {
            r.shaped.push(c);
        }
        r.found.push(c);
    }
}

/// "12 i32, 3 f32, 0 f64"
fn kinds_text(c: &[Candidate]) -> String {
    let count = |k: Kind| c.iter().filter(|c| c.kind() == k).count();
    Kind::ALL.iter().map(|k| format!("{} {}", count(*k), k.name())).collect::<Vec<_>>().join(", ")
}

fn cmd_next(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    if let Some(bar) = arg.strip_prefix("bar ") {
        return cmd_next_bar(out, s, bar);
    }
    let moved = matches!(arg, "+" | "-" | "=" | "!");
    let keep: Box<dyn Fn(Kind, f64, f64) -> bool> = match arg {
        "+" => Box::new(|_, old, new| new > old),
        "-" => Box::new(|_, old, new| new < old),
        "=" => Box::new(|_, old, new| new == old),
        "!" => Box::new(|_, old, new| new != old),
        v => match Shown::parse(v.trim()) {
            Some(n) => Box::new(move |kind, old, new| kind.fits(old, new, n)),
            None => return writeln!(out, "error: usage: next <number, e.g. 1250 or 1.5>|+|-|=|!"),
        },
    };
    let before = s.candidates.len();
    let next: Vec<Candidate> = s
        .candidates
        .iter()
        .zip(s.read_all())
        .filter_map(|(c, new)| {
            let new = new?;
            if keep(c.kind(), c.value, new) {
                return Some(Candidate { value: new, ..*c });
            }
            // A 4-byte match whose upper two bytes changed may be a 2-byte number with a
            // neighbour that was 0 at the scan (Andromeda's loaded ammo next to the reserve).
            // Not for a direction: half of any junk went up or down.
            let low = |v: f64| (v as i64 & 0xFFFF) as f64;
            (!moved && c.kind() == Kind::I32 && keep(Kind::U16, low(c.value), low(new)))
                .then(|| Candidate::new(c.addr(), Kind::U16, low(new)))
        })
        .collect();
    // Nothing fitting is usually a misread (or something covering the number): keep the matches,
    // the caller decides whether to start over.
    if next.is_empty() {
        return writeln!(out, "{before} -> 0 matches (kept the {before} from before)");
    }
    s.replace(&format!("next {arg}"), next);
    writeln!(out, "{before} -> {} matches ({})", s.candidates.len(), kinds_text(&s.candidates))
}

/// A copy of the game's writable memory on disk (gigabytes: Cheat Engine keeps its "unknown
/// initial value" scans so too), for a search with no number to start from. Blocks that are
/// all zeros aren't kept: a value that was 0 can't be a bar's share of anything.
struct Snap {
    path: PathBuf,
    /// (address, offset in the file, length), sorted by address; `SNAP_BLOCK` bytes but a
    /// region's last one.
    blocks: Vec<(u64, u64, usize)>,
    kinds: Vec<Kind>,
}

const SNAP_BLOCK: usize = 64 << 10;

impl Drop for Snap {
    fn drop(&mut self) {
        fs::remove_file(&self.path).ok();
    }
}

/// Runs `work(i, buf)` for every job on up to 6 threads (half the cores, so the game keeps running
/// smoothly), each with its own buffer of `buf_len` bytes; `size(i)` is a job's share of the
/// "progress <done> <in all>" lines written as it goes. The results come back in job order.
fn parallel<T: Send>(
    jobs: usize,
    size: impl Fn(usize) -> u64 + Sync,
    buf_len: usize,
    out: &mut impl Write,
    work: impl Fn(usize, &mut [u8]) -> T + Sync,
) -> Vec<T> {
    let total: u64 = (0..jobs).map(&size).sum();
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get() / 2).clamp(1, 6);
    let (next, done) = (AtomicUsize::new(0), AtomicU64::new(0));
    let _ = writeln!(out, "progress 0 {total}");
    let mut parts: Vec<(usize, T)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut buf = vec![0u8; buf_len];
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= jobs {
                            return mine;
                        }
                        mine.push((i, work(i, &mut buf)));
                        done.fetch_add(size(i), Ordering::Relaxed);
                    }
                })
            })
            .collect();
        while !workers.iter().all(|w| w.is_finished()) {
            std::thread::sleep(Duration::from_millis(100));
            let _ = writeln!(out, "progress {} {total}", done.load(Ordering::Relaxed));
        }
        workers.into_iter().flat_map(|w| w.join().unwrap_or_default()).collect()
    });
    parts.sort_by_key(|p| p.0);
    parts.into_iter().map(|p| p.1).collect()
}

/// snap <file> [types]: copies the game's writable memory into the file, for a search with no
/// number to look for; the matches so far are let go. Types: i32, f32 and f64 unless given.
fn cmd_snap(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let (path, kinds) = match arg.rsplit_once(' ').map(|(p, k)| (p, k.split(',').map(Kind::parse).collect::<Option<Vec<_>>>())) {
        Some((p, Some(k))) => (p.trim(), k),
        _ => (arg, vec![Kind::I32, Kind::F32, Kind::F64]),
    };
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    if path.is_empty() {
        return writeln!(out, "error: usage: snap <file> [i32,f32,f64,xor,u16]");
    }
    let kinds: Vec<Kind> = kinds.into_iter().filter(|k| !s.doubles_only || *k == Kind::F64).collect();
    let t = Instant::now();
    s.snap = None;
    let file = File::create(path)?;
    let regions = maps(s.pid)?;
    let blocks: Vec<(u64, usize)> = regions
        .iter()
        .filter(|r| scannable(r))
        .flat_map(|r| (r.start..r.end).step_by(SNAP_BLOCK).map(move |a| (a, ((r.end - a) as usize).min(SNAP_BLOCK))))
        .collect();
    // 64 blocks (4 MB) per job.
    const PER_JOB: usize = 64;
    let jobs = blocks.len().div_ceil(PER_JOB);
    let (at, failed) = (AtomicU64::new(0), Mutex::new(None));
    let parts = parallel(
        jobs,
        |i| blocks[i * PER_JOB..((i + 1) * PER_JOB).min(blocks.len())].iter().map(|b| b.1 as u64).sum(),
        SNAP_BLOCK,
        out,
        |i, buf| {
            let (mut kept, mut zeros, mut unreadable) = (Vec::new(), 0u64, 0u64);
            for &(addr, len) in &blocks[i * PER_JOB..((i + 1) * PER_JOB).min(blocks.len())] {
                let buf = &mut buf[..len];
                if mem.read_exact_at(buf, addr).is_err() {
                    unreadable += len as u64;
                } else if buf.iter().all(|&b| b == 0) {
                    zeros += len as u64;
                } else {
                    let off = at.fetch_add(len as u64, Ordering::Relaxed);
                    if let Err(e) = file.write_all_at(buf, off) {
                        *failed.lock().unwrap() = Some(e);
                        break;
                    }
                    kept.push((addr, off, len));
                }
            }
            (kept, zeros, unreadable)
        },
    );
    let path = PathBuf::from(path);
    if let Some(e) = failed.into_inner().unwrap() {
        fs::remove_file(&path).ok();
        return writeln!(out, "error: can't keep a copy of the game's memory: {e}");
    }
    let (mut kept, mut zeros, mut unreadable) = (Vec::new(), 0, 0);
    for (k, z, u) in parts {
        kept.extend(k);
        zeros += z;
        unreadable += u;
    }
    s.replace("snap", Vec::new());
    let mib = kept.iter().map(|b| b.2).sum::<usize>() >> 20;
    s.snap = Some(Snap { path, blocks: kept, kinds });
    writeln!(
        out,
        "copied {mib} MiB ({} MiB zeros, {} MiB unreadable) in {} ms",
        zeros >> 20,
        unreadable >> 20,
        t.elapsed().as_millis()
    )
}

/// Whether a value that went from `v0` to `v1` can be a bar that went from `f0` to `f1` full
/// (shares of the whole, measured within `err` either way): the value is the bar's share of
/// some unknown most, so `f0 * v1 / v0` must land on `f1` for some `f0` in its error.
/// Bars show health, shields, ammo: no game's is in the billions, but junk that moved alike is
/// (Shattered Pixel Dungeon: 1208065520 passed every step and was taken for the health).
const BAR_MOST_VALUE: f64 = 1e7;

fn bar_fits(v0: f64, v1: f64, f0: f64, f1: f64, err: f64) -> bool {
    if !(v0 > 0.0 && v1 >= 0.0 && v0 <= BAR_MOST_VALUE && v1 <= BAR_MOST_VALUE) {
        return false;
    }
    let r = v1 / v0;
    (f0 - err).max(0.0) * r <= f1 + err && (f0 + err) * r >= f1 - err
}

/// The most matches the first bar step keeps (16 bytes each).
const BAR_MOST: usize = 64 << 20;

/// next bar <full before> <full now> <error>: keeps the places whose value moved as the bar did
/// (shares of 1: 1 -> 0.73), against their value at the last step or, first, the memory copy.
fn cmd_next_bar(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let f: Vec<f64> = arg.split_whitespace().filter_map(|w| w.parse().ok()).filter(|v: &f64| v.is_finite()).collect();
    let &[f0, f1, err] = f.as_slice() else {
        return writeln!(out, "error: usage: next bar <full before> <full now> <error> (shares of 1)");
    };
    if s.mem.is_none() {
        return writeln!(out, "error: not attached");
    }
    let before = s.candidates.len();
    if s.snap.is_none() && before == 0 {
        return writeln!(out, "error: no copy of the game's memory and no matches to compare with");
    }
    if s.snap.is_none() {
        let marks = s.bar_marks.take().filter(|m| m.len() == before);
        // Kept with the value that fits the bar now: right before the screen was read, if
        // only then.
        let next: Vec<Candidate> = s
            .candidates
            .iter()
            .zip(s.read_all())
            .enumerate()
            .filter_map(|(i, (c, new))| {
                let marked = marks.as_ref().map(|m| m[i]);
                [new, marked].into_iter().flatten().find(|&v| bar_fits(c.value, v, f0, f1, err)).map(|v| Candidate { value: v, ..*c })
            })
            .collect();
        if next.is_empty() {
            return writeln!(out, "{before} -> 0 matches (kept the {before} from before)");
        }
        s.replace(&format!("next bar {arg}"), next);
        return writeln!(out, "{before} -> {} matches ({})", s.candidates.len(), kinds_text(&s.candidates));
    }
    // A bar that hasn't moved keeps everything that stayed the same: wait for it to move.
    if bar_fits(1.0, 1.0, f0, f1, err) {
        return writeln!(out, "0 matches (the bar hasn't moved enough yet, kept the copy)");
    }
    // Integers and pointers read as floats come out next to nothing.
    let fits = |kind: Kind, v0: f64, v1: f64| ((!matches!(kind, Kind::F32 | Kind::F64) || v0.abs() >= 1e-3) && bar_fits(v0, v1, f0, f1, err)).then_some(false);
    snap_step(out, s, &format!("next bar {arg}"), &fits, "too many places changed like the bar, kept the copy: let it move more first")
}

/// The first step after a copy of memory (`snap`): keeps the places whose value went from the
/// copy's to now as `fits` says (kind, then, now: None, or Some(went down)), then lets the copy
/// go. `too_many`: the error past `BAR_MOST` matches (the copy stays).
fn snap_step(out: &mut impl Write, s: &mut Session, step: &str, fits: &(dyn Fn(Kind, f64, f64) -> Option<bool> + Sync), too_many: &str) -> io::Result<()> {
    let (Some(mem), Some(snap)) = (s.mem.as_ref(), s.snap.as_ref()) else {
        return writeln!(out, "error: no copy of the game's memory to compare with");
    };
    let t = Instant::now();
    let file = File::open(&snap.path)?;
    const PER_JOB: usize = 64;
    let jobs = snap.blocks.len().div_ceil(PER_JOB);
    let count = AtomicUsize::new(0);
    let parts = parallel(
        jobs,
        |i| snap.blocks[i * PER_JOB..((i + 1) * PER_JOB).min(snap.blocks.len())].iter().map(|b| b.2 as u64).sum(),
        2 * SNAP_BLOCK,
        out,
        |i, buf| {
            let (old_buf, new_buf) = buf.split_at_mut(SNAP_BLOCK);
            let mut found = Vec::new();
            for &(addr, off, len) in &snap.blocks[i * PER_JOB..((i + 1) * PER_JOB).min(snap.blocks.len())] {
                let (old, new) = (&mut old_buf[..len], &mut new_buf[..len]);
                if count.load(Ordering::Relaxed) > BAR_MOST {
                    break;
                }
                if file.read_exact_at(old, off).is_err() || mem.read_exact_at(new, addr).is_err() {
                    continue;
                }
                // Most memory stays the same: skip unchanged 8-byte words at once.
                for w in (0..len & !7).step_by(8) {
                    if old[w..w + 8] == new[w..w + 8] {
                        continue;
                    }
                    for &kind in &snap.kinds {
                        let step = match kind {
                            Kind::U16 => 2,
                            Kind::I32 | Kind::F32 | Kind::Xor | Kind::Smi => 4,
                            Kind::F64 | Kind::Gm | Kind::Lua | Kind::Py => 8,
                        };
                        for o in (w..w + 8).step_by(step) {
                            let size = kind.size();
                            if o + size > len || old[o..o + size] == new[o..o + size] {
                                continue;
                            }
                            let (v0, v1) = (kind.decode(&old[o..]), kind.decode(&new[o..]));
                            // The key stays; a real one is never 0.
                            if kind == Kind::Xor && (old[o + 4..o + 8] != new[o + 4..o + 8] || old[o + 4..o + 8] == [0; 4]) {
                                continue;
                            }
                            if let Some(mirror) = fits(kind, v0, v1) {
                                let c = Candidate::new(addr + o as u64, kind, v1);
                                found.push(match mirror {
                                    true => Candidate { tagged: c.tagged | MIRROR, ..c },
                                    false => c,
                                });
                            }
                        }
                    }
                }
            }
            count.fetch_add(found.len(), Ordering::Relaxed);
            found.sort_by_key(|c| (c.addr(), c.kind() as u8, c.mirror()));
            found
        },
    );
    if count.into_inner() > BAR_MOST {
        return writeln!(out, "error: {too_many}");
    }
    let next: Vec<Candidate> = parts.into_iter().flatten().collect();
    if next.is_empty() {
        return writeln!(out, "0 matches (nothing changed so since the copy, kept it)");
    }
    let read = snap.blocks.iter().map(|b| b.2).sum::<usize>() >> 20;
    // The copy is gone after this step: there is nothing before it to go back to.
    s.replace(step, next);
    s.history.clear();
    s.history_cut = true;
    s.undone.clear();
    writeln!(
        out,
        "0 -> {} matches ({}) from the copy of {read} MiB in {} ms",
        s.candidates.len(),
        kinds_text(&s.candidates),
        t.elapsed().as_millis()
    )
}

/// phase moving|still|switch: a value with no number, followed by what the player says it does
/// (Ferret samples every few seconds; `Core::phase_step`). Moving: a place may stay or move,
/// but only one way in this phase (`MOVED` + `MIRROR` from its first move). Still: it doesn't
/// change. A place breaking that is forgiven once a phase (`MISSED`). Switch: the phase ends;
/// after a moving one, places that moved fewer than 3 times go (`MOVES`: what has no number
/// keeps moving: a timer, a charge, a drain); then every place starts over from its value
/// now, so the jump between phases (a power starting or ending) is never compared. A place
/// that hasn't moved within a moving phase's first `PHASE_START` steps goes too: the player says it's
/// moving when it starts, and junk doesn't start then. A place moving in two steps in a row
/// must move at about the same rate (`steady`): what has no number is a timer, a charge or a
/// drain, while a game's simulation moves by fits. The first moving step compares with the
/// copy of memory (`snap`): every place that changed either way.
const PHASE_START: usize = 3;

/// Rates (a second) close enough to be one steady movement: within a quarter, as sampling
/// times wobble.
fn steady(before: f32, now: f64) -> bool {
    let before = before as f64;
    (now - before).abs() <= 0.25 * before.abs() + 1e-6
}

fn cmd_phase(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let before = s.candidates.len();
    let arg = arg.trim();
    if before == 0 && s.snap.is_some() {
        return match arg {
            "moving" => {
                s.phase_now = Some(true);
                let sane = |kind: Kind, v: f64| !matches!(kind, Kind::F32 | Kind::F64) || v == 0.0 || (1e-3..=1e9).contains(&v.abs());
                let fits = |kind: Kind, v0: f64, v1: f64| (sane(kind, v0) && sane(kind, v1) && v0 != v1).then_some(v1 < v0);
                snap_step(out, s, "phase moving", &fits, "too many places changed since the copy, kept it: let it move a little longer")?;
                // The copy's places all moved: that's this phase's first move.
                for c in s.candidates.iter_mut() {
                    *c = c.with(MOVED, true).moved_once_more();
                }
                Ok(())
            }
            "still" | "switch" => {
                s.phase_now = Some(arg == "still");
                writeln!(out, "0 matches (nothing moved since the copy yet, kept the copy)")
            }
            _ => writeln!(out, "error: usage: phase moving|still|switch"),
        };
    }
    if s.mem.is_none() || before == 0 {
        return writeln!(out, "error: no copy of the game's memory and no matches to go on from");
    }
    let now = s.read_all();
    let next: Vec<Candidate> = match arg {
        "switch" => {
            let was_moving = s.phase_now == Some(true);
            s.phase_now = None;
            s.phase_steps = 0;
            s.phase_rates = None;
            s.candidates
                .iter()
                .zip(now)
                .filter(|(c, _)| !was_moving || c.moves() >= 3)
                .map(|(c, v)| Candidate { value: v.unwrap_or(c.value), ..c.with(MISSED, false).with(MOVED, false).with(MOVES, false) })
                .collect()
        }
        "moving" | "still" => {
            let moving = arg == "moving";
            s.phase_now = Some(moving);
            s.phase_steps += 1;
            let late = moving && s.phase_steps > PHASE_START;
            let read_at = Instant::now();
            let (since, rates) = match s.phase_rates.take() {
                Some((t, r)) if r.len() == before => (Some(read_at.duration_since(t).as_secs_f64()), r),
                _ => (None, vec![f32::NAN; before]),
            };
            let mut kept_rates = Vec::new();
            let next = s
                .candidates
                .iter()
                .zip(now)
                .zip(rates)
                .filter_map(|((c, v), rate)| {
                    let v = v?;
                    let rate_now = since.filter(|&t| t > 0.0 && v != c.value).map_or(f64::NAN, |t| (v - c.value) / t);
                    let fine = match (moving, c.has(MOVED)) {
                        (true, false) if late => return None,
                        _ if v == c.value => true,
                        (false, _) => false,
                        // This phase's first move sets the way it goes.
                        (true, false) => {
                            kept_rates.push(rate_now as f32);
                            return Some(Candidate { value: v, ..c.with(MOVED, true).with(MIRROR, v < c.value).moved_once_more() });
                        }
                        (true, true) => (v < c.value) == c.mirror() && (rate.is_nan() || rate_now.is_nan() || steady(rate, rate_now)),
                    };
                    let kept = match (fine, c.has(MISSED)) {
                        (true, _) if moving && v != c.value => Candidate { value: v, ..c.moved_once_more() },
                        (true, _) => Candidate { value: v, ..*c },
                        (false, false) => Candidate { value: v, ..c.with(MISSED, true) },
                        (false, true) => return None,
                    };
                    // A miss starts the rate over (the jump isn't a rate).
                    kept_rates.push(if fine { rate_now as f32 } else { f32::NAN });
                    Some(kept)
                })
                .collect();
            s.phase_rates = Some((read_at, kept_rates));
            next
        }
        _ => return writeln!(out, "error: usage: phase moving|still|switch"),
    };
    if next.is_empty() {
        return writeln!(out, "{before} -> 0 matches (kept the {before} from before)");
    }
    s.replace(&format!("phase {arg}"), next);
    writeln!(out, "{before} -> {} matches ({})", s.candidates.len(), kinds_text(&s.candidates))
}

/// has <hex addr>: the matches at that address, with their flags ("has 0x10:f32 = 2.5 moved"),
/// or "has none"; for tests that know where the value is.
fn cmd_has(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let Some(addr) = parse_addr(arg.trim()) else {
        return writeln!(out, "error: usage: has <hex addr>");
    };
    let at: Vec<&Candidate> = s.candidates.iter().filter(|c| c.addr() == addr).collect();
    if at.is_empty() {
        return writeln!(out, "has none");
    }
    for c in at {
        let flags: Vec<&str> = [(MIRROR, "mirror"), (MISSED, "missed"), (MOVED, "moved")].iter().filter(|(f, _)| c.has(*f)).map(|(_, n)| *n).collect();
        writeln!(out, "has 0x{addr:x}:{} = {} {}", c.kind().name(), c.value, flags.join(" "))?;
    }
    Ok(())
}

/// undo: the matches as they were before the last step that changed them ("<before> -> <n>
/// matches", then "undid <command>"), with their values as they are now.
fn cmd_undo(out: &mut impl Write, s: &mut Session) -> io::Result<()> {
    let Some((step, old)) = s.history.pop() else {
        return match s.history_cut {
            true => writeln!(out, "error: can't go back further (the earlier matches were too many to keep)"),
            false => writeln!(out, "error: nothing to undo"),
        };
    };
    let before = s.candidates.len();
    let now = std::mem::replace(&mut s.candidates, old);
    // Too many to keep for `redo` (a fresh scan of a small number): it can't be taken again.
    if now.len() > UNDO_KEEP {
        s.undone.clear();
    } else {
        s.undone.push((step.clone(), now));
    }
    s.candidates = s.candidates.iter().zip(s.read_all()).map(|(c, v)| Candidate { value: v.unwrap_or(c.value), ..*c }).collect();
    writeln!(out, "{before} -> {} matches ({})", s.candidates.len(), kinds_text(&s.candidates))?;
    writeln!(out, "undid {step}")
}

/// redo: takes the last step `undo` took back again ("<before> -> <n> matches", then "redid
/// <command>"), with their values as they are now.
fn cmd_redo(out: &mut impl Write, s: &mut Session) -> io::Result<()> {
    let Some((step, again)) = s.undone.pop() else {
        return writeln!(out, "error: nothing to redo");
    };
    let before = s.candidates.len();
    let old = std::mem::replace(&mut s.candidates, again);
    s.history.push((step.clone(), old));
    s.candidates = s.candidates.iter().zip(s.read_all()).map(|(c, v)| Candidate { value: v.unwrap_or(c.value), ..*c }).collect();
    writeln!(out, "{before} -> {} matches ({})", s.candidates.len(), kinds_text(&s.candidates))?;
    writeln!(out, "redid {step}")
}

/// Remembers every candidate's current value, right before the screen is read: `next` then
/// accepts values that showed the number at any point in between (the screen lags memory).
fn cmd_mark(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    // A bar's step compares with the value at the step before: keep that, note these aside.
    if arg.trim() == "bar" {
        s.bar_marks = Some(s.candidates.iter().zip(s.read_all()).map(|(c, v)| v.unwrap_or(c.value)).collect());
        return writeln!(out, "marked {} matches", s.candidates.len());
    }
    let marked: Vec<Candidate> =
        s.candidates.iter().zip(s.read_all()).map(|(c, v)| Candidate { value: v.unwrap_or(c.value), ..*c }).collect();
    s.candidates = marked;
    writeln!(out, "marked {} matches", s.candidates.len())
}

/// list [n]: the first n matches (20).
fn cmd_list(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let most = arg.trim().parse().unwrap_or(20);
    for c in s.candidates.iter().take(most) {
        writeln!(out, "{}", s.describe(c.addr(), c.kind()))?;
    }
    if s.candidates.len() > most {
        writeln!(out, "... {} more", s.candidates.len() - most)?;
    }
    Ok(())
}

fn cmd_write(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let mut it = arg.split_whitespace();
    let loc = it.next().and_then(parse_loc);
    let value = it.next().and_then(|v| v.parse::<f64>().ok());
    let test = it.next() == Some("test");
    let (Some((addr, kind)), Some(value), Some(mem)) = (loc, value, s.mem.as_ref()) else {
        return writeln!(out, "error: usage: write <hex addr[:type]> <n> [test] (after attach)");
    };
    let before = s.read(addr, kind);
    match write_value(mem, addr, kind, value) {
        Ok(()) => {
            // A test value is kept with the value before it; any other write there undoes it.
            let earlier = s.tests.iter().position(|t| t.0 == addr);
            match (test, earlier, before, s.read(addr, kind)) {
                (true, Some(i), _, Some(now)) => s.tests[i].3 = now,
                (true, None, Some(before), Some(now)) => s.tests.push((addr, kind, before, now)),
                (false, Some(i), _, _) => drop(s.tests.remove(i)),
                _ => {}
            }
            writeln!(out, "wrote {value}, reads back {}", s.describe(addr, kind))
        }
        Err(e) => writeln!(out, "error: write failed: {e}"),
    }
}

/// Puts back the test values the frontend left (it went away mid-test, as when Ferret closes
/// during the in-game check): the value before where the test value is still there, only the
/// test's step off where the game changed it since (nearer the test value than the value
/// before), nothing where the game rewrote it.
fn put_back_tests(s: &mut Session) {
    let Some(mem) = s.mem.as_ref() else { return };
    for (addr, kind, before, test) in std::mem::take(&mut s.tests) {
        let step = test - before;
        let back = match read_value(mem, addr, kind) {
            Some(now) if now == test => before,
            Some(now) if (now - test).abs() * 2.0 < step.abs() => (now - step).max(before.min(0.0)),
            _ => continue,
        };
        write_value(mem, addr, kind, back).ok();
    }
}

fn cmd_set(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let (Ok(value), Some(mem)) = (arg.parse::<f64>(), s.mem.as_ref()) else {
        return writeln!(out, "error: usage: set <n> (after attach)");
    };
    let written = s.candidates.iter().filter(|c| write_value(mem, c.addr(), c.kind(), value).is_ok()).count();
    writeln!(out, "wrote {value} to {written} of {} matches", s.candidates.len())
}

fn parse_addr(a: &str) -> Option<u64> {
    u64::from_str_radix(a.trim_start_matches("0x"), 16).ok()
}

fn cmd_peek(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    for a in arg.split_whitespace() {
        match parse_loc(a) {
            Some((addr, kind)) => writeln!(out, "{}", s.describe(addr, kind))?,
            None => writeln!(out, "{a} = ??")?,
        }
    }
    Ok(())
}

fn cmd_track(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let mut tracked: Vec<Candidate> = arg
        .split_whitespace()
        .filter_map(parse_loc)
        .filter_map(|(addr, kind)| Some(Candidate::new(addr, kind, s.read(addr, kind)?)))
        .collect();
    tracked.sort_by_key(|c| c.addr());
    s.replace(format!("track {arg}").trim_end(), tracked);
    writeln!(out, "tracking {} matches", s.candidates.len())
}

fn cmd_keep(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let Some((addr, kind)) = parse_loc(arg) else {
        return writeln!(out, "error: usage: keep <hex addr[:type]>");
    };
    // With a type, only that one: an int and an XOR value can start at the same address.
    let typed = arg.contains(':');
    let before = s.candidates.len();
    let kept = s.candidates.iter().copied().filter(|c| c.addr() == addr && (!typed || c.kind() == kind)).collect();
    s.replace(&format!("keep {arg}"), kept);
    writeln!(out, "{before} -> {} matches", s.candidates.len())
}

// --- Shapes: the memory around a found value. A game keeps its values of one kind the same way
// (every inventory item is a record like the next), so the next one is likely in a place shaped
// like the last.

/// How far around a value a shape looks, before it and after it.
const SHAPE_SPAN: i64 = 32;

#[derive(Clone, Copy, PartialEq)]
enum ShapeWord {
    Exact(u32),
    /// A pointer-sized word pointing into mapped memory (its value changes between runs).
    Pointer,
}

struct Shape {
    kind: Kind,
    /// Offsets from the value, exact words first (they rule places out cheaply).
    words: Vec<(i64, ShapeWord)>,
}

impl Shape {
    /// "<type> <offset>=<hex word>|p ...", e.g. "i32 -8=2 -16=p 4=0".
    fn parse(text: &str) -> Option<Shape> {
        let mut it = text.split_whitespace();
        let kind = Kind::parse(it.next()?)?;
        let mut words = Vec::new();
        for w in it {
            let (off, v) = w.split_once('=')?;
            let word = if v == "p" { ShapeWord::Pointer } else { ShapeWord::Exact(u32::from_str_radix(v, 16).ok()?) };
            words.push((off.parse().ok()?, word));
        }
        words.sort_by_key(|&(_, w)| w == ShapeWord::Pointer);
        Some(Shape { kind, words })
    }

    /// Whether the place has this shape, but for one word in five (two finds don't show every
    /// field that differs between items: Lumencraft's lumen and iron had a pointer where another
    /// item has two small numbers); `at(offset, bytes)` reads around it.
    fn fits(&self, at: impl Fn(i64, usize) -> Option<u64>, mapped: &Mapped, width: usize) -> bool {
        let mut misses = self.words.len() / 5;
        self.words.iter().all(|&(off, w)| {
            let ok = match w {
                ShapeWord::Exact(v) => at(off, 4) == Some(v as u64),
                ShapeWord::Pointer => at(off, width).is_some_and(|p| mapped.contains(p)),
            };
            if !ok && misses > 0 {
                misses -= 1;
                return true;
            }
            ok
        })
    }
}

/// Mapped address ranges, sorted: tells pointers from other numbers quickly.
struct Mapped(Vec<(u64, u64)>);

impl Mapped {
    fn new(regions: &[Region]) -> Self {
        let mut ranges: Vec<(u64, u64)> = regions.iter().map(|r| (r.start, r.end)).collect();
        ranges.sort_unstable();
        Mapped(ranges)
    }

    fn contains(&self, p: u64) -> bool {
        let i = self.0.partition_point(|r| r.1 <= p);
        p >= 0x10000 && self.0.get(i).is_some_and(|r| r.0 <= p)
    }
}

/// `n` little-endian bytes at `i` in `buf`, when they're all in it.
fn le_at(buf: &[u8], i: i64, n: usize) -> Option<u64> {
    let i = usize::try_from(i).ok()?;
    let b = buf.get(i..i + n)?;
    let mut w = [0u8; 8];
    w[..n].copy_from_slice(b);
    Some(u64::from_le_bytes(w))
}

fn read_le(mem: &Mem, addr: u64, n: usize) -> Option<u64> {
    let mut w = [0u8; 8];
    mem.read_exact_at(&mut w[..n], addr).ok()?;
    Some(u64::from_le_bytes(w))
}

/// How far before a value its Windows heap block's header is looked for.
const HEAP_LOOK_BACK: u64 = 0x1000;

/// Where the data of the Windows heap block holding `addr` lies: from its first byte to where the
/// next block's header starts. A 64-bit NT heap block starts with 16 bytes whose second half is
/// XORed with its heap's key (os::heap_key): its size in 16-byte units, flags (1: in use), a
/// checksum byte (the XOR of the three before it) and the size of the block before it, which the
/// next block's header must repeat. `read` reads 8 bytes. None: not in such a block (the
/// low-fragmentation heap encodes its headers otherwise, but those differ from block to block,
/// so shapes drop them on their own).
fn heap_block(read: impl Fn(u64) -> Option<u64>, key: u64, addr: u64) -> Option<(u64, u64)> {
    // The header before data starting at `data`, decoded: (size in bytes, in use, size before).
    let header = |data: u64| {
        let b = (read(data.checked_sub(8)?)? ^ key).to_le_bytes();
        let size = u16::from_le_bytes([b[0], b[1]]) as u64 * 16;
        (b[3] == b[0] ^ b[1] ^ b[2] && size >= 32).then(|| (size, b[2] & 1 != 0, u16::from_le_bytes([b[4], b[5]]) as u64 * 16))
    };
    let mut data = addr & !15;
    while addr - data < HEAP_LOOK_BACK && data >= 16 {
        if let Some((size, true, _)) = header(data) {
            // The next block's encoded half ends this one's data (the first half is this one's).
            let end = data + size - 8;
            if addr < end && header(data + size).is_some_and(|(_, _, before)| before == size) {
                return Some((data, end));
            }
        }
        data -= 16;
    }
    None
}

/// shape <hex addr[:type]>: the memory around the value as a shape, every word in it (the
/// caller keeps what other finds agree on). On Windows only the words of its heap block: past
/// it is the next block's encoded header, which changes with its neighbours.
fn cmd_shape(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let (Some((addr, kind)), Some(mem)) = (parse_loc(arg), s.mem.as_ref()) else {
        return writeln!(out, "error: usage: shape <hex addr[:type]> (after attach)");
    };
    let mapped = Mapped::new(&maps(s.pid)?);
    let key = os::heap_key(s.pid, addr).filter(|_| s.width == 8);
    let block = key.and_then(|key| heap_block(|a| read_le(mem, a, 8), key, addr));
    let inside = |at: u64, n: i64| block.is_none_or(|(start, end)| at >= start && at.wrapping_add(n as u64) <= end);
    let size = kind.size() as i64;
    let mut words = Vec::new();
    let mut off = -SHAPE_SPAN;
    while off < size + SHAPE_SPAN {
        if (0..size).contains(&off) {
            off = size;
            continue;
        }
        let at = addr.wrapping_add_signed(off);
        if !inside(at, 4) {
            off += 4;
            continue;
        }
        let width = s.width as i64;
        // A pointer: aligned, clear of the value, into mapped memory.
        let pointer = at % s.width as u64 == 0
            && (off + width <= 0 || off >= size)
            && inside(at, width)
            && read_le(mem, at, s.width).is_some_and(|p| mapped.contains(p));
        if pointer {
            words.push(format!("{off}=p"));
            off += width;
            continue;
        }
        let Some(w) = read_le(mem, at, 4) else {
            off += 4;
            continue;
        };
        words.push(format!("{off}={w:x}"));
        off += 4;
    }
    writeln!(out, "shape {} {}", kind.name(), words.join(" "))
}

/// shapes <shape>; <shape>...: the shapes scans prefer (none: every place).
fn cmd_shapes(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let shapes: Option<Vec<Shape>> = arg.split(';').map(str::trim).filter(|t| !t.is_empty()).map(Shape::parse).collect();
    let Some(shapes) = shapes else {
        return writeln!(out, "error: usage: shapes <type> <offset>=<hex>|p ...; ...");
    };
    s.shapes = shapes;
    writeln!(out, "{} shapes", s.shapes.len())
}

/// about <hex addr>...: "0x<addr> <what it is>" for each address Ferret can tell something
/// about (an entry of a Godot dictionary: its key and ids).
fn cmd_about(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let heap = Heap::new(s.pid, mem, s.width)?;
    let locs: Vec<(u64, Kind)> = arg.split_whitespace().filter_map(parse_loc).collect();
    let addrs: Vec<u64> = locs.iter().map(|l| l.0).collect();
    let new: Vec<u64> = addrs.iter().copied().filter(|a| !s.abouts.contains_key(a)).collect();
    let mut texts: Vec<Option<String>> = new.iter().map(|&a| godot::about(&heap, a).or_else(|| mono::about(&heap, a))).collect();
    // Unreal places in one pass over the objects (a fraction of a second).
    let unnamed: Vec<u64> = new.iter().zip(&texts).filter(|(_, t)| t.is_none()).map(|(a, _)| *a).collect();
    if let (Ok(ue), false) = (unreal(s.pid, mem), unnamed.is_empty()) {
        let found = ue.discover_all(mem, &unnamed);
        for (a, f) in unnamed.iter().zip(found) {
            if let Some(i) = new.iter().position(|n| n == a) {
                texts[i] = f.first().map(|(p, _)| p.describe());
            }
        }
    }
    // Godot script variables, one level up from each (a few passes over memory for all).
    let unnamed: Vec<u64> = new.iter().zip(&texts).filter(|(_, t)| t.is_none()).map(|(a, _)| *a).collect();
    if !unnamed.is_empty() && is_godot(s.pid) {
        let unnamed: Vec<(u64, Kind)> = unnamed.iter().filter_map(|a| locs.iter().find(|l| l.0 == *a).copied()).collect();
        for ((a, _), f) in unnamed.iter().zip(gdscript::about_all(&heap, &unnamed)) {
            if let (Some(i), Some(f)) = (new.iter().position(|n| n == a), f) {
                texts[i] = Some(f);
            }
        }
    }
    s.abouts.extend(new.into_iter().zip(texts));
    for a in addrs {
        if let Some(Some(text)) = s.abouts.get(&a) {
            writeln!(out, "0x{a:x} {text}")?;
        }
    }
    Ok(())
}

/// alive: whether the attached game still runs (the same program under the same pid).
fn cmd_alive(out: &mut impl Write, s: &Session) -> io::Result<()> {
    let alive = s.pid != 0 && exe_name(s.pid) == s.exe;
    writeln!(out, "alive {}", if alive { "yes" } else { "no" })
}

/// drop <hex addr[:type]>: the player ruled this one out.
fn cmd_drop(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let Some((addr, kind)) = parse_loc(arg) else {
        return writeln!(out, "error: usage: drop <hex addr[:type]>");
    };
    let typed = arg.contains(':');
    let before = s.candidates.len();
    let left = s.candidates.iter().copied().filter(|c| !(c.addr() == addr && (!typed || c.kind() == kind))).collect();
    s.replace(&format!("drop {arg}"), left);
    writeln!(out, "{before} -> {} matches", s.candidates.len())
}

// --- Code patterns ("??" = any byte) for finding the same code after a restart.

fn is_mapped(regions: &[Region], v: u64) -> bool {
    v >= 0x10000 && regions.iter().any(|r| v >= r.start && v < r.end)
}

/// Pattern of the code around an access (whose bytes are `keep`). Operands
/// that hold addresses change between runs, so they become wildcards: call and
/// jump targets, [absolute] memory operands, and constants pointing into memory.
fn pattern_of(code: &[u8], regions: &[Region], keep: std::ops::Range<usize>) -> Vec<Option<u8>> {
    let mut pat: Vec<Option<u8>> = code.iter().map(|b| Some(*b)).collect();
    for i in 1..code.len().saturating_sub(3) {
        if keep.contains(&i) || keep.contains(&(i + 3)) {
            continue;
        }
        let v = u32::from_le_bytes([code[i], code[i + 1], code[i + 2], code[i + 3]]) as u64;
        let prev = code[i - 1];
        let wild = matches!(prev, 0xE8 | 0xE9)
            || prev & 0xC7 == 0x05
            || (matches!(prev, 0xA1 | 0xA3 | 0x68 | 0xB8..=0xBF) && is_mapped(regions, v));
        if wild {
            pat[i..i + 4].fill(None);
        }
    }
    pat
}

fn pattern_text(pat: &[Option<u8>]) -> String {
    pat.iter().map(|b| b.map_or("??".to_owned(), |b| format!("{b:02x}"))).collect()
}

fn parse_pattern(text: &str) -> Option<Vec<Option<u8>>> {
    text.as_bytes()
        .chunks(2)
        .map(|c| match c {
            b"??" => Some(None),
            _ => u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok().map(Some),
        })
        .collect()
}

/// Addresses in executable memory where `pat` matches (up to `limit`).
fn find_pattern(pid: u32, mem: &Mem, pat: &[Option<u8>], limit: usize) -> io::Result<Vec<u64>> {
    Ok(find_patterns(pid, mem, &[pat], limit)?.pop().unwrap_or_default())
}

/// `find_pattern` for several patterns in one pass over the game's code (see `Patterns`).
fn find_patterns(pid: u32, mem: &Mem, pats: &[&[Option<u8>]], limit: usize) -> io::Result<Vec<Vec<u64>>> {
    let finder = Patterns::new(pats);
    let mut found = vec![Vec::new(); pats.len()];
    for r in maps(pid)?.iter().filter(|r| r.perms.contains('x')) {
        let mut buf = vec![0u8; (r.end - r.start) as usize];
        if mem.read_exact_at(&mut buf, r.start).is_ok() {
            finder.search(&buf, r.start, &mut found, limit);
        }
    }
    Ok(found)
}

/// Code patterns looked for together, by two fixed bytes of each at a time: checking every
/// pattern at every byte took 1.2 s a pattern on From Dust (638 MiB of code), and a value has
/// up to 5.
struct Patterns<'a> {
    pats: &'a [&'a [Option<u8>]],
    /// Each pattern's first two fixed bytes in a row: where they are, and as one number.
    anchors: Vec<(usize, u16)>,
    /// Those numbers, one bit each.
    wanted: Vec<u64>,
}

impl<'a> Patterns<'a> {
    fn new(pats: &'a [&'a [Option<u8>]]) -> Self {
        let anchors: Vec<(usize, u16)> = pats
            .iter()
            .map(|pat| {
                let at = (0..pat.len().saturating_sub(1)).find(|&i| pat[i].is_some() && pat[i + 1].is_some()).unwrap_or(0);
                let byte = |i: usize| pat.get(i).copied().flatten().unwrap_or(0);
                (at, u16::from_le_bytes([byte(at), byte(at + 1)]))
            })
            .collect();
        let mut wanted = vec![0u64; 1024];
        for &(_, a) in &anchors {
            wanted[a as usize >> 6] |= 1 << (a & 63);
        }
        Patterns { pats, anchors, wanted }
    }

    /// Adds where each pattern matches in `buf` (at `start` in the game) to `found`, up to
    /// `limit` each.
    fn search(&self, buf: &[u8], start: u64, found: &mut [Vec<u64>], limit: usize) {
        let matches = |pat: &[Option<u8>], buf: &[u8]| buf.len() >= pat.len() && pat.iter().zip(buf).all(|(p, b)| p.is_none_or(|p| p == *b));
        for i in 0..buf.len().saturating_sub(1) {
            let two = u16::from_le_bytes([buf[i], buf[i + 1]]);
            if self.wanted[two as usize >> 6] & 1 << (two & 63) == 0 {
                continue;
            }
            for (n, &(at, a)) in self.anchors.iter().enumerate() {
                let Some(from) = i.checked_sub(at).filter(|_| a == two && found[n].len() < limit) else { continue };
                if matches(self.pats[n], &buf[from..]) {
                    found[n].push(start + from as u64);
                }
            }
        }
    }
}

/// "What accesses this address": watches it with a hardware breakpoint for a
/// few seconds and prints a restart-proof pattern for each instruction found.
/// Output lines: site <pattern> <offset of instruction in pattern> <base register> <displacement>
/// build: "build <hex>", a stamp of the files holding the game's code (size and modification
/// time of the program, IL2CPP's GameAssembly and Mono's Assembly-CSharp.dll): it changes when
/// the game is updated, when code patterns saved before may find the wrong place.
fn cmd_build(out: &mut impl Write, s: &Session) -> io::Result<()> {
    let regions = maps(s.pid)?;
    let exe = exe_name(s.pid).to_lowercase();
    let file_name = |p: &str| p.rsplit('/').next().unwrap_or_default().to_lowercase();
    let mut files: Vec<String> = regions.iter().map(|r| r.path.clone()).filter(|p| from_file(p) && (file_name(p) == exe || file_name(p).starts_with("gameassembly."))).collect();
    // Mono reads it from <Game>_Data/Managed (not always mapped).
    if let Some(data) = regions.iter().find_map(|r| r.path.find("_Data/").map(|i| r.path[..i + 5].to_owned())) {
        files.push(format!("{data}/Managed/Assembly-CSharp.dll"));
    }
    files.sort();
    files.dedup();
    // FNV-1a over each file's name, size and the start and end of its contents: the same on
    // every computer with this version (a PE program's header holds its build time), so shared
    // values can tell "your version". The stamp before it (path, size, time) differed on every
    // computer; it comes on a "was" line, so saved values can be moved over.
    let fnv = |hash: &mut u64, bytes: &[u8]| {
        for &b in bytes {
            *hash = (*hash ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    };
    let (mut hash, mut was) = (0xcbf2_9ce4_8422_2325u64, 0xcbf2_9ce4_8422_2325u64);
    let mut stamped = 0;
    for f in &files {
        let at = os::inside(s.pid, Path::new(f));
        let Ok(m) = fs::metadata(&at) else { continue };
        let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        fnv(&mut was, format!("{f} {} {mtime}", m.len()).as_bytes());
        let Ok(file) = File::open(&at) else { continue };
        let n = BUILD_SAMPLE.min(m.len());
        let (mut start, mut end) = (vec![0u8; n as usize], vec![0u8; n as usize]);
        if file.read_exact_at(&mut start, 0).is_err() || file.read_exact_at(&mut end, m.len() - n).is_err() {
            continue;
        }
        fnv(&mut hash, format!("{} {} ", file_name(f), m.len()).as_bytes());
        fnv(&mut hash, &start);
        fnv(&mut hash, &end);
        stamped += 1;
    }
    match stamped {
        0 => writeln!(out, "error: the game's program file isn't readable"),
        _ => writeln!(out, "build {hash:016x}").and_then(|_| writeln!(out, "was {was:016x}")),
    }
}

/// Bytes read from each end of a program file for its build stamp.
const BUILD_SAMPLE: u64 = 64 * 1024;

const JAVA: &str = "Java game: it moves its objects around and shares its code between values, so values are found again by name only";

fn cmd_sites(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    if s.java {
        return writeln!(out, "error: {JAVA}");
    }
    let mut it = arg.split_whitespace();
    let (Some((target, kind)), Some(mem)) = (it.next().and_then(parse_loc), s.mem.as_ref()) else {
        return writeln!(out, "error: usage: sites <hex addr[:type]> [seconds] [wait] (after attach)");
    };
    let secs = it.next().and_then(|v| v.parse().ok()).unwrap_or(5);
    // "wait": until the game touches it (then `secs` more, for the other instructions that do),
    // or until the frontend sends a line (`cancel`): values that only change when the player
    // acts (ammo when firing) go untouched for minutes.
    let wait = it.next() == Some("wait");
    let mut hits: Vec<(u64, trace::Regs)> = Vec::new();
    {
        let _tracing = TRACING.lock().unwrap();
        let mut tracer = match trace::Tracer::attach(s.pid) {
            Ok(t) => t,
            Err(e) => return writeln!(out, "error: cannot trace the game: {e}"),
        };
        let threads = tracer.arm(target, if kind == Kind::U16 { trace::DR7_ACCESS_2 } else { trace::DR7_ACCESS_4 });
        if wait {
            writeln!(out, "watching 0x{target:x} on {threads} threads until the game touches it")?;
        } else {
            writeln!(out, "watching 0x{target:x} on {threads} threads for {secs} s")?;
        }
        out.flush()?;
        let start = Instant::now();
        let first = std::cell::Cell::new(None::<Instant>);
        let done = || match first.get() {
            Some(t) if wait => t.elapsed() >= Duration::from_secs(secs),
            _ if wait => frontend_spoke(),
            _ => start.elapsed() >= Duration::from_secs(secs),
        };
        tracer.watch_until(done, |hit| {
            if first.get().is_none() {
                first.set(Some(Instant::now()));
            }
            if !hits.iter().any(|(rip, _)| *rip == hit.regs.rip) {
                hits.push((hit.regs.rip, hit.regs));
            }
            hits.len() < 16
        });
    }
    let regions = maps(s.pid)?;
    for (after, regs) in &hits {
        let before = 64u64;
        let mut code = vec![0u8; before as usize + 16];
        if mem.read_exact_at(&mut code, after - before).is_err() {
            continue;
        }
        let readings = trace::decode_access(&code, after - before, *after, target, regs);
        if readings.is_empty() {
            let tail: Vec<String> = code[before as usize - 16..before as usize].iter().map(|b| format!("{b:02x}")).collect();
            writeln!(out, "instruction before 0x{after:x}: not a [register+offset] access, skipped (bytes before it: {})", tail.join(" "))?;
            continue;
        }
        // Shared code (a runtime function every variable goes through, as in GameMaker games)
        // reads other addresses too: a pattern for it would find some other value next time.
        // An execute breakpoint only fires at the real start, which picks between the readings.
        let (acc, read) = readings
            .iter()
            .map(|acc| (*acc, site_targets(s.pid, acc.start, acc.base, acc.disp, Duration::from_secs(1))))
            .find(|(_, read)| !read.is_empty())
            .unwrap_or((readings[0], Vec::new()));
        let others = read.iter().filter(|a| *a & 0xFFFF_FFFF != target & 0xFFFF_FFFF).count();
        if others > 0 {
            writeln!(
                out,
                "instruction at 0x{:x}: shared code, it also reads {others}{} other addresses, skipped",
                acc.start,
                if read.len() >= SITE_TARGETS_MAX { "+" } else { "" }
            )?;
            continue;
        }
        let instr = (acc.start - (after - before)) as usize;
        // Grow the pattern backwards until it matches only this spot.
        let mut site = None;
        for lead in [16usize, 32, 48, 64] {
            let from = instr.saturating_sub(lead);
            let pat = pattern_of(&code[from..instr + acc.len + 8], &regions, instr - from..instr - from + acc.len);
            if find_pattern(s.pid, mem, &pat, 2)?.len() == 1 {
                site = Some((pat, instr - from));
                break;
            }
        }
        match site {
            Some((pat, off)) => writeln!(
                out,
                "site {} {off} {} {}",
                pattern_text(&pat),
                trace::REG_NAMES[acc.base as usize],
                acc.disp
            )?,
            None => writeln!(out, "instruction at 0x{:x}: no unique pattern, skipped", acc.start)?,
        }
    }
    writeln!(out, "{} instructions accessed it", hits.len())
}

const SITE_TARGETS_MAX: usize = 8;

/// Whether the frontend sent a line (or went away) while a command runs: the line stays unread
/// for the main loop. Commands come one at a time, so nothing sits in stdin's buffer meanwhile.
/// `first <addr:type>... [seconds]`: write watchpoints on up to 4 places (the probe's
/// candidates, holding test values); replies `first <addr:type> <value>` for the first one the
/// game changes (a write leaving it as it was doesn't count), or `none` when time runs out or
/// the frontend sends a line. The real value changes before the copies made from it, even
/// within one frame (Brotato's health and its run-data copy, both rewritten on a hit).
fn cmd_first(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    if s.java {
        return writeln!(out, "error: {JAVA}");
    }
    let mut locs = Vec::new();
    let mut secs = 180;
    for w in arg.split_whitespace() {
        match (parse_loc(w), w.parse()) {
            (_, Ok(n)) if !w.contains(':') && w.len() < 6 => secs = n,
            (Some(l), _) => locs.push(l),
            _ => return writeln!(out, "error: usage: first <hex addr[:type]>... [seconds]"),
        }
    }
    locs.truncate(4);
    if locs.is_empty() {
        return writeln!(out, "error: usage: first <hex addr[:type]>... [seconds]");
    }
    let before: Vec<Option<f64>> = locs.iter().map(|&(a, k)| s.read(a, k)).collect();
    // Write-only (RW 01), 4 bytes (LEN 11), 8 for doubles (LEN 10, 8-aligned), 2 (LEN 01).
    let dr7 = locs.iter().enumerate().fold(0u64, |dr7, (n, &(_, k))| {
        let len: u64 = match k {
            Kind::F64 => 2,
            Kind::U16 => 1,
            _ => 3,
        };
        dr7 | 1 << (2 * n) | 1 << (16 + 4 * n) | len << (18 + 4 * n)
    });
    let addrs: Vec<u64> = locs.iter().map(|&(a, _)| a).collect();
    let mut changed = None;
    {
        let _tracing = TRACING.lock().unwrap();
        let mut tracer = match trace::Tracer::attach(s.pid) {
            Ok(t) => t,
            Err(e) => return writeln!(out, "error: cannot trace the game: {e}"),
        };
        let threads = tracer.arm_slots(&addrs, dr7);
        writeln!(out, "watching {} places on {threads} threads until the game changes one", locs.len())?;
        out.flush()?;
        let start = Instant::now();
        tracer.watch_until(
            || start.elapsed() >= Duration::from_secs(secs) || frontend_spoke(),
            |hit| {
                let Some(&(addr, kind)) = locs.get(hit.slot) else { return true };
                let now = s.read(addr, kind);
                if now != before[hit.slot] {
                    changed = Some((addr, kind, now));
                    return false;
                }
                true
            },
        );
    }
    match changed {
        Some((addr, kind, now)) => {
            writeln!(out, "first 0x{addr:x}:{} {}", kind.name(), now.map_or("??".into(), |v| v.to_string()))
        }
        None => writeln!(out, "none"),
    }
}

fn frontend_spoke() -> bool {
    os::stdin_ready()
}

/// The distinct addresses the instruction at `instr` reads as [base + disp] while the game runs
/// for up to `timeout` (stops early after enough of them).
fn site_targets(pid: u32, instr: u64, base: u8, disp: i64, timeout: Duration) -> Vec<u64> {
    let _tracing = TRACING.lock().unwrap();
    let Ok(mut tracer) = trace::Tracer::attach(pid) else { return Vec::new() };
    tracer.arm(instr, trace::DR7_EXECUTE);
    let mut read: Vec<u64> = Vec::new();
    let mut hits = 0;
    tracer.watch(timeout, |hit| {
        let addr = trace::reg(&hit.regs, base).wrapping_add(disp as u64);
        if !read.contains(&addr) {
            read.push(addr);
        }
        hits += 1;
        hits < 200 && read.len() < SITE_TARGETS_MAX
    });
    read
}

/// Only one tracer may run at a time: two would steal each other's events.
static TRACING: Mutex<()> = Mutex::new(());

/// A saved code pattern: the instruction at `off` reads [`base` + `disp`].
#[derive(Clone)]
struct Site {
    pat: Vec<Option<u8>>,
    off: u64,
    base: u8,
    disp: i64,
    /// Shared code (one routine for every power, unit, slot): the place it reads is the value's
    /// only when the 4-byte number this far from it holds this (an index or an id).
    key: Option<Key>,
}

/// `key=<offset>:<number>` (see `Site.key`).
type Key = (i64, i32);

fn parse_key(token: &str) -> Option<Key> {
    let (off, v) = token.strip_prefix("key=")?.split_once(':')?;
    Some((off.parse().ok()?, v.parse().ok()?))
}

/// Whether the place a site led to is the value's: its key (if any) is there.
fn key_holds(mem: &Mem, addr: u64, key: Option<Key>) -> bool {
    let Some((off, v)) = key else { return true };
    read_value(mem, addr.wrapping_add(off as u64), Kind::I32) == Some(v as f64)
}

impl Site {
    fn parse(f: &[&str]) -> Option<Self> {
        let reg = f.get(2)?;
        Some(Site {
            pat: parse_pattern(f.first()?)?,
            off: f.get(1)?.parse().ok()?,
            base: trace::REG_NAMES.iter().position(|n| n == reg)? as u8,
            disp: f.get(3)?.parse().ok()?,
            key: None,
        })
    }
}

/// The key among a value's tokens (`key=...`), and the tokens without it.
fn take_key<'a>(tokens: &[&'a str]) -> (Option<Key>, Vec<&'a str>) {
    let key = tokens.iter().find_map(|t| parse_key(t));
    (key, tokens.iter().filter(|t| !t.starts_with("key=")).copied().collect())
}

/// Where a code pattern led: the value's address, and when the code loaded the object from a
/// static pointer, that pointer as a path (good for this run only).
struct Resolved {
    addr: u64,
    /// The displacement of the site that led there (the object starts that far before).
    disp: i64,
    is_64bit: bool,
    holder: Option<PtrPath>,
}

/// How far before the instruction its base register may have been loaded.
const HOLDER_REACH: usize = 32;

/// The static pointer the site's instruction (at `instr`) got its object from, when a
/// `mov <base>,[address]` shortly before it (how Mono and C++ code read a static object) loads
/// from a pointer that holds that object now. Following it finds the object the game uses,
/// also after the game replaces it (Particle Fleet does when the map changes).
fn holder(mem: &Mem, instr: u64, site: &Site, is_64bit: bool, width: usize, addr: u64) -> Option<PtrPath> {
    let object = addr.wrapping_sub(site.disp as u64);
    if site.disp < 0 || object < 0x10000 {
        return None;
    }
    let from = instr.checked_sub(HOLDER_REACH as u64)?;
    let mut code = [0u8; HOLDER_REACH];
    mem.read_exact_at(&mut code, from).ok()?;
    let modrm = (site.base & 7) << 3 | 5;
    let rex = 0x48 | (site.base >> 3) << 2;
    // `i` = where the load's 32-bit address or displacement starts; the nearest load first.
    (1..=HOLDER_REACH - 4).rev().find_map(|i| {
        let disp = u32::from_le_bytes([code[i], code[i + 1], code[i + 2], code[i + 3]]);
        let (at, size) = if is_64bit {
            // 48 8b 05 <disp32>: mov r64,[rip+disp32]
            let load = i >= 3 && code[i - 3] == rex && code[i - 2] == 0x8b && code[i - 1] == modrm;
            load.then_some(((from + i as u64 + 4).wrapping_add(disp as i32 as u64), 8))?
        } else if (i >= 2 && code[i - 2] == 0x8b && code[i - 1] == modrm) || (site.base == 0 && code[i - 1] == 0xa1) {
            // 8b 05 <addr32> or a1 <addr32>: mov r32,[addr32]
            (disp as u64, 4)
        } else {
            return None;
        };
        let mut held = [0u8; 8];
        mem.read_exact_at(&mut held[..size], at).ok()?;
        (size == width && u64::from_le_bytes(held) == object)
            .then(|| PtrPath { module: String::new(), base: at, offsets: vec![site.disp as u64] })
    })
}

/// The first of `sites` whose code loads the object holding `addr` from a static pointer.
fn holder_in_code(pid: u32, mem: &Mem, sites: &[Site], is_64bit: bool, width: usize, addr: u64) -> Option<PtrPath> {
    sites.iter().find_map(|site| {
        let [code] = find_pattern(pid, mem, &site.pat, 2).ok()?[..] else { return None };
        holder(mem, code + site.off, site, is_64bit, width, addr)
    })
}

/// Finds a saved pattern in the game's code, waits for that instruction to
/// run and reads its base register: the value's address in this run.
fn resolve_site(pid: u32, mem: &Mem, site: &Site, width: usize, timeout: Duration) -> Result<Resolved, String> {
    let found = find_pattern(pid, mem, &site.pat, 2).map_err(|e| e.to_string())?;
    let [code] = found.as_slice() else {
        return Err(format!("pattern found {} times (the game may not have run that code yet)", found.len()));
    };
    let instr = code + site.off;
    let _tracing = TRACING.lock().unwrap();
    let mut resolved = None;
    let mut tracer = trace::Tracer::attach(pid).map_err(|e| format!("cannot trace the game: {e}"))?;
    tracer.arm(instr, trace::DR7_EXECUTE);
    tracer.watch(timeout, |hit| {
        // The thread is stopped here, so the static pointer still holds what it loaded.
        let addr = trace::reg(&hit.regs, site.base).wrapping_add(site.disp as u64);
        if !key_holds(mem, addr, site.key) {
            return true;
        }
        let is_64bit = trace::is_64bit(&hit.regs);
        resolved = Some(Resolved { addr, disp: site.disp, is_64bit, holder: holder(mem, instr, site, is_64bit, width, addr) });
        false
    });
    resolved.ok_or(format!("the code at 0x{instr:x} did not run within {} s", timeout.as_secs()))
}

/// Finds several values by their code patterns at once: up to 4 instructions are armed
/// together (one per value first), so a restore waits `timeout` per 4 instructions instead of
/// per instruction (Wolfenstein: three sites of a value only firing runs, 10 s each, held up
/// everything else). One result per value: where it led, or why not.
fn resolve_values(pid: u32, mem: &Mem, values: &[Vec<Site>], width: usize, timeout: Duration) -> Vec<Result<Resolved, String>> {
    let mut results: Vec<Result<Resolved, String>> = values.iter().map(|_| Err("none of its code ran meanwhile".into())).collect();
    // (value, site, instruction), first sites of every value first. Values found through the
    // same code (others like a value: From Dust's four power timers) search the code once.
    let mut pending = Vec::new();
    let mut pats: Vec<&[Option<u8>]> = Vec::new();
    for site in values.iter().flatten() {
        if !pats.contains(&&site.pat[..]) {
            pats.push(&site.pat);
        }
    }
    let searched = find_patterns(pid, mem, &pats, 2).map_err(|e| e.to_string());
    let most = values.iter().map(Vec::len).max().unwrap_or(0);
    for n in 0..most {
        for (v, sites) in values.iter().enumerate() {
            let Some(site) = sites.get(n) else { continue };
            match &searched {
                Ok(found) => {
                    if let Some([code]) = pats.iter().position(|p| *p == &site.pat[..]).map(|i| &found[i][..]) {
                        pending.push((v, n, code + site.off));
                    }
                }
                Err(e) => results[v] = Err(e.clone()),
            }
        }
    }
    // None of its patterns is in the game's code once: changed by an update (or a Mono game
    // hasn't compiled that code yet).
    for (v, sites) in values.iter().enumerate() {
        if !sites.is_empty() && !pending.iter().any(|p| p.0 == v) && results[v].as_ref().is_err_and(|e| e.starts_with("none of its code")) {
            results[v] = Err("its code patterns aren't in the game's code: the game was updated, or hasn't run that code yet".into());
        }
    }
    let _tracing = TRACING.lock().unwrap();
    let mut done = vec![false; values.len()];
    while pending.iter().any(|(v, _, _)| !done[*v]) {
        pending.retain(|(v, _, _)| !done[*v]);
        // Up to 4 instructions (the CPU's breakpoints), each with every value that goes through
        // it: shared code runs for each of them in turn, and its key says whose turn it is.
        let mut instrs: Vec<u64> = Vec::new();
        for &(_, _, i) in &pending {
            if instrs.len() < 4 && !instrs.contains(&i) {
                instrs.push(i);
            }
        }
        let round: Vec<(usize, usize, u64)> = pending.iter().filter(|p| instrs.contains(&p.2)).copied().collect();
        pending.retain(|p| !instrs.contains(&p.2));
        let mut tracer = match trace::Tracer::attach(pid) {
            Ok(t) => t,
            Err(e) => {
                for (v, _, _) in &round {
                    results[*v] = Err(format!("cannot trace the game: {e}"));
                }
                return results;
            }
        };
        tracer.arm_execute(&instrs);
        tracer.watch(timeout, |hit| {
            let Some(&instr) = instrs.get(hit.slot) else { return true };
            for &(v, n, _) in round.iter().filter(|r| r.2 == instr) {
                let site = &values[v][n];
                let addr = trace::reg(&hit.regs, site.base).wrapping_add(site.disp as u64);
                // Shared code ran for another power, unit or slot: not this value.
                if !done[v] && key_holds(mem, addr, site.key) {
                    let is_64bit = trace::is_64bit(&hit.regs);
                    results[v] = Ok(Resolved { addr, disp: site.disp, is_64bit, holder: holder(mem, instr, site, is_64bit, width, addr) });
                    done[v] = true;
                }
            }
            round.iter().any(|(v, _, _)| !done[*v])
        });
    }
    results
}

/// resolve-all <seconds> | <name> <type> <pattern> <offset> <register> <displacement> [<pattern>
/// ...] | <name> ...: every value's code patterns armed together; per value a line "<name>
/// <addr:type> = <value>" (and "<name> via <path>"), or "<name> error: <why>".
fn cmd_resolve_all(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    if s.java {
        return writeln!(out, "error: {JAVA}");
    }
    let mut parts = arg.split('|');
    let secs: f64 = parts.next().and_then(|t| t.trim().parse().ok()).unwrap_or(2.0);
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: attach first");
    };
    let mut values = Vec::new();
    for part in parts {
        let f: Vec<&str> = part.split_whitespace().collect();
        let (Some(name), Some(kind)) = (f.first(), f.get(1).and_then(|k| Kind::parse(k))) else {
            return writeln!(out, "error: usage: resolve-all <seconds> | <name> <type> <pattern> <offset> <register> <displacement> ... | ...");
        };
        let (key, f) = take_key(&f);
        let sites: Option<Vec<Site>> = f[2..].chunks(4).map(|c| Site::parse(c).map(|s| Site { key, ..s })).collect();
        let Some(sites) = sites.filter(|s| !s.is_empty()) else {
            return writeln!(out, "error: {name}: bad code pattern");
        };
        values.push((name.to_string(), kind, sites));
    }
    let sites: Vec<Vec<Site>> = values.iter_mut().map(|(_, _, s)| std::mem::take(s)).collect();
    let results = resolve_values(s.pid, mem, &sites, s.width, Duration::from_secs_f64(secs));
    for ((name, kind, _), r) in values.iter().zip(results) {
        match r {
            Ok(Resolved { addr, holder, .. }) => match s.read(addr, *kind) {
                Some(_) => {
                    writeln!(out, "{name} {}", s.describe(addr, *kind))?;
                    if let Some(h) = holder {
                        writeln!(out, "{name} via {}", h.text())?;
                    }
                }
                None => writeln!(out, "{name} error: resolved to unreadable 0x{addr:x}")?,
            },
            Err(e) => writeln!(out, "{name} error: {e}")?,
        }
    }
    Ok(())
}

/// Most other places `others` collects (a game running the code for hundreds of units).
const OTHERS_MAX: usize = 64;
/// How far from a value `others` looks for its key, either way.
const KEY_REACH: i64 = 64;

/// others <seconds> <hex addr[:type]> [key=<offset>:<number>] <pattern:offset:register:displacement>...:
/// "more like this". Watches the value's own code (its saved sites) until the time is up or
/// the frontend sends a line, and collects every other place it reaches the same way: the same
/// field of the game's other powers, units or slots (From Dust runs one routine for each of its
/// four powers while that power runs). Prints "note others <n>" as they come, then "key
/// <offset> <number>": a 4-byte number near the value that tells it from the others (an index
/// or an id: unchanged while watched, 0 to 65535, different in each), or "key none"; then
/// "other 0x<addr>:<type> = <value> <its key number>" for each, and "<n> others". A key given
/// (the value was saved with one) is tried first.
fn cmd_others(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    if s.java {
        return writeln!(out, "error: {JAVA}");
    }
    let usage = "error: usage: others <seconds> <hex addr[:type]> [key=<offset>:<number>] <pattern:offset:register:displacement>... (after attach)";
    let (given, f) = take_key(&arg.split_whitespace().collect::<Vec<_>>());
    let (Some(secs), Some((target, kind)), Some(mem)) =
        (f.first().and_then(|v| v.parse::<f64>().ok()), f.get(1).and_then(|a| parse_loc(a)), s.mem.as_ref())
    else {
        return writeln!(out, "{usage}");
    };
    let sites: Option<Vec<Site>> = f.get(2..).unwrap_or_default().iter().map(|t| Site::parse(&t.split(':').collect::<Vec<_>>())).collect();
    let Some(sites) = sites.filter(|s| !s.is_empty()) else {
        return writeln!(out, "{usage}");
    };
    // Each site's instruction once, up to the CPU's 4 breakpoints.
    let mut armed: Vec<(u64, &Site)> = Vec::new();
    let found = find_patterns(s.pid, mem, &sites.iter().map(|s| &s.pat[..]).collect::<Vec<_>>(), 2)?;
    for (site, found) in sites.iter().zip(found) {
        if let [code] = found[..] {
            if armed.len() < 4 && !armed.iter().any(|(i, _)| *i == code + site.off) {
                armed.push((code + site.off, site));
            }
        }
    }
    if armed.is_empty() {
        return writeln!(out, "error: its code patterns aren't in the game's code");
    }
    let low = |a: u64| a & 0xFFFF_FFFF;
    // The words around a place (its key is among them).
    let around = |addr: u64| -> Option<Vec<u8>> {
        let mut b = vec![0u8; 2 * KEY_REACH as usize + 8];
        mem.read_exact_at(&mut b, addr.checked_sub(KEY_REACH as u64)?).ok().map(|_| b)
    };
    // The value's own words and every other place's, as they were when the code reached them.
    let mut own: Option<Vec<u8>> = None;
    let mut places: Vec<(u64, Option<Vec<u8>>)> = Vec::new();
    {
        let _tracing = TRACING.lock().unwrap();
        let mut tracer = match trace::Tracer::attach(s.pid) {
            Ok(t) => t,
            Err(e) => return writeln!(out, "error: cannot trace the game: {e}"),
        };
        let instrs: Vec<u64> = armed.iter().map(|(i, _)| *i).collect();
        let threads = tracer.arm_execute(&instrs);
        writeln!(out, "watching {} instructions on {threads} threads for up to {secs} s", instrs.len())?;
        out.flush()?;
        let start = Instant::now();
        let mut noted = Ok(());
        tracer.watch_until(
            || start.elapsed().as_secs_f64() >= secs || frontend_spoke(),
            |hit| {
                let Some((_, site)) = armed.get(hit.slot) else { return true };
                let addr = trace::reg(&hit.regs, site.base).wrapping_add(site.disp as u64);
                if low(addr) == low(target) {
                    if own.is_none() {
                        own = around(target);
                    }
                    return true;
                }
                if places.iter().any(|(a, _)| *a == addr) {
                    return true;
                }
                places.push((addr, around(addr)));
                noted = writeln!(out, "note others {}", places.len()).and_then(|_| out.flush());
                places.len() < OTHERS_MAX
            },
        );
        noted?;
    }
    let plausible = |v: f64| !matches!(kind, Kind::F32 | Kind::F64) || v == 0.0 || (1e-9..1e15).contains(&v.abs());
    places.retain(|(a, _)| s.read(*a, kind).is_some_and(plausible));
    // Words as first seen and now, the value's first.
    let Some(own_now) = around(target) else {
        return writeln!(out, "error: the value at 0x{target:x} isn't readable");
    };
    let mut snaps = vec![(own.unwrap_or_else(|| own_now.clone()), own_now)];
    places.retain(|(a, seen)| match around(*a) {
        Some(now) => {
            snaps.push((seen.clone().unwrap_or_else(|| now.clone()), now));
            true
        }
        None => false,
    });
    let word = |b: &[u8], off: i64| {
        let i = (KEY_REACH + off) as usize;
        i32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
    };
    let size = kind.size() as i64;
    let mut offsets: Vec<i64> = (-KEY_REACH..=KEY_REACH).step_by(4).filter(|&o| o <= -4 || o >= size).collect();
    // Nearest first (an index right next to the value), after the key it was saved with.
    offsets.sort_by_key(|&o| (Some(o) != given.map(|k| k.0), o.abs(), o < 0));
    let key = (!places.is_empty())
        .then(|| {
            offsets.into_iter().find(|&off| {
                let Some(mut v): Option<Vec<i32>> = snaps.iter().map(|(seen, now)| (word(seen, off) == word(now, off)).then(|| word(now, off))).collect()
                else {
                    return false;
                };
                let ids = v.iter().all(|n| (0..=0xFFFF).contains(n));
                v.sort_unstable();
                ids && v.windows(2).all(|w| w[0] != w[1])
            })
        })
        .flatten();
    match key {
        Some(off) => writeln!(out, "key {off} {}", word(&snaps[0].1, off))?,
        None => writeln!(out, "key none")?,
    }
    for ((a, _), (_, now)) in places.iter().zip(&snaps[1..]) {
        writeln!(out, "other {} {}", s.describe(*a, kind), key.map_or("-".to_owned(), |off| word(now, off).to_string()))?;
    }
    writeln!(out, "{} others", places.len())
}

/// ptrscan <hex addr[:type]> [depth] [max offset, hex] [max paths] [loose]: pointer paths to the
/// address, best first, as lines "path <module>+<offset>,<offset>,..."; "loose": also paths that
/// step from one object into the next, without trying only the others first.
fn cmd_ptrscan(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let mut it = arg.split_whitespace();
    let (Some((target, _)), Some(mem)) = (it.next().and_then(parse_loc), s.mem.as_ref()) else {
        return writeln!(out, "error: usage: ptrscan <hex addr[:type]> [depth] [max offset] [max paths] [loose] (after attach)");
    };
    let depth = it.next().and_then(|v| v.parse().ok()).unwrap_or(5);
    let max_off = it.next().and_then(parse_addr).unwrap_or(0x1000);
    let max_paths = it.next().and_then(|v| v.parse().ok()).unwrap_or(200);
    let loose = it.next() == Some("loose");
    let t = Instant::now();
    let collected = s.pointer_map.take().filter(|(at, _)| at.elapsed() < Duration::from_secs(60)).map(|(_, c)| c);
    let r = pointers::scan(s.pid, mem, s.width, &s.exe, target, depth, max_off, max_paths, collected, loose)?;
    for p in &r.paths {
        writeln!(out, "path {}", p.text())?;
    }
    let levels: Vec<String> = r.levels.iter().map(|n| n.to_string()).collect();
    writeln!(
        out,
        "{} paths ({} from {}, {} from the main thread's stack; {} more that step from one object into the next more than {} times dropped), {} pointers in {} MiB, addresses per step back: {}, in {} ms",
        r.paths.len(),
        r.paths.iter().filter(|p| p.module.eq_ignore_ascii_case(&s.exe)).count(),
        s.exe,
        r.paths.iter().filter(|p| p.module == pointers::STACK).count(),
        r.dropped,
        r.crossings,
        r.pointers,
        r.bytes >> 20,
        levels.join(" "),
        t.elapsed().as_millis()
    )
}

/// follow <type> <path>...: where each path leads now ("<n>: 0x<addr>:<type> = <value>" or
/// "<n>: broken"), then the address most of them agree on: "best 0x<addr>:<type> = <value>",
/// and "votes <paths leading there> <most leading elsewhere> <paths> clear|unclear".
/// "at=<addr>", "left=<addr>,..." and "came=<hex>" before the paths: where the value was last found
/// in this run, the places it moved away from, and which paths led to it then (path i is bit i % 4
/// of hex digit i / 4; see `pointers::vote`).
fn cmd_follow(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let mut it = arg.split_whitespace().peekable();
    let kind = it.next().and_then(Kind::parse);
    let (mut at, mut left, mut came) = (0, Vec::new(), Vec::new());
    while let Some(t) = it.peek() {
        if let Some(a) = t.strip_prefix("at=") {
            at = parse_addr(a).unwrap_or(0);
        } else if let Some(l) = t.strip_prefix("left=") {
            left = l.split(',').filter_map(parse_addr).collect();
        } else if let Some(c) = t.strip_prefix("came=") {
            let digits = c.chars().map(|d| d.to_digit(16).unwrap_or(0));
            came = digits.flat_map(|d| (0..4).map(move |b| d >> b & 1 == 1)).collect();
        } else {
            break;
        }
        it.next();
    }
    let paths: Option<Vec<PtrPath>> = it.map(PtrPath::parse).collect();
    let (Some(kind), Some(paths), Some(mem)) = (kind, paths, s.mem.as_ref()) else {
        return writeln!(out, "error: usage: follow <type> <module+offset,offset,...>... (after attach)");
    };
    let mods = pointers::modules(s.pid, mem);
    for (i, p) in paths.iter().enumerate() {
        match p.follow(mem, &mods, s.width) {
            Some(addr) => writeln!(out, "{i}: {}", s.describe(addr, kind))?,
            None => writeln!(out, "{i}: broken")?,
        }
    }
    match pointers::vote(mem, &mods, s.width, &paths, at, &left, &came) {
        Some(v) => {
            writeln!(out, "best {}", s.describe(v.addr, kind))?;
            let clear = if v.clear() { "clear" } else { "unclear" };
            writeln!(out, "votes {} {} {} {clear}", v.agree(), v.next, paths.len())
        }
        None => writeln!(out, "none of the paths lead anywhere now"),
    }
}

/// names <hex addr[:type]> [unreal]: named paths that lead to the value now, best first ("unreal": only
/// the engine's own: Unreal objects and Unity classes, no tracing or pointer map needed): "named <path>
/// <places it leads to> <what it means>" (see names.rs), then a summary line.
fn cmd_names(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let (addr, only_unreal) = match arg.trim().strip_suffix(" unreal") {
        Some(a) => (a, true),
        None => (arg.trim(), false),
    };
    let (Some((target, _)), Some(mem)) = (parse_loc(addr), s.mem.as_ref()) else {
        return writeln!(out, "error: usage: names <hex addr[:type]> [unreal] (after attach)");
    };
    let t = Instant::now();
    let heap = Heap::new(s.pid, mem, s.width)?;
    // A property of an Unreal object is named by the objects and properties leading to it.
    if let Ok(ue) = unreal(s.pid, mem) {
        let found = ue.discover(mem, target);
        for (p, leads) in found.iter().take(5) {
            writeln!(out, "named {} {} {}", p.text(), leads.len(), p.describe())?;
        }
        if !found.is_empty() {
            return writeln!(out, "{} named paths in {} ms", found.len().min(5), t.elapsed().as_millis());
        }
    }
    // A field of a Unity (Mono or IL2CPP) class's only live object: by the class's name, past the
    // objects the game replaces (a checkpoint's new player).
    if let Some((p, leads)) = mono::discover(&heap, target) {
        // A List's slot (Valheim's `m_inventory._items[21]`, an item with no id field) leads to
        // another item once an earlier one is gone: a Unity-style path picking the element by
        // its name (`="$item_wood"`, every stack of it) goes first when there is one; Mono's
        // fields tell which of those read the element itself.
        let mut found = Vec::new();
        if p.picks_list_slot() {
            let collected = pointers::collect_pointers(s.pid, mem, s.width)?;
            found = names::discover_where(&heap, &collected.0, target, |np| {
                let chain = match np.steps.last() {
                    Some(names::Step::Named(chain, _)) => &chain[..],
                    _ => &[],
                };
                mono::own_fields_along(&heap, target, np.value, chain) != Some(false)
            });
            for (p, leads) in &found {
                writeln!(out, "named {} {} {}", p.text(), leads.len(), p.describe())?;
            }
        }
        writeln!(out, "named {} {} {}", p.text(), leads.len(), p.describe())?;
        return writeln!(out, "{} named paths in {} ms", found.len() + 1, t.elapsed().as_millis());
    }
    // A variable of a GameMaker instance or of its global object.
    if s.doubles_only {
        if let Some((p, leads)) = gml::discover(&heap, program_of(s.pid).as_deref(), target) {
            writeln!(out, "named {} {} {}", p.text(), leads.len(), p.describe())?;
            return writeln!(out, "1 named paths in {} ms", t.elapsed().as_millis());
        }
    }
    // A number of an RPG Maker MV or MZ game, by its path from the game's globals.
    if is_rpgmaker(s.pid) {
        if let Some((p, leads)) = js::discover(&heap, target) {
            writeln!(out, "named {} {} {}", p.text(), leads.len(), p.describe())?;
            return writeln!(out, "1 named paths in {} ms", t.elapsed().as_millis());
        }
    }
    // A field of a Java game's objects, by its path from a class's static field.
    if s.java {
        if let Some((p, leads)) = java::discover(&heap, target) {
            writeln!(out, "named {} {} {}", p.text(), leads.len(), p.describe())?;
            return writeln!(out, "1 named paths in {} ms", t.elapsed().as_millis());
        }
        return writeln!(out, "0 named paths in {} ms", t.elapsed().as_millis());
    }
    // A field of an AIR game's ActionScript objects, by its path from a class's static field.
    if is_air(s.pid) {
        if let Some((p, leads)) = air::discover(&heap, target) {
            writeln!(out, "named {} {} {}", p.text(), leads.len(), p.describe())?;
            return writeln!(out, "1 named paths in {} ms", t.elapsed().as_millis());
        }
    }
    // A number of a Python game, by its namespace and the names from there (its slot).
    if is_python_game(s.pid, &s.exe) {
        if let Some((p, leads)) = python::discover(&heap, target) {
            writeln!(out, "named {} {} {}", p.text(), leads.len(), p.describe())?;
            return writeln!(out, "1 named paths in {} ms", t.elapsed().as_millis());
        }
    }
    // A number in a Lua game's tables, by its path from the globals table.
    if is_lua(s.pid) {
        if let Some((p, leads)) = lua::discover(&heap, target) {
            writeln!(out, "named {} {} {}", p.text(), leads.len(), p.describe())?;
            return writeln!(out, "1 named paths in {} ms", t.elapsed().as_millis());
        }
    }
    if only_unreal {
        return writeln!(out, "0 named paths in {} ms", t.elapsed().as_millis());
    }
    // An entry of a Godot dictionary is named by its key and its dictionary's other entries
    // (no pointer map needed).
    if let Some((p, leads)) = godot::discover(&heap, target) {
        writeln!(out, "named {} {} {}", p.text(), leads.len(), p.describe())?;
        return writeln!(out, "1 named paths in {} ms", t.elapsed().as_millis());
    }
    // A variable of a Godot script is named by the script's path and the variables leading to
    // it.
    if let Some((p, leads)) = gdscript::discover(&heap, target) {
        writeln!(out, "named {} {} {}", p.text(), leads.len(), p.describe())?;
        return writeln!(out, "1 named paths in {} ms", t.elapsed().as_millis());
    }
    // Godot's memory isn't laid out like Unity's: bytes there that look like a Unity string
    // made a path through "lor" in Brotato, which led nowhere after the next wave.
    if is_godot(s.pid) {
        return writeln!(out, "0 named paths in {} ms", t.elapsed().as_millis());
    }
    let collected = pointers::collect_pointers(s.pid, mem, s.width)?;
    let found = names::discover(&heap, &collected.0, target);
    for (p, leads) in &found {
        writeln!(out, "named {} {} {}", p.text(), leads.len(), p.describe())?;
    }
    writeln!(out, "{} named paths in {} ms", found.len(), t.elapsed().as_millis())?;
    if found.is_empty() {
        s.pointer_map = Some((Instant::now(), collected));
    }
    Ok(())
}

/// Time between searches for a named path's objects when it leads nowhere (each reads all of
/// the game's memory twice), and longer once a search found the same objects again (the game
/// has none of the item right now).
const NAMED_REFIND: Duration = Duration::from_secs(10);
const NAMED_REFIND_SCRIPT: Duration = Duration::from_secs(2);
/// Godot objects once the scene tree is known: a few pointer reads (searches of memory still
/// wait `NAMED_REFIND_SCRIPT`, gdscript.rs keeps their results that long).
const NAMED_REFIND_TREE: Duration = Duration::from_millis(250);
const NAMED_UNCHANGED: Duration = Duration::from_secs(60);

/// When to count a search for a named path's objects as done, so that the next one waits:
/// in the future when it found the same objects as the last one (none at all: soon, a save
/// may be loading).
fn named_found_at(old: &[u64], new: &[u64]) -> Instant {
    if !new.is_empty() && old == new {
        Instant::now() + (NAMED_UNCHANGED - NAMED_REFIND)
    } else {
        Instant::now()
    }
}

/// Where a named path leads, from the objects found for it before when it still leads anywhere
/// from them, else from a new search (at most every `NAMED_REFIND`).
fn named_walk(heap: &Heap, path: &Named, roots: &mut Vec<u64>, found_at: &mut Option<Instant>) -> Vec<u64> {
    let leads = path.walk(heap, roots);
    // A Godot script's objects are found again in the scene tree, or in one pass once its
    // script is known (Brotato makes a new player every wave, and health was lost for up to
    // 10 s after the shop).
    let every = match path {
        Named::Script(_) if gdscript::knows_tree(heap.pid) => NAMED_REFIND_TREE,
        Named::Script(_) => NAMED_REFIND_SCRIPT,
        _ => NAMED_REFIND,
    };
    if !leads.is_empty() || found_at.is_some_and(|t| t.elapsed() < every) {
        return leads;
    }
    let new = path.find_roots(heap);
    *found_at = Some(named_found_at(roots, &new));
    *roots = new;
    path.walk(heap, roots)
}

/// named <type> <named path>: "doubtful <why>" when the places aren't one value (never written
/// then), every place it leads now ("0x<addr>:<type> = <value>"), then "named <count>".
fn cmd_named(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let mut it = arg.split_whitespace();
    let kind = it.next().and_then(Kind::parse);
    let text = it.next().unwrap_or_default();
    let (Some(kind), Some(path), Some(mem)) = (kind, Named::parse(text), s.mem.as_ref()) else {
        return writeln!(out, "error: usage: named <type> <named path> (after attach)");
    };
    let heap = Heap::new(s.pid, mem, s.width)?;
    let (mut roots, mut found_at) = s.named_roots.remove(text).unwrap_or_default();
    let leads = named_walk(&heap, &path, &mut roots, &mut found_at);
    s.named_roots.insert(text.to_owned(), (roots, found_at));
    if let Some(why) = named_doubt(&heap, &leads, kind, &path) {
        writeln!(out, "doubtful {why}")?;
    }
    for a in &leads {
        writeln!(out, "{}", s.describe(*a, kind))?;
    }
    writeln!(out, "named {}", leads.len())
}

/// ue [objects <text> | class <name> | dump <addr> [struct]]: the game's Unreal objects.
fn cmd_ue(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let t = Instant::now();
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    match unreal(s.pid, mem) {
        Ok(ue) => ue.command(out, mem, arg)?,
        Err(e) => return writeln!(out, "error: {e}"),
    }
    writeln!(out, "in {} ms", t.elapsed().as_millis())
}

/// player: the player's numbers by name (`Ue::player`, the Browse tab): an "object
/// <role>\t<owner or ->\t<class>\t<main|more>\t<name>" line (more: shown on asking for everything), then its "field 0x<addr>:<type> = <value>\t<label>"
/// lines, for each object; "none character" while the player has no character (a menu; an RPG
/// Maker game that hasn't made its objects yet), "none engine <why>" for games whose objects
/// Ferret can't read so (not Unreal, Unity, Godot or RPG Maker MV/MZ).
fn cmd_player(out: &mut impl Write, s: &Session) -> io::Result<()> {
    let t = Instant::now();
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let list = match unreal(s.pid, mem) {
        Ok(ue) => match ue.player(mem) {
            Ok(list) => list,
            Err(_) => return writeln!(out, "none character"),
        },
        // Unity: every live object of the game's own classes; Godot: every node running a
        // script; RPG Maker MV/MZ: the party, heroes and variables. The player's first.
        Err(e) => {
            let heap = Heap::new(s.pid, mem, s.width)?;
            if s.doubles_only {
                return match gml::player(&heap, program_of(s.pid).as_deref()) {
                    Some(list) => print_player(out, s, &list, t),
                    None => writeln!(out, "none engine GameMaker objects not found (only GameMaker Studio 2 bytecode games before 2.3 are read)"),
                };
            }
            if is_rpgmaker(s.pid) {
                return match js::player(&heap) {
                    Some(list) => print_player(out, s, &list, t),
                    None => writeln!(out, "none character"),
                };
            }
            if s.java {
                return match java::player(&heap) {
                    Some(list) => print_player(out, s, &list, t),
                    None => writeln!(out, "none engine Java classes not found (only HotSpot JVMs are read)"),
                };
            }
            if is_air(s.pid) {
                return match air::player(&heap) {
                    Some(list) => print_player(out, s, &list, t),
                    None => writeln!(out, "none engine ActionScript objects not found (only 32-bit AIR runtimes are read)"),
                };
            }
            if is_python_game(s.pid, &s.exe) && !mono::runs_csharp(s.pid) && !is_godot(s.pid) {
                return match python::player(&heap) {
                    Some(list) => print_player(out, s, &list, t),
                    None => writeln!(out, "none engine Python objects not found (only 64-bit CPython 2.7 and 3.x)"),
                };
            }
            if is_lua(s.pid) && !mono::runs_csharp(s.pid) && !is_godot(s.pid) {
                return match lua::player(&heap) {
                    Some(list) => print_player(out, s, &list, t),
                    None => writeln!(out, "none engine Lua globals not found (only LuaJIT games are read)"),
                };
            }
            let found = match is_godot(s.pid) {
                true => gdscript::player(&heap),
                false => mono::player(&heap),
            };
            match found {
                Some(list) => list,
                None => return writeln!(out, "none engine {e}"),
            }
        }
    };
    print_player(out, s, &list, t)
}

fn print_player(out: &mut impl Write, s: &Session, list: &[ue::ListedObject], t: Instant) -> io::Result<()> {
    let mut n = 0;
    for o in list {
        let shown = if o.more { "more" } else { "main" };
        writeln!(out, "object {}\t{}\t{}\t{}\t{}", o.role, o.owner.as_deref().unwrap_or("-"), o.class, shown, o.name)?;
        for f in &o.fields {
            writeln!(out, "field {}\t{}", s.describe(f.addr, Kind::parse(f.kind).unwrap_or(Kind::I32)), f.label)?;
            n += 1;
        }
    }
    writeln!(out, "{} objects, {n} numbers in {} ms", list.len(), t.elapsed().as_millis())
}

/// browsable: "browsable unreal|unity|godot|rpgmaker|gamemaker|lua|java|python|air" when `player` can list the game's numbers, else "browsable no".
fn cmd_browsable(out: &mut impl Write, s: &Session) -> io::Result<()> {
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let engine = match unreal(s.pid, mem) {
        Ok(_) => "unreal",
        Err(_) if is_godot(s.pid) => "godot",
        Err(_) if is_rpgmaker(s.pid) => "rpgmaker",
        Err(_) if s.doubles_only && gml::is_runner(s.pid, program_of(s.pid).as_deref()) => "gamemaker",
        Err(_) if mono::runs_csharp(s.pid) => "unity",
        Err(_) if is_air(s.pid) && s.width == 4 => "air",
        Err(_) if is_python_game(s.pid, &s.exe) => "python",
        Err(_) if is_lua(s.pid) => "lua",
        Err(_) if s.java && java::shared(s.pid, mem).is_some() => "java",
        Err(_) => "no",
    };
    writeln!(out, "browsable {engine}")
}

/// jvm [class]: a Java game's JVM as Ferret reads it, or a class's fields (`java::diagnose`).
fn cmd_jvm(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let t = Instant::now();
    let heap = Heap::new(s.pid, mem, s.width)?;
    for l in java::diagnose(&heap, arg.trim()) {
        writeln!(out, "{l}")?;
    }
    writeln!(out, "in {} ms", t.elapsed().as_millis())
}

/// airdump [class[#path]]: an AIR game's VM as Ferret reads it, a class's statics or the object
/// at a path (`air::diagnose`).
fn cmd_airdump(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let t = Instant::now();
    let heap = Heap::new(s.pid, mem, s.width)?;
    for l in air::diagnose(&heap, arg.trim()) {
        writeln!(out, "{l}")?;
    }
    writeln!(out, "in {} ms", t.elapsed().as_millis())
}

/// pydump [path]: a Python game's interpreter as Ferret reads it, or a namespace's or object's
/// entries at a path (`python::diagnose`).
fn cmd_pydump(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let t = Instant::now();
    let heap = Heap::new(s.pid, mem, s.width)?;
    for l in python::diagnose(&heap, arg.trim()) {
        writeln!(out, "{l}")?;
    }
    writeln!(out, "in {} ms", t.elapsed().as_millis())
}

/// jsdump [global]: an RPG Maker MV or MZ game's V8 layout and a global's fields ($gameParty).
fn cmd_jsdump(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let t = Instant::now();
    let heap = Heap::new(s.pid, mem, s.width)?;
    for l in js::diagnose(&heap, arg.trim()) {
        writeln!(out, "{l}")?;
    }
    writeln!(out, "in {} ms", t.elapsed().as_millis())
}

/// luadump [path]: a Lua game's layout and globals table, or the table (or a function's
/// upvalues) at a path from it (`lua::diagnose`).
fn cmd_luadump(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let t = Instant::now();
    let heap = Heap::new(s.pid, mem, s.width)?;
    for l in lua::diagnose(&heap, arg.trim()) {
        writeln!(out, "{l}")?;
    }
    writeln!(out, "in {} ms", t.elapsed().as_millis())
}

/// gml: what a GameMaker game's variable maps look like (`gml::diagnose`).
fn cmd_gml(out: &mut impl Write, s: &Session) -> io::Result<()> {
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let t = Instant::now();
    let heap = Heap::new(s.pid, mem, s.width)?;
    for l in gml::diagnose(&heap, program_of(s.pid).as_deref()) {
        writeln!(out, "{l}")?;
    }
    writeln!(out, "in {} ms", t.elapsed().as_millis())
}

/// gdtree: a Godot game's scene tree, a node a line with its script (after a script path was
/// found or followed).
fn cmd_gdtree(out: &mut impl Write, s: &Session) -> io::Result<()> {
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let heap = Heap::new(s.pid, mem, s.width)?;
    match gdscript::tree_text(&heap) {
        Some(lines) => lines.iter().try_for_each(|l| writeln!(out, "{l}")),
        None => writeln!(out, "error: not known to be a Godot game yet (find or follow a script variable first)"),
    }
}

/// resolve <site> [type] [seconds]: "0x<addr>:<type> = <value>", then "via +<static>,<disp>"
/// when the object comes from a static pointer (a path to follow for the rest of this run).
fn cmd_resolve(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    if s.java {
        return writeln!(out, "error: {JAVA}");
    }
    let (key, f) = take_key(&arg.split_whitespace().collect::<Vec<_>>());
    let (Some(site), Some(mem)) = (Site::parse(&f).map(|s| Site { key, ..s }), s.mem.as_ref()) else {
        return writeln!(out, "error: usage: resolve <pattern> <offset> <register> <displacement> [type] [seconds] [key=<offset>:<number>]");
    };
    let kind = f.get(4..).unwrap_or_default().iter().find_map(|v| Kind::parse(v)).unwrap_or(Kind::I32);
    let secs = f.get(4..).unwrap_or_default().iter().find_map(|v| v.parse().ok()).unwrap_or(10);
    match resolve_site(s.pid, mem, &site, s.width, Duration::from_secs(secs)) {
        Ok(Resolved { addr, holder, .. }) => match s.read(addr, kind) {
            Some(_) => {
                writeln!(out, "{}", s.describe(addr, kind))?;
                match holder {
                    Some(h) => writeln!(out, "via {}", h.text()),
                    None => Ok(()),
                }
            }
            None => writeln!(out, "error: resolved to unreadable 0x{addr:x}"),
        },
        Err(e) => writeln!(out, "error: {e}"),
    }
}

pub fn run(as_user: Option<&str>) {
    os::ignore_interrupt();
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    // Before anything else runs, threads included.
    if let Err(e) = os::drop_root(as_user) {
        writeln!(out, "error: cannot drop root: {e}").and_then(|_| writeln!(out, "end")).ok();
        return;
    }
    trace::block_sigchld();
    os::on_terminate(exit_untraced);
    let mut session = Session::default();
    let limiter = SharedLimiter::default();
    // Anti-cheat that started in the attached game, until `alive` reports it.
    let mut caught: Option<Refusal> = None;
    {
        let limiter = limiter.clone();
        std::thread::spawn(move || limiter_loop(limiter));
    }
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let (cmd, arg) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        let arg = arg.trim();
        // Ends a `sites ... wait` (or `first`, `others`) early; one arriving after it ended on its own changes nothing,
        // and gets no reply (the frontend doesn't wait for one).
        if cmd == "cancel" {
            continue;
        }
        // Anti-cheat started in the game: nothing more is written to it, test values included.
        if let Some((pid, r)) = CAUGHT.lock().unwrap().take() {
            if pid == session.pid {
                session = Session::default();
                caught = Some(r);
            }
        }
        if cmd == "attach" {
            caught = None;
        }
        let res = match cmd {
            "info" => os::info(&mut out),
            "ps" => cmd_ps(&mut out, arg),
            "games" => cmd_games(&mut out),
            "attach" => cmd_attach(&mut out, &mut session, &limiter, arg),
            "scan" => cmd_scan(&mut out, &mut session, arg),
            "next" => cmd_next(&mut out, &mut session, arg),
            "mark" => cmd_mark(&mut out, &mut session, arg),
            "snap" => cmd_snap(&mut out, &mut session, arg),
            "phase" => cmd_phase(&mut out, &mut session, arg),
            "has" => cmd_has(&mut out, &session, arg),
            "undo" => cmd_undo(&mut out, &mut session),
            "redo" => cmd_redo(&mut out, &mut session),
            "list" => cmd_list(&mut out, &session, arg),
            "write" => cmd_write(&mut out, &mut session, arg),
            "set" => cmd_set(&mut out, &session, arg),
            "peek" => cmd_peek(&mut out, &session, arg),
            "keep" => cmd_keep(&mut out, &mut session, arg),
            "drop" => cmd_drop(&mut out, &mut session, arg),
            "alive" => match caught.take() {
                // "alive anti-cheat <name>" or "alive anti-cheat-elsewhere <name>".
                Some(r) => writeln!(out, "alive {}", r.code()),
                None => cmd_alive(&mut out, &session),
            },
            "about" => cmd_about(&mut out, &mut session, arg),
            "shape" => cmd_shape(&mut out, &session, arg),
            "shapes" => cmd_shapes(&mut out, &mut session, arg),
            "track" => cmd_track(&mut out, &mut session, arg),
            "sites" => cmd_sites(&mut out, &session, arg),
            "first" => cmd_first(&mut out, &session, arg),
            "build" => cmd_build(&mut out, &session),
            "resolve" => cmd_resolve(&mut out, &mut session, arg),
            "resolve-all" => cmd_resolve_all(&mut out, &mut session, arg),
            "others" => cmd_others(&mut out, &session, arg),
            "ptrscan" => cmd_ptrscan(&mut out, &mut session, arg),
            "names" => cmd_names(&mut out, &mut session, arg),
            "named" => cmd_named(&mut out, &mut session, arg),
            "ue" => cmd_ue(&mut out, &session, arg),
            "player" => cmd_player(&mut out, &session),
            "browsable" => cmd_browsable(&mut out, &session),
            "gdtree" => cmd_gdtree(&mut out, &session),
            "gml" => cmd_gml(&mut out, &session),
            "luadump" => cmd_luadump(&mut out, &session, arg),
            "jvm" => cmd_jvm(&mut out, &session, arg),
            "pydump" => cmd_pydump(&mut out, &session, arg),
            "jsdump" => cmd_jsdump(&mut out, &session, arg),
            "airdump" => cmd_airdump(&mut out, &session, arg),
            "follow" => cmd_follow(&mut out, &session, arg),
            "limit" => cmd_limit(&mut out, &limiter, arg),
            "unlimit" => cmd_unlimit(&mut out, &limiter, arg),
            "switch" => cmd_switch(&mut out, &limiter, arg),
            "limits" => cmd_limits(&mut out, &limiter),
            _ => writeln!(
                out,
                "commands: sandbox, info, ps [filter], games, attach <pid>, scan <n> [i32,f32,f64,xor,u16] [all], mark, snap <file> [types], next <n>|+|-|=|!|bar <full before> <full now> <error>, undo, redo, list [n], peek <addr>..., keep <addr>, drop <addr>, about <addr>..., alive, shape <addr>, shapes <shape>; ..., track <addr>..., sites <addr> [seconds] [wait], cancel, resolve <site> [type], others <seconds> <addr> <sites>, ptrscan <addr> [depth] [max offset] [max paths] [loose], names <addr> [unreal], named <type> <named path>, ue [objects <text>|class <name>|dump <addr>], gdtree, follow <type> <path>..., limit <name> <addr> <min> <max> <sites>, unlimit <name>, switch <name|*> <on|off|toggle>, limits, write <addr> <n> [test], set <n>, quit (addresses: <hex>[:i32|f32|f64|xor|u16])"
            ),
        };
        // A command that failed (the game quit: its /proc files are gone) says so; only losing
        // the frontend ends the helper.
        let replied = match res {
            Ok(()) => Ok(()),
            Err(e) => writeln!(out, "error: {e}"),
        };
        if replied.and_then(|_| writeln!(out, "end")).and_then(|_| out.flush()).is_err() {
            break;
        }
    }
    put_back_tests(&mut session);
    exit_untraced();
}

/// Ends the helper once no trace is running: a running one stops early, and no other starts.
/// Leaving in the middle of one leaves its hardware breakpoints on the game's threads (the
/// kernel detaches them but keeps their debug registers), and the game dies of SIGTRAP (on
/// Windows, an unhandled single step) the next time it runs that code: Minecraft Dungeons quit
/// 9 s after a helper was stopped during a limiter re-check; the stand-in dies the same way,
/// whether the helper is killed or its stdin ends (Ferret closing).
fn exit_untraced() -> ! {
    trace::stop_all();
    let _no_more = TRACING.lock().unwrap_or_else(|e| e.into_inner());
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::{dosbox_game, heap_block, parse_pattern, run_game, udk_game, wine_launcher, Patterns};

    #[test]
    fn finds_the_heap_block_around_a_value() {
        let key = 0x1600_f8f8_08d5_05e1u64;
        // A Windows heap header: size and size before in 16-byte units, in use or free.
        let header = |size: u16, used: bool, before: u16| {
            let [s0, s1] = size.to_le_bytes();
            let [b0, b1] = before.to_le_bytes();
            let flags = used as u8;
            u64::from_le_bytes([s0, s1, flags, s0 ^ s1 ^ flags, b0, b1, 0, 0x18]) ^ key
        };
        // A record's block (data at 0x1010, 0x70 bytes with its header), then the next one's.
        let mem = [(0x1008, header(7, true, 3)), (0x1078, header(5, true, 7)), (0x1030, 0x1234_5678), (0x1060, 0xdead_beef)];
        let read = |a: u64| Some(mem.iter().find(|(at, _)| *at == a).map_or(0, |(_, v)| *v));
        assert_eq!(heap_block(read, key, 0x1040), Some((0x1010, 0x1078)));
        assert_eq!(heap_block(read, key, 0x1010), Some((0x1010, 0x1078)));
        // The next block's header isn't in it, and another heap's key finds nothing.
        assert_eq!(heap_block(read, key, 0x1078), None);
        assert_eq!(heap_block(read, 0x0000_af93_e9b8_2212, 0x1040), None);
        // A next header that doesn't name this block's size: not a heap block after all.
        let mem = [(0x1008, header(7, true, 3)), (0x1078, header(5, true, 6))];
        let read = |a: u64| Some(mem.iter().find(|(at, _)| *at == a).map_or(0, |(_, v)| *v));
        assert_eq!(heap_block(read, key, 0x1040), None);
    }

    #[test]
    fn finds_code_patterns_together() {
        let code = [0x90, 0x8b, 0x45, 0x10, 0xf3, 0x0f, 0x10, 0x85, 0xe8, 0x04, 0x00, 0x00, 0x8b, 0x45, 0x20, 0x8b, 0x45, 0x10];
        let a = parse_pattern("8b45??f30f").unwrap();
        let b = parse_pattern("??0f1085e8040000").unwrap();
        let c = parse_pattern("8b4510").unwrap();
        let none = parse_pattern("8b4530").unwrap();
        let pats: Vec<&[Option<u8>]> = vec![&a, &b, &c, &none];
        let mut found = vec![Vec::new(); pats.len()];
        Patterns::new(&pats).search(&code, 0x1000, &mut found, 2);
        assert_eq!(found, vec![vec![0x1001], vec![0x1004], vec![0x1001, 0x100f], vec![]]);
        // Up to the limit each.
        let mut found = vec![Vec::new(); pats.len()];
        Patterns::new(&pats).search(&code, 0x1000, &mut found, 1);
        assert_eq!(found[2], vec![0x1001]);
    }

    #[test]
    fn names_the_game_a_player_program_runs() {
        let game = |program: &str, args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            run_game(program, &args, || "/home/p/Games/Hyperplasm".into())
        };
        assert_eq!(game("ruffle", &["--fullscreen", "https://x.org/age_of_war.swf?v=2"]), Some("age_of_war.swf".into()));
        assert_eq!(game("love", &["/home/p/Games/itch/in-a-dark-cave/InADarkCave.love"]), Some("InADarkCave.love".into()));
        assert_eq!(game("love", &["."]), Some("Hyperplasm (LÖVE)".into()));
        assert_eq!(game("love", &["/home/p/Games/Mari0/"]), Some("Mari0 (LÖVE)".into()));
        assert_eq!(game("java", &["-Xmx1g", "-jar", "/home/p/Games/small ship/smallshipbigguns.jar"]), Some("smallshipbigguns.jar".into()));
        let aoc = ["-Xms256M", "-Ddirect=true", "-classpath", "/g/aoc.jar:/g/lib/*", "com.ageofconquest.app.user.aoc.Main", "-x"];
        assert_eq!(game("java", &aoc), Some("com.ageofconquest.app.user.aoc.Main".into()));
        assert_eq!(game("java", &["-p", "mods", "-m", "game/net.x.Main"]), Some("game/net.x.Main".into()));
        assert_eq!(game("java", &["-version"]), None);
        assert_eq!(game("mono", &["--debug", "Game.exe"]), Some("Game.exe".into()));
        assert_eq!(game("python3", &["-u", "frozen_escape.py"]), Some("frozen_escape.py".into()));
        assert_eq!(game("python3.11", &["/home/p/Games/ppu/src/main.py"]), Some("ppu (Python)".into()));
        assert_eq!(game("python3", &["main.py"]), Some("Hyperplasm (Python)".into()));
        assert_eq!(game("python3", &["-m", "gunship"]), Some("gunship".into()));
        assert_eq!(game("python3", &["/usr/bin/some-tool"]), None);
        assert_eq!(game("ue4game-win32-shipping.exe", &["..\\..\\..\\Zombielicious\\Zombielicious.uproject"]), Some("Zombielicious (Unreal)".into()));
        assert_eq!(game("ue4game.exe", &["../../../Boxing1/Boxing1.uproject", "-windowed"]), Some("Boxing1 (Unreal)".into()));
        assert_eq!(game("ue4game-win64-shipping.exe", &[]), None);
        let launcher = |args: &[&str]| wine_launcher(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        assert!(launcher(&["/home/p/.local/share/Steam/steamapps/common/Proton - Experimental/proton", "waitforexitandrun", "/g/Game.exe"]));
        assert!(launcher(&["/home/p/.steam/root/compatibilitytools.d/GE-Proton10-1/proton", "run", "/g/Game.exe"]));
        assert!(launcher(&["-u", "/usr/bin/umu-run", "/g/Game.exe"]));
        assert!(!launcher(&["/home/p/Games/ppu/src/main.py"]));
        assert_eq!(game("love", &[]), None);
        assert_eq!(game("SortTheCourt.x86", &["main.py"]), None);
    }

    #[test]
    fn names_a_udk_game_by_its_folder() {
        assert_eq!(udk_game("S:\\retrobikepark\\Binaries\\Win32\\UDK.exe"), Some("retrobikepark (UDK)".into()));
        assert_eq!(udk_game("C:/Games/Retro Bike/Binaries/Win64/UDK.exe"), Some("Retro Bike (UDK)".into()));
        assert_eq!(udk_game("D:\\Binaries\\Win32\\UDK.exe"), None);
        assert_eq!(udk_game("UDK.exe"), None);
    }

    #[test]
    fn names_a_dosbox_game_by_its_folder() {
        let home = "/home/p";
        let game = |argv0: &str, cwd: &str| dosbox_game(argv0, cwd, home);
        // GOG under Wine, Steam's own copy, GOG's Linux layout (relative), a game's folder.
        assert_eq!(game("S:\\Games\\Tyrian 2000\\Tyrian 2000\\DOSBOX\\dosbox.exe", "/x"), Some("Tyrian 2000".into()));
        assert_eq!(game("C:\\Steam\\steamapps\\common\\Doom\\dosbox\\DOSBox.exe", "/x"), Some("Doom".into()));
        assert_eq!(game("dosbox/dosbox", "/home/p/GOG Games/Tyrian 2000"), Some("Tyrian 2000".into()));
        assert_eq!(game("/home/p/Games/Ultima IV/dosbox", "/home/p"), Some("Ultima IV".into()));
        // Installed system-wide: the folder it runs in, never the home folder or a lone drive.
        assert_eq!(game("/usr/bin/dosbox", "/home/p/Games/Keen/data"), Some("Keen".into()));
        assert_eq!(game("dosbox", "/home/p/Games/Keen"), Some("Keen".into()));
        assert_eq!(game("/app/bin/dosbox", "/home/p"), None);
        assert_eq!(game("C:\\DOSBox\\dosbox.exe", "/x"), Some("x".into()));
    }
}
