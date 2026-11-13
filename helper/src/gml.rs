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
use std::sync::{Arc, Mutex, OnceLock};
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
/// Where the variable map's pointer is in its owner: 64-bit runners before GameMaker 2.3 (a
/// tie goes to it); every offset up to `MAP_AT_MAX` is voted on.
const MAP_AT_64: u64 = 0x58;
const MAP_AT_MAX: u64 = 0x80;
/// Largest variable map read.
const MAX_MAP: u32 = 1 << 16;
/// RValue kinds (0..15 in use; `UNSET` marks a variable not set yet).
const REAL: u32 = 0;
const UNSET: u32 = 0xff_ffff;
const INT32: u32 = 7;
const INT64: u32 = 10;
const ARRAY: u32 = 2;
const OBJECT: u32 = 6;
const STRING: u32 = 1;
/// A reference to a data structure (GameMaker 2024 and later).
const REF: u32 = 15;
/// A ds_map's value marked as a ds_list's or a ds_map's index (ds_map_add_list, json_decode).
const HOLDS_LIST: u32 = 0x4000_0000;
const HOLDS_MAP: u32 = 0x8000_0000;

/// What the game's data file names: objects by index, and which variables the game uses as
/// global ones (VARI's instance type -5; YYC builds have no VARI).
#[derive(Clone)]
pub struct Names {
    globals: HashSet<String>,
    objects: Vec<String>,
    /// VARI's variables by id: (name, instance type: -5 global, -1 self, an object index...;
    /// -7 locals left out).
    vari: Vec<(u32, String, i32)>,
    /// The instances placed in rooms (id, object index): what tells real instances in memory
    /// from look-alikes (`vote_instances`).
    placed: HashSet<(u32, u32)>,
    /// The first rooms' ones, for `gml` (diagnostics).
    first_placed: Vec<(u32, u32)>,
    /// Variables the game's code gives a new ds_list, ds_map or ds_grid (`ds_variables`).
    ds: HashMap<String, Ds>,
}

/// A data structure a variable holds only the index of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ds {
    List,
    Map,
    Grid,
    Stack,
    Queue,
    Priority,
}

impl Ds {
    /// The kind a reference names (the high half of its value): 0x02000000 and one bit each.
    /// Other references have another top byte with the same low bits: a surface's was taken
    /// for a grid's and a sprite's for a list's (Noise's Halloween Bash: `guisurf`).
    fn of_ref(kind: u32) -> Option<Ds> {
        if kind >> 24 != 2 {
            return None;
        }
        match kind & 0xff_ffff {
            1 => Some(Ds::List),
            2 => Some(Ds::Map),
            4 => Some(Ds::Grid),
            8 => Some(Ds::Queue),
            0x10 => Some(Ds::Stack),
            0x20 => Some(Ds::Priority),
            _ => None,
        }
    }
}

/// Which variables hold a ds_list, ds_map or ds_grid: those the game's bytecode stores the
/// result of ds_list_create, ds_map_create, json_decode or ds_grid_create in (a call followed
/// at once by a pop into the variable). A variable holds only a number, the structure's index
/// in the runner's table of its kind, so nothing in memory tells. FUNC lists each function's
/// calls and VARI each variable's uses as chains through the code: an instruction's operand
/// holds the distance to the next one (bytecode 15 and later). A name the code uses for two
/// kinds is left out; compiled games have no bytecode.
fn ds_variables(d: &[u8], chunks: &HashMap<&[u8], (usize, usize)>) -> HashMap<String, Ds> {
    let mut out: HashMap<String, Ds> = HashMap::new();
    let bytecode = chunks.get(&b"GEN8"[..]).and_then(|&(g, _)| d.get(g + 1).copied()).unwrap_or(0);
    let (Some(&(fo, fs)), Some(&(vo, vs))) = (chunks.get(&b"FUNC"[..]), chunks.get(&b"VARI"[..])) else { return out };
    if bytecode < 15 || fs < 4 {
        return out;
    }
    // The instructions of a chain. For a function's calls GameMaker 2.3 and later point at the
    // instruction's operand, 4 bytes in: told by where the first call's opcode (0xd9) is.
    let mut operand = 0usize;
    let chain = |first: usize, uses: usize, operand: usize| -> Vec<usize> {
        let Some(mut at) = first.checked_sub(operand) else { return Vec::new() };
        let mut all = Vec::new();
        for _ in 0..uses.min(1 << 20) {
            let Some(next) = u32_at(d, at + 4) else { break };
            all.push(at);
            at += (next & 0x07ff_ffff) as usize;
        }
        all
    };
    let mut calls: HashMap<usize, Ds> = HashMap::new();
    for k in 0..u32_at(d, fo).unwrap_or(0).min(1 << 20) as usize {
        let e = fo + 4 + 12 * k;
        let (Some(name), Some(uses), Some(first)) = (u32_at(d, e).and_then(|n| cstr(d, n as usize)), u32_at(d, e + 4), u32_at(d, e + 8)) else { break };
        let kind = match name.as_str() {
            "ds_list_create" => Ds::List,
            "ds_map_create" | "json_decode" => Ds::Map,
            "ds_grid_create" => Ds::Grid,
            "ds_stack_create" => Ds::Stack,
            "ds_queue_create" => Ds::Queue,
            "ds_priority_create" => Ds::Priority,
            _ => continue,
        };
        if first as i32 > 4 {
            if d.get(first as usize + 3) != Some(&0xd9) && d.get(first as usize - 1) == Some(&0xd9) {
                operand = 4;
            }
            calls.extend(chain(first as usize, uses as usize, operand).into_iter().map(|a| (a, kind)));
        }
    }
    // The pop right after such a call: whose variable it is.
    let wanted: HashMap<usize, Ds> = calls.into_iter().filter(|&(a, _)| d.get(a + 11) == Some(&0x45)).map(|(a, k)| (a + 8, k)).collect();
    let mut twice = HashSet::new();
    let mut p = vo + 12;
    while p + 20 <= vo + vs && !wanted.is_empty() {
        let (Some(name), Some(uses), Some(first)) = (u32_at(d, p).and_then(|n| cstr(d, n as usize)), u32_at(d, p + 12), u32_at(d, p + 16)) else { break };
        let local = u32_at(d, p + 4) == Some(-7i32 as u32);
        p += 20;
        if local || first as i32 <= 0 {
            continue;
        }
        // A variable's uses point at the instruction in every version.
        for at in chain(first as usize, uses as usize, 0) {
            if let Some(&kind) = wanted.get(&at) {
                if out.insert(name.clone(), kind).is_some_and(|old| old != kind) {
                    twice.insert(name.clone());
                }
            }
        }
    }
    out.retain(|n, _| !twice.contains(n));
    out
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
        let mut placed = HashSet::new();
        let mut first_placed = Vec::new();
        if let Some(&(ro, _)) = chunks.get(&b"ROOM"[..]) {
            for r in 0..u32_at(d, ro).unwrap_or(0).min(1 << 16) as usize {
                let Some(room) = u32_at(d, ro + 4 + 4 * r) else { break };
                let Some(list) = u32_at(d, room as usize + 48) else { continue };
                for k in 0..u32_at(d, list as usize).unwrap_or(0).min(1 << 20) as usize {
                    let Some(inst) = u32_at(d, list as usize + 4 + 4 * k) else { break };
                    if let (Some(obj), Some(id)) = (u32_at(d, inst as usize + 8), u32_at(d, inst as usize + 12)) {
                        placed.insert((id, obj));
                        if r < 4 && k < 8 {
                            first_placed.push((id, obj));
                        }
                    }
                }
            }
        }
        let ds = ds_variables(d, &chunks);
        Some(Names { globals, objects, vari, placed, first_placed, ds })
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
    // Up to 255 bytes: GameMaker 2.3 keeps its script functions among the global variables,
    // and Noise's Halloween Bash has names over 64 long.
    let mut b = [0u8; 256];
    let n = mem.read_at(&mut b, p).ok()?;
    let end = b[..n].iter().position(|&c| c == 0)?;
    let t = std::str::from_utf8(&b[..end]).ok()?;
    (!t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@')).then(|| t.to_owned())
}

/// The game's data file: next to the program, or in its assets folder.
pub fn data_file(pid: u32, program: Option<&Path>) -> Option<PathBuf> {
    // A runner told which game to run ("-game <file>"): that file, in the folder it was
    // started from. Deltarune is one runner with a launcher's data next to it and each
    // chapter's in a folder of its own; the launcher's names fit no chapter's memory.
    let command = crate::helper::cmdline(pid);
    if let Some(file) = command.split(" -game ").nth(1).and_then(|rest| rest.split(' ').next()) {
        let name = file.trim_matches('"').rsplit(['/', '\\']).next().unwrap_or_default();
        let told = crate::os::inside(pid, &Path::new(&crate::os::cwd(pid)).join(name));
        if !name.is_empty() && told.exists() {
            return Some(told);
        }
    }
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
    /// How this runner lays out a GML array (`array_at`), once one was seen.
    arrays: OnceLock<Option<ArrayAt>>,
    /// The runner's tables of ds_lists, ds_maps and ds_grids (`tables`), once looked for.
    tables: OnceLock<Tables>,
}

/// The runner's tables of data structures, arrays of pointers by index: for each kind the
/// address of the runner's own pointer to its table (read at every use: a table that grows
/// moves); the lists' with their C++ class.
#[derive(Clone, Copy, Debug, Default)]
struct Tables {
    lists: Option<(u64, u64)>,
    maps: Option<u64>,
    grids: Option<u64>,
}

/// Where a GML array's object keeps its elements. GameMaker 2.3 and later: a pointer to the
/// RValues and their count (`rows` false: Noise's Halloween Bash and Digital Tamers 2 +0x8 and
/// +0x24, Forgodden's 32-bit runner +0x6c and +0x7c, after its object header). Before: {refcount,
/// rows*, owner, visited, row count}, a row {length, RValue*} (`rows` true: Oak Defenders +0x8
/// and +0x1c; a 1D array is row 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ArrayAt {
    rows: bool,
    data: u64,
    len: u64,
}

/// A step into a value: a struct's variable, an array's or a ds_list's element, an old 2D
/// array's cell or a ds_grid's (x, y), a ds_map's value by its key: a text or a number (its
/// bits).
#[derive(Clone, Debug, PartialEq)]
enum Step {
    Field(String),
    Index(u32),
    Cell(u32, u32),
    Key(String),
    Num(u64),
}

/// The largest index of a data structure looked for, and the largest grid side.
const MAX_DS: u32 = 4096;
const MAX_GRID: u32 = 4096;

/// Most elements of one array read, and listed: the first ones, then only those that aren't 0
/// (Digital Tamers 2 keeps 5001 item counts in one).
const MAX_ARRAY: u32 = 1 << 16;
const LIST_FIRST: usize = 16;
const LIST_ARRAY: usize = 64;
/// Most numbers listed from inside one object's arrays and structs, and how deep.
const LIST_DEEP: usize = 4000;
const MAX_DEPTH: usize = 4;

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

/// A compiled (YYC) GameMaker 2 game's variable names by map key: the program's data holds
/// {name*, key} entries side by side, sorted by name, the keys from 100000 (Boom Boom Explodie
/// Box 3D: 1169 of them; the runner's "prototype" array of names isn't there). The longest run
/// of such entries with keys all different (the functions' tables follow, keys from 100000 too).
fn compiled_names(heap: &Heap) -> HashMap<u32, String> {
    let w = heap.width() as usize;
    let mem = heap.mem();
    let mut best: Vec<(u64, u32)> = Vec::new();
    // Two runs at once: entries may start at either half of a 2-pointer step.
    let mut runs: [Vec<(u64, u32)>; 2] = [Vec::new(), Vec::new()];
    let mut nexts = [0u64; 2];
    let mut close = |run: &mut Vec<(u64, u32)>| {
        let keys: HashSet<u32> = run.iter().map(|e| e.1).collect();
        if run.len() >= 32 && keys.len() == run.len() && run.len() > best.len() {
            best = std::mem::take(run);
        }
        run.clear();
    };
    heap.each_chunk(2 * w, |addr, b, fresh| {
        for off in (0..fresh).step_by(w) {
            let at = addr + off as u64;
            let phase = (at as usize / w) % 2;
            let (run, next) = (&mut runs[phase], &mut nexts[phase]);
            let entry = ptr_in(b, w as u64, off).filter(|&p| p > 0x1_0000).zip(u32_at(b, off + w).filter(|k| (KEY_BASE..KEY_BASE + MAX_MAP).contains(k)));
            if at != *next {
                close(run);
            }
            match entry {
                Some(e) => {
                    run.push(e);
                    *next = at + 2 * w as u64;
                }
                None => close(run),
            }
        }
    });
    for run in &mut runs {
        close(run);
    }
    // Any printable name: compiled games name some variables with characters GML's own
    // identifiers don't have.
    let name = |p: u64| {
        let mut b = [0u8; 256];
        let n = mem.read_at(&mut b, p).ok()?;
        let end = b[..n].iter().position(|&c| c == 0)?;
        (end > 0 && b[..end].iter().all(|c| c.is_ascii_graphic())).then(|| String::from_utf8_lossy(&b[..end]).into_owned())
    };
    let out: HashMap<u32, String> = best.iter().filter_map(|&(p, k)| Some((k, name(p)?))).collect();
    if out.len() * 10 >= best.len() * 9 { out } else { HashMap::new() }
}

/// A variable name: an identifier, as a C string.
fn read_cstr(mem: &Mem, at: u64) -> Option<String> {
    let mut b = [0u8; 256];
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
    // REDPULSE's runner (1.4.1804) grows at 0.6 yet keeps {key, RValue*, hash}: told by the
    // hash, which is the key + 1 there.
    let key_first = key_first || b.chunks(es).filter(|e| u32_at(e, es - 4) != Some(0)).all(|e| u32_at(e, 0).map(|k| k.wrapping_add(1)) == u32_at(e, es - 4));
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

/// A vote's score: instances the data file places in a room, instances, the highest id (less is better).
type Score = (usize, usize, std::cmp::Reverse<u32>);

/// Instances are of one C++ class (one vtable): the class and offset where (nearly) all its
/// objects hold an id and an object index, all different; first the one with the most pairs
/// the data file places in a room (pointers pass for ids: 72 of Forgodden's structs held one
/// with a 0 after it, more than its instances), then the most of them (a constant pair
/// passes the checks too: Zordak's renderers all had 0xffffff, 0 at +0x90), then the smallest
/// (ids count up from 100000). Voting over every owner let thousands of the runtime's own
/// objects outvote Dunkehr's few instances at its title (+0x138, not +0xbc). `owners` are
/// windows of memory from each candidate owner's start, `ID_TO` bytes long.
fn vote_instances(owners: &[Vec<u8>], w: u64, objects: u32, map_at: u64, placed: &HashSet<(u32, u32)>) -> Option<(Score, u64, u64)> {
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
    let mut best: Option<(Score, u64, u64)> = None;
    for (vt, members) in &classes {
        for at in (if w == 8 { ID_FROM_64 } else { ID_FROM_32 }..ID_TO - 8).step_by(4) {
            // Not the map's pointer itself read as an id (the word after it, 0, as an object
            // index: Forgodden's 850 structs outvoted its instances that way).
            if at + 8 > map_at && at < map_at + w {
                continue;
            }
            let mut ids: Vec<u32> = members.iter().filter(|b| valid(b, at)).filter_map(|b| u32_at(b, at as usize)).collect();
            let known = members.iter().filter(|b| u32_at(b, at as usize).zip(u32_at(b, at as usize + 4)).is_some_and(|p| placed.contains(&p))).count();
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
            let score = (known, ids.len(), std::cmp::Reverse(ids.last().copied().unwrap_or(u32::MAX)));
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
        // Compiled (YYC) GameMaker 2 games have neither: their own table of names, by key.
        let mut compiled = HashMap::new();
        let slots = refs
            .iter()
            .copied()
            .find(|&r| ptr(mem, w, r).is_some_and(|p| first.contains(&p)) && ptr(mem, w, r + w).is_some_and(|p| second.contains(&p)))
            .or_else(|| name_tables(heap, program, &names.vari_names()).filter(|(g, i)| g == i).map(|(_, i)| i.0))
            .or_else(|| {
                compiled = compiled_names(heap);
                (!compiled.is_empty()).then_some(0)
            })
            .ok_or(format!(
                "no array of variable names ({} variable maps, {} \"prototype\", {} \"@@array@@\", {} pointers to them)",
                heads.len(),
                first.len(),
                second.len(),
                refs.len()
            ))?;
        // The keys' base: the one under which the most maps have a name for every key.
        let named = |base: u32| -> (HashMap<u32, String>, HashMap<u64, ()>) {
            let mut known: HashMap<u32, String> = if base == KEY_BASE { compiled.clone() } else { HashMap::new() };
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
        // A C++ vtable: its first entry a function, in executable memory (a function's own
        // address, as GameMaker 2.3's objects keep three before their map, isn't one). Not by
        // the program's file: Wine maps a Windows program's code without it.
        let code = |a: u64| regions.iter().any(|r| r.start <= a && a < r.end && r.perms.contains('x'));
        // The table itself is never in writable memory (an object on the heap that starts with
        // its own vtable isn't one: old Linux runners keep vtables in their code segment, so
        // "points to code" alone took a field of Zordak's instances for their class).
        let writable = |a: u64| regions.iter().any(|r| r.start <= a && a < r.end && r.perms.contains('w'));
        let is_vtable = |a: u64| !writable(a) && ptr(mem, w, a).is_some_and(|f| code(f) || (in_image(a) && in_image(f)));
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
            // The map's offset differs by runner (64-bit: +0x58 before GameMaker 2.3, less
            // after), so every one is tried. A class that is a vtable wins over more
            // "instances" (junk at a wrong offset), then the most instances; offsets finding
            // the same ones tie (a field before the map, shared by them, at +0x4 on Mobility's
            // runner; its map is at +0x2c).
            let vote = (w..=MAP_AT_MAX)
                .step_by(w as usize)
                .filter_map(|at| vote_instances(&owners_at(at), w, objects, at, &names.placed).map(|(score, vt, id)| (score, at, (vt, id))))
                .inspect(|(score, at, (vt, id))| if std::env::var_os("FERRET_DEBUG").is_some() { eprintln!("gml vote: base {base} map +{at:#x}: {score:?} class {vt:#x} (vtable {}) id +{id:#x}", is_vtable(*vt)) })
                .max_by_key(|(score, at, (vt, _))| (score.0, is_vtable(*vt), *score, *at == MAP_AT_64 && w == 8, std::cmp::Reverse(*at)));
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
        Ok(Gml { pid: heap.pid, names, w, map_at, base, global_names, global_owner, slots, known, id_at, class, owners, maps: maps.len(), flat: None, arrays: OnceLock::new(), tables: OnceLock::new() })
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
        let (_, class, id_at) = vote_instances(&bytes, w, objects, 0, &names.placed).ok_or(format!("no instances among {} objects with variable arrays", windows.len()))?;
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
            arrays: OnceLock::new(),
            tables: OnceLock::new(),
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

    /// The global object: of the owners that aren't instances, one that starts with a C++
    /// vtable first (a stack frame held a copy of Spelunky SD's pointer; Zombies Days, which
    /// has no global variables, showed a table of the runner's with every variable name in it
    /// as "Global variables"), then the one holding the most of the game's global variables
    /// (by VARI), then the most variables. None when the data file names global variables and
    /// the best holds none of them, or names none at all.
    fn global(&self, mem: &Mem) -> Option<&Owner> {
        if let Some(g) = self.global_owner {
            return self.owners.iter().find(|o| o.addr == g);
        }
        let regions = crate::helper::maps(self.pid).unwrap_or_default();
        let has = |a: u64, c: char| regions.iter().any(|r| r.start <= a && a < r.end && r.perms.contains(c));
        let vtable = |o: &Owner| ptr(mem, self.w, o.addr).is_some_and(|v| !has(v, 'w') && ptr(mem, self.w, v).is_some_and(|f| has(f, 'x')));
        let held = |o: &Owner| {
            let e = entries(mem, self.w, o.map).unwrap_or_default();
            (e.iter().filter(|&&(_, k)| self.name(mem, k).is_some_and(|n| self.names.globals.contains(&n))).count(), e.len())
        };
        let best = self.owners.iter().filter(|o| o.instance.is_none()).max_by_key(|o| (vtable(o), held(o)))?;
        // Compiled games have no VARI to tell by.
        (self.names.vari.is_empty() || self.flat.is_some() || held(best).0 > 0).then_some(best)
    }

    /// An owner's numbers: (variable name, address, kind); after its own, those inside its
    /// arrays and structs ("stats.hp", "items[3]"), up to `LIST_DEEP`.
    fn numbers(&self, heap: &Heap, o: &Owner) -> Vec<(String, u64, &'static str)> {
        let mem = heap.mem();
        let vars = self.vars(mem, o.addr, o.map);
        let mut out: Vec<(String, u64, &'static str)> =
            vars.iter().filter(|&&(rv, _)| holds_number(mem, rv)).filter_map(|&(rv, key)| Some((self.name_in(mem, o.addr, key)?, rv, "gm"))).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        let mut deep: Vec<(String, u64, &'static str)> = Vec::new();
        let mut named: Vec<(String, u64)> = vars.iter().filter_map(|&(rv, key)| Some((self.name_in(mem, o.addr, key)?, rv))).collect();
        named.sort();
        for (name, rv) in named {
            if deep.len() >= LIST_DEEP {
                break;
            }
            let found = self.inside(heap, rv, &name, false, LIST_DEEP - deep.len());
            deep.extend(found.into_iter().map(|(steps, at)| (format!("{name}{}", steps_text(&steps)), at, "gm")));
        }
        out.extend(deep);
        out
    }

    /// How arrays are laid out: a vote over the arrays the global object and the instances
    /// hold, each tried every way (a pointer followed within 0x20 bytes by a count, the
    /// pointer leading to that many RValues, or to rows of them).
    fn array_at(&self, mem: &Mem) -> Option<ArrayAt> {
        *self.arrays.get_or_init(|| self.vote_arrays(mem))
    }

    /// Voted once per located game (it is located again every 2 s): with no array to learn
    /// from, voting at every array-looking value made Zordak's list take 6 s.
    fn vote_arrays(&self, mem: &Mem) -> Option<ArrayAt> {
        let w = self.w;
        let global = self.global(mem).map(|o| o.addr);
        let mut votes: HashMap<ArrayAt, usize> = HashMap::new();
        let mut samples = 0;
        // A double too small to be a game's number among the first elements: a pointer or a
        // whole number read as one, so not RValues (the 2026 Linux runner's arrays start with
        // a pointer to an object of all zeros after its vtable, their elements' comes next).
        let tiny = |q: u64, n: u32| {
            let mut e = vec![0u8; 16 * n.min(16) as usize];
            mem.read_exact_at(&mut e, q).is_err() || e.chunks(16).any(|r| u32_at(r, 12) == Some(REAL) && rvalue_number(r).is_some_and(|v| v != 0.0 && v.abs() < 1e-300))
        };
        for o in self.owners.iter().filter(|o| o.instance.is_some() || Some(o.addr) == global) {
            for (rv, _) in self.vars(mem, o.addr, o.map) {
                let Some((ARRAY, p)) = rvalue(mem, w, rv) else { continue };
                let mut b = [0u8; 0xc0];
                if samples >= 64 || mem.read_exact_at(&mut b, p).is_err() {
                    continue;
                }
                samples += 1;
                for data in (0..0xa0).step_by(w as usize) {
                    let Some(q) = ptr_in(&b, w, data as usize).filter(|&q| q > 0x1000) else { continue };
                    for len in (data + w..=data + 0x28).step_by(4) {
                        let Some(n) = u32_at(&b, len as usize).filter(|n| (1..=MAX_ARRAY).contains(n)) else { continue };
                        // Not a pointer's upper half (0x7f52 passed for the length of Snack
                        // Hoard's only array on the 2026 Linux runner).
                        if w == 8 && len % 8 == 4 && u32_at(&b, len as usize - 4).is_some_and(|low| low > 0x1_0000) {
                            continue;
                        }
                        // Two elements at least: a 1 near any pointer to an object passed for a
                        // one-element array (Boom Boom Explodie Box 3D: +0x0 and +0x18).
                        if n >= 2 && rvalues_ok(mem, q, n) && !tiny(q, n) {
                            *votes.entry(ArrayAt { rows: false, data, len }).or_default() += 1;
                        }
                        if rows_ok(mem, w, q, n) {
                            *votes.entry(ArrayAt { rows: true, data, len }).or_default() += 1;
                        }
                    }
                }
            }
        }
        // The layout most arrays agree on (at least two, or the only one seen); the count
        // nearest the pointer on a tie (a capacity follows the length in newer runners).
        let (&best, &n) = votes.iter().max_by_key(|(a, n)| (**n, std::cmp::Reverse(a.len), std::cmp::Reverse(a.data), a.rows))?;
        (n >= 2 || samples == 1).then_some(best)
    }

    /// An array's elements: (step, RValue address), from the array object an RValue points to.
    fn elements(&self, mem: &Mem, array: u64) -> Vec<(Step, u64)> {
        let Some(at) = self.array_at(mem) else { return Vec::new() };
        let (Some(q), Some(n)) = (ptr(mem, self.w, array + at.data), half(mem, array + at.len)) else { return Vec::new() };
        if n == 0 || n > MAX_ARRAY {
            return Vec::new();
        }
        if !at.rows {
            return (0..n).map(|i| (Step::Index(i), q + 16 * u64::from(i))).collect();
        }
        let mut out = Vec::new();
        for r in 0..n.min(256) {
            let Some((a, k)) = row(mem, self.w, q, r).filter(|&(_, k)| k > 0) else { continue };
            out.extend((0..k).map(|i| (if n == 1 { Step::Index(i) } else { Step::Cell(r, i) }, a + 16 * u64::from(i))));
        }
        out
    }

    /// The data structure an RValue stands for and its index in the runner's table of that
    /// kind. Newer runners (2024 and later) keep a reference: kind 15, the index in the low
    /// half and the kind of structure in the high half. Before, a plain number: told by the
    /// entry's mark when a ds_map holds it (ds_map_add_list, json_decode), else by what the
    /// game's code keeps in a variable of that name (`typed`).
    fn ds_of(&self, mem: &Mem, rv: u64, typed: Option<Ds>) -> Option<(Ds, u32)> {
        let mut b = [0u8; 16];
        mem.read_exact_at(&mut b, rv).ok()?;
        let kind = u32_at(&b, 12)?;
        if kind & 0xff_ffff == REF {
            return Some((Ds::of_ref(u32_at(&b, 4)?)?, u32_at(&b, 0)?)).filter(|x| x.1 < MAX_DS);
        }
        let ds = match (kind & HOLDS_LIST != 0, kind & HOLDS_MAP != 0) {
            (true, _) => Ds::List,
            (_, true) => Ds::Map,
            _ => typed?,
        };
        let v = rvalue_number(&b)?;
        (v >= 0.0 && v < f64::from(MAX_DS) && v.fract() == 0.0).then_some((ds, v as u32))
    }

    /// The indexes the game's variables hold now, by kind of structure (a destroyed one's
    /// index stays in its variable).
    fn ds_held(&self, mem: &Mem) -> HashMap<Ds, Vec<u32>> {
        let global = self.global(mem).map(|o| o.addr);
        let mut held: HashMap<Ds, Vec<u32>> = HashMap::new();
        for o in &self.owners {
            let named = !self.names.ds.is_empty() && (o.instance.is_some() || Some(o.addr) == global);
            for (rv, key) in self.vars(mem, o.addr, o.map) {
                let typed = if named { self.name_in(mem, o.addr, key).and_then(|n| self.names.ds.get(&n).copied()) } else { None };
                if let Some((kind, i)) = self.ds_of(mem, rv, typed) {
                    held.entry(kind).or_default().push(i);
                }
            }
        }
        for v in held.values_mut() {
            v.sort_unstable();
            v.dedup();
        }
        held
    }

    /// A ds_list (a stack and a queue look the same): {vtable, count, capacity, RValue*}, a
    /// pointer's width apart: (count, the RValues).
    fn list_at(&self, mem: &Mem, p: u64) -> Option<(u32, u64)> {
        let (n, room, items) = (half(mem, p + self.w)?, half(mem, p + 2 * self.w)?, ptr(mem, self.w, p + 3 * self.w)?);
        (n <= room && room <= MAX_ARRAY && (n == 0 || rvalues_ok(mem, items, n))).then_some((n, items))
    }

    /// A ds_map's table: the object starts with a pointer to {buckets*, mask, count}: (the
    /// buckets, mask, count).
    fn map_at(&self, mem: &Mem, p: u64) -> Option<(u64, u32, u32)> {
        let h = ptr(mem, self.w, p).filter(|&h| h > 0x1000)?;
        let (buckets, mask, count) = (ptr(mem, self.w, h).filter(|&b| b > 0x1000)?, half(mem, h + self.w)?, half(mem, h + self.w + 4)?);
        (mask >= 1 && mask < 1 << 20 && (mask + 1).is_power_of_two() && count < 1 << 20).then_some((buckets, mask, count))
    }

    /// What a ds_map holds: (key, the value's RValue). A bucket is {first, last}; a node
    /// {next, previous, hash, pair*}, the pair a key's RValue and the value's. None unless
    /// the nodes found are as many as the map counts.
    fn map_entries(&self, mem: &Mem, p: u64) -> Option<Vec<(Step, u64)>> {
        let w = self.w as usize;
        let (buckets, mask, count) = self.map_at(mem, p)?;
        let mut b = vec![0u8; (mask as usize + 1) * 2 * w];
        mem.read_exact_at(&mut b, buckets).ok()?;
        let mut todo: Vec<u64> = (0..b.len() / w).filter_map(|i| ptr_in(&b, self.w, i * w)).filter(|&n| n != 0).collect();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        while let Some(n) = todo.pop() {
            if !seen.insert(n) {
                continue;
            }
            let mut node = [0u8; 32];
            if seen.len() > count as usize || mem.read_exact_at(&mut node[..4 * w], n).is_err() {
                return None;
            }
            todo.extend([ptr_in(&node, self.w, 0)?, ptr_in(&node, self.w, w)?].into_iter().filter(|&l| l != 0));
            let pair = ptr_in(&node, self.w, 3 * w).filter(|&p| p > 0x1000)?;
            let mut kv = [0u8; 16];
            mem.read_exact_at(&mut kv, pair).ok()?;
            let step = match (u32_at(&kv, 12)? & 0xff_ffff, rvalue_number(&kv)) {
                (STRING, _) => match ptr_in(&kv, self.w, 0).and_then(|s| ptr(mem, self.w, s)).and_then(|s| key_text(mem, s)) {
                    Some(t) => Step::Key(t),
                    None => continue,
                },
                (_, Some(v)) => Step::Num(v.to_bits()),
                _ => continue,
            };
            out.push((step, pair + 16));
        }
        out.sort_by_cached_key(|(s, _)| steps_text(std::slice::from_ref(s)));
        (seen.len() == count as usize).then_some(out)
    }

    /// A ds_grid: {RValue*, width, height}, or the sizes first: (width, height, the cells,
    /// row after row).
    fn grid_at(&self, mem: &Mem, p: u64) -> Option<(u32, u32, u64)> {
        let side = |n: u32| (1..=MAX_GRID).contains(&n);
        let ok = |gw: u32, gh: u32, cells: u64| (side(gw) && side(gh) && rvalues_ok(mem, cells, gw * gh)).then_some((gw, gh, cells));
        ok(half(mem, p + self.w)?, half(mem, p + self.w + 4)?, ptr(mem, self.w, p)?).or_else(|| ok(half(mem, p)?, half(mem, p + 4)?, ptr(mem, self.w, p + 8)?))
    }

    /// Whether `p` is a structure of the kind, as a table of that kind holds.
    fn ds_ok(&self, mem: &Mem, kind: Ds, class: u64, p: u64) -> bool {
        match kind {
            Ds::Map => self.map_at(mem, p).is_some(),
            Ds::Grid => self.grid_at(mem, p).is_some(),
            _ => ptr(mem, self.w, p) == Some(class) && self.list_at(mem, p).is_some(),
        }
    }

    /// A table's first slots: None when one holds something that isn't a structure of the kind.
    fn table_slots(&self, heap: &Heap, kind: Ds, class: u64, at: u64, n: u32) -> Option<Vec<u64>> {
        let mem = heap.mem();
        (0..u64::from(n)).map(|i| ptr(mem, self.w, at + self.w * i).filter(|&p| p == 0 || (heap.is_pointer(p) && self.ds_ok(mem, kind, class, p)))).collect()
    }

    /// Whether the pointer at `r` is one of the runner's to a table: its capacity comes right
    /// before, 16 slots or a multiple (every runner seen keeps {capacity, table*}). Without
    /// this a vector of one buffer object passed for the table of Noise's Halloween Bash's
    /// lists (a class of 40 objects that start like a list), and moved at every look.
    fn sized(&self, mem: &Mem, r: u64) -> bool {
        half(mem, r - self.w).is_some_and(|n| n >= 16 && n % 16 == 0 && n <= 1 << 16)
    }

    /// For each set of objects, the runner's table of them: where the pointers to them sit
    /// side by side (gaps: destroyed ones), starting where another pointer (the runner's own)
    /// points, at or before the first of them, empty slots between: (that pointer's address,
    /// the table) for each such pointer. Two passes over memory for all sets.
    fn tables_of(&self, heap: &Heap, sets: &[Vec<u64>]) -> Vec<Vec<(u64, u64)>> {
        let mem = heap.mem();
        let which: HashMap<u64, usize> = sets.iter().enumerate().flat_map(|(i, s)| s.iter().map(move |&o| (o, i))).collect();
        let all: Vec<u64> = which.keys().copied().collect();
        let mut by: Vec<Vec<u64>> = vec![Vec::new(); sets.len()];
        for r in heap.referrers(&all) {
            if let Some(&i) = ptr(mem, self.w, r).and_then(|t| which.get(&t)) {
                by[i].push(r);
            }
        }
        let starts: Vec<Vec<u64>> = by
            .into_iter()
            .map(|mut refs| {
                refs.sort_unstable();
                let mut run: Vec<u64> = Vec::new();
                let mut longest: Vec<u64> = Vec::new();
                for r in refs {
                    if run.last().is_some_and(|&l| r - l > self.w * 64) {
                        if run.len() > longest.len() {
                            longest = std::mem::take(&mut run);
                        }
                        run.clear();
                    }
                    run.push(r);
                }
                if run.len() > longest.len() {
                    longest = run;
                }
                let Some(&first) = longest.first() else { return Vec::new() };
                (0..u64::from(MAX_DS)).filter_map(|k| first.checked_sub(k * self.w)).take_while(|&a| a == first || ptr(mem, self.w, a) == Some(0)).collect()
            })
            .collect();
        let pointed: Vec<(u64, u64)> = heap.referrers(&starts.concat()).into_iter().filter_map(|r| Some((r, ptr(mem, self.w, r)?))).collect();
        starts
            .iter()
            .map(|s| {
                let Some(&at) = s.iter().find(|&&a| pointed.iter().any(|&(_, v)| v == a)) else { return Vec::new() };
                pointed.iter().filter(|&&(r, v)| v == at && self.sized(mem, r)).map(|&(r, _)| (r, at)).collect()
            })
            .collect()
    }

    /// The runner's tables of ds_lists, ds_maps and ds_grids, for the kinds the game's
    /// variables hold now (`ds_held`). Kept per game: finding them reads all of its memory
    /// several times (10 s on Noise's Halloween Bash); a kind not found is looked for again
    /// only once the variables hold other indexes of that kind.
    fn tables(&self, heap: &Heap) -> Tables {
        *self.tables.get_or_init(|| {
            type Held = HashMap<Ds, Vec<u32>>;
            static KNOWN: Mutex<Option<(u32, Tables, Held)>> = Mutex::new(None);
            let mem = heap.mem();
            let held = self.ds_held(mem);
            let wanted = |k: Ds| held.get(&k).is_some_and(|v| !v.is_empty());
            let known = KNOWN.lock().unwrap().clone();
            if let Some((_, mut t, before)) = known.filter(|k| k.0 == self.pid) {
                // Still tables of their kind (a table that grew is read where the runner's
                // pointer leads now).
                let still = |kind: Ds, class: u64, at: Option<u64>| at.filter(|&a| ptr(mem, self.w, a).is_some_and(|t| heap.is_pointer(t) && self.table_slots(heap, kind, class, t, 8).is_some()));
                let had = (t.lists.is_some(), t.maps.is_some(), t.grids.is_some());
                t.lists = t.lists.filter(|&(a, class)| still(Ds::List, class, Some(a)).is_some());
                t.maps = still(Ds::Map, 0, t.maps);
                t.grids = still(Ds::Grid, 0, t.grids);
                let again = |k: Ds, found: bool| !found && wanted(k) && held.get(&k) != before.get(&k);
                // Looked for again too when a table found before no longer is one.
                let lost = had != (t.lists.is_some(), t.maps.is_some(), t.grids.is_some());
                if !lost && !again(Ds::List, t.lists.is_some()) && !again(Ds::Map, t.maps.is_some()) && !again(Ds::Grid, t.grids.is_some()) {
                    return t;
                }
            }
            let t = if [Ds::List, Ds::Map, Ds::Grid].into_iter().any(wanted) { self.find_tables(heap, &held) } else { Tables::default() };
            *KNOWN.lock().unwrap() = Some((self.pid, t, held));
            t
        })
    }

    /// Looks for the tables. Lists: objects of one C++ class, {vtable, count, capacity,
    /// RValue*}; stacks, queues and priority queues are classes of that shape too, each with
    /// a table of its own, and nothing in a runner without symbols tells the classes apart
    /// (their order differs between the Linux and the Windows runner): the tables are given
    /// to the kinds the way most indexes the variables hold lead to a structure; lists are
    /// shown only when one table is theirs that way. Maps and grids have no vtable: their
    /// tables are looked for next to the runner's pointers to those tables (it keeps all six
    /// side by side), then by shape through all of memory.
    fn find_tables(&self, heap: &Heap, held: &HashMap<Ds, Vec<u32>>) -> Tables {
        let mem = heap.mem();
        let w = self.w as usize;
        let debug = std::env::var_os("FERRET_DEBUG").is_some();
        let of = |k: Ds| held.get(&k).map(Vec::as_slice).unwrap_or_default();
        let mut classes: HashMap<u64, Vec<u64>> = HashMap::new();
        heap.each_chunk(4 * w, |addr, b, fresh| {
            for off in (0..fresh).step_by(w) {
                let (Some(vt), Some(count), Some(room), Some(items)) = (ptr_in(b, self.w, off), u32_at(b, off + w), u32_at(b, off + 2 * w), ptr_in(b, self.w, off + 3 * w)) else { break };
                // An empty one has no block yet: {vtable, 0, 0, null}.
                let empty = count == 0 && room == 0 && items == 0;
                if vt < 0x1_0000 || count > room || room > MAX_ARRAY || !heap.read_only(vt) || !(empty || (room != 0 && heap.is_pointer(items))) {
                    continue;
                }
                let of = classes.entry(vt).or_default();
                if of.len() < 1 << 16 {
                    of.push(addr + off as u64);
                }
            }
        });
        let regions = crate::helper::maps(self.pid).unwrap_or_default();
        let has = |a: u64, c: char| regions.iter().any(|r| r.start <= a && a < r.end && r.perms.contains(c));
        // Classes none of whose objects holds what can't be RValues.
        let classes: Vec<(u64, Vec<u64>)> = classes
            .into_iter()
            .filter(|(vt, objects)| ptr(mem, self.w, *vt).is_some_and(|f| has(f, 'x')) && objects.iter().take(256).all(|&o| self.list_at(mem, o).is_some()))
            .collect();
        let sets: Vec<Vec<u64>> = classes.iter().map(|(_, o)| o.clone()).collect();
        let found = if sets.is_empty() { Vec::new() } else { self.tables_of(heap, &sets) };
        // (class, the runner's pointer, objects) of each class with a table. Of several
        // pointers to one table, the one nearest another class's.
        let all: Vec<u64> = found.iter().flatten().map(|x| x.0).collect();
        let mut cands: Vec<(u64, u64, usize)> = Vec::new();
        for (i, tables) in found.iter().enumerate() {
            let near = |r: u64| all.iter().filter(|&&o| !tables.iter().any(|t| t.0 == o)).map(|&o| o.abs_diff(r)).min().unwrap_or(u64::MAX);
            if let Some(&(r, at)) = tables.iter().min_by_key(|t| near(t.0)) {
                if debug {
                    eprintln!("gml ds: class {:#x}: {} objects, table {at:#x}, the runner's pointer at {r:#x}", classes[i].0, classes[i].1.len());
                }
                cands.push((classes[i].0, r, classes[i].1.len()));
            }
        }
        let mut t = Tables::default();
        // Each kind's fit to each table: the indexes leading to an object of the class
        // first, then the fewest that don't (a destroyed list's index stays in its variable:
        // DsBasic's one live and one destroyed list came to nothing, counted one for one).
        let fit = |c: &(u64, u64, usize), kind: Ds| -> i64 {
            let Some(at) = ptr(mem, self.w, c.1) else { return 0 };
            of(kind).iter().map(|&i| if ptr(mem, self.w, at + self.w * u64::from(i)).is_some_and(|p| p != 0 && ptr(mem, self.w, p) == Some(c.0)) { 1024 } else { -1 }).sum()
        };
        let kinds: Vec<Ds> = [Ds::List, Ds::Stack, Ds::Queue, Ds::Priority].into_iter().filter(|&k| !of(k).is_empty()).collect();
        if kinds.first() == Some(&Ds::List) {
            // Every way of giving the kinds a table each (or none): the best total.
            let mut best: Vec<(i64, Option<usize>)> = Vec::new();
            let mut choice: Vec<Option<usize>> = vec![None; kinds.len()];
            fn each(k: usize, n: usize, choice: &mut Vec<Option<usize>>, f: &mut dyn FnMut(&[Option<usize>])) {
                if k == choice.len() {
                    return f(choice);
                }
                for c in std::iter::once(None).chain((0..n).map(Some)) {
                    if c.is_none() || !choice[..k].contains(&c) {
                        choice[k] = c;
                        each(k + 1, n, choice, f);
                    }
                }
            }
            each(0, cands.len().min(6), &mut choice, &mut |choice| {
                let total: i64 = choice.iter().zip(&kinds).map(|(c, &k)| c.map_or(0, |c| fit(&cands[c], k))).sum();
                best.push((total, choice[0]));
            });
            let top = best.iter().map(|b| b.0).max().unwrap_or(0);
            let mut lists: Vec<usize> = best.iter().filter(|b| b.0 == top).filter_map(|b| b.1).filter(|&c| fit(&cands[c], Ds::List) > 0).collect();
            lists.sort_unstable();
            lists.dedup();
            // Lists are the most of them, when the indexes can't tell.
            let most = lists.iter().map(|&c| cands[c].2).max().unwrap_or(0);
            lists.retain(|&c| cands[c].2 == most);
            if debug {
                eprintln!("gml ds: held {held:?}, lists' table: {:x?}", lists.iter().map(|&c| cands[c]).collect::<Vec<_>>());
            }
            if let [c] = lists[..] {
                t.lists = Some((cands[c].1, cands[c].0));
            }
        }
        // Maps and grids: a pointer near the classes' pointers to a table whose slots all
        // hold a map (a grid), the indexes the variables hold among them.
        let fits = |kind: Ds, at: u64| -> Option<(usize, usize)> {
            let n = of(kind).last().map_or(0, |&i| i + 1).max(16);
            let slots = self.table_slots(heap, kind, 0, at, n).or_else(|| self.table_slots(heap, kind, 0, at, 16))?;
            let live = of(kind).iter().filter(|&&i| slots.get(i as usize).is_some_and(|&p| p != 0)).count();
            (live > 0).then_some((live, slots.iter().filter(|&&p| p != 0).count()))
        };
        for kind in [Ds::Map, Ds::Grid].into_iter().filter(|&k| !of(k).is_empty()) {
            let near = cands.iter().flat_map(|c| (c.1.saturating_sub(0x100)..=c.1 + 0x100).step_by(w)).filter(|a| !cands.iter().any(|c| c.1 == *a) && self.sized(mem, *a));
            let best = near.filter_map(|a| Some((fits(kind, ptr(mem, self.w, a).filter(|&p| heap.is_pointer(p))?)?, a))).max();
            match kind {
                Ds::Map => t.maps = best.map(|b| b.1),
                _ => t.grids = best.map(|b| b.1),
            }
        }
        // By shape through all of memory: the hash tables' heads, then what points to one
        // first (a map); pointers followed by two sizes, leading to that many RValues (a grid).
        if !of(Ds::Map).is_empty() && t.maps.is_none() {
            let mut heads: Vec<u64> = Vec::new();
            heap.each_chunk(w + 8, |addr, b, fresh| {
                for off in (0..fresh).step_by(w) {
                    let (Some(buckets), Some(mask), Some(count)) = (ptr_in(b, self.w, off), u32_at(b, off + w), u32_at(b, off + w + 4)) else { break };
                    if mask >= 7 && mask < 1 << 20 && (mask + 1).is_power_of_two() && count <= mask.saturating_mul(8) && heap.is_pointer(buckets) && heads.len() < 1 << 18 {
                        heads.push(addr + off as u64);
                    }
                }
            });
            let maps: Vec<u64> = heap.referrers(&heads).into_iter().filter(|&p| self.map_at(mem, p).is_some()).take(1 << 16).collect();
            t.maps = self.tables_of(heap, &[maps]).concat().into_iter().filter_map(|(r, at)| Some((fits(Ds::Map, at)?, r))).max().map(|b| b.1);
        }
        if !of(Ds::Grid).is_empty() && t.grids.is_none() {
            let mut grids: Vec<u64> = Vec::new();
            let side = |n: Option<u32>| n.is_some_and(|n| (1..=MAX_GRID).contains(&n));
            heap.each_chunk(w + 8, |addr, b, fresh| {
                for off in (0..fresh).step_by(w) {
                    let first = ptr_in(b, self.w, off).is_some_and(|p| heap.is_pointer(p)) && side(u32_at(b, off + w)) && side(u32_at(b, off + w + 4));
                    let last = side(u32_at(b, off)) && side(u32_at(b, off + 4)) && ptr_in(b, self.w, off + 8).is_some_and(|p| heap.is_pointer(p));
                    if (first || last) && grids.len() < 1 << 18 {
                        grids.push(addr + off as u64);
                    }
                }
            });
            grids.retain(|&p| self.grid_at(mem, p).is_some());
            grids.truncate(1 << 16);
            t.grids = self.tables_of(heap, &[grids]).concat().into_iter().filter_map(|(r, at)| Some((fits(Ds::Grid, at)?, r))).max().map(|b| b.1);
        }
        if debug {
            eprintln!("gml ds: tables {t:x?}");
        }
        t
    }

    /// What a ds_list, ds_map or ds_grid holds: (step, RValue address), by its index. Nothing
    /// for a destroyed one (its slot is empty) or when the table of its kind wasn't found.
    fn ds_elements(&self, heap: &Heap, kind: Ds, index: u32) -> Vec<(Step, u64)> {
        let mem = heap.mem();
        let t = self.tables(heap);
        let slot = |at: u64| ptr(mem, self.w, ptr(mem, self.w, at).filter(|&t| heap.is_pointer(t))? + self.w * u64::from(index)).filter(|&p| heap.is_pointer(p));
        let found = match kind {
            Ds::List => t.lists.and_then(|(at, class)| {
                let p = slot(at).filter(|&p| ptr(mem, self.w, p) == Some(class))?;
                let (n, items) = self.list_at(mem, p)?;
                Some((0..n).map(|i| (Step::Index(i), items + 16 * u64::from(i))).collect())
            }),
            Ds::Map => t.maps.and_then(|at| self.map_entries(mem, slot(at)?)),
            Ds::Grid => t.grids.and_then(|at| {
                let (gw, gh, cells) = self.grid_at(mem, slot(at)?)?;
                Some((0..gw * gh).map(|k| (Step::Cell(k % gw, k / gw), cells + 16 * u64::from(k))).collect())
            }),
            _ => None,
        };
        found.unwrap_or_default()
    }

    /// What a value holds inside: an array's elements, a struct's variables; empty for
    /// anything else. `pointee` is the array or struct itself (to tell a loop).
    fn children(&self, mem: &Mem, rv: u64) -> (u64, Vec<(Step, u64)>) {
        match rvalue(mem, self.w, rv) {
            Some((ARRAY, p)) => (p, self.elements(mem, p)),
            // A struct: an object with a variable map (not an instance or the global object,
            // which a variable may also point to: they are listed on their own).
            Some((OBJECT, p)) if self.flat.is_none() && !self.owners.iter().any(|o| o.addr == p && (o.instance.is_some() || self.global_owner == Some(p))) => {
                let Some(map) = ptr(mem, self.w, p + self.map_at) else { return (p, Vec::new()) };
                let vars = entries(mem, self.w, map).unwrap_or_default();
                (p, vars.into_iter().filter_map(|(rv, key)| Some((Step::Field(self.name(mem, key)?), rv))).collect())
            }
            _ => (0, Vec::new()),
        }
    }

    /// What a value holds inside (`children`), or the data structure it stands for holds
    /// (`ds_of`, `typed`: what the code keeps in a variable of its name).
    fn kids(&self, heap: &Heap, rv: u64, typed: Option<Ds>) -> (u64, Vec<(Step, u64)>) {
        let (pointee, kids) = self.children(heap.mem(), rv);
        match self.ds_of(heap.mem(), rv, typed) {
            Some((kind, index)) if kids.is_empty() => (u64::MAX - ((kind as u64) << 32) - u64::from(index), self.ds_elements(heap, kind, index)),
            _ => (pointee, kids),
        }
    }

    /// The numbers inside a variable's arrays, structs and data structures: (steps, RValue address). `all`:
    /// every element (to find one by its address); otherwise what the list shows (`LIST_FIRST`).
    /// `name`: the variable's, which tells whether its number is a data structure's index.
    fn inside(&self, heap: &Heap, rv: u64, name: &str, all: bool, budget: usize) -> Vec<(Vec<Step>, u64)> {
        let mem = heap.mem();
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut todo: Vec<(Vec<Step>, u64, Option<Ds>)> = vec![(Vec::new(), rv, self.names.ds.get(name).copied())];
        while let Some((steps, rv, ds)) = todo.pop() {
            let (pointee, kids) = self.kids(heap, rv, ds);
            if kids.is_empty() || !seen.insert(pointee) || out.len() >= budget {
                continue;
            }
            // One read for an array's elements, which sit side by side.
            let block = match (kids.first(), kids.last()) {
                (Some((_, a)), Some((_, z))) if z - a == 16 * (kids.len() as u64 - 1) => {
                    let mut b = vec![0u8; 16 * kids.len()];
                    mem.read_exact_at(&mut b, *a).ok().map(|_| b)
                }
                _ => None,
            };
            let array = block.is_some();
            let mut shown = 0;
            for (i, (step, kid)) in kids.into_iter().enumerate() {
                let mut one = [0u8; 16];
                let b: &[u8] = match &block {
                    Some(b) => &b[16 * i..16 * i + 16],
                    None if mem.read_exact_at(&mut one, kid).is_ok() => &one,
                    None => continue,
                };
                // A struct's variable may hold a list or a grid by its index too.
                let held = match &step {
                    Step::Field(n) => self.names.ds.get(n).copied(),
                    _ => None,
                };
                let mut path = steps.clone();
                path.push(step);
                let kind = u32_at(b, 12).unwrap_or(0);
                let near = all || !array || i < LIST_FIRST;
                match (rvalue_number(b), kind & 0xff_ffff) {
                    // A number a ds_map marks as a list's or a map's index: what it holds.
                    (Some(_), _) if kind & (HOLDS_LIST | HOLDS_MAP) != 0 => {
                        if path.len() < MAX_DEPTH && near {
                            todo.push((path, kid, None));
                        }
                    }
                    (None, REF) if path.len() < MAX_DEPTH && near => todo.push((path, kid, None)),
                    (Some(v), _) => {
                        if held.is_some() && path.len() < MAX_DEPTH {
                            todo.push((path.clone(), kid, held));
                        }
                        if all || !array || i < LIST_FIRST || (v != 0.0 && shown < LIST_ARRAY) {
                            shown += 1;
                            out.push((path, kid));
                        }
                    }
                    (None, ARRAY | OBJECT) if path.len() < MAX_DEPTH && near => todo.push((path, kid, None)),
                    _ => {}
                }
            }
        }
        out.truncate(budget);
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

/// An RValue's kind and its value as a pointer (arrays, structs).
fn rvalue(mem: &Mem, w: u64, rv: u64) -> Option<(u32, u64)> {
    let mut b = [0u8; 16];
    mem.read_exact_at(&mut b, rv).ok()?;
    Some((u32_at(&b, 12)? & 0xff_ffff, ptr_in(&b, w, 0).filter(|&p| p > 0x1000)?))
}

/// Whether `n` RValues are at `at`: the first ones' and the last one's kinds are kinds.
fn rvalues_ok(mem: &Mem, at: u64, n: u32) -> bool {
    let kind_ok = |at: u64| half(mem, at + 12).is_some_and(|k| k & 0xff_ffff <= 15 || k & 0xff_ffff == UNSET);
    at > 0x1000 && at % 4 == 0 && (0..n.min(16)).all(|i| kind_ok(at + 16 * u64::from(i))) && kind_ok(at + 16 * u64::from(n - 1))
}

/// Row `r` of an older array's rows ({length, RValue*} each): (the RValues, how many); a row
/// never written to is empty (`a[5, 0] = 1` makes rows 0 to 4 that way).
fn row(mem: &Mem, w: u64, rows: u64, r: u32) -> Option<(u64, u32)> {
    let at = rows + 2 * w * u64::from(r);
    match half(mem, at)? {
        0 => Some((0, 0)),
        n if n <= MAX_ARRAY => Some((ptr(mem, w, at + w).filter(|&a| a > 0x1000)?, n)),
        _ => None,
    }
}

/// Whether `n` rows are at `rows`: every one a row (the first 4096 looked at), one at least
/// holding RValues. Looking at row 0 only took the top half of a pointer (0x7fff) for Oak
/// Defenders' row count.
fn rows_ok(mem: &Mem, w: u64, rows: u64, n: u32) -> bool {
    let mut any = false;
    for r in 0..n.min(4096) {
        match row(mem, w, rows, r) {
            Some((_, 0)) => {}
            Some((a, k)) if rvalues_ok(mem, a, k) => any = true,
            _ => return false,
        }
    }
    any
}

/// Steps as a path's text: ".name", "[3]", "[1,2]", a ds_map's keys "[\"gold\"]" (a quote
/// or a backslash in one after a backslash) and "[=42]".
fn steps_text(steps: &[Step]) -> String {
    steps
        .iter()
        .map(|s| match s {
            Step::Field(n) => format!(".{n}"),
            Step::Index(i) => format!("[{i}]"),
            Step::Cell(r, c) => format!("[{r},{c}]"),
            Step::Key(k) => format!("[\"{}\"]", k.replace('\\', "\\\\").replace('"', "\\\"")),
            Step::Num(n) => format!("[={}]", f64::from_bits(*n)),
        })
        .collect()
}

/// A ds_map's key as text: up to 64 bytes, nothing a path can't carry.
fn key_text(mem: &Mem, at: u64) -> Option<String> {
    let mut b = [0u8; 65];
    let n = mem.read_at(&mut b, at).ok()?;
    let end = b[..n].iter().position(|&c| c == 0)?;
    let t = std::str::from_utf8(&b[..end]).ok()?;
    (!t.is_empty() && !t.chars().any(char::is_control)).then(|| t.to_owned())
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
    /// Into the variable's arrays and structs: "gml:global.party[0].hp".
    steps: Vec<Step>,
}

impl GmlPath {
    pub fn parse(text: &str) -> Option<GmlPath> {
        let (object, rest) = text.strip_prefix("gml:")?.split_once('.')?;
        let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        let end = rest.find(['.', '[']).unwrap_or(rest.len());
        let (var, mut rest) = rest.split_at(end);
        if !ok(object) || !ok(var) {
            return None;
        }
        let mut steps = Vec::new();
        while !rest.is_empty() {
            if let Some(r) = rest.strip_prefix('.') {
                let end = r.find(['.', '[']).unwrap_or(r.len());
                steps.push(Step::Field(r[..end].to_owned()));
                if !ok(&r[..end]) {
                    return None;
                }
                rest = &r[end..];
            } else if let Some(r) = rest.strip_prefix("[\"") {
                // A key: up to the quote no backslash stands before.
                let mut key = String::new();
                let mut chars = r.char_indices();
                let end = loop {
                    match chars.next()? {
                        (_, '\\') => key.push(chars.next()?.1),
                        (i, '"') => break i,
                        (_, c) => key.push(c),
                    }
                };
                steps.push(Step::Key(key));
                rest = r[end + 1..].strip_prefix(']')?;
            } else {
                let (inner, after) = rest.strip_prefix('[')?.split_once(']')?;
                steps.push(match (inner.strip_prefix('='), inner.split_once(',')) {
                    (Some(n), _) => Step::Num(n.parse::<f64>().ok()?.to_bits()),
                    (_, Some((r, c))) => Step::Cell(r.parse().ok()?, c.parse().ok()?),
                    _ => Step::Index(inner.parse().ok()?),
                });
                rest = after;
            }
        }
        Some(GmlPath { object: (object != "global").then(|| object.to_owned()), var: var.to_owned(), steps })
    }

    /// Whether the game may move the value: one inside an array, a list, a map or a grid,
    /// which get a new block when they grow (a variable's own RValue stays where it is).
    pub fn moves(&self) -> bool {
        !self.steps.is_empty()
    }

    pub fn text(&self) -> String {
        format!("gml:{}.{}{}", self.object.as_deref().unwrap_or("global"), self.var, steps_text(&self.steps))
    }

    pub fn describe(&self) -> String {
        match &self.object {
            Some(o) => format!("{}{} of every {o}", self.var, steps_text(&self.steps)),
            None => format!("global.{}{}", self.var, steps_text(&self.steps)),
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
            let mut rv = g.vars(mem, r, map).into_iter().find(|&(_, k)| k == key)?.0;
            let mut typed = g.names.ds.get(&path.var).copied();
            for step in &path.steps {
                rv = g.kids(heap, rv, typed).1.into_iter().find(|(s, _)| s == step)?.1;
                typed = match step {
                    Step::Field(n) => g.names.ds.get(n).copied(),
                    _ => None,
                };
            }
            holds_number(mem, rv).then_some(rv)
        })
        .collect()
}

/// The path to a number some instance's or the global object's variable holds. `picked`:
/// the name it was listed by, when the player saves it from the list: two variables may
/// lead to one list (a destroyed list's index stays in its variable and the next list made
/// takes it: Snack Hoard's `inventory[0]` was saved as `old_list[0]`, and lost when the
/// game made the inventory anew).
pub fn discover(heap: &Heap, program: Option<&Path>, target: u64, picked: Option<&str>) -> Option<(GmlPath, Vec<u64>)> {
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
        let path = GmlPath { object, var, steps: Vec::new() };
        let leads = walk(heap, program, &find_roots(heap, program, &path), &path);
        if leads.contains(&rv) {
            return Some((path, leads));
        }
    }
    // Inside a variable's arrays and structs.
    if !holds_number(mem, target) {
        return None;
    }
    let mut first = None;
    for o in &g.owners {
        if o.instance.is_none() && Some(o.addr) != global {
            continue;
        }
        for (rv, key) in g.vars(mem, o.addr, o.map) {
            let Some(var) = g.name_in(mem, o.addr, key) else { continue };
            let Some((steps, _)) = g.inside(heap, rv, &var, true, usize::MAX).into_iter().find(|(_, at)| *at == target) else { continue };
            let path = GmlPath { object: o.instance.map(|(_, obj)| g.object_name(obj)), var, steps };
            let leads = walk(heap, program, &find_roots(heap, program, &path), &path);
            if !leads.contains(&target) {
                continue;
            }
            match picked {
                Some(label) if format!("{}{}", path.var, steps_text(&path.steps)) != label => first = first.or(Some((path, leads))),
                _ => return Some((path, leads)),
            }
        }
    }
    first
}

/// The Browse tab's list: the global variables, then every instance's numbers (the player's
/// objects by name in the main list).
pub fn player(heap: &Heap, program: Option<&Path>) -> Option<Vec<ListedObject>> {
    let g = shared(heap, program, true)?;
    let mem = heap.mem();
    let mut out = Vec::new();
    if let Some(o) = g.global(mem) {
        let fields = g.numbers(heap, o).into_iter().map(|(label, addr, kind)| Listed { label, addr, kind }).collect();
        out.push(ListedObject { name: "@global".into(), class: "global".into(), role: "game", owner: None, more: false, fields });
    }
    let mut instances: Vec<&Owner> = g.owners.iter().filter(|o| o.instance.is_some()).collect();
    instances.sort_by_key(|o| o.instance);
    let mut listed = 0;
    for o in instances {
        let (id, obj) = o.instance.unwrap_or_default();
        let class = g.object_name(obj);
        let fields: Vec<Listed> = g.numbers(heap, o).into_iter().map(|(label, addr, kind)| Listed { label, addr, kind }).collect();
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
    for &(id, obj) in names.first_placed.iter().take(4) {
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
    if seen == 0 && !names.first_placed.is_empty() {
        lines.push(format!("none of the first rooms' {} instances found by id", names.first_placed.len()));
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
        let nums = g.numbers(heap, o);
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
    lines.push(format!("arrays: {:x?}", g.array_at(mem)));
    let mut held: Vec<(Ds, Vec<u32>)> = g.ds_held(mem).into_iter().collect();
    held.sort_by_key(|h| h.0 as u8);
    lines.push(format!("data structures: {} variables typed by the code, held now {held:?}, tables {:x?}", g.names.ds.len(), g.tables(heap)));
    // Which variables hold them (the first of each kind).
    let mut shown: HashMap<Ds, usize> = HashMap::new();
    for o in &g.owners {
        for (rv, key) in g.vars(mem, o.addr, o.map) {
            let name = g.name_in(mem, o.addr, key).unwrap_or_default();
            if let Some((kind, i)) = g.ds_of(mem, rv, g.names.ds.get(&name).copied()) {
                let n = shown.entry(kind).or_default();
                *n += 1;
                if *n <= 6 {
                    lines.push(format!("  {kind:?} {i}: {name} of {}, {} inside", o.instance.map_or("a struct or global".into(), |(_, obj)| g.object_name(obj)), g.ds_elements(heap, kind, i).len()));
                }
            }
        }
    }
    // What the first few arrays (kind 2) and structs (kind 6) point to: their layout.
    let global = g.global(mem).map(|o| o.addr);
    for (kind, label) in [(ARRAY, "array"), (OBJECT, "struct")] {
        let mut shown = 0;
        for o in g.owners.iter().filter(|o| o.instance.is_some() || Some(o.addr) == global) {
            for (rv, key) in g.vars(mem, o.addr, o.map) {
                let mut b = [0u8; 16];
                if shown >= 6 || mem.read_exact_at(&mut b, rv).is_err() || u32_at(&b, 12).unwrap_or(0) & 0xff_ffff != kind {
                    continue;
                }
                let Some(p) = ptr_in(&b, g.w, 0) else { continue };
                let mut d = vec![0u8; 0xc0];
                let _ = mem.read_at(&mut d, p);
                let words: Vec<String> = d.chunks(4).map(|c| format!("{:x}", u32_at(c, 0).unwrap_or(0))).collect();
                lines.push(format!("  {label} {} at 0x{p:x}: {}", g.name_in(mem, o.addr, key).unwrap_or_default(), words.join(" ")));
                shown += 1;
            }
        }
    }
    if let Some(f) = g.flat {
        lines.push(format!("GameMaker Studio 1.4 arrays: {} instance variables, {} global ones; instances' class 0x{:x}", f.instance_n, f.global_n, g.class));
    }
    if let Some(o) = g.global(mem) {
        lines.push(format!("global object 0x{:x}: {} numbers", o.addr, g.numbers(heap, o).len()));
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
        for text in ["gml:global.party[0].hp", "gml:obj_player.stats.hp", "gml:global.grid[2,5]", "gml:o.a[1][2].b.c[3]"] {
            assert_eq!(GmlPath::parse(text).unwrap().text(), text);
        }
        assert_eq!(GmlPath::parse("gml:global.a[1,2].b").unwrap().steps, vec![Step::Cell(1, 2), Step::Field("b".into())]);
        for text in ["gml:global.stats[\"gold\"]", "gml:global.save[\"party\"][2]", "gml:obj_a.m[=42]", "gml:global.m[=-1.5].x", r#"gml:global.m["a \"b\" ]\\."][0]"#] {
            assert_eq!(GmlPath::parse(text).unwrap().text(), text, "{text}");
        }
        assert_eq!(GmlPath::parse(r#"gml:global.m["a \"b\" ]\\."]"#).unwrap().steps, vec![Step::Key(r#"a "b" ]\."#.into())]);
        assert_eq!(GmlPath::parse("gml:global.m[=42]").unwrap().steps, vec![Step::Num(42f64.to_bits())]);
        assert!(GmlPath::parse("gml:global.a[x]").is_none());
        assert!(GmlPath::parse("gml:global.a[1").is_none());
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
