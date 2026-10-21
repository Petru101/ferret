//! Adobe AIR games: ActionScript 3 on Adobe's (now HARMAN's) virtual machine, avmplus, inside
//! the game's own copy of the runtime (`Adobe AIR.dll`, a Windows program run through Wine).
//! Read from outside by the layout of avmplus's C++ objects (Adobe published its source; AIR 23
//! keeps that layout). A value is named by a class's static field and the fields from there:
//! `air:com.wbwar.creeper.Creeper#instance.gameScreen.gameSpace.stash`, `[i]` for an Array's
//! element.
//!
//! An object starts {C++ vtable, reference count, VTable*, prototype}; its VTable points to its
//! class's Traits (name, package, base class, the pool of bytecode it comes from). A field's
//! place isn't kept anywhere the VM is sure to hold on to: its table of names and places
//! (TraitsBindings) is dropped by the collector and rebuilt when needed (most classes have none
//! at any moment). So the places are worked out the way the VM does, from the class's
//! declaration in the game's bytecode (ABC, kept in memory with its pool): ints, uints and
//! Booleans first, then references, then Numbers (8-aligned), each group in declaration order,
//! after the base class's fields. Where a table is alive it is compared (`agrees`); one that
//! disagrees refuses the whole game.
//!
//! Where things start: a class's Traits points to itself among its supertypes (pass 1 finds
//! them by that, and the VM core and bytecode pools from them); an AbcEnv points to a pool and
//! to its DomainEnv (pass 2), whose map of scripts gives every initialised script's global
//! object; a global's fields are the script's classes (ClassClosure objects), whose fields are
//! the classes' statics. The collector doesn't move objects: a found place stays until its
//! object is collected, and limits walk the path again every 250 ms.

use crate::mono::player_like;
use crate::names::Heap;
use crate::ue::{Listed, ListedObject, MAX_LISTED};
use std::collections::{HashMap, HashSet, VecDeque};
use crate::mem::Mem;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Objects looked into for one list at most.
const MAX_VISITED: usize = 20000;
/// How deep the list goes from a static field.
const MAX_DEPTH: usize = 12;
/// Elements read from one Array at most.
const MAX_ELEMENTS: u32 = 256;
/// Bytes of one pool's bytecode read at most.
const MAX_ABC: usize = 64 << 20;
/// Packages that aren't the game's: Adobe's own (the player's API, the Flex framework compiled
/// into the game's SWF) and common libraries; their statics and objects aren't listed.
const LIBRARIES: &[&str] = &[
    "flash.", "mx.", "spark.", "air.", "adobe.", "com.adobe.", "fl.", "avmplus", "__AS3__", "com.amanitadesign.", "com.greensock.",
    "caurina.", "com.freeactionscript.", "org.osmf.", "com.demonsters.", "com.junkbyte.", "nape.", "Box2D.", "starling.", "feathers.",
];

// The 32-bit layout (avmplus's headers, MSVC, the shipped runtime's release build with its
// debugger support: AIR 23).
const W: u64 = 4;
/// ScriptObject: {C++ vtable, composite, VTable*, delegate}, then its fields.
const OBJ_VTABLE: u64 = 8;
/// VTable: {C++ vtable, toplevel, init (MethodEnv), base, ivtable, traits}.
const VT_TRAITS: u64 = 20;
/// Traits.
const TR_CORE: u64 = 4;
const TR_BASE: u64 = 8;
const TR_PRIMARY: u64 = 24;
const TR_POOL: u64 = 60;
const TR_NS: u64 = 68;
const TR_NAME: u64 = 72;
const TR_POS: u64 = 88;
const TR_TBREF: u64 = 100;
const TR_OFFSETOF_SLOTS: u64 = 118;
const TR_HASHTABLE: u64 = 120;
const TR_TOTAL: u64 = 124;
const TR_POSTYPE: u64 = 129;
/// TraitsBindings: {.., owner +8, base +12, bindings +16, slot count +20, .., slots +36}.
const TB_BINDINGS: u64 = 16;
const TB_SLOTS: u64 = 20;
const TB_SLOT0: u64 = 36;
/// String: {.., buffer +8, extra (master) +12, length +16, flags +20}.
const STR_BUF: u64 = 8;
const STR_EXTRA: u64 = 12;
const STR_LEN: u64 = 16;
const STR_FLAGS: u64 = 20;
/// A ScriptEnv's global object.
const SE_GLOBAL: u64 = 20;
/// AbcEnv: {.., pool +4, DomainEnv +8, .., core +20}.
const AE_POOL: u64 = 4;
const AE_DOMAINENV: u64 = 8;
const AE_CORE: u64 = 20;
/// DomainEnv: {.., script map +4, .., base count +32, bases +36 (itself first)}.
const DE_MAP: u64 = 4;
const DE_BASES: u64 = 32;
/// ArrayObject: dense elements' list +20 ({.., count +4, atoms +8}), first dense index +28,
/// dense count +32.
const ARR_LIST: u64 = 20;
const ARR_START: u64 = 28;
const ARR_USED: u64 = 32;
/// Traits position kinds (`TraitsPosType`).
const POS_SCRIPT: u8 = 2;
/// Atom tags.
const ATOM_OBJECT: u64 = 1;

fn read(mem: &Mem, at: u64, n: usize) -> Option<Vec<u8>> {
    if !(0x10000..1 << 32).contains(&at) {
        return None;
    }
    let mut b = vec![0u8; n];
    (mem.read_at(&mut b, at).ok()? == n).then_some(b)
}

fn u(mem: &Mem, at: u64) -> Option<u64> {
    read(mem, at, 4).map(|b| u64::from(u32::from_le_bytes(b.try_into().unwrap())))
}

/// A pointer field: non-null, and 4-aligned.
fn ptr(mem: &Mem, at: u64) -> Option<u64> {
    u(mem, at).filter(|&p| p >= 0x10000 && p % 4 == 0)
}

fn u16_at(mem: &Mem, at: u64) -> Option<u64> {
    read(mem, at, 2).map(|b| u64::from(u16::from_le_bytes(b.try_into().unwrap())))
}

/// An avmplus String's text (8- or 16-bit, its own buffer or a part of another's).
fn string(mem: &Mem, s: u64) -> Option<String> {
    if s < 0x10000 || !s.is_multiple_of(4) {
        return None;
    }
    let len = u(mem, s + STR_LEN)? as usize;
    let flags = u(mem, s + STR_FLAGS)?;
    if len > 4096 {
        return None;
    }
    let mut buf = u(mem, s + STR_BUF)?;
    if (flags >> 1) & 3 == 2 {
        // Dependent: an offset into its master's buffer.
        buf += u(mem, u(mem, s + STR_EXTRA)? + STR_BUF)?;
    }
    let wide = flags & 1 == 1;
    let b = read(mem, buf, len * if wide { 2 } else { 1 })?;
    let t = match wide {
        true => String::from_utf16(&b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()).ok()?,
        false => b.iter().map(|&c| char::from(c)).collect(),
    };
    t.chars().all(|c| !c.is_control()).then_some(t)
}

/// Whether the game runs on the AIR runtime: its `Adobe AIR.dll` mapped (a game's captive copy
/// or the shared runtime's).
pub fn runs_air(regions: &[crate::helper::Region]) -> bool {
    regions.iter().any(|r| r.path.rsplit(['/', '\\']).next().is_some_and(|n| n.eq_ignore_ascii_case("Adobe AIR.dll")))
}

/// A multiname of the bytecode: a qualified name's namespace (an index into the namespaces)
/// and name, or another kind's name only.
#[derive(Clone, Debug)]
enum Mn {
    Q(u32, u32),
    Name(u32),
    /// `Vector.<T>` and other parameterised types.
    Type,
    None,
}

/// A trait of a class, an instance or a script: a field (var 0, const 6) with its type, a
/// class (4), or a method, getter, setter or function (left out).
#[derive(Clone, Debug)]
struct Trait {
    kind: u8,
    name: u32,
    ty: u32,
}

/// One pool's bytecode (an ABC block), parsed as far as names and declarations go.
pub struct Abc {
    start: u64,
    strings: Vec<String>,
    /// Namespace: kind and name (a package's name, or a private namespace's).
    ns: Vec<(u8, u32)>,
    mn: Vec<Mn>,
    /// Instance, class and script declarations by where they start (Traits point there).
    by_pos: HashMap<u32, Vec<Trait>>,
    /// The instance declarations' names (multinames): package and class of each class.
    classes: Vec<u32>,
    /// Adobe's own pool (the player's API: it declares flash.* classes or `Object`).
    builtin: bool,
}

/// Reads a pool's bytecode from the game's memory as the parser needs it.
struct Cur<'a> {
    mem: &'a Mem,
    at: u64,
    buf: Vec<u8>,
    p: usize,
}

impl Cur<'_> {
    fn byte(&mut self) -> Option<u8> {
        if self.p >= self.buf.len() {
            if self.buf.len() >= MAX_ABC {
                return None;
            }
            let mut more = vec![0u8; 256 << 10];
            let n = self.mem.read_at(&mut more, self.at + self.buf.len() as u64).ok()?;
            if n == 0 {
                return None;
            }
            self.buf.extend_from_slice(&more[..n]);
        }
        self.p += 1;
        Some(self.buf[self.p - 1])
    }

    fn u30(&mut self) -> Option<u32> {
        let mut r: u32 = 0;
        for i in 0..5 {
            let b = self.byte()?;
            r |= u32::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                break;
            }
        }
        Some(r)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        for _ in 0..n {
            self.byte()?;
        }
        Some(())
    }

    fn u30s(&mut self, n: u32) -> Option<()> {
        for _ in 0..n {
            self.u30()?;
        }
        Some(())
    }
}

impl Abc {
    /// The block starting at `start` (its version, 46.16).
    fn parse(mem: &Mem, start: u64) -> Option<Abc> {
        let mut c = Cur { mem, at: start, buf: Vec::new(), p: 4 };
        let n = c.u30()?;
        c.u30s(n.saturating_sub(1))?;
        let n = c.u30()?;
        c.u30s(n.saturating_sub(1))?;
        let n = c.u30()?;
        c.skip(8 * n.saturating_sub(1) as usize)?;
        let n = c.u30()?;
        let mut strings = vec![String::new()];
        for _ in 1..n {
            let len = c.u30()? as usize;
            let mut b = Vec::with_capacity(len);
            for _ in 0..len {
                b.push(c.byte()?);
            }
            strings.push(String::from_utf8_lossy(&b).into_owned());
        }
        let n = c.u30()?;
        let mut ns = vec![(0, 0)];
        for _ in 1..n {
            let k = c.byte()?;
            ns.push((k, c.u30()?));
        }
        let n = c.u30()?;
        for _ in 1..n {
            let k = c.u30()?;
            c.u30s(k)?;
        }
        let n = c.u30()?;
        let mut mn = vec![Mn::None];
        for _ in 1..n {
            let m = match c.byte()? {
                0x07 | 0x0d => Mn::Q(c.u30()?, c.u30()?),
                0x0f | 0x10 => Mn::Name(c.u30()?),
                0x11 | 0x12 => Mn::None,
                0x09 | 0x0e => {
                    let name = c.u30()?;
                    c.u30()?;
                    Mn::Name(name)
                }
                0x1b | 0x1c => {
                    c.u30()?;
                    Mn::None
                }
                0x1d => {
                    c.u30()?;
                    let k = c.u30()?;
                    c.u30s(k)?;
                    Mn::Type
                }
                _ => return None,
            };
            mn.push(m);
        }
        // Methods: their signatures only.
        let n = c.u30()?;
        for _ in 0..n {
            let params = c.u30()?;
            c.u30()?;
            c.u30s(params)?;
            c.u30()?;
            let flags = c.byte()?;
            if flags & 0x08 != 0 {
                for _ in 0..c.u30()? {
                    c.u30()?;
                    c.byte()?;
                }
            }
            if flags & 0x80 != 0 {
                c.u30s(params)?;
            }
        }
        // Metadata.
        let n = c.u30()?;
        for _ in 0..n {
            c.u30()?;
            let k = c.u30()?;
            c.u30s(2 * k)?;
        }
        let n = c.u30()?;
        let mut by_pos = HashMap::new();
        let mut classes = Vec::new();
        for _ in 0..n {
            let pos = c.p as u32;
            let name = c.u30()?;
            c.u30()?;
            if c.byte()? & 0x08 != 0 {
                c.u30()?;
            }
            let k = c.u30()?;
            c.u30s(k)?;
            c.u30()?;
            by_pos.insert(pos, traits(&mut c)?);
            classes.push(name);
        }
        for _ in 0..n {
            let pos = c.p as u32;
            c.u30()?;
            by_pos.insert(pos, traits(&mut c)?);
        }
        let n = c.u30()?;
        for _ in 0..n {
            let pos = c.p as u32;
            c.u30()?;
            by_pos.insert(pos, traits(&mut c)?);
        }
        let mut abc = Abc { start, strings, ns, mn, by_pos, classes, builtin: false };
        abc.builtin = abc.classes.iter().any(|&m| {
            let (pkg, name) = abc.qname(m);
            pkg.starts_with("flash.") || pkg.is_empty() && name == "Object"
        });
        Some(abc)
    }

    fn string(&self, i: u32) -> &str {
        self.strings.get(i as usize).map_or("", String::as_str)
    }

    /// A multiname's name.
    fn name(&self, m: u32) -> &str {
        match self.mn.get(m as usize) {
            Some(Mn::Q(_, n) | Mn::Name(n)) => self.string(*n),
            _ => "",
        }
    }

    /// A qualified name's package (its namespace's name) and name.
    fn qname(&self, m: u32) -> (&str, &str) {
        match self.mn.get(m as usize) {
            Some(Mn::Q(ns, n)) => (self.ns.get(*ns as usize).map_or("", |&(_, s)| self.string(s)), self.string(*n)),
            Some(Mn::Name(n)) => ("", self.string(*n)),
            _ => ("", ""),
        }
    }

    /// How a field of this declared type is stored.
    fn slot_type(&self, m: u32) -> Ty {
        match self.qname(m) {
            ("", "Number") => Ty::F64,
            ("", "int") => Ty::I32,
            ("", "uint") => Ty::U32,
            ("", "Boolean") => Ty::Bool,
            ("", "String") => Ty::Str,
            ("", "Namespace") => Ty::Other,
            ("", "" | "*" | "Object") => Ty::Atom,
            _ if m == 0 => Ty::Atom,
            _ => Ty::Obj,
        }
    }
}

fn traits(c: &mut Cur) -> Option<Vec<Trait>> {
    let n = c.u30()?;
    let mut out = Vec::new();
    for _ in 0..n {
        let name = c.u30()?;
        let kb = c.byte()?;
        let kind = kb & 0x0f;
        match kind {
            0 | 6 => {
                c.u30()?;
                let ty = c.u30()?;
                if c.u30()? != 0 {
                    c.byte()?;
                }
                out.push(Trait { kind, name, ty });
            }
            1..=5 => {
                c.u30()?;
                let x = c.u30()?;
                out.push(Trait { kind, name, ty: x });
            }
            _ => return None,
        }
        if kb & 0x40 != 0 {
            let k = c.u30()?;
            c.u30s(k)?;
        }
    }
    Some(out)
}

/// How a field is stored.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Ty {
    F64,
    I32,
    U32,
    Bool,
    Str,
    /// A reference to an object of a declared class.
    Obj,
    /// An untyped field (`*`, Object): an atom (a tagged value).
    Atom,
    Other,
}

impl Ty {
    fn bytes(self) -> u64 {
        match self {
            Ty::F64 => 8,
            _ => 4,
        }
    }

    /// The group the VM lays it out in: 32-bit numbers, references, Numbers.
    fn group(self) -> usize {
        match self {
            Ty::I32 | Ty::U32 | Ty::Bool => 0,
            Ty::F64 => 2,
            _ => 1,
        }
    }
}

/// A field of an object or a class, where it is and how it's stored.
#[derive(Clone, Debug)]
struct Slot {
    name: String,
    off: u64,
    ty: Ty,
    /// Declared by a library's class (a Sprite's own fields in a game's class).
    library: bool,
    /// A class (a script's field holding a ClassClosure).
    class: bool,
}

/// What a field or element holds, for listing and walking.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Val {
    /// A number: its address and the helper's value type.
    Num(u64, &'static str),
    Obj(u64),
    Other,
}

/// A class the game has initialised: the global object holding it and where.
#[derive(Clone, Debug)]
struct Class {
    global: u64,
    off: u64,
    library: bool,
}

/// The located VM: its core, the pools found, the domains, and the classes initialised so far.
pub struct Air {
    core: u64,
    domains: Mutex<Vec<u64>>,
    pools: Mutex<HashMap<u64, Option<Arc<Abc>>>>,
    layouts: Mutex<HashMap<u64, Arc<Vec<Slot>>>>,
    classes: Mutex<(HashMap<String, Class>, Instant)>,
    /// When the domains were last looked for (a SWF loaded later brings its own).
    domains_at: Mutex<Instant>,
}

fn is_library(name: &str) -> bool {
    LIBRARIES.iter().any(|p| name.starts_with(p))
}

impl Air {
    fn locate(heap: &Heap) -> Option<Air> {
        if heap.width() != 4 {
            return None;
        }
        let mem = heap.mem();
        // Pass 1: Traits, by pointing to themselves among their primary supertypes (the slot
        // for their own depth, after their base's), with a core and a pool.
        let mut found: HashMap<u64, usize> = HashMap::new();
        let mut pools: HashMap<u64, u64> = HashMap::new();
        heap.each_chunk(64, |addr, b, fresh| {
            let word = |o: usize| u64::from(u32::from_le_bytes(b[o..o + 4].try_into().unwrap()));
            for off in (0..fresh.min(b.len().saturating_sub(64))).step_by(8) {
                let t = addr + off as u64;
                let base = word(off + TR_BASE as usize);
                let mut is = false;
                for k in 0..8 {
                    let at = off + (TR_PRIMARY + W * k) as usize;
                    if word(at) == t {
                        is = if k == 0 { base == 0 } else { word(at - 4) == base && base != 0 };
                        break;
                    }
                }
                if !is {
                    continue;
                }
                let core = word(off + TR_CORE as usize);
                let pool = word(off + TR_POOL as usize);
                if core < 0x10000 || core % 4 != 0 || pool < 0x10000 || pool % 4 != 0 {
                    continue;
                }
                *found.entry(core).or_default() += 1;
                pools.insert(pool, core);
            }
        });
        let core = found.into_iter().max_by_key(|&(_, n)| n).filter(|&(_, n)| n >= 50)?.0;
        let pools: Vec<u64> = pools.into_iter().filter(|&(p, c)| c == core && u(mem, p + 4) == Some(core)).map(|(p, _)| p).collect();
        let air = Air {
            core,
            domains: Mutex::new(Vec::new()),
            pools: Mutex::new(HashMap::new()),
            layouts: Mutex::new(HashMap::new()),
            classes: Mutex::new((HashMap::new(), Instant::now() - Duration::from_secs(60))),
            domains_at: Mutex::new(Instant::now()),
        };
        air.find_domains(heap, &pools);
        if air.domains.lock().unwrap().is_empty() {
            return None;
        }
        air.index(mem);
        // The places worked out must agree with every table the VM has now.
        let classes: Vec<Class> = air.classes.lock().unwrap().0.values().cloned().collect();
        let mut checked = HashSet::new();
        for c in classes {
            for o in [Some(c.global), ptr(mem, c.global + c.off)].into_iter().flatten() {
                let Some(tr) = air.traits_of(mem, o) else { continue };
                if checked.insert(tr) && air.agrees(mem, tr) == Some(false) {
                    return None;
                }
            }
        }
        Some(air)
    }

    /// Pass 2: AbcEnvs (a pool of ours, our core, and a DomainEnv listing itself first among
    /// its bases).
    fn find_domains(&self, heap: &Heap, pools: &[u64]) {
        let mem = heap.mem();
        let mut out = Vec::new();
        for at in heap.referrers(pools) {
            let env = at - AE_POOL;
            if u(mem, env + AE_CORE) != Some(self.core) {
                continue;
            }
            let Some(de) = ptr(mem, env + AE_DOMAINENV) else { continue };
            let n = u(mem, de + DE_BASES).unwrap_or(0);
            if (1..=64).contains(&n) && u(mem, de + DE_BASES + W) == Some(de) && !out.contains(&de) {
                out.push(de);
            }
        }
        if !out.is_empty() {
            *self.domains.lock().unwrap() = out;
        }
        *self.domains_at.lock().unwrap() = Instant::now();
    }

    /// The pool's bytecode (parsed once).
    fn abc(&self, mem: &Mem, pool: u64) -> Option<Arc<Abc>> {
        if let Some(a) = self.pools.lock().unwrap().get(&pool) {
            return a.clone();
        }
        // The pool points to its block just past the version (46.16, or 47.16).
        let abc = (0..0x100).step_by(4).find_map(|k| {
            let p = u(mem, pool + k)?;
            let v = read(mem, p.checked_sub(4)?, 4)?;
            (v[..] == [16, 0, 46, 0] || v[..] == [16, 0, 47, 0]).then(|| p - 4)
        });
        let abc = abc.and_then(|a| Abc::parse(mem, a)).map(Arc::new);
        self.pools.lock().unwrap().insert(pool, abc.clone());
        abc
    }

    /// An object's Traits, checked: its VTable's Traits belongs to our core, from a pool of it.
    fn traits_of(&self, mem: &Mem, o: u64) -> Option<u64> {
        if !o.is_multiple_of(8) {
            return None;
        }
        let tr = ptr(mem, ptr(mem, o + OBJ_VTABLE)? + VT_TRAITS)?;
        self.is_traits(mem, tr).then_some(tr)
    }

    fn is_traits(&self, mem: &Mem, tr: u64) -> bool {
        u(mem, tr + TR_CORE) == Some(self.core) && ptr(mem, tr + TR_POOL).is_some_and(|p| u(mem, p + 4) == Some(self.core))
    }

    /// A class's name as its Traits has it (`GameSpace`, `Creeper$` for the class itself).
    fn class_name(&self, mem: &Mem, tr: u64) -> Option<String> {
        string(mem, ptr(mem, tr + TR_NAME)?)
    }

    /// The Traits's declaration in its pool's bytecode.
    fn declaration(&self, mem: &Mem, tr: u64) -> Option<(Arc<Abc>, u32)> {
        let abc = self.abc(mem, ptr(mem, tr + TR_POOL)?)?;
        let pos = u(mem, tr + TR_POS)?.checked_sub(abc.start)? as u32;
        abc.by_pos.contains_key(&pos).then_some((abc, pos))
    }

    /// The fields one class declares, where the VM puts them (see the module's notes).
    fn own_slots(&self, mem: &Mem, tr: u64) -> Option<Vec<Slot>> {
        let (abc, pos) = self.declaration(mem, tr)?;
        let decl = &abc.by_pos[&pos];
        let base = ptr(mem, tr + TR_BASE);
        let mut start = u16_at(mem, tr + TR_OFFSETOF_SLOTS)?;
        if start == 0 {
            if let Some(b) = base {
                start = u(mem, b + TR_HASHTABLE).filter(|&h| h != 0).or_else(|| u(mem, b + TR_TOTAL))?;
            }
        }
        let fields: Vec<(&Trait, Ty)> = decl
            .iter()
            .filter_map(|t| match t.kind {
                0 | 6 => Some((t, abc.slot_type(t.ty))),
                4 => Some((t, Ty::Obj)),
                _ => None,
            })
            .collect();
        let count = |g| fields.iter().filter(|(_, ty)| ty.group() == g).count() as u64;
        let mut next = [start, start + 4 * count(0), 0];
        next[2] = next[1] + 4 * count(1);
        if count(2) > 0 {
            next[2] = (next[2] + 7) & !7;
        }
        let library = self.library_class(mem, tr);
        let mut out = Vec::new();
        for (t, ty) in fields {
            let g = ty.group();
            out.push(Slot { name: abc.name(t.name).to_owned(), off: next[g], ty, library, class: t.kind == 4 });
            next[g] += ty.bytes();
        }
        Some(out)
    }

    /// Every field of an object of this class, the most derived class's first (a private field
    /// may share a name with one of a base class: the class's own is the one a path means).
    fn slots(&self, mem: &Mem, tr: u64) -> Option<Arc<Vec<Slot>>> {
        if let Some(s) = self.layouts.lock().unwrap().get(&tr) {
            return Some(s.clone());
        }
        let mut out = Vec::new();
        let mut t = Some(tr);
        let mut depth = 0;
        while let Some(x) = t {
            depth += 1;
            if depth > 64 || !self.is_traits(mem, x) {
                return None;
            }
            out.extend(self.own_slots(mem, x)?);
            t = ptr(mem, x + TR_BASE);
        }
        let out = Arc::new(out);
        self.layouts.lock().unwrap().insert(tr, out.clone());
        Some(out)
    }

    /// A class of Adobe's or a library's (not the game's own).
    fn library_class(&self, mem: &Mem, tr: u64) -> bool {
        self.declaration(mem, tr).is_none_or(|(a, _)| a.builtin) || is_library(&self.package_of(mem, tr).unwrap_or_default())
    }

    /// The package a class is in: its namespace's name.
    fn package_of(&self, mem: &Mem, tr: u64) -> Option<String> {
        // Namespace: {C++ vtable, prefix, uri and type (a String, 3 tag bits)}.
        string(mem, u(mem, ptr(mem, tr + TR_NS)? + 8)? & !7)
    }

    /// Whether the VM's own table of this class's field places, where it has one now, says
    /// what `own_slots` works out: Some(false) when a name sits elsewhere.
    fn agrees(&self, mem: &Mem, tr: u64) -> Option<bool> {
        let tb = ptr(mem, ptr(mem, tr + TR_TBREF)?)?;
        let mine = self.own_slots(mem, tr)?;
        // Bindings: {.., quads +4, .., capacity +12}; quads {.., entries +8}: {name, ns, binding,
        // api}, binding = slot << 3 | 2 (var) or 3 (const).
        let ht = ptr(mem, tb + TB_BINDINGS)?;
        let quads = ptr(mem, ht + 4)?;
        let n = u(mem, ht + 12)?.min(1 << 16);
        let slots = u(mem, tb + TB_SLOTS)?.min(1 << 16);
        let mut theirs: HashMap<String, Vec<u64>> = HashMap::new();
        for i in 0..n {
            let q = read(mem, quads + 8 + 16 * i, 16)?;
            let w = |k: usize| u64::from(u32::from_le_bytes(q[k..k + 4].try_into().unwrap()));
            let (name, b) = (w(0), w(8));
            if name == 0 || !(2..=3).contains(&(b & 7)) || b >> 3 >= slots {
                continue;
            }
            let at = u(mem, tb + TB_SLOT0 + 8 * (b >> 3) + 4)? >> 4 << 2;
            theirs.entry(string(mem, name)?).or_default().push(at);
        }
        // The table holds the base classes' names too (a private one may be declared again).
        Some(mine.iter().all(|s| theirs.get(&s.name).is_none_or(|at| at.contains(&s.off))))
    }

    /// The script globals of every domain, and the classes they hold, by qualified name.
    fn index(&self, mem: &Mem) {
        let mut classes = HashMap::new();
        for de in self.domains.lock().unwrap().clone() {
            for g in self.globals(mem, de) {
                let Some(tr) = self.traits_of(mem, g) else { continue };
                if read(mem, tr + TR_POSTYPE, 1).map(|b| b[0]) != Some(POS_SCRIPT) {
                    continue;
                }
                let Some((abc, pos)) = self.declaration(mem, tr) else { continue };
                let Some(slots) = self.own_slots(mem, tr) else { continue };
                let decl = &abc.by_pos[&pos];
                for (t, s) in decl.iter().filter(|t| matches!(t.kind, 0 | 4 | 6)).zip(slots.iter()) {
                    if !s.class {
                        continue;
                    }
                    let (pkg, name) = abc.qname(t.name);
                    let full = if pkg.is_empty() { name.to_owned() } else { format!("{pkg}.{name}") };
                    let library = abc.builtin || is_library(&full);
                    classes.insert(full, Class { global: g, off: s.off, library });
                }
            }
        }
        *self.classes.lock().unwrap() = (classes, Instant::now());
    }

    /// A domain's script globals: its map from script to ScriptEnv (an InlineHashtable of
    /// atom pairs, after one header word), each ScriptEnv's global (null until the script runs).
    fn globals(&self, mem: &Mem, de: u64) -> Vec<u64> {
        let Some(map) = ptr(mem, de + DE_MAP) else { return Vec::new() };
        let (Some(atoms), Some(size)) = (u(mem, map + 4), u(mem, map + 8)) else { return Vec::new() };
        let log = size >> 27;
        if log == 0 || log > 20 {
            return Vec::new();
        }
        let cap = 1u64 << (log - 1);
        let Some(b) = read(mem, (atoms & !7) + W, (cap * 4) as usize) else { return Vec::new() };
        let mut out = Vec::new();
        for pair in b.chunks(8) {
            let key = u32::from_le_bytes(pair[..4].try_into().unwrap());
            let v = u64::from(u32::from_le_bytes(pair[4..].try_into().unwrap()));
            if key > 7 && v > 7 {
                if let Some(g) = ptr(mem, (v & !7) + SE_GLOBAL) {
                    out.push(g);
                }
            }
        }
        out
    }

    /// The object holding a class's statics (its ClassClosure), looking again at most every
    /// 2 s when it isn't known (classes are made as their scripts first run), and for new
    /// domains at most every 10 s.
    fn class(&self, heap: &Heap, name: &str) -> Option<u64> {
        let mem = heap.mem();
        let known = |air: &Air| {
            let c = air.classes.lock().unwrap().0.get(name).cloned()?;
            let o = ptr(mem, c.global + c.off)?;
            let tr = air.traits_of(mem, o)?;
            let short = name.rsplit('.').next().unwrap_or(name);
            (air.class_name(mem, tr)? == format!("{short}$")).then_some(o)
        };
        if let Some(o) = known(self) {
            return Some(o);
        }
        if self.classes.lock().unwrap().1.elapsed() < Duration::from_secs(2) {
            return None;
        }
        if self.domains_at.lock().unwrap().elapsed() > Duration::from_secs(10) {
            let pools: Vec<u64> = self.pools.lock().unwrap().keys().copied().collect();
            if !pools.is_empty() {
                self.find_domains(heap, &pools);
            }
        }
        self.index(mem);
        known(self)
    }

    /// What a field of an object holds.
    fn val(&self, mem: &Mem, o: u64, s: &Slot) -> Val {
        let at = o + s.off;
        match s.ty {
            Ty::F64 => Val::Num(at, "f64"),
            Ty::I32 | Ty::U32 => Val::Num(at, "i32"),
            Ty::Obj => ptr(mem, at).map_or(Val::Other, Val::Obj),
            Ty::Atom => match u(mem, at) {
                Some(a) if a & 7 == ATOM_OBJECT && a > 7 => Val::Obj(a & !7),
                _ => Val::Other,
            },
            _ => Val::Other,
        }
    }

    /// An Array's elements (up to `MAX_ELEMENTS`) that are objects, by index.
    fn elements(&self, mem: &Mem, a: u64) -> Vec<(u32, u64)> {
        let (Some(list), Some(start), Some(used)) = (ptr(mem, a + ARR_LIST), u(mem, a + ARR_START), u(mem, a + ARR_USED)) else { return Vec::new() };
        if start != 0 || u(mem, list + 4) != Some(used) {
            return Vec::new();
        }
        let n = used.min(u64::from(MAX_ELEMENTS)) as u32;
        let Some(b) = read(mem, list + 8, 4 * n as usize) else { return Vec::new() };
        b.chunks(4)
            .enumerate()
            .filter_map(|(i, c)| {
                let a = u64::from(u32::from_le_bytes(c.try_into().unwrap()));
                (a & 7 == ATOM_OBJECT && a > 7).then_some((i as u32, a & !7))
            })
            .collect()
    }

    /// Whether the object is an Array (Adobe's own `Array`).
    fn is_array(&self, mem: &Mem, tr: u64) -> bool {
        self.class_name(mem, tr).as_deref() == Some("Array") && self.declaration(mem, tr).is_some_and(|(a, _)| a.builtin)
    }
}

/// The located VM of the game attached to, per pid; a failed search is tried again after
/// `RETRY`.
pub fn shared(heap: &Heap) -> Option<Arc<Air>> {
    const RETRY: Duration = Duration::from_secs(10);
    static FOUND: Mutex<Option<(u32, Option<Arc<Air>>, Instant)>> = Mutex::new(None);
    let mut found = FOUND.lock().unwrap();
    if let Some((p, air, at)) = found.as_ref() {
        if *p == heap.pid && (air.is_some() || at.elapsed() < RETRY) {
            return air.clone();
        }
    }
    let air = Air::locate(heap).map(Arc::new);
    *found = Some((heap.pid, air.clone(), Instant::now()));
    air
}

/// The last part of a class name: `com.wbwar.creeper.GameSpace` -> `GameSpace`.
fn short_name(class: &str) -> &str {
    class.rsplit('.').next().unwrap_or(class)
}

/// One step of a path: a field by name, or an Array's element.
#[derive(Clone, Debug, PartialEq)]
enum Step {
    Field(String),
    Index(u32),
}

/// A saved path from a class's static field to a number.
#[derive(Clone, Debug, PartialEq)]
pub struct AirPath {
    /// The class, with its package (`com.wbwar.creeper.Creeper`).
    class: String,
    steps: Vec<Step>,
}

impl AirPath {
    /// "air:<package.Class>#<static>[.<field>|[i]]...".
    pub fn parse(text: &str) -> Option<AirPath> {
        let (class, rest) = text.strip_prefix("air:")?.split_once('#')?;
        if class.is_empty() || class.contains([' ', '[', ']']) {
            return None;
        }
        let mut steps = Vec::new();
        for part in rest.split('.') {
            let (name, idx) = part.split_once('[').map_or((part, None), |(n, i)| (n, Some(i)));
            if name.is_empty() {
                return None;
            }
            steps.push(Step::Field(name.to_owned()));
            if let Some(idx) = idx {
                for i in idx.split('[') {
                    steps.push(Step::Index(i.strip_suffix(']')?.parse().ok()?));
                }
            }
        }
        Some(AirPath { class: class.to_owned(), steps })
    }

    pub fn text(&self) -> String {
        format!("air:{}#{}", self.class, self.fields_text())
    }

    fn fields_text(&self) -> String {
        let mut t = String::new();
        for s in &self.steps {
            match s {
                Step::Field(n) if t.is_empty() => t += n,
                Step::Field(n) => t += &format!(".{n}"),
                Step::Index(i) => t += &format!("[{i}]"),
            }
        }
        t
    }

    /// As the list shows it: the class's short name and the fields (`Creeper.instance.gameScreen`).
    fn shown(&self) -> String {
        match self.steps.is_empty() {
            true => short_name(&self.class).to_owned(),
            false => format!("{}.{}", short_name(&self.class), self.fields_text()),
        }
    }

    pub fn describe(&self) -> String {
        format!("{} in the game's ActionScript objects", self.shown())
    }

    fn then(&self, s: Step) -> AirPath {
        let mut p = self.clone();
        p.steps.push(s);
        p
    }

    /// The path up to its first player-like field, if it goes through one.
    fn player_part(&self) -> Option<AirPath> {
        let i = self.steps.iter().position(|s| matches!(s, Step::Field(n) if the_player(n)))?;
        Some(AirPath { class: self.class.clone(), steps: self.steps[..=i].to_vec() })
    }

    /// Whether the path goes through how something looks or sounds, or the game's menus and
    /// panels (`controlPanel.stashViewer.bar`): listed, not among the player's own numbers.
    fn presentation(&self) -> bool {
        self.steps.iter().any(|s| matches!(s, Step::Field(n) if presentation(n)))
    }

    fn last_field(&self) -> Option<&str> {
        self.steps.iter().rev().find_map(|s| match s {
            Step::Field(n) => Some(n.as_str()),
            _ => None,
        })
    }
}

/// A name's words, lowercase (`stashViewer` -> stash, viewer; `HEALTH_BAR` -> health, bar).
fn words(name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            prev_lower = false;
            out.push(String::new());
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower || out.is_empty() {
            out.push(String::new());
        }
        prev_lower = c.is_ascii_lowercase();
        out.last_mut().unwrap().push(c.to_ascii_lowercase());
    }
    out.retain(|w| !w.is_empty());
    out
}

fn presentation(name: &str) -> bool {
    const WORDS: &[&str] = &[
        "sprite", "bitmap", "image", "img", "shape", "graphics", "sound", "channel", "music", "texture", "matrix", "filter", "font", "text", "label",
        "button", "btn", "dialog", "panel", "menu", "tooltip", "viewer", "legend", "indicator", "bar", "icon", "cursor", "tween", "layer", "camera",
        "particle", "anim", "animation", "frame", "transform", "rect", "point", "display", "hud", "mc", "clip", "skin", "style", "parent", "stage",
    ];
    words(name).iter().any(|w| {
        let w = w.strip_suffix('s').filter(|s| s.len() > 2).unwrap_or(w);
        WORDS.contains(&w)
    })
}

/// Whether a field or class is the player itself: its whole name is one (`player`, `hero`).
fn the_player(name: &str) -> bool {
    let n: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    ["player", "hero", "localplayer", "theplayer", "mainplayer", "playerchar", "playercharacter", "character", "maincharacter", "avatar", "protagonist", "ship", "playership"]
        .contains(&n.as_str())
}

/// Constants by convention (`BASE_COST`, `WIDTH`): not the player's numbers.
fn constant(name: &str) -> bool {
    name.len() > 1 && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && name.chars().any(|c| c.is_ascii_uppercase())
}

/// The class the path starts from (its ClassClosure).
pub fn find_roots(heap: &Heap, path: &AirPath) -> Vec<u64> {
    let Some(air) = shared(heap) else { return Vec::new() };
    air.class(heap, &path.class).into_iter().collect()
}

/// Where the path leads from its class now: the number's address, every object along it
/// checked.
pub fn walk(heap: &Heap, roots: &[u64], path: &AirPath) -> Vec<u64> {
    let Some(air) = shared(heap) else { return Vec::new() };
    roots.iter().filter_map(|&c| follow(&air, heap.mem(), c, path)).collect()
}

/// The first field by that name (the most derived class's).
fn field<'a>(slots: &'a [Slot], name: &str) -> Option<&'a Slot> {
    slots.iter().find(|s| s.name == name)
}

fn follow(air: &Air, mem: &Mem, closure: u64, path: &AirPath) -> Option<u64> {
    let tr = air.traits_of(mem, closure)?;
    if air.class_name(mem, tr)? != format!("{}$", short_name(&path.class)) {
        return None;
    }
    let mut cur = Val::Obj(closure);
    for s in &path.steps {
        let Val::Obj(o) = cur else { return None };
        let tr = air.traits_of(mem, o)?;
        cur = match s {
            Step::Field(name) => {
                let slots = air.slots(mem, tr)?;
                air.val(mem, o, field(&slots, name)?)
            }
            Step::Index(i) => {
                if !air.is_array(mem, tr) {
                    return None;
                }
                Val::Obj(air.elements(mem, o).into_iter().find(|(x, _)| x == i)?.1)
            }
        };
    }
    match cur {
        Val::Num(at, _) => Some(at),
        _ => None,
    }
}

/// A class's statics or an object whose numbers are listed.
struct Group {
    path: AirPath,
    class: String,
    statics: bool,
    numbers: Vec<(String, u64, &'static str, AirPath)>,
}

/// Every number reachable from the game's classes' static fields (libraries' classes left out;
/// Arrays looked through), breadth first, paths through a player-like field first, so each
/// object is named by its shortest path.
fn groups(heap: &Heap) -> Option<Vec<Group>> {
    let air = shared(heap)?;
    let mem = heap.mem();
    air.index(mem);
    let mut classes: Vec<(String, Class)> = air.classes.lock().unwrap().0.iter().filter(|(_, c)| !c.library).map(|(n, c)| (n.clone(), c.clone())).collect();
    classes.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    let mut queues: [VecDeque<(u64, AirPath, usize)>; 2] = [VecDeque::new(), VecDeque::new()];
    let queue_of = |p: &AirPath| usize::from(p.player_part().is_none());
    let mut seen: HashSet<u64> = HashSet::new();
    for (name, c) in classes {
        let Some(closure) = ptr(mem, c.global + c.off) else { continue };
        let Some(tr) = air.traits_of(mem, closure) else { continue };
        let Some(slots) = air.slots(mem, tr) else { continue };
        let root = AirPath { class: name.clone(), steps: Vec::new() };
        let mut g = Group { path: root.clone(), class: name.clone(), statics: true, numbers: Vec::new() };
        for s in slots.iter().filter(|s| !s.library) {
            let p = root.then(Step::Field(s.name.clone()));
            match air.val(mem, closure, s) {
                Val::Num(at, kind) => g.numbers.push((s.name.clone(), at, kind, p)),
                Val::Obj(o) => queues[queue_of(&p)].push_back((o, p, 1)),
                Val::Other => {}
            }
        }
        if !g.numbers.is_empty() {
            out.push(g);
        }
    }
    let mut visited = 0;
    let mut listed = 0;
    while let Some((o, path, depth)) = queues[0].pop_front().or_else(|| queues[1].pop_front()) {
        if !seen.insert(o) {
            continue;
        }
        visited += 1;
        if visited > MAX_VISITED || listed > MAX_LISTED {
            break;
        }
        let Some(tr) = air.traits_of(mem, o) else { continue };
        let Some(class) = air.class_name(mem, tr) else { continue };
        let mut g = Group { path: path.clone(), class: class.clone(), statics: false, numbers: Vec::new() };
        let mut refs: Vec<(Step, u64)> = Vec::new();
        if air.is_array(mem, tr) {
            refs.extend(air.elements(mem, o).into_iter().map(|(i, x)| (Step::Index(i), x)));
        } else if let Some(slots) = air.slots(mem, tr) {
            // A library's object (a Sprite, a Timer): neither listed nor looked into.
            if air.library_class(mem, tr) {
                continue;
            }
            for s in slots.iter().filter(|s| !s.library) {
                match air.val(mem, o, s) {
                    Val::Num(at, kind) => g.numbers.push((s.name.clone(), at, kind, path.then(Step::Field(s.name.clone())))),
                    Val::Obj(x) => refs.push((Step::Field(s.name.clone()), x)),
                    Val::Other => {}
                }
            }
        }
        if depth < MAX_DEPTH {
            for (s, x) in refs {
                if !seen.contains(&x) {
                    let p = path.then(s);
                    queues[queue_of(&p)].push_back((x, p, depth + 1));
                }
            }
        }
        if !g.numbers.is_empty() {
            listed += g.numbers.len();
            out.push(g);
        }
    }
    Some(out)
}

/// The Browse tab's list for an AIR game: the game's classes' statics and the objects they lead
/// to; the player's object (a player-like field or class) first, then the game's objects with
/// player-like numbers (`gameSpace.energyConsumed`: strategy games have no player object), the
/// rest under Show Everything.
pub fn player(heap: &Heap) -> Option<Vec<ListedObject>> {
    let groups = groups(heap)?;
    let named = |n: &str| player_like(n) && !constant(n);
    let mut out: Vec<((bool, usize), ListedObject)> = groups
        .into_iter()
        .map(|g| {
            let hits = g.numbers.iter().filter(|n| named(&n.0)).count();
            let near = g.path.steps.len() <= 2;
            let shown = short_name(&g.class).to_owned();
            let player = (g.path.last_field().is_some_and(the_player) || !g.statics && the_player(&shown)) && !g.path.presentation();
            let owner = g.path.player_part().filter(|p| !player && *p != g.path && !g.path.presentation()).map(|p| p.shown());
            let main = player || owner.is_some() || !g.path.presentation() && hits > 0;
            let o = ListedObject {
                name: g.path.shown(),
                class: shown,
                role: if player { "character" } else if owner.is_some() { "part" } else { "game" },
                owner,
                more: !main,
                fields: g.numbers.into_iter().map(|(label, addr, kind, _)| Listed { label, addr, kind }).collect(),
            };
            ((near, hits), o)
        })
        .collect();
    // The game's objects near a static (two steps at most) first, the most player-like numbers
    // first among them: CW1's `GameSpace.instance` (six energy numbers) before the path
    // finder's `Graph.tentative_g_score` and the units further down.
    out.sort_by_key(|((near, hits), o)| (o.more, o.role != "character", o.role == "game", !near, std::cmp::Reverse(*hits)));
    Some(out.into_iter().map(|(_, o)| o).collect())
}

/// The path to a number the list has, checked by following it the way a restart does.
pub fn discover(heap: &Heap, target: u64) -> Option<(AirPath, Vec<u64>)> {
    let path = groups(heap)?.into_iter().flat_map(|g| g.numbers).find(|n| n.1 == target)?.3;
    let leads = walk(heap, &find_roots(heap, &path), &path);
    leads.contains(&target).then_some((path, leads))
}

/// airdump [class[#path]]: what was found (the core, domains, pools, classes), or a class's
/// statics, or the object at a path, field by field.
pub fn diagnose(heap: &Heap, at: &str) -> Vec<String> {
    let t = Instant::now();
    let mem = heap.mem();
    let Some(air) = shared(heap) else { return vec!["no ActionScript VM found (only 32-bit AIR runtimes are read)".into()] };
    air.index(mem);
    let n = air.classes.lock().unwrap().0.len();
    let pools = air.pools.lock().unwrap().values().filter(|a| a.is_some()).count();
    let mut out = vec![format!("core {:#x}, {} domains, {pools} pools read, {n} classes ({} ms)", air.core, air.domains.lock().unwrap().len(), t.elapsed().as_millis())];
    if at.is_empty() {
        let mut names: Vec<String> = air.classes.lock().unwrap().0.iter().filter(|(_, c)| !c.library).map(|(n, _)| n.clone()).collect();
        names.sort();
        out.extend(names.into_iter().map(|n| format!("class {n}")));
        return out;
    }
    let (class, rest) = at.split_once('#').unwrap_or((at, ""));
    let Some(mut o) = air.class(heap, class) else { return vec![format!("{class}: not initialised")] };
    if !rest.is_empty() {
        let Some(p) = AirPath::parse(&format!("air:{class}#{rest}")) else { return vec![format!("{at}: not a path")] };
        for s in &p.steps {
            let next = air.traits_of(mem, o).and_then(|tr| match s {
                Step::Field(name) => air.slots(mem, tr).and_then(|sl| match field(&sl, name).map(|f| air.val(mem, o, f)) {
                    Some(Val::Obj(x)) => Some(x),
                    _ => None,
                }),
                Step::Index(i) => air.elements(mem, o).into_iter().find(|(x, _)| x == i).map(|e| e.1),
            });
            let Some(x) = next else { return vec![format!("{at}: no object there")] };
            o = x;
        }
    }
    let Some(tr) = air.traits_of(mem, o) else { return vec![format!("{o:#x}: not an object")] };
    out.push(format!("{o:#x} {} (agrees with the VM's table: {:?})", air.class_name(mem, tr).unwrap_or_default(), air.agrees(mem, tr)));
    for s in air.slots(mem, tr).map(|s| s.to_vec()).unwrap_or_default() {
        let v = match air.val(mem, o, &s) {
            Val::Num(a, "f64") => read(mem, a, 8).map(|b| f64::from_le_bytes(b.try_into().unwrap()).to_string()).unwrap_or_default(),
            Val::Num(a, _) => u(mem, a).map(|v| (v as u32 as i32).to_string()).unwrap_or_default(),
            Val::Obj(x) => format!("{x:#x} {}", air.traits_of(mem, x).and_then(|t| air.class_name(mem, t)).unwrap_or_default()),
            Val::Other => String::new(),
        };
        out.push(format!("{}{} {:?} +{:#x} {v}", if s.library { "(library) " } else { "" }, s.name, s.ty, s.off));
    }
    if air.is_array(mem, tr) {
        for (i, x) in air.elements(mem, o) {
            out.push(format!("[{i}] {x:#x} {}", air.traits_of(mem, x).and_then(|t| air.class_name(mem, t)).unwrap_or_default()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_read_back() {
        for text in ["air:com.wbwar.creeper.Creeper#instance.gameScreen.gameSpace.stash", "air:Main#game.units[3].hp", "air:G#grid[1][2]"] {
            assert_eq!(AirPath::parse(text).unwrap().text(), text, "{text}");
        }
        let p = AirPath::parse("air:a.b.Game#hero.items[3].hp").unwrap();
        assert_eq!(p.class, "a.b.Game");
        assert_eq!(p.steps, vec![Step::Field("hero".into()), Step::Field("items".into()), Step::Index(3), Step::Field("hp".into())]);
        assert_eq!(p.shown(), "Game.hero.items[3].hp");
        assert_eq!(p.player_part().unwrap().shown(), "Game.hero");
        assert!(AirPath::parse("air:Game").is_none());
        assert!(AirPath::parse("air:#x").is_none());
        assert!(AirPath::parse("air:G#a..b").is_none());
        assert!(AirPath::parse("air:G#a[x]").is_none());
    }

    #[test]
    fn presentation_words() {
        assert!(presentation("stashViewer"));
        assert!(presentation("controlPanel"));
        assert!(presentation("healthBar"));
        assert!(!presentation("barrel"));
        assert!(!presentation("direction"));
        assert!(!presentation("gameSpace"));
        assert!(!presentation("gameScreen"));
    }

    #[test]
    fn bytecode_names_and_types() {
        // A tiny pool: strings "", "Number", "hp", "com.x"; namespaces: package "" and "com.x";
        // multinames: Number, hp, com.x::Thing.
        let mut abc = Abc {
            start: 0,
            strings: vec!["".into(), "Number".into(), "hp".into(), "com.x".into(), "Thing".into(), "int".into()],
            ns: vec![(0, 0), (0x16, 0), (0x16, 3)],
            mn: vec![Mn::None, Mn::Q(1, 1), Mn::Q(1, 2), Mn::Q(2, 4), Mn::Q(1, 5)],
            by_pos: HashMap::new(),
            classes: vec![3],
            builtin: false,
        };
        assert_eq!(abc.slot_type(1), Ty::F64);
        assert_eq!(abc.slot_type(4), Ty::I32);
        assert_eq!(abc.slot_type(3), Ty::Obj);
        assert_eq!(abc.slot_type(0), Ty::Atom);
        assert_eq!(abc.qname(3), ("com.x", "Thing"));
        abc.builtin = true;
        assert_eq!(abc.name(2), "hp");
    }
}
