// The part that runs on the host (started by the frontend through
// `flatpak-spawn --host`). Reads and writes game memory through /proc/<pid>/mem.
// Protocol: one command per line on stdin; each reply ends with a line "end".

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
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
use crate::trace;

pub struct Region {
    pub start: u64,
    pub end: u64,
    pub perms: String,
    pub path: String,
}

pub fn maps(pid: u32) -> io::Result<Vec<Region>> {
    let text = fs::read_to_string(format!("/proc/{pid}/maps"))?;
    Ok(text
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (s, e) = it.next()?.split_once('-')?;
            let perms = it.next()?.to_owned();
            let path = it.skip(3).collect::<Vec<_>>().join(" ");
            Some(Region {
                start: u64::from_str_radix(s, 16).ok()?,
                end: u64::from_str_radix(e, 16).ok()?,
                perms,
                path,
            })
        })
        .collect())
}

fn environ(pid: u32) -> HashMap<String, String> {
    fs::read(format!("/proc/{pid}/environ"))
        .unwrap_or_default()
        .split(|b| *b == 0)
        .filter_map(|kv| {
            let kv = String::from_utf8_lossy(kv);
            let (k, v) = kv.split_once('=')?;
            Some((k.to_owned(), v.to_owned()))
        })
        .collect()
}

/// The program as started (argv[0]); Wine paths have spaces ("E:\\Mass Effect Andromeda\\...").
fn argv0(pid: u32) -> String {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or_default()).into_owned())
        .unwrap_or_default()
}

fn cmdline(pid: u32) -> String {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            String::from_utf8_lossy(&b)
                .split('\0')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

/// File name of the program, also for Windows programs under Wine.
pub fn exe_name(pid: u32) -> String {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let first = raw.split(|b| *b == 0).next().unwrap_or_default();
    let first = String::from_utf8_lossy(first);
    first.rsplit(['/', '\\']).next().unwrap_or_default().to_owned()
}

/// GameMaker games keep every number as a double; they ship their assets as data.win (or
/// game.unx on Linux) next to the program.
fn is_gamemaker(pid: u32, exe: &str) -> bool {
    let Ok(regions) = maps(pid) else { return false };
    regions.iter().filter(|r| r.path.rsplit('/').next().is_some_and(|n| n.eq_ignore_ascii_case(exe))).any(|r| {
        let dir = std::path::Path::new(&r.path).with_file_name("");
        ["data.win", "game.unx", "assets/game.unx"].iter().any(|f| dir.join(f).exists())
    })
}

/// Godot games load <program>.pck from next to the program, or carry it at the end of the
/// program (which then ends with its magic, "GDPC").
fn is_godot(pid: u32) -> bool {
    let Some(program) = maps(pid).ok().and_then(|m| program_path(pid, &m)) else { return false };
    let root = Path::new("/proc").join(pid.to_string()).join("root");
    let inside = |p: &Path| root.join(p.strip_prefix("/").unwrap_or(p));
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

/// Anti-cheat loaded in the game, shipped in its folder, or known to its launcher or to
/// AreWeAntiCheatYet (VAC runs in the Steam client, others may load only when going online).
fn anti_cheat(pid: u32) -> Option<String> {
    let maps = maps(pid).ok()?;
    if let Some(ac) = maps.iter().find_map(|r| anticheat::in_path(&r.path)) {
        return Some(ac.to_owned());
    }
    let env = environ(pid);
    if let Some(ac) = about(&env).anti_cheat {
        return Some(ac);
    }
    // Seen from inside the game's sandbox (pressure-vessel, flatpak).
    let root = Path::new("/proc").join(pid.to_string()).join("root");
    let inside = |p: &Path| root.join(p.strip_prefix("/").unwrap_or(p));
    let folder = match env.get("STEAM_COMPAT_INSTALL_PATH") {
        Some(dir) => PathBuf::from(dir),
        None => anticheat::game_folder(&program_path(pid, &maps)?),
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
/// native programs are /proc/<pid>/exe.
fn program_path(pid: u32, maps: &[Region]) -> Option<PathBuf> {
    let exe = exe_name(pid);
    let mapped = maps.iter().map(|r| Path::new(&r.path)).find(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(&*exe)));
    match mapped {
        Some(p) => Some(p.to_path_buf()),
        None => fs::read_link(format!("/proc/{pid}/exe")).ok(),
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

/// A game its store lists as played only with other people.
fn online_only(pid: u32) -> bool {
    about(&environ(pid)).online_only
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

fn my_uid() -> u32 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|v| v.split_whitespace().next()?.parse().ok())
        })
        .unwrap_or(u32::MAX)
}

fn owner_uid(pid: u32) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(format!("/proc/{pid}")).ok().map(|m| m.uid())
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
}

impl Kind {
    const ALL: [Kind; 4] = [Kind::I32, Kind::F32, Kind::F64, Kind::Xor];

    fn name(self) -> &'static str {
        match self {
            Kind::I32 => "i32",
            Kind::F32 => "f32",
            Kind::F64 => "f64",
            Kind::Xor => "xor",
        }
    }

    fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == s)
    }

    fn size(self) -> usize {
        match self {
            Kind::I32 | Kind::F32 => 4,
            Kind::F64 | Kind::Xor => 8,
        }
    }

    fn decode(self, b: &[u8]) -> f64 {
        match self {
            Kind::I32 => i32::from_le_bytes(b[..4].try_into().unwrap()) as f64,
            Kind::F32 => f32::from_le_bytes(b[..4].try_into().unwrap()) as f64,
            Kind::F64 => f64::from_le_bytes(b[..8].try_into().unwrap()),
            Kind::Xor => {
                (i32::from_le_bytes(b[..4].try_into().unwrap()) ^ i32::from_le_bytes(b[4..8].try_into().unwrap())) as f64
            }
        }
    }

    /// Whether a value that went from `lo` to `hi` (either way round) meanwhile can be what the
    /// screen showed as `n`. Games show fractions rounded, cut off or rounded up (countdowns:
    /// Lumencraft's wave timer shows 6:37 with 396.4 s left); a decimal like 1.2 may be kept as
    /// a whole number of tenths (12).
    fn fits(self, lo: f64, hi: f64, n: Shown) -> bool {
        let (lo, hi) = (lo.min(hi), lo.max(hi));
        match self {
            Kind::I32 | Kind::Xor => lo <= n.scaled && n.scaled <= hi,
            Kind::F32 | Kind::F64 => lo < n.value + n.step && hi > n.value - n.step,
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

fn read_value(mem: &File, addr: u64, kind: Kind) -> Option<f64> {
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b[..kind.size()], addr).ok()?;
    Some(kind.decode(&b))
}

fn write_value(mem: &File, addr: u64, kind: Kind, v: f64) -> io::Result<()> {
    match kind {
        Kind::I32 => mem.write_all_at(&(v.round() as i32).to_le_bytes(), addr),
        Kind::F32 => mem.write_all_at(&(v as f32).to_le_bytes(), addr),
        Kind::F64 => mem.write_all_at(&v.to_le_bytes(), addr),
        Kind::Xor => {
            let mut key = [0u8; 4];
            mem.read_exact_at(&mut key, addr + 4)?;
            mem.write_all_at(&((v.round() as i32) ^ i32::from_le_bytes(key)).to_le_bytes(), addr)
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

pub fn scannable(r: &Region) -> bool {
    r.perms.starts_with("rw") && !matches!(r.path.as_str(), "[vvar]" | "[vvar_vclock]" | "[vsyscall]")
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
    /// A named path to every place the value is kept (all of an item's stacks): when there is
    /// one, all of them are found through it on every check and kept in range.
    named: Option<NamedLimit>,
    // First bytes of the object holding the value (its type pointer). If they
    // change, the object is gone and writing would corrupt unrelated memory.
    guard_addr: u64,
    guard: [u8; 4],
    fixes: u64,
    restores: u64,
    paused: Option<&'static str>,
    retry_at: Instant,
    /// When its code patterns last found it (the object may be replaced while the old one stays
    /// readable, which the guard can't see).
    checked_at: Instant,
    /// Whether all its code patterns were searched for a static pointer to the object.
    searched_code: bool,
}

/// A named path of any kind: through objects the game names (Unity, names.rs), to an entry of
/// the game's dictionaries (Godot, godot.rs), through its scripts' variables (Godot,
/// gdscript.rs), through its Unreal objects (ue.rs) or to a field of a Mono class's live
/// objects (Unity, mono.rs).
#[derive(Clone)]
enum Named {
    Objects(NamedPath),
    Dict(DictPath),
    Script(ScriptPath),
    Unreal(UePath),
    Mono(MonoPath),
}

impl Named {
    fn parse(text: &str) -> Option<Named> {
        if text.starts_with("ue:") {
            return UePath::parse(text).map(Named::Unreal);
        }
        if text.starts_with("gd:") {
            return ScriptPath::parse(text).map(Named::Script);
        }
        if text.starts_with("mono:") {
            return MonoPath::parse(text).map(Named::Mono);
        }
        match text.starts_with('{') {
            true => DictPath::parse(text).map(Named::Dict),
            false => NamedPath::parse(text).map(Named::Objects),
        }
    }

    fn is_named(text: &str) -> bool {
        text.starts_with('"') || text.starts_with('{') || text.starts_with("ue:") || text.starts_with("gd:") || text.starts_with("mono:")
    }

    /// Where it leads from the roots found before (no search).
    fn walk(&self, heap: &Heap, roots: &[u64]) -> Vec<u64> {
        match self {
            Named::Objects(p) => heap.walk(roots, p),
            Named::Dict(p) => godot::walk(heap, roots, p),
            Named::Script(p) => gdscript::walk(heap, roots, p),
            Named::Mono(p) => mono::walk(heap, roots, p),
            Named::Unreal(p) => unreal(heap.pid, heap.file()).map(|ue| ue.walk(heap.file(), roots, p)).unwrap_or_default(),
        }
    }

    /// Searches the game's memory for its roots (seconds; Unreal's object list: a fraction of one).
    fn find_roots(&self, heap: &Heap) -> Vec<u64> {
        match self {
            Named::Objects(p) => heap.find_roots(p),
            Named::Dict(p) => godot::find_roots(heap, p),
            Named::Script(p) => gdscript::find_roots(heap, p),
            Named::Mono(p) => mono::find_roots(heap, p),
            Named::Unreal(p) => unreal(heap.pid, heap.file()).map(|ue| ue.roots(heap.file(), p)).unwrap_or_default(),
        }
    }
}

/// The game's Unreal objects, when it's an Unreal game.
fn unreal(pid: u32, mem: &File) -> Result<std::sync::Arc<ue::Ue>, String> {
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
fn named_doubt(mem: &File, leads: &[u64], kind: Kind, path: &Named) -> Option<String> {
    if leads.len() > MAX_NAMED_PLACES {
        return Some(format!("it leads to {} places, more than one value is kept in", leads.len()));
    }
    // An Unreal path's places are the properties the game itself describes (a stack and a
    // tally of it may sit at different alignments).
    if !matches!(path, Named::Unreal(_)) && leads.iter().any(|a| a % 8 != leads[0] % 8) {
        return Some("its places don't line up like the same field of objects".into());
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
}

#[derive(Default)]
struct Limiter {
    pid: u32,
    mem: Option<File>,
    width: usize,
    modules: Vec<Module>,
    limits: Vec<Limit>,
}

type SharedLimiter = Arc<Mutex<Limiter>>;

const RETRY_EVERY: Duration = Duration::from_secs(5);
const RECHECK_EVERY: Duration = Duration::from_secs(10);
const RESOLVE_WAIT: Duration = Duration::from_secs(2);

fn read_guard(mem: &File, addr: u64) -> Option<[u8; 4]> {
    let mut g = [0u8; 4];
    mem.read_exact_at(&mut g, addr).ok().map(|_| g)
}

/// How often limits check their values (and write the ones out of range), and how often they
/// find where the values are (pointer paths, named paths). Lumencraft's lumen took a moment to
/// come back after a purchase.
const LIMIT_WRITE_EVERY: Duration = Duration::from_millis(50);
const LIMIT_FIND_EVERY: Duration = Duration::from_millis(250);

/// Keeps limited values in range; paused limits are found again through their
/// saved code patterns and then resume. Values found through code patterns are also found
/// again every 10 s, and through the static pointer their object comes from (when there is
/// one) on every check. A limit with no bounds only keeps the address current.
fn limiter_loop(shared: SharedLimiter) {
    let mut found_at = Instant::now() - LIMIT_FIND_EVERY;
    loop {
        std::thread::sleep(LIMIT_WRITE_EVERY);
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
                match pointers::vote(mem, modules, *width, &l.paths) {
                    Some(v) if !v.clear() => {
                        l.paused = Some("its pointer paths don't agree on where it is, not writing");
                        l.retry_at = now + Duration::from_secs(1);
                    }
                    Some(v) => {
                        if v.addr != l.addr || l.paused.is_some() {
                            l.restores += 1;
                        }
                        l.addr = v.addr;
                        l.paused = None;
                    }
                    None => {
                        l.paused = Some("its pointer paths lead nowhere right now, waiting");
                        l.retry_at = now + Duration::from_secs(1);
                    }
                }
            }
            let heap = (find && limits.iter().any(|l| l.named.is_some())).then(|| Heap::new(*pid, mem, *width).ok()).flatten();
            for l in limits.iter_mut() {
                let (Some(n), Some(heap)) = (l.named.as_mut(), heap.as_ref()) else { continue };
                // Searching memory for its objects again takes seconds: done without the lock.
                let leads = n.path.walk(heap, &n.roots);
                if leads.is_empty() {
                    l.paused = Some("it isn't anywhere right now (none in the game?), waiting");
                } else if named_doubt(mem, &leads, l.kind, &n.path).is_some() {
                    l.paused = Some("its name leads to places that aren't one value, not written");
                } else {
                    if leads != n.addrs || l.paused.is_some() {
                        l.restores += 1;
                    }
                    l.addr = leads[0];
                    l.paused = None;
                }
                n.addrs = leads;
            }
            named_due = limits
                .iter()
                .filter(|_| find)
                .filter_map(|l| {
                    let n = l.named.as_ref()?;
                    (n.addrs.is_empty() && n.found_at.is_none_or(|t| t.elapsed() >= NAMED_REFIND)).then(|| (l.name.clone(), n.path.clone()))
                })
                .collect();
            for l in limits.iter_mut().filter(|l| l.paused.is_none()) {
                if let Some(n) = &l.named {
                    for &addr in &n.addrs {
                        let Some(v) = read_value(mem, addr, l.kind) else { continue };
                        let target = match (l.min, l.max) {
                            (_, Some(max)) if v > max => max,
                            (Some(min), _) if v < min => min,
                            _ => continue,
                        };
                        if write_value(mem, addr, l.kind, target).is_ok() {
                            l.fixes += 1;
                        }
                    }
                    continue;
                }
                if l.paths.is_empty() && read_guard(mem, l.guard_addr) != Some(l.guard) {
                    l.paused = Some("the object holding it changed, finding it again");
                    l.retry_at = Instant::now();
                    continue;
                }
                let Some(v) = read_value(mem, l.addr, l.kind) else {
                    l.paused = Some("unreadable, finding it again");
                    l.retry_at = Instant::now();
                    continue;
                };
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
                .iter()
                .filter(|l| find && l.paths.is_empty() && l.named.is_none())
                .filter(|l| if l.paused.is_some() { l.retry_at <= now } else { l.checked_at + RECHECK_EVERY <= now })
                .map(|l| (l.name.clone(), l.sites.clone(), !l.searched_code))
                .collect();
            let Ok(mem) = mem.try_clone() else { continue };
            (*pid, mem, *width)
        };
        for (name, path) in named_due {
            let roots = Heap::new(pid, &mem, width).map(|h| path.find_roots(&h)).unwrap_or_default();
            let mut guard = shared.lock().unwrap();
            if guard.pid != pid {
                break;
            }
            if let Some(n) = guard.limits.iter_mut().find(|l| l.name == name).and_then(|l| l.named.as_mut()) {
                n.found_at = Some(named_found_at(&n.roots, &roots));
                n.roots = roots;
            }
        }
        // Resolving traces the game for a moment, so it runs without holding the lock.
        for (name, sites, search_code) in due {
            let mut found = sites
                .iter()
                .find_map(|site| Some((resolve_site(pid, &mem, site, width, RESOLVE_WAIT).ok()?, site.disp)));
            // The code that ran may not show where the object comes from; another site's may
            // (searching the code for every site is slow: once per value).
            if let Some((r, _)) = found.as_mut().filter(|(r, _)| r.holder.is_none() && search_code) {
                r.holder = holder_in_code(pid, &mem, &sites, r.is_64bit, width, r.addr);
            }
            let mut guard = shared.lock().unwrap();
            if guard.pid != pid {
                break;
            }
            let Some(l) = guard.limits.iter_mut().find(|l| l.name == name) else { continue };
            l.checked_at = Instant::now();
            l.searched_code |= search_code && found.is_some();
            let object = found.and_then(|(r, disp)| {
                let object = r.addr.wrapping_sub(disp as u64);
                Some((r, object, read_guard(&mem, object)?))
            });
            match object {
                Some((r, object, g)) => {
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
/// quote (see names.rs; it keeps every place it leads to in range). No bounds: only keeps the address
/// current (see `limiter_loop`).
fn cmd_limit(out: &mut impl Write, limiter: &SharedLimiter, arg: &str) -> io::Result<()> {
    let f: Vec<&str> = arg.split_whitespace().collect();
    let rest = f.get(4..).unwrap_or_default();
    let is_named = |s: &&&str| Named::is_named(s);
    let named: Option<Vec<Named>> = rest.iter().filter(is_named).map(|s| Named::parse(s)).collect();
    let rest: Vec<&str> = rest.iter().filter(|s| !is_named(s)).copied().collect();
    let paths: Option<Vec<PtrPath>> = rest.iter().filter(|s| s.contains(',')).map(|s| PtrPath::parse(s)).collect();
    let sites: Option<Vec<Site>> =
        rest.iter().filter(|s| !s.contains(',')).map(|s| Site::parse(&s.split(':').collect::<Vec<_>>())).collect();
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
    let guard_addr = addr.wrapping_sub(sites.first().map_or(0, |s| s.disp) as u64);
    // Paths are followed on every check, so they need no guard (and the value may not exist yet).
    let guard = match read_guard(mem, guard_addr) {
        Some(g) => g,
        None if !paths.is_empty() || !named.is_empty() => [0; 4],
        None => return writeln!(out, "error: cannot read the object at 0x{guard_addr:x}"),
    };
    l.limits.retain(|l| l.name != *name);
    l.limits.push(Limit {
        name: name.to_string(),
        addr,
        kind,
        min,
        max,
        sites,
        paths,
        named: named.pop().map(|path| NamedLimit { path, roots: Vec::new(), found_at: None, addrs: Vec::new() }),
        guard_addr,
        guard,
        fixes: 0,
        restores: 0,
        paused: None,
        retry_at: Instant::now(),
        checked_at: Instant::now(),
        searched_code: false,
    });
    writeln!(out, "limiting {name} at 0x{addr:x}")
}

fn cmd_unlimit(out: &mut impl Write, limiter: &SharedLimiter, name: &str) -> io::Result<()> {
    limiter.lock().unwrap().limits.retain(|l| l.name != name);
    writeln!(out, "no longer limiting {name}")
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
            l.paused.unwrap_or("active")
        )?;
    }
    Ok(())
}

/// A scan can keep millions of these: address and type share one word (addresses fit in 62
/// bits), next to the value at the last scan, mark or next.
#[derive(Clone, Copy)]
struct Candidate {
    tagged: u64,
    value: f64,
}

impl Candidate {
    fn new(addr: u64, kind: Kind, value: f64) -> Self {
        Candidate { tagged: addr | (kind as u64) << 62, value }
    }

    fn addr(&self) -> u64 {
        self.tagged & ((1 << 62) - 1)
    }

    fn kind(&self) -> Kind {
        Kind::ALL[(self.tagged >> 62) as usize]
    }
}

/// Current values of candidates sorted by address, reading memory a block at a time.
fn read_all(mem: &File, cands: &[Candidate]) -> Vec<Option<f64>> {
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
    mem: Option<File>,
    exe: String,
    /// Pointer size in the game: 4 or 8 bytes.
    width: usize,
    /// The game stores numbers only as doubles: scans look for nothing else.
    doubles_only: bool,
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

fn cmd_info(out: &mut impl Write) -> io::Result<()> {
    let ns = fs::read_link("/proc/self/ns/pid").map(|p| p.display().to_string()).unwrap_or_default();
    let scope = fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").unwrap_or_else(|_| "n/a".into());
    let visible = fs::read_dir("/proc")?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()))
        .count();
    writeln!(out, "helper pid namespace: {ns}")?;
    writeln!(out, "helper uid: {}", my_uid())?;
    writeln!(out, "yama ptrace_scope: {}", scope.trim())?;
    writeln!(out, "processes visible to helper: {visible}")
}

fn cmd_ps(out: &mut impl Write, filter: &str) -> io::Result<()> {
    let uid = my_uid();
    let filter = filter.to_ascii_lowercase();
    let mut pids: Vec<u32> = fs::read_dir("/proc")?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .filter(|pid| owner_uid(*pid) == Some(uid))
        .collect();
    pids.sort();
    for pid in pids {
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
        if let Some(ac) = anti_cheat(pid) {
            write!(out, "  [{ac}]")?;
        }
        writeln!(out)?;
    }
    Ok(())
}

/// Processes that come with Wine, Proton or Steam rather than with a game.
const NOT_GAMES: &[&str] = &[
    "steam.exe", "services.exe", "winedevice.exe", "explorer.exe", "plugplay.exe", "rpcss.exe", "svchost.exe",
    "conhost.exe", "tabtip.exe", "start.exe", "wineboot.exe", "winemenubuilder.exe", "rundll32.exe",
    "steamwebhelper.exe", "mscorsvw.exe", "ngen.exe", "reaper", "pressure-vessel-wrap", "pv-adverb", "python3",
    "srt-bwrap", "bwrap", "wineserver", "sh",
    "bash", "steam", "gameoverlayui", "fossilize_replay", "timeout", "sleep", "umu.exe", "xalia.exe",
    "xwayland", "gamescope", "gamescope-wl", "gpu-screen-recorder", "mpv", "ffplay",
];

/// Native games outside Steam: a GPU driver is loaded (the program draws with OpenGL or Vulkan;
/// the plain libGL/libEGL/libvulkan front ends also load in programs that never draw) and it
/// isn't a desktop app (GTK, Qt, Chromium/Electron).
fn draws_like_a_game(pid: u32) -> bool {
    if cmdline(pid).contains(" --type=") {
        return false;
    }
    let Ok(regions) = maps(pid) else { return false };
    let mut driver = false;
    for r in &regions {
        let name = r.path.rsplit('/').next().unwrap_or_default();
        if desktop_toolkit(name) {
            return false;
        }
        driver |= gpu_driver(name);
    }
    driver
}

/// A GPU driver's library: loaded once a program draws with the GPU.
fn gpu_driver(name: &str) -> bool {
    ["libGLX_", "libEGL_", "libvulkan_", "libnvidia-glcore.", "libnvidia-eglcore.", "libgallium", "amdvlk"]
        .iter()
        .any(|d| name.starts_with(d))
        || name.ends_with("_dri.so")
}

/// A launcher's install: its folder or the one above ships Qt or Chromium Embedded DLLs (seen
/// through the process's root, for sandboxed launchers).
fn launcher_folder(pid: u32, program: &Path) -> bool {
    let ships_toolkit = |dir: &Path| {
        fs::read_dir(format!("/proc/{pid}/root{}", dir.display())).is_ok_and(|entries| {
            entries.flatten().any(|e| e.file_name().to_str().is_some_and(|n| n.to_ascii_lowercase().ends_with(".dll") && desktop_toolkit(n)))
        })
    };
    program.ancestors().skip(1).take(2).any(ships_toolkit)
}

/// A library only desktop apps load: GTK, Qt, Chromium Embedded (Linux names and Windows DLLs).
fn desktop_toolkit(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["libgtk-3.", "libgtk-4.", "libqt5core.", "libqt6core.", "libcef.", "qt5core.dll", "qt6core.dll"]
        .iter()
        .any(|t| name.starts_with(t))
}

/// Windows programs under Wine that aren't games: Wine's own (csrss.exe, lsass.exe, ... from
/// its lib/wine folder), Chromium helpers, and launchers' windows and services (the EA app's
/// use Chromium Embedded and Qt; it showed up as ten games), also their helpers that load
/// neither and don't draw (the EA app starts compatibility32\EADesktop.exe next to every game).
fn windows_non_game(pid: u32) -> bool {
    if cmdline(pid).contains(" --type=") {
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
}

/// Running games, one per line: pid, program name, Steam app ID, anti-cheat, the name its
/// launcher gives it and "online" (online only) or "multiplayer" (tab-separated, "-" when
/// unknown or not).
fn cmd_games(out: &mut impl Write) -> io::Result<()> {
    let uid = my_uid();
    let mut pids: Vec<u32> = fs::read_dir("/proc")?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .filter(|pid| owner_uid(*pid) == Some(uid))
        .collect();
    pids.sort();
    let mut rows = Vec::new();
    for pid in pids {
        let exe = exe_name(pid);
        let lower = exe.to_ascii_lowercase();
        let env = environ(pid);
        let app_id = steam_id(&env);
        let windows = lower.ends_with(".exe");
        // Steam's runtime starts a launcher service next to Proton games, with the game's app ID
        // (steam-runtime-launcher-service, now <arch>-srt-launcher-service).
        let not_game = NOT_GAMES.contains(&lower.as_str()) || lower.ends_with("-launcher-service");
        if not_game || lower.contains("crashhandler") || lower.contains("crashreport") {
            continue;
        }
        if !(windows || app_id.is_some() || draws_like_a_game(pid)) || windows && windows_non_game(pid) {
            continue;
        }
        let dash = |s: Option<&str>| s.filter(|s| !s.is_empty()).unwrap_or("-").to_owned();
        let about = about(&env);
        let name = about.name.as_ref().map(|n| n.replace('\t', " "));
        let play = if about.online_only { Some("online") } else { about.multiplayer.then_some("multiplayer") };
        let id = dash(app_id.map(String::as_str));
        let ac = dash(anti_cheat(pid).as_deref());
        let program = maps(pid).ok().and_then(|m| program_path(pid, &m));
        rows.push((program, format!("{pid}\t{exe}\t{id}\t{ac}\t{}\t{}", dash(name.as_deref()), dash(play))));
    }
    // An Unreal stub holds none of the game's values (Astro Colony: two entries, a search in
    // the stub found nothing).
    let stub_folders: Vec<&Path> = rows.iter().filter_map(|(p, _)| unreal_stub_folder(p.as_deref()?)).collect();
    for (program, row) in &rows {
        let stub = program.as_deref().is_some_and(|p| p.parent().is_some_and(|dir| stub_folders.contains(&dir)));
        if !stub {
            writeln!(out, "{row}")?;
        }
    }
    Ok(())
}

fn cmd_attach(out: &mut impl Write, s: &mut Session, limiter: &SharedLimiter, arg: &str) -> io::Result<()> {
    let Ok(pid) = arg.parse::<u32>() else {
        return writeln!(out, "error: usage: attach <pid>");
    };
    if let Some(ac) = anti_cheat(pid) {
        return writeln!(out, "error: refusing to attach, the game comes with {ac}");
    }
    if online_only(pid) {
        return writeln!(out, "error: refusing to attach, the game is played online only");
    }
    match OpenOptions::new().read(true).write(true).open(format!("/proc/{pid}/mem")) {
        Ok(f) => {
            let exe = exe_name(pid);
            let modules = pointers::modules(pid, &f);
            let width = pointers::pointer_width(pid, &f, &modules, &exe);
            *limiter.lock().unwrap() = Limiter { pid, mem: f.try_clone().ok(), width, modules, limits: Vec::new() };
            let doubles_only = is_gamemaker(pid, &exe);
            *s = Session { pid, mem: Some(f), exe: exe.clone(), width, doubles_only, ..Session::default() };
            let regions = maps(pid)?;
            let rw: u64 = regions.iter().filter(|r| scannable(r)).map(|r| r.end - r.start).sum();
            // Only the program: launchers pass login tokens as arguments (Heroic's Epic games get
            // -AUTH_PASSWORD=<code>), and the log is kept.
            writeln!(out, "attached to {pid}: {}", argv0(pid))?;
            writeln!(out, "exe: {exe}")?;
            writeln!(out, "{} mappings, {} MiB writable, {}-bit", regions.len(), rw >> 20, width * 8)?;
            if doubles_only {
                writeln!(out, "GameMaker game: it keeps numbers as doubles, searching only those")?;
            }
            Ok(())
        }
        Err(e) => match ptrace_scope() {
            // Yama, not file permissions: the game is the user's own.
            Some(scope @ 1..) if e.kind() == io::ErrorKind::PermissionDenied && owner_uid(pid) == Some(my_uid()) => {
                writeln!(out, "error: blocked by ptrace_scope {scope}")
            }
            _ => writeln!(out, "error: cannot open /proc/{pid}/mem: {e}"),
        },
    }
}

fn ptrace_scope() -> Option<u8> {
    fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").ok()?.trim().parse().ok()
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
        return writeln!(out, "error: usage: scan <number, e.g. 1250 or 1.5> [i32,f32,f64,xor] [all]");
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
    mem: &'a File,
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
}

impl MemScan<'_> {
    /// Writes "progress <bytes done> <bytes in all>" lines as it goes (at most ten a second), so
    /// the interface can show how far the scan is: the first one takes seconds.
    fn run(&self, kinds: &[Kind], out: &mut impl Write) -> MemScanned {
        let (mem, n) = (self.mem, self.n);
        let mut r = MemScanned { found: Vec::new(), shaped: Vec::new(), bytes: 0, unreadable: 0 };
        let mut buf = vec![0u8; 4 << 20];
        let total: u64 = self.regions.iter().filter(|r| scannable(r)).map(|r| r.end - r.start).sum();
        let mut done = 0u64;
        let mut told = Instant::now();
        let _ = writeln!(out, "progress 0 {total}");
        for reg in self.regions.iter().filter(|r| scannable(r)) {
            let mut addr = reg.start;
            while addr < reg.end {
                if told.elapsed() >= Duration::from_millis(100) {
                    let _ = writeln!(out, "progress {done} {total}");
                    told = Instant::now();
                }
                let len = ((reg.end - addr) as usize).min(buf.len());
                done += len as u64;
                match mem.read_at(&mut buf[..len], addr) {
                    Ok(len) if len > 0 => {
                        r.bytes += len as u64;
                        for off in (0..len.saturating_sub(3)).step_by(4) {
                            // Doubles are 8-aligned. Integers and pointers read as floats come out
                            // as zero or next to nothing (1e-40, 1e-300), never a number on screen.
                            for &kind in kinds {
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
                                    Kind::I32 | Kind::Xor => value == n.scaled,
                                    Kind::F32 | Kind::F64 => kind.fits(value - n.step, value + n.step, n),
                                };
                                if hit {
                                    let at = addr + off as u64;
                                    let c = Candidate::new(at, kind, value);
                                    let around = |o: i64, n: usize| {
                                        le_at(&buf[..len], off as i64 + o, n).or_else(|| read_le(mem, at.wrapping_add_signed(o), n))
                                    };
                                    if self.shapes.iter().any(|sh| sh.kind == kind && sh.fits(around, self.mapped, self.width)) {
                                        r.shaped.push(c);
                                    }
                                    r.found.push(c);
                                }
                            }
                        }
                    }
                    _ => r.unreadable += len as u64,
                }
                addr += len as u64;
            }
        }
        r
    }
}

/// "12 i32, 3 f32, 0 f64"
fn kinds_text(c: &[Candidate]) -> String {
    let count = |k: Kind| c.iter().filter(|c| c.kind() == k).count();
    Kind::ALL.iter().map(|k| format!("{} {}", count(*k), k.name())).collect::<Vec<_>>().join(", ")
}

fn cmd_next(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
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
            keep(c.kind(), c.value, new).then_some(Candidate { value: new, ..*c })
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
fn cmd_mark(out: &mut impl Write, s: &mut Session) -> io::Result<()> {
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

fn cmd_write(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let mut it = arg.split_whitespace();
    let loc = it.next().and_then(parse_loc);
    let value = it.next().and_then(|v| v.parse::<f64>().ok());
    let (Some((addr, kind)), Some(value), Some(mem)) = (loc, value, s.mem.as_ref()) else {
        return writeln!(out, "error: usage: write <hex addr[:type]> <n> (after attach)");
    };
    match write_value(mem, addr, kind, value) {
        Ok(()) => writeln!(out, "wrote {value}, reads back {}", s.describe(addr, kind)),
        Err(e) => writeln!(out, "error: write failed: {e}"),
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

fn read_le(mem: &File, addr: u64, n: usize) -> Option<u64> {
    let mut w = [0u8; 8];
    mem.read_exact_at(&mut w[..n], addr).ok()?;
    Some(u64::from_le_bytes(w))
}

/// shape <hex addr[:type]>: the memory around the value as a shape, every word in it (the
/// caller keeps what other finds agree on).
fn cmd_shape(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let (Some((addr, kind)), Some(mem)) = (parse_loc(arg), s.mem.as_ref()) else {
        return writeln!(out, "error: usage: shape <hex addr[:type]> (after attach)");
    };
    let mapped = Mapped::new(&maps(s.pid)?);
    let size = kind.size() as i64;
    let mut words = Vec::new();
    let mut off = -SHAPE_SPAN;
    while off < size + SHAPE_SPAN {
        if (0..size).contains(&off) {
            off = size;
            continue;
        }
        let at = addr.wrapping_add_signed(off);
        let width = s.width as i64;
        // A pointer: aligned, clear of the value, into mapped memory.
        let pointer = at % s.width as u64 == 0
            && (off + width <= 0 || off >= size)
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
fn find_pattern(pid: u32, mem: &File, pat: &[Option<u8>], limit: usize) -> io::Result<Vec<u64>> {
    let (anchor, first) = pat.iter().enumerate().find_map(|(i, b)| Some((i, (*b)?))).unwrap_or((0, 0));
    let mut found = Vec::new();
    for r in maps(pid)?.iter().filter(|r| r.perms.contains('x')) {
        let mut buf = vec![0u8; (r.end - r.start) as usize];
        if mem.read_exact_at(&mut buf, r.start).is_err() {
            continue;
        }
        for i in 0..buf.len().saturating_sub(pat.len()) {
            if buf[i + anchor] == first
                && pat.iter().zip(&buf[i..]).all(|(p, b)| p.map_or(true, |p| p == *b))
            {
                found.push(r.start + i as u64);
                if found.len() >= limit {
                    return Ok(found);
                }
            }
        }
    }
    Ok(found)
}

/// "What accesses this address": watches it with a hardware breakpoint for a
/// few seconds and prints a restart-proof pattern for each instruction found.
/// Output lines: site <pattern> <offset of instruction in pattern> <base register> <displacement>
fn cmd_sites(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let mut it = arg.split_whitespace();
    let (Some((target, _)), Some(mem)) = (it.next().and_then(parse_loc), s.mem.as_ref()) else {
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
        let threads = tracer.arm(target, trace::DR7_ACCESS_4);
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
fn frontend_spoke() -> bool {
    let mut fd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut fd, 1, 0) > 0 }
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
}

impl Site {
    fn parse(f: &[&str]) -> Option<Self> {
        let reg = f.get(2)?;
        Some(Site {
            pat: parse_pattern(f.first()?)?,
            off: f.get(1)?.parse().ok()?,
            base: trace::REG_NAMES.iter().position(|n| n == reg)? as u8,
            disp: f.get(3)?.parse().ok()?,
        })
    }
}

/// Where a code pattern led: the value's address, and when the code loaded the object from a
/// static pointer, that pointer as a path (good for this run only).
struct Resolved {
    addr: u64,
    is_64bit: bool,
    holder: Option<PtrPath>,
}

/// How far before the instruction its base register may have been loaded.
const HOLDER_REACH: usize = 32;

/// The static pointer the site's instruction (at `instr`) got its object from, when a
/// `mov <base>,[address]` shortly before it (how Mono and C++ code read a static object) loads
/// from a pointer that holds that object now. Following it finds the object the game uses,
/// also after the game replaces it (Particle Fleet does when the map changes).
fn holder(mem: &File, instr: u64, site: &Site, is_64bit: bool, width: usize, addr: u64) -> Option<PtrPath> {
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
fn holder_in_code(pid: u32, mem: &File, sites: &[Site], is_64bit: bool, width: usize, addr: u64) -> Option<PtrPath> {
    sites.iter().find_map(|site| {
        let [code] = find_pattern(pid, mem, &site.pat, 2).ok()?[..] else { return None };
        holder(mem, code + site.off, site, is_64bit, width, addr)
    })
}

/// Finds a saved pattern in the game's code, waits for that instruction to
/// run and reads its base register: the value's address in this run.
fn resolve_site(pid: u32, mem: &File, site: &Site, width: usize, timeout: Duration) -> Result<Resolved, String> {
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
        let is_64bit = trace::is_64bit(&hit.regs);
        resolved = Some(Resolved { addr, is_64bit, holder: holder(mem, instr, site, is_64bit, width, addr) });
        false
    });
    resolved.ok_or(format!("the code at 0x{instr:x} did not run within {} s", timeout.as_secs()))
}

/// ptrscan <hex addr[:type]> [depth] [max offset, hex] [max paths]: pointer paths to the address, best
/// first, as lines "path <module>+<offset>,<offset>,...".
fn cmd_ptrscan(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let mut it = arg.split_whitespace();
    let (Some((target, _)), Some(mem)) = (it.next().and_then(parse_loc), s.mem.as_ref()) else {
        return writeln!(out, "error: usage: ptrscan <hex addr[:type]> [depth] [max offset] [max paths] (after attach)");
    };
    let depth = it.next().and_then(|v| v.parse().ok()).unwrap_or(5);
    let max_off = it.next().and_then(parse_addr).unwrap_or(0x1000);
    let max_paths = it.next().and_then(|v| v.parse().ok()).unwrap_or(200);
    let t = Instant::now();
    let collected = s.pointer_map.take().filter(|(at, _)| at.elapsed() < Duration::from_secs(60)).map(|(_, c)| c);
    let r = pointers::scan(s.pid, mem, s.width, &s.exe, target, depth, max_off, max_paths, collected)?;
    for p in &r.paths {
        writeln!(out, "path {}", p.text())?;
    }
    let levels: Vec<String> = r.levels.iter().map(|n| n.to_string()).collect();
    writeln!(
        out,
        "{} paths ({} from {}; {} more that step from one object into the next more than {} times dropped), {} pointers in {} MiB, addresses per step back: {}, in {} ms",
        r.paths.len(),
        r.paths.iter().filter(|p| p.module.eq_ignore_ascii_case(&s.exe)).count(),
        s.exe,
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
fn cmd_follow(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let mut it = arg.split_whitespace();
    let kind = it.next().and_then(Kind::parse);
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
    match pointers::vote(mem, &mods, s.width, &paths) {
        Some(v) => {
            writeln!(out, "best {}", s.describe(v.addr, kind))?;
            let clear = if v.clear() { "clear" } else { "unclear" };
            writeln!(out, "votes {} {} {} {clear}", v.agree(), v.next, paths.len())
        }
        None => writeln!(out, "none of the paths lead anywhere now"),
    }
}

/// names <hex addr[:type]> [unreal]: named paths that lead to the value now, best first ("unreal": only
/// the engine's own: Unreal objects and Mono classes, no tracing or pointer map needed): "named <path>
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
    // A field of a Unity (Mono) class's only live object: by the class's name, past the
    // objects the game replaces (a checkpoint's new player).
    if let Some((p, leads)) = mono::discover(&heap, target) {
        writeln!(out, "named {} {} {}", p.text(), leads.len(), p.describe())?;
        return writeln!(out, "1 named paths in {} ms", t.elapsed().as_millis());
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
    if let Some(why) = named_doubt(mem, &leads, kind, &path) {
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
    let f: Vec<&str> = arg.split_whitespace().collect();
    let (Some(site), Some(mem)) = (Site::parse(&f), s.mem.as_ref()) else {
        return writeln!(out, "error: usage: resolve <pattern> <offset> <register> <displacement> [type] [seconds]");
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

pub fn run() {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    let mut session = Session::default();
    let limiter = SharedLimiter::default();
    {
        let limiter = limiter.clone();
        std::thread::spawn(move || limiter_loop(limiter));
    }
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let (cmd, arg) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        let arg = arg.trim();
        // Ends a `sites ... wait` early; one arriving after it ended on its own changes nothing,
        // and gets no reply (the frontend doesn't wait for one).
        if cmd == "cancel" {
            continue;
        }
        let res = match cmd {
            "info" => cmd_info(&mut out),
            "ps" => cmd_ps(&mut out, arg),
            "games" => cmd_games(&mut out),
            "attach" => cmd_attach(&mut out, &mut session, &limiter, arg),
            "scan" => cmd_scan(&mut out, &mut session, arg),
            "next" => cmd_next(&mut out, &mut session, arg),
            "mark" => cmd_mark(&mut out, &mut session),
            "undo" => cmd_undo(&mut out, &mut session),
            "redo" => cmd_redo(&mut out, &mut session),
            "list" => cmd_list(&mut out, &session, arg),
            "write" => cmd_write(&mut out, &session, arg),
            "set" => cmd_set(&mut out, &session, arg),
            "peek" => cmd_peek(&mut out, &session, arg),
            "keep" => cmd_keep(&mut out, &mut session, arg),
            "drop" => cmd_drop(&mut out, &mut session, arg),
            "alive" => cmd_alive(&mut out, &session),
            "about" => cmd_about(&mut out, &mut session, arg),
            "shape" => cmd_shape(&mut out, &session, arg),
            "shapes" => cmd_shapes(&mut out, &mut session, arg),
            "track" => cmd_track(&mut out, &mut session, arg),
            "sites" => cmd_sites(&mut out, &session, arg),
            "resolve" => cmd_resolve(&mut out, &mut session, arg),
            "ptrscan" => cmd_ptrscan(&mut out, &mut session, arg),
            "names" => cmd_names(&mut out, &mut session, arg),
            "named" => cmd_named(&mut out, &mut session, arg),
            "ue" => cmd_ue(&mut out, &session, arg),
            "gdtree" => cmd_gdtree(&mut out, &session),
            "follow" => cmd_follow(&mut out, &session, arg),
            "limit" => cmd_limit(&mut out, &limiter, arg),
            "unlimit" => cmd_unlimit(&mut out, &limiter, arg),
            "limits" => cmd_limits(&mut out, &limiter),
            _ => writeln!(
                out,
                "commands: sandbox, info, ps [filter], games, attach <pid>, scan <n> [i32,f32,f64,xor] [all], mark, next <n>|+|-|=|!, undo, redo, list [n], peek <addr>..., keep <addr>, drop <addr>, about <addr>..., alive, shape <addr>, shapes <shape>; ..., track <addr>..., sites <addr> [seconds] [wait], cancel, resolve <site> [type], ptrscan <addr> [depth] [max offset], names <addr> [unreal], named <type> <named path>, ue [objects <text>|class <name>|dump <addr>], gdtree, follow <type> <path>..., limit <name> <addr> <min> <max> <sites>, unlimit <name>, limits, write <addr> <n>, set <n>, quit (addresses: <hex>[:i32|f32|f64|xor])"
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
}
