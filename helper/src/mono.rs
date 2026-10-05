// Unity paths (Mono and IL2CPP): a field of the live objects of a class, found by the class's
// name. A Mono object starts with a pointer to its vtable, whose first word is its class; an
// IL2CPP object starts with its class. A class holds its name, namespace, parent and fields
// (name and offset, from the object's start).
// A checkpoint in ULTRAKILL makes a new player object (NewMovement) and leaves the old one
// readable: named and pointer paths kept leading to the old one. Unity objects carry
// m_CachedPtr (their native object), null once destroyed: those are skipped.
//
// Text: `mono:NewMovement.hp`, `mono:Some.Namespace::Class.field`, `il2cpp:CommandBase._ammo`.

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

/// How the game runs its C#: Mono's runtime, or IL2CPP's (compiled to native code in
/// GameAssembly; class names in the read-only global-metadata.dat).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Runtime {
    Mono,
    Il2cpp,
}

impl Runtime {
    fn prefix(self) -> &'static str {
        match self {
            Runtime::Mono => "mono:",
            Runtime::Il2cpp => "il2cpp:",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MonoPath {
    pub runtime: Runtime,
    pub namespace: String,
    pub class: String,
    pub field: String,
}

fn ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_<>`$".contains(c))
}

impl MonoPath {
    pub fn parse(text: &str) -> Option<Self> {
        let runtime = [Runtime::Mono, Runtime::Il2cpp].into_iter().find(|r| text.starts_with(r.prefix()))?;
        let rest = &text[runtime.prefix().len()..];
        let (namespace, rest) = rest.rsplit_once("::").unwrap_or(("", rest));
        let (class, field) = rest.split_once('.')?;
        let ns_ok = namespace.is_empty() || namespace.split('.').all(ident);
        (ns_ok && ident(class) && ident(field)).then(|| MonoPath { runtime, namespace: namespace.into(), class: class.into(), field: field.into() })
    }

    pub fn text(&self) -> String {
        let pre = self.runtime.prefix();
        match self.namespace.as_str() {
            "" => format!("{pre}{}.{}", self.class, self.field),
            ns => format!("{pre}{ns}::{}.{}", self.class, self.field),
        }
    }

    pub fn describe(&self) -> String {
        format!("{} of {}", self.field, self.class)
    }
}

/// Where a class keeps its name (namespace right after), parent, fields and a pointer to
/// itself, and how a field is laid out, found on the game.
#[derive(Clone, Copy)]
struct Layout {
    runtime: Runtime,
    w: u64,
    name: u64,
    parent: Option<u64>,
    fields: u64,
    /// Mono: 0 (element_class); IL2CPP: `klass` (+0x78 on Creeper World 4).
    this: u64,
    /// In a field: its name and its type (the offset is at 3 words in both).
    field_name: u64,
    field_type: u64,
    field_size: u64,
}

static LAYOUT: Mutex<Option<(u32, Layout)>> = Mutex::new(None);
/// The vtables of classes searched for this run: (pid, namespace::class, vtables).
static VTABLES: Mutex<Vec<(u32, String, Vec<u64>)>> = Mutex::new(Vec::new());

/// Whether the game runs Mono (Unity's, or another's) or IL2CPP.
fn runtime(pid: u32) -> Option<Runtime> {
    let maps = crate::helper::maps(pid).ok()?;
    let paths: Vec<String> = maps.iter().map(|r| r.path.to_lowercase()).collect();
    if paths.iter().any(|p| p.contains("libmono") || p.contains("mono-2.0") || p.ends_with("/mono.dll")) {
        return Some(Runtime::Mono);
    }
    paths.iter().any(|p| p.ends_with("/gameassembly.dll") || p.ends_with("/gameassembly.so")).then_some(Runtime::Il2cpp)
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

/// A class points to itself (Mono: first, element_class, other for arrays only).
fn is_class(mem: &File, k: u64, this: u64, w: u64) -> bool {
    k >= 0x10000 && word(mem, k + this, w) == Some(k)
}

/// The class of the object at `obj`: Mono: its vtable's first word (a class itself fits that
/// shape, pointing to itself, and isn't an object); IL2CPP: its first word.
fn class_of(mem: &File, obj: u64, lay: &Layout) -> Option<u64> {
    let w = lay.w;
    match lay.runtime {
        Runtime::Mono => {
            let vt = word(mem, obj, w).filter(|&v| v >= 0x10000 && !is_class(mem, v, 0, w))?;
            word(mem, vt, w).filter(|&k| is_class(mem, k, 0, w))
        }
        Runtime::Il2cpp => word(mem, obj, w).filter(|&k| is_class(mem, k, lay.this, w)),
    }
}

/// The layout, from what may be an object at `obj` (before it's known).
fn detect_at(mem: &File, obj: u64, runtime: Runtime, w: u64) -> Option<Layout> {
    let first = word(mem, obj, w).filter(|&v| v >= 0x10000)?;
    match runtime {
        Runtime::Mono => {
            let k = word(mem, first, w).filter(|&k| !is_class(mem, first, 0, w) && is_class(mem, k, 0, w))?;
            detect(mem, k, w)
        }
        Runtime::Il2cpp => detect_il2cpp(mem, first, w),
    }
}

/// Finds where the Mono class `k` keeps its name, parent and fields.
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
            p != k && is_class(mem, p, 0, w) && word(mem, p + name, w).and_then(|n| cstr(mem, n)).is_some_and(|s| ident(&s))
        })
    });
    Some(Layout { runtime: Runtime::Mono, w, name, parent, fields, this: 0, field_name: w, field_type: 0, field_size: 4 * w })
}

/// Finds where the IL2CPP class `k` keeps its fields, parent and pointer to itself (Creeper
/// World 4, 64-bit: name +0x10, namespace +0x18, parent +0x58, itself +0x78, fields +0x80;
/// field = {name, type, parent class, i32 offset, token}). Its name comes after two words
/// (image, gc_desc) in every IL2CPP version.
fn detect_il2cpp(mem: &File, k: u64, w: u64) -> Option<Layout> {
    let name = 2 * w;
    let named = word(mem, k + name, w).and_then(|p| cstr(mem, p)).is_some_and(|s| ident(&s)) && word(mem, k + name + w, w).and_then(|p| cstr(mem, p)).is_some();
    if !named {
        return None;
    }
    let fields = (name + 2 * w..0x100).step_by(w as usize).find(|&off| {
        word(mem, k + off, w).is_some_and(|p| p >= 0x10000 && word(mem, p + 2 * w, w) == Some(k) && word(mem, p, w).and_then(|n| cstr(mem, n)).is_some_and(|s| ident(&s)))
    })?;
    // element_class and castClass point to it too, earlier (other classes for arrays).
    let this = (name + 2 * w..fields).step_by(w as usize).filter(|&off| word(mem, k + off, w) == Some(k)).last()?;
    // The declaring type (nested classes) sits next to the parent: the parent's chain ends at
    // System.Object.
    let parent = (name + 2 * w..this).step_by(w as usize).find(|&off| {
        let mut c = k;
        for _ in 0..32 {
            match word(mem, c + off, w).filter(|&p| p != c && is_class(mem, p, this, w)) {
                Some(p) => c = p,
                None => return false,
            }
            let full = (word(mem, c + name + w, w).and_then(|p| cstr(mem, p)), word(mem, c + name, w).and_then(|p| cstr(mem, p)));
            if full == (Some("System".into()), Some("Object".into())) {
                return true;
            }
        }
        false
    });
    Some(Layout { runtime: Runtime::Il2cpp, w, name, parent, fields, this, field_name: 0, field_type: w, field_size: 3 * w + 8 })
}

/// The layout for this run: known, or found from the nearest object before `target`.
fn layout_near(heap: &Heap, target: u64) -> Option<Layout> {
    let runtime = runtime(heap.pid)?;
    let mut known = LAYOUT.lock().unwrap();
    if let Some((p, lay)) = *known {
        if p == heap.pid {
            return Some(lay);
        }
    }
    let (mem, w) = (heap.file(), heap.width() as u64);
    let start = target - target % w;
    let lay = (0..MAX_OBJECT).step_by(w as usize).find_map(|back| detect_at(mem, start.checked_sub(back)?, runtime, w))?;
    *known = Some((heap.pid, lay));
    Some(lay)
}

/// Remembers the layout found on a class (a new run's first search by name).
fn keep_layout(pid: u32, lay: Layout) {
    *LAYOUT.lock().unwrap() = Some((pid, lay));
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

    fn is_class(&self, k: u64) -> bool {
        is_class(self.mem, k, self.lay.this, self.lay.w)
    }

    fn class_of(&self, obj: u64) -> Option<u64> {
        class_of(self.mem, obj, &self.lay)
    }

    fn parent(&self, k: u64) -> Option<u64> {
        self.word(k + self.lay.parent?).filter(|&p| self.is_class(p))
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
        let lay = self.lay;
        let w = lay.w;
        let Some(p) = self.word(k + lay.fields).filter(|&p| p >= 0x10000) else { return Vec::new() };
        let mut fields = Vec::new();
        for i in 0..MAX_FIELDS {
            let f = p + i * lay.field_size;
            if self.word(f + 2 * w) != Some(k) {
                break;
            }
            let mut off = [0u8; 4];
            let (Some(name), Ok(())) = (self.word(f + lay.field_name).and_then(|n| cstr(self.mem, n)), self.mem.read_exact_at(&mut off, f + 3 * w)) else { break };
            let off = u32::from_le_bytes(off) as u64;
            let mut attrs = [0u8; 2];
            let in_object = self.word(f + lay.field_type).is_some_and(|t| self.mem.read_exact_at(&mut attrs, t + w).is_ok() && u16::from_le_bytes(attrs) & NOT_IN_OBJECT == 0);
            if in_object {
                fields.push((name, off));
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

    /// How recent a Unity object is, by its instance ID (right after the native object's
    /// vtable): loaded objects count up, instantiated ones down; instantiated ones are newer
    /// (Creeper World 4's prefab, loaded with the level, has 78990, the player's -7838).
    fn age(&self, obj: u64, k: u64) -> (bool, u32) {
        let mut b = [0u8; 4];
        match self.native(obj, k) {
            Some(n) if n != 0 && self.mem.read_exact_at(&mut b, n + self.lay.w).is_ok() => {
                let id = i32::from_le_bytes(b);
                (id < 0, id.unsigned_abs())
            }
            _ => (false, 0),
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
            let k = self.class_of(obj)?;
            Some((obj, k))
        }).and_then(|(obj, k)| {
            let field = self.chain(k).into_iter().find_map(|c| self.own_fields(c).into_iter().find(|(_, o)| obj + o == target))?;
            Some((obj, k, field.0))
        })
    }
}

/// The class of the object holding `target`, and the field: the path, and the live objects it
/// leads to (only one: a class with many live objects is many things).
/// IL2CPP: other things point to a class too (its fields, its methods, other classes); only
/// Unity objects can be told from those (their native object points back), so a field of a
/// plain C# class gets no path there (it would lead to many places).
pub fn discover(heap: &Heap, target: u64) -> Option<(MonoPath, Vec<u64>)> {
    let lay = layout_near(heap, target)?;
    let m = Mono { mem: heap.file(), lay };
    let (obj, k, field) = m.holder(target)?;
    let (namespace, class) = m.name(k)?;
    if !m.live(obj, k) {
        return None;
    }
    let path = MonoPath { runtime: lay.runtime, namespace, class, field };
    let leads = walk(heap, &find_roots(heap, &path), &path);
    (leads.len() <= MAX_PLACES && leads.contains(&target)).then_some((path, leads))
}

/// What `addr` is, when it's a field of a Mono object: "hp of NewMovement" (for the matches
/// list; a destroyed Unity object says so).
pub fn about(heap: &Heap, addr: u64) -> Option<String> {
    let m = Mono { mem: heap.file(), lay: layout_near(heap, addr)? };
    let (obj, k, field) = m.holder(addr)?;
    let (_, class) = m.name(k)?;
    Some(match m.live(obj, k) {
        true => format!("{field} of {class}"),
        false => format!("{field} of a destroyed {class} (gone from the game)"),
    })
}

/// Every place these bytes are in a chunk (at any alignment).
fn find_in(found: &mut Vec<u64>, bytes: &[u8], addr: u64, b: &[u8], fresh: usize) {
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
}

/// Every place these bytes are where the runtime keeps class names: Mono in writable memory,
/// IL2CPP in global-metadata.dat (mapped read-only).
fn find_name(heap: &Heap, runtime: Runtime, bytes: &[u8]) -> Vec<u64> {
    let mut found = Vec::new();
    match runtime {
        Runtime::Mono => heap.each_chunk(bytes.len(), |addr, b, fresh| find_in(&mut found, bytes, addr, b, fresh)),
        Runtime::Il2cpp => {
            let maps = crate::helper::maps(heap.pid).unwrap_or_default();
            for r in maps.iter().filter(|r| r.path.to_lowercase().ends_with("global-metadata.dat")) {
                let mut b = vec![0u8; (r.end - r.start) as usize];
                let n = heap.file().read_at(&mut b, r.start).unwrap_or(0);
                find_in(&mut found, bytes, r.start, &b[..n], n);
            }
        }
    }
    found
}

/// What the class's objects point to first: Mono: its vtables (its name, the classes pointing
/// to it, the vtables pointing to those: three passes over memory); IL2CPP: the class (its
/// name in the metadata, one pass). Kept for the run.
fn vtables(heap: &Heap, path: &MonoPath) -> Vec<u64> {
    let key = format!("{}::{}", path.namespace, path.class);
    if let Some((_, _, v)) = VTABLES.lock().unwrap().iter().find(|(p, t, _)| *p == heap.pid && *t == key) {
        return v.clone();
    }
    let (mem, w) = (heap.file(), heap.width() as u64);
    let pattern: Vec<u8> = [&[0u8][..], path.class.as_bytes(), &[0]].concat();
    let names: Vec<u64> = find_name(heap, path.runtime, &pattern).into_iter().map(|a| a + 1).collect();
    if names.is_empty() {
        return Vec::new();
    }
    let refs = heap.referrers(&names);
    let classes: Vec<(u64, Layout)> = match known_layout(heap.pid) {
        Some(lay) => refs.iter().filter_map(|&at| at.checked_sub(lay.name)).filter(|&k| is_class(mem, k, lay.this, w)).map(|k| (k, lay)).collect(),
        // A new run: the name's offset from the classes that fit.
        None => refs
            .iter()
            .flat_map(|&at| (2 * w..=0x80).step_by(w as usize).filter_map(move |off| at.checked_sub(off)))
            .filter_map(|k| match path.runtime {
                Runtime::Mono => is_class(mem, k, 0, w).then(|| detect(mem, k, w)).flatten().map(|l| (k, l)),
                Runtime::Il2cpp => detect_il2cpp(mem, k, w).map(|l| (k, l)),
            })
            .filter(|(k, l)| word(mem, k + l.name, w).is_some_and(|p| names.contains(&p)))
            .collect(),
    };
    let Some(&(_, lay)) = classes.first() else { return Vec::new() };
    keep_layout(heap.pid, lay);
    let m = Mono { mem, lay };
    let mut classes: Vec<u64> = classes.into_iter().map(|(k, _)| k).filter(|&k| m.is_class(k) && m.is(k, path)).collect();
    classes.sort_unstable();
    classes.dedup();
    if classes.is_empty() {
        return Vec::new();
    }
    let vts = match path.runtime {
        Runtime::Mono => heap.referrers(&classes),
        Runtime::Il2cpp => classes,
    };
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
        .filter(|&obj| m.class_of(obj).is_some_and(|k| m.is(k, path) && m.live(obj, k)))
        .collect()
}

/// Where the path leads from the objects found before (no search), the newest first (the one
/// shown; a prefab, loaded with the game, comes last). Nothing once one of them is gone: the
/// game replaced it (a checkpoint's new player), and the prefab alone would keep the search
/// from looking again.
pub fn walk(heap: &Heap, roots: &[u64], path: &MonoPath) -> Vec<u64> {
    let Some(lay) = known_layout(heap.pid) else { return Vec::new() };
    let m = Mono { mem: heap.file(), lay };
    let leads: Option<Vec<((bool, u32), u64)>> = roots
        .iter()
        .map(|&obj| {
            let k = m.class_of(obj).filter(|&k| m.is(k, path) && m.live(obj, k))?;
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
        let p = MonoPath::parse("il2cpp:CommandBase._ammo").unwrap();
        assert_eq!((p.runtime, p.describe()), (Runtime::Il2cpp, "_ammo of CommandBase".to_owned()));
        assert_eq!(p.text(), "il2cpp:CommandBase._ammo");
    }
}
