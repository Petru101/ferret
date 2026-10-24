//! RPG Maker MV games: their JavaScript objects as NW.js's V8 keeps them, read from outside.
//! Every MV game keeps its state in the same globals (`$gameParty`, `$gameActors`,
//! `$gameVariables`...), so a value is named by its path from one of them:
//! `js:$gameParty._gold`, `js:$gameActors._data[1]._hp`, `js:$gameParty._items[7]`.
//! 64-bit V8 without pointer compression: NW.js 0.12 (V8 4.1, RPG Maker MV up to 1.5) and
//! 0.25-0.29 (V8 6.x, MV 1.6); 32-bit V8 6.x; and V8 8.x with pointer compression (RPG Maker MZ's
//! NW.js 0.44-0.48, Chromium 80-85: 4-byte references into a 4 GiB cage).
//!
//! V8 basics used here: a tagged word with bit 0 set points to an object (address + 1),
//! with the low 32 bits clear it is a small integer (Smi) kept in the upper half (so a Smi
//! field is a plain i32 at slot + 4); every object starts with its map, and a map's map is
//! the meta map, which is its own map. A map lists the object's properties in a descriptor
//! array {map, length, count, enum cache, (name, details, value)...}; a property's details
//! say where its field is (an index into the object's own slots, then its properties array)
//! and how it's kept (a Smi, a double, anything).

use crate::names::Heap;
use crate::ue::{Listed, ListedObject};
use crate::mem::Mem;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The globals listed and saved paths start from (all of them are looked for in one pass).
const GLOBALS: &[&str] = &[
    "$gameParty",
    "$gameActors",
    "$gameVariables",
    "$gameSwitches",
    "$gamePlayer",
    "$gameSystem",
    "$gameTimer",
    "$gameMap",
    "$dataSystem",
    "$dataItems",
    "$dataWeapons",
    "$dataArmors",
];

/// Elements read from one array or object at most.
const MAX_ELEMENTS: usize = 5000;
/// Characters read from one string at most.
const MAX_TEXT: u32 = 256;
/// The hole in a double array (a missing element).
const DOUBLE_HOLE: u64 = 0xfff7_ffff_fff7_ffff;

/// Where V8 keeps a global, by the engine's age.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Cells {
    /// V8 6.x: a property cell holds its own name: {map, details, name, value}.
    Named,
    /// V8 4.x: an entry of the global object's dictionary {name, cell, details}, the cell
    /// {map, value, ...}.
    InDictionary,
    /// V8 8.x with pointer compression: {map, name, details, value}. The names are external
    /// strings whose text is Blink's (`Js::ext`).
    Compressed,
}

/// A value as a field or element holds it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Val {
    /// A small integer: the address of its i32 (the upper half of the slot).
    Int(u64),
    /// A double kept in the object or in a box of the field's own: the address of the f64.
    Real(u64),
    /// true or false: the address of the slot pointing to it (`Kind::JsBool`).
    Bool(u64),
    /// Any other object (tagged): a JavaScript object, a string, null, a hole, or a number in a
    /// box other fields may share (not written).
    Obj(u64),
    /// Not a value (unreadable, or not what V8 keeps there).
    Other,
}

/// How one game's V8 lays its objects out, and where its globals are.
pub struct Js {
    l: Layout,
    cells: Cells,
    /// The meta map (tagged).
    meta: u64,
    cell_map: u64,
    /// Where a field's index starts in a property's details (19 in V8 4.x, 20 in 6.x: a bit
    /// for constness came in between); its representation sits 14 bits lower.
    field_shift: u32,
    /// The cells of each of `GLOBALS` (several when stale copies are left around).
    globals: Vec<(&'static str, Vec<u64>)>,
    /// V8 8.x: where an external string's resource keeps its Blink string, from the resource
    /// (Blink's string: {refs, length, hash, characters}).
    ext: i64,
    /// V8 8.x and newer: the word of a cell holding its name, its details and value after it:
    /// 1 in 8.x {map, name, details, value}, 2 in 14.x {map, dependent code, name, details,
    /// value}.
    cell_name: u64,
    /// V8 14.x (NW.js 0.10x, Chromium 146): elements kinds in bits 2-7 of bit_field2 (3-7
    /// before), the dictionary kind 13 (12 in 8.x: a shared-array kind came between), and a
    /// property's details {kind, constness, attributes (3), location, ...} (location bit 5;
    /// bit 1 in 8.x). Each native context has its own meta map then (`is_map`).
    modern: bool,
    /// V8's false and true (tagged), once seen (`boolean`); 0 until then.
    bools: Mutex<[u64; 2]>,
}

/// The attached game's false and true as its slots hold them (4 bytes in 32-bit V8 and with
/// pointer compression, else 8), for reading and writing `Kind::JsBool` places without the
/// layout: (slot width, false, true). Set once `Js::boolean` has seen both.
static BOOLS: Mutex<Option<(usize, u64, u64)>> = Mutex::new(None);

/// Whether the attached game's true and false are known (`learn_bools`).
pub fn bools_known() -> bool {
    BOOLS.lock().unwrap().is_some()
}

/// Learns the game's true and false from the fields of its globals (Game_Party's first field,
/// `_inBattle`, is one of them).
pub fn learn_bools(heap: &Heap) {
    let Some(js) = shared(heap) else { return };
    for g in GLOBALS {
        if let Some(o) = js.global(heap.mem(), g) {
            js.fields(heap.mem(), o);
        }
        if bools_known() {
            return;
        }
    }
}

/// How wide a slot holding true or false is in the attached game.
pub fn bool_width() -> usize {
    BOOLS.lock().unwrap().map_or(8, |b| b.0)
}

/// The true or false a slot's bytes hold (None: something else, or not seen yet).
pub fn bool_value(b: &[u8]) -> Option<bool> {
    let (w, f, t) = (*BOOLS.lock().unwrap())?;
    let mut x = [0u8; 8];
    x[..w].copy_from_slice(b.get(..w)?);
    match u64::from_le_bytes(x) {
        v if v == t => Some(true),
        v if v == f => Some(false),
        _ => None,
    }
}

/// Points the slot at `addr` to true or false; only while it holds one of them (a field the
/// game turned into something else is left alone).
pub fn write_bool(mem: &Mem, addr: u64, on: bool) -> std::io::Result<()> {
    let Some((w, f, t)) = *BOOLS.lock().unwrap() else { return Err(std::io::Error::other("this game's true and false aren't known yet")) };
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b[..w], addr)?;
    if bool_value(&b[..w]).is_none() {
        return Err(std::io::Error::other("not true or false now"));
    }
    mem.write_all_at(&(if on { t } else { f }).to_le_bytes()[..w], addr)
}

fn word(mem: &Mem, at: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b, at).ok().map(|_| u64::from_le_bytes(b))
}

fn half(mem: &Mem, at: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    mem.read_exact_at(&mut b, at).ok().map(|_| u32::from_le_bytes(b))
}

fn byte(mem: &Mem, at: u64) -> Option<u8> {
    let mut b = [0u8; 1];
    mem.read_exact_at(&mut b, at).ok().map(|_| b[0])
}

fn tagged(w: u64) -> bool {
    w & 1 == 1 && w > 0x10000 && w < 1 << 47
}

/// Where V8 keeps things, 64-bit or 32-bit (NW.js on Windows often is: Lilac and Her Light).
/// 32-bit V8 has 4-byte words, keeps a small integer as the number times two (bit 0 clear)
/// and never keeps a double inside an object (always in a box of the field's own). V8 8.x with
/// pointer compression is laid out the same way (`base`: references are 4-byte offsets into
/// its cage), with a few places moved.
#[derive(Clone, Copy)]
struct Layout {
    w: u64,
    /// The pointer cage's start (compressed references), else 0.
    base: u64,
}

impl Layout {
    /// The 64-bit offset or the 32-bit one.
    fn at(self, o64: u64, o32: u64) -> u64 {
        if self.w == 8 { o64 } else { o32 }
    }

    fn word(self, mem: &Mem, at: u64) -> Option<u64> {
        match self.w {
            8 => word(mem, at),
            _ => half(mem, at).map(|x| self.decompress(x)),
        }
    }

    /// A 4-byte word as V8 means it: a reference (bit 0 set) is an offset into the cage.
    fn decompress(self, x: u32) -> u64 {
        match x & 1 {
            1 => self.base | u64::from(x),
            _ => u64::from(x),
        }
    }

    fn compressed(self) -> bool {
        self.base != 0
    }

    fn is_smi(self, x: u64) -> bool {
        match self.w {
            8 => x & 0xffff_ffff == 0,
            _ => x & 1 == 0,
        }
    }

    fn smi(self, x: u64) -> Option<i32> {
        match self.w {
            8 => self.is_smi(x).then_some((x >> 32) as u32 as i32),
            _ => self.is_smi(x).then_some(x as u32 as i32 >> 1),
        }
    }

    /// A Smi slot as a number's place: the i32 in its upper half (64-bit), or the slot itself
    /// (32-bit, read as `smi`).
    fn int_at(self, slot: u64) -> u64 {
        slot + self.at(4, 0)
    }

    // Strings: {map, hash, length (a Smi), characters}; cons {.., first, second}, sliced
    // {.., parent, offset}, thin {.., actual}.
    fn str_len(self) -> u64 {
        self.at(16, 8)
    }
    fn str_body(self) -> u64 {
        self.at(24, 12)
    }
    // Maps: sizes (instance size in words, in-object count or start), instance type, bit
    // fields, then pointers (prototype, constructor, transitions, descriptors, layout).
    fn map_words(self) -> u64 {
        self.at(8, 4)
    }
    fn map_type(self) -> u64 {
        self.at(12, 8)
    }
    fn map_bit_field2(self) -> u64 {
        self.at(15, 11)
    }
    fn map_bit_field3(self) -> u64 {
        self.at(16, 12)
    }
    fn map_descriptors(self) -> u64 {
        match self.compressed() {
            true => 24,
            false => self.at(48, 28),
        }
    }
    // Objects: {map, properties, elements, in-object fields...}; arrays {map, length, data}.
    fn obj_props(self) -> u64 {
        self.w
    }
    fn obj_elements(self) -> u64 {
        2 * self.w
    }
    fn array_data(self) -> u64 {
        2 * self.w
    }
    /// A HeapNumber's double.
    fn number_value(self) -> u64 {
        self.at(8, 4)
    }
}

/// The layout of the game attached to: its pointer width.
fn layout_of(heap: &Heap) -> Layout {
    Layout { w: heap.width() as u64, base: 0 }
}

impl Js {
    /// Finds the globals' names (V8 strings: {map, hash, length as a Smi, characters}), what
    /// points to them (their cells) and the layout, in two passes over memory.
    fn locate(heap: &Heap) -> Option<Js> {
        let l = layout_of(heap);
        let mem = heap.mem();
        let texts: Vec<&[u8]> = GLOBALS.iter().map(|g| g.as_bytes()).collect();
        let longest = texts.iter().map(|t| t.len()).max().unwrap_or(0);
        let mut names: Vec<Vec<u64>> = vec![Vec::new(); texts.len()];
        let (len_at, body) = (l.str_len() as usize, l.str_body() as usize);
        heap.each_chunk(body + longest, |addr, b, fresh| {
            for off in (0..fresh).step_by(l.w as usize) {
                if b.get(off + body) != Some(&b'$') {
                    continue;
                }
                let Some(lw) = b.get(off + len_at..off + len_at + l.w as usize) else { continue };
                let mut x = [0u8; 8];
                x[..lw.len()].copy_from_slice(lw);
                let Some(len) = l.smi(u64::from_le_bytes(x)) else { continue };
                for (i, t) in texts.iter().enumerate() {
                    if t.len() as i32 == len && b.get(off + body..off + body + t.len()) == Some(t) {
                        names[i].push(addr + off as u64 + 1);
                    }
                }
            }
        });
        // A string's map's map is the meta map, which is its own map.
        let meta = names.iter().flatten().find_map(|&s| {
            let m = l.word(mem, s - 1).filter(|&m| tagged(m))?;
            let meta = l.word(mem, m - 1).filter(|&x| tagged(x))?;
            (l.word(mem, meta - 1) == Some(meta)).then_some(meta)
        })?;
        let all: Vec<u64> = names.iter().flatten().copied().collect();
        let refs = heap.referrers(&all);
        let is_map = |m: u64| tagged(m) && l.word(mem, m - 1) == Some(meta);
        // Each way of keeping globals, by the cell map it finds: the one finding the most
        // globals wins.
        let mut found: Vec<((Cells, u64), Vec<(usize, u64)>)> = Vec::new();
        let mut note = |key: (Cells, u64), g: usize, cell: u64| match found.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push((g, cell)),
            None => found.push((key, vec![(g, cell)])),
        };
        let w = l.w;
        for &r in &refs {
            let Some(name) = l.word(mem, r) else { continue };
            let Some(g) = names.iter().position(|v| v.contains(&name)) else { continue };
            if let (Some(m), Some(d)) = (l.word(mem, r - 2 * w), l.word(mem, r - w)) {
                if is_map(m) && l.is_smi(d) {
                    note((Cells::Named, m), g, r - 2 * w);
                }
            }
            if let (Some(c), Some(d)) = (l.word(mem, r + w), l.word(mem, r + 2 * w)) {
                if tagged(c) && l.is_smi(d) && l.word(mem, c - 1).is_some_and(is_map) {
                    note((Cells::InDictionary, l.word(mem, c - 1)?), g, c - 1);
                }
            }
        }
        let ((cells, cell_map), got) = found.into_iter().max_by_key(|(_, v)| {
            let mut gs: Vec<usize> = v.iter().map(|(g, _)| *g).collect();
            gs.sort_unstable();
            gs.dedup();
            gs.len()
        })?;
        let mut globals: Vec<(&'static str, Vec<u64>)> = GLOBALS.iter().map(|g| (*g, Vec::new())).collect();
        for (g, cell) in got {
            if !globals[g].1.contains(&cell) {
                globals[g].1.push(cell);
            }
        }
        let field_shift = if cells == Cells::Named { 20 } else { 19 };
        let mut js = Js { l, cells, meta, cell_map, field_shift, globals, ext: 0, cell_name: 1, modern: false, bools: Mutex::default() };
        js.calibrate(mem);
        Some(js)
    }

    /// V8 8.x with pointer compression (64-bit only): the globals' names are external strings
    /// made from Blink's own strings, so they're found from those, in four passes over memory:
    /// Blink's strings with the globals' texts ({refs, length, hash, characters}; V8's own
    /// {map, hash, length, characters} are taken too), the resources pointing to them, V8's
    /// external strings pointing to those ({map, hash, length, resource}), and the cells
    /// holding those names ({map, name, details, value}).
    fn locate_compressed(heap: &Heap) -> Option<Js> {
        if heap.width() != 8 {
            return None;
        }
        let mem = heap.mem();
        let u32_at = |b: &[u8], i: usize| b.get(i..i + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]));
        let u64_at = |b: &[u8], i: usize| b.get(i..i + 8).map(|x| u64::from_le_bytes(x.try_into().unwrap()));
        let texts: Vec<&[u8]> = GLOBALS.iter().map(|g| g.as_bytes()).collect();
        let longest = texts.iter().map(|t| t.len()).max().unwrap_or(0);
        // (global, address) of Blink's strings and of V8's own.
        let (mut blink, mut names) = (Vec::new(), Vec::new());
        heap.each_chunk(12 + longest, |addr, b, fresh| {
            for off in (0..fresh).step_by(4) {
                if b.get(off + 12) != Some(&b'$') {
                    continue;
                }
                for (g, t) in texts.iter().enumerate() {
                    if b.get(off + 12..off + 12 + t.len()) != Some(t) {
                        continue;
                    }
                    let len = Some(t.len() as u32);
                    if u32_at(b, off + 4) == len {
                        blink.push((g, addr + off as u64));
                    }
                    if u32_at(b, off + 8) == len && u32_at(b, off).is_some_and(|m| m & 1 == 1) {
                        names.push((g, addr + off as u64));
                    }
                }
            }
        });
        // Where Blink's strings are held (in resources), by address.
        let held: Vec<u64> = blink.iter().map(|&(_, a)| a).collect();
        let mut fields: Vec<(u64, usize)> =
            heap.referrers(&held).into_iter().filter_map(|f| Some((f, blink.iter().find(|&&(_, a)| Some(a) == word(mem, f))?.0))).collect();
        fields.sort_unstable();
        // V8's external strings whose resource is near one of those fields: {map, hash, length,
        // resource}, the resource within `NEAR` of the field.
        const NEAR: u64 = 64;
        let mut ext: Vec<(usize, u64, i64)> = Vec::new();
        if let (Some(&(lo, _)), Some(&(hi, _))) = (fields.first(), fields.last()) {
            heap.each_chunk(0, |addr, b, fresh| {
                for off in (12..fresh.saturating_sub(8)).step_by(4) {
                    let Some(v) = u64_at(b, off) else { break };
                    if v + NEAR < lo || v > hi + NEAR {
                        continue;
                    }
                    let from = fields.partition_point(|&(f, _)| f + NEAR < v);
                    for &(f, g) in fields[from..].iter().take_while(|&&(f, _)| f <= v + NEAR) {
                        if u32_at(b, off - 4) == Some(texts[g].len() as u32) && u32_at(b, off - 12).is_some_and(|m| m & 1 == 1) {
                            ext.push((g, addr + off as u64 - 12, f as i64 - v as i64));
                        }
                    }
                }
            });
        }
        names.extend(ext.iter().map(|&(g, s, _)| (g, s)));
        // A string's map's map is the meta map, its own map; the cage is the 4 GiB holding it.
        let cage = |a: u64| a & !0xffff_ffff;
        let meta_of = |s: u64| {
            let l = Layout { w: 4, base: cage(s) };
            let m = l.word(mem, s).filter(|&m| tagged(m))?;
            let meta = l.word(mem, m - 1).filter(|&x| tagged(x))?;
            (l.word(mem, meta - 1) == Some(meta)).then_some(meta)
        };
        names.retain(|&(_, s)| meta_of(s).is_some());
        let mut targets: Vec<(u32, u64, usize, u64)> = names.iter().filter_map(|&(g, s)| Some(((s + 1) as u32, cage(s), g, meta_of(s)?))).collect();
        targets.sort_unstable();
        targets.dedup();
        let cages: Vec<u64> = targets.iter().map(|t| t.1).collect();
        // The cells: {map, name, details (a Smi), value}, or {map, dependent code, name,
        // details, value} (V8 14.x); each way found (its cell map and where the name is), with
        // the globals it holds.
        let mut found: Vec<((u64, u64), Vec<(usize, u64)>)> = Vec::new();
        heap.each_chunk(8, |addr, b, fresh| {
            if !cages.contains(&cage(addr)) {
                return;
            }
            for off in (4..fresh).step_by(4) {
                let Some(v) = u32_at(b, off) else { break };
                if v & 1 == 0 {
                    continue;
                }
                let from = targets.partition_point(|t| t.0 < v);
                for &(_, base, g, meta) in targets[from..].iter().take_while(|t| t.0 == v) {
                    if base != cage(addr) || u32_at(b, off + 4).is_none_or(|d| d & 1 == 1) {
                        continue;
                    }
                    let l = Layout { w: 4, base };
                    for k in [1u64, 2] {
                        let Some(at) = off.checked_sub(4 * k as usize) else { continue };
                        let Some(m) = u32_at(b, at).filter(|m| m & 1 == 1).map(|m| l.decompress(m)) else { continue };
                        // A map: its map is the meta map.
                        if l.word(mem, m - 1) != Some(meta) {
                            continue;
                        }
                        let cell = addr + at as u64;
                        match found.iter_mut().find(|(key, _)| *key == (m, k)) {
                            Some((_, v)) => v.push((g, cell)),
                            None => found.push(((m, k), vec![(g, cell)])),
                        }
                    }
                }
            }
        });
        let ((cell_map, cell_name), got) = found.into_iter().max_by_key(|(_, v)| {
            let mut gs: Vec<usize> = v.iter().map(|(g, _)| *g).collect();
            gs.sort_unstable();
            gs.dedup();
            gs.len()
        })?;
        let base = cage(cell_map);
        let l = Layout { w: 4, base };
        let meta = l.word(mem, cell_map - 1)?;
        // Where the resources keep Blink's string: the place most of the names agree on.
        let mut places: Vec<i64> = ext.iter().filter(|e| cage(e.1) == base).map(|e| e.2).collect();
        places.sort_unstable();
        let ext = places.chunk_by(|a, b| a == b).max_by_key(|c| c.len()).map_or(0, |c| c[0]);
        let mut globals: Vec<(&'static str, Vec<u64>)> = GLOBALS.iter().map(|g| (*g, Vec::new())).collect();
        for (g, cell) in got {
            if !globals[g].1.contains(&cell) {
                globals[g].1.push(cell);
            }
        }
        let mut js = Js { l, cells: Cells::Compressed, meta, cell_map, field_shift: 19, globals, ext, cell_name, modern: false, bools: Mutex::default() };
        js.calibrate(mem);
        Some(js)
    }

    /// Checks the field index's place on an object every MV game has: Game_Party's first
    /// fields are `_inBattle`, `_gold`, `_steps`, set in that order.
    fn calibrate(&mut self, mem: &Mem) {
        // A plain object's elements are holey (kind 3): where that kind sits tells V8 14.x
        // from 8.x (looked at with V8 14.x's maps allowed).
        if self.cells == Cells::Compressed {
            self.modern = true;
            let b2 = self.global(mem, "$gameParty").and_then(|p| self.map_of(mem, p)).and_then(|m| byte(mem, m - 1 + self.l.map_bit_field2()));
            self.modern = b2.is_some_and(|b2| b2 >> 3 != 3 && (b2 >> 2) & 0x3f == 3);
        }
        let Some(party) = self.global(mem, "$gameParty") else { return };
        let Some(m) = self.map_of(mem, party) else { return };
        let descs: Vec<(Option<String>, u32)> = self.descriptors(mem, m).into_iter().map(|(k, d)| (self.text(mem, k), d)).collect();
        let order = ["_inBattle", "_gold", "_steps"];
        let fits = |s: u32| {
            order.iter().enumerate().all(|(i, name)| descs.iter().any(|(k, d)| k.as_deref() == Some(name) && (d >> s) & 0x3ff == i as u32))
        };
        if let Some(s) = (16..24).find(|&s| fits(s)) {
            self.field_shift = s;
        }
    }

    fn word(&self, mem: &Mem, at: u64) -> Option<u64> {
        self.l.word(mem, at)
    }

    /// A small integer's value, from its place (`Val::Int`).
    pub fn int(&self, mem: &Mem, at: u64) -> Option<u32> {
        match self.l.w {
            8 => half(mem, at),
            _ => self.l.smi(u64::from(half(mem, at)?)).map(|v| v as u32),
        }
    }

    /// The value type of a small integer's place: a plain i32, or 32-bit V8's doubled one.
    pub fn int_kind(&self) -> &'static str {
        if self.l.w == 8 { "i32" } else { "smi" }
    }

    /// A map: its map is the meta map (V8 14.x: or its native context's own, a map of itself).
    fn is_map(&self, mem: &Mem, m: u64) -> bool {
        if !tagged(m) {
            return false;
        }
        match self.word(mem, m - 1) {
            Some(mm) if mm == self.meta => true,
            Some(mm) => self.modern && tagged(mm) && self.word(mem, mm - 1) == Some(mm),
            None => false,
        }
    }

    /// The elements kind kept in a map's bit_field2.
    fn elements_kind(&self, mem: &Mem, m: u64) -> Option<u8> {
        byte(mem, m - 1 + self.l.map_bit_field2()).map(|b| if self.modern { b >> 2 & 0x3f } else { b >> 3 })
    }

    fn map_of(&self, mem: &Mem, o: u64) -> Option<u64> {
        if !tagged(o) {
            return None;
        }
        self.word(mem, o - 1).filter(|&m| self.is_map(mem, m))
    }

    fn instance_type(&self, mem: &Mem, m: u64) -> Option<u16> {
        let at = m - 1 + self.l.map_type();
        match self.cells {
            Cells::Named | Cells::Compressed => half(mem, at).map(|t| t as u16),
            Cells::InDictionary => byte(mem, at).map(u16::from),
        }
    }

    /// The value of a global (an object), from the cells found for it: a cell still holding its
    /// name and an object.
    fn global(&self, mem: &Mem, name: &str) -> Option<u64> {
        let (_, cells) = self.globals.iter().find(|(g, _)| *g == name)?;
        cells.iter().find_map(|&c| self.cell_value(mem, c, name))
    }

    /// The object a global's cell holds, if it's still the cell of that global.
    fn cell_value(&self, mem: &Mem, cell: u64, name: &str) -> Option<u64> {
        if self.word(mem, cell)? != self.cell_map {
            return None;
        }
        let w = self.l.w;
        let v = match self.cells {
            Cells::Named => {
                let n = self.word(mem, cell + 2 * w)?;
                if self.text(mem, n).as_deref() != Some(name) {
                    return None;
                }
                self.word(mem, cell + 3 * w)?
            }
            Cells::InDictionary => self.word(mem, cell + w)?,
            Cells::Compressed => {
                let n = self.word(mem, cell + self.cell_name * w)?;
                if self.text(mem, n).as_deref() != Some(name) {
                    return None;
                }
                self.word(mem, cell + (self.cell_name + 2) * w)?
            }
        };
        self.map_of(mem, v).map(|_| v)
    }

    /// A map's own properties: each name (a string, tagged) and its details. The descriptor
    /// array: {map, length, count, enum cache, (name, details, value)...}.
    fn descriptors(&self, mem: &Mem, m: u64) -> Vec<(u64, u32)> {
        let l = self.l;
        let Some(bits) = half(mem, m - 1 + l.map_bit_field3()) else { return Vec::new() };
        let own = (bits >> 10) & 0x3ff;
        let Some(d) = self.word(mem, m - 1 + l.map_descriptors()).filter(|&d| tagged(d)) else { return Vec::new() };
        let ws = l.w as usize;
        let mut b = vec![0u8; 3 * ws * own as usize];
        // Entries after {map, counts, marked count, enum cache} (V8 14.x: a word more).
        let first = if self.modern { 20 } else { 4 * l.w };
        if mem.read_exact_at(&mut b, d - 1 + first).is_err() {
            return Vec::new();
        }
        let w = |i: usize| {
            let mut x = [0u8; 8];
            x[..ws].copy_from_slice(&b[i..i + ws]);
            u64::from_le_bytes(x)
        };
        let mut out = Vec::with_capacity(own as usize);
        for i in 0..own as usize {
            let (k, det) = (w(3 * ws * i), w(3 * ws * i + ws));
            let k = if ws == 4 { l.decompress(k as u32) } else { k };
            let Some(det) = l.smi(det).filter(|_| tagged(k)) else { return Vec::new() };
            out.push((k, det as u32));
        }
        out
    }

    /// A string's text: sequential (one or two bytes a character), a concatenation, a slice of
    /// another, or a thin string pointing to its internalized copy.
    fn text(&self, mem: &Mem, s: u64) -> Option<String> {
        self.text_in(mem, s, 0)
    }

    fn text_in(&self, mem: &Mem, s: u64, depth: u32) -> Option<String> {
        if depth > 16 {
            return None;
        }
        let l = self.l;
        let m = self.map_of(mem, s)?;
        let t = self.instance_type(mem, m)?;
        // Strings' types are the lowest: below 0x80 (0x40 in V8 8.x).
        if t >= if l.compressed() { 0x40 } else { 0x80 } {
            return None;
        }
        // The length: a Smi, a plain int in V8 8.x.
        let len = match l.compressed() {
            true => half(mem, s - 1 + l.str_len())?,
            false => l.smi(self.word(mem, s - 1 + l.str_len())?)? as u32,
        };
        if len > MAX_TEXT {
            return None;
        }
        // V8 4.x: representation in bits 0-1, one byte a character = bit 2; 6.x: bits 0-2
        // (thin strings came), bit 3.
        let (one_byte, rep) = match self.cells {
            Cells::InDictionary => (t & 0x4 != 0, t & 0x3),
            Cells::Named | Cells::Compressed => (t & 0x8 != 0, t & 0x7),
        };
        let (o, body) = (s - 1, l.str_body());
        let chars = |at: u64| {
            let mut b = vec![0u8; if one_byte { len } else { 2 * len } as usize];
            mem.read_exact_at(&mut b, at).ok()?;
            Some(match one_byte {
                true => b.iter().map(|&c| c as char).collect(),
                false => String::from_utf16_lossy(&b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()),
            })
        };
        match rep {
            0 => chars(o + body),
            // External (V8 8.x: Blink's names): {.., resource}; the resource holds Blink's
            // string {refs, length, hash, characters}.
            2 if self.cells == Cells::Compressed => {
                let resource = word(mem, o + body)?;
                chars(word(mem, resource.checked_add_signed(self.ext)?)? + 12)
            }
            1 => {
                let a = self.text_in(mem, self.word(mem, o + body)?, depth + 1)?;
                Some(a + &self.text_in(mem, self.word(mem, o + body + l.w)?, depth + 1)?)
            }
            3 => {
                let parent = self.text_in(mem, self.word(mem, o + body)?, depth + 1)?;
                let from = l.smi(self.word(mem, o + body + l.w)?)? as usize;
                Some(parent.chars().skip(from).take(len as usize).collect())
            }
            5 => self.text_in(mem, self.word(mem, o + body)?, depth + 1),
            _ => None,
        }
    }

    /// What a slot holds; `double`: the field is kept as a double in a box of its own.
    fn slot(&self, mem: &Mem, at: u64, double: bool) -> Val {
        let l = self.l;
        let Some(w) = self.word(mem, at) else { return Val::Other };
        if l.is_smi(w) {
            return Val::Int(l.int_at(at));
        }
        if !tagged(w) {
            return Val::Other;
        }
        if double {
            return Val::Real(w - 1 + l.number_value());
        }
        match self.map_of(mem, w) {
            Some(_) if self.boolean(mem, w).is_some() => Val::Bool(at),
            Some(_) => Val::Obj(w),
            None => Val::Other,
        }
    }

    /// Whether the object `o` (tagged) is V8's true or false: an oddball whose own fields hold
    /// the strings "true" or "false" and "boolean" (its to_string and type_of: {map, number,
    /// to_string, to_number, type_of, kind} in every version read). Once one is seen, only
    /// objects of its map are looked at, and the other is looked for next to it (V8 makes the
    /// two one after the other).
    fn boolean(&self, mem: &Mem, o: u64) -> Option<bool> {
        let known = *self.bools.lock().unwrap();
        if o == known[0] || o == known[1] {
            return Some(o == known[1]);
        }
        if known[0] != 0 && known[1] != 0 {
            return None;
        }
        let seen = known.iter().copied().find(|&k| k != 0);
        let m = self.map_of(mem, o)?;
        if seen.is_some_and(|k| self.word(mem, k - 1) != Some(m)) {
            return None;
        }
        let on = self.oddball_bool(mem, o)?;
        let mut bools = self.bools.lock().unwrap();
        bools[on as usize] = o;
        if bools[!on as usize] == 0 {
            let w = self.l.w;
            let other = (1..=64u64).flat_map(|i| [o.wrapping_sub(i * w), o + i * w]).find(|&p| {
                self.word(mem, p - 1) == Some(m) && self.oddball_bool(mem, p) == Some(!on)
            });
            bools[!on as usize] = other.unwrap_or(0);
        }
        if bools[0] != 0 && bools[1] != 0 {
            let mask = if self.l.w == 8 { u64::MAX } else { 0xffff_ffff };
            *BOOLS.lock().unwrap() = Some((self.l.w as usize, bools[0] & mask, bools[1] & mask));
        }
        Some(on)
    }

    /// true or false by an oddball's strings (`boolean`); None for other objects.
    fn oddball_bool(&self, mem: &Mem, o: u64) -> Option<bool> {
        let texts: Vec<String> = (1..7u64)
            .filter_map(|i| self.word(mem, o - 1 + i * self.l.w).filter(|&x| tagged(x)))
            .filter_map(|x| self.text(mem, x))
            .collect();
        if !texts.iter().any(|t| t == "boolean") {
            return None;
        }
        match () {
            _ if texts.iter().any(|t| t == "true") => Some(true),
            _ if texts.iter().any(|t| t == "false") => Some(false),
            _ => None,
        }
    }

    /// An object's own properties by name, with what each holds (fast and dictionary-mode
    /// objects).
    pub fn fields(&self, mem: &Mem, o: u64) -> Vec<(String, Val)> {
        let l = self.l;
        let Some(m) = self.map_of(mem, o) else { return Vec::new() };
        let Some(bits) = half(mem, m - 1 + l.map_bit_field3()) else { return Vec::new() };
        let props = self.word(mem, o - 1 + l.obj_props()).unwrap_or(0);
        // A dictionary-mode object: bit 20 of bit_field3 (21 in V8 8.x and later).
        if bits >> if self.cells == Cells::Compressed { 21 } else { 20 } & 1 == 1 {
            return self.dictionary(mem, props, 5).into_iter().filter_map(|(k, v)| Some((self.text(mem, k)?, v))).collect();
        }
        let (Some(words), Some(b9)) = (byte(mem, m - 1 + l.map_words()), byte(mem, m - 1 + l.map_words() + 1)) else { return Vec::new() };
        let (words, b9) = (u64::from(words), u64::from(b9));
        // V8 4.x keeps the count of in-object fields, 6.x where they start.
        let inside = match self.cells {
            Cells::InDictionary => b9,
            Cells::Named | Cells::Compressed => words.saturating_sub(b9),
        };
        let start = (words.saturating_sub(inside)) * l.w;
        // 64-bit: doubles kept in the object itself (no box), a bit each in the layout
        // descriptor (a Smi; a pointer for objects of more fields, not read: those doubles are
        // left out). 32-bit V8 boxes every double field.
        let layout = if l.w == 8 { word(mem, m - 1 + 56).unwrap_or(0) } else { 0 };
        let mut out = Vec::new();
        for (k, det) in self.descriptors(mem, m) {
            // Constants and accessors aren't fields.
            let not_field = match self.modern {
                true => det & 1 != 0 || det >> 5 & 1 != 0,
                false => det & 3 != 0,
            };
            if not_field {
                continue;
            }
            let index = u64::from((det >> self.field_shift) & 0x3ff);
            // Representations: ..., Smi 5, Integer32 6, Double 7, HeapObject 8, Tagged 9; in
            // V8 8.x 3 bits 13 lower: None 0, Smi 1, Double 2, HeapObject 3, Tagged 4.
            let double = match self.cells {
                Cells::Compressed => (det >> (self.field_shift - 13)) & 0x7 == 2,
                _ => (det >> (self.field_shift - 14)) & 0xf == 7,
            };
            let val = if index < inside {
                let at = o - 1 + start + index * l.w;
                let raw = match (l.w, l.smi(layout)) {
                    (8, Some(bits)) => index < 32 && (bits as u32) >> index & 1 == 1,
                    (8, None) => double,
                    _ => false,
                };
                match (double, raw) {
                    (true, true) if l.is_smi(layout) => Val::Real(at),
                    (true, true) => Val::Other,
                    _ => self.slot(mem, at, double),
                }
            } else {
                if !tagged(props) {
                    continue;
                }
                self.slot(mem, props - 1 + l.array_data() + (index - inside) * l.w, double)
            };
            if let Some(name) = self.text(mem, k) {
                out.push((name, val));
            }
        }
        out
    }

    pub fn field(&self, mem: &Mem, o: u64, name: &str) -> Option<Val> {
        self.fields(mem, o).into_iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    fn field_obj(&self, mem: &Mem, o: u64, name: &str) -> Option<u64> {
        match self.field(mem, o, name)? {
            Val::Obj(p) => Some(p),
            _ => None,
        }
    }

    /// A hash table's entries {key, value, details} from slot `first` on (a dictionary of
    /// names: 5, of numbers: 4): each key with what the value slot holds.
    fn dictionary(&self, mem: &Mem, table: u64, first: u64) -> Vec<(u64, Val)> {
        let l = self.l;
        if !tagged(table) {
            return Vec::new();
        }
        let Some(len) = self.word(mem, table - 1 + l.w).and_then(|x| l.smi(x)) else { return Vec::new() };
        let entries = ((len.max(0) as u64).saturating_sub(first) / 3).min(4 * MAX_ELEMENTS as u64);
        (0..entries)
            .filter_map(|i| {
                let at = table - 1 + l.array_data() + (first + 3 * i) * l.w;
                let key = self.word(mem, at)?;
                l.is_smi(self.word(mem, at + 2 * l.w)?).then_some((key, self.slot(mem, at + l.w, false)))
            })
            .collect()
    }

    /// An array's or object's indexed elements: (index, what it holds), holes left out.
    pub fn elements(&self, mem: &Mem, o: u64) -> Vec<(u32, Val)> {
        let l = self.l;
        let Some(m) = self.map_of(mem, o) else { return Vec::new() };
        let Some(kind) = self.elements_kind(mem, m) else { return Vec::new() };
        let Some(e) = self.word(mem, o - 1 + l.obj_elements()).filter(|&e| tagged(e)) else { return Vec::new() };
        let Some(n) = self.word(mem, e - 1 + l.w).and_then(|x| l.smi(x)) else { return Vec::new() };
        let n = (n.max(0) as usize).min(MAX_ELEMENTS);
        let data = e - 1 + l.array_data();
        // Kinds: Smis or anything, packed or with holes (0-3), doubles (4-5), then a
        // dictionary of numbers (6; V8 8.x has non-extensible, sealed and frozen kinds of the
        // first sort before it: 6-11, the dictionary 12).
        let dictionary = match (self.cells, self.modern) {
            (_, true) => 13,
            (Cells::Compressed, false) => 12,
            _ => 6,
        };
        match kind {
            4 | 5 => (0..n)
                .filter_map(|i| {
                    let at = data + 8 * i as u64;
                    (word(mem, at)? != DOUBLE_HOLE).then_some((i as u32, Val::Real(at)))
                })
                .collect(),
            k if k < dictionary => (0..n).map(|i| (i as u32, self.slot(mem, data + l.w * i as u64, false))).filter(|(_, v)| *v != Val::Other).collect(),
            k if k == dictionary => self.dictionary(mem, e, 4).into_iter().filter_map(|(k, v)| Some((l.smi(k)?.try_into().ok()?, v))).filter(|(_, v)| *v != Val::Other).collect(),
            _ => Vec::new(),
        }
    }

    fn element(&self, mem: &Mem, o: u64, i: u32) -> Option<Val> {
        self.elements(mem, o).into_iter().find(|(k, _)| *k == i).map(|(_, v)| v)
    }

    /// The texts of an array of strings (missing ones empty).
    fn texts(&self, mem: &Mem, o: Option<u64>) -> Vec<String> {
        let Some(o) = o else { return Vec::new() };
        let mut out = Vec::new();
        for (i, v) in self.elements(mem, o) {
            let i = i as usize;
            if out.len() <= i {
                out.resize(i + 1, String::new());
            }
            if let Val::Obj(s) = v {
                out[i] = self.text(mem, s).unwrap_or_default();
            }
        }
        out
    }
}

/// jsdump: the layout found and a global's map and fields (diagnostics).
pub fn diagnose(heap: &Heap, global: &str) -> Vec<String> {
    let Some(js) = shared(heap) else { return vec!["no RPG Maker MV globals found".into()] };
    let mem = heap.mem();
    let l = js.l;
    let mut out = vec![match l.compressed() {
        true => format!(
            "compressed (cage {:#x}{}), field shift {}, Blink's strings at resource {:+}, cells' names at word {}",
            l.base,
            if js.modern { ", V8 14.x" } else { "" },
            js.field_shift,
            js.ext,
            js.cell_name
        ),
        false => format!("{}-bit, cells {:?}, field shift {}", l.w * 8, js.cells, js.field_shift),
    }];
    for (g, cells) in &js.globals {
        let at: Vec<String> = cells.iter().map(|c| format!("{c:#x}")).collect();
        out.push(format!("{g}: cells {}, value {:?}", at.join(" "), js.global(mem, g).map(|v| format!("{v:#x}"))));
    }
    let name = if global.is_empty() { "$gameParty" } else { global };
    let Some(o) = js.global(mem, name) else { return out };
    let Some(m) = js.map_of(mem, o) else { return out };
    let raw = |at: u64, n: usize| -> String {
        let mut b = vec![0u8; n];
        match mem.read_exact_at(&mut b, at) {
            Ok(()) => b.chunks(4).map(|c| format!("{:08x}", u32::from_le_bytes(c.try_into().unwrap()))).collect::<Vec<_>>().join(" "),
            Err(_) => "unreadable".into(),
        }
    };
    out.push(format!("{name} at {o:#x}, map {m:#x}: {}", raw(m - 1, 48)));
    out.push(format!("object: {}", raw(o - 1, 64)));
    for (k, det) in js.descriptors(mem, m) {
        out.push(format!("  {:?} details {det:#x} index {} rep {}", js.text(mem, k), (det >> js.field_shift) & 0x3ff, (det >> (js.field_shift - 14)) & 0xf));
    }
    for (k, v) in js.fields(mem, o) {
        out.push(format!("  field {k}: {v:?}"));
    }
    out
}

/// The value type of the game's whole numbers (`Js::int_kind`), once its globals are found.
pub fn int_kind(heap: &Heap) -> Option<&'static str> {
    shared(heap).map(|js| js.int_kind())
}

/// The located layout of the game attached to, per pid; a failed search is tried again after
/// `RETRY`.
pub fn shared(heap: &Heap) -> Option<Arc<Js>> {
    const RETRY: Duration = Duration::from_secs(10);
    static FOUND: Mutex<Option<(u32, Option<Arc<Js>>, Instant)>> = Mutex::new(None);
    let mut found = FOUND.lock().unwrap();
    if let Some((pid, js, at)) = found.as_ref() {
        // Still the game's globals (a cell can move when V8 compacts memory).
        let valid = js.as_ref().is_some_and(|js| js.global(heap.mem(), "$gameParty").is_some() || js.global(heap.mem(), "$dataSystem").is_some());
        if *pid == heap.pid && (valid || js.is_none() && at.elapsed() < RETRY) {
            return js.clone();
        }
    }
    let js = Js::locate(heap).or_else(|| Js::locate_compressed(heap)).map(Arc::new);
    // Another game's true and false.
    if found.as_ref().is_none_or(|f| f.0 != heap.pid) {
        *BOOLS.lock().unwrap() = None;
    }
    *found = Some((heap.pid, js.clone(), Instant::now()));
    js
}

/// Whether the game is an RPG Maker MV or MZ game: its program sits next to
/// `www/js/rpg_core.js` (MV) or `js/rmmz_core.js` (MZ).
pub fn is_rpgmaker(pid: u32, program: Option<&std::path::Path>) -> bool {
    let Some(dir) = program.and_then(|p| p.parent()) else { return false };
    let inside = crate::os::inside(pid, dir);
    ["www/js/rpg_core.js", "js/rmmz_core.js"].iter().any(|f| inside.join(f).exists() || dir.join(f).exists())
}

/// One step of a path: a property by name, or an element by index (an array's, or an object
/// keyed by numbers: items by id).
#[derive(Clone, Debug, PartialEq)]
enum Step {
    Prop(String),
    Index(u32),
}

/// A saved path from a global to a number.
#[derive(Clone, Debug, PartialEq)]
pub struct JsPath {
    global: String,
    steps: Vec<Step>,
}

fn encode(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '$' => c.to_string(),
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

impl JsPath {
    pub fn parse(text: &str) -> Option<JsPath> {
        let rest = text.strip_prefix("js:")?;
        let mut parts = rest.split('.');
        let first = parts.next()?;
        let (global, first_idx) = match first.split_once('[') {
            Some((g, idx)) => (g, Some(idx)),
            None => (first, None),
        };
        let mut steps = Vec::new();
        let indices = |s: &str, steps: &mut Vec<Step>| -> Option<()> {
            for i in s.split('[') {
                steps.push(Step::Index(i.strip_suffix(']')?.parse().ok()?));
            }
            Some(())
        };
        if let Some(idx) = first_idx {
            indices(idx, &mut steps)?;
        }
        for p in parts {
            let (name, idx) = match p.split_once('[') {
                Some((n, i)) => (n, Some(i)),
                None => (p, None),
            };
            if name.is_empty() {
                return None;
            }
            steps.push(Step::Prop(decode(name)?));
            if let Some(i) = idx {
                indices(i, &mut steps)?;
            }
        }
        (global.starts_with('$') && !steps.is_empty()).then(|| JsPath { global: global.to_owned(), steps })
    }

    pub fn text(&self) -> String {
        let mut t = format!("js:{}", self.global);
        for s in &self.steps {
            match s {
                Step::Prop(n) => t += &format!(".{}", encode(n)),
                Step::Index(i) => t += &format!("[{i}]"),
            }
        }
        t
    }

    pub fn describe(&self) -> String {
        format!("{} of RPG Maker's {}", self.text().trim_start_matches("js:"), self.global)
    }

    fn prop(mut self, name: &str) -> JsPath {
        self.steps.push(Step::Prop(name.to_owned()));
        self
    }

    fn index(mut self, i: u32) -> JsPath {
        self.steps.push(Step::Index(i));
        self
    }
}

fn root(global: &str) -> JsPath {
    JsPath { global: global.to_owned(), steps: Vec::new() }
}

/// The cells of the path's global.
pub fn find_roots(heap: &Heap, path: &JsPath) -> Vec<u64> {
    let Some(js) = shared(heap) else { return Vec::new() };
    js.globals.iter().find(|(g, _)| *g == path.global).map(|(_, c)| c.clone()).unwrap_or_default()
}

/// Where the path leads from its global's cells: the number's address (an i32 or an f64).
pub fn walk(heap: &Heap, roots: &[u64], path: &JsPath) -> Vec<u64> {
    let Some(js) = shared(heap) else { return Vec::new() };
    let mem = heap.mem();
    let mut out: Vec<u64> = roots.iter().filter_map(|&c| follow(&js, mem, js.cell_value(mem, c, &path.global)?, &path.steps)).collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn follow(js: &Js, mem: &Mem, mut o: u64, steps: &[Step]) -> Option<u64> {
    for (i, s) in steps.iter().enumerate() {
        let v = match s {
            Step::Prop(name) => js.field(mem, o, name)?,
            Step::Index(n) => js.element(mem, o, *n)?,
        };
        let last = i + 1 == steps.len();
        match v {
            Val::Obj(p) if !last => o = p,
            Val::Int(a) | Val::Real(a) | Val::Bool(a) if last => return Some(a),
            _ => return None,
        }
    }
    None
}

/// A listed number and the path naming it.
struct Entry {
    label: String,
    val: Val,
    path: JsPath,
}

/// A group of listed numbers (the Browse tab shows each as one object).
struct Group {
    name: String,
    class: &'static str,
    role: &'static str,
    more: bool,
    entries: Vec<Entry>,
}

impl Group {
    fn new(name: impl Into<String>, class: &'static str, role: &'static str, more: bool) -> Group {
        Group { name: name.into(), class, role, more, entries: Vec::new() }
    }

    fn add(&mut self, label: impl Into<String>, val: Option<Val>, path: JsPath) {
        if let Some(val @ (Val::Int(_) | Val::Real(_) | Val::Bool(_))) = val {
            self.entries.push(Entry { label: label.into(), val, path });
        }
    }

    /// Every number and true/false among an object's own fields, by the field's name.
    fn add_all(&mut self, js: &Js, mem: &Mem, o: u64, path: &JsPath) {
        for (name, val) in js.fields(mem, o) {
            self.add(name.clone(), Some(val), path.clone().prop(&name));
        }
    }
}

/// The text the game gives a term (`$dataSystem.terms.<kind>[i]`), else `fallback`.
fn term(terms: &[String], i: usize, fallback: &str) -> String {
    terms.get(i).filter(|t| !t.trim().is_empty()).cloned().unwrap_or_else(|| fallback.to_owned())
}

/// The game's numbers, the party's first: gold and items, each hero's HP, MP, TP, level,
/// experience and stat bonuses, the game's variables by the names its maker gave them, the
/// player's move speed; then (`more`) every number of those objects by field name. Names
/// starting with "@" are words Ferret translates (core.rs). None before the game has made its
/// objects (the title screen of some games, a loading screen).
fn groups(heap: &Heap) -> Option<Vec<Group>> {
    let js = shared(heap)?;
    let mem = heap.mem();
    let party = js.global(mem, "$gameParty")?;
    let system = js.global(mem, "$dataSystem");
    let terms = system.and_then(|s| js.field_obj(mem, s, "terms"));
    let list = |name: &str| js.texts(mem, terms.and_then(|t| js.field_obj(mem, t, name)));
    let (basic, params, commands) = (list("basic"), list("params"), list("commands"));
    let mut out = Vec::new();

    let mut g = Group::new("@party", "Game_Party", "party", false);
    let p = root("$gameParty");
    g.add("@gold", js.field(mem, party, "_gold"), p.clone().prop("_gold"));
    g.add("@steps", js.field(mem, party, "_steps"), p.clone().prop("_steps"));
    out.push(g);
    for (field, data, word, fallback) in
        [("_items", "$dataItems", 4, "@items"), ("_weapons", "$dataWeapons", 12, "@weapons"), ("_armors", "$dataArmors", 13, "@armors")]
    {
        let Some(held) = js.field_obj(mem, party, field) else { continue };
        let named = js.global(mem, data);
        let mut g = Group::new(term(&commands, word, fallback), "Game_Party", "party", false);
        for (id, val) in js.elements(mem, held) {
            let name = named
                .and_then(|d| match js.element(mem, d, id)? {
                    Val::Obj(item) => js.text(mem, js.field_obj(mem, item, "name")?),
                    _ => None,
                })
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| format!("#{id}"));
            g.add(name, Some(val), p.clone().prop(field).index(id));
        }
        out.push(g);
    }

    let members: Vec<u32> = js
        .field_obj(mem, party, "_actors")
        .map(|a| js.elements(mem, a).into_iter().filter_map(|(_, v)| match v {
            Val::Int(at) => js.int(mem, at),
            _ => None,
        }).collect())
        .unwrap_or_default();
    let actors = js.global(mem, "$gameActors").and_then(|a| js.field_obj(mem, a, "_data"));
    let mut raw = Vec::new();
    // The party's heroes first, in their order, then the others.
    let mut heroes: Vec<(u32, u64)> = actors
        .map(|a| js.elements(mem, a).into_iter().filter_map(|(i, v)| match v {
            Val::Obj(o) => Some((i, o)),
            _ => None,
        }).collect())
        .unwrap_or_default();
    heroes.sort_by_key(|(i, _)| members.iter().position(|m| m == i).unwrap_or(usize::MAX));
    for (i, actor) in heroes {
        let path = root("$gameActors").prop("_data").index(i);
        let name = js.field_obj(mem, actor, "_name").and_then(|s| js.text(mem, s)).filter(|n| !n.trim().is_empty()).unwrap_or_else(|| format!("#{i}"));
        let mut g = Group::new(name.clone(), "Game_Actor", "character", !members.contains(&i));
        g.add(term(&basic, 2, "HP"), js.field(mem, actor, "_hp"), path.clone().prop("_hp"));
        g.add(term(&basic, 4, "MP"), js.field(mem, actor, "_mp"), path.clone().prop("_mp"));
        g.add(term(&basic, 6, "TP"), js.field(mem, actor, "_tp"), path.clone().prop("_tp"));
        g.add(term(&basic, 0, "Level"), js.field(mem, actor, "_level"), path.clone().prop("_level"));
        if let (Some(exp), Some(Val::Int(class))) = (js.field_obj(mem, actor, "_exp"), js.field(mem, actor, "_classId")) {
            if let Some(class) = js.int(mem, class) {
                g.add(term(&basic, 8, "EXP"), js.element(mem, exp, class), path.clone().prop("_exp").index(class));
            }
        }
        if let Some(plus) = js.field_obj(mem, actor, "_paramPlus") {
            for (k, val) in js.elements(mem, plus) {
                g.add(format!("{} +", term(&params, k as usize, &format!("#{k}"))), Some(val), path.clone().prop("_paramPlus").index(k));
            }
        }
        out.push(g);
        let mut all = Group::new(format!("{name} ({})", path.text().trim_start_matches("js:")), "Game_Actor", "game", true);
        all.add_all(&js, mem, actor, &path);
        raw.push(all);
    }

    if let Some(data) = js.global(mem, "$gameVariables").and_then(|v| js.field_obj(mem, v, "_data")) {
        let names = js.texts(mem, system.and_then(|s| js.field_obj(mem, s, "variables")));
        let mut g = Group::new("@variables", "Game_Variables", "game", false);
        for (i, val) in js.elements(mem, data) {
            let label = names.get(i as usize).filter(|n| !n.trim().is_empty()).cloned().unwrap_or_else(|| format!("#{i}"));
            g.add(label, Some(val), root("$gameVariables").prop("_data").index(i));
        }
        out.push(g);
    }
    // The maker's switches, on or off (story steps, doors, quests), by the names they gave
    // them; ones the game never set (holes) aren't listed: writing there could break the array.
    if let Some(data) = js.global(mem, "$gameSwitches").and_then(|v| js.field_obj(mem, v, "_data")) {
        let names = js.texts(mem, system.and_then(|s| js.field_obj(mem, s, "switches")));
        let mut g = Group::new("@switches", "Game_Switches", "game", false);
        for (i, val) in js.elements(mem, data) {
            if !matches!(val, Val::Bool(_)) {
                continue;
            }
            let label = names.get(i as usize).filter(|n| !n.trim().is_empty()).cloned().unwrap_or_else(|| format!("#{i}"));
            g.add(label, Some(val), root("$gameSwitches").prop("_data").index(i));
        }
        out.push(g);
    }
    if let Some(player) = js.global(mem, "$gamePlayer") {
        let mut g = Group::new("@player", "Game_Player", "character", false);
        g.add("@speed", js.field(mem, player, "_moveSpeed"), root("$gamePlayer").prop("_moveSpeed"));
        out.push(g);
    }
    if let Some(timer) = js.global(mem, "$gameTimer") {
        let mut g = Group::new("@timer", "Game_Timer", "game", true);
        g.add("@frames", js.field(mem, timer, "_frames"), root("$gameTimer").prop("_frames"));
        out.push(g);
    }
    for (global, class) in [("$gameParty", "Game_Party"), ("$gamePlayer", "Game_Player"), ("$gameSystem", "Game_System"), ("$gameMap", "Game_Map")] {
        if let Some(o) = js.global(mem, global) {
            let mut g = Group::new(global, class, "game", true);
            g.add_all(&js, mem, o, &root(global));
            raw.push(g);
        }
    }
    out.extend(raw);
    out.retain(|g| !g.entries.is_empty());
    Some(out)
}

/// The Browse tab's list for an RPG Maker MV game (helper `player`); None before the game has
/// made its objects.
pub fn player(heap: &Heap) -> Option<Vec<ListedObject>> {
    let groups = groups(heap)?;
    let int_kind = shared(heap)?.int_kind();
    Some(
        groups
            .into_iter()
            .map(|g| ListedObject {
                name: g.name,
                class: g.class.to_owned(),
                role: g.role,
                owner: None,
                more: g.more,
                fields: g
                    .entries
                    .into_iter()
                    .map(|e| match e.val {
                        Val::Real(addr) => Listed { label: e.label, addr, kind: "f64" },
                        Val::Bool(addr) => Listed { label: e.label, addr, kind: "jsbool" },
                        Val::Int(addr) | Val::Obj(addr) => Listed { label: e.label, addr, kind: int_kind },
                        Val::Other => Listed { label: e.label, addr: 0, kind: int_kind },
                    })
                    .collect(),
            })
            .collect(),
    )
}

/// The path to a number the list has (the curated entries before the raw ones), checked by
/// following it the way a restart does.
pub fn discover(heap: &Heap, target: u64) -> Option<(JsPath, Vec<u64>)> {
    let groups = groups(heap)?;
    let entry = groups.into_iter().flat_map(|g| g.entries).find(|e| matches!(e.val, Val::Int(a) | Val::Real(a) | Val::Bool(a) if a == target))?;
    let leads = walk(heap, &find_roots(heap, &entry.path), &entry.path);
    leads.contains(&target).then_some((entry.path, leads))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_read_back() {
        for text in ["js:$gameParty._gold", "js:$gameActors._data[1]._hp", "js:$gameActors._data[1]._exp[3]", "js:$gameParty._items[7]", "js:$x.a%20b[2][3].c"] {
            assert_eq!(JsPath::parse(text).unwrap().text(), text, "{text}");
        }
        let p = JsPath::parse("js:$gameActors._data[1]._paramPlus[2]").unwrap();
        assert_eq!(p.steps, vec![Step::Prop("_data".into()), Step::Index(1), Step::Prop("_paramPlus".into()), Step::Index(2)]);
        assert!(JsPath::parse("js:$gameParty").is_none());
        assert!(JsPath::parse("js:gameParty._gold").is_none());
        assert!(JsPath::parse("js:$gameParty.._gold").is_none());
        assert_eq!(root("$gameParty").prop("a b").text(), "js:$gameParty.a%20b");
    }
}
