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
use crate::pointers::{self, Module, PtrPath};
use crate::launchers;
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
fn exe_name(pid: u32) -> String {
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
        None => {
            // Wine maps the .exe by its Linux path; native programs are /proc/<pid>/exe.
            let exe = exe_name(pid);
            let mapped = maps.iter().map(|r| Path::new(&r.path)).find(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(&*exe)));
            let program = match mapped {
                Some(p) => p.to_path_buf(),
                None => fs::read_link(format!("/proc/{pid}/exe")).ok()?,
            };
            anticheat::game_folder(&program)
        }
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
enum Kind {
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
    /// screen showed as `n`. Games show fractions rounded or cut off; a decimal like 1.2 may
    /// be kept as a whole number of tenths (12).
    fn fits(self, lo: f64, hi: f64, n: Shown) -> bool {
        let (lo, hi) = (lo.min(hi), lo.max(hi));
        match self {
            Kind::I32 | Kind::Xor => lo <= n.scaled && n.scaled <= hi,
            Kind::F32 | Kind::F64 => lo < n.value + n.step && hi >= n.value - n.step / 2.0,
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

/// Keeps limited values in range; paused limits are found again through their
/// saved code patterns and then resume. Values found through code patterns are also found
/// again every 10 s, and through the static pointer their object comes from (when there is
/// one) on every check. A limit with no bounds only keeps the address current.
fn limiter_loop(shared: SharedLimiter) {
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let due: Vec<(String, Vec<Site>, bool)>;
        let (pid, mem, width) = {
            let mut guard = shared.lock().unwrap();
            let Limiter { pid, mem, width, modules, limits } = &mut *guard;
            let Some(mem) = mem.as_ref() else { continue };
            let now = Instant::now();
            // Pointer paths are cheap to follow: re-find the value on every check.
            if limits.iter().any(|l| !l.paths.is_empty() && l.paused.is_some() && l.retry_at <= now) {
                *modules = pointers::modules(*pid, mem);
            }
            for l in limits.iter_mut().filter(|l| !l.paths.is_empty() && (l.paused.is_none() || l.retry_at <= now)) {
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
            for l in limits.iter_mut().filter(|l| l.paused.is_none()) {
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
                .filter(|l| l.paths.is_empty())
                .filter(|l| if l.paused.is_some() { l.retry_at <= now } else { l.checked_at + RECHECK_EVERY <= now })
                .map(|l| (l.name.clone(), l.sites.clone(), !l.searched_code))
                .collect();
            let Ok(mem) = mem.try_clone() else { continue };
            (*pid, mem, *width)
        };
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
/// "pattern:offset:register:displacement" as saved in the profile, and a path
/// "module+offset,offset,..." ("+static,disp" from resolve). No bounds: only keeps the address
/// current (see `limiter_loop`).
fn cmd_limit(out: &mut impl Write, limiter: &SharedLimiter, arg: &str) -> io::Result<()> {
    let f: Vec<&str> = arg.split_whitespace().collect();
    let rest = f.get(4..).unwrap_or_default();
    let paths: Option<Vec<PtrPath>> = rest.iter().filter(|s| s.contains(',')).map(|s| PtrPath::parse(s)).collect();
    let sites: Option<Vec<Site>> =
        rest.iter().filter(|s| !s.contains(',')).map(|s| Site::parse(&s.split(':').collect::<Vec<_>>())).collect();
    let (Some(name), Some(addr), Ok(min), Ok(max), Some(sites), Some(paths)) =
        (f.first(), f.get(1).and_then(|a| parse_loc(a)), bound(f.get(2)), bound(f.get(3)), sites, paths)
    else {
        return writeln!(out, "error: usage: limit <name> <hex addr[:type]> <min|-> <max|-> <pattern:offset:register:displacement | module+offset,offset,...>...");
    };
    if sites.is_empty() && paths.is_empty() {
        return writeln!(out, "error: a limit needs at least one saved code pattern or pointer path");
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
        None if !paths.is_empty() => [0; 4],
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
}

impl Session {
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
    "srt-bwrap", "bwrap", "steam-runtime-launcher-service", "x86_64-linux-gnu-srt-launch", "wineserver", "sh",
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
        if ["libgtk-3.", "libgtk-4.", "libQt5Core.", "libQt6Core.", "libcef."].iter().any(|t| name.starts_with(t)) {
            return false;
        }
        driver |= ["libGLX_", "libEGL_", "libvulkan_", "libnvidia-glcore.", "libnvidia-eglcore.", "libgallium", "amdvlk"]
            .iter()
            .any(|d| name.starts_with(d))
            || name.ends_with("_dri.so");
    }
    driver
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
    for pid in pids {
        let exe = exe_name(pid);
        let lower = exe.to_ascii_lowercase();
        let env = environ(pid);
        let app_id = steam_id(&env);
        let windows = lower.ends_with(".exe");
        if NOT_GAMES.contains(&lower.as_str()) || lower.contains("crashhandler") {
            continue;
        }
        if !(windows || app_id.is_some() || draws_like_a_game(pid)) {
            continue;
        }
        let dash = |s: Option<&str>| s.filter(|s| !s.is_empty()).unwrap_or("-").to_owned();
        let about = about(&env);
        let name = about.name.as_ref().map(|n| n.replace('\t', " "));
        let play = if about.online_only { Some("online") } else { about.multiplayer.then_some("multiplayer") };
        let id = dash(app_id.map(String::as_str));
        let ac = dash(anti_cheat(pid).as_deref());
        writeln!(out, "{pid}\t{exe}\t{id}\t{ac}\t{}\t{}", dash(name.as_deref()), dash(play))?;
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
            *s = Session { pid, mem: Some(f), exe: exe.clone(), width, doubles_only, candidates: Vec::new() };
            let regions = maps(pid)?;
            let rw: u64 = regions.iter().filter(|r| scannable(r)).map(|r| r.end - r.start).sum();
            writeln!(out, "attached to {pid}: {}", cmdline(pid))?;
            writeln!(out, "exe: {exe}")?;
            writeln!(out, "{} mappings, {} MiB writable, {}-bit", regions.len(), rw >> 20, width * 8)?;
            if doubles_only {
                writeln!(out, "GameMaker game: it keeps numbers as doubles, searching only those")?;
            }
            Ok(())
        }
        Err(e) => writeln!(out, "error: cannot open /proc/{pid}/mem: {e}"),
    }
}

/// scan <n>: every value that can be what the screen shows as n, stored as a 4-byte integer
/// (plain or XOR-encoded) or as a float/double (fractions are cut off or rounded on screen).
fn cmd_scan(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let Some(n) = Shown::parse(arg.trim()) else {
        return writeln!(out, "error: usage: scan <number, e.g. 1250 or 1.5>");
    };
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let t = Instant::now();
    let kinds: Vec<Kind> = Kind::ALL.into_iter().filter(|k| !s.doubles_only || *k == Kind::F64).collect();
    let mut found = Vec::new();
    let (mut bytes, mut unreadable) = (0u64, 0u64);
    let mut buf = vec![0u8; 4 << 20];
    for r in maps(s.pid)?.iter().filter(|r| scannable(r)) {
        let mut addr = r.start;
        while addr < r.end {
            let len = ((r.end - addr) as usize).min(buf.len());
            match mem.read_at(&mut buf[..len], addr) {
                Ok(len) if len > 0 => {
                    bytes += len as u64;
                    for off in (0..len.saturating_sub(3)).step_by(4) {
                        // Doubles are 8-aligned. Integers and pointers read as floats come out
                        // as zero or next to nothing (1e-40, 1e-300), never a number on screen.
                        for &kind in &kinds {
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
                                found.push(Candidate::new(addr + off as u64, kind, value));
                            }
                        }
                    }
                }
                _ => unreadable += len as u64,
            }
            addr += len as u64;
        }
    }
    s.candidates = found;
    writeln!(
        out,
        "{} matches ({}) in {} MiB ({} MiB unreadable) in {} ms",
        s.candidates.len(),
        kinds_text(&s.candidates),
        bytes >> 20,
        unreadable >> 20,
        t.elapsed().as_millis()
    )
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
    s.candidates = next;
    writeln!(out, "{before} -> {} matches ({})", s.candidates.len(), kinds_text(&s.candidates))
}

/// Remembers every candidate's current value, right before the screen is read: `next` then
/// accepts values that showed the number at any point in between (the screen lags memory).
fn cmd_mark(out: &mut impl Write, s: &mut Session) -> io::Result<()> {
    let marked: Vec<Candidate> =
        s.candidates.iter().zip(s.read_all()).map(|(c, v)| Candidate { value: v.unwrap_or(c.value), ..*c }).collect();
    s.candidates = marked;
    writeln!(out, "marked {} matches", s.candidates.len())
}

fn cmd_list(out: &mut impl Write, s: &Session) -> io::Result<()> {
    for c in s.candidates.iter().take(20) {
        writeln!(out, "{}", s.describe(c.addr(), c.kind()))?;
    }
    if s.candidates.len() > 20 {
        writeln!(out, "... {} more", s.candidates.len() - 20)?;
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
    s.candidates = arg
        .split_whitespace()
        .filter_map(parse_loc)
        .filter_map(|(addr, kind)| Some(Candidate::new(addr, kind, s.read(addr, kind)?)))
        .collect();
    s.candidates.sort_by_key(|c| c.addr());
    writeln!(out, "tracking {} matches", s.candidates.len())
}

fn cmd_keep(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let Some((addr, kind)) = parse_loc(arg) else {
        return writeln!(out, "error: usage: keep <hex addr[:type]>");
    };
    // With a type, only that one: an int and an XOR value can start at the same address.
    let typed = arg.contains(':');
    let before = s.candidates.len();
    s.candidates.retain(|c| c.addr() == addr && (!typed || c.kind() == kind));
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
        return writeln!(out, "error: usage: sites <hex addr[:type]> [seconds] (after attach)");
    };
    let secs = it.next().and_then(|v| v.parse().ok()).unwrap_or(5);
    let mut hits: Vec<(u64, trace::Regs)> = Vec::new();
    {
        let _tracing = TRACING.lock().unwrap();
        let mut tracer = match trace::Tracer::attach(s.pid) {
            Ok(t) => t,
            Err(e) => return writeln!(out, "error: cannot trace the game: {e}"),
        };
        let threads = tracer.arm(target, trace::DR7_ACCESS_4);
        writeln!(out, "watching 0x{target:x} on {threads} threads for {secs} s")?;
        tracer.watch(Duration::from_secs(secs), |hit| {
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
fn cmd_ptrscan(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let mut it = arg.split_whitespace();
    let (Some((target, _)), Some(mem)) = (it.next().and_then(parse_loc), s.mem.as_ref()) else {
        return writeln!(out, "error: usage: ptrscan <hex addr[:type]> [depth] [max offset] [max paths] (after attach)");
    };
    let depth = it.next().and_then(|v| v.parse().ok()).unwrap_or(5);
    let max_off = it.next().and_then(parse_addr).unwrap_or(0x1000);
    let max_paths = it.next().and_then(|v| v.parse().ok()).unwrap_or(200);
    let t = Instant::now();
    let r = pointers::scan(s.pid, mem, s.width, &s.exe, target, depth, max_off, max_paths)?;
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
        let res = match cmd {
            "info" => cmd_info(&mut out),
            "ps" => cmd_ps(&mut out, arg),
            "games" => cmd_games(&mut out),
            "attach" => cmd_attach(&mut out, &mut session, &limiter, arg),
            "scan" => cmd_scan(&mut out, &mut session, arg),
            "next" => cmd_next(&mut out, &mut session, arg),
            "mark" => cmd_mark(&mut out, &mut session),
            "list" => cmd_list(&mut out, &session),
            "write" => cmd_write(&mut out, &session, arg),
            "set" => cmd_set(&mut out, &session, arg),
            "peek" => cmd_peek(&mut out, &session, arg),
            "keep" => cmd_keep(&mut out, &mut session, arg),
            "track" => cmd_track(&mut out, &mut session, arg),
            "sites" => cmd_sites(&mut out, &session, arg),
            "resolve" => cmd_resolve(&mut out, &mut session, arg),
            "ptrscan" => cmd_ptrscan(&mut out, &session, arg),
            "follow" => cmd_follow(&mut out, &session, arg),
            "limit" => cmd_limit(&mut out, &limiter, arg),
            "unlimit" => cmd_unlimit(&mut out, &limiter, arg),
            "limits" => cmd_limits(&mut out, &limiter),
            _ => writeln!(
                out,
                "commands: sandbox, info, ps [filter], games, attach <pid>, scan <n>, mark, next <n>|+|-|=|!, list, peek <addr>..., keep <addr>, track <addr>..., sites <addr>, resolve <site> [type], ptrscan <addr> [depth] [max offset], follow <type> <path>..., limit <name> <addr> <min> <max> <sites>, unlimit <name>, limits, write <addr> <n>, set <n>, quit (addresses: <hex>[:i32|f32|f64|xor])"
            ),
        };
        if res.and_then(|_| writeln!(out, "end")).and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}
