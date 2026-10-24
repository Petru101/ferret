//! Web games: HTML5 games wrapped in Electron or NW.js other than RPG Maker's (js.rs), the kind
//! made these days in plain JavaScript, Phaser, PixiJS or Three.js, often bundled by Vite. They
//! keep their state in no fixed globals, so it's found where V8 keeps a script's own
//! variables: the contexts of its scopes. A classic script's top-level `let`/`const`/`class`
//! live in its script context, a module's in its module context (nothing on `window` leads
//! there: only the browser's listeners hold the code), an IIFE's in its function context;
//! `var`s and functions are the global object's properties. Each context names its variables
//! through its ScopeInfo. A value is named by where it starts and the fields from there:
//! `web:@ship.hp` (a scope's variable), `web:score` (a global), `web:new:Bt.health` (the
//! objects made by the class a scope's variable holds: a Phaser scene, kept by nothing but the
//! browser's animation callback).
//!
//! V8 14.x and newer with pointer compression (Electron 21 on, NW.js 0.10x): 4-byte references
//! into a 4 GiB cage. ScopeInfos are found by their shape ({map, flags, parameter count, local
//! count, start, end, [module variable count], names or a hash table of them, infos}), the
//! contexts by their references to them ({map, length, scope info, previous, [extension],
//! variables}). Since V8 13.x a script context's variable sits in a ContextCell ({map, value,
//! dependent code, state, double}): a constant, a small integer or any value in `value`, an
//! int32 or a double in the last field, by its state.
//!
//! Not listed, never written: `const`s and ContextCells still in their constant state (V8's
//! optimised code may have the number built in, so a write might not be seen; a `let` the game
//! has changed once isn't constant any more), and numbers in boxes other values may share.

use crate::js::{Js, Val};
use crate::mem::Mem;
use crate::mono::player_like;
use crate::names::Heap;
use crate::python::{decode, encode};
use crate::ue::{Listed, ListedObject, MAX_LISTED};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// V8's kScopeInfoMaxInlinedLocalNamesSize: from this many variables a ScopeInfo keeps their
/// names in a hash table.
const MAX_INLINED: u32 = 75;
/// Variables read from one scope at most (a bundled game's module has thousands).
const MAX_LOCALS: u32 = 20000;
/// Objects looked into for one list at most, how deep it goes from a root, and elements of
/// one array listed at most.
const MAX_VISITED: usize = 20000;
const MAX_DEPTH: usize = 8;
const MAX_ELEMENTS: usize = 64;
/// Instances of one class listed at most.
const MAX_INSTANCES: usize = 16;

/// V8's read-only objects (the roots: maps of arrays, strings, cells, the empty array) sit in
/// the cage's first pages.
const READ_ONLY: u64 = 0x40000;

/// Scope types (ScopeInfo's flags, bits 0-3).
const SCRIPT_SCOPE: u32 = 0;
const MODULE_SCOPE: u32 = 3;
const FUNCTION_SCOPE: u32 = 5;
/// VariableMode of a `const` (infos' bits 0-3).
const MODE_CONST: u32 = 1;

/// ContextCell states: the value is in `value` (constant or small integer), an int32 or a
/// double in the last field, or the cell isn't used any more.
const CELL_CONST: u32 = 0;
const CELL_SMI: u32 = 1;
const CELL_INT32: u32 = 2;
const CELL_FLOAT64: u32 = 3;

/// What Phaser puts into every scene (Phaser.Scene's systems): not followed from a scene.
const PHASER: &[&str] = &[
    "sys", "game", "anims", "cache", "plugins", "registry", "scale", "sound", "textures", "events", "cameras", "add", "make", "scene", "children",
    "lights", "data", "input", "load", "time", "tweens", "physics", "matter", "renderer",
];

/// The global object's own properties that aren't the game's (JavaScript's and the browser's
/// objects kept as data).
const BUILTINS: &[&str] = &[
    "Math", "JSON", "Reflect", "Intl", "Atomics", "WebAssembly", "console", "CSS", "globalThis", "window", "self", "frames", "parent", "top", "chrome",
    "Iterator", "Temporal", "process", "require", "module", "exports", "global", "Buffer", "navigation", "crossOriginIsolated", "isSecureContext",
    "NaN", "Infinity", "undefined",
];

fn half(mem: &Mem, at: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    mem.read_exact_at(&mut b, at).ok().map(|_| u32::from_le_bytes(b))
}

fn f64_at(mem: &Mem, at: u64) -> Option<f64> {
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b, at).ok().map(|_| f64::from_le_bytes(b))
}

/// A compressed Smi's value (the number times two, bit 0 clear).
fn smi(x: u32) -> Option<i32> {
    (x & 1 == 0).then_some(x as i32 >> 1)
}

/// The attached game's ContextCell and HeapNumber maps (their 4-byte references, either may
/// be missing: a V8 without ContextCells), for reading and writing `Kind::JsVar` places
/// without the layout. Set when the game's V8 is located.
static VARS: Mutex<(Option<u32>, Option<u32>)> = Mutex::new((None, None));

/// A scope variable's number (`Val::Var`): its slot holds a small integer, a HeapNumber
/// ({map, double}) or a ContextCell (by its state: a constant or small integer in `value`,
/// an int32 or a double in the last field). None when it holds anything else now.
pub fn read_var(mem: &Mem, slot: u64) -> Option<f64> {
    let (cell, number) = *VARS.lock().unwrap();
    let number = number?;
    let base = slot & !0xffff_ffff;
    let x = half(mem, slot)?;
    if let Some(v) = smi(x) {
        return Some(f64::from(v));
    }
    let o = base | u64::from(x - 1);
    let map = half(mem, o)?;
    if map == number {
        return f64_at(mem, o + 4);
    }
    if Some(map) != cell {
        return None;
    }
    match half(mem, o + 12)? {
        CELL_CONST | CELL_SMI => {
            let v = half(mem, o + 4)?;
            match smi(v) {
                Some(v) => Some(f64::from(v)),
                None => {
                    let box_ = base | u64::from(v - 1);
                    (half(mem, box_)? == number).then(|| f64_at(mem, box_ + 4)).flatten()
                }
            }
        }
        CELL_INT32 => half(mem, o + 16).map(|v| f64::from(v as i32)),
        CELL_FLOAT64 => f64_at(mem, o + 16),
        _ => None,
    }
}

/// Writes a scope variable's number the way its slot holds it now: a ContextCell in its state
/// (never a constant: V8's optimised code may have it built in), else a small integer into
/// the slot (a HeapNumber is left alone, other variables may hold the same one: so only whole
/// numbers there).
pub fn write_var(mem: &Mem, slot: u64, v: f64) -> std::io::Result<()> {
    let refused = |why: &str| Err(std::io::Error::other(why.to_owned()));
    let (cell, Some(number)) = *VARS.lock().unwrap() else { return refused("this game's numbers aren't known yet") };
    let whole = v.fract() == 0.0 && v.abs() < f64::from(1 << 30);
    let small = (v as i32).wrapping_shl(1) as u32;
    let base = slot & !0xffff_ffff;
    let Some(x) = half(mem, slot) else { return refused("unreadable") };
    let o = base | u64::from(x.wrapping_sub(1));
    let map = if x & 1 == 1 { half(mem, o) } else { None };
    match map {
        None | Some(_) if x & 1 == 0 || map == Some(number) => match whole {
            true => mem.write_all_at(&small.to_le_bytes(), slot),
            false => refused("this variable takes whole numbers only"),
        },
        Some(m) if Some(m) == cell => match half(mem, o + 12) {
            Some(CELL_SMI) if whole => mem.write_all_at(&small.to_le_bytes(), o + 4),
            Some(CELL_SMI) => refused("this variable takes whole numbers only"),
            Some(CELL_INT32) if whole => mem.write_all_at(&(v as i32).to_le_bytes(), o + 16),
            Some(CELL_INT32) => refused("this variable takes whole numbers only"),
            Some(CELL_FLOAT64) => mem.write_all_at(&v.to_le_bytes(), o + 16),
            _ => refused("a constant now"),
        },
        _ => refused("not a number now"),
    }
}

/// A scope's variables as V8 keeps them.
pub struct Scope {
    /// Its ScopeInfo (tagged).
    pub info: u64,
    pub kind: u32,
    pub names: Vec<String>,
    /// Each variable's mode (`MODE_CONST`...).
    pub modes: Vec<u32>,
    /// Where its variables start among the context's slots (after scope info, previous and,
    /// for modules and some functions, the extension).
    pub first: u32,
}

/// One live context of a scope: its address (tagged) and the scope.
pub struct Context {
    pub at: u64,
    pub scope: usize,
}

/// A web game's V8: how to read its objects, its scopes and their contexts.
pub struct Web {
    pub js: Js,
    pub scope_map: u64,
    /// ContextCell's map, if this V8 has them.
    pub cell_map: Option<u64>,
    /// PropertyCell's map (the global object's properties).
    pub property_map: Option<u64>,
    /// HeapNumber's map (a double's box).
    pub number_map: Option<u64>,
    pub scopes: Vec<Scope>,
    pub contexts: Vec<Context>,
    /// The native context (tagged), from a script or module context's previous.
    pub native: Option<u64>,
}

impl Web {
    /// Finds the ScopeInfos of scripts, modules and functions by their shape in one pass over
    /// memory, then the contexts holding them in a second.
    pub fn locate(heap: &Heap) -> Option<Web> {
        if heap.width() != 8 {
            return None;
        }
        let mem = heap.mem();
        let u32_at = |b: &[u8], i: usize| b.get(i..i + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]));
        // (map, ScopeInfo) of everything shaped like one.
        let mut found: Vec<(u32, u64)> = Vec::new();
        heap.each_chunk(64, |addr, b, fresh| {
            for off in (0..fresh).step_by(4) {
                let w = |i: usize| u32_at(b, off + 4 * i);
                let (Some(map), Some(params), Some(n), Some(start), Some(end)) = (w(0), w(2), w(3), w(4), w(5)) else { break };
                if map & 1 == 0 || params & 1 != 0 || n & 1 != 0 || start & 1 != 0 || end & 1 != 0 {
                    continue;
                }
                let (n, params, start, end) = (n >> 1, params >> 1, start >> 1, end >> 1);
                if n == 0 || n > MAX_LOCALS || params > 255 || end < start || start > 1 << 26 {
                    continue;
                }
                let Some(flags) = w(1) else { break };
                let kind = flags & 0xf;
                if ![SCRIPT_SCOPE, MODULE_SCOPE, FUNCTION_SCOPE].contains(&kind) {
                    continue;
                }
                let names = 6 + usize::from(kind == MODULE_SCOPE);
                let n = n as usize;
                // Names (references; from MAX_INLINED on, one hash table) then infos (Smis; the
                // first few looked at).
                let refs = if n < MAX_INLINED as usize { n } else { 1 };
                if !(0..refs).all(|i| w(names + i).is_some_and(|x| x & 1 == 1)) || !(0..n.min(8)).all(|i| w(names + refs + i).is_some_and(|x| x & 1 == 0)) {
                    continue;
                }
                found.push((map, addr + off as u64 + 1));
            }
        });
        // The ScopeInfo map: of the candidates that are maps (a map's map is the meta map, a
        // map of itself), the one whose objects' names read as strings most often. A small
        // game has only a few scopes, so chance matches elsewhere can outnumber them.
        let mut maps: Vec<(u64, u32)> = found.iter().map(|f| (f.1 & !0xffff_ffff, f.0)).collect();
        maps.sort_unstable();
        maps.dedup();
        let mut best: Option<Web> = None;
        for &(base, map32) in maps.iter().take(1000) {
            let scope_map = base | u64::from(map32);
            let Some(meta) = half(mem, scope_map - 1).filter(|m| m & 1 == 1).map(|m| base | u64::from(m)) else { continue };
            if half(mem, meta - 1).map(|m| base | u64::from(m)) != Some(meta) {
                continue;
            }
            let mut web = Web { js: Js::web(base, meta, 0, true), scope_map, cell_map: None, property_map: None, number_map: None, scopes: Vec::new(), contexts: Vec::new(), native: None };
            for &(_, info) in found.iter().filter(|f| f.0 == map32 && f.1 & !0xffff_ffff == base) {
                if let Some(scope) = web.scope(mem, info) {
                    web.scopes.push(scope);
                }
            }
            if web.scopes.len() > best.as_ref().map_or(0, |b| b.scopes.len()) {
                best = Some(web);
            }
        }
        let mut web = best?;
        // Names V8's sandbox keeps outside the heap: read through its external pointer table,
        // then the scopes again.
        web.js.ept = web.external_table(heap);
        if web.js.ept.is_some() {
            let infos: Vec<u64> = web.scopes.iter().map(|s| s.info).collect();
            web.scopes = infos.into_iter().filter_map(|i| web.scope(mem, i)).collect();
        }
        web.find_contexts(heap);
        // V8 14.x or 15.x: from the objects the scopes' variables hold.
        let mut objects: Vec<u64> = Vec::new();
        for c in &web.contexts {
            let s = &web.scopes[c.scope];
            for i in 0..s.names.len() {
                if let Val::Obj(o) = web.js.slot(mem, c.at - 1 + 8 + 4 * u64::from(s.first + i as u32), false) {
                    objects.push(o);
                }
            }
            if objects.len() > 400 {
                break;
            }
        }
        web.js.calibrate_web(mem, &objects);
        web.learn_cells(mem);
        Some(web)
    }

    /// A ScopeInfo's kind and variables (None unless every name is a string).
    fn scope(&self, mem: &Mem, info: u64) -> Option<Scope> {
        let o = info - 1;
        let kind = half(mem, o + 4)? & 0xf;
        let n = smi(half(mem, o + 12)?)?.max(0) as u32;
        let first_name = o + 24 + 4 * u64::from(kind == MODULE_SCOPE);
        let (names, infos) = match n >= MAX_INLINED {
            true => (self.hashed_names(mem, n, first_name)?, first_name + 4),
            false => {
                let mut names = Vec::with_capacity(n as usize);
                for i in 0..u64::from(n) {
                    // A name the sandbox keeps outside the heap (an external string: function
                    // declarations' names in Electron) is shown by its place.
                    let w = self.js.word(mem, first_name + 4 * i)?;
                    if !self.is_string(mem, w) {
                        return None;
                    }
                    names.push(self.js.text(mem, w).unwrap_or_else(|| format!("#{i}")));
                }
                (names, first_name + 4 * u64::from(n))
            }
        };
        let modes = (0..u64::from(n)).map(|i| half(mem, infos + 4 * i).and_then(smi).map_or(0, |v| v as u32 & 0xf)).collect();
        Some(Scope { info, kind, names, modes, first: 0 })
    }

    /// Where V8's sandbox keeps its external pointer table: the start of the mapping that
    /// decodes the most of the scopes' unread names (external one-byte strings: {map, hash,
    /// length, resource handle, characters handle}) into identifiers of their length. The
    /// table is reserved outside the cage, its first segment read-only.
    fn external_table(&self, heap: &Heap) -> Option<u64> {
        let mem = heap.mem();
        let mut samples: Vec<(u32, u32)> = Vec::new();
        for s in &self.scopes {
            let first = s.info - 1 + 24 + 4 * u64::from(s.kind == MODULE_SCOPE);
            for (i, n) in s.names.iter().enumerate().take(MAX_INLINED as usize) {
                if !n.starts_with('#') {
                    continue;
                }
                let Some(w) = self.js.word(mem, first + 4 * i as u64) else { continue };
                let (Some(len), Some(handle)) = (half(mem, w - 1 + 8), half(mem, w - 1 + 16)) else { continue };
                if (1..=64).contains(&len) {
                    samples.push((handle, len));
                }
            }
            if samples.len() >= 24 {
                break;
            }
        }
        if samples.is_empty() {
            return None;
        }
        let identifier = |p: u64, len: u32| {
            let mut b = vec![0u8; len as usize];
            mem.read_exact_at(&mut b, p).is_ok() && b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$') && !b[0].is_ascii_digit()
        };
        let decodes = |t: u64| {
            samples
                .iter()
                .filter(|&&(h, len)| {
                    let mut e = [0u8; 8];
                    mem.read_exact_at(&mut e, t + u64::from(h >> 6) * 8).is_ok() && identifier(u64::from_le_bytes(e) & 0xffff_ffff_ffff, len)
                })
                .count()
        };
        let base = self.js.base();
        let (t, n) = heap.starts().into_iter().filter(|&t| t & 0xfff == 0 && !(base..base + (1 << 32)).contains(&t)).map(|t| (t, decodes(t))).max_by_key(|x| x.1)?;
        (n * 2 >= samples.len() && n > 0).then_some(t)
    }

    fn is_string(&self, mem: &Mem, w: u64) -> bool {
        self.js.map_of(mem, w).and_then(|m| self.js.instance_type(mem, m)).is_some_and(|t| t < 0x40)
    }

    /// The names of a scope of `MAX_INLINED` variables or more, from its NameToIndexHashTable
    /// ({map, length, element count, deleted count, capacity, then entries {name, index}}).
    fn hashed_names(&self, mem: &Mem, n: u32, at: u64) -> Option<Vec<String>> {
        let table = self.js.word(mem, at)?;
        self.js.map_of(mem, table)?;
        let capacity = smi(half(mem, table - 1 + 16)?)?.max(0) as u32;
        if capacity > 4 * MAX_LOCALS {
            return None;
        }
        let mut names: Vec<String> = (0..n).map(|i| format!("#{i}")).collect();
        let mut seen = 0;
        for e in 0..u64::from(capacity) {
            let entry = table - 1 + 20 + 8 * e;
            let (Some(key), Some(index)) = (self.js.word(mem, entry), half(mem, entry + 4).and_then(smi)) else { continue };
            let Some(slot) = names.get_mut(index.max(0) as usize) else { continue };
            if let Some(t) = self.js.text(mem, key) {
                *slot = t;
                seen += 1;
            }
        }
        (seen > 0).then_some(names)
    }

    /// The contexts referring to the scopes ({map, length, scope info, ...}): a context's
    /// variables are its last slots (`length` counts scope info, previous, an extension if
    /// any, then the variables).
    fn find_contexts(&mut self, heap: &Heap) {
        let mem = heap.mem();
        let base = self.js.base();
        let mut targets: Vec<(u32, usize)> = self.scopes.iter().enumerate().map(|(i, s)| ((s.info - base) as u32, i)).collect();
        targets.sort_unstable();
        let (Some(&(lo, _)), Some(&(hi, _))) = (targets.first(), targets.last()) else { return };
        let mut refs: Vec<(u64, usize)> = Vec::new();
        heap.each_chunk(0, |addr, b, fresh| {
            if addr & !0xffff_ffff != base {
                return;
            }
            for off in (8..fresh - fresh % 4).step_by(4) {
                let v = u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]);
                if v < lo || v > hi {
                    continue;
                }
                if let Ok(i) = targets.binary_search_by_key(&v, |t| t.0) {
                    refs.push((addr + off as u64 - 8 + 1, targets[i].1));
                }
            }
        });
        for (c, s) in refs {
            let Some(len) = half(mem, c - 1 + 4).and_then(smi) else { continue };
            let n = self.scopes[s].names.len() as i32;
            if len < n + 2 || len > n + 4 || !self.js.map_of(mem, c).is_some_and(|m| m != self.scope_map) {
                continue;
            }
            self.scopes[s].first = (len - n) as u32;
            if self.native.is_none() && self.scopes[s].kind != FUNCTION_SCOPE {
                self.native = self.js.word(mem, c - 1 + 12).filter(|&p| self.js.map_of(mem, p).is_some());
            }
            self.contexts.push(Context { at: c, scope: s });
        }
        // No script or module scope (a game wholly inside function wrappers: Construct 2): the
        // native context ends every context's chain of previous ones (theirs hold a ScopeInfo).
        if self.native.is_none() {
            self.native = self.contexts.iter().take(64).find_map(|c| {
                let mut at = c.at;
                for _ in 0..256 {
                    let p = self.js.word(mem, at - 1 + 12).filter(|&p| self.js.map_of(mem, p).is_some());
                    let info = p.and_then(|p| self.js.word(mem, p - 1 + 8));
                    match (p, info) {
                        (Some(p), Some(i)) if self.js.map_of(mem, i) == Some(self.scope_map) => at = p,
                        _ => return Some(at).filter(|&a| a != c.at),
                    }
                }
                None
            });
        }
    }

    /// The contexts whose variables are the game's own: scripts', modules' and outermost
    /// functions' (an IIFE wrapping the game, `window.onload = function () {...}`); not the
    /// closures inside libraries.
    fn roots(&self, mem: &Mem) -> Vec<&Context> {
        let native = self.native;
        self.contexts.iter().filter(|c| self.scopes[c.scope].kind != FUNCTION_SCOPE || native.is_some() && self.js.word(mem, c.at - 1 + 12) == native).collect()
    }

    /// A context's variable `i` as a place: a number as its slot (`Val::Var`: what the slot
    /// holds changes as the game goes, a ContextCell's state too), an object or a flag. Constants
    /// (see the module's notes) only as objects.
    pub fn variable(&self, mem: &Mem, c: &Context, i: usize) -> Val {
        let scope = &self.scopes[c.scope];
        let slot = c.at - 1 + 8 + 4 * u64::from(scope.first + i as u32);
        let constant = scope.modes.get(i) == Some(&MODE_CONST);
        let v = match self.js.slot(mem, slot, false) {
            Val::Obj(cell) if self.is_cell(mem, cell) => match half(mem, cell - 1 + 12) {
                Some(CELL_SMI | CELL_INT32 | CELL_FLOAT64) => Val::Var(slot),
                Some(CELL_CONST) => match self.js.slot(mem, cell - 1 + 4, false) {
                    v @ Val::Obj(_) if !self.is_number(mem, v) => v,
                    _ => Val::Other,
                },
                _ => Val::Other,
            },
            Val::Int(_) => Val::Var(slot),
            v @ Val::Obj(_) if self.is_number(mem, v) => Val::Var(slot),
            v => v,
        };
        match v {
            Val::Var(_) | Val::Bool(_) if constant => Val::Other,
            v => v,
        }
    }

    /// Whether a value is a HeapNumber.
    fn is_number(&self, mem: &Mem, v: Val) -> bool {
        let Val::Obj(o) = v else { return false };
        self.number_map.is_some_and(|n| self.js.map_of(mem, o) == Some(n))
    }

    /// Whether an object is a ContextCell: {map, value, dependent code, state 0-4, double}, its
    /// map a read-only one (once learned, that map).
    fn is_cell(&self, mem: &Mem, o: u64) -> bool {
        let Some(m) = self.js.map_of(mem, o) else { return false };
        if let Some(cm) = self.cell_map {
            return m == cm;
        }
        m - self.js.base() < READ_ONLY && half(mem, o - 1 + 12).is_some_and(|s| s <= 4) && half(mem, o - 1 + 8).is_some_and(|d| d & 1 == 1)
    }

    /// Learns ContextCell's map (the read-only map most script variables' slots point to) and
    /// PropertyCell's (the one most of the global object's dictionary entries have).
    fn learn_cells(&mut self, mem: &Mem) {
        let majority = |mut maps: Vec<u64>| {
            maps.sort_unstable();
            maps.chunk_by(|a, b| a == b).max_by_key(|c| c.len()).map(|c| c[0])
        };
        let mut maps: Vec<u64> = Vec::new();
        for c in self.contexts.iter().filter(|c| self.scopes[c.scope].kind == SCRIPT_SCOPE) {
            for i in 0..self.scopes[c.scope].names.len() {
                let slot = c.at - 1 + 8 + 4 * u64::from(self.scopes[c.scope].first + i as u32);
                if let Val::Obj(o) = self.js.slot(mem, slot, false) {
                    if self.is_cell(mem, o) {
                        maps.extend(self.js.map_of(mem, o));
                    }
                }
            }
        }
        self.cell_map = majority(maps);
        let maps: Vec<u64> = self.dictionary(mem).into_iter().filter_map(|e| self.js.map_of(mem, e)).collect();
        self.property_map = majority(maps);
        // HeapNumber's: the box of a double field ({map, double}) of the scopes' objects.
        let mut maps: Vec<u64> = Vec::new();
        for c in self.roots(mem) {
            for i in 0..self.scopes[c.scope].names.len() {
                let slot = c.at - 1 + 8 + 4 * u64::from(self.scopes[c.scope].first + i as u32);
                let Val::Obj(o) = self.js.slot(mem, slot, false) else { continue };
                maps.extend(self.js.fields(mem, o).into_iter().filter_map(|(_, v)| match v {
                    Val::Real(a) => self.js.map_of(mem, a - 4 + 1),
                    _ => None,
                }));
            }
            if maps.len() > 16 {
                break;
            }
        }
        self.number_map = majority(maps).or_else(|| {
            // Else the global NaN's: a HeapNumber on every page.
            self.dictionary(mem).into_iter().find_map(|cell| {
                let name = self.js.word(mem, cell - 1 + 4).and_then(|n| self.js.text(mem, n))?;
                (name == "NaN").then(|| self.js.word(mem, cell - 1 + 12).and_then(|v| self.js.map_of(mem, v)))?
            })
        });
        let base = self.js.base();
        *VARS.lock().unwrap() = (self.cell_map.map(|c| (c - base) as u32), self.number_map.map(|n| (n - base) as u32));
        self.js.number_map = self.number_map;
    }

    /// The global object's property dictionary's entries (objects): the global object is the
    /// native context's extension (slot 2), its properties a GlobalDictionary of cells.
    fn dictionary(&self, mem: &Mem) -> Vec<u64> {
        let Some(global) = self.native.and_then(|n| self.js.word(mem, n - 1 + 16)) else { return Vec::new() };
        let Some(dict) = self.js.word(mem, global - 1 + 4).filter(|&d| self.js.map_of(mem, d).is_some()) else { return Vec::new() };
        let Some(len) = half(mem, dict - 1 + 4).and_then(smi) else { return Vec::new() };
        (0..len.clamp(0, 1 << 16) as u64).filter_map(|i| self.js.word(mem, dict - 1 + 8 + 4 * i)).filter(|&e| self.js.map_of(mem, e).is_some()).collect()
    }

    /// The game's global properties: (name, cell, value) of each data property that isn't
    /// JavaScript's or the browser's (PropertyCell {map, name, details, value, dependent code};
    /// details' bit 0: an accessor).
    pub fn globals(&self, mem: &Mem) -> Vec<(String, u64, Val)> {
        let Some(pm) = self.property_map else { return Vec::new() };
        let mut out = Vec::new();
        for cell in self.dictionary(mem) {
            if self.js.map_of(mem, cell) != Some(pm) || half(mem, cell - 1 + 8).and_then(smi).is_none_or(|d| d & 1 == 1) {
                continue;
            }
            let Some(name) = self.js.word(mem, cell - 1 + 4).and_then(|n| self.js.text(mem, n)) else { continue };
            if BUILTINS.contains(&name.as_str()) || name.starts_with("on") && name.len() > 2 && name[2..].chars().all(|c| c.is_ascii_lowercase()) {
                continue;
            }
            // Functions (the browser's, JavaScript's constructors, the game's own) hold no state.
            let v = self.js.slot(mem, cell - 1 + 12, false);
            if let Val::Obj(o) = v {
                if self.callable(mem, o) || self.is_number(mem, v) {
                    continue;
                }
            }
            out.push((name, cell, v));
        }
        out
    }

    /// Every variable of the game's own scopes: (name, context, index).
    pub fn variables(&self, mem: &Mem) -> Vec<(String, u64, usize)> {
        self.roots(mem).into_iter().flat_map(|c| self.scopes[c.scope].names.iter().enumerate().map(move |(i, n)| (n.clone(), c.at, i))).collect()
    }

    /// An object's class: following its map's back pointers to the constructor (map +20:
    /// constructor or back pointer).
    fn constructor(&self, mem: &Mem, mut m: u64) -> Option<u64> {
        for _ in 0..64 {
            let c = self.js.word(mem, m - 1 + 20)?;
            match self.js.is_map(mem, c) {
                true => m = c,
                false => return Some(c),
            }
        }
        None
    }

    /// The objects made by the classes `classes` (tagged constructors, by name), at most
    /// `MAX_INSTANCES` each: those whose map's constructor is one of them.
    pub fn instances(&self, heap: &Heap, classes: &[(String, u64)]) -> Vec<(String, u64)> {
        if classes.is_empty() {
            return Vec::new();
        }
        let mem = heap.mem();
        self.objects_where(heap, |m| self.constructor(mem, m).and_then(|c| classes.iter().find(|(_, k)| *k == c)).map(|(n, _)| n.clone()))
    }

    /// Phaser's scenes, by their key (`sys.settings.key`): objects whose map's first own
    /// property is `sys` and that have Phaser's `add` and `cameras` too (Phaser.Scene's
    /// constructor sets `sys`, then the scene's systems are injected). A bundled game keeps its
    /// scene class nowhere else: `new Phaser.Game({scene: MainScene})` hands it to Phaser.
    pub fn scenes(&self, heap: &Heap) -> Vec<(String, u64)> {
        let mem = heap.mem();
        let found = self.objects_where(heap, |m| {
            let names: Vec<String> = self.js.descriptors(mem, m).into_iter().take(40).filter_map(|(k, _)| self.js.text(mem, k)).collect();
            (names.first().map(String::as_str) == Some("sys") && names.iter().any(|n| n == "add") && names.iter().any(|n| n == "cameras")).then(String::new)
        });
        found
            .into_iter()
            .filter_map(|(_, o)| {
                let Val::Obj(sys) = self.js.field(mem, o, "sys")? else { return None };
                let Val::Obj(settings) = self.js.field(mem, sys, "settings")? else { return None };
                let Val::Obj(key) = self.js.field(mem, settings, "key")? else { return None };
                Some((self.js.text(mem, key)?, o))
            })
            .collect()
    }

    /// The objects whose map `name_of` names (at most `MAX_INSTANCES` a name), in two passes
    /// over the cage: every JS object map (its map is the native context's meta map), then the
    /// objects with the maps named.
    fn objects_where(&self, heap: &Heap, name_of: impl Fn(u64) -> Option<String>) -> Vec<(String, u64)> {
        let mem = heap.mem();
        let base = self.js.base();
        // A JS object's map's map: the native context's own meta map (the native context is
        // made with one of them).
        let Some(meta) = self.native.and_then(|n| self.js.map_of(mem, n)).and_then(|m| self.js.word(mem, m - 1)) else { return Vec::new() };
        let meta32 = (meta - base) as u32;
        let mut maps: Vec<u64> = Vec::new();
        heap.each_chunk(0, |addr, b, fresh| {
            if addr & !0xffff_ffff != base {
                return;
            }
            for off in (0..fresh - fresh % 4).step_by(4) {
                if u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]) == meta32 {
                    maps.push(addr + off as u64 + 1);
                }
            }
        });
        let mut of: HashMap<u32, String> = HashMap::new();
        for m in maps {
            if self.js.is_map(mem, m) {
                if let Some(name) = name_of(m) {
                    of.insert((m - base) as u32, name);
                }
            }
        }
        if of.is_empty() {
            return Vec::new();
        }
        let object = |o: u64| self.is_object(mem, o);
        let mut out: Vec<(String, u64)> = Vec::new();
        let mut count: HashMap<String, usize> = HashMap::new();
        heap.each_chunk(0, |addr, b, fresh| {
            if addr & !0xffff_ffff != base {
                return;
            }
            for off in (0..fresh - fresh % 4).step_by(4) {
                let v = u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]);
                let Some(name) = of.get(&v) else { continue };
                // A map's back pointer (map +20) holds the map before it: not an object.
                if off >= 20 && u32::from_le_bytes([b[off - 20], b[off - 19], b[off - 18], b[off - 17]]) == meta32 {
                    continue;
                }
                let o = addr + off as u64 + 1;
                let n = count.entry(name.clone()).or_default();
                if *n < MAX_INSTANCES && object(o) {
                    *n += 1;
                    out.push((name.clone(), o));
                }
            }
        });
        out
    }

    /// GDevelop's games (`gdjs.RuntimeGame`): objects whose map has `_sceneStack`, `_variables`
    /// and `_data` (the game's runtime keeps its scenes, global variables and project data so).
    pub fn gd_games(&self, heap: &Heap) -> Vec<u64> {
        let mem = heap.mem();
        self.objects_where(heap, |m| {
            let names: Vec<String> = self.js.descriptors(mem, m).into_iter().take(80).filter_map(|(k, _)| self.js.text(mem, k)).collect();
            ["_sceneStack", "_variables", "_data"].iter().all(|n| names.iter().any(|x| x == n)).then(String::new)
        })
        .into_iter()
        .map(|(_, o)| o)
        .collect()
    }

    /// Construct 2's runtimes: the canvas element's `c2runtime` (cr_createRuntime sets it; the
    /// runtime's own properties are minified, `name` and `data` of its variables aren't).
    pub fn c2_runtimes(&self, heap: &Heap) -> Vec<u64> {
        let mem = heap.mem();
        self.objects_where(heap, |m| self.js.descriptors(mem, m).into_iter().take(80).any(|(k, _)| self.js.text(mem, k).as_deref() == Some("c2runtime")).then(String::new))
            .into_iter()
            .filter_map(|(_, canvas)| match self.js.field(mem, canvas, "c2runtime")? {
                Val::Obj(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    /// Construct 2's global variables: the runtime's array whose elements are its variables
    /// (objects with `name` and `data`): the array's field, and each variable's index, name and
    /// value.
    fn c2_globals(&self, mem: &Mem, runtime: u64) -> Option<(String, Vec<(u32, String, Val)>)> {
        let mut best: Option<(String, Vec<(u32, String, Val)>)> = None;
        for (field, v) in self.js.fields(mem, runtime) {
            let Val::Obj(list) = v else { continue };
            let mut vars = Vec::new();
            for (i, e) in self.js.elements(mem, list).into_iter().take(1000) {
                let Val::Obj(var) = e else { continue };
                let fields = self.js.fields(mem, var);
                let name = fields.iter().find(|f| f.0 == "name").and_then(|f| match f.1 {
                    Val::Obj(t) => self.js.text(mem, t),
                    _ => None,
                });
                let data = fields.iter().find(|f| f.0 == "data").map(|f| f.1).filter(|v| kind_of(*v).is_some());
                if let (Some(name), Some(data)) = (name, data) {
                    vars.push((i, name, data));
                }
            }
            if vars.len() > best.as_ref().map_or(0, |b| b.1.len()) {
                best = Some((field, vars));
            }
        }
        best
    }

    /// A GDevelop game's running scenes (`_sceneStack._stack`), by `_name`.
    fn gd_scenes(&self, mem: &Mem, game: u64) -> Vec<(String, u64)> {
        let Some(Val::Obj(stack)) = self.js.field(mem, game, "_sceneStack") else { return Vec::new() };
        let Some(Val::Obj(list)) = self.js.field(mem, stack, "_stack") else { return Vec::new() };
        self.js
            .elements(mem, list)
            .into_iter()
            .filter_map(|(_, v)| {
                let Val::Obj(scene) = v else { return None };
                let Val::Obj(n) = self.js.field(mem, scene, "_name")? else { return None };
                Some((self.js.text(mem, n)?, scene))
            })
            .collect()
    }

    /// A GDevelop VariablesContainer's variables (`_variables.items`: name -> Variable): each
    /// name, the Variable and its number or flag (`_value`, `_bool`).
    fn gd_variables(&self, mem: &Mem, container: u64) -> Vec<(String, u64, Val, &'static str)> {
        let Some(Val::Obj(table)) = self.js.field(mem, container, "_variables") else { return Vec::new() };
        let Some(Val::Obj(items)) = self.js.field(mem, table, "items") else { return Vec::new() };
        let mut out = Vec::new();
        for (name, v) in self.js.fields(mem, items) {
            // Extensions' own variables ("__CameraShake_Duration") aren't the game's.
            let Val::Obj(var) = v else { continue };
            if name.starts_with("__") {
                continue;
            }
            let fields = self.js.fields(mem, var);
            let ty = fields.iter().find(|f| f.0 == "_type").and_then(|f| match f.1 {
                Val::Obj(t) => self.js.text(mem, t),
                _ => None,
            });
            let (field, val) = match ty.as_deref() {
                Some("boolean") => ("_bool", fields.iter().find(|f| f.0 == "_bool").map(|f| f.1)),
                Some("string") | Some("structure") | Some("array") => continue,
                _ => ("_value", fields.iter().find(|f| f.0 == "_value").map(|f| f.1)),
            };
            if let Some(val) = val.filter(|v| kind_of(*v).is_some()) {
                out.push((name, var, val, field));
            }
        }
        out
    }

    /// Whether an object is a JavaScript object (not a stale copy's leftover word, a string or a
    /// number): its properties and elements are V8's arrays, the read-only empty ones, or a
    /// PropertyArray or FixedArray (read-only maps).
    fn is_object(&self, mem: &Mem, o: u64) -> bool {
        let base = self.js.base();
        let read_only = |p: u64| p.wrapping_sub(base) < READ_ONLY || self.js.word(mem, p - 1).is_some_and(|m| m.wrapping_sub(base) < READ_ONLY);
        [4, 8].iter().all(|f| self.js.word(mem, o - 1 + f).is_some_and(|p| p & 1 == 1 && read_only(p)))
    }

    /// Whether an object is a function (its map's bit_field, map +10: callable bit 1).
    fn callable(&self, mem: &Mem, o: u64) -> bool {
        self.js.map_of(mem, o).and_then(|m| half(mem, m - 1 + 8)).is_some_and(|w| (w >> 16) & 0x2 != 0)
    }

    /// The game's classes: the constructors its scopes' variables hold (a map's bit_field,
    /// map +10: callable bit 1, constructor bit 6).
    fn classes(&self, mem: &Mem) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = Vec::new();
        for (name, c, i) in self.variables(mem) {
            let Some(ctx) = self.contexts.iter().find(|x| x.at == c) else { continue };
            let Val::Obj(o) = self.variable(mem, ctx, i) else { continue };
            let bits = self.js.map_of(mem, o).and_then(|m| half(mem, m - 1 + 8)).map(|w| (w >> 16) as u8);
            if bits.is_some_and(|b| b & 0x42 == 0x42) {
                out.push((name, o));
            }
        }
        out
    }
}

/// The located V8 of the game attached to, per pid: checked on each use (its first context
/// still names its scope), found again when not (a reload makes new contexts), a failed search
/// again after `RETRY`.
pub fn shared(heap: &Heap) -> Option<Arc<Web>> {
    const RETRY: Duration = Duration::from_secs(5);
    static FOUND: Mutex<Option<(u32, Option<Arc<Web>>, Instant)>> = Mutex::new(None);
    let mut found = FOUND.lock().unwrap();
    if let Some((pid, web, at)) = found.as_ref() {
        let valid = web.as_ref().is_some_and(|w| w.contexts.first().is_some_and(|c| w.js.word(heap.mem(), c.at - 1 + 8) == Some(w.scopes[c.scope].info)));
        if *pid == heap.pid && (valid || web.is_none() && at.elapsed() < RETRY) {
            return web.clone();
        }
    }
    let web = Web::locate(heap).map(Arc::new);
    *found = Some((heap.pid, web.clone(), Instant::now()));
    web
}

/// Where a path starts.
#[derive(Clone, Debug, PartialEq)]
pub enum Root {
    /// A variable of a script's, module's or outermost function's scope: `@ship`.
    Var(String),
    /// A property of the global object (a `var` or a function): `score`.
    Global(String),
    /// The objects made by the class a scope's variable holds: `new:Bt`.
    New(String),
    /// A Phaser scene, by its key: `scene:main`.
    Scene(String),
    /// GDevelop's game (`gdjs.RuntimeGame`): `gdgame`.
    GdGame,
    /// A GDevelop scene in the game's scene stack, by its name: `gdscene:Level1`.
    GdScene(String),
    /// Construct 2's runtime (the canvas's `c2runtime`): `c2`.
    C2,
}

/// One step of a path: a property by name, or an element by index.
#[derive(Clone, Debug, PartialEq)]
enum Step {
    Prop(String),
    Index(u32),
}

/// A saved path from a root to a number or flag.
#[derive(Clone, Debug, PartialEq)]
pub struct WebPath {
    root: Root,
    steps: Vec<Step>,
}

impl WebPath {
    /// "web:@<variable>|new:<class variable>|scene:<key>|<global>[.<property>|[i]]...".
    pub fn parse(text: &str) -> Option<WebPath> {
        let rest = text.strip_prefix("web:")?;
        for (prefix, root) in [("gdgame", Root::GdGame), ("c2", Root::C2)] {
            if let Some(r) = rest.strip_prefix(prefix).filter(|r| r.is_empty() || r.starts_with(['.', '['])) {
                let mut p = WebPath::parse(&format!("web:@x{r}"))?;
                p.root = root;
                return Some(p);
            }
        }
        let (make, rest): (fn(String) -> Root, &str) = match () {
            _ if rest.starts_with('@') => (Root::Var, &rest[1..]),
            _ if rest.starts_with("new:") => (Root::New, &rest[4..]),
            _ if rest.starts_with("scene:") => (Root::Scene, &rest[6..]),
            _ if rest.starts_with("gdscene:") => (Root::GdScene, &rest[8..]),
            _ => (Root::Global, rest),
        };
        let end = rest.find(['.', '[']).unwrap_or(rest.len());
        let name = decode(&rest[..end])?;
        if name.is_empty() {
            return None;
        }
        let mut steps = Vec::new();
        let mut rest = &rest[end..];
        while !rest.is_empty() {
            if let Some(r) = rest.strip_prefix('[') {
                let (n, after) = r.split_once(']')?;
                steps.push(Step::Index(n.parse().ok()?));
                rest = after;
                continue;
            }
            let r = rest.strip_prefix('.')?;
            let end = r.find(['.', '[']).unwrap_or(r.len());
            let n = decode(&r[..end])?;
            if n.is_empty() {
                return None;
            }
            steps.push(Step::Prop(n));
            rest = &r[end..];
        }
        Some(WebPath { root: make(name), steps })
    }

    pub fn text(&self) -> String {
        let root = match &self.root {
            Root::Var(n) => format!("@{}", encode(n)),
            Root::Global(n) => encode(n),
            Root::New(n) => format!("new:{}", encode(n)),
            Root::Scene(n) => format!("scene:{}", encode(n)),
            Root::GdGame => "gdgame".into(),
            Root::GdScene(n) => format!("gdscene:{}", encode(n)),
            Root::C2 => "c2".into(),
        };
        format!("web:{root}{}", self.steps_text(true))
    }

    fn steps_text(&self, encoded: bool) -> String {
        let mut t = String::new();
        for s in &self.steps {
            match s {
                Step::Prop(n) => t += &format!(".{}", if encoded { encode(n) } else { n.clone() }),
                Step::Index(i) => t += &format!("[{i}]"),
            }
        }
        t
    }

    /// As the list shows it: `ship.hp`, `new Bt.health`.
    fn shown(&self) -> String {
        let root = match &self.root {
            Root::Var(n) | Root::Global(n) => n.clone(),
            Root::New(n) => format!("new {n}"),
            Root::Scene(n) => format!("scene {n}"),
            Root::GdGame => "game".into(),
            Root::GdScene(n) => n.clone(),
            Root::C2 => "runtime".into(),
        };
        root + &self.steps_text(false)
    }

    pub fn describe(&self) -> String {
        format!("{} in the game's JavaScript", self.shown())
    }

    fn then(&self, s: Step) -> WebPath {
        let mut p = self.clone();
        p.steps.push(s);
        p
    }

    fn last_name(&self) -> &str {
        self.steps
            .iter()
            .rev()
            .find_map(|s| match s {
                Step::Prop(n) => Some(n.as_str()),
                _ => None,
            })
            .unwrap_or(match &self.root {
                Root::Var(n) | Root::Global(n) | Root::New(n) | Root::Scene(n) | Root::GdScene(n) => n,
                Root::GdGame => "game",
                Root::C2 => "runtime",
            })
    }
}

/// Where the path's root is now: the contexts with that variable, the global's cell, or the
/// class's objects (two passes over memory).
pub fn find_roots(heap: &Heap, path: &WebPath) -> Vec<u64> {
    let Some(web) = shared(heap) else { return Vec::new() };
    let mem = heap.mem();
    match &path.root {
        Root::Var(name) => web.variables(mem).into_iter().filter(|(n, _, _)| n == name).map(|(_, c, _)| c).collect(),
        Root::Global(name) => web.globals(mem).into_iter().filter(|(n, _, _)| n == name).map(|(_, c, _)| c).collect(),
        Root::New(name) => {
            let classes: Vec<(String, u64)> = web.classes(mem).into_iter().filter(|(n, _)| n == name).collect();
            web.instances(heap, &classes).into_iter().map(|(_, o)| o).collect()
        }
        Root::Scene(key) => web.scenes(heap).into_iter().filter(|(k, _)| k == key).map(|(_, o)| o).collect(),
        Root::GdGame => web.gd_games(heap),
        Root::GdScene(name) => web.gd_games(heap).into_iter().flat_map(|g| web.gd_scenes(mem, g)).filter(|(n, _)| n == name).map(|(_, o)| o).collect(),
        Root::C2 => web.c2_runtimes(heap),
    }
}

/// Where the path leads from its roots now: each number's or flag's place.
pub fn walk(heap: &Heap, roots: &[u64], path: &WebPath) -> Vec<u64> {
    let Some(web) = shared(heap) else { return Vec::new() };
    let mem = heap.mem();
    let mut out: Vec<u64> = roots.iter().filter_map(|&r| follow(&web, mem, root_value(&web, mem, r, &path.root)?, &path.steps)).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// What a root holds now (None when it isn't that root any more).
fn root_value(web: &Web, mem: &Mem, r: u64, root: &Root) -> Option<Val> {
    match root {
        Root::Var(name) => {
            let c = web.contexts.iter().find(|c| c.at == r)?;
            // Still that scope's context.
            if web.js.word(mem, r - 1 + 8) != Some(web.scopes[c.scope].info) {
                return None;
            }
            let i = web.scopes[c.scope].names.iter().position(|n| n == name)?;
            Some(web.variable(mem, c, i))
        }
        Root::Global(name) => {
            let n = web.js.word(mem, r - 1 + 4)?;
            (web.js.map_of(mem, r) == web.property_map && web.js.text(mem, n).as_deref() == Some(name)).then(|| web.js.slot(mem, r - 1 + 12, false))
        }
        Root::New(_) | Root::Scene(_) | Root::GdGame | Root::GdScene(_) | Root::C2 => web.js.map_of(mem, r).map(|_| Val::Obj(r)),
    }
}

fn follow(web: &Web, mem: &Mem, mut v: Val, steps: &[Step]) -> Option<u64> {
    for s in steps {
        let Val::Obj(o) = v else { return None };
        v = match s {
            Step::Prop(name) => web.js.field(mem, o, name)?,
            Step::Index(n) => web.js.elements(mem, o).into_iter().find(|(k, _)| k == n)?.1,
        };
    }
    match v {
        Val::Int(a) | Val::Real(a) | Val::Bool(a) | Val::Var(a) => Some(a),
        _ => None,
    }
}

/// The value type of the place a path leads to now (a field kept as a small integer becomes a
/// double's box when the game stores a fraction there: written as the old type, the number
/// would go wrong).
pub fn kind_now(heap: &Heap, roots: &[u64], path: &WebPath) -> Option<&'static str> {
    let web = shared(heap)?;
    let mem = heap.mem();
    let mut v = roots.iter().find_map(|&r| root_value(&web, mem, r, &path.root))?;
    for s in &path.steps {
        let Val::Obj(o) = v else { return None };
        v = match s {
            Step::Prop(name) => web.js.field(mem, o, name)?,
            Step::Index(n) => web.js.elements(mem, o).into_iter().find(|(k, _)| k == n)?.1,
        };
    }
    kind_of(v)
}

/// An object (or a scope's or the globals' variables) whose numbers are listed.
struct Group {
    path: WebPath,
    /// Shown: "Script", "Globals", the class's variable, "Object".
    class: String,
    numbers: Vec<(String, Val, WebPath)>,
}

fn kind_of(v: Val) -> Option<&'static str> {
    match v {
        Val::Int(_) => Some("smi"),
        Val::Var(_) => Some("jsvar"),
        Val::Real(_) => Some("f64"),
        Val::Bool(_) => Some("jsbool"),
        _ => None,
    }
}

/// Every number and flag reachable from the game's roots, breadth first (each object named
/// by its shortest path): the scopes' variables, the globals, the classes' objects.
fn groups(heap: &Heap) -> Option<Vec<Group>> {
    let web = shared(heap)?;
    let mem = heap.mem();
    let mut out: Vec<Group> = Vec::new();
    let mut queue: VecDeque<(u64, WebPath, usize)> = VecDeque::new();
    let mut seen: HashSet<u64> = HashSet::new();
    // GDevelop: the game's, each running scene's and each object's variables, by the names the
    // game's maker gave them.
    for game in web.gd_games(heap).into_iter().take(1) {
        let vars = |container: u64, root: &WebPath, seen: &mut HashSet<u64>| -> Vec<(String, Val, WebPath)> {
            web.gd_variables(mem, container)
                .into_iter()
                .map(|(name, var, val, field)| {
                    seen.insert(var);
                    (name.clone(), val, root.then(Step::Prop("_variables".into())).then(Step::Prop("items".into())).then(Step::Prop(name)).then(Step::Prop(field.into())))
                })
                .collect()
        };
        let root = WebPath { root: Root::GdGame, steps: vec![Step::Prop("_variables".into())] };
        if let Some(Val::Obj(c)) = web.js.field(mem, game, "_variables") {
            out.push(Group { path: WebPath { root: Root::GdGame, steps: Vec::new() }, class: "Global variables".into(), numbers: vars(c, &root, &mut seen) });
        }
        for (scene_name, scene) in web.gd_scenes(mem, game) {
            let sroot = WebPath { root: Root::GdScene(scene_name.clone()), steps: vec![Step::Prop("_variables".into())] };
            if let Some(Val::Obj(c)) = web.js.field(mem, scene, "_variables") {
                out.push(Group { path: WebPath { root: Root::GdScene(scene_name.clone()), steps: Vec::new() }, class: "Scene variables".into(), numbers: vars(c, &sroot, &mut seen) });
            }
            let Some(Val::Obj(instances)) = web.js.field(mem, scene, "_instances") else { continue };
            let Some(Val::Obj(items)) = web.js.field(mem, instances, "items") else { continue };
            for (object, list) in web.js.fields(mem, items) {
                let Val::Obj(list) = list else { continue };
                for (i, v) in web.js.elements(mem, list).into_iter().take(8) {
                    let Val::Obj(o) = v else { continue };
                    let Some(Val::Obj(c)) = web.js.field(mem, o, "_variables") else { continue };
                    let path = WebPath { root: Root::GdScene(scene_name.clone()), steps: vec![Step::Prop("_instances".into()), Step::Prop("items".into()), Step::Prop(object.clone()), Step::Index(i)] };
                    let numbers = vars(c, &path.then(Step::Prop("_variables".into())), &mut seen);
                    seen.insert(o);
                    if !numbers.is_empty() {
                        out.push(Group { path, class: object.clone(), numbers });
                    }
                }
            }
        }
    }
    let mut vars = Group { path: WebPath { root: Root::Var(String::new()), steps: Vec::new() }, class: "Script".into(), numbers: Vec::new() };
    for (name, c, i) in web.variables(mem) {
        let Some(ctx) = web.contexts.iter().find(|x| x.at == c) else { continue };
        let path = WebPath { root: Root::Var(name.clone()), steps: Vec::new() };
        match web.variable(mem, ctx, i) {
            v @ (Val::Var(_) | Val::Bool(_)) => vars.numbers.push((name, v, path)),
            Val::Obj(o) => queue.push_back((o, path, 1)),
            _ => {}
        }
    }
    let mut globals = Group { path: WebPath { root: Root::Global(String::new()), steps: Vec::new() }, class: "Globals".into(), numbers: Vec::new() };
    for (name, _, v) in web.globals(mem) {
        let path = WebPath { root: Root::Global(name.clone()), steps: Vec::new() };
        match v {
            Val::Int(_) | Val::Real(_) | Val::Bool(_) => globals.numbers.push((name, v, path)),
            Val::Obj(o) => queue.push_back((o, path, 1)),
            _ => {}
        }
    }
    // Phaser's scenes before the rest: the game's state, but also the way to Phaser's own
    // objects (only the scene's own numbers are the game's: `PHASER` below).
    for (key, o) in web.scenes(heap).into_iter().filter(|(k, _)| k != "__SYSTEM") {
        queue.push_front((o, WebPath { root: Root::Scene(key), steps: Vec::new() }, 1));
    }
    for (class, o) in web.instances(heap, &web.classes(mem)) {
        queue.push_back((o, WebPath { root: Root::New(class), steps: Vec::new() }, 1));
    }
    // Construct 2: the event sheets' global variables, by name.
    for runtime in web.c2_runtimes(heap).into_iter().take(1) {
        if let Some((field, list)) = web.c2_globals(mem, runtime) {
            let path = WebPath { root: Root::C2, steps: vec![Step::Prop(field)] };
            let numbers = list.into_iter().map(|(i, name, v)| (name, v, path.then(Step::Index(i)).then(Step::Prop("data".into())))).collect();
            out.push(Group { path: WebPath { root: Root::C2, steps: Vec::new() }, class: "Global variables".into(), numbers });
        }
        seen.insert(runtime);
    }
    out.extend([vars, globals].into_iter().filter(|g| !g.numbers.is_empty()));
    let (mut visited, mut listed) = (0, 0);
    while let Some((o, path, depth)) = queue.pop_front() {
        if !seen.insert(o) || !web.is_object(mem, o) {
            continue;
        }
        visited += 1;
        if visited > MAX_VISITED || listed > MAX_LISTED {
            break;
        }
        let class = match &path.root {
            Root::New(c) if path.steps.is_empty() => c.clone(),
            Root::Scene(_) if path.steps.is_empty() => "Scene".into(),
            _ => "Object".into(),
        };
        // A Phaser scene's own fields only: its systems (`sys`, `add`, `cameras`...) lead into
        // the engine, thousands of objects that aren't the game's.
        let scene = matches!(path.root, Root::Scene(_)) && path.steps.is_empty();
        let mut g = Group { path: path.clone(), class, numbers: Vec::new() };
        let mut refs: Vec<(Step, u64)> = Vec::new();
        for (name, v) in web.js.fields(mem, o) {
            match v {
                Val::Obj(_) if scene && PHASER.contains(&name.as_str()) => {}
                Val::Obj(x) => refs.push((Step::Prop(name), x)),
                v if kind_of(v).is_some() => g.numbers.push((name.clone(), v, path.then(Step::Prop(name)))),
                _ => {}
            }
        }
        for (i, v) in web.js.elements(mem, o).into_iter().take(MAX_ELEMENTS) {
            match v {
                Val::Obj(x) => refs.push((Step::Index(i), x)),
                v if kind_of(v).is_some() => g.numbers.push((format!("[{i}]"), v, path.then(Step::Index(i)))),
                _ => {}
            }
        }
        if depth < MAX_DEPTH {
            for (s, x) in refs {
                if !seen.contains(&x) {
                    queue.push_back((x, path.then(s), depth + 1));
                }
            }
        }
        // V8's read-only objects (the empty arrays' elements, read-only strings) are never the
        // game's.
        let base = web.js.base();
        g.numbers.retain(|n| place(n.1).is_some_and(|a| a.wrapping_sub(base) >= READ_ONLY));
        if !g.numbers.is_empty() {
            listed += g.numbers.len();
            out.push(g);
        }
    }
    Some(out)
}

/// The player's sort of number by its name: `player_like`'s words, and the short ones hand-written
/// games use (`hp`, `maxHp`, `life`, `xp`, `credits`, `hull`...).
fn players(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    player_like(name) || ["hp", "maxhp", "life", "xp", "exp", "credits", "cash", "hull", "shield", "shields", "armor", "armour"].contains(&n.as_str())
}

/// The Browse tab's list for a web game: the objects with the player's sort of numbers first
/// (hp, energy, gold, lives...), then the rest under Show Everything.
pub fn player(heap: &Heap) -> Option<Vec<ListedObject>> {
    let mut groups = groups(heap)?;
    groups.retain(|g| !g.numbers.is_empty());
    // An engine's own state found (GDevelop's variables, a Phaser scene): the scripts' top-level
    // variables are then mostly its libraries' (PixiJS's).
    let engine = groups.iter().any(|g| matches!(g.path.root, Root::Scene(_) | Root::GdGame | Root::GdScene(_) | Root::C2));
    let mut out: Vec<ListedObject> = groups
        .into_iter()
        .map(|g| {
            // The game's own variables and its scene are its state, whatever their names.
            // GDevelop's objects' variables are the game's too (the maker named them).
            let own = !engine && g.path.steps.is_empty() && matches!(&g.path.root, Root::Var(n) | Root::Global(n) if n.is_empty())
                || matches!(g.path.root, Root::Scene(_) | Root::GdGame | Root::GdScene(_) | Root::C2) && (g.path.steps.is_empty() || g.class != "Object");
            let main = own || g.numbers.iter().any(|n| players(&n.0));
            let character = players(g.path.last_name()) || ["player", "hero", "ship"].contains(&g.path.last_name().to_ascii_lowercase().as_str());
            let name = match g.path.root {
                Root::Var(ref n) | Root::Global(ref n) if n.is_empty() => g.class.clone(),
                Root::GdGame | Root::C2 if g.path.steps.is_empty() => g.class.clone(),
                Root::GdScene(ref n) if g.path.steps.is_empty() => format!("{n} ({})", g.class),
                // A GDevelop object's instance: "Player[0]".
                Root::GdScene(_) if g.path.steps.len() == 4 => format!("{}{}", g.class, g.path.steps_text(false).rsplit('.').next().unwrap_or_default().trim_start_matches(&*g.class)),
                _ => g.path.shown(),
            };
            ListedObject {
                name,
                class: g.class,
                role: if character && main { "character" } else { "game" },
                owner: None,
                more: !main,
                fields: g.numbers.into_iter().filter_map(|(label, v, _)| Some(Listed { label, addr: place(v)?, kind: kind_of(v)? })).collect(),
            }
        })
        .collect();
    out.sort_by_key(|o| (o.more, o.role != "character"));
    Some(out)
}

fn place(v: Val) -> Option<u64> {
    match v {
        Val::Int(a) | Val::Real(a) | Val::Bool(a) | Val::Var(a) => Some(a),
        _ => None,
    }
}

/// The path to a number the list has, checked by following it the way a restart does, and
/// its value type.
pub fn discover(heap: &Heap, target: u64) -> Option<(WebPath, Vec<u64>, &'static str)> {
    let (_, v, path) = groups(heap)?.into_iter().flat_map(|g| g.numbers).find(|n| place(n.1) == Some(target))?;
    let leads = walk(heap, &find_roots(heap, &path), &path);
    leads.contains(&target).then_some((path, leads, kind_of(v)?))
}

/// Learns the game's true and false (`js::learn_bools`) from its variables' objects.
pub fn learn_bools(heap: &Heap) {
    let Some(web) = shared(heap) else { return };
    let mem = heap.mem();
    for (_, c, i) in web.variables(mem) {
        let Some(ctx) = web.contexts.iter().find(|x| x.at == c) else { continue };
        if let Val::Obj(o) = web.variable(mem, ctx, i) {
            web.js.fields(mem, o);
        }
        if crate::js::bools_known() {
            return;
        }
    }
}

/// Web game engines, by a word in their files' names (Phaser, PixiJS, Three.js, Babylon.js,
/// GDevelop, Construct, Kaboom/Kaplay, PlayCanvas, Excalibur, melonJS, Cocos, LittleJS) or
/// their packages' names in package.json.
const ENGINES: &[&str] = &[
    "phaser", "pixi", "three.module", "three.min", "three.core", "babylon", "gdjs", "c3runtime", "c2runtime", "kaboom", "kaplay", "playcanvas", "excalibur",
    "melonjs", "cocos2d", "cocos-engine", "littlejs",
];
const ENGINE_PACKAGES: &[&str] = &["phaser", "pixi.js", "three", "babylonjs", "@babylonjs/core", "kaboom", "kaplay", "playcanvas", "excalibur", "melonjs", "littlejsengine"];

/// An Electron app: where its files are, what it's called, whether it's a game.
pub struct App {
    pub dir: PathBuf,
    pub name: String,
    pub game: bool,
}

/// The Electron app a program runs (Chromium's `resources.pak` beside it): its files in
/// `resources/app.asar` or `resources/app/` (a packaged app), or the folder `--app-path`
/// names (Electron's own program running a folder). A game when its files name a web game
/// engine, or a page of it declares its `<canvas>` (a hand-written game draws in one; desktop
/// apps build their pages with frameworks). Named after its program, which a packaged app
/// carries the name of, else its package.json's productName or name, else its folder.
/// `inside`: a path as the game's process sees it (`os::inside`).
pub fn electron_app(program: &Path, cmdline: &str, inside: impl Fn(&Path) -> PathBuf) -> Option<App> {
    let dir = program.parent()?;
    let here = inside(dir);
    if !here.join("resources.pak").exists() || here.join("package.json").exists() || here.join("package.nw").exists() {
        return None;
    }
    // Chromium rewrites its process title into one string: a path with spaces runs to the next
    // flag.
    let app_path = cmdline.split_once("--app-path=").map(|(_, rest)| PathBuf::from(rest.split(" --").next().unwrap_or(rest).trim()));
    let files: Box<dyn Files> = match () {
        _ if here.join("resources/app.asar").is_file() => Box::new(Asar::open(&here.join("resources/app.asar"))?),
        _ if here.join("resources/app/package.json").is_file() => Box::new(Folder(here.join("resources/app"))),
        _ => Box::new(Folder(inside(app_path.as_deref()?))),
    };
    let names = files.names();
    let engine = names.iter().any(|n| ENGINES.iter().any(|e| n.to_ascii_lowercase().contains(e)));
    let package = files.read("package.json").and_then(|b| crate::json::parse(&String::from_utf8_lossy(&b)));
    let depends = package.as_ref().is_some_and(|p| {
        ["dependencies", "devDependencies"].iter().any(|d| match p.get(d) {
            Some(crate::json::Value::Obj(deps)) => deps.iter().any(|(k, _)| ENGINE_PACKAGES.contains(&k.as_str())),
            _ => false,
        })
    });
    let canvas = names.iter().filter(|n| n.ends_with(".html") && n.matches('/').count() <= 2).take(16).any(|n| files.read(n).is_some_and(|b| b.windows(7).any(|w| w.eq_ignore_ascii_case(b"<canvas"))));
    let program_name = program.file_name()?.to_string_lossy().into_owned();
    let product = package.as_ref().and_then(|p| p.get("productName").or_else(|| p.get("name"))).and_then(|v| v.str()).map(str::to_owned);
    let name = match program_name.as_str() {
        "electron" | "electron.exe" => product.or_else(|| app_path.as_deref().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()))?,
        _ => program_name.trim_end_matches(".exe").to_owned(),
    };
    Some(App { dir: app_path.unwrap_or_else(|| dir.to_path_buf()), name, game: engine || depends || canvas })
}

/// An app's files: their paths ('/'-separated, a few folders deep) and contents (the first
/// `MAX_FILE` bytes).
trait Files {
    fn names(&self) -> Vec<String>;
    fn read(&self, name: &str) -> Option<Vec<u8>>;
}

const MAX_FILE: usize = 256 * 1024;

struct Folder(PathBuf);

impl Files for Folder {
    fn names(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut dirs = vec![(self.0.clone(), String::new(), 0)];
        while let Some((d, prefix, depth)) = dirs.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten().take(2000) {
                let name = format!("{prefix}{}", e.file_name().to_string_lossy());
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    if depth < 3 && out.len() < 20000 {
                        dirs.push((e.path(), format!("{name}/"), depth + 1));
                    }
                } else {
                    out.push(name);
                }
            }
        }
        out
    }

    fn read(&self, name: &str) -> Option<Vec<u8>> {
        use std::io::Read;
        let mut b = Vec::new();
        std::fs::File::open(self.0.join(name)).ok()?.take(MAX_FILE as u64).read_to_end(&mut b).ok()?;
        Some(b)
    }
}

/// An asar archive (Electron's): {4, header size, ..., JSON length, JSON}, the files after the
/// header; the JSON a tree of {"files": {name: {"files": ...} | {"offset": "<n>", "size": n}}}.
struct Asar {
    path: PathBuf,
    data: u64,
    /// (path, offset in the data).
    files: Vec<(String, u64)>,
}

impl Asar {
    fn open(path: &Path) -> Option<Asar> {
        use std::io::Read;
        let mut f = std::fs::File::open(path).ok()?;
        let mut head = [0u8; 16];
        f.read_exact(&mut head).ok()?;
        let at = |i: usize| u32::from_le_bytes(head[i..i + 4].try_into().unwrap());
        let (size, len) = (at(4), at(12) as usize);
        if len > 64 << 20 {
            return None;
        }
        let mut json = vec![0u8; len];
        f.read_exact(&mut json).ok()?;
        let tree = crate::json::parse(&String::from_utf8_lossy(&json))?;
        let mut files = Vec::new();
        let mut todo = vec![(tree.get("files")?, String::new(), 0)];
        while let Some((node, prefix, depth)) = todo.pop() {
            let crate::json::Value::Obj(entries) = node else { continue };
            for (name, v) in entries.iter().take(5000) {
                let full = format!("{prefix}{name}");
                match (v.get("files"), v.get("offset").and_then(|o| o.str()).and_then(|o| o.parse().ok())) {
                    (Some(sub), _) if depth < 3 && files.len() < 20000 => todo.push((sub, format!("{full}/"), depth + 1)),
                    (None, Some(off)) => files.push((full, off)),
                    _ => {}
                }
            }
        }
        Some(Asar { path: path.to_path_buf(), data: 8 + u64::from(size), files })
    }
}

impl Files for Asar {
    fn names(&self) -> Vec<String> {
        self.files.iter().map(|f| f.0.clone()).collect()
    }

    /// The file's bytes from its offset on (its size isn't kept: the archive's next files may
    /// follow).
    fn read(&self, name: &str) -> Option<Vec<u8>> {
        use std::io::{Read, Seek};
        let &(_, off) = self.files.iter().find(|f| f.0 == name)?;
        let mut f = std::fs::File::open(&self.path).ok()?;
        f.seek(std::io::SeekFrom::Start(self.data + off)).ok()?;
        let mut b = Vec::new();
        f.take(MAX_FILE as u64).read_to_end(&mut b).ok()?;
        Some(b)
    }
}

/// webscan: the scopes and contexts found, each variable with what it holds, and the globals
/// (diagnostics).
pub fn diagnose(heap: &Heap) -> Vec<String> {
    let t = Instant::now();
    let Some(web) = shared(heap) else { return vec!["no V8 scopes found".into()] };
    let mem = heap.mem();
    let kinds = |k: u32| match k {
        SCRIPT_SCOPE => "script",
        MODULE_SCOPE => "module",
        _ => "function",
    };
    let show = |v: Val| match v {
        Val::Int(a) => format!("smi {:?}", half(mem, a).and_then(smi)),
        Val::Var(a) => format!("number {:?}", read_var(mem, a)),
        Val::Real(a) => format!("double {:?}", f64_at(mem, a)),
        Val::Bool(a) => format!("bool at {a:#x}"),
        Val::Obj(o) => format!("object {o:#x} {}", web.js.fields(mem, o).iter().take(8).map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(",")),
        Val::Other => "-".into(),
    };
    let mut out = vec![format!(
        "cage {:#x}, scope info map {:#x}, context cell map {:?}, property cell map {:?}, {} scopes, {} contexts, native context {:?} ({} ms)",
        web.js.base(),
        web.scope_map,
        web.cell_map.map(|m| format!("{m:#x}")),
        web.property_map.map(|m| format!("{m:#x}")),
        web.scopes.len(),
        web.contexts.len(),
        web.native.map(|n| format!("{n:#x}")),
        t.elapsed().as_millis()
    )];
    for c in web.roots(mem) {
        let s = &web.scopes[c.scope];
        out.push(format!("{} context {:#x} ({} variables, from slot {})", kinds(s.kind), c.at, s.names.len(), s.first));
        for (i, name) in s.names.iter().enumerate() {
            out.push(format!("  {name} = {}", show(web.variable(mem, c, i))));
        }
    }
    for (name, cell, v) in web.globals(mem) {
        out.push(format!("global {name} (cell {cell:#x}) = {}", show(v)));
    }
    for (key, o) in web.scenes(heap) {
        out.push(format!("Phaser scene {key} at {o:#x}"));
    }
    for g in web.gd_games(heap) {
        out.push(format!("GDevelop game at {g:#x}, scenes {:?}", web.gd_scenes(mem, g).iter().map(|s| s.0.clone()).collect::<Vec<_>>()));
    }
    for r in web.c2_runtimes(heap) {
        out.push(format!("Construct 2 runtime at {r:#x}, global variables {:?}", web.c2_globals(mem, r).map(|(f, v)| (f, v.len()))));
    }
    for (class, o) in web.instances(heap, &web.classes(mem)) {
        out.push(format!("object of {class} at {o:#x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_read_back() {
        for text in ["web:@ship.hp", "web:@omnis[0].hp", "web:score", "web:new:Bt.health", "web:scene:main.health", "web:gdgame._variables._variables.items.Score._value", "web:gdscene:Level%201._variables.x", "web:c2.Vf[3].data", "web:@a%20b.c[2][3].d", "web:@%24game.x"] {
            assert_eq!(WebPath::parse(text).unwrap().text(), text, "{text}");
        }
        let p = WebPath::parse("web:@game.ship.cargo[1]").unwrap();
        assert_eq!(p.root, Root::Var("game".into()));
        assert_eq!(p.steps, vec![Step::Prop("ship".into()), Step::Prop("cargo".into()), Step::Index(1)]);
        assert_eq!(p.shown(), "game.ship.cargo[1]");
        assert_eq!(WebPath::parse("web:new:Bt.health").unwrap().shown(), "new Bt.health");
        assert!(WebPath::parse("web:@").is_none());
        assert!(WebPath::parse("web:@a..b").is_none());
        assert!(WebPath::parse("web:@a[x]").is_none());
        assert!(WebPath::parse("js:@a").is_none());
    }
}
