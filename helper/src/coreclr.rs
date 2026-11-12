// Godot games written in C# (Godot 4: .NET's CoreCLR, shipped in data_<game>_<platform>/ next
// to the program): their objects are .NET objects, not GDScript instances, so gdscript.rs sees
// nothing. The game's own assembly (<name>.dll, the one with a <name>.runtimeconfig.json) says
// which classes and fields there are (ECMA-335 metadata: the TypeDef and Field tables, the
// #Strings heap); the runtime says where they are.
// Memory (.NET 8, x86-64; found on the game itself where it can be): an object starts with its
// MethodTable pointer (a 4-byte pad and the sync word before it); a MethodTable is
// { u32 flags; u32 base size; u16 flags; u16 token (the TypeDef's row); u16 virtuals;
// u16 interfaces; parent +0x10; module +0x18; ... EEClass +0x28 }, an EEClass
// { ...; its MethodTable +0x10; its FieldDescs +0x18 }, a FieldDesc 16 bytes { MethodTable;
// u32 the Field row (24 bits), static bit 24; u32 offset (27 bits, from after the object's
// MethodTable pointer), element type (5 bits) }: a class's instance fields first, then its
// static ones. The module keeps a table of its classes' MethodTables by TypeDef row.
// Godot's own objects derive from GodotObject, whose IntPtr field (NativePtr) points to the
// engine's object and is zeroed when that is freed: such an object is live while it is set.
// The collector moves objects, so a value is found again by name every time.
// Text form: cs:Game.Cell.PlayerCell#Size = the field Size (its own or an ancestor's) of every
// live object of the class Game.Cell.PlayerCell; cs:Game.Game#Stats.Health goes on through the
// object the field Stats holds.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::mem::Mem;
use crate::mono::player_like;
use crate::names::Heap;
use crate::ue::{Listed, ListedObject, MAX_LISTED};

/// Element types (ECMA-335 II.23.1.16) a field may have.
const BOOLEAN: u32 = 0x02;
const CLASS: u32 = 0x12;
const INTPTR: u32 = 0x18;
/// A FieldDesc's static bit, in the word with its Field row.
const STATIC: u32 = 1 << 24;
/// Fields a class may have, bytes an object may span.
const MAX_FIELDS: usize = 4096;
const MAX_OFFSET: u32 = 1 << 16;
/// Objects of one class listed, and followed by a path.
const MAX_ALIKE: usize = 8;
const MAX_PLACES: usize = 16;
const MAX_NAME: usize = 200;
/// How long the objects found serve other searches.
const FOUND_FOR: Duration = Duration::from_secs(5);

fn u16_at(b: &[u8], o: usize) -> Option<usize> {
    b.get(o..o + 2).map(|x| u16::from_le_bytes([x[0], x[1]]) as usize)
}

fn u32_at(b: &[u8], o: usize) -> Option<usize> {
    b.get(o..o + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]) as usize)
}

/// A class of the assembly: its TypeDef row is its place in the list plus one.
struct TypeDef {
    name: String,
    /// (Field row, name, static).
    fields: Vec<(u32, String, bool)>,
}

/// The classes and fields of a .NET assembly, from its metadata.
fn metadata(b: &[u8]) -> Option<Vec<TypeDef>> {
    let pe = u32_at(b, 0x3c)?;
    let sections = u16_at(b, pe + 6)?;
    let opt = pe + 24;
    let dirs = opt + if u16_at(b, opt)? == 0x20b { 112 } else { 96 };
    let cli_rva = u32_at(b, dirs + 14 * 8)?;
    let table = opt + u16_at(b, pe + 20)?;
    let offset = |rva: usize| {
        (0..sections).find_map(|i| {
            let s = table + 40 * i;
            let (vsize, va, rsize, raw) = (u32_at(b, s + 8)?, u32_at(b, s + 12)?, u32_at(b, s + 16)?, u32_at(b, s + 20)?);
            (va <= rva && rva < va + vsize.max(rsize)).then(|| raw + rva - va)
        })
    };
    let md = offset(u32_at(b, offset(cli_rva)? + 8)?)?;
    let mut p = md + 16 + u32_at(b, md + 12)? + 2;
    let streams = u16_at(b, p)?;
    p += 2;
    let (mut tables, mut strings) = (None, None);
    for _ in 0..streams {
        let at = md + u32_at(b, p)?;
        let end = p + 8 + b.get(p + 8..)?.iter().position(|&c| c == 0)?;
        match b.get(p + 8..end)? {
            b"#~" | b"#-" => tables = Some(at),
            b"#Strings" => strings = Some(at),
            _ => {}
        }
        p = (end + 4) & !3;
    }
    let (t, strings) = (tables?, strings?);
    let heaps = *b.get(t + 6)?;
    let valid = u64::from_le_bytes(b.get(t + 8..t + 16)?.try_into().ok()?);
    let mut rows = [0usize; 64];
    let mut p = t + 24;
    for (i, r) in rows.iter_mut().enumerate() {
        if valid >> i & 1 == 1 {
            *r = u32_at(b, p)?;
            p += 4;
        }
    }
    // Index widths: into the heaps, into one table, into one of several ("coded": the table
    // in the low bits).
    let wide = |flag: u8| if heaps & flag != 0 { 4 } else { 2 };
    let (sidx, gidx, bidx) = (wide(1), wide(2), wide(4));
    let row = |tables: &[usize]| if tables.iter().any(|&x| rows[x] >= 1 << 16) { 4 } else { 2 };
    let coded = |bits: u32, tables: &[usize]| if tables.iter().any(|&x| rows[x] >= 1 << (16 - bits)) { 4 } else { 2 };
    let (extends, scope) = (coded(2, &[2, 1, 27]), coded(2, &[0, 26, 35, 1]));
    // Module, TypeRef, TypeDef, FieldPtr, Field: the tables before and with the two wanted.
    let size = [2 + sidx + 3 * gidx, scope + 2 * sidx, 4 + 2 * sidx + extends + row(&[4]) + row(&[6]), row(&[4]), 2 + sidx + bidx];
    let mut at = [0usize; 5];
    let mut q = p;
    for i in 0..5 {
        at[i] = q;
        q += size[i] * rows[i];
    }
    let read = |o: usize, w: usize| if w == 4 { u32_at(b, o) } else { u16_at(b, o) };
    let text = |o: usize| {
        let s = b.get(strings + o..)?;
        let s = &s[..s.iter().position(|&c| c == 0)?];
        (s.len() <= MAX_NAME).then(|| String::from_utf8_lossy(s).into_owned())
    };
    let mut fields = Vec::new();
    for i in 0..rows[4] {
        let o = at[4] + size[4] * i;
        fields.push((u16_at(b, o)? & 0x10 != 0, text(read(o + 2, sidx)?)?));
    }
    let mut firsts = Vec::new();
    let mut types = Vec::new();
    for i in 0..rows[2] {
        let o = at[2] + size[2] * i;
        let (name, space) = (text(read(o + 4, sidx)?)?, text(read(o + 4 + sidx, sidx)?)?);
        firsts.push(read(o + 4 + 2 * sidx + extends, row(&[4]))?);
        types.push(TypeDef { name: if space.is_empty() { name } else { format!("{space}.{name}") }, fields: Vec::new() });
    }
    for i in 0..types.len() {
        let end = firsts.get(i + 1).copied().unwrap_or(fields.len() + 1);
        for r in firsts[i]..end.min(fields.len() + 1) {
            let (is_static, name) = fields.get(r.checked_sub(1)?)?.clone();
            types[i].fields.push((r as u32, name, is_static));
        }
    }
    Some(types)
}

#[derive(Clone, Debug)]
struct Field {
    name: String,
    /// From the object's start.
    offset: u64,
    kind: u32,
}

#[derive(Clone, Debug)]
struct Class {
    mt: u64,
    name: String,
    parent: u64,
    /// Its own instance fields.
    fields: Vec<Field>,
}

/// A game's .NET classes as its runtime keeps them.
pub struct Clr {
    classes: Vec<Class>,
    /// GodotObject's MethodTable and where its objects keep the engine's object.
    godot: Option<(u64, u64)>,
}

fn word(mem: &Mem, at: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b, at).ok()?;
    Some(u64::from_le_bytes(b))
}

fn half(mem: &Mem, at: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    mem.read_exact_at(&mut b, at).ok()?;
    Some(u32::from_le_bytes(b))
}

/// A MethodTable: its EEClass points back to it. (TypeDef row, parent, module, FieldDescs.)
fn method_table(heap: &Heap, mt: u64) -> Option<(u32, u64, u64, u64)> {
    let mem = heap.mem();
    if mt % 8 != 0 || !heap.is_pointer(mt) {
        return None;
    }
    let class = word(mem, mt + 0x28).filter(|&c| c % 8 == 0 && heap.is_pointer(c))?;
    if word(mem, class + 0x10)? != mt {
        return None;
    }
    Some((half(mem, mt + 8)? >> 16, word(mem, mt + 0x10)?, word(mem, mt + 0x18)?, word(mem, class + 0x18)?))
}

/// The game's own assembly: the .dll with a .runtimeconfig.json of its name, in the folder
/// the runtime (libcoreclr.so, coreclr.dll) is loaded from.
fn assembly(pid: u32) -> Option<PathBuf> {
    let regions = crate::helper::maps(pid).ok()?;
    let runtime = regions.iter().map(|r| r.path.replace('\\', "/")).find(|p| {
        let name = p.rsplit('/').next().unwrap_or_default().to_ascii_lowercase();
        name == "libcoreclr.so" || name == "coreclr.dll"
    })?;
    let dir = crate::os::inside(pid, Path::new(&runtime).parent()?);
    let config = std::fs::read_dir(&dir).ok()?.flatten().map(|e| e.path()).find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".runtimeconfig.json")))?;
    let stem = config.file_name()?.to_string_lossy().strip_suffix(".runtimeconfig.json")?.to_owned();
    Some(dir.join(format!("{stem}.dll")))
}

/// Whether the game runs .NET's CoreCLR (a Godot 4 game with C# scripts).
pub fn runs_coreclr(pid: u32) -> bool {
    crate::helper::maps(pid).is_ok_and(|regions| {
        regions.iter().any(|r| {
            let name = r.path.rsplit(['/', '\\']).next().unwrap_or_default().to_ascii_lowercase();
            name == "libcoreclr.so" || name == "coreclr.dll"
        })
    })
}

impl Clr {
    /// Finds the classes of the game's assembly in memory: their FieldDescs by the Field rows
    /// the assembly gives them (one pass), the MethodTables those belong to, then the
    /// module's table of all its classes.
    fn locate(heap: &Heap) -> Option<Clr> {
        let mem = heap.mem();
        let types = metadata(&std::fs::read(assembly(heap.pid)?).ok()?)?;
        let owner: HashMap<u32, usize> = types.iter().enumerate().flat_map(|(i, t)| t.fields.iter().map(move |f| (f.0, i))).collect();
        let most = *owner.keys().max()?;
        // Places shaped like a FieldDesc of one of those fields, by the MethodTable they name.
        let mut seen: HashMap<u64, u32> = HashMap::new();
        heap.each_chunk(16, |_, b, fresh| {
            for off in (0..fresh).step_by(8) {
                let Some(rec) = b.get(off..off + 16) else { break };
                let (token, place) = (u32::from_le_bytes(rec[8..12].try_into().unwrap()), u32::from_le_bytes(rec[12..16].try_into().unwrap()));
                let row = token & 0xff_ffff;
                if row == 0 || row > most || place & 0x7ff_ffff >= MAX_OFFSET || !(2..=0x1c).contains(&(place >> 27)) {
                    continue;
                }
                let mt = u64::from_le_bytes(rec[..8].try_into().unwrap());
                if mt % 8 == 0 && mt > 0x10000 && mt < 1 << 47 {
                    seen.entry(mt).or_insert(row);
                }
            }
        });
        // The ones that are MethodTables of the class owning the field, by module.
        let mut found: HashMap<u64, Vec<(usize, u64)>> = HashMap::new();
        for (&mt, &row) in &seen {
            let Some((token, _, module, _)) = method_table(heap, mt) else { continue };
            if owner.get(&row) == Some(&(token as usize).wrapping_sub(1)) {
                found.entry(module).or_default().push((token as usize - 1, mt));
            }
        }
        let (module, known) = found.into_iter().max_by_key(|(_, v)| v.len())?;
        // The module's table of MethodTables by TypeDef row: the place in it whose entries are
        // the classes found. Classes without fields of their own are only there.
        let mut tables: HashMap<usize, u64> = known.iter().copied().collect();
        let fits = |table: u64| known.iter().filter(|&&(t, mt)| word(mem, table + 8 * (t as u64 + 1)).is_some_and(|x| x & !3 == mt)).count();
        let table = (0..0x200u64).filter_map(|i| word(mem, module + 8 * i)).filter(|&p| p % 8 == 0 && heap.is_pointer(p)).find(|&p| fits(p) == known.len());
        if let Some(table) = table {
            for t in 0..types.len() {
                let Some(mt) = word(mem, table + 8 * (t as u64 + 1)).map(|x| x & !3).filter(|&x| x != 0) else { continue };
                if method_table(heap, mt).is_some_and(|m| m.0 as usize == t + 1 && m.2 == module) {
                    tables.insert(t, mt);
                }
            }
        }
        let mut classes = Vec::new();
        let mut names = HashSet::new();
        let mut by_table: Vec<(usize, u64)> = tables.into_iter().collect();
        by_table.sort_unstable();
        for (t, mt) in by_table {
            let Some((_, parent, _, descs)) = method_table(heap, mt) else { continue };
            let mut fields = Vec::new();
            for i in 0..types[t].fields.len().min(MAX_FIELDS) as u64 {
                let (Some(owner), Some(token), Some(place)) = (word(mem, descs + 16 * i), half(mem, descs + 16 * i + 8), half(mem, descs + 16 * i + 12)) else { break };
                if owner != mt {
                    break;
                }
                let Some((_, name, _)) = types[t].fields.iter().find(|f| f.0 == token & 0xff_ffff) else { continue };
                if token & STATIC == 0 {
                    fields.push(Field { name: name.clone(), offset: 8 + u64::from(place & 0x7ff_ffff), kind: place >> 27 });
                }
            }
            // (Two classes of one name: compiler-made ones, nested in different classes.)
            if names.insert(types[t].name.clone()) {
                classes.push(Class { mt, name: types[t].name.clone(), parent, fields });
            }
        }
        // GodotObject: the ancestor below System.Object; its pointer-sized field is the
        // engine's object.
        let mut godot = None;
        for c in &classes {
            let mut chain = vec![c.mt];
            while let Some((_, parent, _, _)) = method_table(heap, *chain.last()?).filter(|m| m.1 != 0 && chain.len() < 64) {
                chain.push(parent);
            }
            // (The last is System.Object.)
            let Some(&base) = chain.len().checked_sub(2).and_then(|i| chain.get(i)).filter(|_| chain.len() >= 3) else { continue };
            let Some((_, _, _, descs)) = method_table(heap, base) else { continue };
            let native = (0..16u64).find_map(|i| {
                let (owner, token, place) = (word(mem, descs + 16 * i)?, half(mem, descs + 16 * i + 8)?, half(mem, descs + 16 * i + 12)?);
                (owner == base && token & STATIC == 0 && place >> 27 == INTPTR).then(|| 8 + u64::from(place & 0x7ff_ffff))
            });
            if let Some(native) = native {
                godot = Some((base, native));
                break;
            }
        }
        if std::env::var_os("FERRET_DEBUG").is_some() {
            eprintln!("coreclr: {} classes of the assembly's {} found ({} by their fields), module {module:x}, class table {table:x?}, GodotObject {godot:x?}", classes.len(), types.len(), known.len());
        }
        (!classes.is_empty()).then_some(Clr { classes, godot })
    }

    fn class_of(&self, mt: u64) -> Option<&Class> {
        self.classes.iter().find(|c| c.mt == mt)
    }

    /// Whether the class derives from GodotObject.
    fn engine_object(&self, heap: &Heap, mt: u64) -> bool {
        let Some((base, _)) = self.godot else { return false };
        let mut at = mt;
        for _ in 0..64 {
            if at == base {
                return true;
            }
            match method_table(heap, at) {
                Some((_, parent, _, _)) if parent != 0 => at = parent,
                _ => return false,
            }
        }
        false
    }

    /// The fields of an object of the class: its own and its ancestors' in the assembly.
    fn fields(&self, mt: u64) -> Vec<&Field> {
        let mut out = Vec::new();
        let mut at = mt;
        for _ in 0..64 {
            let Some(c) = self.class_of(at) else { break };
            out.extend(c.fields.iter());
            at = c.parent;
        }
        out
    }

    /// Whether `o` is an object of the class in use: the pad before it is empty, and one of
    /// Godot's still has its engine object.
    fn live(&self, heap: &Heap, o: u64, mt: u64) -> bool {
        let mem = heap.mem();
        if word(mem, o) != Some(mt) || o.checked_sub(8).and_then(|h| half(mem, h)) != Some(0) {
            return false;
        }
        // The runtime's own records name the MethodTable too: its EEClass (at +0x10) and the
        // first of its FieldDescs looked like objects.
        if let Some(class) = word(mem, mt + 0x28) {
            if o == class + 0x10 || word(mem, class + 0x18) == Some(o) {
                return false;
            }
        }
        match self.godot.filter(|_| self.engine_object(heap, mt)) {
            Some((_, native)) => word(mem, o + native).is_some_and(|p| p != 0 && heap.is_pointer(p)),
            None => true,
        }
    }

    /// Every live object of Godot's among the game's classes, by class (one pass).
    fn objects(&self, heap: &Heap) -> Vec<(u64, u64)> {
        let wanted: HashSet<u64> = self.classes.iter().map(|c| c.mt).filter(|&mt| self.engine_object(heap, mt)).collect();
        let mut hits = Vec::new();
        heap.each_chunk(0, |addr, b, fresh| {
            for off in (0..fresh - fresh % 8).step_by(8) {
                let v = u64::from_le_bytes(b[off..off + 8].try_into().unwrap());
                if wanted.contains(&v) {
                    hits.push((addr + off as u64, v));
                }
            }
        });
        hits.retain(|&(o, mt)| self.live(heap, o, mt));
        // An object of a finished round keeps its engine object's address after that is freed
        // (get killed by circles: every round's player cell stayed "live"): once the scene
        // tree is known, only the objects whose engine object is a node in it.
        let Some((_, native)) = self.godot else { return hits };
        let in_tree: Vec<Option<bool>> = hits.iter().map(|&(o, _)| word(heap.mem(), o + native).and_then(|n| crate::gdscript::node_in_tree(heap, n))).collect();
        if in_tree.iter().any(|t| *t == Some(true)) {
            // (The tree may have been learned midway: asked again.)
            hits.retain(|&(o, _)| word(heap.mem(), o + native).and_then(|n| crate::gdscript::node_in_tree(heap, n)) == Some(true));
        }
        hits
    }

    /// The object a reference field holds, when it is one of the game's classes'.
    fn held(&self, heap: &Heap, slot: u64) -> Option<(u64, u64)> {
        let o = word(heap.mem(), slot).filter(|&o| o % 8 == 0 && heap.is_pointer(o))?;
        let mt = word(heap.mem(), o)?;
        (self.class_of(mt).is_some() && self.live(heap, o, mt)).then_some((o, mt))
    }
}

/// A field's value type for the helper, when it is a number or a flag.
fn kind(element: u32) -> Option<&'static str> {
    Some(match element {
        BOOLEAN => "bit0",
        0x04 => "i8",
        0x05 => "u8",
        0x07 => "u16",
        0x08 | 0x09 => "i32",
        0x0a | 0x0b => "i64",
        0x0c => "f32",
        0x0d => "f64",
        _ => return None,
    })
}

/// The runtime of the game attached to, per pid; a failed search is tried again after a while.
fn shared(heap: &Heap) -> Option<Arc<Clr>> {
    const RETRY: Duration = Duration::from_secs(10);
    static FOUND: Mutex<Option<(u32, Option<Arc<Clr>>, Instant)>> = Mutex::new(None);
    let mut found = FOUND.lock().unwrap();
    if let Some((pid, clr, at)) = found.as_ref() {
        let valid = clr.as_ref().is_some_and(|c| c.classes.first().is_some_and(|k| method_table(heap, k.mt).is_some()));
        if *pid == heap.pid && (valid || clr.is_none() && at.elapsed() < RETRY) {
            return clr.clone();
        }
    }
    let clr = Clr::locate(heap).map(Arc::new);
    *found = Some((heap.pid, clr.clone(), Instant::now()));
    clr
}

/// The live objects found last, by process.
fn objects(heap: &Heap, clr: &Clr) -> Vec<(u64, u64)> {
    static FOUND: Mutex<Option<(u32, Instant, Vec<(u64, u64)>)>> = Mutex::new(None);
    let mut found = FOUND.lock().unwrap();
    if let Some((_, _, objects)) = found.as_ref().filter(|(pid, at, objects)| *pid == heap.pid && at.elapsed() < FOUND_FOR && objects.iter().all(|&(o, mt)| clr.live(heap, o, mt))) {
        return objects.clone();
    }
    let objects = clr.objects(heap);
    *found = Some((heap.pid, Instant::now(), objects.clone()));
    objects
}

#[derive(Clone, Debug, PartialEq)]
pub struct CsPath {
    class: String,
    /// Fields by name: the last is the value, the ones before hold objects.
    fields: Vec<String>,
}

fn good_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= MAX_NAME && !n.chars().any(|c| c.is_whitespace() || c.is_control() || c == '#' || c == '.')
}

impl CsPath {
    pub fn parse(text: &str) -> Option<Self> {
        let (class, fields) = text.strip_prefix("cs:")?.split_once('#')?;
        let fields: Vec<String> = fields.split('.').map(str::to_owned).collect();
        let class_ok = !class.is_empty() && class.len() <= MAX_NAME && !class.chars().any(|c| c.is_whitespace() || c.is_control() || c == '#');
        (class_ok && fields.iter().all(|f| good_name(f))).then(|| CsPath { class: class.to_owned(), fields })
    }

    pub fn text(&self) -> String {
        format!("cs:{}#{}", self.class, self.fields.join("."))
    }

    /// In words: "Size of PlayerCell".
    pub fn describe(&self) -> String {
        format!("{} of {}", self.fields.join("."), self.class.rsplit('.').next().unwrap_or(&self.class))
    }
}

/// The live objects of the path's class.
pub fn find_roots(heap: &Heap, path: &CsPath) -> Vec<u64> {
    let Some(clr) = shared(heap) else { return Vec::new() };
    let Some(class) = clr.classes.iter().find(|c| c.name == path.class) else { return Vec::new() };
    objects(heap, &clr).into_iter().filter(|o| o.1 == class.mt).map(|o| o.0).take(MAX_PLACES + 1).collect()
}

/// Where the path leads from the objects found before (no search): gone objects lead nowhere.
pub fn walk(heap: &Heap, roots: &[u64], path: &CsPath) -> Vec<u64> {
    let Some(clr) = shared(heap) else { return Vec::new() };
    let Some(class) = clr.classes.iter().find(|c| c.name == path.class) else { return Vec::new() };
    let mut out = Vec::new();
    'root: for &root in roots {
        if !clr.live(heap, root, class.mt) {
            continue;
        }
        let (mut o, mut mt) = (root, class.mt);
        for (i, name) in path.fields.iter().enumerate() {
            let Some(f) = clr.fields(mt).into_iter().find(|f| f.name == *name) else { continue 'root };
            if i + 1 == path.fields.len() {
                if kind(f.kind).is_some() {
                    out.push(o + f.offset);
                }
            } else {
                match clr.held(heap, o + f.offset).filter(|_| f.kind == CLASS) {
                    Some(next) => (o, mt) = next,
                    None => continue 'root,
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// One listed object: its name, class, numbers (label, address, type, path).
struct Group {
    name: String,
    class: String,
    main: bool,
    numbers: Vec<(String, u64, &'static str, CsPath)>,
}

fn groups(heap: &Heap) -> Option<Vec<Group>> {
    let clr = shared(heap)?;
    let mut out = Vec::new();
    let mut alike: HashMap<u64, usize> = HashMap::new();
    for (o, mt) in objects(heap, &clr) {
        let Some(class) = clr.class_of(mt) else { continue };
        let n = alike.entry(mt).or_default();
        *n += 1;
        let short = class.name.rsplit('.').next().unwrap_or(&class.name).to_owned();
        let main = player_like(&short);
        if *n > MAX_ALIKE && !main {
            continue;
        }
        let mut numbers = Vec::new();
        for f in clr.fields(mt) {
            let path = CsPath { class: class.name.clone(), fields: vec![f.name.clone()] };
            if let Some(k) = kind(f.kind) {
                numbers.push((f.name.clone(), o + f.offset, k, path));
            } else if let Some((held, held_mt)) = clr.held(heap, o + f.offset).filter(|h| f.kind == CLASS && !clr.engine_object(heap, h.1)) {
                // A plain object the field holds (the game's own data), one level.
                for g in clr.fields(held_mt) {
                    if let Some(k) = kind(g.kind) {
                        numbers.push((format!("{}.{}", f.name, g.name), held + g.offset, k, CsPath { class: class.name.clone(), fields: vec![f.name.clone(), g.name.clone()] }));
                    }
                }
            }
        }
        if !numbers.is_empty() {
            let name = if *n > 1 { format!("{short} ({n})") } else { short };
            out.push(Group { name, class: class.name.clone(), main, numbers });
        }
    }
    Some(out)
}

/// The Browse tab's list: the game's live objects with their numbers, the player's first.
pub fn player(heap: &Heap) -> Option<Vec<ListedObject>> {
    let mut total = 0;
    let mut out: Vec<ListedObject> = groups(heap)?
        .into_iter()
        .filter_map(|g| {
            if !g.main && total + g.numbers.len() > MAX_LISTED {
                return None;
            }
            total += g.numbers.len();
            let role = if g.main { "character" } else { "game" };
            Some(ListedObject { name: g.name, class: g.class, role, owner: None, more: !g.main, fields: g.numbers.into_iter().map(|(label, addr, kind, _)| Listed { label, addr, kind }).collect() })
        })
        .collect();
    out.sort_by_key(|o| o.more);
    Some(out)
}

/// The path to a number the list has, checked by following it the way a restart does, with
/// the places it leads to now and the field's value type.
pub fn discover(heap: &Heap, target: u64) -> Option<(CsPath, Vec<u64>, &'static str)> {
    let (_, _, kind, path) = groups(heap)?.into_iter().flat_map(|g| g.numbers).find(|n| n.1 == target)?;
    let leads = walk(heap, &find_roots(heap, &path), &path);
    (leads.contains(&target) && leads.len() <= MAX_PLACES).then_some((path, leads, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_read_back() {
        for text in ["cs:Game.Cell.PlayerCell#Size", "cs:Game#Stats.Health", "cs:A.B#<Score>k__BackingField"] {
            assert_eq!(CsPath::parse(text).unwrap().text(), text);
        }
        assert_eq!(CsPath::parse("cs:Game.Cell.PlayerCell#Size").unwrap().describe(), "Size of PlayerCell");
        assert!(CsPath::parse("cs:Game.Cell.PlayerCell").is_none());
        assert!(CsPath::parse("cs:#Size").is_none());
        assert!(CsPath::parse("cs:A#b c").is_none());
        assert!(CsPath::parse("gd:a.gd.x").is_none());
    }
}
