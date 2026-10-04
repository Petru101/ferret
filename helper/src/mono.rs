// Unity (Mono) paths: a field of the live objects of a class, found by the class's name. Every
// managed object starts with a pointer to its vtable, whose first word is its class; a class
// holds its name, namespace, parent and fields (name and offset, from the object's start).
// A checkpoint in ULTRAKILL makes a new player object (NewMovement) and leaves the old one
// readable: named and pointer paths kept leading to the old one. Unity objects carry
// m_CachedPtr (their native object), null once destroyed: those are skipped.
//
// Text: `mono:NewMovement.hp`, `mono:Some.Namespace::Class.field`.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Mutex;

use crate::names::Heap;

/// How far before a value its object may start.
const MAX_OBJECT: u64 = 0x4000;
/// Fields a class may declare at most (beyond: not a class).
const MAX_FIELDS: u64 = 4096;
/// FIELD_ATTRIBUTE_STATIC and _LITERAL: kept elsewhere than in the object.
const NOT_IN_OBJECT: u16 = 0x10 | 0x40;
/// Live objects of a class a path may be found through: the player and its prefab (Unity's
/// template for the next one, as live as the player; ULTRAKILL has both). More: they're many
/// things, like enemies, and a write would reach them all.
const MAX_PLACES: usize = 2;
/// How far into a Unity native object its pointer back to the managed one is.
const NATIVE_BACK: u64 = 0x80;

#[derive(Clone, Debug, PartialEq)]
pub struct MonoPath {
    pub namespace: String,
    pub class: String,
    pub field: String,
}

fn ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_<>`$".contains(c))
}

impl MonoPath {
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix("mono:")?;
        let (namespace, rest) = rest.rsplit_once("::").unwrap_or(("", rest));
        let (class, field) = rest.split_once('.')?;
        let ns_ok = namespace.is_empty() || namespace.split('.').all(ident);
        (ns_ok && ident(class) && ident(field)).then(|| MonoPath { namespace: namespace.into(), class: class.into(), field: field.into() })
    }

    pub fn text(&self) -> String {
        match self.namespace.as_str() {
            "" => format!("mono:{}.{}", self.class, self.field),
            ns => format!("mono:{ns}::{}.{}", self.class, self.field),
        }
    }

    pub fn describe(&self) -> String {
        format!("{} of {}", self.field, self.class)
    }
}

/// Where a class keeps its name (namespace right after), parent and fields, found on the game.
#[derive(Clone, Copy)]
struct Layout {
    w: u64,
    name: u64,
    parent: Option<u64>,
    fields: u64,
}

static LAYOUT: Mutex<Option<(u32, Layout)>> = Mutex::new(None);
/// The vtables of classes searched for this run: (pid, namespace::class, vtables).
static VTABLES: Mutex<Vec<(u32, String, Vec<u64>)>> = Mutex::new(Vec::new());

/// Whether the game runs Mono (Unity's, or another's).
pub fn is_mono(pid: u32) -> bool {
    crate::helper::maps(pid).is_ok_and(|m| {
        m.iter().any(|r| {
            let p = r.path.to_lowercase();
            p.contains("libmono") || p.contains("mono-2.0") || p.ends_with("/mono.dll")
        })
    })
}

fn word(mem: &File, at: u64, w: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b[..w as usize], at).ok()?;
    Some(u64::from_le_bytes(b))
}

/// A C string of printable ASCII (empty is fine: the global namespace).
fn cstr(mem: &File, at: u64) -> Option<String> {
    if at < 0x10000 {
        return None;
    }
    let mut b = [0u8; 128];
    let n = mem.read_at(&mut b, at).ok()?;
    let end = b[..n].iter().position(|&c| c == 0)?;
    let s = std::str::from_utf8(&b[..end]).ok()?;
    s.chars().all(|c| c.is_ascii_graphic()).then(|| s.to_owned())
}

/// A class points to itself first (element_class: other for arrays only).
fn is_class(mem: &File, k: u64, w: u64) -> bool {
    k >= 0x10000 && word(mem, k, w) == Some(k)
}

/// The class of the object at `obj`: its vtable's first word. A class itself fits that shape
/// (it points to itself), and isn't an object.
fn class_of(mem: &File, obj: u64, w: u64) -> Option<u64> {
    let vt = word(mem, obj, w).filter(|&v| v >= 0x10000 && !is_class(mem, v, w))?;
    word(mem, vt, w).filter(|&k| is_class(mem, k, w))
}

/// Finds where `k` keeps its name, parent and fields.
fn detect(mem: &File, k: u64, w: u64) -> Option<Layout> {
    let name = (4 * w..=0x80).step_by(w as usize).find(|&off| {
        word(mem, k + off, w).and_then(|p| cstr(mem, p)).is_some_and(|s| ident(&s))
            && word(mem, k + off + w, w).and_then(|p| cstr(mem, p)).is_some()
    })?;
    let fields = (name + 2 * w..name + 0x100).step_by(w as usize).find(|&off| {
        word(mem, k + off, w).is_some_and(|p| {
            p >= 0x10000 && word(mem, p + 2 * w, w) == Some(k) && word(mem, p + w, w).and_then(|n| cstr(mem, n)).is_some_and(|s| ident(&s))
        })
    })?;
    let parent = (2 * w..name).step_by(w as usize).find(|&off| {
        word(mem, k + off, w).is_some_and(|p| {
            p != k && is_class(mem, p, w) && word(mem, p + name, w).and_then(|n| cstr(mem, n)).is_some_and(|s| ident(&s))
        })
    });
    Some(Layout { w, name, parent, fields })
}

fn layout(pid: u32, mem: &File, k: u64, w: u64) -> Option<Layout> {
    let mut known = LAYOUT.lock().unwrap();
    if let Some((p, lay)) = *known {
        if p == pid {
            return Some(lay);
        }
    }
    let lay = detect(mem, k, w)?;
    *known = Some((pid, lay));
    Some(lay)
}

fn known_layout(pid: u32) -> Option<Layout> {
    LAYOUT.lock().unwrap().filter(|(p, _)| *p == pid).map(|(_, l)| l)
}

struct Mono<'a> {
    mem: &'a File,
    lay: Layout,
}

impl Mono<'_> {
    fn word(&self, at: u64) -> Option<u64> {
        word(self.mem, at, self.lay.w)
    }

    /// Namespace and name.
    fn name(&self, k: u64) -> Option<(String, String)> {
        let name = cstr(self.mem, self.word(k + self.lay.name)?)?;
        let ns = cstr(self.mem, self.word(k + self.lay.name + self.lay.w)?)?;
        Some((ns, name))
    }

    fn parent(&self, k: u64) -> Option<u64> {
        self.word(k + self.lay.parent?).filter(|&p| is_class(self.mem, p, self.lay.w))
    }

    /// The class and its parents.
    fn chain(&self, k: u64) -> Vec<u64> {
        let mut chain = vec![k];
        while let Some(p) = self.parent(*chain.last().unwrap()).filter(|p| chain.len() < 32 && !chain.contains(p)) {
            chain.push(p);
        }
        chain
    }

    /// The fields this class itself declares that objects hold: name and offset.
    fn own_fields(&self, k: u64) -> Vec<(String, u64)> {
        let w = self.lay.w;
        let Some(p) = self.word(k + self.lay.fields).filter(|&p| p >= 0x10000) else { return Vec::new() };
        let mut fields = Vec::new();
        for i in 0..MAX_FIELDS {
            let f = p + i * 4 * w;
            if self.word(f + 2 * w) != Some(k) {
                break;
            }
            let (Some(name), Some(off)) = (self.word(f + w).and_then(|n| cstr(self.mem, n)), self.word(f + 3 * w)) else { break };
            let mut attrs = [0u8; 2];
            let in_object = self.word(f).is_some_and(|t| self.mem.read_exact_at(&mut attrs, t + w).is_ok() && u16::from_le_bytes(attrs) & NOT_IN_OBJECT == 0);
            if in_object {
                fields.push((name, off as u32 as u64));
            }
        }
        fields
    }

    /// The field's offset, declared by the class or one of its parents.
    fn offset(&self, k: u64, field: &str) -> Option<u64> {
        self.chain(k).into_iter().find_map(|c| self.own_fields(c).into_iter().find(|(n, _)| n == field).map(|(_, o)| o))
    }

    /// The native object of a Unity object (UnityEngine.Object.m_CachedPtr; Some(0): destroyed),
    /// None for other classes.
    fn native(&self, obj: u64, k: u64) -> Option<u64> {
        let unity = self.chain(k).into_iter().find(|&c| self.name(c).is_some_and(|(ns, n)| ns == "UnityEngine" && n == "Object"))?;
        Some(self.word(obj + self.offset(unity, "m_CachedPtr")?).unwrap_or(0))
    }

    /// Whether the object is one, and not a destroyed Unity object. A live Unity object's
    /// native object points back to it (places in Mono's own tables that point to the class's
    /// vtable don't: 175 of them for ULTRAKILL's player).
    fn live(&self, obj: u64, k: u64) -> bool {
        match self.native(obj, k) {
            Some(0) => false,
            Some(n) => (0..NATIVE_BACK).step_by(self.lay.w as usize).any(|o| self.word(n + o) == Some(obj)),
            None => true,
        }
    }

    /// How recent a Unity object is: the size of its instance ID (loaded objects count up,
    /// instantiated ones down), right after the native object's vtable.
    fn age(&self, obj: u64, k: u64) -> u32 {
        let mut b = [0u8; 4];
        match self.native(obj, k) {
            Some(n) if n != 0 && self.mem.read_exact_at(&mut b, n + self.lay.w).is_ok() => i32::from_le_bytes(b).unsigned_abs(),
            _ => 0,
        }
    }

    fn is(&self, k: u64, path: &MonoPath) -> bool {
        self.name(k).is_some_and(|(ns, n)| ns == path.namespace && n == path.class)
    }

    /// The object holding `target` (the nearest object header before it) and the field it is.
    fn holder(&self, target: u64) -> Option<(u64, u64, String)> {
        let w = self.lay.w;
        let start = target - target % w;
        (0..MAX_OBJECT).step_by(w as usize).find_map(|back| {
            let obj = start.checked_sub(back)?;
            let k = class_of(self.mem, obj, w)?;
            Some((obj, k))
        }).and_then(|(obj, k)| {
            let field = self.chain(k).into_iter().find_map(|c| self.own_fields(c).into_iter().find(|(_, o)| obj + o == target))?;
            Some((obj, k, field.0))
        })
    }
}

/// The class of the object holding `target`, and the field: the path, and the live objects it
/// leads to (only one: a class with many live objects is many things).
pub fn discover(heap: &Heap, target: u64) -> Option<(MonoPath, Vec<u64>)> {
    if !is_mono(heap.pid) {
        return None;
    }
    let (mem, w) = (heap.file(), heap.width() as u64);
    let start = target - target % w;
    let k = (0..MAX_OBJECT).step_by(w as usize).find_map(|back| class_of(mem, start.checked_sub(back)?, w))?;
    let m = Mono { mem, lay: layout(heap.pid, mem, k, w)? };
    let (obj, k, field) = m.holder(target)?;
    let (namespace, class) = m.name(k)?;
    if !m.live(obj, k) {
        return None;
    }
    let path = MonoPath { namespace, class, field };
    let leads = walk(heap, &find_roots(heap, &path), &path);
    (leads.len() <= MAX_PLACES && leads.contains(&target)).then_some((path, leads))
}

/// What `addr` is, when it's a field of a Mono object: "hp of NewMovement" (for the matches
/// list; a destroyed Unity object says so).
pub fn about(heap: &Heap, addr: u64) -> Option<String> {
    if !is_mono(heap.pid) {
        return None;
    }
    let (mem, w) = (heap.file(), heap.width() as u64);
    let start = addr - addr % w;
    let k = (0..MAX_OBJECT).step_by(w as usize).find_map(|back| class_of(mem, start.checked_sub(back)?, w))?;
    let m = Mono { mem, lay: layout(heap.pid, mem, k, w)? };
    let (obj, k, field) = m.holder(addr)?;
    let (_, class) = m.name(k)?;
    Some(match m.live(obj, k) {
        true => format!("{field} of {class}"),
        false => format!("{field} of a destroyed {class} (gone from the game)"),
    })
}

/// Every place these bytes are in writable memory, at any alignment.
fn find_bytes(heap: &Heap, bytes: &[u8]) -> Vec<u64> {
    let mut found = Vec::new();
    heap.each_chunk(bytes.len(), |addr, b, fresh| {
        let mut i = 0;
        while i < fresh {
            match b[i..fresh].iter().position(|&c| c == bytes[0]) {
                Some(p) => i += p,
                None => break,
            }
            if b[i..].starts_with(bytes) {
                found.push(addr + i as u64);
            }
            i += 1;
        }
    });
    found
}

/// The class's vtables: its name, the classes pointing to it, the vtables pointing to those
/// (three passes over memory; kept for the run).
fn vtables(heap: &Heap, path: &MonoPath) -> Vec<u64> {
    let key = format!("{}::{}", path.namespace, path.class);
    if let Some((_, _, v)) = VTABLES.lock().unwrap().iter().find(|(p, t, _)| *p == heap.pid && *t == key) {
        return v.clone();
    }
    let (mem, w) = (heap.file(), heap.width() as u64);
    let pattern: Vec<u8> = [&[0u8][..], path.class.as_bytes(), &[0]].concat();
    let names: Vec<u64> = find_bytes(heap, &pattern).into_iter().map(|a| a + 1).collect();
    if names.is_empty() {
        return Vec::new();
    }
    let refs = heap.referrers(&names);
    let mut classes: Vec<u64> = match known_layout(heap.pid) {
        Some(lay) => refs.iter().filter_map(|&at| at.checked_sub(lay.name)).filter(|&k| is_class(mem, k, w)).collect(),
        // A new run: the name's offset from the classes that fit.
        None => refs
            .iter()
            .flat_map(|&at| (4 * w..=0x80).step_by(w as usize).filter_map(move |off| at.checked_sub(off)))
            .filter(|&k| is_class(mem, k, w) && detect(mem, k, w).is_some_and(|l| word(mem, k + l.name, w).is_some_and(|p| names.contains(&p))))
            .collect(),
    };
    let Some(lay) = classes.first().and_then(|&k| layout(heap.pid, mem, k, w)) else { return Vec::new() };
    let m = Mono { mem, lay };
    classes.retain(|&k| m.is(k, path));
    classes.sort_unstable();
    classes.dedup();
    if classes.is_empty() {
        return Vec::new();
    }
    let vts = heap.referrers(&classes);
    if !vts.is_empty() {
        let mut cache = VTABLES.lock().unwrap();
        cache.retain(|(p, _, _)| *p == heap.pid);
        cache.push((heap.pid, key, vts.clone()));
    }
    vts
}

/// The live objects of the path's class (a pass over memory, three on a new run).
pub fn find_roots(heap: &Heap, path: &MonoPath) -> Vec<u64> {
    let vts = vtables(heap, path);
    let (Some(lay), false) = (known_layout(heap.pid), vts.is_empty()) else { return Vec::new() };
    let m = Mono { mem: heap.file(), lay };
    heap.referrers(&vts)
        .into_iter()
        .filter(|&obj| class_of(m.mem, obj, lay.w).is_some_and(|k| m.is(k, path) && m.live(obj, k)))
        .collect()
}

/// Where the path leads from the objects found before (no search), the newest first (the one
/// shown; a prefab, loaded with the game, comes last). Nothing once one of them is gone: the
/// game replaced it (a checkpoint's new player), and the prefab alone would keep the search
/// from looking again.
pub fn walk(heap: &Heap, roots: &[u64], path: &MonoPath) -> Vec<u64> {
    let Some(lay) = known_layout(heap.pid) else { return Vec::new() };
    let m = Mono { mem: heap.file(), lay };
    let leads: Option<Vec<(u32, u64)>> = roots
        .iter()
        .map(|&obj| {
            let k = class_of(m.mem, obj, lay.w).filter(|&k| m.is(k, path) && m.live(obj, k))?;
            Some((m.age(obj, k), obj + m.offset(k, &path.field)?))
        })
        .collect();
    let Some(mut leads) = leads else { return Vec::new() };
    leads.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    leads.dedup_by_key(|l| l.1);
    leads.into_iter().map(|(_, a)| a).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip() {
        let p = MonoPath::parse("mono:NewMovement.hp").unwrap();
        assert_eq!((p.namespace.as_str(), p.class.as_str(), p.field.as_str()), ("", "NewMovement", "hp"));
        assert_eq!(p.text(), "mono:NewMovement.hp");
        assert_eq!(p.describe(), "hp of NewMovement");
        let t = "mono:Game.Player::Stats`1.<Gold>k__BackingField";
        assert_eq!(MonoPath::parse(t).unwrap().text(), t);
        assert!(MonoPath::parse("mono:NewMovement").is_none());
        assert!(MonoPath::parse("mono:New Movement.hp").is_none());
        assert!(MonoPath::parse("gd:a.gd.x").is_none());
    }
}
