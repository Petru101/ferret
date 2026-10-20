//! Lua games on LuaJIT (LÖVE, Defold and others): their tables as LuaJIT keeps them, read from
//! outside. A value is named by its path from the globals table `_G`: `lua:player.hp`,
//! `lua:Scene.static.currentState.heat`, `lua:enemies[3].hp`, and through a function's
//! upvalue for state a script keeps in its own locals: `lua:love.update^score` (`score`, a
//! local of main.lua that love.update uses).
//!
//! LuaJIT basics used here: a value (TValue) is 8 bytes, a double unless its top 13 bits are
//! all set (a NaN), when the next bits say what it is (a table, a string...) and the rest
//! points to it. Two layouts: GC64 (LuaJIT 2.1's default on x64 since 2023: a 17-bit tag above
//! a 47-bit pointer, 8-byte references) and 32-bit references (2.0, older 2.1 builds and 32-bit
//! games: the tag in the upper half, the pointer in the lower). Its collector never moves an
//! object: a table found stays where it is; only its array and hash parts move when it grows.

use crate::mono::player_like;
use crate::names::Heap;
use crate::ue::{Listed, ListedObject, MAX_LISTED};
use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Object types (a GC object's `gct`; a TValue's tag is its complement).
const STR: u8 = 4;
const UPVAL: u8 = 5;
const PROTO: u8 = 7;
const FUNC: u8 = 8;
const TAB: u8 = 11;
/// Characters read from one string at most.
const MAX_TEXT: u32 = 256;
/// Slots read from one table's array or hash part at most.
const MAX_SLOTS: u32 = 1 << 16;
/// Tables and functions looked into for one list at most.
const MAX_VISITED: usize = 20000;
/// How deep the list goes from `_G`.
const MAX_DEPTH: usize = 8;
/// Numbers listed from one table at most (bigger arrays are a map's tiles or a sprite's
/// frames, not the player's numbers).
const MAX_PER_TABLE: usize = 256;
/// Lua's and LÖVE's own tables: never what the game keeps its state in.
const LIBRARIES: &[&str] = &["string", "table", "math", "io", "os", "debug", "coroutine", "package", "jit", "bit", "bit32", "ffi", "utf8", "love", "arg", "_G"];
/// Defold's own modules (skipped when `_G` has `go`, `msg` and `vmath`: a LÖVE game's own
/// `timer` or `window` table is the game's).
const DEFOLD: &[&str] = &[
    "go", "gui", "msg", "sys", "render", "vmath", "physics", "sound", "resource", "timer", "http", "json", "zlib", "crash", "image", "buffer",
    "window", "label", "model", "particlefx", "sprite", "collectionfactory", "factory", "collectionproxy", "tilemap", "profiler", "html5",
    "socket", "b2d", "liveupdate", "graphics", "camera", "font", "spine", "iac", "iap", "push", "webview",
];
/// Table names (besides player-like ones) shown in the main list.
const GAME_WORDS: &[&str] = &["game", "gamestate", "game_state", "currentstate", "current_state", "currentscene", "current_scene", "stats", "inventory", "save", "savedata", "save_data", "progress"];

/// How one game's LuaJIT lays its objects out.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Layout {
    gc64: bool,
    /// Where a string keeps its length (LuaJIT 2.1 since 2020 has a string id before its hash)
    /// and its characters.
    len_at: u64,
    data_at: u64,
}

const LAYOUTS: [Layout; 4] = [
    Layout { gc64: true, len_at: 20, data_at: 24 },
    Layout { gc64: true, len_at: 16, data_at: 24 },
    Layout { gc64: false, len_at: 16, data_at: 20 },
    Layout { gc64: false, len_at: 12, data_at: 16 },
];

/// A located game: its layout and its globals table.
pub struct Lj {
    lay: Layout,
    g: u64,
}

/// A table key as the list and paths name it.
#[derive(Clone, Debug, PartialEq)]
enum Key {
    Name(String),
    Index(i64),
}

fn word(mem: &File, at: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b, at).ok().map(|_| u64::from_le_bytes(b))
}

fn half(mem: &File, at: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    mem.read_exact_at(&mut b, at).ok().map(|_| u32::from_le_bytes(b))
}

fn byte(mem: &File, at: u64) -> Option<u8> {
    let mut b = [0u8; 1];
    mem.read_exact_at(&mut b, at).ok().map(|_| b[0])
}

/// Whether a TValue holds a number (either layout): anything but the NaNs LuaJIT tags its
/// other values with; the one NaN arithmetic makes counts.
pub fn is_number(w: u64) -> bool {
    w >> 51 != 0x1fff || w == 0xfff8_0000_0000_0000
}

/// A TValue's number, if it holds one.
pub fn number(b: &[u8]) -> Option<f64> {
    let w = u64::from_le_bytes(b.get(..8)?.try_into().ok()?);
    is_number(w).then(|| f64::from_bits(w))
}

impl Layout {
    /// The object type a TValue holds (the complement of its tag), when not a number.
    fn kind(self, w: u64) -> Option<u8> {
        if is_number(w) {
            return None;
        }
        let tag = match self.gc64 {
            true => ((w as i64) >> 47) as u32,
            false => (w >> 32) as u32,
        };
        Some(!tag as u8).filter(|_| !tag < 0x20)
    }

    /// The object a TValue of this type points to.
    fn object(self, w: u64, kind: u8) -> Option<u64> {
        if self.kind(w) != Some(kind) {
            return None;
        }
        let p = match self.gc64 {
            true => w & ((1 << 47) - 1),
            false => w & 0xffff_ffff,
        };
        (p > 0x10000 && p % 4 == 0).then_some(p)
    }

    /// A TValue pointing to an object of this type.
    fn value(self, p: u64, kind: u8) -> u64 {
        let tag = !(kind as u32);
        match self.gc64 {
            true => (u64::from(tag & 0x1ffff) << 47) | p,
            false => (u64::from(tag) << 32) | p,
        }
    }

    /// A reference (GCRef or MRef) at `at`.
    fn reference(self, mem: &File, at: u64) -> Option<u64> {
        match self.gc64 {
            true => word(mem, at),
            false => half(mem, at).map(u64::from),
        }
        .filter(|&p| p > 0x10000)
    }

    /// An object's type byte (`gct`).
    fn gct(self, mem: &File, o: u64) -> Option<u8> {
        byte(mem, o + if self.gc64 { 9 } else { 5 })
    }
}

/// A table's array part (address, slots) and hash part (address, nodes of 24 bytes:
/// {value, key, next}).
struct Parts {
    array: u64,
    asize: u32,
    node: u64,
    nodes: u32,
}

impl Lj {
    /// Finds `_G`: the strings "_G", the hash nodes keyed by one of them, and the node whose
    /// value is the very table it is in (`_G._G == _G`), in two passes over memory. The biggest
    /// such table when the game runs several Lua states (LÖVE's threads).
    fn locate(heap: &Heap) -> Option<Lj> {
        let mut strings: Vec<(Layout, u64)> = Vec::new();
        heap.each_chunk(32, |addr, b, fresh| {
            for off in (0..fresh).step_by(4) {
                if b.get(off..off + 3) != Some(b"_G\0") {
                    continue;
                }
                for lay in LAYOUTS {
                    let Some(o) = off.checked_sub(lay.data_at as usize) else { continue };
                    let gct = if lay.gc64 { 9 } else { 5 };
                    let len = b.get(o + lay.len_at as usize..o + lay.len_at as usize + 4).map(|l| u32::from_le_bytes(l.try_into().unwrap()));
                    if (addr + o as u64) % 8 == 0 && b.get(o + gct) == Some(&STR) && len == Some(2) {
                        strings.push((lay, addr + o as u64));
                    }
                }
            }
        });
        if strings.is_empty() {
            return None;
        }
        let mem = heap.file();
        // A key is a whole TValue; on a 32-bit game the pass looks at 4-byte words, so its
        // pointer half.
        let wanted: Vec<u64> = strings.iter().map(|&(lay, s)| if heap.width() == 8 { lay.value(s, STR) } else { s }).collect();
        let mut best: Option<(Lj, u32)> = None;
        for r in heap.referrers(&wanted) {
            if r % 8 != 0 {
                continue;
            }
            let Some(key) = word(mem, r) else { continue };
            for &(lay, s) in &strings {
                if key != lay.value(s, STR) {
                    continue;
                }
                let lj = Lj { lay, g: 0 };
                let node = r - 8;
                let Some(t) = word(mem, node).and_then(|v| lay.object(v, TAB)) else { continue };
                let Some(p) = lj.parts(mem, t) else { continue };
                if (p.node..p.node + 24 * u64::from(p.nodes)).contains(&node) && best.as_ref().is_none_or(|(_, n)| p.nodes > *n) {
                    best = Some((Lj { lay, g: t }, p.nodes));
                }
            }
        }
        best.map(|(lj, _)| lj)
    }

    fn parts(&self, mem: &File, t: u64) -> Option<Parts> {
        if self.lay.gct(mem, t)? != TAB {
            return None;
        }
        let (array, asize, node, hmask) = match self.lay.gc64 {
            true => (word(mem, t + 16)?, half(mem, t + 48)?, word(mem, t + 40)?, half(mem, t + 52)?),
            false => (u64::from(half(mem, t + 8)?), half(mem, t + 24)?, u64::from(half(mem, t + 20)?), half(mem, t + 28)?),
        };
        let nodes = hmask.checked_add(1)?;
        (asize <= MAX_SLOTS && nodes <= MAX_SLOTS && nodes.is_power_of_two()).then_some(Parts { array, asize, node, nodes })
    }

    /// A string's text.
    fn text(&self, mem: &File, s: u64) -> Option<String> {
        if self.lay.gct(mem, s)? != STR {
            return None;
        }
        let len = half(mem, s + self.lay.len_at)?;
        if len > MAX_TEXT {
            return None;
        }
        let mut b = vec![0u8; len as usize];
        mem.read_exact_at(&mut b, s + self.lay.data_at).ok()?;
        Some(String::from_utf8_lossy(&b).into_owned())
    }

    /// A key TValue as a name or a whole number.
    fn key(&self, mem: &File, k: u64) -> Option<Key> {
        if is_number(k) {
            let n = f64::from_bits(k);
            return (n.fract() == 0.0 && n.abs() < 1e15).then_some(Key::Index(n as i64));
        }
        self.text(mem, self.lay.object(k, STR)?).map(Key::Name)
    }

    /// A table's entries: each key with its value (a TValue) and where that is; nil ones left
    /// out.
    fn entries(&self, mem: &File, t: u64) -> Vec<(Key, u64, u64)> {
        let Some(p) = self.parts(mem, t) else { return Vec::new() };
        let mut out = Vec::new();
        let nil = |w: u64| w == u64::MAX || !self.lay.gc64 && w >> 32 == 0xffff_ffff;
        if p.asize > 0 {
            let mut b = vec![0u8; 8 * p.asize as usize];
            if mem.read_exact_at(&mut b, p.array).is_ok() {
                for (i, c) in b.chunks(8).enumerate() {
                    let w = u64::from_le_bytes(c.try_into().unwrap());
                    if !nil(w) {
                        out.push((Key::Index(i as i64), w, p.array + 8 * i as u64));
                    }
                }
            }
        }
        let mut b = vec![0u8; 24 * p.nodes as usize];
        if mem.read_exact_at(&mut b, p.node).is_ok() {
            for (i, c) in b.chunks(24).enumerate() {
                let v = u64::from_le_bytes(c[..8].try_into().unwrap());
                let k = u64::from_le_bytes(c[8..16].try_into().unwrap());
                if nil(v) || nil(k) {
                    continue;
                }
                if let Some(key) = self.key(mem, k) {
                    out.push((key, v, p.node + 24 * i as u64));
                }
            }
        }
        out
    }

    /// One entry's value and where it is.
    fn get(&self, mem: &File, t: u64, key: &Key) -> Option<(u64, u64)> {
        let p = self.parts(mem, t)?;
        if let Key::Index(i) = *key {
            if (0..i64::from(p.asize)).contains(&i) {
                let at = p.array + 8 * i as u64;
                return word(mem, at).map(|w| (w, at));
            }
        }
        let mut b = vec![0u8; 24 * p.nodes as usize];
        mem.read_exact_at(&mut b, p.node).ok()?;
        for (i, c) in b.chunks(24).enumerate() {
            let k = u64::from_le_bytes(c[8..16].try_into().unwrap());
            let hit = match key {
                Key::Index(n) => is_number(k) && f64::from_bits(k) == *n as f64,
                Key::Name(name) => self.lay.object(k, STR).is_some_and(|s| {
                    half(mem, s + self.lay.len_at) == Some(name.len() as u32) && self.text(mem, s).as_deref() == Some(name.as_str())
                }),
            };
            if hit {
                return Some((u64::from_le_bytes(c[..8].try_into().unwrap()), p.node + 24 * i as u64));
            }
        }
        None
    }

    /// A Lua function's closed upvalues: each name (from its prototype's debug info; "#i" when
    /// stripped) with its value and where that is. Open ones (on a running function's stack,
    /// which moves when it grows) are left out. Functions of LÖVE's own boot code too.
    fn upvalues(&self, mem: &File, f: u64) -> Vec<(String, u64, u64)> {
        let lay = self.lay;
        if lay.gct(mem, f) != Some(FUNC) {
            return Vec::new();
        }
        // GCfuncL: {header, ffid, nupvalues, env, gclist, pc, uvptr[]}; ffid 0 = a Lua function.
        let (ffid, n, pc, uv, step, proto_size) = match lay.gc64 {
            true => (byte(mem, f + 10), byte(mem, f + 11), word(mem, f + 32), f + 40, 8, 104),
            false => (byte(mem, f + 6), byte(mem, f + 7), half(mem, f + 16).map(u64::from), f + 20, 4, 64),
        };
        let (Some(0), Some(n), Some(pc)) = (ffid, n, pc) else { return Vec::new() };
        // The prototype sits right before its bytecode.
        let proto = pc.wrapping_sub(proto_size);
        if n == 0 || lay.gct(mem, proto) != Some(PROTO) {
            return Vec::new();
        }
        let (chunk_at, uvinfo_at) = if lay.gc64 { (64, 88) } else { (40, 56) };
        let chunk = lay.reference(mem, proto + chunk_at).and_then(|c| self.text(mem, c)).unwrap_or_default();
        if chunk.contains("[love ") {
            return Vec::new();
        }
        let mut names: Vec<String> = Vec::new();
        if let Some(info) = lay.reference(mem, proto + uvinfo_at) {
            let mut b = vec![0u8; 1024];
            let got = mem.read_at(&mut b, info).unwrap_or(0);
            names = b[..got].split(|&c| c == 0).take(n as usize).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
        }
        let mut out = Vec::new();
        for i in 0..u64::from(n) {
            let Some(u) = lay.reference(mem, uv + step * i) else { continue };
            // GCupval: {header, closed, immutable, the value or two links (open), v}; closed, v
            // points to its own value.
            let (closed, v, own) = match lay.gc64 {
                true => (byte(mem, u + 10), word(mem, u + 32), u + 16),
                false => (byte(mem, u + 6), half(mem, u + 16).map(u64::from), u + 8),
            };
            if lay.gct(mem, u) != Some(UPVAL) || closed != Some(1) || v != Some(own) {
                continue;
            }
            let (Some(v), name) = (v, names.get(i as usize).filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| format!("#{i}"))) else { continue };
            if let Some(w) = word(mem, v) {
                out.push((name, w, v));
            }
        }
        out
    }

    /// Whether `_G` is still the game's globals table.
    fn valid(&self, mem: &File) -> bool {
        self.get(mem, self.g, &Key::Name("_G".into())).is_some_and(|(w, _)| self.lay.object(w, TAB) == Some(self.g))
    }
}

/// The located globals of the game attached to, per pid; a failed search is tried again after
/// `RETRY`.
pub fn shared(heap: &Heap) -> Option<Arc<Lj>> {
    const RETRY: Duration = Duration::from_secs(10);
    static FOUND: Mutex<Option<(u32, Option<Arc<Lj>>, Instant)>> = Mutex::new(None);
    let mut found = FOUND.lock().unwrap();
    if let Some((pid, lj, at)) = found.as_ref() {
        let valid = lj.as_ref().is_some_and(|lj| lj.valid(heap.file()));
        if *pid == heap.pid && (valid || lj.is_none() && at.elapsed() < RETRY) {
            return lj.clone();
        }
    }
    let lj = Lj::locate(heap).map(Arc::new);
    *found = Some((heap.pid, lj.clone(), Instant::now()));
    lj
}

/// Whether the game runs LuaJIT or Lua: a Lua library mapped (LÖVE's `libluajit-5.1.so`,
/// `lua51.dll` on Windows), or Defold's engine (LuaJIT built in; its games ship `game.arcd`
/// next to the program).
pub fn runs_lua(pid: u32, regions: &[crate::helper::Region], program: Option<&std::path::Path>) -> bool {
    let lib = regions.iter().any(|r| {
        let name = r.path.rsplit(['/', '\\']).next().unwrap_or_default().to_ascii_lowercase();
        name.starts_with("libluajit") || name.starts_with("liblua5") || name.starts_with("lua51.") || name.starts_with("lua5.1.") || name == "liblove.so" || name.starts_with("liblove")
    });
    let defold = program.and_then(|p| p.parent()).is_some_and(|dir| {
        let root = std::path::Path::new("/proc").join(pid.to_string()).join("root");
        let inside = root.join(dir.strip_prefix("/").unwrap_or(dir));
        inside.join("game.arcd").exists() || dir.join("game.arcd").exists()
    });
    lib || defold
}

/// One step of a path: a table entry, or an upvalue of the function before it.
#[derive(Clone, Debug, PartialEq)]
enum Step {
    Key(Key),
    Up(String),
}

/// A saved path from `_G` to a number.
#[derive(Clone, Debug, PartialEq)]
pub struct LuaPath {
    steps: Vec<Step>,
}

fn encode(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' => c.to_string(),
            _ => c.to_string().bytes().map(|b| format!("%{b:02X}")).collect(),
        })
        .collect()
}

fn decode(text: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut it = text.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hex = [it.next()?, it.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}

impl LuaPath {
    /// "lua:" then steps: a name (".name" after the first), "[3]" or "^name" (an upvalue).
    pub fn parse(text: &str) -> Option<LuaPath> {
        let mut rest = text.strip_prefix("lua:")?;
        let mut steps = Vec::new();
        let mut first = true;
        while !rest.is_empty() {
            let (sep, body) = match rest.as_bytes()[0] {
                b'.' | b'^' | b'[' => (rest.as_bytes()[0], &rest[1..]),
                _ if first => (b'.', rest),
                _ => return None,
            };
            first = false;
            if sep == b'[' {
                let (n, after) = body.split_once(']')?;
                steps.push(Step::Key(Key::Index(n.parse().ok()?)));
                rest = after;
                continue;
            }
            let end = body.find(['.', '^', '[']).unwrap_or(body.len());
            let name = decode(&body[..end])?;
            if name.is_empty() {
                return None;
            }
            steps.push(if sep == b'^' { Step::Up(name) } else { Step::Key(Key::Name(name)) });
            rest = &body[end..];
        }
        (!steps.is_empty()).then_some(LuaPath { steps })
    }

    pub fn text(&self) -> String {
        format!("lua:{}", self.short())
    }

    /// The path as the list shows it: names as they are, a module's table by its module's
    /// name (`package.loaded["src.save"].data`: `src.save.data`).
    fn shown(&self) -> String {
        let mut steps = &self.steps[..];
        let mut t = String::new();
        if let [Step::Key(Key::Name(p)), Step::Key(Key::Name(l)), Step::Key(Key::Name(m)), rest @ ..] = steps {
            if p == "package" && l == "loaded" {
                t = m.clone();
                steps = rest;
            }
        }
        for s in steps {
            match s {
                Step::Key(Key::Name(n)) if t.is_empty() => t += n,
                Step::Key(Key::Name(n)) => t += &format!(".{n}"),
                Step::Key(Key::Index(i)) => t += &format!("[{i}]"),
                Step::Up(n) => t += &format!("^{n}"),
            }
        }
        t
    }

    fn short(&self) -> String {
        let mut t = String::new();
        for s in &self.steps {
            match s {
                Step::Key(Key::Name(n)) if t.is_empty() => t += &encode(n),
                Step::Key(Key::Name(n)) => t += &format!(".{}", encode(n)),
                Step::Key(Key::Index(i)) => t += &format!("[{i}]"),
                Step::Up(n) => t += &format!("^{}", encode(n)),
            }
        }
        t
    }

    pub fn describe(&self) -> String {
        format!("{} in the game's Lua tables", self.shown())
    }

    fn then(&self, s: Step) -> LuaPath {
        let mut p = self.clone();
        p.steps.push(s);
        p
    }

    /// The last name along the path (a table's own name).
    fn last_name(&self) -> Option<&str> {
        self.steps.iter().rev().find_map(|s| match s {
            Step::Key(Key::Name(n)) | Step::Up(n) => Some(n.as_str()),
            Step::Key(Key::Index(_)) => None,
        })
    }
}

/// The globals table.
pub fn find_roots(heap: &Heap, _path: &LuaPath) -> Vec<u64> {
    shared(heap).map(|lj| vec![lj.g]).unwrap_or_default()
}

/// Where the path leads from `_G`: the number's TValue, while it holds a number.
pub fn walk(heap: &Heap, roots: &[u64], path: &LuaPath) -> Vec<u64> {
    let Some(lj) = shared(heap) else { return Vec::new() };
    let mem = heap.file();
    roots.iter().filter(|&&r| r == lj.g).filter_map(|_| follow(&lj, mem, &path.steps)).collect()
}

fn follow(lj: &Lj, mem: &File, steps: &[Step]) -> Option<u64> {
    let mut cur = lj.lay.value(lj.g, TAB);
    let mut at = 0;
    for s in steps {
        (cur, at) = match s {
            Step::Key(k) => lj.get(mem, lj.lay.object(cur, TAB)?, k)?,
            Step::Up(name) => {
                let f = lj.lay.object(cur, FUNC)?;
                lj.upvalues(mem, f).into_iter().find(|(n, _, _)| n == name).map(|(_, w, a)| (w, a))?
            }
        };
    }
    is_number(cur).then_some(at)
}

/// A table (or a function's upvalues) whose numbers are listed.
struct Group {
    path: LuaPath,
    locals: bool,
    numbers: Vec<(String, u64, LuaPath)>,
}

/// Every number reachable from `_G` (Lua's and LÖVE's own tables left out), by table: each
/// table's numbers by key, a function's numbers in upvalues. Tables are named by their
/// shortest path (breadth first): a module's table by its global's name when it has one, else
/// through `package.loaded`.
fn groups(heap: &Heap) -> Option<Vec<Group>> {
    let lj = shared(heap)?;
    let mem = heap.file();
    let lay = lj.lay;
    let mut seen: HashSet<u64> = HashSet::new();
    let mut numbers_seen: HashSet<u64> = HashSet::new();
    seen.insert(lj.g);
    // The libraries' own tables (their modules in package.loaded too), and the `love` table's
    // functions (the game's callbacks) whose upvalues are looked at.
    let globals = lj.entries(mem, lj.g);
    let defold = ["go", "msg", "vmath"].iter().all(|n| globals.iter().any(|(k, w, _)| *k == Key::Name((*n).into()) && lay.kind(*w) == Some(TAB)));
    let library = |n: &str| LIBRARIES.contains(&n) || defold && DEFOLD.contains(&n);
    let mut queue: VecDeque<(u64, u8, LuaPath, usize)> = VecDeque::new();
    let root = LuaPath { steps: Vec::new() };
    let mut loaded = None;
    for (k, w, _) in &globals {
        let Key::Name(name) = k else { continue };
        if !library(name) {
            continue;
        }
        let Some(t) = lay.object(*w, TAB) else { continue };
        seen.insert(t);
        let path = root.then(Step::Key(k.clone()));
        if name == "package" {
            loaded = lj.get(mem, t, &Key::Name("loaded".into())).and_then(|(w, _)| lay.object(w, TAB)).map(|l| (l, path.then(Step::Key(Key::Name("loaded".into())))));
        }
        if name == "love" {
            for (k, w, _) in lj.entries(mem, t) {
                if let Some(f) = lay.object(w, FUNC) {
                    queue.push_back((f, FUNC, path.then(Step::Key(k)), 2));
                } else if let Some(t) = lay.object(w, TAB) {
                    seen.insert(t);
                }
            }
        }
    }
    if let Some((l, _)) = loaded {
        seen.insert(l);
        for (k, w, _) in lj.entries(mem, l) {
            if let (Key::Name(n), Some(t)) = (&k, lay.object(w, TAB)) {
                if library(n) || n.starts_with("love.") || n.starts_with("jit.") {
                    seen.insert(t);
                }
            }
        }
    }
    queue.push_front((lj.g, TAB, root.clone(), 0));
    if let Some((l, path)) = loaded {
        queue.push_back((l, TAB, path, 2));
    }
    let mut out: Vec<Group> = Vec::new();
    let mut visited = 0;
    let mut listed = 0;
    while let Some((o, kind, path, depth)) = queue.pop_front() {
        visited += 1;
        if visited > MAX_VISITED || listed > MAX_LISTED {
            break;
        }
        let entries: Vec<(Step, u64, u64)> = match kind {
            TAB => lj.entries(mem, o).into_iter().map(|(k, w, a)| (Step::Key(k), w, a)).collect(),
            _ => lj.upvalues(mem, o).into_iter().map(|(n, w, a)| (Step::Up(n), w, a)).collect(),
        };
        let mut g = Group { path: path.clone(), locals: kind == FUNC, numbers: Vec::new() };
        for (step, w, at) in entries {
            if is_number(w) {
                if g.numbers.len() < MAX_PER_TABLE && numbers_seen.insert(at) {
                    let label = match &step {
                        Step::Key(Key::Name(n)) | Step::Up(n) => n.clone(),
                        Step::Key(Key::Index(i)) => format!("[{i}]"),
                    };
                    g.numbers.push((label, at, path.then(step)));
                }
                continue;
            }
            if depth >= MAX_DEPTH {
                continue;
            }
            for k in [TAB, FUNC] {
                if let Some(p) = lay.object(w, k) {
                    if seen.insert(p) {
                        queue.push_back((p, k, path.then(step.clone()), depth + 1));
                    }
                }
            }
        }
        if !g.numbers.is_empty() {
            listed += g.numbers.len();
            out.push(g);
        }
    }
    Some(out)
}

/// Whether a table is named like the player's (`player`, `hero`): its own name, not its
/// parent's (a `Player` class's constants are its `corners`).
fn players(g: &Group) -> bool {
    g.path.last_name().is_some_and(player_like)
}

/// Whether a group is the player's or the game's state: a player-like name, or a table named
/// like the game's state (`G.GAME`, `stats`, `Scene.static.currentState`).
fn main_group(g: &Group) -> bool {
    players(g) || g.path.last_name().is_some_and(|n| GAME_WORDS.contains(&n.to_ascii_lowercase().as_str()))
}

/// Constants by convention (`MAX_SPEED`): left out of the globals' main group.
fn constant(name: &str) -> bool {
    name.len() > 1 && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && name.chars().any(|c| c.is_ascii_uppercase())
}

/// The Browse tab's list for a Lua game: the global numbers ("Global variables"), tables named
/// like the player's or the game's state, then (`more`) every other table's numbers, named
/// by their path.
pub fn player(heap: &Heap) -> Option<Vec<ListedObject>> {
    let groups = groups(heap)?;
    let mut out = Vec::new();
    let listed = |numbers: Vec<(String, u64, LuaPath)>| numbers.into_iter().map(|(label, addr, _)| Listed { label, addr, kind: "lua" }).collect::<Vec<_>>();
    for g in groups {
        if g.path.steps.is_empty() {
            let (consts, vars): (Vec<_>, Vec<_>) = g.numbers.into_iter().partition(|(n, _, _)| constant(n));
            out.push(ListedObject { name: "@global".into(), class: "Lua table".into(), role: "game", owner: None, more: false, fields: listed(vars) });
            out.push(ListedObject { name: "_G".into(), class: "Lua table".into(), role: "game", owner: None, more: true, fields: listed(consts) });
            continue;
        }
        let main = main_group(&g);
        let player = players(&g);
        out.push(ListedObject {
            name: g.path.shown(),
            class: if g.locals { "Lua locals" } else { "Lua table" }.into(),
            role: if player { "character" } else { "game" },
            owner: None,
            more: !main,
            fields: listed(g.numbers),
        });
    }
    out.retain(|o| !o.fields.is_empty());
    out.sort_by_key(|o| (o.more, o.role != "character"));
    Some(out)
}

/// The path to a number the list has, checked by following it the way a restart does.
pub fn discover(heap: &Heap, target: u64) -> Option<(LuaPath, Vec<u64>)> {
    let groups = groups(heap)?;
    let path = groups.into_iter().flat_map(|g| g.numbers).find(|(_, a, _)| *a == target)?.2;
    let leads = walk(heap, &find_roots(heap, &path), &path);
    leads.contains(&target).then_some((path, leads))
}

/// luadump: what was found (the layout, `_G`) and a table at a path, a line an entry.
pub fn diagnose(heap: &Heap, at: &str) -> Vec<String> {
    let t = Instant::now();
    let Some(lj) = shared(heap) else { return vec!["no LuaJIT globals table found".into()] };
    let mem = heap.file();
    let mut out = vec![format!("layout {:?}, _G at {:#x} ({} ms)", lj.lay, lj.g, t.elapsed().as_millis())];
    let mut cur = lj.lay.value(lj.g, TAB);
    if !at.is_empty() {
        let Some(p) = LuaPath::parse(&format!("lua:{at}")) else { return vec!["bad path".into()] };
        for s in &p.steps {
            let next = match s {
                Step::Key(k) => lj.lay.object(cur, TAB).and_then(|t| lj.get(mem, t, k)).map(|(w, _)| w),
                Step::Up(n) => lj.lay.object(cur, FUNC).and_then(|f| lj.upvalues(mem, f).into_iter().find(|(x, _, _)| x == n)).map(|(_, w, _)| w),
            };
            let Some(next) = next else { return vec![format!("{at}: not found")] };
            cur = next;
        }
    }
    let shown = |w: u64| match lj.lay.kind(w) {
        None => format!("{}", f64::from_bits(w)),
        Some(STR) => format!("{:?}", lj.lay.object(w, STR).and_then(|s| lj.text(mem, s)).unwrap_or_default()),
        Some(k) => format!("{} {:#x}", match k { TAB => "table", FUNC => "function", 0 => "nil", 1 => "false", 2 => "true", 12 => "userdata", 10 => "cdata", _ => "other" }, lj.lay.object(w, k).unwrap_or(0)),
    };
    if let Some(t) = lj.lay.object(cur, TAB) {
        for (k, w, _) in lj.entries(mem, t) {
            out.push(format!("{k:?} = {}", shown(w)));
        }
    }
    if let Some(f) = lj.lay.object(cur, FUNC) {
        for (n, w, _) in lj.upvalues(mem, f) {
            out.push(format!("^{n} = {}", shown(w)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_read_back() {
        for text in ["lua:player.hp", "lua:score", "lua:enemies[3].hp", "lua:love.update^score", "lua:love.update^game.state.coins", "lua:package.loaded.src%2Esave.coins", "lua:grid[1][2]"] {
            assert_eq!(LuaPath::parse(text).unwrap().text(), text, "{text}");
        }
        let p = LuaPath::parse("lua:love.update^game[2].hp").unwrap();
        assert_eq!(
            p.steps,
            vec![Step::Key(Key::Name("love".into())), Step::Key(Key::Name("update".into())), Step::Up("game".into()), Step::Key(Key::Index(2)), Step::Key(Key::Name("hp".into()))]
        );
        assert!(LuaPath::parse("lua:").is_none());
        assert!(LuaPath::parse("lua:a..b").is_none());
        assert!(LuaPath::parse("lua:a[x]").is_none());
        assert!(LuaPath::parse("js:a").is_none());
        assert_eq!(LuaPath::parse("lua:package.loaded.src%2Esave.data[2]").unwrap().shown(), "src.save.data[2]");
        assert_eq!(LuaPath::parse("lua:love.update^game.coins").unwrap().shown(), "love.update^game.coins");
    }

    #[test]
    fn values_by_layout() {
        for lay in LAYOUTS {
            let t = lay.value(0x4024_a960, TAB);
            assert_eq!(lay.kind(t), Some(TAB));
            assert_eq!(lay.object(t, TAB), Some(0x4024_a960));
            assert_eq!(lay.object(t, STR), None);
            assert!(!is_number(t));
        }
        assert_eq!(LAYOUTS[0].value(0x7f39_60a5_a800, STR), 0xfffd_ff39_60a5_a800);
        assert!(is_number(1.5f64.to_bits()) && is_number((-3.0f64).to_bits()) && is_number(f64::NAN.to_bits()));
        assert!(!is_number(u64::MAX));
        assert!(constant("MAX_SPEED") && constant("HP2") && !constant("score") && !constant("_") && !constant("Player"));
    }
}
