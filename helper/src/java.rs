//! Java games on HotSpot (OpenJDK's JVM): their objects read from outside through the JVM's
//! own description of its layout. libjvm exports tables for its serviceability agent
//! (`gHotSpotVMStructs`, `gHotSpotVMTypes`, constants): every field of its C++ classes that
//! tools need, by name, with its offset (or a static's address). From those: the loaded
//! classes (ClassLoaderDataGraph -> ClassLoaderData -> Klass list), each class's name (a
//! Symbol), its fields (name, type, offset: InstanceKlass's field info, names in its constant
//! pool) and its statics (in the class's mirror, a java.lang.Class object). A value is named by
//! a class's static field and the fields from there: `java:com.example.Game#player.hp`,
//! `java:...Dungeon#gold`, `[i]` for an array's element (an ArrayList's `elementData[2]`),
//! `["key"]` for a map's entry by its key (a string's text, a boxed number's digits, an enum
//! constant's name; written `%XX`-encoded like Python's names).
//!
//! Java's collector moves objects: a found address goes stale at the next collection, and the
//! old copy's memory is reused. So a path is walked again right before every write (limits do
//! it on each check, `Named::fresh`), and every object along it is checked: in the heap, of a
//! class the JVM has loaded, not being moved (its mark word forwarded).

use crate::mono::player_like;
use crate::names::Heap;
use crate::python::{decode, encode};
use crate::ue::{Listed, ListedObject, MAX_LISTED};
use std::collections::{HashMap, HashSet, VecDeque};
use crate::mem::Mem;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Objects looked into for one list at most.
const MAX_VISITED: usize = 20000;
/// How deep the list goes from a static field (a list's element is two steps: `items.elementData[i]`;
/// a pouch's in the hero's backpack is nine down).
const MAX_DEPTH: usize = 12;
/// Elements read from one array at most.
const MAX_ELEMENTS: u32 = 256;
/// Slots of a map's table read at most (its entries: `MAX_ELEMENTS`; a table has room to spare).
const MAX_SLOTS: u32 = 4096;
/// Maps looked through by key: Java's hash maps (a table of node chains: the key, value and
/// next fields), TreeMap, EnumMap, and libGDX's (keys and values in two arrays).
const MAPS: &[&str] = &[
    "java/util/HashMap", "java/util/LinkedHashMap", "java/util/Hashtable", "java/util/concurrent/ConcurrentHashMap", "java/util/TreeMap", "java/util/EnumMap",
    "com/badlogic/gdx/utils/ObjectMap", "com/badlogic/gdx/utils/OrderedMap", "com/badlogic/gdx/utils/ObjectIntMap", "com/badlogic/gdx/utils/ObjectFloatMap",
    "com/badlogic/gdx/utils/IntMap", "com/badlogic/gdx/utils/IntIntMap", "com/badlogic/gdx/utils/IntFloatMap",
];
/// Classes read from the class list at most (a runaway list is garbage).
const MAX_CLASSES: usize = 200_000;
/// Packages that aren't the game's: Java's own, and common libraries (their statics and
/// objects aren't listed; their collections are looked through).
const LIBRARIES: &[&str] = &[
    "java/", "javax/", "jdk/", "sun/", "com/sun/", "kotlin/", "kotlinx/", "scala/", "org/lwjgl/", "com/badlogic/gdx/", "com/esotericsoftware/",
    "org/json/", "com/google/", "org/apache/", "org/slf4j/", "ch/qos/", "io/netty/", "it/unimi/", "org/objectweb/", "net/java/", "paulscode/",
    "de/jcm/", "org/jetbrains/", "org/intellij/", "com/fasterxml/", "org/yaml/", "javafx/", "com/jogamp/", "jogamp/", "org/joml/",
    "club/minnced/", "net/arikia/", "com/codedisaster/", "org/luaj/", "processing/core/", "processing/opengl/", "processing/awt/",
    "com/jcraft/",
];

/// A field of a class, as its metadata describes it.
#[derive(Clone, Debug)]
pub struct Field {
    name: String,
    sig: String,
    offset: u32,
    is_static: bool,
    /// `final`: a static final number is a constant javac copies into the code using it
    /// (`serialVersionUID`, `MAX_HP = 100`): writing it changes nothing.
    is_final: bool,
}

/// What a field or element holds, for listing and walking.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Val {
    /// A number: its address and the helper's value type.
    Num(u64, &'static str),
    /// A reference to an object (its address), or null.
    Obj(u64),
    Other,
}

/// Offsets and addresses from the JVM's own tables, and how this JVM packs references.
pub struct Jvm {
    cld_head: u64,
    cld_klasses: u64,
    cld_next: u64,
    klass_name: u64,
    klass_super: u64,
    klass_next: u64,
    klass_mirror: u64,
    /// The mirror is held through an OopHandle (JDK 9+: a pointer to the slot holding it),
    /// else directly.
    mirror_handle: bool,
    ik_constants: u64,
    cp_size: u64,
    sym_length: u64,
    sym_body: u64,
    /// JDK 21+: a compressed stream of field info (`fieldinfo_stream`, its flag bits: `scheme`).
    stream: Option<(u64, u64)>,
    scheme: Scheme,
    /// JDK 8-20: an array of u2 slots a field (`_fields`, `_java_fields_count`, the slots'
    /// meaning from the constants).
    old: Option<OldFields>,
    coops: bool,
    oop_base: u64,
    oop_shift: u32,
    ckp: bool,
    compact: bool,
    klass_base: u64,
    klass_shift: u32,
    klass_in_mark: u32,
    pub major: u32,
    threads: Option<Threads>,
    classes: Mutex<Classes>,
    fields: Mutex<HashMap<u64, Arc<Vec<Field>>>>,
}

/// Where the JVM keeps its Java threads: JDK 10+ a ThreadsList
/// (`ThreadsSMRSupport::_java_thread_list`: a length and an array of JavaThread*), JDK 8-9 a
/// chain from `Threads::_thread_list` through `JavaThread::_next`. A thread's java.lang.Thread
/// object: `JavaThread::_threadObj` (an OopHandle since JDK 16, else the oop itself).
#[derive(Clone, Copy)]
struct Threads {
    list: Option<(u64, u64, u64)>,
    chain: Option<(u64, u64)>,
    obj: u64,
    handle: bool,
}

/// Where a field's optional items' flag bits are (JDK 21-26; later JDKs added value-class
/// flags before them).
#[derive(Clone, Copy, PartialEq, Debug)]
struct Scheme {
    optional: [Option<u32>; 5],
}

const SCHEMES: [Scheme; 2] = [
    // initialized, generic, contended
    Scheme { optional: [Some(0), Some(2), Some(4), None, None] },
    // initialized, generic, contended, flat (layout kind), null marker
    Scheme { optional: [Some(3), Some(5), Some(7), Some(1), Some(2)] },
];

#[derive(Clone, Copy, Debug)]
struct OldFields {
    fields: u64,
    count: u64,
    data: u64,
    slots: u64,
    access: u64,
    name: u64,
    sig: u64,
    low: u64,
    high: u64,
    tag: u32,
}

#[derive(Default)]
struct Classes {
    by_name: HashMap<String, u64>,
    all: HashSet<u64>,
    at: Option<Instant>,
}

fn word(mem: &Mem, at: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b, at).ok().map(|_| u64::from_le_bytes(b))
}

fn half(mem: &Mem, at: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    mem.read_exact_at(&mut b, at).ok().map(|_| u32::from_le_bytes(b))
}

fn short(mem: &Mem, at: u64) -> Option<u16> {
    let mut b = [0u8; 2];
    mem.read_exact_at(&mut b, at).ok().map(|_| u16::from_le_bytes(b))
}

fn byte(mem: &Mem, at: u64) -> Option<u8> {
    let mut b = [0u8; 1];
    mem.read_exact_at(&mut b, at).ok().map(|_| b[0])
}

fn cstr(mem: &Mem, at: u64) -> Option<String> {
    if at == 0 {
        return None;
    }
    let mut b = [0u8; 128];
    let n = mem.read_at(&mut b, at).ok()?;
    let end = b[..n].iter().position(|&c| c == 0)?;
    Some(String::from_utf8_lossy(&b[..end]).into_owned())
}

/// The `gHotSpot*` symbols libjvm exports (its .dynsym), as addresses in the game.
fn hotspot_symbols(pid: u32) -> Option<HashMap<String, u64>> {
    let maps = crate::helper::maps(pid).ok()?;
    // Its first mapping is where it's loaded (the maps are in address order).
    let r = maps.iter().find(|r| r.path.ends_with("/libjvm.so"))?;
    let elf = std::fs::read(crate::os::inside(pid, std::path::Path::new(&r.path))).or_else(|_| std::fs::read(&r.path)).ok()?;
    let u16_at = |o: usize| elf.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |o: usize| elf.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let u64_at = |o: usize| elf.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
    if elf.get(..4) != Some(b"\x7fELF") || elf.get(4) != Some(&2) {
        return None;
    }
    let (shoff, shentsize, shnum) = (u64_at(0x28)? as usize, u16_at(0x3a)? as usize, u16_at(0x3c)? as usize);
    let section = |i: usize| {
        let o = shoff + i * shentsize;
        Some((u32_at(o + 4)?, u64_at(o + 0x18)? as usize, u64_at(o + 0x20)? as usize, u32_at(o + 0x28)? as usize))
    };
    let mut out = HashMap::new();
    for i in 0..shnum {
        let (kind, off, size, link) = section(i)?;
        // SHT_DYNSYM; its names in the linked string table.
        if kind != 11 {
            continue;
        }
        let (_, stroff, _, _) = section(link)?;
        for s in 0..size / 24 {
            let o = off + 24 * s;
            let name = stroff + u32_at(o)? as usize;
            let Some(end) = elf.get(name..).and_then(|b| b.iter().position(|&c| c == 0)) else { continue };
            let n = String::from_utf8_lossy(&elf[name..name + end]);
            if n.starts_with("gHotSpot") {
                out.insert(n.into_owned(), r.start + u64_at(o + 8)?);
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The same `gHotSpot*` symbols on Windows: jvm.dll's exports, read from the loaded image (its
/// first mapping holds the PE headers; the export directory is data directory 0).
fn hotspot_exports(pid: u32, mem: &Mem) -> Option<HashMap<String, u64>> {
    let maps = crate::helper::maps(pid).ok()?;
    let base = maps.iter().find(|r| r.path.to_ascii_lowercase().ends_with("/jvm.dll"))?.start;
    let (u16_at, u32_at) = (|a: u64| short(mem, a), |a: u64| half(mem, a));
    if u16_at(base)? != 0x5A4D {
        return None;
    }
    let pe = base + u32_at(base + 0x3C)? as u64;
    // PE32+ only (a 64-bit JVM): the data directories start 112 bytes into its optional header.
    if u32_at(pe)? != 0x4550 || u16_at(pe + 24)? != 0x20B {
        return None;
    }
    let dir = base + u32_at(pe + 24 + 112)? as u64;
    let (count, funcs, names, ords) = (u32_at(dir + 24)?, u32_at(dir + 28)? as u64, u32_at(dir + 32)? as u64, u32_at(dir + 36)? as u64);
    let mut out = HashMap::new();
    for i in 0..count.min(100_000) as u64 {
        let Some(n) = u32_at(base + names + 4 * i).and_then(|r| cstr(mem, base + r as u64)) else { continue };
        if n.starts_with("gHotSpot") {
            let ord = u16_at(base + ords + 2 * i)? as u64;
            out.insert(n, base + u32_at(base + funcs + 4 * ord)? as u64);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The JVM's tables: fields (type, field) -> (type string, static, offset, address), types ->
/// size, integer constants.
type Structs = HashMap<(String, String), (Option<String>, bool, u64, u64)>;

fn read_tables(mem: &Mem, syms: &HashMap<String, u64>) -> Option<(Structs, HashMap<String, u64>, HashMap<String, i32>)> {
    let sym = |n: &str| word(mem, *syms.get(n)?);
    let mut structs = HashMap::new();
    let (mut e, stride) = (sym("gHotSpotVMStructs")?, sym("gHotSpotVMStructEntryArrayStride")?);
    let off = |k: &str| sym(&format!("gHotSpotVMStructEntry{k}Offset"));
    let (tn, fname, ts, st, of, ad) = (off("TypeName")?, off("FieldName")?, off("TypeString")?, off("IsStatic")?, off("Offset")?, off("Address")?);
    for _ in 0..10000 {
        let f = word(mem, e + fname)?;
        if f == 0 {
            break;
        }
        let key = (cstr(mem, word(mem, e + tn)?)?, cstr(mem, f)?);
        structs.insert(key, (word(mem, e + ts).and_then(|s| cstr(mem, s)), half(mem, e + st)? != 0, word(mem, e + of)?, word(mem, e + ad)?));
        e += stride;
    }
    let mut types = HashMap::new();
    let (mut e, stride) = (sym("gHotSpotVMTypes")?, sym("gHotSpotVMTypeEntryArrayStride")?);
    let (tn, sz) = (sym("gHotSpotVMTypeEntryTypeNameOffset")?, sym("gHotSpotVMTypeEntrySizeOffset")?);
    for _ in 0..10000 {
        let n = word(mem, e + tn)?;
        if n == 0 {
            break;
        }
        types.insert(cstr(mem, n)?, word(mem, e + sz)?);
        e += stride;
    }
    let mut ints = HashMap::new();
    if let (Some(mut e), Some(stride), Some(no), Some(vo)) = (
        sym("gHotSpotVMIntConstants"),
        sym("gHotSpotVMIntConstantEntryArrayStride"),
        sym("gHotSpotVMIntConstantEntryNameOffset"),
        sym("gHotSpotVMIntConstantEntryValueOffset"),
    ) {
        for _ in 0..20000 {
            let Some(n) = word(mem, e + no).filter(|&n| n != 0) else { break };
            if let (Some(name), Some(v)) = (cstr(mem, n), half(mem, e + vo)) {
                ints.insert(name, v as i32);
            }
            e += stride;
        }
    }
    Some((structs, types, ints))
}

/// Decodes one UNSIGNED5 number (HotSpot's compressed ints: bytes 1-255, 191 "low" values,
/// then 64 per further byte).
fn unsigned5(b: &[u8], i: &mut usize) -> Option<u32> {
    let mut sum = 0u32;
    let mut shift = 0;
    for n in 0..5 {
        let x = u32::from(b.get(*i)?.checked_sub(1)?);
        *i += 1;
        sum = sum.wrapping_add(x << shift);
        if x < 191 || n == 4 {
            return Some(sum);
        }
        shift += 6;
    }
    Some(sum)
}

impl Jvm {
    fn locate(pid: u32, mem: &Mem) -> Option<Jvm> {
        let syms = hotspot_symbols(pid).or_else(|| hotspot_exports(pid, mem))?;
        let (structs, types, ints) = read_tables(mem, &syms)?;
        let off = |t: &str, f: &str| structs.get(&(t.to_owned(), f.to_owned())).filter(|v| !v.1).map(|v| v.2);
        let addr = |t: &str, f: &str| structs.get(&(t.to_owned(), f.to_owned())).filter(|v| v.1).map(|v| v.3);
        let flag = |name: &str| -> Option<bool> {
            let ty = if types.contains_key("JVMFlag") { "JVMFlag" } else { "Flag" };
            let (flags, n, size) = (word(mem, addr(ty, "flags")?)?, word(mem, addr(ty, "numFlags")?)?, *types.get(ty)?);
            (0..n.min(5000)).find_map(|i| {
                let e = flags + i * size;
                (cstr(mem, word(mem, e + off(ty, "_name")?)?)? == name).then(|| word(mem, e + off(ty, "_addr")?).and_then(|a| byte(mem, a)).map(|b| b != 0))?
            })
        };
        // The first of these names the JVM has: JDK 8-11 kept them in Universe, 12-26 in
        // CompressedOops/CompressedKlassPointers as `_narrow_oop._base`..., 27 as `_base`.
        let any = |names: &[(&str, &str)]| names.iter().find_map(|(t, f)| addr(t, f));
        let oop_base = word(mem, any(&[("CompressedOops", "_base"), ("CompressedOops", "_narrow_oop._base"), ("Universe", "_narrow_oop._base")])?)?;
        let oop_shift = half(mem, any(&[("CompressedOops", "_shift"), ("CompressedOops", "_narrow_oop._shift"), ("Universe", "_narrow_oop._shift")])?)?;
        let klass_base = word(mem, any(&[("CompressedKlassPointers", "_base"), ("CompressedKlassPointers", "_narrow_klass._base"), ("Universe", "_narrow_klass._base")])?)?;
        let klass_shift =
            half(mem, any(&[("CompressedKlassPointers", "_shift"), ("CompressedKlassPointers", "_narrow_klass._shift"), ("Universe", "_narrow_klass._shift")])?)?;
        let mirror_type = structs.get(&("Klass".into(), "_java_mirror".into()))?.0.clone().unwrap_or_default();
        let old = match (off("InstanceKlass", "_fields"), off("InstanceKlass", "_java_fields_count")) {
            (Some(fields), Some(count)) => Some(OldFields {
                fields,
                count,
                data: off("Array<u2>", "_data").unwrap_or(4),
                slots: *ints.get("FieldInfo::field_slots")? as u64,
                access: *ints.get("FieldInfo::access_flags_offset")? as u64,
                name: *ints.get("FieldInfo::name_index_offset")? as u64,
                sig: *ints.get("FieldInfo::signature_index_offset")? as u64,
                low: *ints.get("FieldInfo::low_packed_offset")? as u64,
                high: *ints.get("FieldInfo::high_packed_offset")? as u64,
                tag: ints.get("FIELDINFO_TAG_SIZE").copied().unwrap_or(2) as u32,
            }),
            _ => None,
        };
        let stream = off("InstanceKlass", "_fieldinfo_stream").map(|s| (s, off("Array<u1>", "_data").unwrap_or(4)));
        if stream.is_none() && old.is_none() {
            return None;
        }
        let major = addr("Abstract_VM_Version", "_vm_major_version").and_then(|a| half(mem, a)).unwrap_or(0);
        let threads = structs.get(&("JavaThread".into(), "_threadObj".into())).filter(|v| !v.1).map(|v| Threads {
            list: addr("ThreadsSMRSupport", "_java_thread_list").zip(off("ThreadsList", "_length")).zip(off("ThreadsList", "_threads")).map(|((a, l), t)| (a, l, t)),
            chain: addr("Threads", "_thread_list").zip(off("JavaThread", "_next")),
            obj: v.2,
            handle: v.0.as_deref().is_some_and(|t| t.contains("OopHandle")),
        });
        let mut jvm = Jvm {
            cld_head: addr("ClassLoaderDataGraph", "_head")?,
            cld_klasses: off("ClassLoaderData", "_klasses")?,
            cld_next: off("ClassLoaderData", "_next")?,
            klass_name: off("Klass", "_name")?,
            klass_super: off("Klass", "_super")?,
            klass_next: off("Klass", "_next_link")?,
            klass_mirror: off("Klass", "_java_mirror")?,
            mirror_handle: mirror_type.contains("OopHandle"),
            ik_constants: off("InstanceKlass", "_constants")?,
            cp_size: *types.get("ConstantPool")?,
            sym_length: off("Symbol", "_length")?,
            sym_body: off("Symbol", "_body").or_else(|| off("Symbol", "_body[0]"))?,
            stream,
            scheme: SCHEMES[0],
            old,
            coops: flag("UseCompressedOops")?,
            oop_base,
            oop_shift,
            ckp: flag("UseCompressedClassPointers").unwrap_or(true),
            compact: flag("UseCompactObjectHeaders").unwrap_or(false),
            klass_base,
            klass_shift,
            klass_in_mark: ints.get("markWord::klass_shift").copied().unwrap_or(42) as u32,
            major,
            threads: threads.filter(|t| t.list.is_some() || t.chain.is_some()),
            classes: Mutex::new(Classes::default()),
            fields: Mutex::new(HashMap::new()),
        };
        jvm.scan_classes(mem);
        jvm.pick_scheme(mem);
        Some(jvm)
    }

    /// A Symbol's text (class and field names; class names with '/').
    fn symbol(&self, mem: &Mem, s: u64) -> Option<String> {
        let n = short(mem, s + self.sym_length)?;
        let mut b = vec![0u8; n as usize];
        mem.read_exact_at(&mut b, s + self.sym_body).ok()?;
        Some(String::from_utf8_lossy(&b).into_owned())
    }

    fn klass_name(&self, mem: &Mem, k: u64) -> Option<String> {
        self.symbol(mem, word(mem, k + self.klass_name)?)
    }

    /// Reads the class list again (classes load as the game goes; at most once a second).
    fn scan_classes(&self, mem: &Mem) {
        let mut c = self.classes.lock().unwrap();
        if c.at.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        let (mut by_name, mut all) = (HashMap::new(), HashSet::new());
        let mut cld = word(mem, self.cld_head).unwrap_or(0);
        while cld != 0 && all.len() < MAX_CLASSES {
            let mut k = word(mem, cld + self.cld_klasses).unwrap_or(0);
            while k != 0 && all.len() < MAX_CLASSES {
                if !all.insert(k) {
                    break;
                }
                if let Some(n) = self.klass_name(mem, k) {
                    by_name.entry(n).or_insert(k);
                }
                k = word(mem, k + self.klass_next).unwrap_or(0);
            }
            cld = word(mem, cld + self.cld_next).unwrap_or(0);
        }
        *c = Classes { by_name, all, at: Some(Instant::now()) };
    }

    /// The class of that name (with '/'), looking at the class list again when it isn't known.
    fn class(&self, mem: &Mem, name: &str) -> Option<u64> {
        if let Some(&k) = self.classes.lock().unwrap().by_name.get(name) {
            return Some(k);
        }
        self.scan_classes(mem);
        self.classes.lock().unwrap().by_name.get(name).copied()
    }

    fn known(&self, mem: &Mem, k: u64) -> bool {
        if self.classes.lock().unwrap().all.contains(&k) {
            return true;
        }
        self.scan_classes(mem);
        self.classes.lock().unwrap().all.contains(&k)
    }

    /// The flag bits of the field info stream: the scheme whose decoding of the game's
    /// classes ends where their streams do.
    fn pick_scheme(&mut self, mem: &Mem) {
        if self.stream.is_none() {
            return;
        }
        let classes: Vec<u64> = self.classes.lock().unwrap().all.iter().copied().take(400).collect();
        let best = SCHEMES.iter().max_by_key(|&&s| {
            self.scheme = s;
            classes.iter().filter(|&&k| self.decode_stream(mem, k).is_some_and(|(_, clean)| clean)).count()
        });
        self.scheme = *best.unwrap_or(&SCHEMES[0]);
    }

    fn cp_symbol(&self, mem: &Mem, cp: u64, i: u32) -> Option<String> {
        self.symbol(mem, word(mem, cp + self.cp_size + 8 * u64::from(i))?)
    }

    /// JDK 21+: a class's own fields from its field info stream, and whether the stream ended
    /// where it should.
    fn decode_stream(&self, mem: &Mem, k: u64) -> Option<(Vec<Field>, bool)> {
        let (at, data) = self.stream?;
        let a = word(mem, k + at)?;
        if a == 0 {
            return Some((Vec::new(), true));
        }
        let len = half(mem, a)?;
        if len > 1 << 20 {
            return None;
        }
        let mut b = vec![0u8; len as usize];
        mem.read_exact_at(&mut b, a + data).ok()?;
        let cp = word(mem, k + self.ik_constants)?;
        let mut i = 0;
        let java = unsigned5(&b, &mut i)?;
        let _injected = unsigned5(&b, &mut i)?;
        let mut out = Vec::new();
        for _ in 0..java.min(10000) {
            let (name, sig, offset, access, flags) = (unsigned5(&b, &mut i)?, unsigned5(&b, &mut i)?, unsigned5(&b, &mut i)?, unsigned5(&b, &mut i)?, unsigned5(&b, &mut i)?);
            for bit in self.scheme.optional.iter().flatten() {
                if flags >> bit & 1 == 1 {
                    unsigned5(&b, &mut i)?;
                }
            }
            out.push(Field { name: self.cp_symbol(mem, cp, name)?, sig: self.cp_symbol(mem, cp, sig)?, offset, is_static: access & 8 != 0, is_final: access & 0x10 != 0 });
        }
        // The rest: injected fields, then a zero byte (and, in later JDKs, nothing else).
        let clean = b.get(i..).is_some_and(|rest| rest.len() <= 1 + 8 * 4) && out.iter().all(|f| f.sig.len() >= 1 && "BCDFIJSZL[".contains(&f.sig[..1]));
        Some((out, clean))
    }

    /// JDK 8-20: a class's own fields from its `_fields` array.
    fn decode_old(&self, mem: &Mem, k: u64) -> Option<Vec<Field>> {
        let o = self.old?;
        let a = word(mem, k + o.fields)?;
        let count = short(mem, k + o.count)?;
        let cp = word(mem, k + self.ik_constants)?;
        let mut out = Vec::new();
        for f in 0..u64::from(count) {
            let slot = |s: u64| short(mem, a + o.data + 2 * (f * o.slots + s)).map(u32::from);
            let packed = slot(o.low)? | slot(o.high)? << 16;
            let access = slot(o.access)?;
            out.push(Field {
                name: self.cp_symbol(mem, cp, slot(o.name)?)?,
                sig: self.cp_symbol(mem, cp, slot(o.sig)?)?,
                offset: packed >> o.tag,
                is_static: access & 8 != 0,
                is_final: access & 0x10 != 0,
            });
        }
        Some(out)
    }

    /// A class's own fields (cached: a class's fields don't change).
    fn own_fields(&self, mem: &Mem, k: u64) -> Arc<Vec<Field>> {
        if let Some(f) = self.fields.lock().unwrap().get(&k) {
            return f.clone();
        }
        let name = self.klass_name(mem, k).unwrap_or_default();
        let fields = if name.starts_with('[') {
            Vec::new()
        } else if self.stream.is_some() {
            self.decode_stream(mem, k).map(|(f, _)| f).unwrap_or_default()
        } else {
            self.decode_old(mem, k).unwrap_or_default()
        };
        let fields = Arc::new(fields);
        self.fields.lock().unwrap().insert(k, fields.clone());
        fields
    }

    /// An object's instance fields, its own class's first, then its superclasses'.
    fn instance_fields(&self, mem: &Mem, mut k: u64) -> Vec<Field> {
        let mut out = Vec::new();
        for _ in 0..32 {
            if k == 0 {
                break;
            }
            out.extend(self.own_fields(mem, k).iter().filter(|f| !f.is_static).cloned());
            k = word(mem, k + self.klass_super).unwrap_or(0);
        }
        out
    }

    /// The instance fields an object's game classes declare (not those it inherits from a
    /// library's class: a libGDX Stage's `debugAll`, a Swing panel's sizes).
    fn game_fields(&self, mem: &Mem, mut k: u64) -> Vec<Field> {
        let mut out = Vec::new();
        for _ in 0..32 {
            if k == 0 {
                break;
            }
            if self.klass_name(mem, k).is_some_and(|n| !is_library(&n)) {
                out.extend(self.own_fields(mem, k).iter().filter(|f| !f.is_static).cloned());
            }
            k = word(mem, k + self.klass_super).unwrap_or(0);
        }
        out
    }

    /// Whether a class is an enum (its constants are objects too, never the game's state).
    fn is_enum(&self, mem: &Mem, mut k: u64) -> bool {
        for _ in 0..32 {
            k = match word(mem, k + self.klass_super) {
                Some(s) if s != 0 => s,
                _ => return false,
            };
            if self.klass_name(mem, k).as_deref() == Some("java/lang/Enum") {
                return true;
            }
        }
        false
    }

    /// A class's mirror (java.lang.Class object), where its statics are.
    fn mirror(&self, mem: &Mem, k: u64) -> Option<u64> {
        let m = word(mem, k + self.klass_mirror)?;
        let m = if self.mirror_handle { word(mem, m)? } else { m };
        (m != 0).then_some(m)
    }

    /// The object a reference slot points to (0 for null).
    fn reference(&self, mem: &Mem, at: u64) -> Option<u64> {
        match self.coops {
            true => half(mem, at).map(|n| if n == 0 { 0 } else { self.oop_base + (u64::from(n) << self.oop_shift) }),
            false => word(mem, at),
        }
    }

    /// An object's class, if the object is sound: in the heap, not being moved by the collector,
    /// of a class the JVM has loaded.
    fn klass_of(&self, heap: &Heap, o: u64) -> Option<u64> {
        let mem = heap.mem();
        if o == 0 || o % 8 != 0 || !heap.is_pointer(o) {
            return None;
        }
        let mark = word(mem, o)?;
        // Lock bits 11: marked, the object is being moved (its mark word points to the copy).
        if mark & 3 == 3 {
            return None;
        }
        let k = match (self.compact, self.ckp) {
            (true, _) => self.klass_base + ((mark >> self.klass_in_mark) << self.klass_shift),
            (false, true) => self.klass_base + (u64::from(half(mem, o + 8)?) << self.klass_shift),
            (false, false) => word(mem, o + 8)?,
        };
        self.known(mem, k).then_some(k)
    }

    /// Where an array's length and first element are.
    fn array_layout(&self, elem: u64) -> (u64, u64) {
        match (self.compact, self.ckp) {
            (true, _) => (8, if elem == 8 { 16 } else { 12 }),
            (false, true) => (12, 16),
            (false, false) => (16, if self.major >= 22 && elem < 8 { 20 } else { 24 }),
        }
    }

    /// What a field of this type holds at `at`.
    fn val(&self, mem: &Mem, at: u64, sig: &str) -> Val {
        match sig.as_bytes().first() {
            Some(b'I') => Val::Num(at, "i32"),
            Some(b'F') => Val::Num(at, "f32"),
            Some(b'D') => Val::Num(at, "f64"),
            Some(b'S' | b'C') => Val::Num(at, "u16"),
            Some(b'J') => Val::Num(at, "i64"),
            Some(b'B') => Val::Num(at, "i8"),
            // A boolean is a byte holding 0 or 1.
            Some(b'Z') => Val::Num(at, "bit0"),
            Some(b'L' | b'[') => self.reference(mem, at).map_or(Val::Other, Val::Obj),
            _ => Val::Other,
        }
    }

    /// An array's elements (up to `MAX_ELEMENTS`): index and what it holds.
    fn elements(&self, heap: &Heap, a: u64, name: &str) -> Vec<(u32, Val)> {
        self.elements_up_to(heap, a, name, MAX_ELEMENTS)
    }

    fn elements_up_to(&self, heap: &Heap, a: u64, name: &str, most: u32) -> Vec<(u32, Val)> {
        let mem = heap.mem();
        let elem_sig = &name[1..];
        let size = match elem_sig.as_bytes().first() {
            Some(b'D' | b'J') => 8,
            Some(b'S' | b'C') => 2,
            Some(b'Z' | b'B') => 1,
            Some(b'L' | b'[') if !self.coops => 8,
            _ => 4,
        };
        let (len_at, base) = self.array_layout(size);
        let Some(len) = half(mem, a + len_at) else { return Vec::new() };
        (0..len.min(most)).map(|i| (i, self.val(mem, a + base + size * u64::from(i), elem_sig))).collect()
    }

    /// What an object's field holds (`k`: the object's class).
    fn field(&self, mem: &Mem, o: u64, k: u64, name: &str) -> Option<Val> {
        let f = self.instance_fields(mem, k).into_iter().find(|f| f.name == name)?;
        Some(self.val(mem, o + u64::from(f.offset), &f.sig))
    }

    /// The object a field points to, checked, with its class.
    fn field_object(&self, heap: &Heap, o: u64, k: u64, name: &str) -> Option<(u64, u64)> {
        match self.field(heap.mem(), o, k, name)? {
            Val::Obj(x) if x != 0 => Some((x, self.klass_of(heap, x)?)),
            _ => None,
        }
    }

    /// A java.lang.String's text (JDK 9+: bytes, Latin-1 or UTF-16 by `coder`; JDK 8: chars).
    fn string(&self, heap: &Heap, o: u64, k: u64) -> Option<String> {
        let mem = heap.mem();
        let (a, ak) = self.field_object(heap, o, k, "value")?;
        let wide = match self.klass_name(mem, ak)?.as_str() {
            "[C" => true,
            "[B" => matches!(self.field(mem, o, k, "coder"), Some(Val::Num(at, _)) if byte(mem, at) == Some(1)),
            _ => return None,
        };
        let (len_at, base) = self.array_layout(if wide { 2 } else { 1 });
        let len = half(mem, a + len_at)? as usize;
        let bytes = if self.klass_name(mem, ak)? == "[C" { len * 2 } else { len };
        if bytes > 1024 {
            return None;
        }
        let mut b = vec![0u8; bytes];
        mem.read_exact_at(&mut b, a + base).ok()?;
        match wide {
            true => String::from_utf16(&b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()).ok(),
            false => Some(b.iter().map(|&c| char::from(c)).collect()),
        }
    }

    /// A map's key as a path names it: a string's text, a boxed whole number's digits, an enum
    /// constant's name; None for other keys (their entries aren't listed).
    fn key_text(&self, heap: &Heap, v: Val) -> Option<String> {
        let mem = heap.mem();
        let o = match v {
            Val::Num(at, kind) => return whole(mem, at, kind).map(|n| n.to_string()),
            Val::Obj(o) if o != 0 => o,
            _ => return None,
        };
        let k = self.klass_of(heap, o)?;
        match self.klass_name(mem, k)?.as_str() {
            "java/lang/String" => self.string(heap, o, k),
            "java/lang/Integer" | "java/lang/Long" | "java/lang/Short" | "java/lang/Byte" => self.key_text(heap, self.field(mem, o, k, "value")?),
            _ if self.is_enum(mem, k) => {
                let (n, nk) = self.field_object(heap, o, k, "name")?;
                self.string(heap, n, nk)
            }
            _ => None,
        }
    }

    /// Which of `MAPS` a class is or extends.
    fn map_kind(&self, mem: &Mem, mut k: u64) -> Option<&'static str> {
        for _ in 0..32 {
            if k == 0 {
                return None;
            }
            let name = self.klass_name(mem, k)?;
            if let Some(m) = MAPS.iter().find(|m| **m == name) {
                return Some(m);
            }
            k = word(mem, k + self.klass_super)?;
        }
        None
    }

    /// A map's entries (up to `most`: the list shows `MAX_ELEMENTS`, a saved path looks among
    /// `MAX_SLOTS`): each key's text and what its value holds. None for anything but `MAPS`.
    fn entries(&self, heap: &Heap, o: u64, k: u64, most: u32) -> Option<Vec<(String, Val)>> {
        let mem = heap.mem();
        let most = most as usize;
        let mut out: Vec<(Val, Val)> = Vec::new();
        let array = |name: &str| -> Vec<Val> {
            let Some((a, ak)) = self.field_object(heap, o, k, name) else { return Vec::new() };
            let Some(an) = self.klass_name(mem, ak).filter(|n| n.starts_with('[')) else { return Vec::new() };
            self.elements_up_to(heap, a, &an, MAX_SLOTS).into_iter().map(|(_, v)| v).collect()
        };
        match self.map_kind(mem, k)? {
            "java/util/TreeMap" => {
                let mut stack: Vec<(u64, u64)> = self.field_object(heap, o, k, "root").into_iter().collect();
                while let Some((n, nk)) = stack.pop() {
                    if out.len() >= most || stack.len() > most {
                        break;
                    }
                    if let (Some(key), Some(v)) = (self.field(mem, n, nk, "key"), self.field(mem, n, nk, "value")) {
                        out.push((key, v));
                    }
                    stack.extend(["left", "right"].iter().filter_map(|s| self.field_object(heap, n, nk, s)));
                }
            }
            "java/util/EnumMap" => {
                let keys = array("keyUniverse");
                out.extend(keys.into_iter().zip(array("vals")).filter(|(_, v)| *v != Val::Obj(0)).take(most));
            }
            m if m.starts_with("com/badlogic/") => {
                // A key of 0 (IntMap's) or null is an empty slot.
                let keys = array("keyTable");
                out.extend(keys.into_iter().zip(array("valueTable")).filter(|(key, _)| *key != Val::Obj(0) && !matches!(key, Val::Num(at, kind) if whole(mem, *at, kind) == Some(0))).take(most));
            }
            m => {
                let value = if m.ends_with("ConcurrentHashMap") { "val" } else { "value" };
                for slot in array("table") {
                    let Val::Obj(mut n) = slot else { continue };
                    // A bucket's chain (a tree's nodes are chained too).
                    for _ in 0..64 {
                        let Some(nk) = self.klass_of(heap, n) else { break };
                        match self.klass_name(mem, nk).as_deref() {
                            // ConcurrentHashMap: a tree's bin holds its first node; a resize's
                            // placeholders hold nothing.
                            Some("java/util/concurrent/ConcurrentHashMap$TreeBin") => match self.field_object(heap, n, nk, "first") {
                                Some((f, _)) => n = f,
                                None => break,
                            },
                            Some(c) if c.starts_with("java/util/concurrent/ConcurrentHashMap$") && !c.ends_with("$Node") && !c.ends_with("$TreeNode") => break,
                            _ => {
                                if let (Some(key), Some(v)) = (self.field(mem, n, nk, "key"), self.field(mem, n, nk, value)) {
                                    out.push((key, v));
                                }
                                match self.field_object(heap, n, nk, "next") {
                                    Some((x, _)) => n = x,
                                    None => break,
                                }
                            }
                        }
                    }
                    if out.len() >= most {
                        break;
                    }
                }
            }
        }
        Some(out.into_iter().take(most).filter_map(|(key, v)| Some((self.key_text(heap, key)?, v))).collect())
    }

    /// The java.lang.Thread objects of the JVM's Java threads.
    fn thread_objects(&self, mem: &Mem) -> Vec<u64> {
        let Some(t) = self.threads else { return Vec::new() };
        let mut threads = Vec::new();
        if let Some((list, len, array)) = t.list {
            if let Some(l) = word(mem, list).filter(|&l| l != 0) {
                let n = half(mem, l + len).unwrap_or(0).min(4096);
                let a = word(mem, l + array).unwrap_or(0);
                threads.extend((0..u64::from(n)).filter_map(|i| word(mem, a + 8 * i)));
            }
        } else if let Some((head, next)) = t.chain {
            let mut jt = word(mem, head).unwrap_or(0);
            while jt != 0 && threads.len() < 4096 {
                threads.push(jt);
                jt = word(mem, jt + next).unwrap_or(0);
            }
        }
        threads
            .into_iter()
            .filter_map(|jt| {
                let o = word(mem, jt + t.obj)?;
                if t.handle { word(mem, o) } else { Some(o) }
            })
            .filter(|&o| o != 0)
            .collect()
    }

    /// The game's objects its threads run, with their classes: a Runnable given to `new Thread`
    /// (JDK 19+ keeps it in the thread's `holder.task`, before in `target`), a lambda's captured
    /// objects, a game class extending Thread. Roots for games no static field leads into
    /// (Blue Boy Adventure: `GamePanel implements Runnable`, holding the player).
    fn thread_roots(&self, heap: &Heap) -> Vec<(u64, u64)> {
        let mem = heap.mem();
        let mut out: Vec<(u64, u64)> = Vec::new();
        for t in self.thread_objects(mem) {
            let Some(tk) = self.klass_of(heap, t) else { continue };
            let mut objects = vec![(t, tk)];
            let task = match self.field_object(heap, t, tk, "holder") {
                Some((h, hk)) => self.field_object(heap, h, hk, "task"),
                None => self.field_object(heap, t, tk, "target"),
            };
            match task {
                Some((o, ok)) if self.klass_name(mem, ok).is_some_and(|n| n.contains("$$Lambda")) => {
                    for f in self.instance_fields(mem, ok) {
                        if let Val::Obj(x) = self.val(mem, o + u64::from(f.offset), &f.sig) {
                            objects.extend(self.klass_of(heap, x).map(|xk| (x, xk)));
                        }
                    }
                }
                Some(task) => objects.push(task),
                None => {}
            }
            for (o, k) in objects {
                if self.klass_name(mem, k).is_some_and(|n| !is_library(&n) && !n.starts_with('[') && !n.contains("$$Lambda")) && !out.iter().any(|r| r.0 == o) {
                    out.push((o, k));
                }
            }
        }
        out
    }
}

/// A whole number of one of the list's types, as a key (IntMap's, a boxed number's).
fn whole(mem: &Mem, at: u64, kind: &str) -> Option<i64> {
    match kind {
        "i32" => half(mem, at).map(|v| i64::from(v as i32)),
        "i64" => word(mem, at).map(|v| v as i64),
        "u16" => short(mem, at).map(i64::from),
        "i8" => byte(mem, at).map(|v| i64::from(v as i8)),
        _ => None,
    }
}

/// The located JVM of the game attached to, per pid; a failed search is tried again after
/// `RETRY`.
pub fn shared(pid: u32, mem: &Mem) -> Option<Arc<Jvm>> {
    const RETRY: Duration = Duration::from_secs(10);
    static FOUND: Mutex<Option<(u32, Option<Arc<Jvm>>, Instant)>> = Mutex::new(None);
    let mut found = FOUND.lock().unwrap();
    if let Some((p, jvm, at)) = found.as_ref() {
        if *p == pid && (jvm.is_some() || at.elapsed() < RETRY) {
            return jvm.clone();
        }
    }
    let jvm = Jvm::locate(pid, mem).map(Arc::new);
    *found = Some((pid, jvm.clone(), Instant::now()));
    jvm
}

fn is_library(class: &str) -> bool {
    LIBRARIES.iter().any(|p| class.starts_with(p))
}

/// The last part of a class name: `com/example/Game$Player` -> `Game$Player`.
fn short_name(class: &str) -> &str {
    class.rsplit('/').next().unwrap_or(class)
}

/// One step of a path: a field by name, an array's element, or a map's entry.
#[derive(Clone, Debug, PartialEq)]
enum Step {
    Field(String),
    Index(u32),
    /// A map's entry by its key's text.
    Key(String),
}

/// A saved path from a class's static field to a number.
#[derive(Clone, Debug, PartialEq)]
pub struct JavaPath {
    /// The class, with '/' (as the JVM names it).
    class: String,
    /// From the object of that class one of the game's threads runs (`java:thread:<class>#`,
    /// the steps then start with its field), else from the class's static field.
    thread: bool,
    steps: Vec<Step>,
}

impl JavaPath {
    /// "java:[thread:]<class with dots>#<static or field>[.<field>|[i]|["<key>"]]...".
    pub fn parse(text: &str) -> Option<JavaPath> {
        let text = text.strip_prefix("java:")?;
        let (text, thread) = text.strip_prefix("thread:").map_or((text, false), |t| (t, true));
        let (class, mut rest) = text.split_once('#')?;
        if class.is_empty() || class.contains(['/', ' ', ':']) || rest.is_empty() {
            return None;
        }
        let mut steps = Vec::new();
        while !rest.is_empty() {
            let (sep, body) = match rest.as_bytes()[0] {
                b'.' | b'[' if !steps.is_empty() => (rest.as_bytes()[0], &rest[1..]),
                _ if steps.is_empty() => (b'.', rest),
                _ => return None,
            };
            if sep == b'[' {
                if let Some(key) = body.strip_prefix('"') {
                    let (k, after) = key.split_once("\"]")?;
                    steps.push(Step::Key(decode(k)?));
                    rest = after;
                } else {
                    let (n, after) = body.split_once(']')?;
                    steps.push(Step::Index(n.parse().ok()?));
                    rest = after;
                }
                continue;
            }
            let end = body.find(['.', '[']).unwrap_or(body.len());
            if end == 0 {
                return None;
            }
            steps.push(Step::Field(body[..end].to_owned()));
            rest = &body[end..];
        }
        Some(JavaPath { class: class.replace('.', "/"), thread, steps })
    }

    pub fn text(&self) -> String {
        format!("java:{}{}#{}", if self.thread { "thread:" } else { "" }, self.class.replace('/', "."), self.fields_text(true))
    }

    /// The steps; `encoded`: keys as a saved path writes them (`%20`), else as the list shows them.
    fn fields_text(&self, encoded: bool) -> String {
        let mut t = String::new();
        for s in &self.steps {
            match s {
                Step::Field(n) if t.is_empty() => t += n,
                Step::Field(n) => t += &format!(".{n}"),
                Step::Index(i) => t += &format!("[{i}]"),
                Step::Key(k) => t += &format!("[\"{}\"]", if encoded { encode(k) } else { k.clone() }),
            }
        }
        t
    }

    /// As the list shows it: the class's short name and the fields (`Dungeon.hero.HP`).
    fn shown(&self) -> String {
        match self.steps.is_empty() {
            true => short_name(&self.class).to_owned(),
            false => format!("{}.{}", short_name(&self.class), self.fields_text(false)),
        }
    }

    pub fn describe(&self) -> String {
        format!("{} in the game's Java objects", self.shown())
    }

    fn then(&self, s: Step) -> JavaPath {
        let mut p = self.clone();
        p.steps.push(s);
        p
    }

    /// The path up to its first player-like field (`Dungeon.hero` of
    /// `Dungeon.hero.belongings.backpack`), if it goes through one.
    fn player_part(&self) -> Option<JavaPath> {
        let i = self.steps.iter().position(|s| matches!(s, Step::Field(n) if the_player(n)))?;
        Some(JavaPath { class: self.class.clone(), thread: self.thread, steps: self.steps[..=i].to_vec() })
    }

    /// Whether the path goes through how something looks or sounds (`hero.sprite.scale`):
    /// listed, not among the player's own numbers.
    fn presentation(&self) -> bool {
        const WORDS: &[&str] = &["sprite", "parent", "texture", "buffer", "vertices", "matrix", "anim", "emitter", "camera", "font", "image", "icon", "sound", "shader", "mesh", "frame", "particle"];
        self.steps.iter().any(|s| matches!(s, Step::Field(n) if WORDS.iter().any(|w| n.to_ascii_lowercase().contains(w))))
    }

    fn last_field(&self) -> Option<&str> {
        self.steps.iter().rev().find_map(|s| match s {
            Step::Field(n) => Some(n.as_str()),
            _ => None,
        })
    }
}

/// The class the path starts from.
pub fn find_roots(heap: &Heap, path: &JavaPath) -> Vec<u64> {
    let Some(jvm) = shared(heap.pid, heap.mem()) else { return Vec::new() };
    jvm.class(heap.mem(), &path.class).into_iter().collect()
}

/// Where the path leads from its class now: the number's address, every object along it
/// checked (see the module's notes).
pub fn walk(heap: &Heap, roots: &[u64], path: &JavaPath) -> Vec<u64> {
    let Some(jvm) = shared(heap.pid, heap.mem()) else { return Vec::new() };
    roots.iter().filter_map(|&k| follow(&jvm, heap, k, path)).collect()
}

fn follow(jvm: &Jvm, heap: &Heap, k: u64, path: &JavaPath) -> Option<u64> {
    let mem = heap.mem();
    if jvm.klass_name(mem, k)? != path.class {
        return None;
    }
    // A thread's object is looked up again each time (the collector moves it).
    let (mut cur, steps) = if path.thread {
        (Val::Obj(jvm.thread_roots(heap).into_iter().find(|r| r.1 == k)?.0), &path.steps[..])
    } else {
        let mirror = jvm.mirror(mem, k)?;
        let Some(Step::Field(first)) = path.steps.first() else { return None };
        let f = jvm.own_fields(mem, k).iter().find(|f| f.is_static && f.name == *first)?.clone();
        (jvm.val(mem, mirror + u64::from(f.offset), &f.sig), &path.steps[1..])
    };
    for s in steps {
        let Val::Obj(o) = cur else { return None };
        let ok = jvm.klass_of(heap, o)?;
        cur = match s {
            Step::Field(name) => {
                let f = jvm.instance_fields(mem, ok).into_iter().find(|f| f.name == *name)?;
                jvm.val(mem, o + u64::from(f.offset), &f.sig)
            }
            Step::Index(i) => {
                let name = jvm.klass_name(mem, ok)?;
                if !name.starts_with('[') {
                    return None;
                }
                jvm.elements(heap, o, &name).into_iter().find(|(x, _)| x == i)?.1
            }
            Step::Key(key) => jvm.entries(heap, o, ok, MAX_SLOTS)?.into_iter().find(|(x, _)| x == key)?.1,
        };
    }
    match cur {
        Val::Num(at, _) => Some(at),
        _ => None,
    }
}

/// A class's statics or an object whose numbers are listed.
struct Group {
    path: JavaPath,
    class: String,
    statics: bool,
    numbers: Vec<(String, u64, &'static str, JavaPath)>,
}

/// Collections looked through (their own numbers aren't listed).
fn container(class: &str) -> Option<&'static str> {
    match class {
        "java/util/ArrayList" | "java/util/Vector" => Some("elementData"),
        "com/badlogic/gdx/utils/Array" | "com/badlogic/gdx/utils/IntArray" | "com/badlogic/gdx/utils/FloatArray" => Some("items"),
        _ => None,
    }
}

/// Every number reachable from the game's classes' static fields (Java's and common
/// libraries' classes left out; their lists looked through), breadth first, so each object is
/// named by its shortest path.
fn groups(heap: &Heap) -> Option<Vec<Group>> {
    let jvm = shared(heap.pid, heap.mem())?;
    let mem = heap.mem();
    jvm.scan_classes(mem);
    let mut classes: Vec<(String, u64)> = jvm.classes.lock().unwrap().by_name.iter().filter(|(n, _)| !is_library(n) && !n.starts_with('[')).map(|(n, &k)| (n.clone(), k)).collect();
    classes.sort();
    let mut out = Vec::new();
    // Paths through a player-like field (`Dungeon.hero...`) are walked first: the list stops at
    // `MAX_LISTED` numbers, and a game's UI objects alone can fill it before the hero's
    // inventory, four steps down, is reached (Shattered Pixel Dungeon).
    let mut queues: [VecDeque<(u64, JavaPath, usize)>; 2] = [VecDeque::new(), VecDeque::new()];
    let queue_of = |p: &JavaPath| usize::from(p.player_part().is_none());
    let mut seen: HashSet<u64> = HashSet::new();
    for (name, k) in classes {
        let statics: Vec<Field> = jvm.own_fields(mem, k).iter().filter(|f| f.is_static).cloned().collect();
        if statics.is_empty() || jvm.is_enum(mem, k) {
            continue;
        }
        let Some(mirror) = jvm.mirror(mem, k) else { continue };
        let root = JavaPath { class: name.clone(), thread: false, steps: Vec::new() };
        let mut g = Group { path: root.clone(), class: name.clone(), statics: true, numbers: Vec::new() };
        // `$SwitchMap$...` and other fields the compiler made.
        for f in statics.into_iter().filter(|f| !f.name.starts_with('$')) {
            let p = root.then(Step::Field(f.name.clone()));
            match jvm.val(mem, mirror + u64::from(f.offset), &f.sig) {
                Val::Num(at, kind) if !f.is_final => g.numbers.push((f.name.clone(), at, kind, p)),
                Val::Obj(o) if o != 0 => queues[queue_of(&p)].push_back((o, p, 1)),
                _ => {}
            }
        }
        if !g.numbers.is_empty() {
            out.push(g);
        }
    }
    for (o, k) in jvm.thread_roots(heap) {
        if let Some(class) = jvm.klass_name(mem, k) {
            let p = JavaPath { class, thread: true, steps: Vec::new() };
            queues[queue_of(&p)].push_back((o, p, 1));
        }
    }
    let mut visited = 0;
    let mut listed = 0;
    // An object is named by the first path taken off the queues (not the first queued: a UI
    // static's `lastBag` would take the hero's backpack).
    while let Some((o, path, depth)) = queues[0].pop_front().or_else(|| queues[1].pop_front()) {
        if !seen.insert(o) {
            continue;
        }
        visited += 1;
        if visited > MAX_VISITED || listed > MAX_LISTED {
            break;
        }
        let Some(k) = jvm.klass_of(heap, o) else { continue };
        let Some(class) = jvm.klass_name(mem, k) else { continue };
        if !class.starts_with('[') && jvm.is_enum(mem, k) {
            continue;
        }
        let mut g = Group { path: path.clone(), class: class.clone(), statics: false, numbers: Vec::new() };
        let mut refs: Vec<(Step, u64)> = Vec::new();
        // Byte and boolean arrays are buffers and grids (a save's bytes, a map's tiles), not the
        // player's numbers.
        if class == "[B" || class == "[Z" {
        } else if class.starts_with('[') {
            for (i, v) in jvm.elements(heap, o, &class) {
                match v {
                    Val::Num(at, kind) => g.numbers.push((format!("[{i}]"), at, kind, path.then(Step::Index(i)))),
                    Val::Obj(x) if x != 0 => refs.push((Step::Index(i), x)),
                    _ => {}
                }
            }
        } else if let Some(entries) = jvm.entries(heap, o, k, MAX_ELEMENTS) {
            for (key, v) in entries {
                let p = path.then(Step::Key(key.clone()));
                match v {
                    Val::Num(at, kind) => g.numbers.push((format!("[\"{key}\"]"), at, kind, p)),
                    Val::Obj(x) if x != 0 => refs.push((Step::Key(key), x)),
                    _ => {}
                }
            }
        } else if let Some(items) = container(&class) {
            let f = jvm.instance_fields(mem, k).into_iter().find(|f| f.name == items);
            if let Some(Val::Obj(x)) = f.map(|f| jvm.val(mem, o + u64::from(f.offset), &f.sig)) {
                refs.push((Step::Field(items.to_owned()), x));
            }
        } else if !is_library(&class) {
            // A library superclass's numbers aren't listed (UI state); its references are
            // followed (libGDX's `Game.screen` leads to the game's screen).
            let own = jvm.game_fields(mem, k);
            for f in jvm.instance_fields(mem, k) {
                match jvm.val(mem, o + u64::from(f.offset), &f.sig) {
                    Val::Num(at, kind) if own.iter().any(|g| g.name == f.name && g.offset == f.offset) => {
                        g.numbers.push((f.name.clone(), at, kind, path.then(Step::Field(f.name.clone()))))
                    }
                    Val::Obj(x) if x != 0 => refs.push((Step::Field(f.name.clone()), x)),
                    _ => {}
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

/// Whether a field or class is the player itself: its whole name is one (`hero`, `player`,
/// `localPlayer`); not a UI object named after the player's things (`InventoryPane.gold`, an
/// image, or `BuffIndicator.heroInstance`).
fn the_player(name: &str) -> bool {
    let n: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    ["player", "hero", "localplayer", "theplayer", "mainplayer", "playerchar", "playercharacter", "character", "maincharacter", "avatar", "protagonist"].contains(&n.as_str())
}

/// Constants by convention (`MAX_HP`, `GOLD`): not the player's numbers.
fn constant(name: &str) -> bool {
    name.len() > 1 && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && name.chars().any(|c| c.is_ascii_uppercase())
}

/// The Browse tab's list for a Java game: the game's classes' statics and the objects they
/// lead to, the player's (by a player-like field or class name) and classes with a player-like
/// static number (`Dungeon.gold`) first.
pub fn player(heap: &Heap) -> Option<Vec<ListedObject>> {
    let groups = groups(heap)?;
    let mut out: Vec<ListedObject> = groups
        .into_iter()
        .map(|g| {
            let named = |n: &str| player_like(n) && !constant(n);
            let player = (g.path.last_field().is_some_and(the_player) || !g.statics && the_player(short_name(&g.class))) && !g.path.presentation();
            // Inside the player's object: its inventory, equipment, buffs.
            let owner = g.path.player_part().filter(|p| !player && *p != g.path && !g.path.presentation()).map(|p| p.shown());
            let main = player || owner.is_some() || g.statics && g.numbers.iter().any(|n| named(&n.0));
            ListedObject {
                name: g.path.shown(),
                class: short_name(&g.class).to_owned(),
                role: if player { "character" } else if owner.is_some() { "part" } else { "game" },
                owner,
                more: !main,
                fields: g.numbers.into_iter().map(|(label, addr, kind, _)| Listed { label, addr, kind }).collect(),
            }
        })
        .collect();
    out.sort_by_key(|o| (o.more, o.role != "character"));
    Some(out)
}

/// The path to a number the list has, checked by following it the way a restart does, and
/// the field's type (a search for a whole number lands on a `long`'s first 4 bytes, or a
/// `byte`'s: written as 4 bytes, either would go wrong).
pub fn discover(heap: &Heap, target: u64) -> Option<(JavaPath, Vec<u64>, &'static str)> {
    let (_, _, kind, path) = groups(heap)?.into_iter().flat_map(|g| g.numbers).find(|n| n.1 == target)?;
    let leads = walk(heap, &find_roots(heap, &path), &path);
    leads.contains(&target).then_some((path, leads, kind))
}

/// jvm [class]: what was found (the JVM, how it packs references, the field format), or a
/// class's fields with their values.
pub fn diagnose(heap: &Heap, class: &str) -> Vec<String> {
    let t = Instant::now();
    let mem = heap.mem();
    let Some(jvm) = shared(heap.pid, mem) else { return vec!["no HotSpot tables found (libjvm.so's gHotSpotVMStructs)".into()] };
    let n = jvm.classes.lock().unwrap().all.len();
    let mut out = vec![format!(
        "JDK {}, {} classes, compressed oops {} (base {:#x}, shift {}), class pointers {} (base {:#x}, shift {}), compact headers {}, field info {} ({} ms)",
        jvm.major,
        n,
        jvm.coops,
        jvm.oop_base,
        jvm.oop_shift,
        jvm.ckp,
        jvm.klass_base,
        jvm.klass_shift,
        jvm.compact,
        if jvm.stream.is_some() { format!("stream {:?}", jvm.scheme.optional) } else { "old".into() },
        t.elapsed().as_millis()
    )];
    if class.is_empty() {
        return out;
    }
    let name = class.replace('.', "/");
    let Some(k) = jvm.class(mem, &name) else { return vec![format!("{class}: not loaded")] };
    let mirror = jvm.mirror(mem, k).unwrap_or(0);
    for f in jvm.own_fields(mem, k).iter() {
        let at = if f.is_static { mirror } else { 0 } + u64::from(f.offset);
        let v = match (f.is_static, jvm.val(mem, at, &f.sig)) {
            (false, _) => String::new(),
            (true, Val::Num(a, "i32")) => half(mem, a).map(|v| (v as i32).to_string()).unwrap_or_default(),
            (true, Val::Num(a, "f32")) => half(mem, a).map(|v| f32::from_bits(v).to_string()).unwrap_or_default(),
            (true, Val::Num(a, "f64")) => word(mem, a).map(|v| f64::from_bits(v).to_string()).unwrap_or_default(),
            (true, Val::Num(a, "bit0")) => byte(mem, a).map(|v| (v != 0).to_string()).unwrap_or_default(),
            (true, Val::Num(a, k @ ("i64" | "i8"))) => whole(mem, a, k).map(|v| v.to_string()).unwrap_or_default(),
            (true, Val::Num(a, _)) => short(mem, a).map(|v| v.to_string()).unwrap_or_default(),
            (true, Val::Obj(o)) => format!("{o:#x}"),
            _ => String::new(),
        };
        out.push(format!("{} {} {} +{} {v}", if f.is_static { "static" } else { "field" }, f.name, f.sig, f.offset));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_read_back() {
        for text in ["java:com.shatteredpixel.shatteredpixeldungeon.Dungeon#gold", "java:a.b.Game#hero.HP", "java:Game$Inner#items.elementData[2].quantity", "java:G#grid[1][2]", "java:G#m[\"k\"][3]", "java:thread:main.GamePanel#player.life"] {
            assert_eq!(JavaPath::parse(text).unwrap().text(), text, "{text}");
        }
        let p = JavaPath::parse("java:a.b.Game#hero.items[3].hp").unwrap();
        assert_eq!(p.class, "a/b/Game");
        assert_eq!(p.steps, vec![Step::Field("hero".into()), Step::Field("items".into()), Step::Index(3), Step::Field("hp".into())]);
        assert_eq!(p.shown(), "Game.hero.items[3].hp");
        assert!(JavaPath::parse("java:Game").is_none());
        assert!(JavaPath::parse("java:#x").is_none());
        assert!(JavaPath::parse("java:G#a..b").is_none());
        assert!(JavaPath::parse("java:G#a[x]").is_none());
        let p = JavaPath::parse("java:G#stats[\"Iron%20Sword\"].count").unwrap();
        assert_eq!(p.steps, vec![Step::Field("stats".into()), Step::Key("Iron Sword".into()), Step::Field("count".into())]);
        assert_eq!(p.text(), "java:G#stats[\"Iron%20Sword\"].count");
        assert_eq!(p.shown(), "G.stats[\"Iron Sword\"].count");
        assert!(JavaPath::parse("java:G#m[\"a\"]x").is_none());
    }

    #[test]
    fn unsigned5_numbers() {
        let mut i = 0;
        // 0 -> 1; 190 -> 191; 191 -> [192, 1]: 191 + 0*64; 300 -> 191 + (300-191) = 109 more: [192 + 109 % 64, 1 + 109 / 64]
        let b = [1u8, 191, 192, 1, 192 + 45, 2];
        assert_eq!(unsigned5(&b, &mut i), Some(0));
        assert_eq!(unsigned5(&b, &mut i), Some(190));
        assert_eq!(unsigned5(&b, &mut i), Some(191));
        assert_eq!(unsigned5(&b, &mut i), Some(191 + 45 + 64));
        assert_eq!(i, 6);
    }
}
