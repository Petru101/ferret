//! Python games (CPython 2.7 and 3.x: Ren'Py visual novels, pygame games): their objects read
//! from outside. A value is named by the namespace it lives in and the names from there:
//! `py:store#souls` (a Ren'Py game's variable), `py:__main__#player.hp` (a pygame game's
//! global `player`, its attribute `hp`), `py:__main__#party[0].hp` (a list's element).
//!
//! Found by shape, no symbols: the `type` type is its own type and is named "type"; the builtin
//! types (dict, str, int, float, module...) are objects of type `type` with those names;
//! modules are objects of type `module`, each holding its namespace dict (Ren'Py's `store` is a
//! `StoreDict` of its own, found the same way). Layouts by version (CPython's headers 2.7,
//! 3.6-3.14): dicts (2.7 open tables; 3.6-3.10 and 3.11+ compact tables of keys with indices),
//! strings (3.3+ compact: characters after a 40-byte header in 3.12+, 48 before; 2.7: at +36),
//! ints (2.7: a C long at +16; 3.x: 30-bit digits at +24, the size or 3.12's tag at +16), an
//! object's attributes (its dict at `tp_dictoffset`; 3.11+ classes keep a dict or values before
//! or after the object, named by the class's shared keys).
//!
//! Python's numbers are immutable objects: a variable points to one, and `hp -= 1` makes a new
//! one. So a value's place is the slot pointing to its object (the `py` value type), and a write
//! changes the object in place only when nothing else points to it (`ob_refcnt` 1); small ints
//! (-5..256) are shared by the whole game, so the slot is pointed to the cached one instead
//! (3.12+: those are immortal; earlier, their count is raised by one).

use crate::mono::player_like;
use crate::names::Heap;
use crate::ue::{Listed, ListedObject, MAX_LISTED};
use std::collections::{HashMap, HashSet, VecDeque};
use crate::mem::Mem;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Objects looked into for one list at most.
const MAX_VISITED: usize = 20000;
/// How deep the list goes from a namespace.
const MAX_DEPTH: usize = 10;
/// Entries read from one dict, list or tuple at most.
const MAX_ENTRIES: u64 = 1 << 16;
/// Elements listed from one list or tuple at most.
const MAX_ELEMENTS: u64 = 256;

// tp_flags bits.
const HEAPTYPE: u64 = 1 << 9;
const INLINE_VALUES: u64 = 1 << 2;
const MANAGED_DICT: u64 = 1 << 4;
const LIST_SUBCLASS: u64 = 1 << 25;
const TUPLE_SUBCLASS: u64 = 1 << 26;
const DICT_SUBCLASS: u64 = 1 << 29;
// PyTypeObject fields, in words (the same from 2.7 to 3.14: every field before them is a
// pointer or a C long padded to one).
const TP_NAME: u64 = 3;
const TP_BASICSIZE: u64 = 4;
const TP_FLAGS: u64 = 21;
const TP_DICTOFFSET: u64 = 36;

fn word(mem: &Mem, at: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b, at).ok().map(|_| u64::from_le_bytes(b))
}

/// A little-endian number of `n` bytes (4 or 8), sign-extended when `signed`.
fn sized(mem: &Mem, at: u64, n: u64, signed: bool) -> Option<u64> {
    if n == 8 {
        return word(mem, at);
    }
    half(mem, at).map(|v| if signed { v as i32 as i64 as u64 } else { u64::from(v) })
}

fn half(mem: &Mem, at: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    mem.read_exact_at(&mut b, at).ok().map(|_| u32::from_le_bytes(b))
}

fn byte(mem: &Mem, at: u64) -> Option<u8> {
    let mut b = [0u8; 1];
    mem.read_exact_at(&mut b, at).ok().map(|_| b[0])
}

fn cstr(mem: &Mem, at: u64) -> Option<String> {
    if at < 0x10000 {
        return None;
    }
    let mut b = [0u8; 64];
    let n = mem.read_at(&mut b, at).ok()?;
    let end = b[..n].iter().position(|&c| c == 0)?;
    std::str::from_utf8(&b[..end]).ok().map(str::to_owned)
}

/// The version the game's Python reports in its libraries' names (libpython3.12.so,
/// python3.14, libpython2.7.so) or in Ren'Py 8's librenpython.so (its version string).
fn version(pid: u32, regions: &[crate::helper::Region]) -> Option<(u8, u8)> {
    let from_name = |n: &str| {
        let i = n.find("python")? + 6;
        let rest = &n[i..];
        // "2.7.so", "3.14", or Windows' "27.dll", "312.dll".
        let (maj, min): (String, String) = match rest.split_once('.') {
            Some((maj, rest)) if !maj.is_empty() && maj.len() <= 1 => (maj.into(), rest.chars().take_while(|c| c.is_ascii_digit()).collect()),
            _ => {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                (digits.get(..1)?.into(), digits.get(1..)?.into())
            }
        };
        Some((maj.parse().ok()?, min.parse().ok()?))
    };
    let names: Vec<&str> = regions.iter().map(|r| r.path.rsplit('/').next().unwrap_or_default()).collect();
    if let Some(v) = names.iter().find_map(|n| from_name(n).filter(|&(maj, _): &(u8, u8)| maj == 2 || maj == 3)) {
        return Some(v);
    }
    let lib = regions.iter().find(|r| r.path.ends_with("/librenpython.so") || r.path.contains("/librenpython"))?;
    let data = std::fs::read(crate::os::inside(pid, std::path::Path::new(&lib.path))).ok()?;
    // Its paths and module names say "python3.12" (its first "3.x.y" string isn't always the
    // version: Ren'Py 8's had "3.3.x" earlier): the most common minor version wins.
    let mut counts: HashMap<u8, usize> = HashMap::new();
    for i in memchr_all(&data, b"python3.") {
        let minor: String = data[i + 8..].iter().take(2).take_while(|c| c.is_ascii_digit()).map(|&c| c as char).collect();
        if let Ok(m) = minor.parse::<u8>() {
            *counts.entry(m).or_default() += 1;
        }
    }
    counts.into_iter().max_by_key(|&(_, n)| n).map(|(m, _)| (3, m))
}

/// Where a byte string occurs in a buffer.
fn memchr_all(data: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = data[i..].iter().position(|&c| c == needle[0]) {
        let at = i + p;
        if data[at..].starts_with(needle) {
            out.push(at);
        }
        i = at + 1;
    }
    out
}

/// Whether the game runs CPython: a libpython or Ren'Py's librenpython mapped, or a python
/// program.
pub fn runs_python(regions: &[crate::helper::Region], exe: &str) -> bool {
    // python, python3.14; Windows' python27.dll, python312.dll.
    let python = |n: &str| {
        let n = n.strip_suffix(".dll").unwrap_or(n);
        n.strip_prefix("python").is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
    };
    python(&exe.to_ascii_lowercase())
        || regions.iter().any(|r| {
            let n = r.path.rsplit('/').next().unwrap_or_default();
            n.starts_with("libpython2") || n.starts_with("libpython3") || n.starts_with("librenpython") || python(&n.to_ascii_lowercase())
        })
}

/// A dict key: a name or a whole number.
#[derive(Clone, Debug, PartialEq)]
enum Key {
    Name(String),
    Index(i64),
}

/// The builtin types this game's Python has.
#[derive(Default, Debug, Clone, Copy)]
struct Types {
    module: u64,
    dict: u64,
    str_: u64,
    int: u64,
    long: u64,
    float: u64,
    bool_: u64,
    list: u64,
    tuple: u64,
    store: u64,
    /// Python 2's old-style classes' objects (`class Player:` with no base) and those classes.
    instance: u64,
    classobj: u64,
    /// Python 2's text strings (Ren'Py 7 keys its volumes and more by them).
    unicode: u64,
}

/// A located game: its Python's version and types, and its namespaces by name.
pub struct Py {
    pid: u32,
    ver: (u8, u8),
    /// Bytes in a pointer (8; 4 for 32-bit Python 2.7, Ren'Py 7's Windows builds) and in a C
    /// long (4 on Windows and 32-bit: 2.7's ints and string hashes).
    w: u64,
    long: u64,
    t: Types,
    /// Where a compact ASCII string's characters start (40 in 3.12+, 48 in 3.3-3.11).
    str_at: u64,
    roots: Mutex<(HashMap<String, u64>, Instant)>,
    /// Where a heap type keeps `ht_name` (then `ht_slots`, `ht_qualname`, `ht_cached_keys`).
    ht_name_at: Mutex<Option<u64>>,
    /// The cached small ints: the object for 0 and their size when they're consecutive
    /// objects (3.11+), else the table of pointers to them (2.7, 3.9-3.10) with size 0.
    small: Mutex<Option<(u64, u64)>>,
}

/// What a slot holds.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Val {
    Int(i64),
    Float(f64),
    /// A dict (or subclass), list, tuple or an object with attributes.
    Obj(u64),
    Other,
}

impl Py {
    fn locate(heap: &Heap) -> Option<Py> {
        let mem = heap.mem();
        let regions = crate::helper::maps(heap.pid).ok()?;
        let ver = version(heap.pid, &regions)?;
        let w = heap.width() as u64;
        // 32-bit: Python 2.7 only (3.x's dicts differ more than in width).
        if w == 4 && ver.0 != 2 {
            return None;
        }
        let windows = regions.iter().any(|r| r.path.to_ascii_lowercase().ends_with(".dll"));
        let long = if w == 4 || windows { 4 } else { 8 };
        let ptr = |at: u64| sized(mem, at, w, false);
        // The `type` type: its own type, named "type".
        let mut ty = 0;
        let ws = w as usize;
        heap.each_chunk(0, |addr, b, fresh| {
            if ty != 0 {
                return;
            }
            for off in (0..fresh.saturating_sub(2 * ws)).step_by(ws) {
                let mut x = [0u8; 8];
                x[..ws].copy_from_slice(&b[off + ws..off + 2 * ws]);
                let t = u64::from_le_bytes(x);
                if t == addr + off as u64 && ptr(t + TP_NAME * w).and_then(|n| cstr(mem, n)).as_deref() == Some("type") {
                    ty = t;
                    return;
                }
            }
        });
        if ty == 0 {
            return None;
        }
        // The builtin types: objects of type `type` by name, not made by Python code (heap types).
        let mut t = Types::default();
        for r in heap.referrers(&[ty]) {
            let o = r - w;
            let Some(name) = ptr(o + TP_NAME * w).and_then(|n| cstr(mem, n)) else { continue };
            let heap_type = half(mem, o + TP_FLAGS * w).is_some_and(|f| u64::from(f) & HEAPTYPE != 0);
            let slot = match name.as_str() {
                "module" => &mut t.module,
                "dict" => &mut t.dict,
                "str" => &mut t.str_,
                "int" => &mut t.int,
                "long" => &mut t.long,
                "float" => &mut t.float,
                "bool" => &mut t.bool_,
                "list" => &mut t.list,
                "tuple" => &mut t.tuple,
                "StoreDict" => &mut t.store,
                "instance" if ver.0 == 2 => &mut t.instance,
                "classobj" if ver.0 == 2 => &mut t.classobj,
                "unicode" if ver.0 == 2 => &mut t.unicode,
                _ => continue,
            };
            if *slot == 0 && (!heap_type || name == "StoreDict") {
                *slot = o;
            }
        }
        if t.module == 0 || t.dict == 0 || t.str_ == 0 || t.int == 0 || t.float == 0 {
            return None;
        }
        let mut py = Py {
            pid: heap.pid,
            ver,
            w,
            long,
            t,
            str_at: if ver >= (3, 12) { 40 } else { 48 },
            roots: Mutex::new((HashMap::new(), Instant::now())),
            ht_name_at: Mutex::new(None),
            small: Mutex::new(None),
        };
        py.find_roots(heap);
        // The string layout, checked on the names found (a build could differ).
        if py.roots.lock().unwrap().0.is_empty() && ver.0 == 3 {
            py.str_at = if py.str_at == 40 { 48 } else { 40 };
            py.find_roots(heap);
        }
        Some(py)
    }

    /// The namespaces: every module's dict and Ren'Py's store dicts, by their `__name__`.
    fn find_roots(&self, heap: &Heap) {
        let mem = heap.mem();
        let targets: Vec<u64> = [self.t.module, self.t.store].into_iter().filter(|&x| x != 0).collect();
        let mut roots = HashMap::new();
        for r in heap.referrers(&targets) {
            let o = r - self.w;
            let Some(ty) = self.ptr(mem, r) else { continue };
            let d = if ty == self.t.module { self.ptr(mem, o + 2 * self.w).unwrap_or(0) } else { o };
            if d == 0 || !self.is_dict(mem, d) {
                continue;
            }
            let name = self.dict_get(mem, d, &Key::Name("__name__".into())).and_then(|(v, _)| self.text(mem, v));
            if let Some(name) = name {
                roots.entry(name).or_insert(d);
            }
        }
        *self.roots.lock().unwrap() = (roots, Instant::now());
    }

    /// A pointer-sized word (also Py_ssize_t: sizes, counts).
    fn ptr(&self, mem: &Mem, at: u64) -> Option<u64> {
        sized(mem, at, self.w, false)
    }

    fn type_of(&self, mem: &Mem, o: u64) -> Option<u64> {
        self.ptr(mem, o + self.w).filter(|&t| t > 0x10000)
    }

    /// tp_flags: a C long (4 bytes on Windows; no flag is past bit 31).
    fn flags(&self, mem: &Mem, t: u64) -> u64 {
        half(mem, t + TP_FLAGS * self.w).map_or(0, u64::from)
    }

    fn is_dict(&self, mem: &Mem, o: u64) -> bool {
        self.type_of(mem, o).is_some_and(|t| t == self.t.dict || t == self.t.store || self.flags(mem, t) & DICT_SUBCLASS != 0)
    }

    /// A str's text (3.x compact strings; 2.7's byte strings and unicode strings).
    fn text(&self, mem: &Mem, o: u64) -> Option<String> {
        let t = self.type_of(mem, o)?;
        if self.t.unicode != 0 && t == self.t.unicode {
            return self.unicode_text(mem, o);
        }
        if t != self.t.str_ {
            return None;
        }
        let len = self.ptr(mem, o + 2 * self.w)?;
        if len > 256 {
            return None;
        }
        let (at, width) = if self.ver.0 == 2 {
            // {head, ob_size, long ob_shash, int ob_sstate, chars}
            (3 * self.w + self.long + 4, 1)
        } else {
            let state = half(mem, o + 32)?;
            let (kind, compact, ascii) = ((state >> 2) & 7, state >> 5 & 1 == 1, state >> 6 & 1 == 1);
            if !compact {
                return None;
            }
            // Non-ASCII compact strings keep two more fields (utf8 length and pointer).
            (if ascii { self.str_at } else { self.str_at + 16 }, u64::from(kind.max(1)))
        };
        let mut b = vec![0u8; (len * width) as usize];
        mem.read_exact_at(&mut b, o + at).ok()?;
        Some(match width {
            1 => b.iter().map(|&c| c as char).collect(),
            2 => String::from_utf16_lossy(&b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()),
            _ => b.chunks(4).filter_map(|c| char::from_u32(u32::from_le_bytes(c.try_into().unwrap()))).collect(),
        })
    }

    /// Python 2's unicode string: {head, length, Py_UNICODE *str, ...}, characters of 2 bytes
    /// (Windows) or 4 (most Linux builds): 4 tried first, unless they don't make text.
    fn unicode_text(&self, mem: &Mem, o: u64) -> Option<String> {
        let len = self.ptr(mem, o + 2 * self.w)?;
        let at = self.ptr(mem, o + 3 * self.w)?;
        if len > 256 || at == 0 {
            return None;
        }
        let mut b = vec![0u8; 4 * len as usize];
        let n = mem.read_at(&mut b, at).ok()?;
        if n >= b.len() {
            let wide: Option<String> = b.chunks(4).map(|c| char::from_u32(u32::from_le_bytes(c.try_into().unwrap())).filter(|&c| c != '\0')).collect();
            if wide.is_some() {
                return wide;
            }
        }
        b.truncate(2 * len as usize);
        if n < b.len() {
            return None;
        }
        Some(String::from_utf16_lossy(&b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()))
    }

    /// An int's digits (30 bits each) and sign, as the version keeps them.
    fn int_value(&self, mem: &Mem, o: u64, ty: u64) -> Option<i64> {
        let w = self.w;
        if self.ver.0 == 2 && ty == self.t.int {
            return sized(mem, o + 2 * w, self.long, true).map(|v| v as i64);
        }
        let (ndigits, negative) = if self.ver >= (3, 12) {
            let tag = self.ptr(mem, o + 2 * w)?;
            (tag >> 3, tag & 3 == 2)
        } else {
            let size = sized(mem, o + 2 * w, w, true)? as i64;
            (size.unsigned_abs(), size < 0)
        };
        // 32-bit Python 2.7 keeps 15-bit digits in 2 bytes.
        let (bits, bytes) = if w == 4 { (15, 2) } else { (30, 4) };
        if ndigits * bits > 62 {
            return None;
        }
        let mut v: i64 = 0;
        for i in 0..ndigits {
            let d = if bytes == 2 { u32::from(u16::from_le_bytes(self.bytes::<2>(mem, o + 3 * w + 2 * i)?)) } else { half(mem, o + 3 * w + 4 * i)? };
            v |= i64::from(d & ((1 << bits) - 1)) << (bits * i);
        }
        Some(if negative { -v } else { v })
    }

    fn bytes<const N: usize>(&self, mem: &Mem, at: u64) -> Option<[u8; N]> {
        let mut b = [0u8; N];
        mem.read_exact_at(&mut b, at).ok().map(|_| b)
    }

    /// What an object is, for listing.
    fn val(&self, mem: &Mem, o: u64) -> Val {
        let Some(t) = self.type_of(mem, o) else { return Val::Other };
        if t == self.t.float {
            return word(mem, o + 2 * self.w).map_or(Val::Other, |b| Val::Float(f64::from_bits(b)));
        }
        if t == self.t.bool_ {
            return Val::Other;
        }
        if t == self.t.int || (self.t.long != 0 && t == self.t.long) {
            return self.int_value(mem, o, t).map_or(Val::Other, Val::Int);
        }
        let flags = self.flags(mem, t);
        if flags & (DICT_SUBCLASS | LIST_SUBCLASS | TUPLE_SUBCLASS) != 0
            || t == self.t.dict
            || t == self.t.store
            || flags & HEAPTYPE != 0
            || (self.t.instance != 0 && t == self.t.instance)
        {
            return Val::Obj(o);
        }
        Val::Other
    }

    fn key(&self, mem: &Mem, k: u64) -> Option<Key> {
        if let Some(s) = self.text(mem, k) {
            return Some(Key::Name(s));
        }
        match self.val(mem, k) {
            Val::Int(i) => Some(Key::Index(i)),
            _ => None,
        }
    }

    /// A dict's entries: each key with the object it holds and the slot holding it.
    fn dict_entries(&self, mem: &Mem, d: u64) -> Vec<(Key, u64, u64)> {
        let mut out = Vec::new();
        let mut push = |k: u64, slot: u64| {
            if let (Some(v), Some(key)) = (self.ptr(mem, slot).filter(|&v| v != 0), (k != 0).then(|| self.key(mem, k)).flatten()) {
                out.push((key, v, slot));
            }
        };
        if self.ver.0 == 2 {
            // {head, ma_fill, ma_used, ma_mask, ma_table}, entries {hash, key, value}.
            let w = self.w as usize;
            let (Some(mask), Some(table)) = (self.ptr(mem, d + 4 * self.w), self.ptr(mem, d + 5 * self.w)) else { return out };
            let n = (mask + 1).min(MAX_ENTRIES);
            let mut b = vec![0u8; 3 * w * n as usize];
            if mem.read_exact_at(&mut b, table).is_err() {
                return out;
            }
            for i in 0..n as usize {
                let mut x = [0u8; 8];
                x[..w].copy_from_slice(&b[3 * w * i + w..3 * w * i + 2 * w]);
                push(u64::from_le_bytes(x), table + (3 * w * i + 2 * w) as u64);
            }
            return out;
        }
        let (Some(keys), Some(values)) = (word(mem, d + 32), word(mem, d + 40)) else { return out };
        let Some((entries, n, unicode)) = self.keys_layout(mem, keys) else { return out };
        let values = if values != 0 && self.ver >= (3, 13) { values + 8 } else { values };
        let size = if unicode { 16 } else { 24 };
        for i in 0..n {
            let e = entries + size * i;
            let k = word(mem, if unicode { e } else { e + 8 }).unwrap_or(0);
            let slot = if values != 0 { values + 8 * i } else if unicode { e + 8 } else { e + 16 };
            push(k, slot);
        }
        out
    }

    /// A dict keys object's entries (address, count) and whether they're the short kind
    /// {key, value} (3.11+: tables of str keys only; earlier entries are {hash, key, value}).
    fn keys_layout(&self, mem: &Mem, keys: u64) -> Option<(u64, u64, bool)> {
        if self.ver >= (3, 11) {
            let mut b = [0u8; 32];
            mem.read_exact_at(&mut b, keys).ok()?;
            let (log2_index, kind) = (b[9], b[10]);
            let n = u64::from_le_bytes(b[24..32].try_into().unwrap());
            if log2_index > 32 || n > MAX_ENTRIES {
                return None;
            }
            Some((keys + 32 + (1u64 << log2_index), n, kind != 0))
        } else {
            let size = word(mem, keys + 8)?;
            let n = word(mem, keys + 32)?;
            if size > MAX_ENTRIES * 2 || n > MAX_ENTRIES {
                return None;
            }
            let ix = match size {
                0..=0xff => 1,
                0x100..=0xffff => 2,
                0x1_0000..=0xffff_ffff => 4,
                _ => 8,
            };
            Some((keys + 40 + size * ix, n, false))
        }
    }

    fn dict_get(&self, mem: &Mem, d: u64, key: &Key) -> Option<(u64, u64)> {
        self.dict_entries(mem, d).into_iter().find(|(k, _, _)| k == key).map(|(_, v, s)| (v, s))
    }

    /// Where a heap type keeps its shared keys (`ht_cached_keys`): after `ht_name` (a str
    /// equal to the class's name), `ht_slots` and `ht_qualname`.
    fn cached_keys(&self, mem: &Mem, t: u64) -> Option<u64> {
        let mut at = self.ht_name_at.lock().unwrap();
        if at.is_none() {
            let name = word(mem, t + TP_NAME * self.w).and_then(|n| cstr(mem, n))?;
            let short = name.rsplit('.').next().unwrap_or(&name).to_owned();
            *at = (400..1600).step_by(8).find(|&o| word(mem, t + o).and_then(|s| self.text(mem, s)).as_deref() == Some(short.as_str()));
        }
        word(mem, t + (*at)? + 24).filter(|&k| k != 0)
    }

    /// An object's attributes (its dict's entries, or its values named by its class's keys).
    fn attrs(&self, mem: &Mem, o: u64) -> Vec<(Key, u64, u64)> {
        let Some(t) = self.type_of(mem, o) else { return Vec::new() };
        let flags = self.flags(mem, t);
        let split = |values: u64| {
            let Some(keys) = self.cached_keys(mem, t) else { return Vec::new() };
            let Some((entries, n, unicode)) = self.keys_layout(mem, keys) else { return Vec::new() };
            let size = if unicode { 16 } else { 24 };
            (0..n)
                .filter_map(|i| {
                    let k = word(mem, entries + size * i + if unicode { 0 } else { 8 })?;
                    let slot = values + 8 * i;
                    let v = word(mem, slot).filter(|&v| v != 0)?;
                    Some((self.key(mem, k)?, v, slot))
                })
                .collect()
        };
        if self.ver >= (3, 11) && flags & MANAGED_DICT != 0 {
            let w = word(mem, o.wrapping_sub(24)).unwrap_or(0);
            return match self.ver {
                (3, 11) if w != 0 => self.dict_entries(mem, w),
                (3, 11) => word(mem, o.wrapping_sub(32)).filter(|&v| v != 0).map(split).unwrap_or_default(),
                (3, 12) if w & 1 == 1 => split(w + 1),
                (3, 12) => if w != 0 { self.dict_entries(mem, w) } else { Vec::new() },
                _ if w != 0 => self.dict_entries(mem, w),
                _ if flags & INLINE_VALUES != 0 => {
                    let values = o + word(mem, t + TP_BASICSIZE * self.w).unwrap_or(0);
                    if byte(mem, values + 3) == Some(1) { split(values + 8) } else { Vec::new() }
                }
                _ => Vec::new(),
            };
        }
        // Python 2's old-style objects: {head, in_class, in_dict}.
        if self.t.instance != 0 && t == self.t.instance {
            return self.ptr(mem, o + 3 * self.w).filter(|&d| self.is_dict(mem, d)).map(|d| self.dict_entries(mem, d)).unwrap_or_default();
        }
        let off = sized(mem, t + TP_DICTOFFSET * self.w, self.w, true).unwrap_or(0) as i64;
        if off > 0 {
            if let Some(d) = self.ptr(mem, o + off as u64).filter(|&d| d != 0 && self.is_dict(mem, d)) {
                return self.dict_entries(mem, d);
            }
        }
        Vec::new()
    }

    /// A list's or tuple's elements: index, object and slot.
    fn elements(&self, mem: &Mem, o: u64) -> Vec<(i64, u64, u64)> {
        let Some(t) = self.type_of(mem, o) else { return Vec::new() };
        let flags = self.flags(mem, t);
        let w = self.w;
        let Some(n) = self.ptr(mem, o + 2 * w).filter(|&n| n <= MAX_ENTRIES) else { return Vec::new() };
        let items = if flags & LIST_SUBCLASS != 0 || t == self.t.list {
            self.ptr(mem, o + 3 * w).unwrap_or(0)
        } else if flags & TUPLE_SUBCLASS != 0 || t == self.t.tuple {
            o + 3 * w
        } else {
            return Vec::new();
        };
        if items == 0 {
            return Vec::new();
        }
        (0..n.min(MAX_ELEMENTS)).filter_map(|i| Some((i as i64, self.ptr(mem, items + w * i).filter(|&v| v != 0)?, items + w * i))).collect()
    }

    /// An object's entries by name or index: a dict's, a list's or an object's attributes.
    fn children(&self, mem: &Mem, o: u64) -> Vec<(Key, u64, u64)> {
        if self.is_dict(mem, o) {
            return self.dict_entries(mem, o);
        }
        let el = self.elements(mem, o);
        if !el.is_empty() {
            return el.into_iter().map(|(i, v, s)| (Key::Index(i), v, s)).collect();
        }
        self.attrs(mem, o)
    }

    fn type_name(&self, mem: &Mem, o: u64) -> String {
        let Some(t) = self.type_of(mem, o) else { return String::new() };
        // An old-style object: its class's name ({head, cl_bases, cl_dict, cl_name}).
        if self.t.instance != 0 && t == self.t.instance {
            let class = self.ptr(mem, o + 2 * self.w).filter(|&c| self.type_of(mem, c) == Some(self.t.classobj));
            if let Some(name) = class.and_then(|c| self.ptr(mem, c + 4 * self.w)).and_then(|n| self.text(mem, n)) {
                return name;
            }
        }
        self.ptr(mem, t + TP_NAME * self.w).and_then(|n| cstr(mem, n)).unwrap_or_default()
    }

    /// Reference count: the low 32 bits in 3.12+ (the rest are flags), the whole word before.
    fn refcount(&self, mem: &Mem, o: u64) -> Option<u64> {
        let w = self.ptr(mem, o)?;
        Some(if self.ver >= (3, 12) { w & 0xffff_ffff } else { w })
    }

    /// Writes a pointer-sized word.
    fn put(&self, mem: &Mem, at: u64, v: u64) -> std::io::Result<()> {
        mem.write_all_at(&v.to_le_bytes()[..self.w as usize], at)
    }

    /// The cached small int for `v` (-5..256), found from any small int's place: the int
    /// objects next to the one for 0 (3.11+: one array), else the interpreter's table of
    /// pointers to them (2.7 takes them from blocks of ints, 3.9-3.10 allocates each).
    fn small_int(&self, mem: &Mem, v: i64, near: Option<u64>) -> Option<u64> {
        if !(-5..=256).contains(&v) {
            return None;
        }
        let is = |at: u64, v: i64| self.type_of(mem, at).is_some_and(|t| t == self.t.int && self.int_value(mem, at, t) == Some(v));
        let mut small = self.small.lock().unwrap();
        if small.is_none() {
            let o = near?;
            let have = self.type_of(mem, o).and_then(|t| self.int_value(mem, o, t)).filter(|h| (-5..=256).contains(h))?;
            let size = if self.ver.0 == 2 { 2 * self.w + self.long.max(self.w) } else { 32 };
            let zero = o.wrapping_add_signed(-have * size as i64);
            let checks = [-5, 0, 1, 7, 100, 256];
            if checks.iter().all(|&k| is(zero.wrapping_add_signed(k * size as i64), k)) {
                *small = Some((zero, size));
            } else {
                // The table: a pointer to `o` with pointers to the others around it.
                let heap = Heap::new(self.pid, mem, self.w as usize).ok()?;
                let table = heap.referrers(&[o]).into_iter().map(|r| r - (have + 5) as u64 * self.w).find(|&t| {
                    checks.iter().all(|&k| self.ptr(mem, t + (k + 5) as u64 * self.w).is_some_and(|p| is(p, k)))
                })?;
                *small = Some((table, 0));
            }
        }
        let (base, size) = (*small)?;
        let at = if size == 0 { self.ptr(mem, base + (v + 5) as u64 * self.w)? } else { base.wrapping_add_signed(v * size as i64) };
        is(at, v).then_some(at)
    }
}

/// The located game per pid (one game is attached at a time: the `py` value type reads through
/// the last one); a failed search is tried again after `RETRY`.
pub fn shared(heap: &Heap) -> Option<Arc<Py>> {
    const RETRY: Duration = Duration::from_secs(10);
    let mut found = FOUND.lock().unwrap();
    if let Some((pid, py, at)) = found.as_ref() {
        if *pid == heap.pid && (py.is_some() || at.elapsed() < RETRY) {
            return py.clone();
        }
    }
    let py = Py::locate(heap).map(Arc::new);
    *found = Some((heap.pid, py.clone(), Instant::now()));
    py
}

static FOUND: Mutex<Option<(u32, Option<Arc<Py>>, Instant)>> = Mutex::new(None);

fn current() -> Option<Arc<Py>> {
    FOUND.lock().unwrap().as_ref().and_then(|(_, py, _)| py.clone())
}

/// The number a slot's object holds now (the `py` value type).
pub fn read_slot(mem: &Mem, slot: u64) -> Option<f64> {
    let py = current()?;
    match py.val(mem, py.ptr(mem, slot)?) {
        Val::Int(i) => Some(i as f64),
        Val::Float(f) => Some(f),
        _ => None,
    }
}

/// Writes a number to the object a slot points to: in place when only that slot points to
/// it; a small int by pointing the slot to the cached one; else refused (the object is shared:
/// another variable, a constant of the game's code, Ren'Py's rollback log).
pub fn write_slot(mem: &Mem, slot: u64, v: f64) -> std::io::Result<()> {
    let refused = |why: &str| Err(std::io::Error::other(why.to_owned()));
    let Some(py) = current() else { return refused("not a Python game") };
    let Some(o) = py.ptr(mem, slot) else { return refused("unreadable") };
    let Some(t) = py.type_of(mem, o) else { return refused("unreadable") };
    let own = py.refcount(mem, o) == Some(1);
    if t == py.t.float {
        if !own {
            return refused("the game shares this number with something else, not written");
        }
        return mem.write_all_at(&v.to_bits().to_le_bytes(), o + 2 * py.w);
    }
    let is_int = t == py.t.int || (py.t.long != 0 && t == py.t.long);
    let Some(have) = is_int.then(|| py.int_value(mem, o, t)).flatten() else { return refused("not a Python number now") };
    let v = v.round() as i64;
    if v == have {
        return Ok(());
    }
    if own && !(-5..=256).contains(&have) {
        if py.ver.0 == 2 && t == py.t.int {
            if py.long == 4 && i32::try_from(v).is_err() {
                return refused("the number doesn't fit the game's number object, not written");
            }
            return mem.write_all_at(&v.to_le_bytes()[..py.long as usize], o + 2 * py.w);
        }
        if py.w == 4 {
            return refused("not written (a long number of 32-bit Python)");
        }
        // 30-bit digits, as many as the object has room for (a value never grows it).
        let ndigits = if py.ver >= (3, 12) { word(mem, o + 16).unwrap_or(0) >> 3 } else { (word(mem, o + 16).unwrap_or(0) as i64).unsigned_abs() };
        let mag = v.unsigned_abs();
        let need = match mag {
            0 => 0,
            m if m < 1 << 30 => 1,
            m if m < 1 << 60 => 2,
            _ => 3,
        };
        if need > ndigits {
            return refused("the number doesn't fit the game's number object, not written");
        }
        for i in 0..need {
            mem.write_all_at(&(((mag >> (30 * i)) & 0x3fff_ffff) as u32).to_le_bytes(), o + 24 + 4 * i)?;
        }
        let head = if py.ver >= (3, 12) {
            (need << 3) | if v == 0 { 1 } else if v < 0 { 2 } else { 0 }
        } else {
            (if v < 0 { -(need as i64) } else { need as i64 }) as u64
        };
        return mem.write_all_at(&head.to_le_bytes(), o + 16);
    }
    // Shared (small ints, constants): point the slot to the cached small int.
    let Some(target) = py.small_int(mem, v, (-5..=256).contains(&have).then_some(o)) else {
        return refused("the game shares this number object (a small number or a constant of its code): only -5 to 256 can be set now; it can be set once the game changes it");
    };
    if py.ver < (3, 12) {
        // Not immortal before 3.12: the slot's new reference is counted (a small int is held
        // by thousands, a count one off never frees it).
        let Some(n) = py.ptr(mem, target).filter(|&n| n > 100) else { return refused("not written") };
        py.put(mem, target, n + 1)?;
    }
    py.put(mem, slot, target)
}

/// One step of a path: a name (a dict key or an attribute) or an index (a list's element or a
/// dict's int key).
#[derive(Clone, Debug, PartialEq)]
enum Step {
    Name(String),
    Index(i64),
}

/// A saved path from a namespace to a number.
#[derive(Clone, Debug, PartialEq)]
pub struct PyPath {
    root: String,
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

impl PyPath {
    /// "py:<namespace>#<name>[.<name>|[i]]...".
    pub fn parse(text: &str) -> Option<PyPath> {
        let (root, mut rest) = text.strip_prefix("py:")?.split_once('#')?;
        if root.is_empty() || rest.is_empty() {
            return None;
        }
        let mut steps = Vec::new();
        let mut first = true;
        while !rest.is_empty() {
            let (sep, body) = match rest.as_bytes()[0] {
                b'.' | b'[' => (rest.as_bytes()[0], &rest[1..]),
                _ if first => (b'.', rest),
                _ => return None,
            };
            first = false;
            if sep == b'[' {
                let (n, after) = body.split_once(']')?;
                steps.push(Step::Index(n.parse().ok()?));
                rest = after;
                continue;
            }
            let end = body.find(['.', '[']).unwrap_or(body.len());
            let name = decode(&body[..end])?;
            if name.is_empty() {
                return None;
            }
            steps.push(Step::Name(name));
            rest = &body[end..];
        }
        Some(PyPath { root: root.to_owned(), steps })
    }

    pub fn text(&self) -> String {
        format!("py:{}#{}", self.root, self.steps_text(true))
    }

    fn steps_text(&self, encoded: bool) -> String {
        let mut t = String::new();
        for s in &self.steps {
            match s {
                Step::Name(n) => {
                    if !t.is_empty() {
                        t.push('.');
                    }
                    t += &if encoded { encode(n) } else { n.clone() };
                }
                Step::Index(i) => t += &format!("[{i}]"),
            }
        }
        t
    }

    /// As the list shows it: `player.hp`, a module's other than the game's own with its
    /// module's name (`settings.WIDTH`).
    fn shown(&self) -> String {
        let s = self.steps_text(false);
        match (self.root.as_str(), s.is_empty()) {
            ("__main__" | "store", _) => s,
            (r, true) => r.to_owned(),
            (r, false) => format!("{r}.{s}"),
        }
    }

    pub fn describe(&self) -> String {
        match self.root.as_str() {
            "store" => format!("{} among the game's Ren'Py variables", self.shown()),
            _ => format!("{} in the game's Python objects", self.shown()),
        }
    }

    fn then(&self, s: Step) -> PyPath {
        let mut p = self.clone();
        p.steps.push(s);
        p
    }

    fn last_name(&self) -> Option<&str> {
        self.steps.iter().rev().find_map(|s| match s {
            Step::Name(n) => Some(n.as_str()),
            _ => None,
        })
    }

    /// The path up to its first field named like the player itself.
    fn player_part(&self) -> Option<PyPath> {
        let i = self.steps.iter().position(|s| matches!(s, Step::Name(n) if the_player(n)))?;
        Some(PyPath { root: self.root.clone(), steps: self.steps[..=i].to_vec() })
    }

    /// Through how something looks or sounds (`player.image`, `player.rect`).
    fn presentation(&self) -> bool {
        const WORDS: &[&str] = &["sprite", "image", "surface", "rect", "font", "sound", "music", "anim", "frame", "texture", "screen", "mask", "particle", "camera", "clock"];
        self.steps.iter().any(|s| matches!(s, Step::Name(n) if WORDS.iter().any(|w| n.to_ascii_lowercase().contains(w))))
    }
}

/// Whether a name is the player itself (`player`, `hero`, `self_player`...): its whole name.
fn the_player(name: &str) -> bool {
    let n: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    ["player", "hero", "localplayer", "theplayer", "mainplayer", "playerchar", "playercharacter", "character", "maincharacter", "avatar", "protagonist", "mc", "pc", "ship", "playership"].contains(&n.as_str())
}

/// Constants by convention (`WIDTH`, `MAX_HP`).
fn constant(name: &str) -> bool {
    name.len() > 1 && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && name.chars().any(|c| c.is_ascii_uppercase())
}

/// The namespace the path starts from.
pub fn find_roots(heap: &Heap, path: &PyPath) -> Vec<u64> {
    let Some(py) = shared(heap) else { return Vec::new() };
    let mem = heap.mem();
    let found = py.roots.lock().unwrap().0.get(&path.root).copied();
    if let Some(d) = found.filter(|&d| py.is_dict(mem, d)) {
        return vec![d];
    }
    // Modules are imported as the game goes: look again (at most every 2 s).
    if py.roots.lock().unwrap().1.elapsed() >= Duration::from_secs(2) {
        py.find_roots(heap);
    }
    let found = py.roots.lock().unwrap().0.get(&path.root).copied();
    found.into_iter().collect()
}

/// Where the path leads: the slot pointing to the number, while it holds one.
pub fn walk(heap: &Heap, roots: &[u64], path: &PyPath) -> Vec<u64> {
    let Some(py) = shared(heap) else { return Vec::new() };
    let mem = heap.mem();
    roots
        .iter()
        .filter(|&&d| py.is_dict(mem, d))
        .filter_map(|&d| {
            let mut cur = d;
            let mut slot = 0;
            for s in &path.steps {
                let key = match s {
                    Step::Name(n) => Key::Name(n.clone()),
                    Step::Index(i) => Key::Index(*i),
                };
                let (v, at) = py.children(mem, cur).into_iter().find(|(k, _, _)| *k == key).map(|(_, v, s)| (v, s))?;
                cur = v;
                slot = at;
            }
            matches!(py.val(mem, cur), Val::Int(_) | Val::Float(_)).then_some(slot)
        })
        .collect()
}

/// A namespace or object whose numbers are listed.
struct Group {
    path: PyPath,
    class: String,
    numbers: Vec<(String, u64, PyPath)>,
}

/// Namespaces listed: the game's (`__main__`, Ren'Py's `store`, modules with no file or one
/// outside Python's and Ren'Py's own folders), not Python's or the engine's.
fn game_roots(py: &Py, mem: &Mem) -> Vec<(String, u64)> {
    let roots = py.roots.lock().unwrap().0.clone();
    let mut out: Vec<(String, u64)> = roots
        .into_iter()
        .filter(|(name, d)| {
            if name == "__main__" || name == "store" {
                return true;
            }
            if name.starts_with("store.") {
                return !name[6..].starts_with('_') && !["gui", "layeredimage", "updater", "iap", "icon", "achievement", "director", "build", "config"].contains(&&name[6..]);
            }
            if name.starts_with('_') || name.starts_with("renpy") || name.starts_with("pygame") || name.starts_with("encodings") {
                return false;
            }
            // Windows' paths with forward slashes.
            let file = py.dict_get(mem, *d, &Key::Name("__file__".into())).and_then(|(v, _)| py.text(mem, v)).map(|f| f.replace('\\', "/").to_ascii_lowercase());
            match file {
                None => false,
                Some(f) => {
                    // Python's own library: lib/python2.7/ (Linux), lib/windows-i686/lib/ (Ren'Py
                    // 7 on Windows), C:/Python27/Lib/.
                    let stdlib = f.rsplit_once("/lib/").is_some_and(|(before, _)| {
                        let dir = before.rsplit('/').next().unwrap_or_default();
                        dir.starts_with("python") || dir.starts_with("windows-") || dir.starts_with("py2-") || dir.starts_with("py3-")
                    });
                    !stdlib && !["/lib/python", "site-packages", "dist-packages", "/lib/py", "pythonlib", "/renpy/", ".zip/", "/usr/lib"].iter().any(|p| f.contains(p))
                }
            }
        })
        .collect();
    out.sort_by_key(|(n, _)| (n != "store" && n != "__main__", n.clone()));
    out
}

/// Every number reachable from the game's namespaces, breadth first (paths through the player
/// first: `game_roots`' `player` before its UI), each object by its shortest path.
fn groups(heap: &Heap) -> Option<Vec<Group>> {
    let py = shared(heap)?;
    let mem = heap.mem();
    if py.roots.lock().unwrap().1.elapsed() >= Duration::from_secs(2) {
        py.find_roots(heap);
    }
    let mut queues: [VecDeque<(u64, PyPath, usize)>; 2] = [VecDeque::new(), VecDeque::new()];
    let queue_of = |p: &PyPath| usize::from(p.player_part().is_none());
    for (name, d) in game_roots(&py, mem) {
        queues[1].push_back((d, PyPath { root: name, steps: Vec::new() }, 0));
    }
    let mut seen: HashSet<u64> = HashSet::new();
    let mut out = Vec::new();
    let (mut visited, mut listed) = (0, 0);
    while let Some((o, path, depth)) = queues[0].pop_front().or_else(|| queues[1].pop_front()) {
        if !seen.insert(o) {
            continue;
        }
        visited += 1;
        if visited > MAX_VISITED || listed > MAX_LISTED {
            break;
        }
        let mut g = Group { path: path.clone(), class: py.type_name(mem, o), numbers: Vec::new() };
        for (k, v, slot) in py.children(mem, o) {
            let step = match &k {
                Key::Name(n) if n.starts_with("__") => continue,
                Key::Name(n) => Step::Name(n.clone()),
                Key::Index(i) => Step::Index(*i),
            };
            match py.val(mem, v) {
                Val::Int(_) | Val::Float(_) => {
                    let label = match &k {
                        Key::Name(n) => n.clone(),
                        Key::Index(i) => format!("[{i}]"),
                    };
                    g.numbers.push((label, slot, path.then(step)));
                }
                Val::Obj(x) if depth < MAX_DEPTH && !seen.contains(&x) => {
                    let p = path.then(step);
                    queues[queue_of(&p)].push_back((x, p, depth + 1));
                }
                _ => {}
            }
        }
        if !g.numbers.is_empty() {
            listed += g.numbers.len();
            out.push(g);
        }
    }
    Some(out)
}

/// The Browse tab's list for a Python game: the game's variables (Ren'Py's store, a pygame
/// game's globals), the player's object and what's in it, then (`more`) everything else.
pub fn player(heap: &Heap) -> Option<Vec<ListedObject>> {
    let groups = groups(heap)?;
    let mut out = Vec::new();
    for g in groups {
        let fields = |nums: Vec<(String, u64, PyPath)>| nums.into_iter().map(|(label, addr, _)| Listed { label, addr, kind: "py" }).collect::<Vec<_>>();
        if g.path.steps.is_empty() {
            let word = match g.path.root.as_str() {
                "store" => Some("@variables"),
                "__main__" => Some("@global"),
                _ => None,
            };
            let (consts, vars): (Vec<_>, Vec<_>) = g.numbers.into_iter().partition(|n| constant(&n.0));
            let name = word.map_or(g.path.root.clone(), str::to_owned);
            out.push(ListedObject { name: name.clone(), class: "module".into(), role: "game", owner: None, more: word.is_none(), fields: fields(vars) });
            out.push(ListedObject { name: format!("{} ({})", g.path.root, "constants"), class: "module".into(), role: "game", owner: None, more: true, fields: fields(consts) });
            continue;
        }
        let player = (g.path.last_name().is_some_and(the_player) || the_player(&g.class)) && !g.path.presentation();
        let owner = g.path.player_part().filter(|p| !player && *p != g.path && !g.path.presentation()).map(|p| p.shown());
        let main = player || owner.is_some() || g.numbers.iter().any(|n| player_like(&n.0) && !constant(&n.0)) && g.path.steps.len() == 1;
        out.push(ListedObject {
            name: g.path.shown(),
            class: g.class,
            role: if player { "character" } else if owner.is_some() { "part" } else { "game" },
            owner,
            more: !main,
            fields: fields(g.numbers),
        });
    }
    out.retain(|o| !o.fields.is_empty());
    out.sort_by_key(|o| (o.more, o.role != "character" && o.class != "module"));
    Some(out)
}

/// The path to a number the list has (its slot's address), checked by following it.
pub fn discover(heap: &Heap, target: u64) -> Option<(PyPath, Vec<u64>)> {
    let path = groups(heap)?.into_iter().flat_map(|g| g.numbers).find(|n| n.1 == target)?.2;
    let leads = walk(heap, &find_roots(heap, &path), &path);
    leads.contains(&target).then_some((path, leads))
}

/// pydump [path]: the game's Python as Ferret reads it (version, types, namespaces), or what a
/// path leads to (a namespace's or object's entries).
pub fn diagnose(heap: &Heap, at: &str) -> Vec<String> {
    let t = Instant::now();
    let Some(py) = shared(heap) else { return vec!["no Python objects found (only CPython 2.7 and 3.x, 64-bit; 2.7 also 32-bit)".into()] };
    let mem = heap.mem();
    let roots = py.roots.lock().unwrap().0.clone();
    let mut out = vec![format!("Python {}.{} ({}-bit, long {}), {} namespaces, str at +{}, types {:x?} ({} ms)", py.ver.0, py.ver.1, py.w * 8, py.long, roots.len(), py.str_at, py.t, t.elapsed().as_millis())];
    let _ = py.pid;
    if at.is_empty() {
        let mut names: Vec<String> = game_roots(&py, mem).into_iter().map(|(n, _)| n).collect();
        names.truncate(80);
        out.push(format!("game namespaces: {}", names.join(" ")));
        return out;
    }
    let text = if at.contains('#') { format!("py:{at}") } else { format!("py:{at}#x") };
    let Some(mut p) = PyPath::parse(&text) else { return vec!["bad path".into()] };
    if !at.contains('#') {
        p.steps.clear();
    }
    let Some(&root) = roots.get(&p.root) else { return vec![format!("{}: no such namespace", p.root)] };
    let mut cur = root;
    for s in &p.steps {
        let key = match s {
            Step::Name(n) => Key::Name(n.clone()),
            Step::Index(i) => Key::Index(*i),
        };
        let Some((_, v, _)) = py.children(mem, cur).into_iter().find(|(k, _, _)| *k == key) else { return vec![format!("{at}: not found")] };
        cur = v;
    }
    out.push(format!("{} at {cur:#x}{}", py.type_name(mem, cur), py.text(mem, cur).map(|t| format!(" = {t:?}")).unwrap_or_default()));
    for (k, v, slot) in py.children(mem, cur) {
        let shown = match py.val(mem, v) {
            Val::Int(i) => format!("{i} (refs {})", py.refcount(mem, v).unwrap_or(0)),
            Val::Float(f) => format!("{f} (refs {})", py.refcount(mem, v).unwrap_or(0)),
            _ => py.text(mem, v).map(|s| format!("{s:?}")).unwrap_or_else(|| py.type_name(mem, v)),
        };
        out.push(format!("{k:?} = {shown} @{slot:#x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_read_back() {
        for text in ["py:store#souls", "py:__main__#player.hp", "py:__main__#party[0].hp", "py:game.state#a%20b.c", "py:__main__#grid[1][2]"] {
            assert_eq!(PyPath::parse(text).unwrap().text(), text, "{text}");
        }
        let p = PyPath::parse("py:__main__#party[2].stats.hp").unwrap();
        assert_eq!(p.steps, vec![Step::Name("party".into()), Step::Index(2), Step::Name("stats".into()), Step::Name("hp".into())]);
        assert_eq!(p.shown(), "party[2].stats.hp");
        assert_eq!(PyPath::parse("py:settings#WIDTH").unwrap().shown(), "settings.WIDTH");
        assert!(PyPath::parse("py:store").is_none());
        assert!(PyPath::parse("py:store#").is_none());
        assert!(PyPath::parse("py:#x").is_none());
        assert!(PyPath::parse("py:store#a..b").is_none());
        assert!(PyPath::parse("py:store#a[x]").is_none());
    }

    #[test]
    fn python_libraries() {
        let lib = |path: &str| crate::helper::Region { start: 0, end: 0, perms: "r--p".into(), path: path.into() };
        assert!(runs_python(&[lib("C:/games/tv/lib/windows-i686/python27.dll")], "tunnelvision.exe"));
        assert!(runs_python(&[lib("C:/Python312/PYTHON312.DLL")], "game.exe"));
        assert!(runs_python(&[lib("/usr/lib/libpython3.14.so.1.0")], "game"));
        assert!(runs_python(&[], "python3.14"));
        assert!(!runs_python(&[lib("C:/games/x/pythonnet.dll")], "game.exe"));
    }

    #[test]
    fn names() {
        assert!(the_player("player") && the_player("Hero") && the_player("main_character") && !the_player("players_score"));
        assert!(constant("WIDTH") && !constant("hp"));
    }
}
