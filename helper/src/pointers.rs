// Pointer paths: a value's address as a chain of pointers that starts at a fixed spot inside a
// module (the game's exe or one of its libraries), found the way Cheat Engine's pointer scan
// does. Following one needs no tracing, and it works for values that only shared code touches.
// Text form: "<module>+<hex offset>,<hex offset>,...": read the pointer at module+offset, add the
// next offset and read again, ...; the last offset leads to the value itself.
// Native games also start paths in the main thread's stack (Cheat Engine's THREADSTACK0), counted
// down from where its stack starts: "[stack]-1a8,...". Rust programs (Ruffle) keep their state
// there, reachable from no global.

use std::fs;
use crate::mem::Mem;
use std::io;

use crate::helper::{maps, scannable};

/// A file mapped into the game: where its static data lives.
pub struct Module {
    pub name: String,
    pub start: u64,
    pub end: u64,
    /// Another loaded file has the same name: offsets from it would be ambiguous.
    pub ambiguous: bool,
    /// The main thread's stack (`STACK`): offsets count down from `end`.
    pub stack: bool,
}

pub const STACK: &str = "[stack]";
/// How far below where the main thread's stack starts its slots count as roots: the frames of
/// main and the loop it runs stay put for the whole run, deeper ones come and go.
const STACK_ROOTS: u64 = 64 << 10;

fn read_u32(mem: &Mem, addr: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    mem.read_exact_at(&mut b, addr).ok().map(|_| u32::from_le_bytes(b))
}

/// Size of the Windows program mapped at `start` ("MZ" header), from its PE header.
fn pe_image(mem: &Mem, start: u64) -> Option<(u16, u64)> {
    let mut mz = [0u8; 2];
    mem.read_exact_at(&mut mz, start).ok()?;
    if &mz != b"MZ" {
        return None;
    }
    let pe = start + read_u32(mem, start + 0x3C)? as u64;
    if read_u32(mem, pe)? != 0x4550 {
        return None;
    }
    let machine = read_u32(mem, pe + 4)? as u16;
    // SizeOfImage sits at the same place in 32- and 64-bit optional headers.
    Some((machine, read_u32(mem, pe + 24 + 56)? as u64))
}

/// A library loaded from a copy in a temp folder under a new name each run (F.E.A.R. unpacks its
/// game DLLs to `AppData/Local/Temp/Gam39e4.tmp`): paths from it can't be found again.
fn temporary(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".tmp") || lower.contains("/appdata/local/temp/") || lower.starts_with("/tmp/")
}

pub fn modules(pid: u32, mem: &Mem) -> Vec<Module> {
    let regions = maps(pid).unwrap_or_default();
    let mut mods: Vec<(String, Module)> = Vec::new();
    for (i, r) in regions.iter().enumerate() {
        let path = &r.path;
        if !crate::helper::from_file(path) || path.starts_with("/dev/") || path.starts_with("/memfd:") || path.ends_with(" (deleted)") {
            // Zero-filled data (.bss) right after a file's mappings belongs to it.
            let after_file = i > 0 && !regions[i - 1].path.is_empty() && regions[i - 1].end == r.start;
            if path.is_empty() && after_file {
                if let Some((_, m)) = mods.iter_mut().find(|(_, m)| m.end == r.start) {
                    m.end = r.end;
                }
            }
            continue;
        }
        match mods.iter_mut().rev().find(|(p, m)| p == path && r.start <= m.end + (16 << 20)) {
            Some((_, m)) => m.end = m.end.max(r.end),
            None => {
                let name = path.rsplit('/').next().unwrap_or(path).to_owned();
                mods.push((path.clone(), Module { name, start: r.start, end: r.end, ambiguous: false, stack: false }));
            }
        }
    }
    // A Windows program's sections are not all mapped from its file under Wine.
    let mut wine = false;
    for (_, m) in &mut mods {
        if let Some((_, size)) = pe_image(mem, m.start) {
            m.end = m.end.max(m.start + size);
            wine = true;
        }
    }
    // Only programs and libraries have statics: data files mapped in (Ruffle maps its fonts)
    // were followed by heap memory taken for theirs, and paths from "DejaVuSans.ttf+9c018"
    // led nowhere after a restart.
    mods.retain(|(path, m)| {
        let mut magic = [0u8; 4];
        mem.read_exact_at(&mut magic, m.start).is_ok() && (magic == *b"\x7fELF" || magic[..2] == *b"MZ") && !temporary(path)
    });
    let mut mods: Vec<Module> = mods.into_iter().map(|(_, m)| m).collect();
    // Under Wine the game's threads run on stacks of Wine's own, not this one.
    if let (false, Some(top)) = (wine, crate::os::stack_start(pid)) {
        if let Some(r) = regions.iter().find(|r| r.start < top && top <= r.end) {
            let start = r.start.max(top.saturating_sub(STACK_ROOTS));
            mods.push(Module { name: STACK.to_owned(), start, end: top, ambiguous: false, stack: true });
        }
    }
    for i in 0..mods.len() {
        let dup = mods.iter().enumerate().any(|(j, o)| j != i && o.name.eq_ignore_ascii_case(&mods[i].name));
        mods[i].ambiguous = dup;
    }
    mods.sort_by_key(|m| m.start);
    mods
}

/// Pointer size of the game: from the exe's PE header under Wine, else from the ELF header.
pub fn pointer_width(pid: u32, mem: &Mem, mods: &[Module], exe: &str) -> usize {
    if let Some(m) = mods.iter().find(|m| m.name.eq_ignore_ascii_case(exe)) {
        if let Some((machine, _)) = pe_image(mem, m.start) {
            return if machine == 0x14C { 4 } else { 8 };
        }
    }
    let elf = crate::os::exe(pid).and_then(|p| fs::read(p).ok());
    match elf.as_deref().and_then(|e| e.get(4)) {
        Some(1) => 4,
        _ => 8,
    }
}

fn read_ptr(mem: &Mem, addr: u64, width: usize) -> Option<u64> {
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b[..width], addr).ok()?;
    Some(u64::from_le_bytes(b))
}

#[derive(Clone, Debug, PartialEq)]
pub struct PtrPath {
    pub module: String,
    pub base: u64,
    pub offsets: Vec<u64>,
}

impl PtrPath {
    /// "Forager.exe+177a64c,44,2c,10,3a8,0" (a space in the module name is written as %20).
    /// No module ("+3a1b2c,66c") = an absolute address, only good for this run of the game.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split(',');
        let root = parts.next()?;
        let (module, base) = match root.strip_prefix(STACK) {
            Some(below) => (STACK, below.strip_prefix('-')?),
            None => root.rsplit_once('+')?,
        };
        let hex = |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok();
        let offsets = parts.map(hex).collect::<Option<Vec<_>>>()?;
        if offsets.is_empty() {
            return None;
        }
        Some(PtrPath { module: module.replace("%20", " "), base: hex(base)?, offsets })
    }

    pub fn text(&self) -> String {
        let offsets: Vec<String> = self.offsets.iter().map(|o| format!("{o:x}")).collect();
        let sign = if self.module == STACK { '-' } else { '+' };
        format!("{}{sign}{:x},{}", self.module.replace(' ', "%20"), self.base, offsets.join(","))
    }

    /// Where the path leads right now. Values are aligned and in the game's memory: a path that
    /// ends anywhere else went through something that changed (Age of War: 64 of 247 paths shared
    /// a step into junk and agreed on an unmapped address, outvoting the real one).
    pub fn follow(&self, mem: &Mem, mods: &[Module], width: usize) -> Option<u64> {
        Some(self.pointers(mem, mods, width)?.last()? + self.offsets.last()?).filter(|&a| a % 4 == 0 && read_u32(mem, a).is_some())
    }

    /// The pointers read along the path right now, one per offset.
    fn pointers(&self, mem: &Mem, mods: &[Module], width: usize) -> Option<Vec<u64>> {
        let root = match self.module.as_str() {
            "" => self.base,
            name => {
                let m = mods.iter().find(|m| !m.ambiguous && m.name.eq_ignore_ascii_case(name))?;
                if m.stack { m.end.checked_sub(self.base)? } else { m.start + self.base }
            }
        };
        let mut read = vec![read_ptr(mem, root, width)?];
        for off in &self.offsets[..self.offsets.len() - 1] {
            if *read.last()? < 0x10000 {
                return None;
            }
            read.push(read_ptr(mem, read.last()? + off, width)?);
        }
        (*read.last()? >= 0x10000).then_some(read)
    }
}

/// Where most pointer paths lead now.
pub struct Vote {
    pub addr: u64,
    /// Which paths lead there.
    pub leads: Vec<bool>,
    /// How many paths lead to the most common other address.
    pub next: usize,
}

impl Vote {
    pub fn agree(&self) -> usize {
        self.leads.iter().filter(|l| **l).count()
    }

    /// Clear enough to write there: several paths agree (or the only path given leads there)
    /// and far fewer lead anywhere else. One path out of thousands of unconfirmed ones leading
    /// to some aligned address is no evidence: writing there corrupts the game's memory.
    pub fn clear(&self) -> bool {
        let agree = self.agree();
        (agree >= 2 || self.leads.len() == 1) && agree > 2 * self.next
    }
}

/// How many times more paths may still lead to where a value was than to the new place it moved
/// to (see `tally`). paths-test on Windows: 2866 to the freed old gems, 125 to the new ones.
const MOVED_SHARE: usize = 32;

/// The address most of `paths` lead to now, and which paths lead there. Paths that stopped
/// working or lead elsewhere went through something that changed. `at`: where the value was
/// last found in this run (0: not yet), `left`: the places it moved away from in this run.
pub fn vote(mem: &Mem, mods: &[Module], width: usize, paths: &[PtrPath], at: u64, left: &[u64]) -> Option<Vote> {
    let ends: Vec<Option<u64>> = paths.iter().map(|p| p.follow(mem, mods, width)).collect();
    // Only Windows' heap (and Wine's) leaves freed objects readable; elsewhere paths through
    // them break, and the most paths lead to where the value is now.
    let windows = mods.iter().any(|m| m.name.eq_ignore_ascii_case("ntdll.dll"));
    if windows { tally(paths, &ends, at, left) } else { tally(paths, &ends, 0, &[]) }
}

/// `vote` on where the paths lead now. Windows keeps freed memory readable, so after the game
/// moves a value the paths that ran through the old object still lead to it, often far more of
/// them than follow the game (paths-test: 2866 of 2998, through the leftovers of ntdll's, the C
/// runtime's and the game's own bookkeeping). So a place the value moved away from only wins when
/// nothing else does, and a new place beats where it was when the paths leading there clearly
/// agree, start in more than one module and are more than a stray few.
fn tally(paths: &[PtrPath], ends: &[Option<u64>], at: u64, left: &[u64]) -> Option<Vote> {
    let mut counts: Vec<(u64, usize)> = Vec::new();
    for e in ends.iter().flatten() {
        match counts.iter_mut().find(|(a, _)| a == e) {
            Some((_, n)) => *n += 1,
            None => counts.push((*e, 1)),
        }
    }
    let live: Vec<(u64, usize)> = counts.iter().copied().filter(|(a, _)| *a == at || !left.contains(a)).collect();
    let pool = if live.is_empty() { &counts } else { &live };
    let vote = |addr: u64, next: usize| Vote { addr, leads: ends.iter().map(|e| *e == Some(addr)).collect(), next };
    let stayed = pool.iter().find(|(a, _)| at != 0 && *a == at).map_or(0, |(_, n)| *n);
    if let Some((addr, n)) = top(pool, &[at]).filter(|_| stayed > 0) {
        let next = top(pool, &[at, addr]).map_or(0, |(_, n)| n);
        let mut modules: Vec<&str> = paths.iter().zip(ends).filter(|(_, e)| **e == Some(addr)).map(|(p, _)| p.module.as_str()).collect();
        modules.sort_unstable();
        modules.dedup();
        if n >= 2 && n > 2 * next && n * MOVED_SHARE >= stayed && modules.len() >= 2 {
            return Some(vote(addr, next));
        }
    }
    let best = top(pool, &[])?;
    Some(vote(best.0, top(pool, &[best.0]).map_or(0, |(_, n)| n)))
}

/// Notes that a value moved from `from` (0: not found before) to `to` in `left`, the places it
/// moved away from (`vote`).
pub fn moved_away(left: &mut Vec<u64>, from: u64, to: u64) {
    left.retain(|&a| a != to);
    if from != 0 && from != to && !left.contains(&from) {
        left.push(from);
    }
}

/// The address most paths lead to, but for `except`, and how many; ties go to the address of the
/// earliest path (paths come best first).
fn top(counts: &[(u64, usize)], except: &[u64]) -> Option<(u64, usize)> {
    counts.iter().filter(|(a, _)| !except.contains(a)).fold(None, |best, &(a, n)| match best {
        Some((_, bn)) if bn >= n => best,
        _ => Some((a, n)),
    })
}

/// Every aligned pointer-sized word in writable memory that points (aligned) into writable memory, as
/// (pointer, where it is), sorted by pointer. 32-bit games pack both into one word.
pub enum Pointers {
    Narrow(Vec<u64>),
    Wide(Vec<(u64, u64)>),
}

impl Pointers {
    pub fn len(&self) -> usize {
        match self {
            Pointers::Narrow(v) => v.len(),
            Pointers::Wide(v) => v.len(),
        }
    }

    pub fn get(&self, i: usize) -> (u64, u64) {
        match self {
            Pointers::Narrow(v) => (v[i] >> 32, v[i] & 0xFFFF_FFFF),
            Pointers::Wide(v) => v[i],
        }
    }
}

pub fn collect_pointers(pid: u32, mem: &Mem, width: usize) -> io::Result<(Pointers, u64)> {
    // A 32-bit game's own memory is all below 4 GiB (Wine's 64-bit parts may not be).
    let rw: Vec<(u64, u64)> = maps(pid)?
        .iter()
        .filter(|r| scannable(r) && (width == 8 || r.end <= 1 << 32))
        .map(|r| (r.start, r.end))
        .collect();
    let (lo, hi) = (rw.first().map_or(0, |r| r.0), rw.last().map_or(0, |r| r.1));
    // Objects and their fields are aligned: other values that look like pointers are noise.
    let inside = |v: u64| {
        if v < lo.max(0x10000) || v >= hi || v % 4 != 0 {
            return false;
        }
        let i = rw.partition_point(|r| r.0 <= v);
        i > 0 && v < rw[i - 1].1
    };
    let mut narrow = Vec::new();
    let mut wide = Vec::new();
    let mut bytes = 0u64;
    let mut buf = vec![0u8; 4 << 20];
    for &(start, end) in &rw {
        let mut addr = start;
        while addr < end {
            let len = ((end - addr) as usize).min(buf.len());
            let len = mem.read_at(&mut buf[..len], addr).unwrap_or(0);
            if len == 0 {
                // An unreadable page: the rest of the region may be readable.
                addr = (addr | 0xfff) + 1;
                continue;
            }
            bytes += len as u64;
            for off in (0..len - len % width).step_by(width) {
                let at = addr + off as u64;
                if width == 4 {
                    let v = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as u64;
                    if inside(v) {
                        narrow.push(v << 32 | at);
                    }
                } else {
                    let v = u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
                    if inside(v) {
                        wide.push((v, at));
                    }
                }
            }
            addr += len as u64;
        }
    }
    Ok(if width == 4 {
        narrow.sort_unstable();
        (Pointers::Narrow(narrow), bytes)
    } else {
        wide.sort_unstable();
        (Pointers::Wide(wide), bytes)
    })
}

pub struct ScanResult {
    /// Best first: rooted in the game's own exe, then smaller offsets, then fewer steps.
    pub paths: Vec<PtrPath>,
    pub pointers: usize,
    pub bytes: u64,
    /// Pointers found at each step back from the value.
    pub levels: Vec<usize>,
    /// Steps into a neighbouring object on the paths kept, and paths dropped for having more.
    pub crossings: usize,
    pub dropped: usize,
}

/// One bit per pointer.
struct Bits(Vec<u64>);

impl Bits {
    fn new(n: usize) -> Self {
        Bits(vec![0; n.div_ceil(64)])
    }

    fn set(&mut self, i: usize) {
        self.0[i / 64] |= 1 << (i % 64);
    }

    fn get(&self, i: usize) -> bool {
        self.0[i / 64] >> (i % 64) & 1 == 1
    }

    fn count(&self) -> usize {
        self.0.iter().map(|w| w.count_ones() as usize).sum()
    }
}

/// Searches backwards from `target`: step 1 marks the pointers that point at most `max_off`
/// bytes before the target, step 2 the ones pointing just before where a step-1 pointer is
/// kept, and so on. A pointer can be on several steps (objects next to each other make short
/// paths across them, and the real, longer one must not be lost to those). Pointers kept
/// inside a module are where paths start: they are there in every run.
/// `collected`: the pointers from `collect_pointers`, when there are recent ones. `loose`: skip
/// the strict pass (the caller found its paths unsteady).
#[allow(clippy::too_many_arguments)]
pub fn scan(
    pid: u32,
    mem: &Mem,
    width: usize,
    exe: &str,
    target: u64,
    depth: usize,
    max_off: u64,
    max_paths: usize,
    collected: Option<(Pointers, u64)>,
    loose: bool,
) -> io::Result<ScanResult> {
    let mods = modules(pid, mem);
    let roots: Vec<&Module> = mods.iter().filter(|m| !m.ambiguous).collect();
    let module_of = |a: u64| {
        let i = roots.partition_point(|m| m.start <= a);
        (i > 0 && a < roots[i - 1].end).then(|| roots[i - 1])
    };
    let (pointers, bytes) = match collected {
        Some(c) => c,
        None => collect_pointers(pid, mem, width)?,
    };
    let n = pointers.len();
    let addr = |i: usize| pointers.get(i).1;
    let mut by_addr: Vec<u32> = (0..n as u32).collect();
    by_addr.sort_unstable_by_key(|&i| addr(i as usize));
    let mut fixed = Bits::new(n);
    for i in 0..n {
        if module_of(addr(i)).is_some() {
            fixed.set(i);
        }
    }
    let mut near: Vec<(u64, u64)> = (0..n).map(|i| pointers.get(i)).filter(|(p, a)| p.abs_diff(*a) <= max_off).map(|(p, a)| (a, p)).collect();
    near.sort_unstable();
    let mut graph = Graph { pointers: &pointers, by_addr: &by_addr, fixed: &fixed, near: &near, levels: Vec::new(), target, max_off, strict: true };
    // First only along pointers between objects; if that finds nothing, also across them.
    let mut found = Vec::new();
    for strict in [true, false].into_iter().skip(loose as usize) {
        graph.strict = strict;
        graph.levels = graph.levels(depth);
        let is_exe = |i: usize| module_of(addr(i)).is_some_and(|m| m.name.eq_ignore_ascii_case(exe));
        let mut starts: Vec<(usize, usize)> = Vec::new();
        for (k, level) in graph.levels.iter().enumerate() {
            starts.extend((0..n).filter(|&i| fixed.get(i) && level.get(i)).map(|i| (k + 1, i)));
        }
        starts.sort_by_key(|&(k, i)| (!is_exe(i), k, addr(i)));
        const PER_START: usize = 64;
        for (k, i) in starts {
            if found.len() >= 100 * max_paths {
                break;
            }
            let m = module_of(addr(i)).unwrap();
            let mut offsets = Vec::new();
            graph.walk(k, pointers.get(i).0, &mut Vec::new(), &mut offsets, PER_START);
            let base = if m.stack { m.end - addr(i) } else { addr(i) - m.start };
            found.extend(offsets.into_iter().map(|offsets| PtrPath { module: m.name.clone(), base, offsets }));
        }
        // Only paths that still work now (memory moved on while the scan ran).
        found.retain(|p| p.follow(mem, &mods, width) == Some(target));
        if !found.is_empty() {
            break;
        }
    }
    // Across objects: keep the paths that cross the fewest times. Loose: keep them all, the
    // game's own module first, then by crossings (a step inside one object can look like one:
    // Dawn of War's player object has small arrays pointing 12 bytes ahead at their own items).
    let crossings: Vec<usize> = found.iter().map(|p| p.pointers(mem, &mods, width).map_or(usize::MAX, |r| graph.crossings(&r, &p.offsets))).collect();
    let fewest = crossings.iter().copied().min().unwrap_or(0);
    let before = found.len();
    let mut paths: Vec<(PtrPath, usize)> = found.into_iter().zip(crossings).filter(|(_, c)| loose || *c == fewest).collect();
    let dropped = before - paths.len();
    let biggest = |p: &PtrPath| p.offsets.iter().max().copied().unwrap_or(0);
    paths.sort_by_key(|(p, c)| (!p.module.eq_ignore_ascii_case(exe), *c, biggest(p), p.offsets.len()));
    let mut paths: Vec<PtrPath> = paths.into_iter().map(|(p, _)| p).collect();
    paths.dedup();
    paths.truncate(max_paths);
    let levels = graph.levels.iter().map(Bits::count).collect();
    Ok(ScanResult { paths, pointers: n, bytes, levels, crossings: fewest, dropped })
}

struct Graph<'a> {
    pointers: &'a Pointers,
    /// Pointers in the order of where they are kept.
    by_addr: &'a [u32],
    /// Pointers kept inside a module.
    fixed: &'a Bits,
    /// Pointers to near their own place, as (where, pointer), sorted: the ones that can point
    /// from one object into the next.
    near: &'a [(u64, u64)],
    levels: Vec<Bits>,
    target: u64,
    max_off: u64,
    /// Never step from one object into the next.
    strict: bool,
}

impl Graph<'_> {
    fn addr(&self, j: u32) -> u64 {
        self.pointers.get(j as usize).1
    }

    /// Whether going from `p` to `end` steps from one object into another: a pointer in
    /// between points further into that stretch (the first object pointing at the second).
    fn crosses(&self, p: u64, end: u64) -> bool {
        let from = self.near.partition_point(|&(a, _)| a < p);
        self.near[from..].iter().take_while(|&&(a, _)| a < end).any(|&(_, q)| p < q && q <= end)
    }

    fn crossings(&self, read: &[u64], offsets: &[u64]) -> usize {
        read.iter().zip(offsets).filter(|&(&p, &off)| self.crosses(p, p + off)).count()
    }

    fn levels(&self, depth: usize) -> Vec<Bits> {
        let n = self.pointers.len();
        let mut levels: Vec<Bits> = Vec::new();
        for k in 0..depth {
            // Where the previous step's pointers are kept, in address order (step 0: the value).
            let mut targets: Box<dyn Iterator<Item = u64>> = match k {
                0 => Box::new(std::iter::once(self.target)),
                _ => {
                    let prev = &levels[k - 1];
                    let fixed = self.fixed;
                    Box::new(self.by_addr.iter().filter(move |&&i| prev.get(i as usize) && !fixed.get(i as usize)).map(|&i| self.addr(i)))
                }
            };
            let mut next = Bits::new(n);
            let mut t = targets.next();
            for i in 0..n {
                let (ptr, _) = self.pointers.get(i);
                while t.is_some_and(|t| t < ptr) {
                    t = targets.next();
                }
                // Only the nearest target counts: if the step there crosses into another
                // object, the step to any one further away does too.
                match t {
                    Some(t) if t - ptr <= self.max_off && !(self.strict && self.crosses(ptr, t)) => next.set(i),
                    Some(_) => {}
                    None => break,
                }
            }
            drop(targets);
            levels.push(next);
        }
        levels
    }

    /// Ways from a pointer `ptr` on step `level` down to the value, as offsets: at most `cap`,
    /// and at most a few through each next pointer, so that one busy branch (often noise, like
    /// an int that looks like a pointer) can't crowd out the others.
    fn walk(&self, level: usize, ptr: u64, offsets: &mut Vec<u64>, found: &mut Vec<Vec<u64>>, cap: usize) {
        const PER_BRANCH: usize = 4;
        if level == 1 {
            offsets.push(self.target - ptr);
            found.push(offsets.clone());
            offsets.pop();
            return;
        }
        let below = &self.levels[level - 2];
        let from = self.by_addr.partition_point(|&j| self.addr(j) < ptr);
        let start = found.len();
        for &j in self.by_addr[from..].iter().take_while(|&&j| self.addr(j) - ptr <= self.max_off) {
            let used = found.len() - start;
            if used >= cap {
                return;
            }
            // A path through another fixed address is a longer copy of that one's own.
            if !below.get(j as usize) || self.fixed.get(j as usize) || (self.strict && self.crosses(ptr, self.addr(j))) {
                continue;
            }
            offsets.push(self.addr(j) - ptr);
            self.walk(level - 1, self.pointers.get(j as usize).0, offsets, found, PER_BRANCH.min(cap - used));
            offsets.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_read_back() {
        for text in ["Forager.exe+177a64c,44,2c,0", "[stack]-1a8,10,468", "+3a1b2c,66c", "lib%20x-1.so+20,8"] {
            assert_eq!(PtrPath::parse(text).unwrap().text(), text);
        }
        let p = PtrPath::parse("[stack]-1a8,10").unwrap();
        assert_eq!((p.module.as_str(), p.base), (STACK, 0x1a8));
        assert!(PtrPath::parse("[stack]+1a8,10").is_none());
    }

    #[test]
    fn temp_copies_are_no_roots() {
        assert!(temporary("/home/u/Games/pfx/drive_c/users/steamuser/AppData/Local/Temp/Gam39e4.tmp"));
        assert!(temporary("/tmp/.mount_x/lib.so"));
        assert!(!temporary("/run/media/u/Games/FEAR/FEAR.exe"));
        assert!(!temporary("/usr/lib/libtmpfiles.so"));
    }

    #[test]
    fn moved_beats_stayed() {
        let (old, new, junk) = (0x1000, 0x2000, 0x3000);
        let paths: Vec<PtrPath> = ["ntdll.dll+10,8", "ntdll.dll+18,8", "ucrtbase.dll+20,8", "target.exe+30,8", "target.exe+40,8", "msvcrt.dll+50,8", "ntdll.dll+60,8"]
            .iter()
            .map(|t| PtrPath::parse(t).unwrap())
            .collect();
        let ends = [Some(old), Some(old), Some(old), Some(new), Some(old), Some(new), Some(junk)];
        // Not knowing where it was, the most paths win.
        assert_eq!(tally(&paths, &ends, 0, &[]).unwrap().addr, old);
        // Half as many again leading somewhere else: no clear move.
        assert_eq!(tally(&paths, &ends, old, &[]).unwrap().addr, old);
        // The game moved it: the paths that followed win, clearly.
        let ends = [Some(old), Some(old), Some(old), Some(new), Some(old), Some(new), None];
        let v = tally(&paths, &ends, old, &[]).unwrap();
        assert_eq!((v.addr, v.clear()), (new, true));
        // Next time it's at the new place, and the old one is a place it left.
        let v = tally(&paths, &ends, new, &[old]).unwrap();
        assert_eq!((v.addr, v.clear()), (new, true));
        // Paths from one module agreeing on a new place are no move.
        let ends = [Some(old), Some(old), Some(old), Some(old), Some(old), Some(junk), Some(junk)];
        let paths: Vec<PtrPath> = ["a.exe+1,0", "a.exe+2,0", "a.exe+3,0", "a.exe+4,0", "a.exe+5,0", "ntdll.dll+6,0", "ntdll.dll+7,0"]
            .iter()
            .map(|t| PtrPath::parse(t).unwrap())
            .collect();
        assert_eq!(tally(&paths, &ends, old, &[]).unwrap().addr, old);
        // Nor are a stray few against thousands.
        let many: Vec<PtrPath> = (0..100).map(|i| PtrPath::parse(&format!("m{}.dll+{i:x},0", i % 3)).unwrap()).collect();
        let mut ends = vec![Some(old); 100];
        (ends[0], ends[1]) = (Some(junk), Some(junk));
        assert_eq!(tally(&many, &ends, old, &[]).unwrap().addr, old);
    }
}
