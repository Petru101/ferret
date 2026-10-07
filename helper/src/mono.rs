// Unity paths (Mono and IL2CPP): a field of the live objects of a class, found by the class's
// name. A Mono object starts with a pointer to its vtable, whose first word is its class; an
// IL2CPP object starts with its class. A class holds its name, namespace, parent and fields
// (name and offset, from the object's start).
// A checkpoint in ULTRAKILL makes a new player object (NewMovement) and leaves the old one
// readable: named and pointer paths kept leading to the old one. Unity objects carry
// m_CachedPtr (their native object), null once destroyed: those are skipped.
//
// Text: `mono:NewMovement.hp`, `mono:Some.Namespace::Class.field`, `il2cpp:CommandBase._ammo`.
// A class with many objects (Terraria keeps 256 Players, one per multiplayer slot) is reached
// through a static field instead (Mono only): `mono:Terraria::Main.player[0].statLife` = slot 0
// of the array in Main's static field `player`, its field `statLife`; `mono:Game.instance.gold`
// without a slot. Mono keeps a class's static fields in a block its vtable points to.

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
/// How far into a vtable its pointer to the class's static data may be (after the method
/// slots; +0xe0 for Terraria's Main).
const MAX_VTABLE: u64 = 0x2000;
/// How big a class's static data may be (Terraria's Main: `player` at +0xf98).
const MAX_STATIC_DATA: u64 = 0x10000;
/// How far into an array a slot holding the object is looked for.
const MAX_SLOT: u64 = 4096;
/// MonoTypeEnum: a single-dimension array.
const TYPE_SZARRAY: u8 = 0x1d;

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
    /// A static field of the class leading to the object (and its slot, when it's an array):
    /// `field` is then a field of that object.
    pub via: Option<Static>,
    pub field: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Static {
    pub field: String,
    pub slot: Option<u64>,
}

impl Static {
    fn parse(text: &str) -> Option<Self> {
        let (field, slot) = match text.strip_suffix(']').and_then(|t| t.split_once('[')) {
            Some((f, n)) => (f, Some(n.parse().ok()?)),
            None => (text, None),
        };
        ident(field).then(|| Static { field: field.into(), slot })
    }

    fn text(&self) -> String {
        match self.slot {
            Some(n) => format!("{}[{n}]", self.field),
            None => self.field.clone(),
        }
    }
}

fn ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_<>`$".contains(c))
}

impl MonoPath {
    pub fn parse(text: &str) -> Option<Self> {
        let runtime = [Runtime::Mono, Runtime::Il2cpp].into_iter().find(|r| text.starts_with(r.prefix()))?;
        let rest = &text[runtime.prefix().len()..];
        let (namespace, rest) = rest.rsplit_once("::").unwrap_or(("", rest));
        let (class, rest) = rest.split_once('.')?;
        let (via, field) = match rest.split_once('.') {
            Some((s, f)) if runtime == Runtime::Mono => (Some(Static::parse(s)?), f),
            Some(_) => return None,
            None => (None, rest),
        };
        let ns_ok = namespace.is_empty() || namespace.split('.').all(ident);
        (ns_ok && ident(class) && ident(field)).then(|| MonoPath { runtime, namespace: namespace.into(), class: class.into(), via, field: field.into() })
    }

    pub fn text(&self) -> String {
        let pre = self.runtime.prefix();
        let class = match &self.via {
            Some(s) => format!("{}.{}", self.class, s.text()),
            None => self.class.clone(),
        };
        match self.namespace.as_str() {
            "" => format!("{pre}{class}.{}", self.field),
            ns => format!("{pre}{ns}::{class}.{}", self.field),
        }
    }

    pub fn describe(&self) -> String {
        match &self.via {
            Some(s) => format!("{} of {}.{}", self.field, self.class, s.text()),
            None => format!("{} of {}", self.field, self.class),
        }
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
/// Classes whose static data was found this run: (pid, class, its static data).
static STATICS: Mutex<Vec<(u32, u64, u64)>> = Mutex::new(Vec::new());
/// Static paths found this run (field left empty): the next value of the same object (mana
/// after life) is tried there first, in a moment instead of three passes over memory.
static FOUND: Mutex<Vec<(u32, u64, u64, MonoPath)>> = Mutex::new(Vec::new());
/// Fields looked up this run (a walk runs 4 times a second for a limit; Terraria's Main
/// declares 1178 fields): (pid, class, name, static) -> offset and type.
#[allow(clippy::type_complexity)]
static FIELDS: Mutex<Vec<((u32, u64, String, bool), Option<(u64, u64)>)>> = Mutex::new(Vec::new());

/// Whether the game runs Mono (Unity's, or another's) or IL2CPP. Mono can be linked into the
/// game's program (mkbundle: native Linux Terraria) with no library of its own: it still maps
/// the assemblies (mscorlib.dll) and its shared counters file. Microsoft's .NET Framework
/// under Wine maps an mscorlib.dll too, from its own folder.
fn runtime(pid: u32) -> Option<Runtime> {
    let maps = crate::helper::maps(pid).ok()?;
    let paths: Vec<String> = maps.iter().map(|r| r.path.to_lowercase()).collect();
    let library = |p: &String| p.contains("libmono") || p.contains("mono-2.0") || p.ends_with("/mono.dll");
    let built_in = |p: &String| p.starts_with("/dev/shm/mono.") || p.ends_with("/mscorlib.dll") && !p.contains("microsoft.net");
    if paths.iter().any(|p| library(p) || built_in(p)) {
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

    /// The fields this class itself declares: name, offset, attributes and type (a MonoType:
    /// data, then attrs u16 and the type's kind u8; data is the class for objects and the
    /// element class for arrays).
    fn declared(&self, k: u64) -> Vec<(String, u64, u16, u64)> {
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
            let Some(t) = self.word(f + lay.field_type).filter(|&t| self.mem.read_exact_at(&mut attrs, t + w).is_ok()) else { continue };
            fields.push((name, off, u16::from_le_bytes(attrs), t));
        }
        fields
    }

    /// The fields this class itself declares that objects hold: name and offset.
    fn own_fields(&self, k: u64) -> Vec<(String, u64)> {
        self.declared(k).into_iter().filter(|f| f.2 & NOT_IN_OBJECT == 0).map(|f| (f.0, f.1)).collect()
    }

    /// The static fields this class itself declares (not constants): name, offset in its
    /// static data, type.
    fn static_fields(&self, k: u64) -> Vec<(String, u64, u64)> {
        self.declared(k).into_iter().filter(|f| f.2 & NOT_IN_OBJECT == 0x10).map(|f| (f.0, f.1, f.3)).collect()
    }

    /// A field's offset and type, remembered for the run: an object's field (declared by the
    /// class or a parent) or a static field of the class.
    fn field(&self, pid: u32, k: u64, name: &str, statics: bool) -> Option<(u64, u64)> {
        let key = (pid, k, name.to_owned(), statics);
        if let Some((_, found)) = FIELDS.lock().unwrap().iter().find(|(k, _)| *k == key) {
            return *found;
        }
        let found = match statics {
            true => self.static_fields(k).into_iter().find(|f| f.0 == name).map(|f| (f.1, f.2)),
            false => self.chain(k).into_iter().find_map(|c| self.declared(c).into_iter().find(|f| f.2 & NOT_IN_OBJECT == 0 && f.0 == name).map(|f| (f.1, f.3))),
        };
        let mut cache = FIELDS.lock().unwrap();
        cache.retain(|(k, _)| k.0 == pid);
        cache.push((key, found));
        found
    }

    /// The kind of a MonoType (MonoTypeEnum).
    fn kind(&self, t: u64) -> Option<u8> {
        let mut b = [0u8; 1];
        self.mem.read_exact_at(&mut b, t + self.lay.w + 2).ok().map(|_| b[0])
    }

    /// A Mono array: its element class and length. An array is {vtable, sync, bounds, length,
    /// elements}; its class's first word is the element class (not itself).
    fn array(&self, a: u64) -> Option<(u64, u64)> {
        let ak = self.word(self.word(a).filter(|&v| v >= 0x10000)?).filter(|&k| k >= 0x10000)?;
        let elem = self.word(ak).filter(|&e| e != ak && self.is_class(e))?;
        let len = self.word(a + 3 * self.lay.w).filter(|&n| n < 1 << 24)?;
        Some((elem, len))
    }

    /// The object the class's static field leads to, from its static data `data`.
    fn static_target(&self, pid: u32, k: u64, data: u64, via: &Static) -> Option<u64> {
        let w = self.lay.w;
        let (off, t) = self.field(pid, k, &via.field, true)?;
        let v = self.word(data + off).filter(|&v| v >= 0x10000)?;
        let (obj, of) = match via.slot {
            None => (v, self.word(t)?),
            Some(i) => {
                if self.kind(t) != Some(TYPE_SZARRAY) {
                    return None;
                }
                let (elem, _) = self.array(v).filter(|&(e, n)| Some(e) == self.word(t) && i < n)?;
                (self.word(v + 4 * w + i * w).filter(|&o| o >= 0x10000)?, elem)
            }
        };
        self.class_of(obj).filter(|&c| self.chain(c).contains(&of)).map(|_| obj)
    }

    /// Where the field of the object a static path leads to is.
    fn static_walk(&self, pid: u32, k: u64, data: u64, path: &MonoPath) -> Option<u64> {
        let obj = self.static_target(pid, k, data, path.via.as_ref()?)?;
        Some(obj + self.field(pid, self.class_of(obj)?, &path.field, false)?.0)
    }

    /// The array slot holding the object at `x`, when it's in an array of `k`'s objects:
    /// the array and the slot.
    fn array_slot(&self, x: u64, k: u64) -> Option<(u64, u64)> {
        let w = self.lay.w;
        (0..MAX_SLOT).find_map(|i| {
            let a = x.checked_sub(4 * w + i * w)?;
            let (elem, len) = self.array(a)?;
            (i < len && self.chain(k).contains(&elem)).then_some((a, i))
        })
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
    let found: Vec<(u64, u64, MonoPath)> = FOUND.lock().unwrap().iter().filter(|f| f.0 == heap.pid).map(|f| (f.1, f.2, f.3.clone())).collect();
    for (k2, data, via) in found {
        let path = MonoPath { field: field.clone(), ..via };
        if m.static_walk(heap.pid, k2, data, &path) == Some(target) {
            return Some((path, vec![target]));
        }
    }
    let path = MonoPath { runtime: lay.runtime, namespace, class, via: None, field: field.clone() };
    let leads = walk(heap, &find_roots(heap, &path), &path);
    if leads.len() <= MAX_PLACES && leads.contains(&target) {
        return Some((path, leads));
    }
    match lay.runtime {
        Runtime::Mono => static_discover(heap, &m, obj, k, &field, target),
        Runtime::Il2cpp => None,
    }
}

/// The static field the object is kept in, itself or in a slot of an array, for a class with
/// too many objects to tell by its name (Terraria: `Main.player[0]` of 256 Players). Three
/// passes over memory: what points to the object, to the arrays holding it, and to the static
/// data holding either (from a vtable).
fn static_discover(heap: &Heap, m: &Mono, obj: u64, k: u64, field: &str, target: u64) -> Option<(MonoPath, Vec<u64>)> {
    let w = m.lay.w;
    let refs = heap.referrers(&[obj]);
    let mut arrays: Vec<(u64, u64)> = refs.iter().filter_map(|&x| m.array_slot(x, k)).collect();
    arrays.sort_unstable();
    arrays.dedup();
    // Words that may be a static field: holding the object, or an array holding it (and its slot).
    let mut holders: Vec<(u64, Option<u64>)> = refs.iter().map(|&x| (x, None)).collect();
    if !arrays.is_empty() {
        for y in heap.referrers(&arrays.iter().map(|a| a.0).collect::<Vec<_>>()) {
            if let Some(&(_, slot)) = m.word(y).and_then(|a| arrays.iter().find(|x| x.0 == a)) {
                holders.push((y, Some(slot)));
            }
        }
    }
    let starts: Vec<u64> =
        holders.iter().flat_map(|&(y, _)| (0..MAX_STATIC_DATA).step_by(w as usize).filter_map(move |o| y.checked_sub(o))).collect();
    let mut statics: Vec<(u64, Vec<(String, u64, u64)>)> = Vec::new();
    for r in heap.referrers(&starts) {
        let Some(data) = m.word(r) else { continue };
        // The vtable pointing to the static data: the nearest class pointer before (its start).
        let Some(k2) = (0..MAX_VTABLE).step_by(w as usize).find_map(|back| {
            let vt = r.checked_sub(back)?;
            m.word(vt).filter(|&c| c != vt && m.is_class(c))
        }) else {
            continue;
        };
        if !statics.iter().any(|s| s.0 == k2) {
            statics.push((k2, m.static_fields(k2)));
        }
        let fields = &statics.iter().find(|s| s.0 == k2).unwrap().1;
        for &(y, slot) in holders.iter().filter(|h| h.0 >= data && h.0 - data < MAX_STATIC_DATA) {
            let Some(f) = fields.iter().find(|f| f.1 == y - data) else { continue };
            let Some((namespace, class)) = m.name(k2) else { continue };
            let path = MonoPath { runtime: Runtime::Mono, namespace, class, via: Some(Static { field: f.0.clone(), slot }), field: field.to_owned() };
            if m.static_walk(heap.pid, k2, data, &path) == Some(target) {
                keep_static(heap.pid, k2, data);
                let mut found = FOUND.lock().unwrap();
                found.retain(|f| f.0 == heap.pid);
                found.push((heap.pid, k2, data, MonoPath { field: String::new(), ..path.clone() }));
                return Some((path, vec![target]));
            }
        }
    }
    None
}

fn keep_static(pid: u32, k: u64, data: u64) {
    let mut cache = STATICS.lock().unwrap();
    cache.retain(|s| s.0 == pid && s.1 != k);
    cache.push((pid, k, data));
}

/// The class and its static data a static path starts from: known this run, or found from
/// the class's vtables (a slot after the methods points to it).
fn find_static(heap: &Heap, path: &MonoPath) -> Option<(u64, u64)> {
    let pid = heap.pid;
    if let Some(lay) = known_layout(pid) {
        let m = Mono { mem: heap.file(), lay };
        let known = STATICS.lock().unwrap().iter().filter(|s| s.0 == pid).map(|s| (s.1, s.2)).collect::<Vec<_>>();
        if let Some(found) = known.into_iter().find(|&(k, d)| m.is(k, path) && m.static_walk(pid, k, d, path).is_some()) {
            return Some(found);
        }
    }
    let vts = vtables(heap, path);
    let m = Mono { mem: heap.file(), lay: known_layout(pid)? };
    let w = m.lay.w;
    let mut b = vec![0u8; MAX_VTABLE as usize];
    for &vt in &vts {
        let Some(k) = m.word(vt).filter(|&k| k != vt && m.is_class(k) && m.is(k, path)) else { continue };
        let n = heap.file().read_at(&mut b, vt).unwrap_or(0);
        for at in (w as usize..n - n % w as usize).step_by(w as usize) {
            let mut x = [0u8; 8];
            x[..w as usize].copy_from_slice(&b[at..at + w as usize]);
            let data = u64::from_le_bytes(x);
            if data >= 0x10000 && m.static_walk(pid, k, data, path).is_some() {
                keep_static(pid, k, data);
                return Some((k, data));
            }
        }
    }
    None
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

/// Every place these bytes are where the runtime keeps class names: Mono in writable memory
/// (Unity's reads its assemblies in) or in assemblies mapped read-only next to mscorlib.dll
/// (native Linux Terraria's; not every DLL: under Proton those are many), IL2CPP in
/// global-metadata.dat (mapped read-only).
fn find_name(heap: &Heap, runtime: Runtime, bytes: &[u8]) -> Vec<u64> {
    let mut found = Vec::new();
    let maps = crate::helper::maps(heap.pid).unwrap_or_default();
    let read_only = maps.iter().filter(|r| !r.perms.starts_with("rw"));
    let files: Vec<_> = match runtime {
        Runtime::Mono => {
            heap.each_chunk(bytes.len(), |addr, b, fresh| find_in(&mut found, bytes, addr, b, fresh));
            let corlib = maps.iter().find_map(|r| r.path.strip_suffix("/mscorlib.dll"));
            read_only
                .filter(|r| corlib.is_some_and(|dir| r.path.strip_prefix(dir).is_some_and(|f| !f[1..].contains('/'))))
                .filter(|r| [".dll", ".exe"].iter().any(|e| r.path.to_lowercase().ends_with(e)))
                .collect()
        }
        Runtime::Il2cpp => read_only.filter(|r| r.path.to_lowercase().ends_with("global-metadata.dat")).collect(),
    };
    for r in files {
        let mut b = vec![0u8; (r.end - r.start) as usize];
        let n = heap.file().read_at(&mut b, r.start).unwrap_or(0);
        find_in(&mut found, bytes, r.start, &b[..n], n);
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
    // An assembly's names share endings: Terraria.exe keeps "Player" only as the end of
    // "DeadPlayer" and others, where Mono points. The classes found are checked by name.
    let (pattern, skip): (Vec<u8>, u64) = match path.runtime {
        Runtime::Mono => ([path.class.as_bytes(), &[0]].concat(), 0),
        Runtime::Il2cpp => ([&[0u8][..], path.class.as_bytes(), &[0]].concat(), 1),
    };
    let names: Vec<u64> = find_name(heap, path.runtime, &pattern).into_iter().map(|a| a + skip).collect();
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
    if path.via.is_some() {
        return find_static(heap, path).map(|(_, data)| vec![data]).unwrap_or_default();
    }
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
    // A static path: its root is the class's static data.
    if path.via.is_some() {
        let pid = heap.pid;
        let class = roots.first().and_then(|&d| STATICS.lock().unwrap().iter().find(|s| s.0 == pid && s.2 == d).map(|s| (s.1, d)));
        return class.and_then(|(k, d)| m.static_walk(pid, k, d, path)).into_iter().collect();
    }
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
        let t = "mono:Terraria::Main.player[0].statLife";
        let p = MonoPath::parse(t).unwrap();
        assert_eq!((p.class.as_str(), p.via.clone(), p.field.as_str()), ("Main", Some(Static { field: "player".into(), slot: Some(0) }), "statLife"));
        assert_eq!((p.text(), p.describe()), (t.to_owned(), "statLife of Main.player[0]".to_owned()));
        assert_eq!(MonoPath::parse("mono:Game.instance.gold").unwrap().text(), "mono:Game.instance.gold");
        assert!(MonoPath::parse("mono:Main.player[x].statLife").is_none());
        assert!(MonoPath::parse("il2cpp:Main.player[0].statLife").is_none());
        let p = MonoPath::parse("il2cpp:CommandBase._ammo").unwrap();
        assert_eq!((p.runtime, p.describe()), (Runtime::Il2cpp, "_ammo of CommandBase".to_owned()));
        assert_eq!(p.text(), "il2cpp:CommandBase._ammo");
    }
}
