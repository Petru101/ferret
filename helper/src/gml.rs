//! GameMaker games (GameMaker Studio 2 before 2.3, bytecode games: their code and names are in
//! data.win or game.unx). Every instance, and the global object, keeps its variables in a hash
//! map at +0x58 (64-bit runners; 32-bit ones: voted on): {size (a power of two), used, mask =
//! size - 1, grow threshold = 0.6 size, elements}, elements {RValue*, key, hash (0 = empty)}
//! (16 bytes, 12 on 32-bit runners), the key being the
//! variable's id from the data file's VARI chunk + 100000. An RValue is 16 bytes {value, flags,
//! kind}: kind 0 a double, 7 an int32, 10 an int64 (Ferret's `gm` type reads whichever). An instance's id (100000 up) and object index (into the
//! OBJT chunk) sit side by side further in (+0xbc and +0xc4 on the runners seen; voted on). So a value is
//! named by its object and variable: `gml:global.totalmoney`, `gml:player1.hp` (every
//! instance of player1).
//!
//! GameMaker Studio 1.4 runners (bytecode 14-15 and YYC builds; their 32-bit Linux and Windows
//! runners) keep them flat instead: an owner's variables are an array of RValues at +4, one per
//! variable id (unset ones of kind 0xffffff), its length kept further in; instance variables
//! and global ones have ids of their own, named by the runner's tables of names (`Flat`).

use crate::names::Heap;
use crate::ue::{Listed, ListedObject};
use std::collections::{HashMap, HashSet};
use crate::mem::Mem;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Map keys are a variable's slot in the runner's array of names plus this (newer runners), or
/// the slot itself (older ones: Mobility's GameMaker Studio 2.0 runner adds 100000 only when it
/// looks a name up). Instance ids count from it too.
const KEY_BASE: u32 = 100000;
/// Where an instance's id and object index are looked for (from the map pointer's object; 32-bit
/// runners' objects are smaller).
const ID_FROM_64: u64 = 0x60;
const ID_FROM_32: u64 = 0x30;
const ID_TO: u64 = 0x200;
/// Where the variable map's pointer is in its owner: 64-bit runners; 32-bit ones are voted on
/// up to `MAP_AT_MAX`.
const MAP_AT_64: u64 = 0x58;
const MAP_AT_MAX: u64 = 0x80;
/// Largest variable map read.
const MAX_MAP: u32 = 1 << 16;
/// RValue kinds (0..15 in use; `UNSET` marks a variable not set yet).
const REAL: u32 = 0;
const UNSET: u32 = 0xff_ffff;
const INT32: u32 = 7;
const INT64: u32 = 10;

/// What the game's data file names: objects by index, and which variables the game uses as
/// global ones (VARI's instance type -5; YYC builds have no VARI).
#[derive(Clone)]
pub struct Names {
    globals: HashSet<String>,
    objects: Vec<String>,
    /// VARI's variables by id: (name, instance type: -5 global, -1 self, an object index...;
    /// -7 locals left out).
    vari: Vec<(u32, String, i32)>,
    /// The first rooms' instances (id, object index), for `gml` (diagnostics).
    placed: Vec<(u32, u32)>,
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
        let mut vari = Vec::new();
        if let Some(&(vo, vs)) = chunks.get(&b"VARI"[..]) {
            let mut p = vo + 12;
            while p + 20 <= vo + vs {
                let kind = u32_at(d, p + 4)? as i32;
                let name = cstr(d, u32_at(d, p)? as usize);
                if kind == -5 {
                    globals.extend(name.clone());
                }
                if let (Some(n), true) = (name, kind != -7) {
                    vari.push((u32_at(d, p + 8)?, n, kind));
                }
                p += 20;
            }
        }
        let (oo, _) = *chunks.get(&b"OBJT"[..])?;
        let n = u32_at(d, oo)? as usize;
        let objects = (0..n).map(|k| u32_at(d, oo + 4 + 4 * k).and_then(|e| cstr(d, u32_at(d, e as usize)? as usize)).unwrap_or_default()).collect();
        // ROOM: offsets to rooms {name, caption, width, height, speed, persistent, colour,
        // show colour, creation code, flags, backgrounds, views, instances*, tiles}; instances
        // {x, y, object index, id, ...}.
        let mut placed = Vec::new();
        if let Some(&(ro, _)) = chunks.get(&b"ROOM"[..]) {
            for r in 0..u32_at(d, ro).unwrap_or(0).min(4) as usize {
                let Some(room) = u32_at(d, ro + 4 + 4 * r) else { break };
                let Some(list) = u32_at(d, room as usize + 48) else { break };
                for k in 0..u32_at(d, list as usize).unwrap_or(0).min(8) as usize {
                    let Some(inst) = u32_at(d, list as usize + 4 + 4 * k) else { break };
                    if let (Some(obj), Some(id)) = (u32_at(d, inst as usize + 8), u32_at(d, inst as usize + 12)) {
                        placed.push((id, obj));
                    }
                }
            }
        }
        Some(Names { globals, objects, vari, placed })
    }
}

impl Names {
    fn vari_names(&self) -> Vec<String> {
        self.vari.iter().map(|v| v.1.clone()).collect()
    }
}

/// A variable's name the way the runner keeps it: a C string an array of slots points to
/// (map key - 100000), read from memory.
fn read_name(mem: &Mem, w: u64, slots: u64, base: u32, key: u32) -> Option<String> {
    let p = ptr(mem, w, slots + w * u64::from(key.checked_sub(base)?))?;
    let mut b = [0u8; 64];
    let n = mem.read_at(&mut b, p).ok()?;
    let end = b[..n].iter().position(|&c| c == 0)?;
    let t = std::str::from_utf8(&b[..end]).ok()?;
    (!t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@')).then(|| t.to_owned())
}

/// The game's data file: next to the program, or in its assets folder.
pub fn data_file(pid: u32, program: Option<&Path>) -> Option<PathBuf> {
    let dir = program?.parent()?;
    let inside = crate::os::inside(pid, dir);
    ["data.win", "game.unx", "assets/game.unx"].iter().map(|f| inside.join(f)).find(|p| p.exists())
}

/// Whether the program is a GameMaker runner: a data file next to it, or (a game packed into
/// one .exe, GameMaker Studio 1's single-file builds: Spelunky SD's Windows build) GML's own
/// function names in the program (every runner has `YoYo_OpenURL`). Programs past 256 MB
/// aren't read.
pub fn is_runner(pid: u32, program: Option<&Path>) -> bool {
    if data_file(pid, program).is_some() {
        return true;
    }
    let Some(path) = program.map(|p| crate::os::inside(pid, p)) else { return false };
    if std::fs::metadata(&path).map_or(true, |m| m.len() > 256 << 20) {
        return false;
    }
    std::fs::read(&path).is_ok_and(|d| d.windows(12).any(|w| w == b"YoYo_OpenURL"))
}

/// The game's data: its data file, or (packed into the program, not even compressed the same
/// way) the copy the runner keeps in memory, found as "FORM", its size, "GEN8". Kept per pid.
fn game_data(heap: &Heap, program: Option<&Path>) -> Option<Arc<Vec<u8>>> {
    static DATA: Mutex<Option<(u32, Arc<Vec<u8>>)>> = Mutex::new(None);
    if let Some((pid, d)) = DATA.lock().unwrap().as_ref() {
        if *pid == heap.pid {
            return Some(d.clone());
        }
    }
    let data = match data_file(heap.pid, program) {
        Some(path) => std::fs::read(path).ok()?,
        None => {
            let mut at = None;
            heap.each_chunk(12, |addr, b, fresh| {
                if at.is_some() {
                    return;
                }
                for off in (0..fresh).step_by(4) {
                    if b.get(off..off + 4) == Some(b"FORM") && b.get(off + 8..off + 12) == Some(b"GEN8") {
                        at = Some((addr + off as u64, u32_at(b, off + 4).unwrap_or(0)));
                        return;
                    }
                }
            });
            let (at, size) = at?;
            let mut d = vec![0u8; size as usize + 8];
            heap.mem().read_exact_at(&mut d, at).ok()?;
            d
        }
    };
    let d = Arc::new(data);
    *DATA.lock().unwrap() = Some((heap.pid, d.clone()));
    Some(d)
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
    pid: u32,
    names: Names,
    /// Pointer width (8, or 4 for 32-bit runners).
    w: u64,
    /// Where an owner keeps its variable map's pointer.
    map_at: u64,
    /// What map keys add to a slot (`KEY_BASE` or 0).
    base: u32,
    /// Older runners (keys from 0): the global object's map is keyed by VARI's ids of global
    /// variables, named from the data file, not the runner's array; the object itself.
    global_names: HashMap<u32, String>,
    global_owner: Option<u64>,
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
    /// GameMaker Studio 1.4: arrays instead of maps.
    flat: Option<Flat>,
}

/// GameMaker Studio 1.4's arrays of variables: how many an instance's and the global object's
/// hold (the runner's counts of instance and global variable names).
#[derive(Clone, Copy, Debug)]
struct Flat {
    instance_n: u32,
    global_n: u32,
}

/// The runner's tables of variable names (GameMaker Studio 1.4): three {names*, count} pairs
/// side by side in its data: names shared by instance and global variables, global ones,
/// instance ones (the first pair empty when the game keeps the two apart, as bytecode 15 games
/// do; the other two empty when it doesn't). Seen at 0x8474200 in Iron Snout's runner (its
/// name lookup, decompiled). Returns (global table, instance table) as (address, count).
fn name_tables(heap: &Heap, program: Option<&Path>, vari_names: &[String]) -> Option<((u64, u32), (u64, u32))> {
    let w = heap.width() as u64;
    let mem = heap.mem();
    let file = program?.file_name()?.to_string_lossy().into_owned();
    let regions = crate::helper::maps(heap.pid).ok()?;
    // The program's data and the zeroed data after it.
    let mut ranges = Vec::new();
    for (i, r) in regions.iter().enumerate() {
        if r.perms.starts_with("rw") && r.path.rsplit('/').next() == Some(file.as_str()) {
            ranges.push((r.start, r.end));
            if let Some(next) = regions.get(i + 1).filter(|n| n.start == r.end && n.path.is_empty() && n.perms.starts_with("rw")) {
                ranges.push((next.start, next.end.min(next.start + (16 << 20))));
            }
        }
    }
    // At least 8 names, nearly all of the first 16 identifiers (Iron Snout's second is empty).
    let table = |p: u64, n: u32| -> bool {
        if p == 0 || !(8..=MAX_MAP).contains(&n) || !heap.is_pointer(p) {
            return false;
        }
        let k = n.min(16);
        (0..k).filter(|&i| ptr(mem, w, p + w * u64::from(i)).and_then(|s| read_cstr(mem, s)).is_some()).count() + 2 >= k as usize
    };
    let mut found = Vec::new();
    let mut data = Vec::new();
    for (start, end) in ranges {
        let mut b = vec![0u8; (end - start) as usize];
        let Ok(n) = mem.read_at(&mut b, start) else { continue };
        b.truncate(n);
        for off in (0..b.len().saturating_sub(6 * w as usize)).step_by(w as usize) {
            let pair = |k: usize| (ptr_in(&b, w, off + 2 * w as usize * k).unwrap_or(0), u32_at(&b, off + 2 * w as usize * k + w as usize).unwrap_or(0));
            let (shared, global, instance) = (pair(0), pair(1), pair(2));
            if instance.1 == 0 && shared.1 == 0 {
                continue;
            }
            if table(global.0, global.1) && table(instance.0, instance.1) && (shared == (0, 0) || table(shared.0, shared.1)) {
                found.push((global, instance));
            } else if table(shared.0, shared.1) && global == (0, 0) && instance == (0, 0) {
                found.push((shared, shared));
            }
        }
        data.push(b);
    }
    if found.len() < 2 {
        return found.pop();
    }
    // Other tables have the same shape (the scripts' names, in Spelunky SD's Windows build):
    // the one YYC's own references agree with wins, pairs {name*, id} in the program's data
    // whose name is the table's at id - 100000 (bytecode games have none: the table holding
    // the most of VARI's names then).
    let agree = |(at, n): (u64, u32)| -> usize {
        let names = table_names(mem, w, (at, n));
        let refs = data
            .iter()
            .map(|b| {
                (0..b.len().saturating_sub(2 * w as usize))
                    .step_by(w as usize)
                    .filter(|&o| {
                        let id = u32_at(b, o + w as usize).unwrap_or(0);
                        id >= KEY_BASE && id - KEY_BASE < n && ptr_in(b, w, o).and_then(|p| read_cstr(mem, p)).is_some_and(|s| names.get(&(id - KEY_BASE)) == Some(&s))
                    })
                    .count()
            })
            .sum::<usize>();
        let vari: HashSet<&String> = names.values().collect();
        refs * 1000 + vari_names.iter().filter(|v| vari.contains(v)).count()
    };
    found.into_iter().max_by_key(|&(g, i)| agree(i) + agree(g))
}

/// A variable name: an identifier, as a C string.
fn read_cstr(mem: &Mem, at: u64) -> Option<String> {
    let mut b = [0u8; 64];
    let n = mem.read_at(&mut b, at).ok()?;
    let end = b[..n].iter().position(|&c| c == 0)?;
    let t = std::str::from_utf8(&b[..end]).ok()?;
    (!t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@')).then(|| t.to_owned())
}

/// A table's names by index.
fn table_names(mem: &Mem, w: u64, (at, n): (u64, u32)) -> HashMap<u32, String> {
    (0..n).filter_map(|i| Some((i, read_cstr(mem, ptr(mem, w, at + w * u64::from(i))?)?))).collect()
}

/// A flat array's set variables: (RValue address, id).
fn flat_entries(mem: &Mem, arr: u64, n: u32) -> Vec<(u64, u32)> {
    let mut b = vec![0u8; 16 * n.min(MAX_MAP) as usize];
    if mem.read_exact_at(&mut b, arr).is_err() {
        return Vec::new();
    }
    (0..n).filter(|&i| u32_at(&b, 16 * i as usize + 12).is_some_and(|k| k & UNSET != UNSET)).map(|i| (arr + 16 * u64::from(i), i)).collect()
}

/// Whether a flat array's first `n` entries are all RValues (a known kind or unset) and some
/// are set.
fn flat_ok(mem: &Mem, arr: u64, n: u32) -> bool {
    let mut b = vec![0u8; 16 * n.min(MAX_MAP) as usize];
    if n == 0 || mem.read_exact_at(&mut b, arr).is_err() {
        return false;
    }
    let kinds: Vec<u32> = (0..n as usize).filter_map(|i| u32_at(&b, 16 * i + 12)).map(|k| k & UNSET).collect();
    kinds.iter().all(|&k| k < 16 || k == UNSET) && kinds.iter().any(|&k| k != UNSET)
}

fn word(mem: &Mem, at: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b, at).ok().map(|_| u64::from_le_bytes(b))
}

fn half(mem: &Mem, at: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    mem.read_exact_at(&mut b, at).ok().map(|_| u32::from_le_bytes(b))
}

/// A pointer of the runner's width.
fn ptr(mem: &Mem, w: u64, at: u64) -> Option<u64> {
    match w {
        4 => half(mem, at).map(u64::from),
        _ => word(mem, at),
    }
}

fn ptr_in(b: &[u8], w: u64, o: usize) -> Option<u64> {
    match w {
        4 => u32_at(b, o).map(u64::from),
        _ => b.get(o..o + 8).map(|x| u64::from_le_bytes(x.try_into().unwrap())),
    }
}

/// Whether a map header's grow threshold is GameMaker Studio 2's (0.6 of its size: entries
/// {RValue*, key, hash}) or later GameMaker Studio 1.4 runners' (0.8: entries {key, RValue*,
/// hash}, Spelunky SD's YYC runner); None when it isn't a map's.
fn map_kind(size: u32, mask: u32, grow: u32) -> Option<bool> {
    if size < 8 || size > MAX_MAP || size & (size - 1) != 0 || mask != size - 1 {
        return None;
    }
    match grow {
        g if g == size * 6 / 10 => Some(false),
        g if g == size * 8 / 10 => Some(true),
        _ => None,
    }
}

/// A map's entries: (RValue address, key), read from its header ({size, used, mask, grow,
/// elements*}); an entry is {RValue*, key, hash} or {key, RValue*, hash} (`map_kind`).
fn entries(mem: &Mem, w: u64, map: u64) -> Option<Vec<(u64, u32)>> {
    let mut h = [0u8; 24];
    mem.read_exact_at(&mut h[..16 + w as usize], map).ok()?;
    let (size, used, mask, grow) = (u32_at(&h, 0)?, u32_at(&h, 4)?, u32_at(&h, 8)?, u32_at(&h, 12)?);
    let key_first = map_kind(size, mask, grow)?;
    if used == 0 || used > size {
        return None;
    }
    let el = ptr_in(&h, w, 16)?;
    let es = w as usize + 8;
    let mut b = vec![0u8; es * size as usize];
    mem.read_exact_at(&mut b, el).ok()?;
    let out: Vec<(u64, u32)> = b
        .chunks(es)
        .filter(|e| u32_at(e, es - 4) != Some(0))
        .map(|e| match key_first {
            // {key (4 bytes, padded to a pointer), RValue*, hash}: 32-bit only so far.
            true => (ptr_in(e, w, w as usize).unwrap_or(0), u32_at(e, 0).unwrap_or(0)),
            false => (ptr_in(e, w, 0).unwrap_or(0), u32_at(e, es - 8).unwrap_or(0)),
        })
        .collect();
    (out.len() == used as usize).then_some(out)
}

/// Instances are of one C++ class (one vtable): the class and offset where (nearly) all its
/// objects hold an id and an object index, all different, the most of them (a constant pair
/// passes the checks too: Zordak's renderers all had 0xffffff, 0 at +0x90), then the smallest
/// (ids count up from 100000). Voting over every owner let thousands of the runtime's own
/// objects outvote Dunkehr's few instances at its title (+0x138, not +0xbc). `owners` are
/// windows of memory from each candidate owner's start, `ID_TO` bytes long.
fn vote_instances(owners: &[Vec<u8>], w: u64, objects: u32) -> Option<((usize, std::cmp::Reverse<u32>), u64, u64)> {
    let valid = |b: &[u8], at: u64| {
        let (Some(id), Some(obj)) = (u32_at(b, at as usize), u32_at(b, at as usize + 4)) else { return false };
        (KEY_BASE..100_000_000).contains(&id) && obj < objects
    };
    let mut classes: HashMap<u64, Vec<&Vec<u8>>> = HashMap::new();
    for b in owners {
        if let Some(vt) = ptr_in(b, w, 0) {
            classes.entry(vt).or_default().push(b);
        }
    }
    let mut best: Option<((usize, std::cmp::Reverse<u32>), u64, u64)> = None;
    for (vt, members) in &classes {
        for at in (if w == 8 { ID_FROM_64 } else { ID_FROM_32 }..ID_TO - 8).step_by(4) {
            let mut ids: Vec<u32> = members.iter().filter(|b| valid(b, at)).filter_map(|b| u32_at(b, at as usize)).collect();
            if ids.len() * 10 < members.len() * 9 || ids.is_empty() {
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
    best
}

impl Gml {
    /// Finds the variable maps (their headers, in one pass with the runtime's first two
    /// variables' names, "prototype" and "@@array@@"), their owners and the array of variable
    /// names pointing to those names (a second pass), and where instances keep their id (a vote).
    fn locate(heap: &Heap, program: Option<&Path>, names: Names) -> Result<Gml, String> {
        let w = heap.width() as u64;
        let mem = heap.mem();
        let mut heads = Vec::new();
        let (mut first, mut second) = (Vec::new(), Vec::new());
        heap.each_chunk(24, |addr, b, fresh| {
            for off in (0..fresh).step_by(w as usize) {
                let Some(h) = b.get(off..off + 16) else { break };
                if map_kind(u32_at(h, 0).unwrap(), u32_at(h, 8).unwrap(), u32_at(h, 12).unwrap()).is_none() {
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
        let heads: Vec<(u64, Vec<(u64, u32)>)> =
            heads.into_iter().filter_map(|h| Some((h, entries(mem, w, h).filter(|e| e.iter().all(|&(_, k)| k < KEY_BASE + MAX_MAP))?))).collect();
        let targets: Vec<u64> = heads.iter().map(|(h, _)| *h).chain(first.iter().copied()).collect();
        let refs = heap.referrers(&targets);
        // The runtime's names start "prototype", "@@array@@" (GameMaker Studio 2); GameMaker
        // Studio 1.4 runners have their tables of names (one shared by both kinds of variables
        // here: tables kept apart are read as `Flat`).
        let slots = refs
            .iter()
            .copied()
            .find(|&r| ptr(mem, w, r).is_some_and(|p| first.contains(&p)) && ptr(mem, w, r + w).is_some_and(|p| second.contains(&p)))
            .or_else(|| name_tables(heap, program, &names.vari_names()).filter(|(g, i)| g == i).map(|(_, i)| i.0))
            .ok_or(format!(
                "no array of variable names ({} variable maps, {} \"prototype\", {} \"@@array@@\", {} pointers to them)",
                heads.len(),
                first.len(),
                second.len(),
                refs.len()
            ))?;
        // The keys' base: the one under which the most maps have a name for every key.
        let named = |base: u32| -> (HashMap<u32, String>, HashMap<u64, ()>) {
            let mut known: HashMap<u32, String> = HashMap::new();
            let mut maps = HashMap::new();
            for (h, e) in &heads {
                let all = e.iter().all(|&(_, k)| {
                    if known.contains_key(&k) {
                        return true;
                    }
                    match (k >= base && k - base < MAX_MAP).then(|| read_name(mem, w, slots, base, k)).flatten() {
                        Some(n) => {
                            known.insert(k, n);
                            true
                        }
                        None => false,
                    }
                });
                if all {
                    maps.insert(*h, ());
                }
            }
            (known, maps)
        };
        let objects = names.objects.len() as u32;
        let regions = crate::helper::maps(heap.pid).unwrap_or_default();
        let in_image = |a: u64| regions.iter().any(|r| r.start <= a && a < r.end && !r.path.is_empty() && !r.path.starts_with('['));
        // For each way of keying, its named maps, what points to them (with the memory around:
        // from MAP_AT_MAX before, an owner's start, to ID_TO after) and the instance vote.
        let try_base = |base: u32| {
            let (known, maps) = named(base);
            let windows: Vec<(u64, u64, Vec<u8>)> = refs
                .iter()
                .filter_map(|&r| {
                    let map = ptr(mem, w, r).filter(|m| maps.contains_key(m))?;
                    let from = r.checked_sub(MAP_AT_MAX)?;
                    let mut b = vec![0u8; (MAP_AT_MAX + ID_TO) as usize];
                    let n = mem.read_at(&mut b, from).ok()?;
                    b.truncate(n);
                    Some((r, map, b))
                })
                .collect();
            let owners_at = |at: u64| -> Vec<Vec<u8>> {
                windows.iter().filter_map(|(_, _, b)| b.get((MAP_AT_MAX - at) as usize..).map(|x| x.to_vec())).collect()
            };
            let offsets: Vec<u64> = if w == 8 { vec![MAP_AT_64] } else { (w..=MAP_AT_MAX).step_by(w as usize).collect() };
            // Offsets that find the same instances tie (a field before the map, shared by them,
            // at +0x4 on Mobility's runner; its map is at +0x2c): the one whose class is a
            // vtable, in the program's image, wins.
            let vote = offsets
                .into_iter()
                .filter_map(|at| vote_instances(&owners_at(at), w, objects).map(|(score, vt, id)| (score, at, (vt, id))))
                .max_by_key(|(score, at, (vt, _))| (*score, in_image(*vt), std::cmp::Reverse(*at)));
            (base, known, maps, windows, vote)
        };
        // The keying that finds the most instances (keyed from 0, many unrelated maps get names
        // by chance: Dunkehr's 36000; a tie keeps KEY_BASE).
        let (base, known, maps, windows, vote) = [try_base(KEY_BASE), try_base(0)]
            .into_iter()
            .max_by_key(|(b, _, _, _, v)| (v.as_ref().map(|(score, _, _)| *score), *b == KEY_BASE))
            .unwrap();
        let (_, map_at, (class, id_at)) = vote.ok_or(format!("no instances: {} named variable maps, {} pointers to them", maps.len(), windows.len()))?;
        let valid = |a: u64| {
            let (Some(id), Some(obj)) = (half(mem, a + id_at), half(mem, a + id_at + 4)) else { return false };
            (KEY_BASE..100_000_000).contains(&id) && obj < objects
        };
        let mut owners: Vec<Owner> = windows
            .iter()
            .filter_map(|&(r, map, _)| {
                let addr = r.checked_sub(map_at)?;
                let ok = ptr(mem, w, addr) == Some(class) && valid(addr);
                let instance = ok.then(|| (half(mem, addr + id_at).unwrap_or(0), half(mem, addr + id_at + 4).unwrap_or(0)));
                Some(Owner { addr, map, instance })
            })
            .collect();
        // Older runners: the global object's map, keyed by global variables' VARI ids (the
        // map with the most entries all of them; its owner what points to it, at -map_at).
        let mut global_names = HashMap::new();
        let mut global_owner = None;
        if base == 0 {
            global_names = names.vari.iter().filter(|v| v.2 == -5).map(|v| (v.0, v.1.clone())).collect();
            let best = heads.iter().filter(|(_, e)| e.iter().all(|(_, k)| global_names.contains_key(k))).max_by_key(|(_, e)| e.len());
            if let Some(&(map, _)) = best {
                if let Some(addr) = heap.referrers(&[map]).into_iter().find_map(|r| r.checked_sub(map_at)) {
                    owners.push(Owner { addr, map, instance: None });
                    global_owner = Some(addr);
                }
            }
        }
        Ok(Gml { pid: heap.pid, names, w, map_at, base, global_names, global_owner, slots, known, id_at, class, owners, maps: maps.len(), flat: None })
    }

    /// GameMaker Studio 1.4: the runner's name tables; owners are objects with a C++ vtable,
    /// an array of variables at +w and that array's length (the count of instance variable
    /// names) a few words in; the instances' class and id offset by vote as for maps; the
    /// global object the owner of another class with the most global variables set.
    fn locate_flat(heap: &Heap, program: Option<&Path>, names: Names) -> Result<Gml, String> {
        let w = heap.width() as u64;
        let mem = heap.mem();
        let (globals, instances) = name_tables(heap, program, &names.vari_names()).ok_or("no tables of variable names")?;
        let flat = Flat { instance_n: instances.1, global_n: globals.1 };
        let mut windows: Vec<(u64, u64, Vec<u8>)> = Vec::new();
        let ws = w as usize;
        heap.each_chunk(ID_TO as usize, |addr, b, fresh| {
            for off in (0..fresh).step_by(ws) {
                let (Some(vt), Some(arr)) = (ptr_in(b, w, off), ptr_in(b, w, off + ws)) else { break };
                if vt % w != 0 || !heap.read_only(vt) || !heap.is_pointer(arr) {
                    continue;
                }
                let holds_n = (2..10).any(|k| u32_at(b, off + k * 4) == Some(flat.instance_n));
                if holds_n {
                    if let Some(win) = b.get(off..off + ID_TO as usize) {
                        windows.push((addr + off as u64, arr, win.to_vec()));
                    }
                }
            }
        });
        windows.retain(|(_, arr, _)| flat_ok(mem, *arr, flat.instance_n.min(flat.global_n.max(1)).min(64)));
        let objects = names.objects.len() as u32;
        let bytes: Vec<Vec<u8>> = windows.iter().map(|(_, _, b)| b.clone()).collect();
        let (_, class, id_at) = vote_instances(&bytes, w, objects).ok_or(format!("no instances among {} objects with variable arrays", windows.len()))?;
        let mut owners: Vec<Owner> = windows
            .iter()
            .map(|(addr, arr, b)| {
                let (id, obj) = (u32_at(b, id_at as usize).unwrap_or(0), u32_at(b, id_at as usize + 4).unwrap_or(0));
                let ok = ptr_in(b, w, 0) == Some(class) && (KEY_BASE..100_000_000).contains(&id) && obj < objects;
                Owner { addr: *addr, map: *arr, instance: ok.then_some((id, obj)) }
            })
            .collect();
        let global_owner = owners
            .iter()
            .filter(|o| o.instance.is_none() && ptr(mem, w, o.addr) != Some(class) && flat_ok(mem, o.map, flat.global_n))
            .max_by_key(|o| flat_entries(mem, o.map, flat.global_n).len())
            .map(|o| o.addr);
        owners.retain(|o| o.instance.is_some() || Some(o.addr) == global_owner);
        let maps = windows.len();
        Ok(Gml {
            pid: heap.pid,
            names,
            w,
            map_at: w,
            base: 0,
            global_names: table_names(mem, w, globals),
            global_owner,
            slots: instances.0,
            known: table_names(mem, w, instances),
            id_at,
            class,
            owners,
            maps,
            flat: Some(flat),
        })
    }

    /// An owner's variables: (RValue address, key).
    fn vars(&self, mem: &Mem, owner: u64, map: u64) -> Vec<(u64, u32)> {
        match self.flat {
            Some(f) => flat_entries(mem, map, if self.global_owner == Some(owner) { f.global_n } else { f.instance_n }),
            None => entries(mem, self.w, map).unwrap_or_default(),
        }
    }

    fn name(&self, mem: &Mem, key: u32) -> Option<String> {
        self.known.get(&key).cloned().or_else(|| read_name(mem, self.w, self.slots, self.base, key))
    }

    /// A key's name in an owner's map: the global object's on older runners from the data file.
    fn name_in(&self, mem: &Mem, owner: u64, key: u32) -> Option<String> {
        match self.global_owner == Some(owner) {
            true => self.global_names.get(&key).cloned(),
            false => self.name(mem, key),
        }
    }

    /// The key of a variable by name (a slot of the runner's array; for the global object of an
    /// older runner its VARI id).
    fn key_of(&self, mem: &Mem, name: &str, global: bool) -> Option<u32> {
        if global && self.global_owner.is_some() {
            return self.global_names.iter().find(|(_, n)| *n == name).map(|(k, _)| *k);
        }
        if let Some((k, _)) = self.known.iter().find(|(_, n)| *n == name) {
            return Some(*k);
        }
        (self.base..self.base + MAX_MAP)
            .take_while(|&k| ptr(mem, self.w, self.slots + self.w * u64::from(k - self.base)).is_some())
            .find(|&k| read_name(mem, self.w, self.slots, self.base, k).as_deref() == Some(name))
    }

    /// The global object: of the owners that aren't instances, the one holding the most of the
    /// game's global variables (by VARI), or the most variables; of those sharing its map, the
    /// one that starts with a C++ vtable (a stack frame held a copy of Spelunky SD's pointer).
    fn global(&self, mem: &Mem) -> Option<&Owner> {
        if let Some(g) = self.global_owner {
            return self.owners.iter().find(|o| o.addr == g);
        }
        let image = crate::helper::maps(self.pid).unwrap_or_default();
        let vtable = |o: &Owner| ptr(mem, self.w, o.addr).is_some_and(|v| image.iter().any(|r| r.start <= v && v < r.end && !r.perms.contains('w') && r.path.starts_with('/')));
        self.owners.iter().filter(|o| o.instance.is_none()).max_by_key(|o| {
            let e = entries(mem, self.w, o.map).unwrap_or_default();
            (e.iter().filter(|&&(_, k)| self.name(mem, k).is_some_and(|n| self.names.globals.contains(&n))).count(), e.len(), vtable(o))
        })
    }

    /// An owner's numbers: (variable name, address, kind).
    fn numbers(&self, mem: &Mem, o: &Owner) -> Vec<(String, u64, &'static str)> {
        let mut out: Vec<(String, u64, &'static str)> = self
            .vars(mem, o.addr, o.map)
            .into_iter()
            .filter(|&(rv, _)| holds_number(mem, rv))
            .filter_map(|(rv, key)| Some((self.name_in(mem, o.addr, key)?, rv, "gm")))
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
fn holds_number(mem: &Mem, rv: u64) -> bool {
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
    let names = Names::parse(&game_data(heap, program)?)?;
    // GameMaker Studio 1.4 first: its name tables are quick to look for.
    let g = Arc::new(Gml::locate_flat(heap, program, names.clone()).or_else(|_| Gml::locate(heap, program, names)).ok()?);
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
    let mem = heap.mem();
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
    let mem = heap.mem();
    let Some(key) = g.key_of(mem, &path.var, path.object.is_none()) else { return Vec::new() };
    let index = path.object.as_ref().and_then(|n| g.names.objects.iter().position(|o| o == n));
    roots
        .iter()
        .filter(|&&r| match index {
            Some(i) => ptr(mem, g.w, r) == Some(g.class) && half(mem, r + g.id_at).is_some_and(|id| id >= KEY_BASE) && half(mem, r + g.id_at + 4) == Some(i as u32),
            None => path.object.is_none(),
        })
        .filter_map(|&r| {
            let map = ptr(mem, g.w, r + g.map_at)?;
            let rv = g.vars(mem, r, map).into_iter().find(|&(_, k)| k == key)?.0;
            holds_number(mem, rv).then_some(rv)
        })
        .collect()
}

/// The path to a number some instance's or the global object's variable holds.
pub fn discover(heap: &Heap, program: Option<&Path>, target: u64) -> Option<(GmlPath, Vec<u64>)> {
    let g = shared(heap, program, true)?;
    let mem = heap.mem();
    let global = g.global(mem).map(|o| o.addr);
    for o in &g.owners {
        if o.instance.is_none() && Some(o.addr) != global {
            continue;
        }
        let Some((rv, key)) = g.vars(mem, o.addr, o.map).into_iter().find(|&(rv, _)| rv == target) else { continue };
        let Some(var) = g.name_in(mem, o.addr, key).filter(|_| holds_number(mem, rv)) else { continue };
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
    let mem = heap.mem();
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
    let path = data_file(heap.pid, program).map_or("the data in memory".into(), |p| p.display().to_string());
    let Some(data) = game_data(heap, program) else { return vec!["no data.win or game.unx next to the program, none in memory".into()] };
    let Some(names) = Names::parse(&data) else { return vec![format!("{path}: no OBJT chunk")] };
    let mut lines = vec![format!("{path}: {} global variable names, {} objects", names.globals.len(), names.objects.len())];
    // Where the first rooms' instances are (their id and object index side by side), with the
    // words before them: an instance's layout.
    let mut seen = 0;
    for &(id, obj) in names.placed.iter().take(4) {
        let mut hits = Vec::new();
        heap.each_chunk(8, |addr, b, fresh| {
            for off in (0..fresh).step_by(4) {
                if u32_at(b, off) == Some(id) && u32_at(b, off + 4) == Some(obj) && hits.len() < 3 {
                    hits.push(addr + off as u64);
                }
            }
        });
        for at in hits {
            let mut b = vec![0u8; 0x60];
            let _ = heap.mem().read_at(&mut b, at.saturating_sub(0x58));
            let words: Vec<String> = b.chunks(4).map(|c| format!("{:x}", u32_at(c, 0).unwrap_or(0))).collect();
            lines.push(format!("  {} ({id}) at 0x{at:x}, from -0x58: {}", names.objects.get(obj as usize).map_or("?", |s| s.as_str()), words.join(" ")));
            seen += 1;
        }
    }
    if seen == 0 && !names.placed.is_empty() {
        lines.push(format!("none of the first rooms' {} instances found by id", names.placed.len()));
    }
    // Every map header, valid keys or not: the biggest, with their key ranges.
    let mem = heap.mem();
    let w = heap.width() as u64;
    let mut all: Vec<(u64, Vec<(u64, u32)>)> = Vec::new();
    heap.each_chunk(24, |addr, b, fresh| {
        for off in (0..fresh).step_by(w as usize) {
            let Some(h) = b.get(off..off + 16) else { break };
            if map_kind(u32_at(h, 0).unwrap(), u32_at(h, 8).unwrap(), u32_at(h, 12).unwrap()).is_some() {
                if let Some(e) = entries(mem, w, addr + off as u64) {
                    all.push((addr + off as u64, e));
                }
            }
        }
    });
    all.sort_by_key(|(_, e)| std::cmp::Reverse(e.len()));
    // Older runners key a map by VARI's id itself: how many of each map's keys are global (-5)
    // or instance (-1 and object indexes) variables' ids.
    let globals_ids: HashSet<u32> = names.vari.iter().filter(|v| v.2 == -5).map(|v| v.0).collect();
    let self_ids: HashSet<u32> = names.vari.iter().filter(|v| v.2 != -5).map(|v| v.0).collect();
    lines.push(format!("VARI: {} ids, {} global, {} instance", names.vari.len(), globals_ids.len(), self_ids.len()));
    let mut by_vari: Vec<(usize, usize, u64, usize)> = all
        .iter()
        .map(|(h, e)| (e.iter().filter(|x| globals_ids.contains(&x.1)).count(), e.iter().filter(|x| self_ids.contains(&x.1)).count(), *h, e.len()))
        .filter(|&(g, i, _, n)| g == n || i == n)
        .collect();
    by_vari.sort_by_key(|&(g, i, _, _)| std::cmp::Reverse(g.max(i)));
    lines.push(format!("{} maps keyed by VARI ids only", by_vari.len()));
    for (g, i, h, n) in by_vari.iter().take(8) {
        lines.push(format!("  map 0x{h:x}: {n} keys, {g} global ids, {i} instance ids, {} pointers to it", heap.referrers(&[*h]).len()));
    }
    // The biggest map keyed by global ids: whose keys have no name in the runner's array.
    if let (Some(&(_, _, h, _)), Some(g)) = (by_vari.first(), shared(heap, program, true)) {
        let e = entries(mem, w, h).unwrap_or_default();
        let unnamed: Vec<String> = e.iter().filter(|&&(_, k)| g.name(mem, k).is_none()).take(8).map(|&(_, k)| k.to_string()).collect();
        let nums = e.iter().filter(|&&(rv, _)| holds_number(mem, rv)).count();
        lines.push(format!("  map 0x{h:x}: {} of {} keys unnamed (first: {}), {nums} hold numbers", e.len() - e.iter().filter(|&&(_, k)| g.name(mem, k).is_some()).count(), e.len(), unnamed.join(" ")));
    }
    for (h, e) in all.iter().take(12) {
        let (lo, hi) = (e.iter().map(|x| x.1).min().unwrap_or(0), e.iter().map(|x| x.1).max().unwrap_or(0));
        lines.push(format!("map 0x{h:x}: {} keys {lo}..{hi}", e.len()));
    }
    let Some(g) = shared(heap, program, true) else {
        lines.push(format!("as GameMaker Studio 1.4: {}", Gml::locate_flat(heap, program, names.clone()).err().unwrap_or_default()));
        lines.push(Gml::locate(heap, program, names).err().unwrap_or_else(|| "no variable maps found".into()));
        return lines;
    };
    let instances = g.owners.iter().filter(|o| o.instance.is_some()).count();
    lines.push(format!(
        "{} maps, {} owners, {instances} instances, map at +0x{:x}, keys from {}, id at +0x{:x}, names at 0x{:x} ({}-bit)",
        g.maps,
        g.owners.len(),
        g.map_at,
        g.base,
        g.id_at,
        g.slots,
        g.w * 8
    ));
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
    if let Some(f) = g.flat {
        lines.push(format!("GameMaker Studio 1.4 arrays: {} instance variables, {} global ones; instances' class 0x{:x}", f.instance_n, f.global_n, g.class));
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

    #[test]
    fn map_kinds() {
        // GameMaker Studio 2 grows a map at 0.6 of its size, later 1.4 runners at 0.8.
        assert_eq!(map_kind(64, 63, 38), Some(false));
        assert_eq!(map_kind(32, 31, 25), Some(true));
        assert_eq!(map_kind(32, 31, 24), None);
        assert_eq!(map_kind(48, 47, 28), None);
        assert_eq!(map_kind(4, 3, 2), None);
    }
}
