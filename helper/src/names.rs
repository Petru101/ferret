// Named paths: a value reached from an object the game names, for games whose objects all live
// in memory allocated while they run (Unity's Mono and IL2CPP), where no pointer path starts in
// a module. Their strings are objects with a two-pointer header, a 4-byte length and UTF-16
// text; arrays have the header, a bounds pointer, a length and then their elements.
// Text form: "Inventory"@10,28,10,*,10.10="$item_wood",+38 = an object whose field at 0x10 is
// the string "Inventory"; read the pointer at +0x28, then at +0x10; every element of that array
// whose [+10]+10 is the string "$item_wood"; the value at +0x38 of each (Valheim's wood stacks).
// Names are percent-encoded (no spaces, commas, quotes), so a path is one word.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

use crate::helper::{maps, scannable};
use crate::pointers::Pointers;

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Read the pointer at this offset.
    Field(u64),
    /// Every element of the array.
    Each,
    /// Keep it only when following these offsets leads to a string with this text.
    Named(Vec<u64>, String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct NamedPath {
    pub root: String,
    pub root_field: u64,
    pub steps: Vec<Step>,
    pub value: u64,
}

fn encode(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c == '%' || c == '"' || c == ',' || c == '=' || c == '@' || c.is_whitespace() || c.is_control() {
            let mut b = [0u8; 4];
            for byte in c.encode_utf8(&mut b).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn decode(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(text.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn quoted(text: &str) -> Option<String> {
    decode(text.strip_prefix('"')?.strip_suffix('"')?)
}

fn hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s, 16).ok()
}

impl NamedPath {
    pub fn parse(text: &str) -> Option<Self> {
        let parts: Vec<&str> = text.split(',').collect();
        let (first, rest) = parts.split_first()?;
        let (last, middle) = rest.split_last()?;
        let (root, field) = first.rsplit_once('@')?;
        let steps = middle
            .iter()
            .map(|s| match *s {
                "*" => Some(Step::Each),
                s => match s.split_once('=') {
                    Some((chain, name)) => Some(Step::Named(chain.split('.').map(hex).collect::<Option<_>>()?, quoted(name)?)),
                    None => Some(Step::Field(hex(s)?)),
                },
            })
            .collect::<Option<Vec<_>>>()?;
        Some(NamedPath { root: quoted(root)?, root_field: hex(field)?, steps, value: hex(last.strip_prefix('+')?)? })
    }

    pub fn text(&self) -> String {
        let mut parts = vec![format!("\"{}\"@{:x}", encode(&self.root), self.root_field)];
        for s in &self.steps {
            parts.push(match s {
                Step::Field(off) => format!("{off:x}"),
                Step::Each => "*".into(),
                Step::Named(chain, name) => {
                    let chain: Vec<String> = chain.iter().map(|o| format!("{o:x}")).collect();
                    format!("{}=\"{}\"", chain.join("."), encode(name))
                }
            });
        }
        parts.push(format!("+{:x}", self.value));
        parts.join(",")
    }

    /// In words: "every "$item_wood" in "Inventory"".
    pub fn describe(&self) -> String {
        let names: Vec<&str> = self.steps.iter().filter_map(|s| if let Step::Named(_, n) = s { Some(n.as_str()) } else { None }).collect();
        match names.last() {
            Some(n) => format!("every \"{n}\" in \"{}\"", self.root),
            None => format!("in \"{}\"", self.root),
        }
    }
}

const MAX_NAME: usize = 64;
const MAX_ELEMENTS: u64 = 1 << 16;
/// Followed objects per step: a wrong root can't make a walk take forever.
const MAX_FRONTIER: usize = 1 << 16;
const CHUNK: usize = 4 << 20;

/// Text that reads as a name: two letters or more of plain ASCII, and nothing but printable
/// ASCII and letters. Two random UTF-16 units often decode to letters of some script ("즨̚" in
/// Forager, which has no named objects at all).
fn good_name(units: &[u16]) -> Option<String> {
    let t = String::from_utf16(units).ok()?;
    let ascii_letters = t.chars().filter(char::is_ascii_alphabetic).count();
    (ascii_letters >= 2 && t.chars().all(|c| c.is_ascii_graphic() || c == ' ' || c.is_alphabetic())).then_some(t)
}

/// The game's writable memory, read as objects.
pub struct Heap<'a> {
    pub pid: u32,
    mem: &'a File,
    width: usize,
    rw: Vec<(u64, u64)>,
}

impl<'a> Heap<'a> {
    pub fn new(pid: u32, mem: &'a File, width: usize) -> io::Result<Self> {
        let rw = maps(pid)?
            .iter()
            .filter(|r| scannable(r) && (width == 8 || r.end <= 1 << 32))
            .map(|r| (r.start, r.end))
            .collect();
        Ok(Heap { pid, mem, width, rw })
    }

    fn w(&self) -> u64 {
        self.width as u64
    }

    pub fn file(&self) -> &File {
        self.mem
    }

    /// Whether `v` is an aligned address in writable memory.
    pub fn is_pointer(&self, v: u64) -> bool {
        self.inside(v)
    }

    fn inside(&self, v: u64) -> bool {
        if v % self.w() != 0 {
            return false;
        }
        let i = self.rw.partition_point(|r| r.0 <= v);
        i > 0 && v < self.rw[i - 1].1
    }

    fn word(&self, at: u64) -> Option<u64> {
        let mut b = [0u8; 8];
        self.mem.read_exact_at(&mut b[..self.width], at).ok()?;
        Some(u64::from_le_bytes(b))
    }

    fn ptr(&self, at: u64) -> Option<u64> {
        self.word(at).filter(|&p| self.inside(p))
    }

    /// The text of the string object at `obj`.
    fn string(&self, obj: u64) -> Option<Vec<u16>> {
        let len_at = obj + 2 * self.w();
        let mut l = [0u8; 4];
        self.mem.read_exact_at(&mut l, len_at).ok()?;
        let len = i32::from_le_bytes(l);
        if !(1..=MAX_NAME as i32).contains(&len) {
            return None;
        }
        let mut b = vec![0u8; 2 * len as usize];
        self.mem.read_exact_at(&mut b, len_at + 4).ok()?;
        Some(b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect())
    }

    fn name_of(&self, obj: u64) -> Option<String> {
        good_name(&self.string(obj)?)
    }

    /// The elements of the array at `obj`.
    fn elements(&self, obj: u64) -> Vec<u64> {
        let w = self.w();
        let (Some(0), Some(len)) = (self.word(obj + 2 * w), self.word(obj + 3 * w)) else { return Vec::new() };
        if len == 0 || len > MAX_ELEMENTS {
            return Vec::new();
        }
        let mut b = vec![0u8; (len * w) as usize];
        if self.mem.read_exact_at(&mut b, obj + 4 * w).is_err() {
            return Vec::new();
        }
        b.chunks(self.width)
            .map(|c| {
                let mut x = [0u8; 8];
                x[..c.len()].copy_from_slice(c);
                u64::from_le_bytes(x)
            })
            .filter(|&p| self.inside(p))
            .collect()
    }

    /// The array whose elements include the word at `at`.
    fn array_holding(&self, at: u64) -> Option<u64> {
        let w = self.w();
        const BACK: u64 = 4096;
        let i = self.rw.partition_point(|r| r.0 <= at);
        let region = *self.rw.get(i.checked_sub(1)?)?;
        let lo = at.saturating_sub(4 * w + BACK * w).max(region.0);
        if at < lo + 4 * w {
            return None;
        }
        let mut b = vec![0u8; (at - lo) as usize];
        self.mem.read_exact_at(&mut b, lo).ok()?;
        let word = |a: u64| {
            let i = (a - lo) as usize;
            let mut x = [0u8; 8];
            x[..self.width].copy_from_slice(&b[i..i + self.width]);
            u64::from_le_bytes(x)
        };
        // Bytes that only look like a header ([pointer, 0, 0, small number] is common) are
        // followed by more than pointers: an array of objects holds only pointers and nulls.
        let holds_objects = |h: u64, len: u64| {
            let mut e = vec![0u8; (len * w) as usize];
            self.mem.read_exact_at(&mut e, h + 4 * w).is_ok()
                && e.chunks(self.width).all(|c| {
                    let mut x = [0u8; 8];
                    x[..c.len()].copy_from_slice(c);
                    let v = u64::from_le_bytes(x);
                    v == 0 || self.inside(v)
                })
        };
        (0..BACK).take_while(|k| at >= lo + 4 * w + k * w).find_map(|k| {
            let h = at - 4 * w - k * w;
            let len = word(h + 3 * w);
            (word(h + 2 * w) == 0 && len > k && len <= MAX_ELEMENTS && self.inside(word(h)) && holds_objects(h, len)).then_some(h)
        })
    }

    fn leads_to(&self, obj: u64, chain: &[u64], name: &[u16]) -> bool {
        let mut p = obj;
        for off in chain {
            match self.ptr(p + off) {
                Some(q) => p = q,
                None => return false,
            }
        }
        self.string(p).as_deref() == Some(name)
    }

    /// Where the path leads from these roots (objects named like its root).
    pub fn walk(&self, roots: &[u64], path: &NamedPath) -> Vec<u64> {
        let root: Vec<u16> = path.root.encode_utf16().collect();
        let mut cur: Vec<u64> = roots.iter().copied().filter(|&r| self.leads_to(r, &[path.root_field], &root)).collect();
        for step in &path.steps {
            cur = match step {
                Step::Field(off) => cur.iter().filter_map(|&p| self.ptr(p + off)).collect(),
                Step::Each => cur.iter().flat_map(|&p| self.elements(p)).collect(),
                Step::Named(chain, name) => {
                    let name: Vec<u16> = name.encode_utf16().collect();
                    cur.into_iter().filter(|&p| self.leads_to(p, chain, &name)).collect()
                }
            };
            cur.sort_unstable();
            cur.dedup();
            cur.truncate(MAX_FRONTIER);
        }
        cur.iter().map(|p| p + path.value).filter(|a| a % 4 == 0).collect()
    }

    /// Reads all writable memory in chunks that overlap by `overlap` bytes; `f` gets the
    /// chunk's address, its bytes and how many of them start something new.
    pub fn each_chunk(&self, overlap: usize, mut f: impl FnMut(u64, &[u8], usize)) {
        let mut buf = vec![0u8; CHUNK + overlap];
        for &(start, end) in &self.rw {
            let mut addr = start;
            while addr < end {
                let want = ((end - addr) as usize).min(CHUNK + overlap);
                let len = self.mem.read_at(&mut buf[..want], addr).unwrap_or(0);
                if len == 0 {
                    break;
                }
                let fresh = len.min(CHUNK);
                f(addr, &buf[..len], fresh);
                addr += fresh as u64;
            }
        }
    }

    /// String objects with each of these texts, in one pass over memory.
    pub fn find_strings(&self, texts: &[Vec<u16>]) -> Vec<Vec<u64>> {
        let w = self.width;
        let longest = texts.iter().map(Vec::len).max().unwrap_or(0);
        let mut found = vec![Vec::new(); texts.len()];
        self.each_chunk(2 * w + 4 + 2 * longest, |addr, b, fresh| {
            for off in (0..fresh).step_by(w) {
                let at = off + 2 * w;
                let Some(l) = b.get(at..at + 4) else { break };
                let len = i32::from_le_bytes([l[0], l[1], l[2], l[3]]);
                if len < 2 || len as usize > longest {
                    continue;
                }
                for (i, t) in texts.iter().enumerate() {
                    if t.len() == len as usize
                        && b.get(at + 4..at + 4 + 2 * t.len()).is_some_and(|c| c.chunks(2).zip(t).all(|(c, u)| u16::from_le_bytes([c[0], c[1]]) == *u))
                    {
                        found[i].push(addr + off as u64);
                    }
                }
            }
        });
        found
    }

    /// Where pointers to any of `targets` are kept, in one pass over memory.
    fn referrers(&self, targets: &[u64]) -> Vec<u64> {
        let mut t = targets.to_vec();
        t.sort_unstable();
        let (Some(&lo), Some(&hi)) = (t.first(), t.last()) else { return Vec::new() };
        let w = self.width;
        let mut found = Vec::new();
        self.each_chunk(0, |addr, b, fresh| {
            for off in (0..fresh - fresh % w).step_by(w) {
                let mut x = [0u8; 8];
                x[..w].copy_from_slice(&b[off..off + w]);
                let v = u64::from_le_bytes(x);
                if v >= lo && v <= hi && t.binary_search(&v).is_ok() {
                    found.push(addr + off as u64);
                }
            }
        });
        found
    }

    /// The objects named like the path's root (two passes over memory: a few seconds).
    pub fn find_roots(&self, path: &NamedPath) -> Vec<u64> {
        let text: Vec<u16> = path.root.encode_utf16().collect();
        let strings = self.find_strings(&[text.clone()]).remove(0);
        let mut roots: Vec<u64> = self
            .referrers(&strings)
            .into_iter()
            .filter_map(|at| at.checked_sub(path.root_field))
            .filter(|&r| self.leads_to(r, &[path.root_field], &text))
            .collect();
        roots.sort_unstable();
        roots.dedup();
        roots
    }

    /// Names the object at `obj` has: its fields that are strings ([f]) and the strings in the
    /// objects its fields point to ([f, f2]). Objects lie next to each other: one ends where
    /// the next one something points to starts.
    fn names(&self, pointers: &Pointers, obj: u64, one_hop: bool) -> Vec<(Vec<u64>, String)> {
        const FIELDS: u64 = 0x100;
        const INNER: u64 = 0x60;
        let w = self.w();
        let size = |o: u64, most: u64| pointers.to(o + 1, o + most - 1).first().map_or(most, |(next, _)| next - o);
        let mut out = Vec::new();
        for f in (2 * w..size(obj, FIELDS)).step_by(self.width) {
            let Some(p) = self.ptr(obj + f) else { continue };
            if let Some(n) = self.name_of(p) {
                out.push((vec![f], n));
            } else if one_hop {
                for f2 in (2 * w..size(p, INNER)).step_by(self.width) {
                    if let Some(n) = self.ptr(p + f2).and_then(|q| self.name_of(q)) {
                        out.push((vec![f, f2], n));
                    }
                }
            }
        }
        out
    }
}

impl Pointers {
    /// Pointers to `lo..=hi`, as (pointer, where it is kept), by pointer.
    fn to(&self, lo: u64, hi: u64) -> Vec<(u64, u64)> {
        let n = self.len();
        let first = partition(n, |i| self.get(i).0 < lo);
        (first..n).map(|i| self.get(i)).take_while(|(p, _)| *p <= hi).collect()
    }
}

fn partition(n: usize, below: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if below(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// A way down from `obj` to the value, found going up from it.
struct Partial {
    obj: u64,
    steps: Vec<Step>,
    value: u64,
}

/// Most bytes between an object's start and a field Ferret follows.
const MAX_FIELD: u64 = 0x400;
/// Objects above the value's own.
const MAX_UP: usize = 6;
const MAX_PARTIALS: usize = 256;
const REFERRERS_PER_OBJECT: usize = 16;

/// Named paths that lead to `target` now, best first (fewest places they lead to, then fewest
/// steps), with where each leads.
pub fn discover(heap: &Heap, pointers: &Pointers, target: u64) -> Vec<(NamedPath, Vec<u64>)> {
    let w = heap.w();
    // The value's object: the nearest address before it that something points to.
    let mut bases: Vec<u64> = pointers.to(target.saturating_sub(MAX_FIELD), target.saturating_sub(4)).iter().map(|(p, _)| *p).collect();
    bases.dedup();
    let mut level: Vec<Partial> = bases.iter().rev().take(2).map(|&b| Partial { obj: b, steps: Vec::new(), value: target - b }).collect();
    let mut seen: Vec<u64> = level.iter().map(|p| p.obj).collect();
    let mut found: Vec<NamedPath> = Vec::new();
    for depth in 0..=MAX_UP {
        for p in &level {
            for (chain, root) in heap.names(pointers, p.obj, false) {
                found.push(NamedPath { root, root_field: chain[0], steps: p.steps.clone(), value: p.value });
            }
        }
        if depth == MAX_UP {
            break;
        }
        let mut next = Vec::new();
        for p in &level {
            for (_, at) in pointers.to(p.obj, p.obj).into_iter().take(REFERRERS_PER_OBJECT) {
                if let Some(array) = heap.array_holding(at) {
                    // Which element it is changes as the game adds and removes things: every
                    // element with the same name.
                    for (chain, name) in heap.names(pointers, p.obj, true).into_iter().take(8) {
                        let mut steps = vec![Step::Each, Step::Named(chain, name)];
                        steps.extend(p.steps.iter().cloned());
                        next.push(Partial { obj: array, steps, value: p.value });
                    }
                } else if let Some(&(obj, _)) = pointers.to(at.saturating_sub(MAX_FIELD), at.saturating_sub(w)).last() {
                    let mut steps = vec![Step::Field(at - obj)];
                    steps.extend(p.steps.iter().cloned());
                    next.push(Partial { obj, steps, value: p.value });
                }
            }
        }
        next.retain(|p| !seen.contains(&p.obj) || p.steps.first() == Some(&Step::Each));
        next.truncate(MAX_PARTIALS);
        seen.extend(next.iter().map(|p| p.obj));
        level = next;
    }
    // Which of them lead to the value: find their roots by name, as after a restart.
    found.truncate(2000);
    let mut texts: Vec<String> = found.iter().map(|p| p.root.clone()).collect();
    texts.sort();
    texts.dedup();
    texts.truncate(64);
    let units: Vec<Vec<u16>> = texts.iter().map(|t| t.encode_utf16().collect()).collect();
    let strings = heap.find_strings(&units);
    let mut good: Vec<(NamedPath, Vec<u64>)> = Vec::new();
    for path in found {
        let Some(i) = texts.iter().position(|t| *t == path.root) else { continue };
        let roots: Vec<u64> =
            strings[i].iter().flat_map(|&s| pointers.to(s, s)).filter_map(|(_, at)| at.checked_sub(path.root_field)).take(4096).collect();
        let leads = heap.walk(&roots, &path);
        if leads.contains(&target) && !good.iter().any(|(p, _)| *p == path) {
            good.push((path, leads));
        }
    }
    good.sort_by_key(|(p, leads)| (leads.len(), p.steps.len()));
    good.truncate(5);
    good
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip() {
        let p = NamedPath {
            root: "Inventory".into(),
            root_field: 0x10,
            steps: vec![Step::Field(0x28), Step::Field(0x10), Step::Each, Step::Named(vec![0x10, 0x10], "$item wood, 50%".into())],
            value: 0x38,
        };
        let t = p.text();
        assert_eq!(t, "\"Inventory\"@10,28,10,*,10.10=\"$item%20wood%2C%2050%25\",+38");
        assert!(!t.contains(' '));
        assert_eq!(NamedPath::parse(&t), Some(p));
        assert_eq!(NamedPath::parse("Forager.exe+177a64c,44,2c"), None);
    }
}
