// The part that runs on the host (started by the frontend through
// `flatpak-spawn --host`). Reads and writes game memory through /proc/<pid>/mem.
// Protocol: one command per line on stdin; each reply ends with a line "end".

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::trace;

struct Region {
    start: u64,
    end: u64,
    perms: String,
    path: String,
}

fn maps(pid: u32) -> io::Result<Vec<Region>> {
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

fn anti_cheat(pid: u32) -> Option<&'static str> {
    let maps = maps(pid).ok()?;
    maps.iter().find_map(|r| {
        let p = r.path.to_ascii_lowercase();
        if p.contains("easyanticheat") {
            Some("Easy Anti-Cheat")
        } else if p.contains("beclient") || p.contains("battleye") {
            Some("BattlEye")
        } else {
            None
        }
    })
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
    /// screen showed as the whole number `n`. Games show fractions rounded or cut off.
    fn fits(self, lo: f64, hi: f64, n: f64) -> bool {
        let (lo, hi) = (lo.min(hi), lo.max(hi));
        match self {
            Kind::I32 | Kind::Xor => lo <= n && n <= hi,
            Kind::F32 | Kind::F64 => lo < n + 1.0 && hi >= n - 0.5,
        }
    }

    fn show(self, v: f64) -> String {
        match self {
            Kind::F32 => (v as f32).to_string(),
            _ => v.to_string(),
        }
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

fn scannable(r: &Region) -> bool {
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
    // First bytes of the object holding the value (its type pointer). If they
    // change, the object is gone and writing would corrupt unrelated memory.
    guard_addr: u64,
    guard: [u8; 4],
    fixes: u64,
    restores: u64,
    paused: Option<&'static str>,
    retry_at: Instant,
}

#[derive(Default)]
struct Limiter {
    pid: u32,
    mem: Option<File>,
    limits: Vec<Limit>,
}

type SharedLimiter = Arc<Mutex<Limiter>>;

const RETRY_EVERY: Duration = Duration::from_secs(5);
const RESOLVE_WAIT: Duration = Duration::from_secs(2);

fn read_guard(mem: &File, addr: u64) -> Option<[u8; 4]> {
    let mut g = [0u8; 4];
    mem.read_exact_at(&mut g, addr).ok().map(|_| g)
}

/// Keeps limited values in range; paused limits are found again through their
/// saved code patterns and then resume.
fn limiter_loop(shared: SharedLimiter) {
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let due: Vec<(String, Vec<Site>)>;
        let (pid, mem) = {
            let mut guard = shared.lock().unwrap();
            let Limiter { pid, mem, limits } = &mut *guard;
            let Some(mem) = mem.as_ref() else { continue };
            for l in limits.iter_mut().filter(|l| l.paused.is_none()) {
                if read_guard(mem, l.guard_addr) != Some(l.guard) {
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
                .filter(|l| l.paused.is_some() && l.retry_at <= now)
                .map(|l| (l.name.clone(), l.sites.clone()))
                .collect();
            let Ok(mem) = mem.try_clone() else { continue };
            (*pid, mem)
        };
        // Resolving traces the game for a moment, so it runs without holding the lock.
        for (name, sites) in due {
            let found = sites
                .iter()
                .find_map(|site| Some((resolve_site(pid, &mem, site, RESOLVE_WAIT).ok()?, site.disp)));
            let mut guard = shared.lock().unwrap();
            if guard.pid != pid {
                break;
            }
            let Some(l) = guard.limits.iter_mut().find(|l| l.name == name) else { continue };
            let object = found.and_then(|(addr, disp)| {
                let object = addr.wrapping_sub(disp as u64);
                Some((addr, object, read_guard(&mem, object)?))
            });
            match object {
                Some((addr, object, g)) => {
                    l.addr = addr;
                    l.guard_addr = object;
                    l.guard = g;
                    l.restores += 1;
                    l.paused = None;
                }
                None => l.retry_at = Instant::now() + RETRY_EVERY,
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

/// limit <name> <hex addr[:type]> <min|-> <max|-> <site>... where a site is
/// "pattern:offset:register:displacement" as saved in the profile.
fn cmd_limit(out: &mut impl Write, limiter: &SharedLimiter, arg: &str) -> io::Result<()> {
    let f: Vec<&str> = arg.split_whitespace().collect();
    let sites: Option<Vec<Site>> = f
        .get(4..)
        .map(|s| s.iter().map(|s| Site::parse(&s.split(':').collect::<Vec<_>>())).collect())
        .unwrap_or(None);
    let (Some(name), Some(addr), Ok(min), Ok(max), Some(sites)) =
        (f.first(), f.get(1).and_then(|a| parse_loc(a)), bound(f.get(2)), bound(f.get(3)), sites)
    else {
        return writeln!(out, "error: usage: limit <name> <hex addr[:type]> <min|-> <max|-> <pattern:offset:register:displacement>...");
    };
    let Some(disp) = sites.first().map(|s| s.disp) else {
        return writeln!(out, "error: a limit needs at least one saved code pattern");
    };
    let mut l = limiter.lock().unwrap();
    let Some(mem) = l.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let (addr, kind) = addr;
    let guard_addr = addr.wrapping_sub(disp as u64);
    let Some(guard) = read_guard(mem, guard_addr) else {
        return writeln!(out, "error: cannot read the object at 0x{guard_addr:x}");
    };
    l.limits.retain(|l| l.name != *name);
    l.limits.push(Limit {
        name: name.to_string(),
        addr,
        kind,
        min,
        max,
        sites,
        guard_addr,
        guard,
        fixes: 0,
        restores: 0,
        paused: None,
        retry_at: Instant::now(),
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
        let is_game = app_id.is_some() || cmd.to_ascii_lowercase().contains(".exe");
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
    "bash", "steam", "gameoverlayui", "fossilize_replay", "timeout", "sleep",
];

/// Running games, one per line: pid, program name, Steam app ID and anti-cheat
/// (tab-separated, "-" when unknown).
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
        let app_id = env.get("SteamAppId").or_else(|| env.get("SteamGameId")).filter(|id| *id != "0");
        let windows = lower.ends_with(".exe");
        if !(windows || app_id.is_some()) || NOT_GAMES.contains(&lower.as_str()) || lower.contains("crashhandler") {
            continue;
        }
        let dash = |s: Option<&str>| s.unwrap_or("-").to_owned();
        writeln!(out, "{pid}\t{exe}\t{}\t{}", dash(app_id.map(String::as_str)), dash(anti_cheat(pid)))?;
    }
    Ok(())
}

fn cmd_attach(out: &mut impl Write, s: &mut Session, limiter: &SharedLimiter, arg: &str) -> io::Result<()> {
    let Ok(pid) = arg.parse::<u32>() else {
        return writeln!(out, "error: usage: attach <pid>");
    };
    if let Some(ac) = anti_cheat(pid) {
        return writeln!(out, "error: refusing to attach, {ac} is loaded in this process");
    }
    match OpenOptions::new().read(true).write(true).open(format!("/proc/{pid}/mem")) {
        Ok(f) => {
            *limiter.lock().unwrap() = Limiter { pid, mem: f.try_clone().ok(), limits: Vec::new() };
            *s = Session { pid, mem: Some(f), candidates: Vec::new() };
            let regions = maps(pid)?;
            let rw: u64 = regions.iter().filter(|r| scannable(r)).map(|r| r.end - r.start).sum();
            writeln!(out, "attached to {pid}: {}", cmdline(pid))?;
            writeln!(out, "exe: {}", exe_name(pid))?;
            writeln!(out, "{} mappings, {} MiB writable", regions.len(), rw >> 20)
        }
        Err(e) => writeln!(out, "error: cannot open /proc/{pid}/mem: {e}"),
    }
}

/// scan <n>: every value that can be what the screen shows as n, stored as a 4-byte integer
/// (plain or XOR-encoded) or as a float/double (fractions are cut off or rounded on screen).
fn cmd_scan(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let Ok(n) = arg.parse::<i32>() else {
        return writeln!(out, "error: usage: scan <whole number>");
    };
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let t = Instant::now();
    let n = n as f64;
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
                        for kind in Kind::ALL {
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
                                Kind::I32 | Kind::Xor => value == n,
                                Kind::F32 | Kind::F64 => kind.fits(value - 1.0, value + 1.0, n),
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
        v => match v.parse::<i32>() {
            Ok(n) => Box::new(move |kind, old, new| kind.fits(old, new, n as f64)),
            Err(_) => return writeln!(out, "error: usage: next <whole number>|+|-|=|!"),
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
    let Some((addr, _)) = parse_loc(arg) else {
        return writeln!(out, "error: usage: keep <hex addr[:type]>");
    };
    let before = s.candidates.len();
    s.candidates.retain(|c| c.addr() == addr);
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
        let Some(acc) = trace::decode_access(&code, after - before, *after, target, regs) else {
            let tail: Vec<String> = code[before as usize - 16..before as usize].iter().map(|b| format!("{b:02x}")).collect();
            writeln!(out, "instruction before 0x{after:x}: not a [register+offset] access, skipped (bytes before it: {})", tail.join(" "))?;
            continue;
        };
        // Shared code (a runtime function every variable goes through, as in GameMaker games)
        // reads other addresses too: a pattern for it would find some other value next time.
        let read = site_targets(s.pid, acc.start, acc.base, acc.disp, Duration::from_secs(1));
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

/// Finds a saved pattern in the game's code, waits for that instruction to
/// run and reads its base register: the value's address in this run.
fn resolve_site(pid: u32, mem: &File, site: &Site, timeout: Duration) -> Result<u64, String> {
    let found = find_pattern(pid, mem, &site.pat, 2).map_err(|e| e.to_string())?;
    let [code] = found.as_slice() else {
        return Err(format!("pattern found {} times (the game may not have run that code yet)", found.len()));
    };
    let instr = code + site.off;
    let _tracing = TRACING.lock().unwrap();
    let mut value_addr = None;
    let mut tracer = trace::Tracer::attach(pid).map_err(|e| format!("cannot trace the game: {e}"))?;
    tracer.arm(instr, trace::DR7_EXECUTE);
    tracer.watch(timeout, |hit| {
        value_addr = Some(trace::reg(&hit.regs, site.base).wrapping_add(site.disp as u64));
        false
    });
    value_addr.ok_or(format!("the code at 0x{instr:x} did not run within {} s", timeout.as_secs()))
}

fn cmd_resolve(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let f: Vec<&str> = arg.split_whitespace().collect();
    let (Some(site), Some(mem)) = (Site::parse(&f), s.mem.as_ref()) else {
        return writeln!(out, "error: usage: resolve <pattern> <offset> <register> <displacement> [type] [seconds]");
    };
    let kind = f.get(4..).unwrap_or_default().iter().find_map(|v| Kind::parse(v)).unwrap_or(Kind::I32);
    let secs = f.get(4..).unwrap_or_default().iter().find_map(|v| v.parse().ok()).unwrap_or(10);
    match resolve_site(s.pid, mem, &site, Duration::from_secs(secs)) {
        Ok(addr) => match s.read(addr, kind) {
            Some(_) => writeln!(out, "{}", s.describe(addr, kind)),
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
            "limit" => cmd_limit(&mut out, &limiter, arg),
            "unlimit" => cmd_unlimit(&mut out, &limiter, arg),
            "limits" => cmd_limits(&mut out, &limiter),
            _ => writeln!(
                out,
                "commands: sandbox, info, ps [filter], games, attach <pid>, scan <n>, mark, next <n>|+|-|=|!, list, peek <addr>..., keep <addr>, track <addr>..., sites <addr>, resolve <site> [type], limit <name> <addr> <min> <max> <sites>, unlimit <name>, limits, write <addr> <n>, set <n>, quit (addresses: <hex>[:i32|f32|f64|xor])"
            ),
        };
        if res.and_then(|_| writeln!(out, "end")).and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}
