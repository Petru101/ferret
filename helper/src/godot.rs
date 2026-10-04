// Godot dictionaries: a value kept as an entry of a Dictionary the game's scripts fill
// (Lumencraft's inventory stacks are {"id": 0, "amount": 2, "data": null, "index": 6}). The
// game re-creates such entries as it runs, in memory allocated at run time, so neither code
// patterns (the GDScript interpreter is shared code) nor pointer paths find them again; their
// keys do.
// Memory (64-bit Linux builds): a Dictionary keeps its entries in a list whose elements are
// { const Variant *key; Variant value; Element *next, *prev; void *list }, a Variant is
// { u32 type; 4 bytes of padding (not always zero); 16 bytes of data }, type 2 = int (an int64),
// 3 = real (a double),
// 4 = String (data = pointer to its characters, UTF-32: wchar_t is 4 bytes on Linux).
// Godot 4 keeps the entries in a HashMap whose elements are { Element *next, *prev; Variant key;
// Variant value }, in insertion order; keys are Strings (4) or StringNames (21: data = pointer
// to { u32 refcount, u32 static count, then its String at +8 (4.5+), else a C string there and
// the String at +0x10 }). Strings are UTF-32 on every platform.
// Text form: {amount|id=0,index} = the "amount" entry of every dictionary that also has an
// "id" entry holding 0 and an "index" entry.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;

use crate::names::Heap;

const INT: u32 = 2;
const REAL: u32 = 3;
const STRING: u32 = 4;
const STRING_NAME: u32 = 21;
/// Bytes of an element read when searching memory.
const ELEMENT: usize = 0x38;
const MAX_KEY: usize = 64;
/// Entries per dictionary followed: a misread link can't make a walk take forever.
const MAX_ENTRIES: usize = 64;
/// A path leading to more places than this is too loose to write through.
const MAX_PLACES: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub struct DictPath {
    key: String,
    /// The dictionary's other entries: the key, and the int it must hold when given.
    with: Vec<(String, Option<i64>)>,
}

/// Keys go into the text as they are: only those that keep a path one word.
fn good_key(k: &str) -> bool {
    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl DictPath {
    pub fn parse(text: &str) -> Option<Self> {
        let inner = text.strip_prefix('{')?.strip_suffix('}')?;
        let (key, rest) = inner.split_once('|').unwrap_or((inner, ""));
        let mut with = Vec::new();
        for w in rest.split(',').filter(|w| !w.is_empty()) {
            let (k, v) = match w.split_once('=') {
                Some((k, v)) => (k, Some(v.parse().ok()?)),
                None => (w, None),
            };
            if !good_key(k) {
                return None;
            }
            with.push((k.to_owned(), v));
        }
        good_key(key).then(|| DictPath { key: key.to_owned(), with })
    }

    pub fn text(&self) -> String {
        let with: Vec<String> = self
            .with
            .iter()
            .map(|(k, v)| match v {
                Some(v) => format!("{k}={v}"),
                None => k.clone(),
            })
            .collect();
        format!("{{{}|{}}}", self.key, with.join(","))
    }

    /// The same path whatever slot the item is in.
    fn any_slot(&self) -> DictPath {
        let with = self.with.iter().map(|(k, v)| (k.clone(), if slot_key(k) { None } else { *v })).collect();
        DictPath { key: self.key.clone(), with }
    }

    fn has_slot(&self) -> bool {
        self.with.iter().any(|(k, v)| v.is_some() && slot_key(k))
    }

    /// In words: ""amount" where "id" is 0".
    pub fn describe(&self) -> String {
        let held: Vec<String> = self.with.iter().filter_map(|(k, v)| Some(format!("\"{k}\" is {}", (*v)?))).collect();
        match held.is_empty() {
            true => format!("\"{}\" in the game's dictionaries", self.key),
            false => format!("\"{}\" where {}", self.key, held.join(" and ")),
        }
    }
}

/// How a dictionary's elements are laid out.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Shape {
    V3,
    V4,
}

const SHAPES: [Shape; 2] = [Shape::V3, Shape::V4];

impl Shape {
    /// Offsets in an element: the value's data, the links.
    fn value(self) -> u64 {
        match self {
            Shape::V3 => 16,
            Shape::V4 => 0x30,
        }
    }

    fn next(self) -> u64 {
        match self {
            Shape::V3 => 32,
            Shape::V4 => 0,
        }
    }

    fn prev(self) -> u64 {
        match self {
            Shape::V3 => 40,
            Shape::V4 => 8,
        }
    }
}

/// Reads the game's dictionaries; keys are cached by the address of their characters (a key's
/// text is shared by every dictionary that has it).
struct Dicts<'a> {
    heap: &'a Heap<'a>,
    keys: RefCell<HashMap<u64, Option<String>>>,
}

impl<'a> Dicts<'a> {
    fn new(heap: &'a Heap<'a>) -> Self {
        Dicts { heap, keys: RefCell::default() }
    }

    fn mem(&self) -> &File {
        self.heap.file()
    }

    fn u32_at(&self, at: u64) -> Option<u32> {
        let mut b = [0u8; 4];
        self.mem().read_exact_at(&mut b, at).ok()?;
        Some(u32::from_le_bytes(b))
    }

    fn u64_at(&self, at: u64) -> Option<u64> {
        let mut b = [0u8; 8];
        self.mem().read_exact_at(&mut b, at).ok()?;
        Some(u64::from_le_bytes(b))
    }

    fn link(&self, p: u64) -> bool {
        p == 0 || self.heap.is_pointer(p)
    }

    /// Looks like an element with a number for its value (checked on the words already read):
    /// `key` is the key's pointer (Godot 3) or its Variant type (Godot 4).
    fn shaped(&self, shape: Shape, key: u64, value_type: u32, next: u64, prev: u64) -> bool {
        let key_ok = match shape {
            Shape::V3 => self.heap.is_pointer(key),
            Shape::V4 => key == STRING as u64 || key == STRING_NAME as u64,
        };
        (value_type == INT || value_type == REAL) && key_ok && (next != 0 || prev != 0) && self.link(next) && self.link(prev)
    }

    fn is(&self, e: u64, shape: Shape) -> bool {
        let key = match shape {
            Shape::V3 => self.u64_at(e),
            Shape::V4 => self.u32_at(e + 0x10).map(u64::from),
        };
        let (Some(key), Some(t), Some(next), Some(prev)) =
            (key, self.u32_at(e + shape.value() - 8), self.u64_at(e + shape.next()), self.u64_at(e + shape.prev()))
        else {
            return false;
        };
        self.shaped(shape, key, t, next, prev)
    }

    /// The shape of the element at `e`, when it is one.
    fn element(&self, e: u64) -> Option<Shape> {
        SHAPES.into_iter().find(|&shape| self.is(e, shape))
    }

    /// The text of the String or StringName key of the element at `e`.
    fn key(&self, e: u64, shape: Shape) -> Option<String> {
        let k = match shape {
            Shape::V3 => self.u64_at(e)?,
            Shape::V4 => e + 0x10,
        };
        let chars = match self.u32_at(k)? {
            STRING => self.u64_at(k + 8)?,
            STRING_NAME if shape == Shape::V4 => {
                let sn = self.u64_at(k + 8).filter(|&p| self.heap.is_pointer(p))?;
                return [8, 0x10].into_iter().find_map(|at| self.u64_at(sn + at).filter(|&p| self.heap.is_pointer(p)).and_then(|c| self.chars(c)));
            }
            _ => return None,
        };
        self.chars(chars)
    }

    /// A key's UTF-32 characters at `chars`.
    fn chars(&self, chars: u64) -> Option<String> {
        if let Some(cached) = self.keys.borrow().get(&chars) {
            return cached.clone();
        }
        let mut b = [0u8; 4 * MAX_KEY];
        let text = self.mem().read_exact_at(&mut b, chars).ok().and_then(|_| {
            let units: Vec<u32> = b.chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).take_while(|&c| c != 0).collect();
            let t: String = units.iter().map(|&c| char::from_u32(c)).collect::<Option<_>>()?;
            (units.len() < MAX_KEY && good_key(&t)).then_some(t)
        });
        self.keys.borrow_mut().insert(chars, text.clone());
        text
    }

    /// The element's number, int or real.
    fn number(&self, e: u64, shape: Shape) -> Option<f64> {
        match self.u32_at(e + shape.value() - 8)? {
            INT => self.int(e, shape).map(|v| v as f64),
            REAL => self.u64_at(e + shape.value()).map(f64::from_bits),
            _ => None,
        }
    }

    /// The slot the element's dictionary is in, when it has a slot key.
    fn slot(&self, e: u64, shape: Shape) -> Option<i64> {
        self.siblings(e, shape).into_iter().find(|(k, v)| v.is_some() && slot_key(k))?.1
    }

    fn int(&self, e: u64, shape: Shape) -> Option<i64> {
        (self.u32_at(e + shape.value() - 8)? == INT).then(|| self.u64_at(e + shape.value()).map(|v| v as i64)).flatten()
    }

    /// The other entries of the dictionary the element at `e` is in: their keys and ints.
    fn siblings(&self, e: u64, shape: Shape) -> Vec<(String, Option<i64>)> {
        let mut first = e;
        for _ in 0..MAX_ENTRIES {
            match self.u64_at(first + shape.prev()) {
                Some(p) if p != 0 && p != e && self.heap.is_pointer(p) => first = p,
                _ => break,
            }
        }
        let mut out = Vec::new();
        let mut at = first;
        for _ in 0..MAX_ENTRIES {
            if at != e {
                if let Some(k) = self.key(at, shape) {
                    out.push((k, self.int(at, shape)));
                }
            }
            match self.u64_at(at + shape.next()) {
                Some(n) if n != 0 && n != first && self.heap.is_pointer(n) => at = n,
                _ => break,
            }
        }
        out
    }

    fn fits(&self, e: u64, path: &DictPath) -> bool {
        let Some(shape) = self.element(e) else { return false };
        if self.key(e, shape).as_deref() != Some(path.key.as_str()) {
            return false;
        }
        let siblings = self.siblings(e, shape);
        path.with.iter().all(|(k, v)| siblings.iter().any(|(sk, sv)| sk == k && (v.is_none() || sv == v)))
    }

    /// Every element in memory whose key is `key`, of either shape (one pass over the game's
    /// memory).
    fn with_key(&self, key: &str) -> Vec<u64> {
        let mut found = Vec::new();
        let word = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let half = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        self.heap.each_chunk(ELEMENT, |addr, b, fresh| {
            for o in (0..fresh).step_by(8).take_while(|o| o + ELEMENT <= b.len()) {
                let e = addr + o as u64;
                for shape in SHAPES {
                    let t = half(b, o + shape.value() as usize - 8);
                    if t != INT && t != REAL {
                        continue;
                    }
                    let k = match shape {
                        Shape::V3 => word(b, o),
                        Shape::V4 => half(b, o + 0x10) as u64,
                    };
                    let (next, prev) = (word(b, o + shape.next() as usize), word(b, o + shape.prev() as usize));
                    if self.shaped(shape, k, t, next, prev) && self.key(e, shape).as_deref() == Some(key) {
                        found.push(e);
                    }
                }
            }
        });
        found
    }
}

/// The value's address of each element.
fn values(d: &Dicts, elements: &[u64]) -> Vec<u64> {
    elements.iter().filter_map(|&e| Some(e + d.element(e)?.value())).collect()
}

/// Elements the path leads to, among those found before (cheap: no search). A stack found
/// stays followed when the player moves it to another slot.
pub fn walk(heap: &Heap, roots: &[u64], path: &DictPath) -> Vec<u64> {
    let d = Dicts::new(heap);
    let path = path.any_slot();
    values(&d, &roots.iter().copied().filter(|&e| d.fits(e, &path)).collect::<Vec<_>>())
}

/// Searches the game's memory for the path's elements (the roots `walk` takes). The slot saved
/// is a preference: the game picks slots by pickup order, so on another map the item is
/// elsewhere (Lumencraft's iron: saved in slot 1, then in 7); then its biggest stack (a full
/// stack in the backpack rather than the one that overflowed onto the hotbar), with its copies.
pub fn find_roots(heap: &Heap, path: &DictPath) -> Vec<u64> {
    let d = Dicts::new(heap);
    let any = path.any_slot();
    let all: Vec<u64> = d.with_key(&path.key).into_iter().filter(|&e| d.fits(e, &any)).collect();
    let mut found: Vec<u64> = all.iter().copied().filter(|&e| d.fits(e, path)).collect();
    if found.is_empty() && path.has_slot() {
        let stacks: Vec<(u64, Option<i64>, f64)> = all
            .iter()
            .filter_map(|&e| {
                let shape = d.element(e)?;
                Some((e, d.slot(e, shape), d.number(e, shape)?))
            })
            .collect();
        let biggest = stacks.iter().max_by(|a, b| a.2.total_cmp(&b.2)).map(|s| s.1);
        found = stacks.iter().filter(|s| Some(s.1) == biggest).map(|s| s.0).collect();
    }
    found.truncate(MAX_PLACES);
    found
}

/// A path to the value at `target` when it is an entry of a dictionary: its key, the other
/// keys of its dictionary, and the int of one of them: an "id" when there is one, else the one
/// that picks out the fewest dictionaries; plus the slot it is in when it is in one: an item can have several stacks,
/// and the game shows their total (Lumencraft: 200 lumen in the backpack, 1 on the hotbar; a
/// limit holding every stack at once let pickups and purchases go to the other one). The
/// player keeps the item in that slot. With the places it leads to now.
pub fn discover(heap: &Heap, target: u64) -> Option<(DictPath, Vec<u64>)> {
    let d = Dicts::new(heap);
    let (e, shape) = at_value(&d, target)?;
    let key = d.key(e, shape)?;
    let siblings = d.siblings(e, shape);
    if siblings.is_empty() {
        return None;
    }
    let base = DictPath { key: key.clone(), with: siblings.iter().map(|(k, _)| (k.clone(), None)).collect() };
    let all: Vec<u64> = d.with_key(&key).into_iter().filter(|&x| d.fits(x, &base)).collect();
    let mut options = vec![base.clone()];
    // A slot alone would write to whatever the player puts there: it only narrows an item down.
    for (k, v) in siblings.iter().filter(|(k, v)| v.is_some() && !slot_key(k)) {
        let mut p = base.clone();
        p.with.iter_mut().filter(|(pk, _)| pk == k).for_each(|w| w.1 = *v);
        options.push(p);
    }
    let identity = |p: &DictPath| if p.with.iter().any(|(k, v)| v.is_some() && identity_key(k)) { 0 } else { 1 };
    options
        .into_iter()
        .map(|p| {
            let leads: Vec<u64> = all.iter().copied().filter(|&x| d.fits(x, &p)).collect();
            (p, leads)
        })
        .filter(|(_, leads)| leads.contains(&e) && leads.len() <= MAX_PLACES)
        .min_by_key(|(p, leads)| (identity(p), leads.len(), p.with.iter().filter(|w| w.1.is_some()).count()))
        .map(|(mut p, mut leads)| {
            let slot = siblings.iter().find(|(k, v)| v.is_some() && slot_key(k));
            if let Some((k, v)) = slot.filter(|_| identity(&p) == 0) {
                p.with.iter_mut().filter(|(pk, _)| pk == k).for_each(|w| w.1 = *v);
                leads.retain(|&x| d.fits(x, &p));
            }
            (p, values(&d, &leads))
        })
}

/// The element whose value is at `addr`, and its shape.
fn at_value(d: &Dicts, addr: u64) -> Option<(u64, Shape)> {
    SHAPES.into_iter().find_map(|shape| {
        let e = addr.checked_sub(shape.value())?;
        d.is(e, shape).then_some((e, shape))
    })
}

/// What the value at `addr` is, when it is an entry of a dictionary: its key, and the ids of
/// its dictionary (""amount" where "id" is 0"), for telling an inventory stack from a statistic
/// among a search's matches.
pub fn about(heap: &Heap, addr: u64) -> Option<String> {
    let d = Dicts::new(heap);
    let (e, shape) = at_value(&d, addr)?;
    let key = d.key(e, shape)?;
    let ids: Vec<String> = d
        .siblings(e, shape)
        .into_iter()
        .filter(|(k, v)| v.is_some() && (identity_key(k) || slot_key(k)))
        .map(|(k, v)| format!("\"{k}\" is {}", v.unwrap_or_default()))
        .collect();
    Some(match ids.is_empty() {
        true => format!("\"{key}\""),
        false => format!("\"{key}\" where {}", ids.join(" and ")),
    })
}

/// A key that says what an entry is (rather than where or how much).
fn identity_key(k: &str) -> bool {
    let k = k.to_lowercase();
    !slot_key(&k) && ["id", "type", "kind", "name"].iter().any(|n| k.contains(n))
}

/// A key that says which inventory slot an entry is in. Only these names: a key that moves
/// (a sort position) would lose the value.
fn slot_key(k: &str) -> bool {
    ["index", "slot", "slot_index", "slotindex", "slot_id", "slotid"].contains(&k.to_lowercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip() {
        let p = DictPath::parse("{amount|id=0,data,index}").unwrap();
        assert_eq!(p.with, vec![("id".into(), Some(0)), ("data".into(), None), ("index".into(), None)]);
        assert_eq!(p.text(), "{amount|id=0,data,index}");
        assert_eq!(p.describe(), "\"amount\" where \"id\" is 0");
        assert_eq!(DictPath::parse("{amount|id=-3}").unwrap().with, vec![("id".into(), Some(-3))]);
        assert!(DictPath::parse("{amo unt|id}").is_none());
        assert!(DictPath::parse("\"Inventory\"@10,+38").is_none());
        let p = DictPath::parse("{amount|id=1,data,index=7}").unwrap();
        assert_eq!(p.describe(), "\"amount\" where \"id\" is 1 and \"index\" is 7");
        assert!(p.has_slot() && !p.any_slot().has_slot());
        assert_eq!(p.any_slot().text(), "{amount|id=1,data,index}");
    }

    #[test]
    fn slot_keys() {
        assert!(slot_key("index") && slot_key("Slot") && slot_key("slot_id"));
        assert!(!identity_key("slot_id") && identity_key("item_id") && !slot_key("position"));
    }
}
