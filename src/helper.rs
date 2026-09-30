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

fn scannable(r: &Region) -> bool {
    r.perms.starts_with("rw") && !matches!(r.path.as_str(), "[vvar]" | "[vvar_vclock]" | "[vsyscall]")
}

// --- Limits: keep a value within a range, writing only when the game moves it out.

struct Limit {
    addr: u64,
    min: Option<i32>,
    max: Option<i32>,
    // First bytes of the object holding the value (its type pointer). If they
    // change, the object is gone and writing would corrupt unrelated memory.
    guard_addr: u64,
    guard: [u8; 4],
    fixes: u64,
    paused: Option<&'static str>,
}

#[derive(Default)]
struct Limiter {
    mem: Option<File>,
    limits: Vec<Limit>,
}

type SharedLimiter = Arc<Mutex<Limiter>>;

fn limiter_loop(shared: SharedLimiter) {
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let mut guard = shared.lock().unwrap();
        let Limiter { mem, limits } = &mut *guard;
        let Some(mem) = mem.as_ref() else { continue };
        for l in limits.iter_mut().filter(|l| l.paused.is_none()) {
            let mut g = [0u8; 4];
            if mem.read_exact_at(&mut g, l.guard_addr).is_err() || g != l.guard {
                l.paused = Some("the object holding it changed (new mission?), run restore");
                continue;
            }
            let mut b = [0u8; 4];
            if mem.read_exact_at(&mut b, l.addr).is_err() {
                l.paused = Some("unreadable");
                continue;
            }
            let v = i32::from_le_bytes(b);
            let target = match (l.min, l.max) {
                (_, Some(max)) if v > max => max,
                (Some(min), _) if v < min => min,
                _ => continue,
            };
            if mem.write_all_at(&target.to_le_bytes(), l.addr).is_ok() {
                l.fixes += 1;
            }
        }
    }
}

fn bound(v: Option<&&str>) -> Result<Option<i32>, ()> {
    match v {
        Some(&"-") => Ok(None),
        Some(v) => v.parse().map(Some).map_err(drop),
        None => Err(()),
    }
}

fn cmd_limit(out: &mut impl Write, limiter: &SharedLimiter, arg: &str) -> io::Result<()> {
    let f: Vec<&str> = arg.split_whitespace().collect();
    let (Some(addr), Ok(min), Ok(max), Some(guard_addr)) =
        (f.first().and_then(|a| parse_addr(a)), bound(f.get(1)), bound(f.get(2)), f.get(3).and_then(|a| parse_addr(a)))
    else {
        return writeln!(out, "error: usage: limit <hex addr> <min|-> <max|-> <hex object addr>");
    };
    let mut l = limiter.lock().unwrap();
    let Some(mem) = l.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let mut guard = [0u8; 4];
    if mem.read_exact_at(&mut guard, guard_addr).is_err() {
        return writeln!(out, "error: cannot read the object at 0x{guard_addr:x}");
    }
    l.limits.retain(|l| l.addr != addr);
    l.limits.push(Limit { addr, min, max, guard_addr, guard, fixes: 0, paused: None });
    writeln!(out, "limiting 0x{addr:x}")
}

fn cmd_unlimit(out: &mut impl Write, limiter: &SharedLimiter, arg: &str) -> io::Result<()> {
    let Some(addr) = parse_addr(arg) else {
        return writeln!(out, "error: usage: unlimit <hex addr>");
    };
    limiter.lock().unwrap().limits.retain(|l| l.addr != addr);
    writeln!(out, "no longer limiting 0x{addr:x}")
}

fn cmd_limits(out: &mut impl Write, limiter: &SharedLimiter) -> io::Result<()> {
    let show = |b: Option<i32>| b.map_or("-".to_owned(), |b| b.to_string());
    for l in &limiter.lock().unwrap().limits {
        writeln!(
            out,
            "0x{:012x} {} {} fixed {} times, {}",
            l.addr,
            show(l.min),
            show(l.max),
            l.fixes,
            l.paused.unwrap_or("active")
        )?;
    }
    Ok(())
}

#[derive(Default)]
struct Session {
    pid: u32,
    mem: Option<File>,
    candidates: Vec<(u64, i32)>,
}

impl Session {
    fn read_i32(&self, addr: u64) -> Option<i32> {
        let mut b = [0u8; 4];
        self.mem.as_ref()?.read_exact_at(&mut b, addr).ok()?;
        Some(i32::from_le_bytes(b))
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

fn cmd_attach(out: &mut impl Write, s: &mut Session, limiter: &SharedLimiter, arg: &str) -> io::Result<()> {
    let Ok(pid) = arg.parse::<u32>() else {
        return writeln!(out, "error: usage: attach <pid>");
    };
    if let Some(ac) = anti_cheat(pid) {
        return writeln!(out, "error: refusing to attach, {ac} is loaded in this process");
    }
    match OpenOptions::new().read(true).write(true).open(format!("/proc/{pid}/mem")) {
        Ok(f) => {
            *limiter.lock().unwrap() = Limiter { mem: f.try_clone().ok(), limits: Vec::new() };
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

fn cmd_scan(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let Ok(value) = arg.parse::<i32>() else {
        return writeln!(out, "error: usage: scan <int32>");
    };
    let Some(mem) = s.mem.as_ref() else {
        return writeln!(out, "error: not attached");
    };
    let t = Instant::now();
    let needle = value.to_le_bytes();
    let mut found = Vec::new();
    let (mut bytes, mut unreadable) = (0u64, 0u64);
    let mut buf = vec![0u8; 4 << 20];
    for r in maps(s.pid)?.iter().filter(|r| scannable(r)) {
        let mut addr = r.start;
        while addr < r.end {
            let len = ((r.end - addr) as usize).min(buf.len());
            match mem.read_at(&mut buf[..len], addr) {
                Ok(n) if n > 0 => {
                    bytes += n as u64;
                    for off in (0..n.saturating_sub(3)).step_by(4) {
                        if buf[off..off + 4] == needle {
                            found.push((addr + off as u64, value));
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
        "{} matches in {} MiB ({} MiB unreadable) in {} ms",
        s.candidates.len(),
        bytes >> 20,
        unreadable >> 20,
        t.elapsed().as_millis()
    )
}

fn cmd_next(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let keep: Box<dyn Fn(i32, i32) -> bool> = match arg {
        "+" => Box::new(|old, new| new > old),
        "-" => Box::new(|old, new| new < old),
        "=" => Box::new(|old, new| new == old),
        "!" => Box::new(|old, new| new != old),
        v => match v.parse::<i32>() {
            Ok(want) => Box::new(move |_, new| new == want),
            Err(_) => return writeln!(out, "error: usage: next <int32>|+|-|=|!"),
        },
    };
    let before = s.candidates.len();
    let next: Vec<(u64, i32)> = s
        .candidates
        .iter()
        .filter_map(|&(addr, old)| {
            let new = s.read_i32(addr)?;
            keep(old, new).then_some((addr, new))
        })
        .collect();
    s.candidates = next;
    writeln!(out, "{before} -> {} matches", s.candidates.len())
}

fn cmd_list(out: &mut impl Write, s: &Session) -> io::Result<()> {
    for &(addr, _) in s.candidates.iter().take(20) {
        match s.read_i32(addr) {
            Some(v) => writeln!(out, "0x{addr:012x} = {v}")?,
            None => writeln!(out, "0x{addr:012x} = ??")?,
        }
    }
    if s.candidates.len() > 20 {
        writeln!(out, "... {} more", s.candidates.len() - 20)?;
    }
    Ok(())
}

fn cmd_write(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let mut it = arg.split_whitespace();
    let addr = it.next().and_then(parse_addr);
    let value = it.next().and_then(|v| v.parse::<i32>().ok());
    let (Some(addr), Some(value), Some(mem)) = (addr, value, s.mem.as_ref()) else {
        return writeln!(out, "error: usage: write <hex addr> <int32> (after attach)");
    };
    match mem.write_all_at(&value.to_le_bytes(), addr) {
        Ok(()) => writeln!(out, "wrote {value} to 0x{addr:x}, reads back {:?}", s.read_i32(addr)),
        Err(e) => writeln!(out, "error: write failed: {e}"),
    }
}

fn cmd_set(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    let (Ok(value), Some(mem)) = (arg.parse::<i32>(), s.mem.as_ref()) else {
        return writeln!(out, "error: usage: set <int32> (after attach)");
    };
    let written = s
        .candidates
        .iter()
        .filter(|(addr, _)| mem.write_all_at(&value.to_le_bytes(), *addr).is_ok())
        .count();
    writeln!(out, "wrote {value} to {written} of {} matches", s.candidates.len())
}

fn parse_addr(a: &str) -> Option<u64> {
    u64::from_str_radix(a.trim_start_matches("0x"), 16).ok()
}

fn cmd_peek(out: &mut impl Write, s: &Session, arg: &str) -> io::Result<()> {
    for a in arg.split_whitespace() {
        match parse_addr(a).and_then(|addr| Some((addr, s.read_i32(addr)?))) {
            Some((addr, v)) => writeln!(out, "0x{addr:012x} = {v}")?,
            None => writeln!(out, "{a} = ??")?,
        }
    }
    Ok(())
}

fn cmd_track(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    s.candidates = arg
        .split_whitespace()
        .filter_map(parse_addr)
        .filter_map(|addr| Some((addr, s.read_i32(addr)?)))
        .collect();
    writeln!(out, "tracking {} matches", s.candidates.len())
}

fn cmd_keep(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let Some(addr) = parse_addr(arg) else {
        return writeln!(out, "error: usage: keep <hex addr>");
    };
    let before = s.candidates.len();
    s.candidates.retain(|(a, _)| *a == addr);
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
    let (Some(target), Some(mem)) = (it.next().and_then(parse_addr), s.mem.as_ref()) else {
        return writeln!(out, "error: usage: sites <hex addr> [seconds] (after attach)");
    };
    let secs = it.next().and_then(|v| v.parse().ok()).unwrap_or(5);
    let mut hits: Vec<(u64, trace::Regs)> = Vec::new();
    {
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
            writeln!(out, "instruction before 0x{after:x}: not a [register+offset] access, skipped")?;
            continue;
        };
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

/// Finds a saved pattern in the game's code, waits for that instruction to
/// run and reads its base register: the value's address in this run.
fn cmd_resolve(out: &mut impl Write, s: &mut Session, arg: &str) -> io::Result<()> {
    let f: Vec<&str> = arg.split_whitespace().collect();
    let (Some(pat), Some(off), Some(base), Some(disp), Some(mem)) = (
        f.first().and_then(|p| parse_pattern(p)),
        f.get(1).and_then(|v| v.parse::<u64>().ok()),
        f.get(2).and_then(|r| trace::REG_NAMES.iter().position(|n| n == r)),
        f.get(3).and_then(|v| v.parse::<i64>().ok()),
        s.mem.as_ref(),
    ) else {
        return writeln!(out, "error: usage: resolve <pattern> <offset> <register> <displacement> [seconds]");
    };
    let secs = f.get(4).and_then(|v| v.parse().ok()).unwrap_or(10);
    let found = find_pattern(s.pid, mem, &pat, 2)?;
    let [code] = found.as_slice() else {
        return writeln!(out, "error: pattern found {} times (the game may not have run that code yet)", found.len());
    };
    let instr = code + off;
    let mut value_addr = None;
    {
        let mut tracer = match trace::Tracer::attach(s.pid) {
            Ok(t) => t,
            Err(e) => return writeln!(out, "error: cannot trace the game: {e}"),
        };
        tracer.arm(instr, trace::DR7_EXECUTE);
        tracer.watch(Duration::from_secs(secs), |hit| {
            value_addr = Some(trace::reg(&hit.regs, base as u8).wrapping_add(disp as u64));
            false
        });
    }
    match value_addr {
        Some(addr) => match s.read_i32(addr) {
            Some(v) => writeln!(out, "0x{addr:012x} = {v}"),
            None => writeln!(out, "error: resolved to unreadable 0x{addr:x}"),
        },
        None => writeln!(out, "error: the code at 0x{instr:x} did not run within {secs} s"),
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
            "attach" => cmd_attach(&mut out, &mut session, &limiter, arg),
            "scan" => cmd_scan(&mut out, &mut session, arg),
            "next" => cmd_next(&mut out, &mut session, arg),
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
                "commands: sandbox, info, ps [filter], attach <pid>, scan <n>, next <n>|+|-|=|!, list, peek <addr>..., keep <addr>, track <addr>..., sites <addr>, resolve <site>, limit <addr> <min> <max> <object>, unlimit <addr>, limits, write <addr> <n>, set <n>, quit"
            ),
        };
        if res.and_then(|_| writeln!(out, "end")).and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}
