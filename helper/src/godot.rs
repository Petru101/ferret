// Godot 3 dictionaries: a value kept as an entry of a Dictionary the game's scripts fill
// (Lumencraft's inventory stacks are {"id": 0, "amount": 2, "data": null, "index": 6}). The
// game re-creates such entries as it runs, in memory allocated at run time, so neither code
// patterns (the GDScript interpreter is shared code) nor pointer paths find them again; their
// keys do.
// Memory (64-bit Linux builds): a Dictionary keeps its entries in a list whose elements are
// { const Variant *key; Variant value; Element *next, *prev; void *list }, a Variant is
// { u32 type; 4 bytes of padding (not always zero); 16 bytes of data }, type 2 = int (an int64),
// 3 = real (a double),
// 4 = String (data = pointer to its characters, UTF-32: wchar_t is 4 bytes on Linux).
// Text form: {amount|id=0,index} = the "amount" entry of every dictionary that also has an
// "id" entry holding 0 and an "index" entry.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;

use crate::names::Heap;

const VALUE_TYPE: u64 = 8;
const VALUE: u64 = 16;
const NEXT: u64 = 32;
const PREV: u64 = 40;
const INT: u32 = 2;
const REAL: u32 = 3;
const STRING: u32 = 4;
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

    /// In words: ""amount" where "id" is 0".
    pub fn describe(&self) -> String {
        let held: Vec<String> = self.with.iter().filter_map(|(k, v)| Some(format!("\"{k}\" is {}", (*v)?))).collect();
        match held.is_empty() {
            true => format!("\"{}\" in the game's dictionaries", self.key),
            false => format!("\"{}\" where {}", self.key, held.join(" and ")),
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

    /// Looks like a list element with a number for its value (checked on the words already read).
    fn shaped(&self, key: u64, value_type: u32, next: u64, prev: u64) -> bool {
        (value_type == INT || value_type == REAL)
            && self.heap.is_pointer(key)
            && (next != 0 || prev != 0)
            && self.link(next)
            && self.link(prev)
    }

    fn element(&self, e: u64) -> bool {
        let (Some(key), Some(t), Some(next), Some(prev)) =
            (self.u64_at(e), self.u32_at(e + VALUE_TYPE), self.u64_at(e + NEXT), self.u64_at(e + PREV))
        else {
            return false;
        };
        self.shaped(key, t, next, prev)
    }

    /// The text of the String key of the element at `e`.
    fn key(&self, e: u64) -> Option<String> {
        let k = self.u64_at(e)?;
        if self.u32_at(k)? != STRING {
            return None;
        }
        let chars = self.u64_at(k + 8)?;
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

    fn int(&self, e: u64) -> Option<i64> {
        (self.u32_at(e + VALUE_TYPE)? == INT).then(|| self.u64_at(e + VALUE).map(|v| v as i64)).flatten()
    }

    /// The other entries of the dictionary the element at `e` is in: their keys and ints.
    fn siblings(&self, e: u64) -> Vec<(String, Option<i64>)> {
        let mut first = e;
        for _ in 0..MAX_ENTRIES {
            match self.u64_at(first + PREV) {
                Some(p) if p != 0 && p != e && self.heap.is_pointer(p) => first = p,
                _ => break,
            }
        }
        let mut out = Vec::new();
        let mut at = first;
        for _ in 0..MAX_ENTRIES {
            if at != e {
                if let Some(k) = self.key(at) {
                    out.push((k, self.int(at)));
                }
            }
            match self.u64_at(at + NEXT) {
                Some(n) if n != 0 && n != first && self.heap.is_pointer(n) => at = n,
                _ => break,
            }
        }
        out
    }

    fn fits(&self, e: u64, path: &DictPath) -> bool {
        if !self.element(e) || self.key(e).as_deref() != Some(path.key.as_str()) {
            return false;
        }
        let siblings = self.siblings(e);
        path.with.iter().all(|(k, v)| siblings.iter().any(|(sk, sv)| sk == k && (v.is_none() || sv == v)))
    }

    /// Every element in memory whose key is `key` (one pass over the game's memory).
    fn with_key(&self, key: &str) -> Vec<u64> {
        let mut found = Vec::new();
        let word = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let half = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        self.heap.each_chunk(48, |addr, b, fresh| {
            for o in (0..fresh).step_by(8).take_while(|o| o + 48 <= b.len()) {
                let t = half(b, o + 8);
                if t != INT && t != REAL {
                    continue;
                }
                let e = addr + o as u64;
                if self.shaped(word(b, o), t, word(b, o + 32), word(b, o + 40)) && self.key(e).as_deref() == Some(key) {
                    found.push(e);
                }
            }
        });
        found
    }
}

/// The value's address of each element.
fn values(elements: &[u64]) -> Vec<u64> {
    elements.iter().map(|e| e + VALUE).collect()
}

/// Elements the path leads to, among those found before (cheap: no search).
pub fn walk(heap: &Heap, roots: &[u64], path: &DictPath) -> Vec<u64> {
    let d = Dicts::new(heap);
    values(&roots.iter().copied().filter(|&e| d.fits(e, path)).collect::<Vec<_>>())
}

/// Searches the game's memory for the path's elements (the roots `walk` takes).
pub fn find_roots(heap: &Heap, path: &DictPath) -> Vec<u64> {
    let d = Dicts::new(heap);
    let mut found: Vec<u64> = d.with_key(&path.key).into_iter().filter(|&e| d.fits(e, path)).collect();
    found.truncate(MAX_PLACES);
    found
}

/// A path to the value at `target` when it is an entry of a dictionary: its key, the other
/// keys of its dictionary, and the int of one of them that picks out the fewest dictionaries
/// (an "id" first: the slot an item is in changes, what it is doesn't). With the places it
/// leads to now.
pub fn discover(heap: &Heap, target: u64) -> Option<(DictPath, Vec<u64>)> {
    let d = Dicts::new(heap);
    let e = target.checked_sub(VALUE)?;
    if !d.element(e) {
        return None;
    }
    let key = d.key(e)?;
    let siblings = d.siblings(e);
    if siblings.is_empty() {
        return None;
    }
    let base = DictPath { key: key.clone(), with: siblings.iter().map(|(k, _)| (k.clone(), None)).collect() };
    let all: Vec<u64> = d.with_key(&key).into_iter().filter(|&x| d.fits(x, &base)).collect();
    let mut options = vec![base.clone()];
    for (k, v) in siblings.iter().filter(|(_, v)| v.is_some()) {
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
        .min_by_key(|(p, leads)| (leads.len(), identity(p), p.with.iter().filter(|w| w.1.is_some()).count()))
        .map(|(p, leads)| (p, values(&leads)))
}

/// What the value at `addr` is, when it is an entry of a dictionary: its key, and the ids of
/// its dictionary (""amount" where "id" is 0"), for telling an inventory stack from a statistic
/// among a search's matches.
pub fn about(heap: &Heap, addr: u64) -> Option<String> {
    let d = Dicts::new(heap);
    let e = addr.checked_sub(VALUE)?;
    if !d.element(e) {
        return None;
    }
    let key = d.key(e)?;
    let ids: Vec<String> = d
        .siblings(e)
        .into_iter()
        .filter(|(k, v)| v.is_some() && identity_key(k))
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
    ["id", "type", "kind", "name"].iter().any(|n| k.contains(n))
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
    }
}
