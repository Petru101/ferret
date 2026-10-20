//! GameMaker games (GameMaker Studio 2 before 2.3, bytecode games: their code and names are in
//! data.win or game.unx). Every instance, and the global object, keeps its variables in a hash
//! map at +0x58: {size (a power of two), used, mask = size - 1, grow threshold = 0.6 size,
//! elements}, elements of 16 bytes {RValue*, key, hash (0 = empty)}, the key being the
//! variable's id from the data file's VARI chunk + 100000. An RValue is 16 bytes {value, flags,
//! kind}: kind 0 a double, 7 an int32, 10 an int64 (Ferret's `gm` type reads whichever). An instance's id (100000 up) and object index (into the
//! OBJT chunk) sit side by side further in (+0xbc and +0xc4 on the runners seen; voted on). So a value is
//! named by its object and variable: `gml:global.totalmoney`, `gml:player1.hp` (every
//! instance of player1).

use crate::names::Heap;
use crate::ue::{Listed, ListedObject};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Map keys are a variable's id plus this.
const KEY_BASE: u32 = 100000;
/// Where an instance's id and object index are looked for (from the map pointer's object).
const ID_FROM: u64 = 0x60;
const ID_TO: u64 = 0x200;
/// Largest variable map read.
const MAX_MAP: u32 = 1 << 16;
/// RValue kinds.
const REAL: u32 = 0;
const INT32: u32 = 7;
const INT64: u32 = 10;

/// What the game's data file names: objects by index, and which variables the game uses as
/// global ones (VARI's instance type -5; YYC builds have no VARI).
pub struct Names {
    globals: HashSet<String>,
    objects: Vec<String>,
}

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

fn cstr(d: &[u8], o: usize) -> Option<String> {
    let rest = d.get(o..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

impl Names {
    /// Reads OBJT (a list of offsets to {name, ...}) and VARI (bytecode 15 and later: a 12-byte
    /// header, then {name, instance type, id, occurrences, first use}).
    pub fn parse(d: &[u8]) -> Option<Names> {
        if d.get(..4) != Some(b"FORM") {
            return None;
        }
        let mut chunks: HashMap<&[u8], (usize, usize)> = HashMap::new();
        let mut i = 8;
        while i + 8 <= d.len() {
            let size = u32_at(d, i + 4)? as usize;
            chunks.insert(&d[i..i + 4], (i + 8, size));
            i += 8 + size;
        }
        let mut globals = HashSet::new();
        if let Some(&(vo, vs)) = chunks.get(&b"VARI"[..]) {
            let mut p = vo + 12;
            while p + 20 <= vo + vs {
                if u32_at(d, p + 4)? as i32 == -5 {
                    globals.extend(cstr(d, u32_at(d, p)? as usize));
                }
                p += 20;
            }
        }
        let (oo, _) = *chunks.get(&b"OBJT"[..])?;
        let n = u32_at(d, oo)? as usize;
        let objects = (0..n).map(|k| u32_at(d, oo + 4 + 4 * k).and_then(|e| cstr(d, u32_at(d, e as usize)? as usize)).unwrap_or_default()).collect();
        Some(Names { globals, objects })
    }
}

/// A variable's name the way the runner keeps it: a C string an array of slots points to
/// (map key - 100000), read from memory.
fn read_name(mem: &File, slots: u64, key: u32) -> Option<String> {
    let p = word(mem, slots + 8 * u64::from(key.checked_sub(KEY_BASE)?))?;
    let mut b = [0u8; 64];
    let n = mem.read_at(&mut b, p).ok()?;
    let end = b[..n].iter().position(|&c| c == 0)?;
    let t = std::str::from_utf8(&b[..end]).ok()?;
    (!t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@')).then(|| t.to_owned())
}

/// The game's data file: next to the program, or in its assets folder.
pub fn data_file(pid: u32, program: Option<&Path>) -> Option<PathBuf> {
    let dir = program?.parent()?;
    let root = Path::new("/proc").join(pid.to_string()).join("root");
    let inside = root.join(dir.strip_prefix("/").unwrap_or(dir));
    ["data.win", "game.unx", "assets/game.unx"].iter().map(|f| inside.join(f)).find(|p| p.exists())
}

/// An owner of a variable map: the global object or an instance.
#[derive(Clone, Debug)]
struct Owner {
    addr: u64,
    /// The variable map's header.
    map: u64,
    /// Instance id and object index; None for the global object.
    instance: Option<(u32, u32)>,
}

/// The located objects of one game (per run).
pub struct Gml {
    names: Names,
    /// The runner's array of variable names by slot (a map key - 100000).
    slots: u64,
    /// The names of the keys seen when located (others are read when asked for).
    known: HashMap<u32, String>,
    /// Where an instance's id is, from its start (the object index follows).
    id_at: u64,
    /// Instances' C++ vtable.
    class: u64,
    owners: Vec<Owner>,
    /// Maps found, for `gml` (diagnostics).
    maps: usize,
}

fn word(mem: &File, at: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b, at).ok().map(|_| u64::from_le_bytes(b))
}

fn half(mem: &File, at: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    mem.read_exact_at(&mut b, at).ok().map(|_| u32::from_le_bytes(b))
}

/// A map's entries: (RValue address, key), read from its header.
fn entries(mem: &File, map: u64) -> Option<Vec<(u64, u32)>> {
    let mut h = [0u8; 24];
    mem.read_exact_at(&mut h, map).ok()?;
    let (size, used, mask, grow) = (u32_at(&h, 0)?, u32_at(&h, 4)?, u32_at(&h, 8)?, u32_at(&h, 12)?);
    if size < 8 || size > MAX_MAP || size & (size - 1) != 0 || mask != size - 1 || grow != size * 6 / 10 || used == 0 || used > size {
        return None;
    }
    let el = u64::from_le_bytes(h[16..24].try_into().unwrap());
    let mut b = vec![0u8; 16 * size as usize];
    mem.read_exact_at(&mut b, el).ok()?;
    let out: Vec<(u64, u32)> = b
        .chunks(16)
        .filter(|e| u32_at(e, 12) != Some(0))
        .map(|e| (u64::from_le_bytes(e[..8].try_into().unwrap()), u32_at(e, 8).unwrap_or(0)))
        .collect();
    (out.len() == used as usize).then_some(out)
}

impl Gml {
    /// Finds the variable maps (their headers, in one pass with the runtime's first two
    /// variables' names, "prototype" and "@@array@@"), their owners and the array of variable
    /// names pointing to those names (a second pass), and where instances keep their id (a vote).
    fn locate(heap: &Heap, names: Names) -> Option<Gml> {
        if heap.width() != 8 {
            return None;
        }
        let mem = heap.file();
        let mut heads = Vec::new();
        let (mut first, mut second) = (Vec::new(), Vec::new());
        heap.each_chunk(24, |addr, b, fresh| {
            for off in (0..fresh).step_by(8) {
                let Some(h) = b.get(off..off + 16) else { break };
                let (size, mask) = (u32_at(h, 0).unwrap(), u32_at(h, 8).unwrap());
                if mask.wrapping_add(1) != size || size < 8 || size > MAX_MAP || size & mask != 0 || u32_at(h, 12) != Some(size * 6 / 10) {
                    continue;
                }
                heads.push(addr + off as u64);
            }
            for i in 0..fresh.min(b.len().saturating_sub(10)) {
                match b[i] {
                    b'p' if &b[i..i + 10] == b"prototype\0" => first.push(addr + i as u64),
                    b'@' if &b[i..i + 10] == b"@@array@@\0" => second.push(addr + i as u64),
                    _ => {}
                }
            }
        });
        // Maps keyed by variables only (their names are checked once the names are found).
        let in_range = |k: u32| k >= KEY_BASE && k - KEY_BASE < MAX_MAP;
        let heads: Vec<(u64, Vec<(u64, u32)>)> =
            heads.into_iter().filter_map(|h| Some((h, entries(mem, h).filter(|e| e.iter().all(|&(_, k)| in_range(k)))?))).collect();
        let targets: Vec<u64> = heads.iter().map(|(h, _)| *h).chain(first.iter().copied()).collect();
        let refs = heap.referrers(&targets);
        let slots = refs.iter().copied().find(|&r| word(mem, r).is_some_and(|w| first.contains(&w)) && word(mem, r + 8).is_some_and(|w| second.contains(&w)))?;
        let mut known: HashMap<u32, String> = HashMap::new();
        let mut name_of = |k: u32| -> bool {
            if known.contains_key(&k) {
                return true;
            }
            match read_name(mem, slots, k) {
                Some(n) => {
                    known.insert(k, n);
                    true
                }
                None => false,
            }
        };
        let mut maps = HashMap::new();
        for (h, e) in &heads {
            if e.iter().all(|&(_, k)| name_of(k)) {
                maps.insert(*h, ());
            }
        }
        let objects = names.objects.len() as u32;
        let valid = |a: u64, at: u64| {
            let (Some(id), Some(obj)) = (half(mem, a + at), half(mem, a + at + 4)) else { return false };
            (KEY_BASE..100_000_000).contains(&id) && obj < objects
        };
        let candidates: Vec<(u64, u64)> =
            refs.iter().filter_map(|&r| Some((r.checked_sub(0x58)?, word(mem, r).filter(|m| maps.contains_key(m))?))).collect();
        // Instances are of one C++ class (one vtable): the class and offset where (nearly) all
        // its objects hold an id and an object index, all different, the most of them (a constant pair
        // passes the checks too: Zordak's renderers all had 0xffffff, 0 at +0x90), then the
        // smallest (ids count up from 100000). Voting over every owner let thousands of the
        // runtime's own objects outvote Dunkehr's few instances at its title (+0x138, not +0xbc).
        let mut classes: HashMap<u64, Vec<u64>> = HashMap::new();
        for &(o, _) in &candidates {
            if let Some(vt) = word(mem, o) {
                classes.entry(vt).or_default().push(o);
            }
        }
        let mut best: Option<((usize, std::cmp::Reverse<u32>), u64, u64)> = None;
        for (vt, members) in &classes {
            for at in (ID_FROM..ID_TO).step_by(4) {
                let mut ids: Vec<u32> = members.iter().filter(|&&o| valid(o, at)).filter_map(|&o| half(mem, o + at)).collect();
                if ids.len() * 10 < members.len() * 9 {
                    continue;
                }
                let found = ids.len();
                ids.sort_unstable();
                ids.dedup();
                // No two instances share an id (Dunkehr's array objects held 3 values at +0x138).
                if ids.len() * 10 < found * 9 {
                    continue;
                }
                let score = (ids.len(), std::cmp::Reverse(ids.last().copied().unwrap_or(u32::MAX)));
                if best.as_ref().is_none_or(|(b, _, _)| score > *b) {
                    best = Some((score, *vt, at));
                }
            }
        }
        let (_, class, id_at) = best?;
        let owners: Vec<Owner> = candidates
            .into_iter()
            .map(|(addr, map)| {
                let ok = word(mem, addr) == Some(class) && valid(addr, id_at);
                let instance = ok.then(|| (half(mem, addr + id_at).unwrap_or(0), half(mem, addr + id_at + 4).unwrap_or(0)));
                Owner { addr, map, instance }
            })
            .collect();
        Some(Gml { names, slots, known, id_at, class, owners, maps: maps.len() })
    }

    fn name(&self, mem: &File, key: u32) -> Option<String> {
        self.known.get(&key).cloned().or_else(|| read_name(mem, self.slots, key))
    }

    /// The key of a variable by name (a slot of the runner's array).
    fn key_of(&self, mem: &File, name: &str) -> Option<u32> {
        if let Some((k, _)) = self.known.iter().find(|(_, n)| *n == name) {
            return Some(*k);
        }
        (KEY_BASE..KEY_BASE + MAX_MAP).take_while(|&k| word(mem, self.slots + 8 * u64::from(k - KEY_BASE)).is_some()).find(|&k| read_name(mem, self.slots, k).as_deref() == Some(name))
    }

    /// The global object: of the owners that aren't instances, the one holding the most of the
    /// game's global variables (by VARI), or the most variables.
    fn global(&self, mem: &File) -> Option<&Owner> {
        self.owners.iter().filter(|o| o.instance.is_none()).max_by_key(|o| {
            let e = entries(mem, o.map).unwrap_or_default();
            (e.iter().filter(|&&(_, k)| self.name(mem, k).is_some_and(|n| self.names.globals.contains(&n))).count(), e.len())
        })
    }

    /// An owner's numbers: (variable name, address, kind).
    fn numbers(&self, mem: &File, o: &Owner) -> Vec<(String, u64, &'static str)> {
        let mut out: Vec<(String, u64, &'static str)> = entries(mem, o.map)
            .unwrap_or_default()
            .into_iter()
            .filter(|&(rv, _)| holds_number(mem, rv))
            .filter_map(|(rv, key)| Some((self.name(mem, key)?, rv, "gm")))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn object_name(&self, index: u32) -> String {
        self.names.objects.get(index as usize).cloned().unwrap_or_else(|| format!("object {index}"))
    }
}

/// The number an RValue's 16 bytes hold: a double, an int32 or an int64; None for others
/// (strings, arrays, undefined).
pub fn rvalue_number(b: &[u8]) -> Option<f64> {
    let v = b.get(..8)?;
    match u32_at(b, 12)? & 0xff_ffff {
        REAL => Some(f64::from_le_bytes(v.try_into().unwrap())).filter(|x| x.is_finite()),
        INT32 => Some(i32::from_le_bytes(v[..4].try_into().unwrap()) as f64),
        INT64 => Some(i64::from_le_bytes(v.try_into().unwrap()) as f64),
        _ => None,
    }
}

/// Whether an RValue holds a number now.
fn holds_number(mem: &File, rv: u64) -> bool {
    let mut b = [0u8; 16];
    mem.read_exact_at(&mut b, rv).is_ok() && rvalue_number(&b).is_some()
}

/// The game attached to, located; searched again when asked for after `REFRESH` (instances
/// come and go with rooms).
pub fn shared(heap: &Heap, program: Option<&Path>, fresh: bool) -> Option<Arc<Gml>> {
    const REFRESH: Duration = Duration::from_secs(2);
    static FOUND: Mutex<Option<(u32, Arc<Gml>, Instant)>> = Mutex::new(None);
    let mut found = FOUND.lock().unwrap();
    if let Some((pid, g, at)) = found.as_ref() {
        if *pid == heap.pid && (!fresh || at.elapsed() < REFRESH) {
            return Some(g.clone());
        }
    }
    let data = std::fs::read(data_file(heap.pid, program)?).ok()?;
    let g = Arc::new(Gml::locate(heap, Names::parse(&data)?)?);
    *found = Some((heap.pid, g.clone(), Instant::now()));
    Some(g)
}

/// A saved path: the global object's or an object's instances' variable.
#[derive(Clone, Debug, PartialEq)]
pub struct GmlPath {
    /// None for global.
    object: Option<String>,
    var: String,
}

impl GmlPath {
    pub fn parse(text: &str) -> Option<GmlPath> {
        let (object, var) = text.strip_prefix("gml:")?.rsplit_once('.')?;
        let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !ok(object) || !ok(var) {
            return None;
        }
        Some(GmlPath { object: (object != "global").then(|| object.to_owned()), var: var.to_owned() })
    }

    pub fn text(&self) -> String {
        format!("gml:{}.{}", self.object.as_deref().unwrap_or("global"), self.var)
    }

    pub fn describe(&self) -> String {
        match &self.object {
            Some(o) => format!("{} of every {o}", self.var),
            None => format!("global.{}", self.var),
        }
    }
}

/// The global object or the instances the path starts from.
pub fn find_roots(heap: &Heap, program: Option<&Path>, path: &GmlPath) -> Vec<u64> {
    let Some(g) = shared(heap, program, true) else { return Vec::new() };
    let mem = heap.file();
    match &path.object {
        None => g.global(mem).map(|o| vec![o.addr]).unwrap_or_default(),
        Some(name) => {
            let index = g.names.objects.iter().position(|o| o == name);
            g.owners.iter().filter(|o| o.instance.is_some_and(|(_, obj)| Some(obj as usize) == index)).map(|o| o.addr).collect()
        }
    }
}

/// Where the variable is in each root that still is what it was (an instance of the object,
/// the global object holding the variable): the RValue's value, while it holds a number.
pub fn walk(heap: &Heap, program: Option<&Path>, roots: &[u64], path: &GmlPath) -> Vec<u64> {
    let Some(g) = shared(heap, program, false) else { return Vec::new() };
    let mem = heap.file();
    let Some(key) = g.key_of(mem, &path.var) else { return Vec::new() };
    let index = path.object.as_ref().and_then(|n| g.names.objects.iter().position(|o| o == n));
    roots
        .iter()
        .filter(|&&r| match index {
            Some(i) => word(mem, r) == Some(g.class) && half(mem, r + g.id_at).is_some_and(|id| id >= KEY_BASE) && half(mem, r + g.id_at + 4) == Some(i as u32),
            None => path.object.is_none(),
        })
        .filter_map(|&r| {
            let map = word(mem, r + 0x58)?;
            let rv = entries(mem, map)?.into_iter().find(|&(_, k)| k == key)?.0;
            holds_number(mem, rv).then_some(rv)
        })
        .collect()
}

/// The path to a number some instance's or the global object's variable holds.
pub fn discover(heap: &Heap, program: Option<&Path>, target: u64) -> Option<(GmlPath, Vec<u64>)> {
    let g = shared(heap, program, true)?;
    let mem = heap.file();
    let global = g.global(mem).map(|o| o.addr);
    for o in &g.owners {
        if o.instance.is_none() && Some(o.addr) != global {
            continue;
        }
        let Some((rv, key)) = entries(mem, o.map).unwrap_or_default().into_iter().find(|&(rv, _)| rv == target) else { continue };
        let Some(var) = g.name(mem, key).filter(|_| holds_number(mem, rv)) else { continue };
        let object = o.instance.map(|(_, obj)| g.object_name(obj));
        let path = GmlPath { object, var };
        let leads = walk(heap, program, &find_roots(heap, program, &path), &path);
        if leads.contains(&rv) {
            return Some((path, leads));
        }
    }
    None
}

/// The Browse tab's list: the global variables, then every instance's numbers (the player's
/// objects by name in the main list).
pub fn player(heap: &Heap, program: Option<&Path>) -> Option<Vec<ListedObject>> {
    let g = shared(heap, program, true)?;
    let mem = heap.file();
    let mut out = Vec::new();
    if let Some(o) = g.global(mem) {
        let fields = g.numbers(mem, o).into_iter().map(|(label, addr, kind)| Listed { label, addr, kind }).collect();
        out.push(ListedObject { name: "@global".into(), class: "global".into(), role: "game", owner: None, more: false, fields });
    }
    let mut instances: Vec<&Owner> = g.owners.iter().filter(|o| o.instance.is_some()).collect();
    instances.sort_by_key(|o| o.instance);
    let mut listed = 0;
    for o in instances {
        let (id, obj) = o.instance.unwrap_or_default();
        let class = g.object_name(obj);
        let fields: Vec<Listed> = g.numbers(mem, o).into_iter().map(|(label, addr, kind)| Listed { label, addr, kind }).collect();
        if fields.is_empty() || listed > crate::ue::MAX_LISTED {
            continue;
        }
        listed += fields.len();
        let main = crate::mono::player_like(&class);
        out.push(ListedObject { name: format!("{class} ({id})"), class, role: if main { "character" } else { "game" }, owner: None, more: !main, fields });
    }
    out.sort_by_key(|o| (o.more, o.role != "character"));
    Some(out)
}

/// gml: what was found (maps, owners, the id offset, the global object).
pub fn diagnose(heap: &Heap, program: Option<&Path>) -> Vec<String> {
    let Some(path) = data_file(heap.pid, program) else { return vec!["no data.win or game.unx next to the program".into()] };
    let Ok(data) = std::fs::read(&path) else { return vec![format!("can't read {}", path.display())] };
    let Some(names) = Names::parse(&data) else { return vec![format!("{}: no OBJT chunk", path.display())] };
    let mut lines = vec![format!("{}: {} global variable names, {} objects", path.display(), names.globals.len(), names.objects.len())];
    // Every map header, valid keys or not: the biggest, with their key ranges.
    let mem = heap.file();
    let mut all: Vec<(u64, Vec<(u64, u32)>)> = Vec::new();
    heap.each_chunk(24, |addr, b, fresh| {
        for off in (0..fresh).step_by(8) {
            let Some(h) = b.get(off..off + 16) else { break };
            let (size, mask) = (u32_at(h, 0).unwrap(), u32_at(h, 8).unwrap());
            if mask.wrapping_add(1) == size && size >= 64 && size <= MAX_MAP && size & mask == 0 && u32_at(h, 12) == Some(size * 6 / 10) {
                if let Some(e) = entries(mem, addr + off as u64) {
                    all.push((addr + off as u64, e));
                }
            }
        }
    });
    all.sort_by_key(|(_, e)| std::cmp::Reverse(e.len()));
    for (h, e) in all.iter().take(12) {
        let (lo, hi) = (e.iter().map(|x| x.1).min().unwrap_or(0), e.iter().map(|x| x.1).max().unwrap_or(0));
        lines.push(format!("map 0x{h:x}: {} keys {lo}..{hi}", e.len()));
    }
    let Some(g) = shared(heap, program, true) else {
        lines.push("no variable maps found".into());
        return lines;
    };
    let instances = g.owners.iter().filter(|o| o.instance.is_some()).count();
    lines.push(format!("{} maps, {} owners, {instances} instances, id at +0x{:x}, names at 0x{:x}", g.maps, g.owners.len(), g.id_at, g.slots));
    for o in g.owners.iter().filter(|o| o.instance.is_some()).take(15) {
        let (id, obj) = o.instance.unwrap_or_default();
        let nums = g.numbers(mem, o);
        let sample: Vec<String> = nums
            .iter()
            .take(6)
            .map(|(n, a, _)| {
                let mut b = [0u8; 16];
                let v = mem.read_exact_at(&mut b, *a).ok().and_then(|_| rvalue_number(&b));
                format!("{n}={}", v.unwrap_or(f64::NAN))
            })
            .collect();
        lines.push(format!("  {} ({id}) at 0x{:x}: {}", g.object_name(obj), o.addr, sample.join(", ")));
    }
    if let Some(o) = g.global(mem) {
        lines.push(format!("global object 0x{:x}: {} numbers", o.addr, g.numbers(mem, o).len()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_read_back() {
        for text in ["gml:global.totalmoney", "gml:player1.hp", "gml:obj_Player_2.max_hp"] {
            assert_eq!(GmlPath::parse(text).unwrap().text(), text);
        }
        assert_eq!(GmlPath::parse("gml:global.gold").unwrap().object, None);
        assert!(GmlPath::parse("gml:hp").is_none());
        assert!(GmlPath::parse("gml:a b.c").is_none());
    }
}
