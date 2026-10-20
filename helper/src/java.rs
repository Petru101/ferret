//! Java games on HotSpot (OpenJDK's JVM): their objects read from outside through the JVM's
//! own description of its layout. libjvm exports tables for its serviceability agent
//! (`gHotSpotVMStructs`, `gHotSpotVMTypes`, constants): every field of its C++ classes that
//! tools need, by name, with its offset (or a static's address). From those: the loaded
//! classes (ClassLoaderDataGraph -> ClassLoaderData -> Klass list), each class's name (a
//! Symbol), its fields (name, type, offset: InstanceKlass's field info, names in its constant
//! pool) and its statics (in the class's mirror, a java.lang.Class object). A value is named by
//! a class's static field and the fields from there: `java:com.example.Game#player.hp`,
//! `java:...Dungeon#gold`, `[i]` for an array's element (an ArrayList's `elementData[2]`).
//!
//! Java's collector moves objects: a found address goes stale at the next collection, and the
//! old copy's memory is reused. So a path is walked again right before every write (limits do
//! it on each check, `Named::fresh`), and every object along it is checked: in the heap, of a
//! class the JVM has loaded, not being moved (its mark word forwarded).

use crate::mono::player_like;
use crate::names::Heap;
use crate::ue::{Listed, ListedObject, MAX_LISTED};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Objects looked into for one list at most.
const MAX_VISITED: usize = 20000;
/// How deep the list goes from a static field (a list's element is two steps: `items.elementData[i]`;
/// a pouch's in the hero's backpack is nine down).
const MAX_DEPTH: usize = 12;
/// Elements read from one array at most.
const MAX_ELEMENTS: u32 = 256;
/// Classes read from the class list at most (a runaway list is garbage).
const MAX_CLASSES: usize = 200_000;
/// Packages that aren't the game's: Java's own, and common libraries (their statics and
/// objects aren't listed; their collections are looked through).
const LIBRARIES: &[&str] = &[
    "java/", "javax/", "jdk/", "sun/", "com/sun/", "kotlin/", "kotlinx/", "scala/", "org/lwjgl/", "com/badlogic/gdx/", "com/esotericsoftware/",
    "org/json/", "com/google/", "org/apache/", "org/slf4j/", "ch/qos/", "io/netty/", "it/unimi/", "org/objectweb/", "net/java/", "paulscode/",
    "de/jcm/", "org/jetbrains/", "org/intellij/", "com/fasterxml/", "org/yaml/", "javafx/", "com/jogamp/", "jogamp/", "org/joml/",
    "club/minnced/", "net/arikia/", "com/codedisaster/", "org/luaj/", "processing/core/", "processing/opengl/", "processing/awt/",
];

/// A field of a class, as its metadata describes it.
#[derive(Clone, Debug)]
pub struct Field {
    name: String,
    sig: String,
    offset: u32,
    is_static: bool,
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
    classes: Mutex<Classes>,
    fields: Mutex<HashMap<u64, Arc<Vec<Field>>>>,
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

fn word(mem: &File, at: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b, at).ok().map(|_| u64::from_le_bytes(b))
}

fn half(mem: &File, at: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    mem.read_exact_at(&mut b, at).ok().map(|_| u32::from_le_bytes(b))
}

fn short(mem: &File, at: u64) -> Option<u16> {
    let mut b = [0u8; 2];
    mem.read_exact_at(&mut b, at).ok().map(|_| u16::from_le_bytes(b))
}

fn byte(mem: &File, at: u64) -> Option<u8> {
    let mut b = [0u8; 1];
    mem.read_exact_at(&mut b, at).ok().map(|_| b[0])
}

fn cstr(mem: &File, at: u64) -> Option<String> {
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
    let root = std::path::Path::new("/proc").join(pid.to_string()).join("root");
    let elf = std::fs::read(root.join(r.path.trim_start_matches('/'))).or_else(|_| std::fs::read(&r.path)).ok()?;
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

/// The JVM's tables: fields (type, field) -> (type string, static, offset, address), types ->
/// size, integer constants.
type Structs = HashMap<(String, String), (Option<String>, bool, u64, u64)>;

fn read_tables(mem: &File, syms: &HashMap<String, u64>) -> Option<(Structs, HashMap<String, u64>, HashMap<String, i32>)> {
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
    fn locate(pid: u32, mem: &File) -> Option<Jvm> {
        let syms = hotspot_symbols(pid)?;
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
            classes: Mutex::new(Classes::default()),
            fields: Mutex::new(HashMap::new()),
        };
        jvm.scan_classes(mem);
        jvm.pick_scheme(mem);
        Some(jvm)
    }

    /// A Symbol's text (class and field names; class names with '/').
    fn symbol(&self, mem: &File, s: u64) -> Option<String> {
        let n = short(mem, s + self.sym_length)?;
        let mut b = vec![0u8; n as usize];
        mem.read_exact_at(&mut b, s + self.sym_body).ok()?;
        Some(String::from_utf8_lossy(&b).into_owned())
    }

    fn klass_name(&self, mem: &File, k: u64) -> Option<String> {
        self.symbol(mem, word(mem, k + self.klass_name)?)
    }

    /// Reads the class list again (classes load as the game goes; at most once a second).
    fn scan_classes(&self, mem: &File) {
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
    fn class(&self, mem: &File, name: &str) -> Option<u64> {
        if let Some(&k) = self.classes.lock().unwrap().by_name.get(name) {
            return Some(k);
        }
        self.scan_classes(mem);
        self.classes.lock().unwrap().by_name.get(name).copied()
    }

    fn known(&self, mem: &File, k: u64) -> bool {
        if self.classes.lock().unwrap().all.contains(&k) {
            return true;
        }
        self.scan_classes(mem);
        self.classes.lock().unwrap().all.contains(&k)
    }

    /// The flag bits of the field info stream: the scheme whose decoding of the game's
    /// classes ends where their streams do.
    fn pick_scheme(&mut self, mem: &File) {
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

    fn cp_symbol(&self, mem: &File, cp: u64, i: u32) -> Option<String> {
        self.symbol(mem, word(mem, cp + self.cp_size + 8 * u64::from(i))?)
    }

    /// JDK 21+: a class's own fields from its field info stream, and whether the stream ended
    /// where it should.
    fn decode_stream(&self, mem: &File, k: u64) -> Option<(Vec<Field>, bool)> {
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
            out.push(Field { name: self.cp_symbol(mem, cp, name)?, sig: self.cp_symbol(mem, cp, sig)?, offset, is_static: access & 8 != 0 });
        }
        // The rest: injected fields, then a zero byte (and, in later JDKs, nothing else).
        let clean = b.get(i..).is_some_and(|rest| rest.len() <= 1 + 8 * 4) && out.iter().all(|f| f.sig.len() >= 1 && "BCDFIJSZL[".contains(&f.sig[..1]));
        Some((out, clean))
    }

    /// JDK 8-20: a class's own fields from its `_fields` array.
    fn decode_old(&self, mem: &File, k: u64) -> Option<Vec<Field>> {
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
            });
        }
        Some(out)
    }

    /// A class's own fields (cached: a class's fields don't change).
    fn own_fields(&self, mem: &File, k: u64) -> Arc<Vec<Field>> {
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
    fn instance_fields(&self, mem: &File, mut k: u64) -> Vec<Field> {
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

    /// Whether a class is an enum (its constants are objects too, never the game's state).
    fn is_enum(&self, mem: &File, mut k: u64) -> bool {
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
    fn mirror(&self, mem: &File, k: u64) -> Option<u64> {
        let m = word(mem, k + self.klass_mirror)?;
        let m = if self.mirror_handle { word(mem, m)? } else { m };
        (m != 0).then_some(m)
    }

    /// The object a reference slot points to (0 for null).
    fn reference(&self, mem: &File, at: u64) -> Option<u64> {
        match self.coops {
            true => half(mem, at).map(|n| if n == 0 { 0 } else { self.oop_base + (u64::from(n) << self.oop_shift) }),
            false => word(mem, at),
        }
    }

    /// An object's class, if the object is sound: in the heap, not being moved by the collector,
    /// of a class the JVM has loaded.
    fn klass_of(&self, heap: &Heap, o: u64) -> Option<u64> {
        let mem = heap.file();
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
    fn val(&self, mem: &File, at: u64, sig: &str) -> Val {
        match sig.as_bytes().first() {
            Some(b'I') => Val::Num(at, "i32"),
            Some(b'F') => Val::Num(at, "f32"),
            Some(b'D') => Val::Num(at, "f64"),
            Some(b'S' | b'C') => Val::Num(at, "u16"),
            Some(b'L' | b'[') => self.reference(mem, at).map_or(Val::Other, Val::Obj),
            _ => Val::Other,
        }
    }

    /// An array's elements (up to `MAX_ELEMENTS`): index and what it holds.
    fn elements(&self, heap: &Heap, a: u64, name: &str) -> Vec<(u32, Val)> {
        let mem = heap.file();
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
        (0..len.min(MAX_ELEMENTS)).map(|i| (i, self.val(mem, a + base + size * u64::from(i), elem_sig))).collect()
    }
}

/// The located JVM of the game attached to, per pid; a failed search is tried again after
/// `RETRY`.
pub fn shared(pid: u32, mem: &File) -> Option<Arc<Jvm>> {
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

/// One step of a path: a field by name, or an array's element.
#[derive(Clone, Debug, PartialEq)]
enum Step {
    Field(String),
    Index(u32),
}

/// A saved path from a class's static field to a number.
#[derive(Clone, Debug, PartialEq)]
pub struct JavaPath {
    /// The class, with '/' (as the JVM names it).
    class: String,
    steps: Vec<Step>,
}

impl JavaPath {
    /// "java:<class with dots>#<static>[.<field>|[i]]...".
    pub fn parse(text: &str) -> Option<JavaPath> {
        let (class, rest) = text.strip_prefix("java:")?.split_once('#')?;
        if class.is_empty() || class.contains(['/', ' ']) {
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
        Some(JavaPath { class: class.replace('.', "/"), steps })
    }

    pub fn text(&self) -> String {
        format!("java:{}#{}", self.class.replace('/', "."), self.fields_text())
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

    /// As the list shows it: the class's short name and the fields (`Dungeon.hero.HP`).
    fn shown(&self) -> String {
        match self.steps.is_empty() {
            true => short_name(&self.class).to_owned(),
            false => format!("{}.{}", short_name(&self.class), self.fields_text()),
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
        Some(JavaPath { class: self.class.clone(), steps: self.steps[..=i].to_vec() })
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
    let Some(jvm) = shared(heap.pid, heap.file()) else { return Vec::new() };
    jvm.class(heap.file(), &path.class).into_iter().collect()
}

/// Where the path leads from its class now: the number's address, every object along it
/// checked (see the module's notes).
pub fn walk(heap: &Heap, roots: &[u64], path: &JavaPath) -> Vec<u64> {
    let Some(jvm) = shared(heap.pid, heap.file()) else { return Vec::new() };
    roots.iter().filter_map(|&k| follow(&jvm, heap, k, path)).collect()
}

fn follow(jvm: &Jvm, heap: &Heap, k: u64, path: &JavaPath) -> Option<u64> {
    let mem = heap.file();
    if jvm.klass_name(mem, k)? != path.class {
        return None;
    }
    let mirror = jvm.mirror(mem, k)?;
    let Some(Step::Field(first)) = path.steps.first() else { return None };
    let f = jvm.own_fields(mem, k).iter().find(|f| f.is_static && f.name == *first)?.clone();
    let mut cur = jvm.val(mem, mirror + u64::from(f.offset), &f.sig);
    for s in &path.steps[1..] {
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
    let jvm = shared(heap.pid, heap.file())?;
    let mem = heap.file();
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
        let root = JavaPath { class: name.clone(), steps: Vec::new() };
        let mut g = Group { path: root.clone(), class: name.clone(), statics: true, numbers: Vec::new() };
        // `$SwitchMap$...` and other fields the compiler made.
        for f in statics.into_iter().filter(|f| !f.name.starts_with('$')) {
            let p = root.then(Step::Field(f.name.clone()));
            match jvm.val(mem, mirror + u64::from(f.offset), &f.sig) {
                Val::Num(at, kind) => g.numbers.push((f.name.clone(), at, kind, p)),
                Val::Obj(o) if o != 0 => queues[queue_of(&p)].push_back((o, p, 1)),
                _ => {}
            }
        }
        if !g.numbers.is_empty() {
            out.push(g);
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
        if class.starts_with('[') {
            for (i, v) in jvm.elements(heap, o, &class) {
                match v {
                    Val::Num(at, kind) => g.numbers.push((format!("[{i}]"), at, kind, path.then(Step::Index(i)))),
                    Val::Obj(x) if x != 0 => refs.push((Step::Index(i), x)),
                    _ => {}
                }
            }
        } else if let Some(items) = container(&class) {
            let f = jvm.instance_fields(mem, k).into_iter().find(|f| f.name == items);
            if let Some(Val::Obj(x)) = f.map(|f| jvm.val(mem, o + u64::from(f.offset), &f.sig)) {
                refs.push((Step::Field(items.to_owned()), x));
            }
        } else if !is_library(&class) {
            for f in jvm.instance_fields(mem, k) {
                match jvm.val(mem, o + u64::from(f.offset), &f.sig) {
                    Val::Num(at, kind) => g.numbers.push((f.name.clone(), at, kind, path.then(Step::Field(f.name.clone())))),
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

/// The path to a number the list has, checked by following it the way a restart does.
pub fn discover(heap: &Heap, target: u64) -> Option<(JavaPath, Vec<u64>)> {
    let path = groups(heap)?.into_iter().flat_map(|g| g.numbers).find(|n| n.1 == target)?.3;
    let leads = walk(heap, &find_roots(heap, &path), &path);
    leads.contains(&target).then_some((path, leads))
}

/// jvm [class]: what was found (the JVM, how it packs references, the field format), or a
/// class's fields with their values.
pub fn diagnose(heap: &Heap, class: &str) -> Vec<String> {
    let t = Instant::now();
    let mem = heap.file();
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
        for text in ["java:com.shatteredpixel.shatteredpixeldungeon.Dungeon#gold", "java:a.b.Game#hero.HP", "java:Game$Inner#items.elementData[2].quantity", "java:G#grid[1][2]"] {
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
