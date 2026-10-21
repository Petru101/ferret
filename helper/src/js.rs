//! RPG Maker MV games: their JavaScript objects as NW.js's V8 keeps them, read from outside.
//! Every MV game keeps its state in the same globals (`$gameParty`, `$gameActors`,
//! `$gameVariables`...), so a value is named by its path from one of them:
//! `js:$gameParty._gold`, `js:$gameActors._data[1]._hp`, `js:$gameParty._items[7]`.
//! 64-bit V8 without pointer compression: NW.js 0.12 (V8 4.1, RPG Maker MV up to 1.5) and
//! 0.25-0.29 (V8 6.x, MV 1.6). MZ's newer NW.js isn't handled.
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
}

/// A value as a field or element holds it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Val {
    /// A small integer: the address of its i32 (the upper half of the slot).
    Int(u64),
    /// A double kept in the object or in a box of the field's own: the address of the f64.
    Real(u64),
    /// Any other object (tagged): a JavaScript object, a string, true/false, null, a hole, or
    /// a number in a box other fields may share (not written).
    Obj(u64),
    /// Not a value (unreadable, or not what V8 keeps there).
    Other,
}

/// How one game's V8 lays its objects out, and where its globals are.
pub struct Js {
    cells: Cells,
    /// The meta map (tagged).
    meta: u64,
    cell_map: u64,
    /// Where a field's index starts in a property's details (19 in V8 4.x, 20 in 6.x: a bit
    /// for constness came in between); its representation sits 14 bits lower.
    field_shift: u32,
    /// The cells of each of `GLOBALS` (several when stale copies are left around).
    globals: Vec<(&'static str, Vec<u64>)>,
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

fn is_smi(w: u64) -> bool {
    w & 0xffff_ffff == 0
}

fn smi(w: u64) -> Option<i32> {
    is_smi(w).then_some((w >> 32) as u32 as i32)
}

impl Js {
    /// Finds the globals' names (V8 strings: {map, hash, length as a Smi, characters at +24}),
    /// what points to them (their cells) and the layout, in two passes over memory.
    fn locate(heap: &Heap) -> Option<Js> {
        if heap.width() != 8 {
            return None;
        }
        let mem = heap.mem();
        let texts: Vec<&[u8]> = GLOBALS.iter().map(|g| g.as_bytes()).collect();
        let longest = texts.iter().map(|t| t.len()).max().unwrap_or(0);
        let mut names: Vec<Vec<u64>> = vec![Vec::new(); texts.len()];
        heap.each_chunk(24 + longest, |addr, b, fresh| {
            for off in (0..fresh).step_by(8) {
                if b.get(off + 24) != Some(&b'$') || b.get(off + 16..off + 20) != Some(&[0, 0, 0, 0]) {
                    continue;
                }
                let len = u32::from_le_bytes([b[off + 20], b[off + 21], b[off + 22], b[off + 23]]) as usize;
                for (i, t) in texts.iter().enumerate() {
                    if t.len() == len && b.get(off + 24..off + 24 + len) == Some(t) {
                        names[i].push(addr + off as u64 + 1);
                    }
                }
            }
        });
        // A string's map's map is the meta map, which is its own map.
        let meta = names.iter().flatten().find_map(|&s| {
            let m = word(mem, s - 1).filter(|&m| tagged(m))?;
            let meta = word(mem, m - 1).filter(|&x| tagged(x))?;
            (word(mem, meta - 1) == Some(meta)).then_some(meta)
        })?;
        let all: Vec<u64> = names.iter().flatten().copied().collect();
        let refs = heap.referrers(&all);
        let is_map = |m: u64| tagged(m) && word(mem, m - 1) == Some(meta);
        // Each way of keeping globals, by the cell map it finds: the one finding the most
        // globals wins.
        let mut found: Vec<((Cells, u64), Vec<(usize, u64)>)> = Vec::new();
        let mut note = |key: (Cells, u64), g: usize, cell: u64| match found.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push((g, cell)),
            None => found.push((key, vec![(g, cell)])),
        };
        for &r in &refs {
            let Some(name) = word(mem, r) else { continue };
            let Some(g) = names.iter().position(|v| v.contains(&name)) else { continue };
            if let (Some(m), Some(d)) = (word(mem, r - 16), word(mem, r - 8)) {
                if is_map(m) && is_smi(d) {
                    note((Cells::Named, m), g, r - 16);
                }
            }
            if let (Some(c), Some(d)) = (word(mem, r + 8), word(mem, r + 16)) {
                if tagged(c) && is_smi(d) && word(mem, c - 1).is_some_and(is_map) {
                    note((Cells::InDictionary, word(mem, c - 1)?), g, c - 1);
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
        let mut js = Js { cells, meta, cell_map, field_shift, globals };
        js.calibrate(mem);
        Some(js)
    }

    /// Checks the field index's place on an object every MV game has: Game_Party's first
    /// fields are `_inBattle`, `_gold`, `_steps`, set in that order.
    fn calibrate(&mut self, mem: &Mem) {
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

    fn is_map(&self, mem: &Mem, m: u64) -> bool {
        tagged(m) && word(mem, m - 1) == Some(self.meta)
    }

    fn map_of(&self, mem: &Mem, o: u64) -> Option<u64> {
        if !tagged(o) {
            return None;
        }
        word(mem, o - 1).filter(|&m| self.is_map(mem, m))
    }

    fn instance_type(&self, mem: &Mem, m: u64) -> Option<u16> {
        match self.cells {
            Cells::Named => half(mem, m - 1 + 12).map(|t| t as u16),
            Cells::InDictionary => byte(mem, m - 1 + 12).map(u16::from),
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
        if word(mem, cell)? != self.cell_map {
            return None;
        }
        let v = match self.cells {
            Cells::Named => {
                let n = word(mem, cell + 16)?;
                if self.text(mem, n).as_deref() != Some(name) {
                    return None;
                }
                word(mem, cell + 24)?
            }
            Cells::InDictionary => word(mem, cell + 8)?,
        };
        self.map_of(mem, v).map(|_| v)
    }

    /// A map's own properties: each name (a string, tagged) and its details.
    fn descriptors(&self, mem: &Mem, m: u64) -> Vec<(u64, u32)> {
        let Some(bits) = half(mem, m - 1 + 16) else { return Vec::new() };
        let own = (bits >> 10) & 0x3ff;
        let Some(d) = word(mem, m - 1 + 48).filter(|&d| tagged(d)) else { return Vec::new() };
        let mut b = vec![0u8; 24 * own as usize];
        if mem.read_exact_at(&mut b, d - 1 + 32).is_err() {
            return Vec::new();
        }
        let w = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let mut out = Vec::with_capacity(own as usize);
        for i in 0..own as usize {
            let (k, det) = (w(24 * i), w(24 * i + 8));
            if !tagged(k) || !is_smi(det) {
                return Vec::new();
            }
            out.push((k, (det >> 32) as u32));
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
        let m = self.map_of(mem, s)?;
        let t = self.instance_type(mem, m)?;
        if t >= 0x80 {
            return None;
        }
        let len = half(mem, s - 1 + 20)?;
        if len > MAX_TEXT {
            return None;
        }
        // V8 4.x: representation in bits 0-1, one byte a character = bit 2; 6.x: bits 0-2
        // (thin strings came), bit 3.
        let (one_byte, rep) = match self.cells {
            Cells::InDictionary => (t & 0x4 != 0, t & 0x3),
            Cells::Named => (t & 0x8 != 0, t & 0x7),
        };
        let o = s - 1;
        match rep {
            0 => {
                let mut b = vec![0u8; if one_byte { len } else { 2 * len } as usize];
                mem.read_exact_at(&mut b, o + 24).ok()?;
                Some(match one_byte {
                    true => b.iter().map(|&c| c as char).collect(),
                    false => String::from_utf16_lossy(&b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()),
                })
            }
            1 => {
                let a = self.text_in(mem, word(mem, o + 24)?, depth + 1)?;
                Some(a + &self.text_in(mem, word(mem, o + 32)?, depth + 1)?)
            }
            3 => {
                let parent = self.text_in(mem, word(mem, o + 24)?, depth + 1)?;
                let from = smi(word(mem, o + 32)?)? as usize;
                Some(parent.chars().skip(from).take(len as usize).collect())
            }
            5 => self.text_in(mem, word(mem, o + 24)?, depth + 1),
            _ => None,
        }
    }

    /// What a slot holds; `double`: the field is kept as a double in a box of its own.
    fn slot(&self, mem: &Mem, at: u64, double: bool) -> Val {
        let Some(w) = word(mem, at) else { return Val::Other };
        if is_smi(w) {
            return Val::Int(at + 4);
        }
        if !tagged(w) {
            return Val::Other;
        }
        if double {
            return Val::Real(w - 1 + 8);
        }
        match self.map_of(mem, w) {
            Some(_) => Val::Obj(w),
            None => Val::Other,
        }
    }

    /// An object's own properties by name, with what each holds (fast and dictionary-mode
    /// objects).
    pub fn fields(&self, mem: &Mem, o: u64) -> Vec<(String, Val)> {
        let Some(m) = self.map_of(mem, o) else { return Vec::new() };
        let Some(bits) = half(mem, m - 1 + 16) else { return Vec::new() };
        let props = word(mem, o - 1 + 8).unwrap_or(0);
        if bits >> 20 & 1 == 1 {
            return self.dictionary(mem, props, 56).into_iter().filter_map(|(k, v)| Some((self.text(mem, k)?, v))).collect();
        }
        let (Some(words), Some(b9)) = (byte(mem, m - 1 + 8), byte(mem, m - 1 + 9)) else { return Vec::new() };
        let (words, b9) = (u64::from(words), u64::from(b9));
        // V8 4.x keeps the count of in-object fields, 6.x where they start.
        let inside = match self.cells {
            Cells::InDictionary => b9,
            Cells::Named => words.saturating_sub(b9),
        };
        let start = (words.saturating_sub(inside)) * 8;
        // Doubles kept in the object itself (no box): a bit each in the layout descriptor (a
        // Smi; a pointer for objects of more fields, not read: those doubles are left out).
        let layout = word(mem, m - 1 + 56).unwrap_or(0);
        let mut out = Vec::new();
        for (k, det) in self.descriptors(mem, m) {
            // Constants and accessors aren't fields.
            if det & 3 != 0 {
                continue;
            }
            let index = u64::from((det >> self.field_shift) & 0x3ff);
            let rep = (det >> (self.field_shift - 14)) & 0xf;
            // Representations: ..., Smi 5, Integer32 6, Double 7, HeapObject 8, Tagged 9.
            let double = rep == 7;
            let val = if index < inside {
                let at = o - 1 + start + index * 8;
                let raw = match smi(layout) {
                    Some(bits) => index < 32 && (bits as u32) >> index & 1 == 1,
                    None => double,
                };
                match (double, raw) {
                    (true, true) if is_smi(layout) => Val::Real(at),
                    (true, true) => Val::Other,
                    _ => self.slot(mem, at, double),
                }
            } else {
                if !tagged(props) {
                    continue;
                }
                self.slot(mem, props - 1 + 16 + (index - inside) * 8, double)
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

    /// A hash table's entries {key, value, details} from `first` bytes in (a dictionary of
    /// names: 56, of numbers: 48): each key with what the value slot holds.
    fn dictionary(&self, mem: &Mem, table: u64, first: u64) -> Vec<(u64, Val)> {
        if !tagged(table) {
            return Vec::new();
        }
        let Some(len) = word(mem, table - 1 + 8).and_then(smi) else { return Vec::new() };
        let words = (len.max(0) as u64 + 2) * 8;
        let entries = (words.saturating_sub(first) / 24).min(4 * MAX_ELEMENTS as u64);
        (0..entries)
            .filter_map(|i| {
                let at = table - 1 + first + 24 * i;
                let key = word(mem, at)?;
                is_smi(word(mem, at + 16)?).then_some((key, self.slot(mem, at + 8, false)))
            })
            .collect()
    }

    /// An array's or object's indexed elements: (index, what it holds), holes left out.
    pub fn elements(&self, mem: &Mem, o: u64) -> Vec<(u32, Val)> {
        let Some(m) = self.map_of(mem, o) else { return Vec::new() };
        let Some(kind) = byte(mem, m - 1 + 15).map(|b| b >> 3) else { return Vec::new() };
        let Some(e) = word(mem, o - 1 + 16).filter(|&e| tagged(e)) else { return Vec::new() };
        let Some(n) = word(mem, e - 1 + 8).and_then(smi) else { return Vec::new() };
        let n = (n.max(0) as usize).min(MAX_ELEMENTS);
        match kind {
            // Smis or anything, packed or with holes.
            0..=3 => (0..n).map(|i| (i as u32, self.slot(mem, e - 1 + 16 + 8 * i as u64, false))).filter(|(_, v)| *v != Val::Other).collect(),
            // Doubles, packed or with holes.
            4 | 5 => (0..n)
                .filter_map(|i| {
                    let at = e - 1 + 16 + 8 * i as u64;
                    (word(mem, at)? != DOUBLE_HOLE).then_some((i as u32, Val::Real(at)))
                })
                .collect(),
            // A dictionary of numbers.
            6 => self.dictionary(mem, e, 48).into_iter().filter_map(|(k, v)| Some((smi(k)?.try_into().ok()?, v))).filter(|(_, v)| *v != Val::Other).collect(),
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
    let js = Js::locate(heap).map(Arc::new);
    *found = Some((heap.pid, js.clone(), Instant::now()));
    js
}

/// Whether the game is an RPG Maker MV game: its program sits next to `www/js/rpg_core.js`.
pub fn is_rpgmaker(pid: u32, program: Option<&std::path::Path>) -> bool {
    let Some(dir) = program.and_then(|p| p.parent()) else { return false };
    let inside = crate::os::inside(pid, dir);
    inside.join("www/js/rpg_core.js").exists() || dir.join("www/js/rpg_core.js").exists()
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
            Val::Int(a) | Val::Real(a) if last => return Some(a),
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
        if let Some(val @ (Val::Int(_) | Val::Real(_))) = val {
            self.entries.push(Entry { label: label.into(), val, path });
        }
    }

    /// Every number among an object's own fields, by the field's name.
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
            Val::Int(at) => half(mem, at),
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
            if let Some(class) = half(mem, class) {
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
                        Val::Int(addr) | Val::Obj(addr) => Listed { label: e.label, addr, kind: "i32" },
                        Val::Other => Listed { label: e.label, addr: 0, kind: "i32" },
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
    let entry = groups.into_iter().flat_map(|g| g.entries).find(|e| matches!(e.val, Val::Int(a) | Val::Real(a) if a == target))?;
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
