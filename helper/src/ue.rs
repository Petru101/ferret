// Unreal Engine 4.25+ and 5 (64-bit): the engine keeps a list of every object (GUObjectArray)
// with its name (an FName: an index into the name pool, FNamePool) and its class, and every
// class describes its properties (name, type, offset). Modding tools read the game this way;
// Ferret uses it to name a value ("Amount of the element of Items whose Item is DA_Carbon, in
// the InventoryComponent of BP_Player_C") so it can be found again after a restart, where
// pointer paths from one run don't hold (Astro Colony: every chain from the game's own static
// data is longer than a pointer scan goes).
// Layouts (64-bit, 4.25 to 5.x shipping builds):
// - FNamePool: { lock 8, u32 CurrentBlock, u32 CurrentByteCursor, u8 *Blocks[8192] }; an
//   FName's index = block << 16 | offset / 2; an entry = u16 header (bit 0 wide, bits 6-15
//   length) + the characters (Latin-1, or UTF-16 when wide).
// - FChunkedFixedUObjectArray: { FUObjectItem **Objects, *PreAllocated, i32 MaxElements,
//   NumElements, MaxChunks, NumChunks }, chunks of 65536 items { UObject *, i32 flags, cluster,
//   serial } (24 bytes).
// - UObject: { vtable, i32 flags, i32 InternalIndex, UClass *Class @0x10, FName Name @0x18,
//   UObject *Outer @0x20 }.
// - UStruct: SuperStruct @0x40, ChildProperties (FField *) @0x50, PropertiesSize @0x58.
// - FField: FFieldClass *Class @0x8 (its FName first), Next @0x20, FName Name @0x28;
//   FProperty: ArrayDim @0x38, ElementSize @0x3C, Offset @0x4C, and @0x78 the struct of a
//   StructProperty, the inner property of an ArrayProperty, the class of an ObjectProperty, the
//   key property of a MapProperty (value property @0x80).
//   From UE 5.4 FField's Owner (FFieldVariant) is one tagged pointer instead of a pointer and a
//   bool, so everything after it is 8 bytes earlier (Starship Troopers: Ultimate Bug War!, 5.6).
//   Minecraft Dungeons II (a 5.x with Utf8Str/AnsiStr properties) has the 5.4 FField, but its
//   Offset is at 0x48, its type words at 0x78 (struct, class, map key) and 0x80 (map value),
//   and an ArrayProperty's inner property at 0x80 (its flags first): `LAYOUTS`.
// Older engines (4.11 to 4.24) keep properties as UObjects (UProperty) in a struct's Children
// (UField.Next @0x28, with functions and enums: only "...Property" classes count), their type
// being their class's name; 4.22 and older keep names in TNameEntryArray (heap: the exe holds a
// pointer to it; chunks of 16384 FNameEntry pointers; an entry holds its index shifted left by
// one, bit 0 wide, then the NUL-terminated characters) and 4.20 and older objects in one flat
// array. Offsets move between minor versions (UStruct has FStructBaseChain from 4.21 or so,
// UProperty's Offset_Internal moved): every known form is tried. Epic's source, 4.13 and 4.22.
// The layout is checked on the engine's own structs: Guid (A, B, C, D: four ints), Transform
// (Quat, Vector, Vector) and Actor's Tags (an array of names), in each known form.

use std::collections::HashMap;
use crate::mem::Mem;
use std::sync::{Arc, Mutex};

use crate::helper::{maps, Region};
use crate::pointers::Module;

const CHUNK_ITEMS: u64 = 65536;
const ITEM: u64 = 24;
const OBJ_CLASS: u64 = 0x10;
const OBJ_NAME: u64 = 0x18;
const OBJ_OUTER: u64 = 0x20;
/// Names per chunk of TNameEntryArray.
const NAME_CHUNK: u64 = 16384;

/// Where a UStruct keeps its parent, properties, size and alignment, and an FField (or
/// UProperty) its fields.
#[derive(Clone, Copy, Debug)]
struct Layout {
    sup: u64,
    /// ChildProperties (FField), or Children (UField: properties among functions and enums).
    props: u64,
    struct_size: u64,
    align: u64,
    /// The field's class, and where that class keeps its name ("IntProperty").
    class: u64,
    class_name: u64,
    name: u64,
    next: u64,
    dim: u64,
    size: u64,
    offset: u64,
    /// A StructProperty's struct, an ObjectProperty's class, a MapProperty's key property.
    extra: u64,
    /// An ArrayProperty's (or SetProperty's) element property.
    inner: u64,
    /// A MapProperty's value property.
    value: u64,
}

/// Tried in this order at attach (`check_layout` picks the first that reads the engine's own
/// structs right).
const FFIELD: Layout = Layout {
    sup: 0x40,
    props: 0x50,
    struct_size: 0x58,
    align: 0x5C,
    class: 0x8,
    class_name: 0,
    name: 0x28,
    next: 0x20,
    dim: 0x38,
    size: 0x3C,
    offset: 0x4C,
    extra: 0x78,
    inner: 0x78,
    value: 0x80,
};
/// 4.22: UStruct { UField (Next @0x28), FStructBaseChain, SuperStruct @0x40, Children @0x48,
/// PropertiesSize @0x50, MinAlignment @0x54 }; UProperty { UField, ArrayDim @0x30, ElementSize
/// @0x34, Offset_Internal @0x44, ..., 0x70 bytes }, then Struct/PropertyClass/Inner/KeyProp,
/// ValueProp.
const UPROPERTY: Layout = Layout {
    sup: 0x40,
    props: 0x48,
    struct_size: 0x50,
    align: 0x54,
    class: OBJ_CLASS,
    class_name: OBJ_NAME,
    name: OBJ_NAME,
    next: 0x28,
    dim: 0x30,
    size: 0x34,
    offset: 0x44,
    extra: 0x70,
    inner: 0x70,
    value: 0x78,
};
/// Styx: Shards of Darkness (4.13.1): no FStructBaseChain, and UProperty's RepNotifyFunc @0x48,
/// Offset_Internal @0x50, 0x78 bytes in all (Epic's 4.13.2 source would put the offset at 0x4C).
const UPROPERTY_OLD: Layout = Layout { sup: 0x30, props: 0x38, struct_size: 0x40, align: 0x44, offset: 0x50, extra: 0x78, inner: 0x78, value: 0x80, ..UPROPERTY };

const LAYOUTS: [Layout; 7] = [
    // 4.25 to 5.3.
    FFIELD,
    // 5.4 and later: FField's Owner is one tagged pointer, everything after it 8 bytes earlier.
    Layout { name: 0x20, next: 0x18, dim: 0x30, size: 0x34, offset: 0x44, extra: 0x70, inner: 0x70, value: 0x78, ..FFIELD },
    // Minecraft Dungeons II.
    Layout { name: 0x20, next: 0x18, dim: 0x30, size: 0x34, offset: 0x48, extra: 0x78, inner: 0x80, value: 0x80, ..FFIELD },
    // UProperty games (4.11 to 4.24), the struct and property halves of 4.22 and Styx's 4.13 in
    // each combination: which version moved which isn't known.
    UPROPERTY,
    UPROPERTY_OLD,
    Layout { offset: 0x50, extra: 0x78, inner: 0x78, value: 0x80, ..UPROPERTY },
    Layout { offset: 0x44, extra: 0x70, inner: 0x70, value: 0x78, ..UPROPERTY_OLD },
];
impl Layout {
    /// The same in a build with stats compiled in (development builds: Injected, a 4.15 jam
    /// game's NGJ17.exe): UObjectBase gets a TStatId after Outer, so everything a UObject keeps
    /// after its header is 8 bytes later (UStruct's fields; all of a UProperty's).
    const fn with_stats(self) -> Layout {
        let prop = if self.class == OBJ_CLASS { 8 } else { 0 };
        Layout {
            sup: self.sup + 8,
            props: self.props + 8,
            struct_size: self.struct_size + 8,
            align: self.align + 8,
            next: self.next + prop,
            dim: self.dim + prop,
            size: self.size + prop,
            offset: self.offset + prop,
            extra: self.extra + prop,
            inner: self.inner + prop,
            value: self.value + prop,
            ..self
        }
    }
}

/// Properties per struct followed (a misread link can't make a walk take forever).
const MAX_PROPS: usize = 2000;
const MAX_NAME: usize = 1024;

fn u32_at(mem: &Mem, at: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    mem.read_exact_at(&mut b, at).ok()?;
    Some(u32::from_le_bytes(b))
}

fn i32_at(mem: &Mem, at: u64) -> Option<i32> {
    u32_at(mem, at).map(|v| v as i32)
}

fn u64_at(mem: &Mem, at: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b, at).ok()?;
    Some(u64::from_le_bytes(b))
}

fn ptr_at(mem: &Mem, at: u64) -> Option<u64> {
    u64_at(mem, at).filter(|&p| p >= 0x10000 && p < 1 << 47 && p % 8 == 0)
}

/// A property of a class or struct.
#[derive(Clone, Debug)]
pub struct Prop {
    pub name: String,
    /// Its type: "IntProperty", "StructProperty", "ArrayProperty", ...
    pub kind: String,
    pub offset: u64,
    pub size: u64,
    pub dim: u64,
    /// StructProperty: the struct; ArrayProperty: the inner property (an FField); ObjectProperty:
    /// the class; MapProperty: the key property.
    pub extra: u64,
    /// MapProperty: the value property.
    pub extra2: u64,
    /// ArrayProperty and SetProperty: the elements' property; MapProperty: the keys'.
    pub inner: Option<Arc<Prop>>,
    /// MapProperty: the values' property.
    pub value: Option<Arc<Prop>>,
}

/// Where the game keeps its names.
#[derive(Clone, Copy, Debug)]
enum Names {
    /// FNamePool's blocks (4.23 and later).
    Pool(u64),
    /// TNameEntryArray (4.22 and older): its chunk table, and where an entry keeps its index
    /// and its characters.
    Array { table: u64, index: u64, chars: u64 },
}

/// Where the game keeps its objects and names (found once per attach).
pub struct Ue {
    objects: u64,
    /// The objects in one array (FFixedUObjectArray, 4.20 and older), not in chunks.
    flat: bool,
    names_at: Names,
    /// Where its structs and properties keep their fields (one of `LAYOUTS`).
    layout: Layout,
    names: Mutex<HashMap<u32, Option<String>>>,
    props: Mutex<HashMap<u64, Arc<Vec<Prop>>>>,
    containers: Mutex<HashMap<u64, bool>>,
}

/// The game's own program and the rw memory in it (static data).
fn module_data(pid: u32, mods: &[Module], exe: &str) -> Vec<(u64, u64)> {
    let Some(m) = mods.iter().find(|m| m.name.eq_ignore_ascii_case(exe)) else { return Vec::new() };
    let regions: Vec<Region> = maps(pid).unwrap_or_default();
    regions.iter().filter(|r| r.perms.starts_with("rw") && r.start >= m.start && r.end <= m.end).map(|r| (r.start, r.end)).collect()
}

impl Ue {
    /// Finds the object array and the name pool in the program's static data.
    pub fn locate(pid: u32, mem: &Mem, mods: &[Module], exe: &str) -> Result<Ue, String> {
        let data = module_data(pid, mods, exe);
        if data.is_empty() {
            return Err(format!("no writable data in {exe}"));
        }
        let (mut objects, mut flat, mut names) = (None, None, None);
        let mut words_of = Vec::new();
        for &(start, end) in &data {
            let mut buf = vec![0u8; (end - start) as usize];
            let Ok(n) = mem.read_at(&mut buf, start) else { continue };
            let words: Vec<u64> = buf[..n].chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
            for i in 0..words.len().saturating_sub(4) {
                let at = start + 8 * i as u64;
                if objects.is_none() && is_object_array(mem, &words[i..]) {
                    objects = Some(at);
                }
                if objects.is_none() && flat.is_none() && is_flat_object_array(mem, &words[i..]) {
                    flat = Some(at);
                }
                if names.is_none() && i >= 1 && is_name_blocks(mem, &words[i - 1..]) {
                    names = Some(Names::Pool(at));
                }
            }
            words_of.push((start, words));
        }
        // TNameEntryArray is on the heap: a pointer to it among the static data. Looked for only
        // when there's no name pool (one more read for every heap pointer there).
        if names.is_none() {
            let (lo, hi) = data.iter().fold((u64::MAX, 0), |(lo, hi), &(s, e)| (lo.min(s), hi.max(e)));
            names = words_of.iter().flat_map(|(_, w)| w.iter().copied()).filter(|&p| (p < lo || p >= hi) && ptr_at_ok(p)).find_map(|p| name_array(mem, p));
        }
        let (objects, flat) = match (objects, flat) {
            (Some(o), _) => (o, false),
            (None, Some(o)) => (o, true),
            (None, None) => return Err("no Unreal object list found".into()),
        };
        let names_at = names.ok_or("no Unreal name pool found")?;
        let mut ue = Ue { objects, flat, names_at, layout: LAYOUTS[0], names: Mutex::default(), props: Mutex::default(), containers: Mutex::default() };
        // The first layout that reads all three right; else the first that reads Guid right (all
        // a layout had to pass before Transform and Tags were checked: no game that worked
        // then loses it).
        // The error to tell: the layout that read the most of Guid.
        let (mut why, mut fallback) = ((0, String::new()), None);
        for layout in LAYOUTS.iter().flat_map(|l| [*l, l.with_stats()]) {
            ue.layout = layout;
            ue.props.lock().unwrap().clear();
            match ue.check_layout(mem) {
                Ok(true) => return Ok(ue),
                Ok(false) => fallback = fallback.or(Some(layout)),
                Err(e) if why.1.is_empty() || e.0 > why.0 => why = e,
                Err(_) => {}
            }
        }
        match fallback {
            Some(layout) => {
                ue.layout = layout;
                ue.props.lock().unwrap().clear();
                Ok(ue)
            }
            None => Err(why.1),
        }
    }

    /// The engine's Guid struct must read as four ints at 0, 4, 8 and 12 (else an error).
    /// Then whether the types are where the layout says: Transform's fields must lead to the
    /// Quat and Vector structs, and Actor's Tags to an array of names (checked when the game
    /// has them).
    fn check_layout(&self, mem: &Mem) -> Result<bool, (usize, String)> {
        let objects = self.objects(mem);
        let find = |name: &str, class: &str| {
            objects.iter().copied().find(|&o| self.object_name(mem, o).as_deref() == Some(name) && self.class_name(mem, o).as_deref() == Some(class))
        };
        let guid = find("Guid", "ScriptStruct").ok_or((0, "no Guid struct among the objects (another engine version?)".to_string()))?;
        let props = self.props(mem, guid);
        let got: Vec<(String, u64)> = props.iter().map(|p| (p.name.clone(), p.offset)).collect();
        let want: Vec<(String, u64)> = ["A", "B", "C", "D"].iter().zip([0, 4, 8, 12]).map(|(n, o)| (n.to_string(), o)).collect();
        if got != want {
            return Err((got.len(), format!("unknown property layout (Guid at 0x{guid:x} reads as {got:?})")));
        }
        if let Some(t) = find("Transform", "ScriptStruct") {
            let got: Vec<Option<String>> = self.props(mem, t).iter().map(|p| self.object_name(mem, p.extra)).collect();
            if got != [Some("Quat".to_string()), Some("Vector".into()), Some("Vector".into())] {
                return Ok(false);
            }
        }
        if let Some(a) = find("Actor", "Class") {
            let tags = self.props(mem, a).iter().find(|p| p.name == "Tags").cloned();
            let inner = tags.as_ref().and_then(|p| p.inner.as_ref()).map(|i| i.kind.clone());
            if tags.is_some() && inner.as_deref() != Some("NameProperty") {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn object_count(&self, mem: &Mem) -> usize {
        i32_at(mem, self.objects + if self.flat { 12 } else { 20 }).unwrap_or(0).max(0) as usize
    }

    /// Every object alive now.
    pub fn objects(&self, mem: &Mem) -> Vec<u64> {
        let num = self.object_count(mem) as u64;
        let Some(table) = ptr_at(mem, self.objects) else { return Vec::new() };
        let mut out = Vec::with_capacity(num as usize);
        if self.flat {
            let mut buf = vec![0u8; (num * ITEM) as usize];
            if let Ok(n) = mem.read_at(&mut buf, table) {
                out.extend(buf[..n].chunks_exact(ITEM as usize).map(|i| u64::from_le_bytes(i[..8].try_into().unwrap())).filter(|&p| p != 0));
            }
            return out;
        }
        for c in 0..num.div_ceil(CHUNK_ITEMS) {
            let Some(chunk) = ptr_at(mem, table + 8 * c) else { continue };
            let n = (num - c * CHUNK_ITEMS).min(CHUNK_ITEMS);
            let mut buf = vec![0u8; (n * ITEM) as usize];
            if mem.read_exact_at(&mut buf, chunk).is_err() {
                continue;
            }
            out.extend(buf.chunks_exact(ITEM as usize).map(|i| u64::from_le_bytes(i[..8].try_into().unwrap())).filter(|&p| p != 0));
        }
        out
    }

    /// The text of name pool entry `index`.
    pub fn name(&self, mem: &Mem, index: u32) -> Option<String> {
        if let Some(n) = self.names.lock().unwrap().get(&index) {
            return n.clone();
        }
        let n = self.read_name(mem, index);
        self.names.lock().unwrap().insert(index, n.clone());
        n
    }

    fn read_name(&self, mem: &Mem, index: u32) -> Option<String> {
        let blocks = match self.names_at {
            Names::Pool(blocks) => blocks,
            Names::Array { table, index: at, chars } => {
                let chunk = ptr_at(mem, table + 8 * (index as u64 / NAME_CHUNK))?;
                let entry = ptr_at(mem, chunk + 8 * (index as u64 % NAME_CHUNK))?;
                let wide = u32_at(mem, entry + at)? & 1 == 1;
                return entry_text(mem, entry + chars, wide);
            }
        };
        let block = ptr_at(mem, blocks + 8 * (index >> 16) as u64)?;
        let at = block + 2 * (index & 0xFFFF) as u64;
        let mut h = [0u8; 2];
        mem.read_exact_at(&mut h, at).ok()?;
        let header = u16::from_le_bytes(h);
        let len = (header >> 6) as usize;
        if len == 0 || len > MAX_NAME {
            return None;
        }
        if header & 1 == 1 {
            let mut b = vec![0u8; 2 * len];
            mem.read_exact_at(&mut b, at + 2).ok()?;
            let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            Some(String::from_utf16_lossy(&units))
        } else {
            let mut b = vec![0u8; len];
            mem.read_exact_at(&mut b, at + 2).ok()?;
            Some(b.iter().map(|&c| c as char).collect())
        }
    }

    /// The FName at `at` as text ("Item_3" for number 4: numbers are stored plus one).
    pub fn fname(&self, mem: &Mem, at: u64) -> Option<String> {
        let index = u32_at(mem, at)?;
        let number = u32_at(mem, at + 4)?;
        let base = self.name(mem, index)?;
        Some(if number == 0 { base } else { format!("{base}_{}", number - 1) })
    }

    pub fn object_name(&self, mem: &Mem, obj: u64) -> Option<String> {
        self.fname(mem, obj + OBJ_NAME)
    }

    pub fn class_of(&self, mem: &Mem, obj: u64) -> Option<u64> {
        ptr_at(mem, obj + OBJ_CLASS)
    }

    pub fn class_name(&self, mem: &Mem, obj: u64) -> Option<String> {
        self.object_name(mem, self.class_of(mem, obj)?)
    }

    pub fn outer(&self, mem: &Mem, obj: u64) -> Option<u64> {
        ptr_at(mem, obj + OBJ_OUTER)
    }

    pub fn struct_size(&self, mem: &Mem, st: u64) -> u64 {
        i32_at(mem, st + self.layout.struct_size).unwrap_or(0).max(0) as u64
    }

    /// The struct or class `st` derives from.
    pub fn super_of(&self, mem: &Mem, st: u64) -> Option<u64> {
        ptr_at(mem, st + self.layout.sup)
    }

    /// The properties of a class or struct, its parents' first.
    pub fn props(&self, mem: &Mem, st: u64) -> Arc<Vec<Prop>> {
        if let Some(p) = self.props.lock().unwrap().get(&st) {
            return p.clone();
        }
        let mut chain = Vec::new();
        let mut cur = Some(st);
        while let Some(s) = cur.filter(|_| chain.len() < 64) {
            chain.push(s);
            cur = self.super_of(mem, s);
        }
        let mut out = Vec::new();
        for s in chain.into_iter().rev() {
            let mut f = ptr_at(mem, s + self.layout.props);
            let mut n = 0;
            while let Some(field) = f.filter(|_| n < MAX_PROPS) {
                n += 1;
                if let Some(mut p) = self.prop(mem, field).filter(|p| p.kind.ends_with("Property")) {
                    if matches!(p.kind.as_str(), "ArrayProperty" | "SetProperty" | "MapProperty") {
                        p.inner = self.prop(mem, p.extra).map(Arc::new);
                    }
                    if p.kind == "MapProperty" {
                        p.value = self.prop(mem, p.extra2).map(Arc::new);
                    }
                    out.push(p);
                }
                f = ptr_at(mem, field + self.layout.next);
            }
        }
        let out = Arc::new(out);
        self.props.lock().unwrap().insert(st, out.clone());
        out
    }

    /// The property described by the FField at `field`.
    pub fn prop(&self, mem: &Mem, field: u64) -> Option<Prop> {
        let l = self.layout;
        let kind = self.fname(mem, ptr_at(mem, field + l.class)? + l.class_name)?;
        let extra = if matches!(kind.as_str(), "ArrayProperty" | "SetProperty") { l.inner } else { l.extra };
        Some(Prop {
            name: self.fname(mem, field + l.name)?,
            offset: i32_at(mem, field + l.offset)?.max(0) as u64,
            size: i32_at(mem, field + l.size)?.max(0) as u64,
            dim: i32_at(mem, field + l.dim)?.max(1) as u64,
            extra: u64_at(mem, field + extra).unwrap_or(0),
            extra2: u64_at(mem, field + l.value).unwrap_or(0),
            kind,
            inner: None,
            value: None,
        })
    }

    /// The object's full name: outers first ("Level.BP_Player_C_0.Inventory").
    pub fn full_name(&self, mem: &Mem, obj: u64) -> String {
        let mut parts = Vec::new();
        let mut cur = Some(obj);
        while let Some(o) = cur.filter(|_| parts.len() < 16) {
            parts.push(self.object_name(mem, o).unwrap_or_else(|| "?".into()));
            cur = self.outer(mem, o);
        }
        parts.reverse();
        parts.join(".")
    }
}

/// The object array's header: counts that fit together, and its first object has index 0.
fn is_object_array(mem: &Mem, w: &[u64]) -> bool {
    let (max, num) = (w[2] as u32 as u64, w[2] >> 32);
    let (max_chunks, num_chunks) = (w[3] as u32 as u64, w[3] >> 32);
    if num == 0 || num > max || max > 1 << 26 || num_chunks == 0 {
        return false;
    }
    if max_chunks != max.div_ceil(CHUNK_ITEMS) || num_chunks != num.div_ceil(CHUNK_ITEMS) {
        return false;
    }
    let first = ptr_at(mem, w[0]).and_then(|chunk| Some((ptr_at(mem, chunk)?, ptr_at(mem, chunk + ITEM)?)));
    let Some((a, b)) = first else { return false };
    u32_at(mem, a + 0xC) == Some(0) && u32_at(mem, b + 0xC) == Some(1)
}

/// FFixedUObjectArray (4.20 and older): { items, i32 max, i32 num }, its first two objects
/// with indices 0 and 1.
fn is_flat_object_array(mem: &Mem, w: &[u64]) -> bool {
    let (max, num) = (w[1] as u32 as u64, w[1] >> 32);
    if num < 2 || num > max || max > 1 << 26 {
        return false;
    }
    let first = Some(w[0]).filter(|&p| ptr_at_ok(p)).and_then(|items| Some((ptr_at(mem, items)?, ptr_at(mem, items + ITEM)?)));
    let Some((a, b)) = first else { return false };
    u32_at(mem, a + 0xC) == Some(0) && u32_at(mem, b + 0xC) == Some(1)
}

/// Whether `p` can be a pointer (`ptr_at`'s rule), before reading anything.
fn ptr_at_ok(p: u64) -> bool {
    p >= 0x10000 && p < 1 << 47 && p % 8 == 0
}

/// TNameEntryArray at `p` (4.22 and older): { FNameEntry **Chunks[N]; i32 NumElements, NumChunks }
/// with N = 128 (4.13: 2M names) or 256 (4.22: 4M), chunks as many as the count needs, and its
/// first entries "None" and "ByteProperty", in either entry layout: { HashNext, Index, chars }
/// (4.22) or { Index, HashNext, chars } (4.13).
fn name_array(mem: &Mem, p: u64) -> Option<Names> {
    let chunk = ptr_at(mem, p)?;
    for slots in [128u64, 256] {
        let (Some(num), Some(chunks)) = (i32_at(mem, p + 8 * slots), i32_at(mem, p + 8 * slots + 4)) else { continue };
        let chunks = chunks as u64;
        if num < 2 || chunks == 0 || chunks > slots || chunks != (num as u64).div_ceil(NAME_CHUNK) {
            continue;
        }
        if ptr_at(mem, p + 8 * (chunks - 1)).is_none() || (chunks < slots && u64_at(mem, p + 8 * chunks) != Some(0)) {
            continue;
        }
        let (Some(none), Some(byte)) = (ptr_at(mem, chunk), ptr_at(mem, chunk + 8)) else { continue };
        for (index, chars) in [(8, 0xC), (0, 0x10)] {
            let indices = (u32_at(mem, none + index).map(|i| i >> 1), u32_at(mem, byte + index).map(|i| i >> 1));
            if indices == (Some(0), Some(1))
                && entry_text(mem, none + chars, false).as_deref() == Some("None")
                && entry_text(mem, byte + chars, false).as_deref() == Some("ByteProperty")
            {
                return Some(Names::Array { table: p, index, chars });
            }
        }
    }
    None
}

/// A TNameEntryArray entry's NUL-terminated characters (Latin-1, or UTF-16 when wide).
fn entry_text(mem: &Mem, at: u64, wide: bool) -> Option<String> {
    let unit = if wide { 2 } else { 1 };
    let mut b = vec![0u8; unit * MAX_NAME];
    let n = mem.read_at(&mut b, at).ok()?;
    let b = &b[..n - n % unit];
    let text = if wide {
        let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let len = units.iter().position(|&u| u == 0)?;
        String::from_utf16_lossy(&units[..len])
    } else {
        let len = b.iter().position(|&c| c == 0)?;
        b[..len].iter().map(|&c| c as char).collect()
    };
    Some(text).filter(|t| !t.is_empty())
}

/// The name pool's blocks (`w[1..]`, after CurrentBlock and CurrentByteCursor in `w[0]`): as
/// many as the current block says, and the first starts with "None" then "ByteProperty".
fn is_name_blocks(mem: &Mem, w: &[u64]) -> bool {
    let (current, cursor) = (w[0] as u32 as usize, w[0] >> 32);
    if current >= 8192 || cursor == 0 || cursor > 0x20000 || w.len() < current + 3 {
        return false;
    }
    if w[1..=current + 1].iter().any(|&b| b == 0) || w[current + 2] != 0 {
        return false;
    }
    let mut b = [0u8; 20];
    if w[1] % 8 != 0 || mem.read_exact_at(&mut b, w[1]).is_err() {
        return false;
    }
    &b[2..6] == b"None" && &b[8..20] == b"ByteProperty"
}

impl Ue {
    /// A property's value as text, for looking at objects (`ue dump`).
    fn value_text(&self, mem: &Mem, at: u64, p: &Prop) -> String {
        let int = |n: usize| {
            let mut b = [0u8; 8];
            mem.read_exact_at(&mut b[..n], at).ok().map(|_| i64::from_le_bytes(b))
        };
        let shown = match p.kind.as_str() {
            "IntProperty" | "UInt32Property" => int(4).map(|v| (v as i32).to_string()),
            "Int64Property" | "UInt64Property" => int(8).map(|v| v.to_string()),
            "Int16Property" | "UInt16Property" => int(2).map(|v| (v as i16).to_string()),
            "ByteProperty" | "Int8Property" | "BoolProperty" | "EnumProperty" if p.size <= 8 => int(p.size as usize).map(|v| v.to_string()),
            "FloatProperty" => u32_at(mem, at).map(|v| f32::from_bits(v).to_string()),
            "DoubleProperty" => u64_at(mem, at).map(|v| f64::from_bits(v).to_string()),
            "NameProperty" => self.fname(mem, at),
            "StrProperty" => self.fstring(mem, at).map(|s| format!("{s:?}")),
            "ObjectProperty" | "ClassProperty" => {
                u64_at(mem, at).map(|o| if o == 0 { "null".into() } else { format!("0x{o:x} {}", self.object_name(mem, o).unwrap_or_default()) })
            }
            "StructProperty" => self.object_name(mem, p.extra).map(|s| format!("<{s}>")),
            "ArrayProperty" | "MapProperty" | "SetProperty" => {
                let data = u64_at(mem, at).unwrap_or(0);
                let num = i32_at(mem, at + 8).unwrap_or(0);
                Some(format!("0x{data:x} [{num}]"))
            }
            _ => None,
        };
        shown.unwrap_or_else(|| "?".into())
    }

    /// An FString's text (TArray of UTF-16 units, the last one 0).
    pub fn fstring(&self, mem: &Mem, at: u64) -> Option<String> {
        let data = u64_at(mem, at)?;
        let num = i32_at(mem, at + 8)?;
        if data == 0 || num <= 0 {
            return Some(String::new());
        }
        let len = (num as usize).min(MAX_NAME);
        let mut b = vec![0u8; 2 * len];
        mem.read_exact_at(&mut b, data).ok()?;
        let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        Some(String::from_utf16_lossy(&units))
    }

    /// ue objects <text> | ue props <hex struct> | ue class <name> | ue dump <hex obj> [hex struct]:
    /// looking around a game's objects while working out where it keeps a value.
    pub fn command(&self, out: &mut impl std::io::Write, mem: &Mem, arg: &str) -> std::io::Result<()> {
        let (sub, rest) = arg.split_once(' ').unwrap_or((arg, ""));
        let hex = |s: &str| u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
        match sub {
            "objects" => {
                let want = rest.to_ascii_lowercase();
                let mut shown = 0;
                for o in self.objects(mem) {
                    let class = self.class_name(mem, o).unwrap_or_default();
                    let name = self.object_name(mem, o).unwrap_or_default();
                    if (class.to_ascii_lowercase().contains(&want) || name.to_ascii_lowercase().contains(&want)) && shown < 300 {
                        writeln!(out, "0x{o:x} {class} {}", self.full_name(mem, o))?;
                        shown += 1;
                    }
                }
                writeln!(out, "{shown} shown")
            }
            "class" => {
                for o in self.objects(mem) {
                    if self.object_name(mem, o).as_deref() == Some(rest.trim()) {
                        let class = self.class_name(mem, o).unwrap_or_default();
                        if class.ends_with("Class") || class.ends_with("Struct") {
                            writeln!(out, "0x{o:x} {class} size 0x{:x}", self.struct_size(mem, o))?;
                            for p in self.props(mem, o).iter() {
                                writeln!(out, "  +0x{:x} {} {} (size {}{})", p.offset, p.kind, p.name, p.size, self.extra_text(mem, &p))?;
                            }
                        }
                    }
                }
                Ok(())
            }
            "dump" => {
                let mut w = rest.split_whitespace();
                let Some(obj) = w.next().and_then(hex) else { return writeln!(out, "error: usage: ue dump <hex addr> [hex struct]") };
                let st = match w.next().and_then(hex) {
                    Some(s) => s,
                    None => {
                        writeln!(out, "{} {}", self.class_name(mem, obj).unwrap_or_default(), self.full_name(mem, obj))?;
                        self.class_of(mem, obj).unwrap_or(0)
                    }
                };
                for p in self.props(mem, st).iter() {
                    writeln!(out, "  +0x{:x} {} {} = {}{}", p.offset, p.kind, p.name, self.value_text(mem, obj + p.offset, p), self.extra_text(mem, p))?;
                }
                Ok(())
            }
            _ => writeln!(
                out,
                "objects 0x{:x}{}, names {:x?}, layout {:x?}, {} objects",
                self.objects,
                if self.flat { " (flat)" } else { "" },
                self.names_at,
                self.layout,
                self.object_count(mem)
            ),
        }
    }

    /// What a property refers to: its struct, inner type or class.
    fn extra_text(&self, mem: &Mem, p: &Prop) -> String {
        match p.kind.as_str() {
            "StructProperty" | "ObjectProperty" | "ClassProperty" | "WeakObjectProperty" | "SoftObjectProperty" => {
                format!(", {} 0x{:x}", self.object_name(mem, p.extra).unwrap_or_default(), p.extra)
            }
            "ArrayProperty" | "SetProperty" => match self.prop(mem, p.extra) {
                Some(inner) => format!(", of {} {}", inner.kind, inner.size) + &self.extra_text(mem, &inner),
                None => String::new(),
            },
            "MapProperty" => match (self.prop(mem, p.extra), self.prop(mem, p.extra2)) {
                (Some(k), Some(v)) => format!(", {} -> {}{}{}", k.kind, v.kind, self.extra_text(mem, &k), self.extra_text(mem, &v)),
                _ => String::new(),
            },
            _ => String::new(),
        }
    }
}

// --- Named paths through the game's objects.
// Text form: ue:<class>[/<name>][^<outer's class>].<step>.<step>...: every object of that
// class (with that name, inside an object of that class), then for each step a property:
// `Prop` enters a struct, follows an object or is the value itself (the last step);
// `Prop[Key=Value]` = the elements of an array whose Key property shows Value (an object's
// name, a name, a string or a number), `Prop[#3]` = element 3; `Prop{Key}` = a map's value
// under that key. Names are percent-encoded outside [A-Za-z0-9_-], so a path is one word.
// ue:EHHandCraftingObject/PlayerContainer^BP_PersonCharacter_C.Items[Item=CarbonOreResource].Quantity

const OBJ_FLAGS: u64 = 0x8;
/// Class default objects and archetypes: templates, not things in the game.
const TEMPLATE_FLAGS: u32 = 0x10 | 0x20;
const MAX_ELEMENTS: u64 = 1 << 16;
/// Places a path leads to at most (a wrong root can't make a walk take forever).
const MAX_LEADS: usize = 4096;
const MAX_DEPTH: usize = 4;

#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    Index(usize),
    /// The element's property and what it shows.
    By(String, String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Field(String),
    Elem(String, Key),
    Map(String, String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct UePath {
    pub class: String,
    pub name: Option<String>,
    pub outer: Option<String>,
    pub steps: Vec<Step>,
    /// Other places in the same object that keep the same count (a tally by item next to the
    /// stacks): written together, so the game never sees them disagree.
    pub also: Vec<Vec<Step>>,
}

fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(text.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else if b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'-' {
            out.push(b[i]);
            i += 1;
        } else {
            return None;
        }
    }
    String::from_utf8(out).ok().filter(|s| !s.is_empty())
}

impl UePath {
    pub fn parse(text: &str) -> Option<Self> {
        let (root, rest) = text.strip_prefix("ue:")?.split_once('.')?;
        let (root, outer) = match root.split_once('^') {
            Some((r, o)) => (r, Some(decode(o)?)),
            None => (root, None),
        };
        let (class, name) = match root.split_once('/') {
            Some((c, n)) => (decode(c)?, Some(decode(n)?)),
            None => (decode(root)?, None),
        };
        let mut lists = rest.split('+').map(parse_steps);
        let steps = lists.next()??;
        let also = lists.collect::<Option<Vec<_>>>()?;
        Some(UePath { class, name, outer, steps, also })
    }

    pub fn text(&self) -> String {
        let mut t = format!("ue:{}", encode(&self.class));
        if let Some(n) = &self.name {
            t += &format!("/{}", encode(n));
        }
        if let Some(o) = &self.outer {
            t += &format!("^{}", encode(o));
        }
        let lists: Vec<String> = std::iter::once(&self.steps).chain(&self.also).map(|l| steps_text(l)).collect();
        t + "." + &lists.join("+")
    }

    /// In words: "Quantity of the Items whose Item is CarbonOreResource, in PlayerContainer
    /// (of a BP_PersonCharacter_C)".
    pub fn describe(&self) -> String {
        let mut t = steps_words(&self.steps);
        for a in &self.also {
            t += &format!(" (and {})", steps_words(a));
        }
        t += &format!(", in {}", self.name.as_deref().unwrap_or(&self.class));
        if let Some(o) = &self.outer {
            t += &format!(" (of a {o})");
        }
        t
    }
}

fn parse_steps(text: &str) -> Option<Vec<Step>> {
    let steps = text
            .split('.')
            .map(|p| {
                if let Some(inner) = p.strip_suffix(']') {
                    let (prop, key) = inner.split_once('[')?;
                    let key = match key.strip_prefix('#') {
                        Some(i) => Key::Index(i.parse().ok()?),
                        None => {
                            let (k, v) = key.split_once('=')?;
                            Key::By(if k.is_empty() { String::new() } else { decode(k)? }, decode(v)?)
                        }
                    };
                    Some(Step::Elem(decode(prop)?, key))
                } else if let Some(inner) = p.strip_suffix('}') {
                    let (prop, key) = inner.split_once('{')?;
                    Some(Step::Map(decode(prop)?, decode(key)?))
                } else {
                    Some(Step::Field(decode(p)?))
                }
            })
            .collect::<Option<Vec<_>>>()?;
    (!steps.is_empty()).then_some(steps)
}

fn steps_text(steps: &[Step]) -> String {
    let parts: Vec<String> = steps
        .iter()
        .map(|s| match s {
            Step::Field(p) => encode(p),
            Step::Elem(p, Key::Index(i)) => format!("{}[#{i}]", encode(p)),
            Step::Elem(p, Key::By(k, v)) => format!("{}[{}={}]", encode(p), if k.is_empty() { String::new() } else { encode(k) }, encode(v)),
            Step::Map(p, k) => format!("{}{{{}}}", encode(p), encode(k)),
        })
        .collect();
    parts.join(".")
}

fn steps_words(steps: &[Step]) -> String {
    let what: Vec<String> = steps
        .iter()
        .rev()
        .map(|s| match s {
            Step::Field(p) => p.clone(),
            Step::Elem(p, Key::Index(i)) => format!("{p} #{i}"),
            Step::Elem(p, Key::By(k, v)) if k.is_empty() => format!("the {p} named {v}"),
            Step::Elem(p, Key::By(k, v)) => format!("the {p} whose {k} is {v}"),
            Step::Map(p, k) => format!("{p} of {k}"),
        })
        .collect();
    what.join(" of ")
}

/// An object's bytes, read at once: the headers of its arrays and maps come from here.
#[derive(Default)]
struct Snap {
    base: u64,
    bytes: Vec<u8>,
}

impl Snap {
    fn read(mem: &Mem, base: u64, size: u64) -> Snap {
        let mut bytes = vec![0u8; size.min(1 << 16) as usize];
        let n = mem.read_at(&mut bytes, base).unwrap_or(0);
        bytes.truncate(n);
        Snap { base, bytes }
    }

    /// An array's or a map's data pointer and element count (nothing when empty).
    fn head(&self, mem: &Mem, at: u64) -> Option<(u64, u64)> {
        let mut h = [0u8; 12];
        match at.checked_sub(self.base).filter(|o| o + 12 <= self.bytes.len() as u64) {
            Some(o) => h.copy_from_slice(&self.bytes[o as usize..][..12]),
            None => mem.read_exact_at(&mut h, at).ok()?,
        }
        let data = u64::from_le_bytes(h[..8].try_into().unwrap());
        let num = i32::from_le_bytes(h[8..].try_into().unwrap());
        (data != 0 && num > 0 && num as u64 <= MAX_ELEMENTS).then_some((data, num as u64))
    }
}

/// A name the game gives an object for this run only: "EHItemsContainer_2147479948".
fn run_name(name: &str) -> bool {
    name.rsplit_once('_').is_some_and(|(_, n)| n.len() >= 6 && n.bytes().all(|b| b.is_ascii_digit()))
}

fn round_up(v: u64, align: u64) -> u64 {
    v.div_ceil(align.max(1)) * align.max(1)
}

fn numeric(kind: &str) -> bool {
    matches!(
        kind,
        "IntProperty" | "Int64Property" | "UInt32Property" | "UInt64Property" | "Int16Property" | "UInt16Property"
            | "FloatProperty" | "DoubleProperty" | "ByteProperty" | "Int8Property" | "EnumProperty"
    )
}

impl Ue {
    fn is_template(&self, mem: &Mem, obj: u64) -> bool {
        u32_at(mem, obj + OBJ_FLAGS).is_none_or(|f| f & TEMPLATE_FLAGS != 0)
    }

    fn find_prop(&self, mem: &Mem, st: u64, name: &str) -> Option<Prop> {
        self.props(mem, st).iter().find(|p| p.name == name).cloned()
    }

    /// How a property's value reads as a key: an object's name, a name, a string, a number.
    fn key_text(&self, mem: &Mem, at: u64, p: &Prop) -> Option<String> {
        match p.kind.as_str() {
            "ObjectProperty" | "ClassProperty" => self.object_name(mem, ptr_at(mem, at)?),
            "NameProperty" => self.fname(mem, at),
            "StrProperty" => self.fstring(mem, at).filter(|s| !s.is_empty()),
            "IntProperty" | "EnumProperty" | "ByteProperty" | "Int64Property" if p.size <= 8 => {
                let mut b = [0u8; 8];
                mem.read_exact_at(&mut b[..p.size as usize], at).ok()?;
                let v = i64::from_le_bytes(b);
                Some(if p.size == 4 { (v as i32).to_string() } else { v.to_string() })
            }
            _ => None,
        }
    }

    fn align(&self, mem: &Mem, p: &Prop) -> u64 {
        match p.kind.as_str() {
            "StructProperty" => i32_at(mem, p.extra + self.layout.align).map_or(8, |a| a.clamp(1, 16) as u64),
            "BoolProperty" => 1,
            _ => match p.size {
                1 | 2 | 4 => p.size,
                s if s % 8 == 0 => 8,
                _ => 4,
            },
        }
    }

    /// The elements of the array at `at`: (address, struct or class, or 0 for a number).
    fn elements(&self, mem: &Mem, at: u64, p: &Prop) -> Vec<(u64, u64)> {
        let Some(inner) = p.inner.as_deref() else { return Vec::new() };
        let mut h = [0u8; 12];
        if mem.read_exact_at(&mut h, at).is_err() {
            return Vec::new();
        }
        let data = u64::from_le_bytes(h[..8].try_into().unwrap());
        let num = i32::from_le_bytes(h[8..].try_into().unwrap());
        if data == 0 || num <= 0 || num as u64 > MAX_ELEMENTS || inner.size == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u8; (num as u64 * inner.size) as usize];
        if mem.read_exact_at(&mut buf, data).is_err() {
            return Vec::new();
        }
        (0..num as u64)
            .filter_map(|i| {
                let e = data + i * inner.size;
                match inner.kind.as_str() {
                    "StructProperty" => Some((e, inner.extra)),
                    "ObjectProperty" => {
                        let o = u64::from_le_bytes(buf[(i * inner.size) as usize..][..8].try_into().unwrap());
                        Some((o, self.class_of(mem, o)?))
                    }
                    k if numeric(k) => Some((e, 0)),
                    _ => None,
                }
            })
            .collect()
    }

    /// The entries of the map at `at` that are in use: (key address, value address). A map is
    /// a sparse array of { key, value, hash next, hash index } with a bit per slot in use.
    fn map_entries(&self, mem: &Mem, at: u64, kp: &Prop, vp: &Prop) -> Vec<(u64, u64)> {
        let (Some(data), Some(num)) = (u64_at(mem, at), i32_at(mem, at + 8)) else { return Vec::new() };
        if data == 0 || num <= 0 || num as u64 > MAX_ELEMENTS {
            return Vec::new();
        }
        let secondary = u64_at(mem, at + 0x20).unwrap_or(0);
        let bits_at = if secondary != 0 { secondary } else { at + 0x10 };
        let mut bits = vec![0u8; (num as usize).div_ceil(32) * 4];
        if mem.read_exact_at(&mut bits, bits_at).is_err() {
            return Vec::new();
        }
        let (ka, va) = (self.align(mem, kp), self.align(mem, vp));
        let voff = round_up(kp.size, va);
        let pair = voff + vp.size;
        let stride = round_up(round_up(pair, 4) + 8, ka.max(va).max(4)).max(8);
        (0..num as u64)
            .filter(|i| bits[(i / 8) as usize] & (1 << (i % 8)) != 0)
            .map(|i| (data + i * stride, data + i * stride + voff))
            .collect()
    }

    /// Where a property leads from a struct or object at `at`: (address, its struct or class,
    /// 0 for a number).
    fn enter(&self, mem: &Mem, at: u64, p: &Prop) -> Option<(u64, u64)> {
        match p.kind.as_str() {
            "StructProperty" => Some((at, p.extra)),
            "ObjectProperty" => {
                let o = ptr_at(mem, at)?;
                Some((o, self.class_of(mem, o)?))
            }
            k if numeric(k) => Some((at, 0)),
            _ => None,
        }
    }

    /// The objects a path starts from now.
    pub fn roots(&self, mem: &Mem, path: &UePath) -> Vec<u64> {
        let mut class_names: HashMap<u64, Option<String>> = HashMap::new();
        let mut class_name = |c: u64| class_names.entry(c).or_insert_with(|| self.object_name(mem, c)).clone();
        self.objects(mem)
            .into_iter()
            .filter(|&o| {
                self.class_of(mem, o).is_some_and(|c| class_name(c).as_deref() == Some(path.class.as_str()))
                    && !self.is_template(mem, o)
                    && path.name.as_ref().is_none_or(|n| self.object_name(mem, o).as_ref() == Some(n))
                    && path.outer.as_ref().is_none_or(|n| {
                        self.outer(mem, o).and_then(|out| self.class_of(mem, out)).is_some_and(|c| class_name(c).as_ref() == Some(n))
                    })
            })
            .collect()
    }

    /// Where a path leads from these objects (each still checked: the game may have freed it).
    pub fn walk(&self, mem: &Mem, roots: &[u64], path: &UePath) -> Vec<u64> {
        let start: Vec<(u64, u64)> = roots
            .iter()
            .filter(|&&r| self.class_name(mem, r).as_deref() == Some(path.class.as_str()) && !self.is_template(mem, r))
            .filter_map(|&r| Some((r, self.class_of(mem, r)?)))
            .collect();
        let mut leads: Vec<u64> = std::iter::once(&path.steps).chain(&path.also).flat_map(|s| self.walk_steps(mem, start.clone(), s)).collect();
        leads.sort();
        leads.dedup();
        leads
    }

    fn walk_steps(&self, mem: &Mem, mut cur: Vec<(u64, u64)>, steps: &[Step]) -> Vec<u64> {
        for (i, step) in steps.iter().enumerate() {
            let last = i + 1 == steps.len();
            let mut next = Vec::new();
            for &(at, st) in &cur {
                if st == 0 {
                    continue;
                }
                match step {
                    Step::Field(name) => {
                        let Some(p) = self.find_prop(mem, st, name) else { continue };
                        next.extend(self.enter(mem, at + p.offset, &p));
                    }
                    Step::Elem(name, key) => {
                        let Some(p) = self.find_prop(mem, st, name).filter(|p| p.kind == "ArrayProperty") else { continue };
                        for (n, (e, est)) in self.elements(mem, at + p.offset, &p).into_iter().enumerate() {
                            let keep = match key {
                                Key::Index(k) => n == *k,
                                Key::By(k, v) if k.is_empty() => est != 0 && self.object_name(mem, e).as_ref() == Some(v),
                                Key::By(k, v) => est != 0
                                    && self.find_prop(mem, est, k).and_then(|kp| self.key_text(mem, e + kp.offset, &kp)).as_ref() == Some(v),
                            };
                            if keep {
                                next.push((e, est));
                            }
                        }
                    }
                    Step::Map(name, key) => {
                        let Some(p) = self.find_prop(mem, st, name).filter(|p| p.kind == "MapProperty") else { continue };
                        let (Some(kp), Some(vp)) = (p.inner.as_deref(), p.value.as_deref()) else { continue };
                        for (k, v) in self.map_entries(mem, at + p.offset, kp, vp) {
                            if self.key_text(mem, k, kp).as_ref() == Some(key) {
                                next.extend(self.enter(mem, v, vp));
                            }
                        }
                    }
                }
            }
            // Only the last step may end at a number.
            next.retain(|&(_, st)| (st == 0) == last);
            next.truncate(MAX_LEADS);
            cur = next;
        }
        cur.into_iter().map(|(a, _)| a).collect()
    }

    /// Other places in the root object keeping the same count as the path's place: a map
    /// under the same key (Astro Colony's ItemsLookupCounts{CarbonOreResource} next to
    /// Items[Item=CarbonOreResource].Quantity), or another array's element with the same key
    /// and the same field, holding the same number now.
    fn twins(&self, mem: &Mem, root: u64, steps: &[Step], target: u64) -> Vec<Vec<Step>> {
        let keys: Vec<&String> = steps
            .iter()
            .filter_map(|s| match s {
                Step::Elem(_, Key::By(_, v)) => Some(v),
                Step::Map(_, k) => Some(k),
                _ => None,
            })
            .collect();
        let first = match steps.first() {
            Some(Step::Field(p) | Step::Elem(p, _) | Step::Map(p, _)) => p,
            None => return Vec::new(),
        };
        let leaf = match steps.last() {
            Some(Step::Field(f)) => Some(f),
            _ => None,
        };
        // A Gameplay Ability System attribute keeps BaseValue (what the game adds to and saves)
        // and CurrentValue (with temporary bonuses: what its checks read), equal unless a
        // bonus runs. Written together: Minecraft Dungeons II adds a pickup to Emeralds'
        // BaseValue, and a CurrentValue left behind would keep the old amount for buying.
        let mut out = Vec::new();
        if let (Some(leaf), Some(class)) = (leaf, self.class_of(mem, root)) {
            let other = match leaf.as_str() {
                "BaseValue" => Some("CurrentValue"),
                "CurrentValue" => Some("BaseValue"),
                _ => None,
            };
            if let (Some(other), true) = (other, self.in_attribute(mem, class, steps)) {
                let mut twin = steps.to_vec();
                *twin.last_mut().unwrap() = Step::Field(other.into());
                out.push(twin);
            }
        }
        let (Some(class), false) = (self.class_of(mem, root), keys.is_empty()) else { return out };
        let same = |at: u64, size: u64| {
            let n = size.clamp(1, 8) as usize;
            let (mut a, mut b) = ([0u8; 8], [0u8; 8]);
            mem.read_exact_at(&mut a[..n], at).is_ok() && mem.read_exact_at(&mut b[..n], target).is_ok() && a == b && a != [0; 8]
        };
        for p in self.props(mem, class).iter().filter(|p| p.name != *first) {
            match p.kind.as_str() {
                "MapProperty" => {
                    let (Some(kp), Some(vp)) = (p.inner.as_deref(), p.value.as_deref()) else { continue };
                    if !numeric(&vp.kind) {
                        continue;
                    }
                    for (k, v) in self.map_entries(mem, root + p.offset, kp, vp) {
                        let Some(key) = self.key_text(mem, k, kp).filter(|k| keys.contains(&k)) else { continue };
                        if same(v, vp.size) {
                            out.push(vec![Step::Map(p.name.clone(), key)]);
                        }
                    }
                }
                "ArrayProperty" => {
                    let Some(leaf) = leaf else { continue };
                    for (e, est) in self.elements(mem, root + p.offset, p) {
                        let Some(lp) = self.find_prop(mem, est, leaf).filter(|lp| numeric(&lp.kind)) else { continue };
                        for kp in self.props(mem, est).iter() {
                            let Some(key) = self.key_text(mem, e + kp.offset, kp).filter(|k| keys.contains(&k)) else { continue };
                            if same(e + lp.offset, lp.size) {
                                out.push(vec![Step::Elem(p.name.clone(), Key::By(kp.name.clone(), key)), Step::Field(leaf.clone())]);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out.dedup();
        out
    }

    /// Whether `steps` (fields only) end in a field of a GameplayAttributeData struct (or a
    /// struct made from it).
    fn in_attribute(&self, mem: &Mem, class: u64, steps: &[Step]) -> bool {
        let mut st = class;
        for s in &steps[..steps.len().saturating_sub(1)] {
            let Step::Field(name) = s else { return false };
            match self.find_prop(mem, st, name) {
                Some(p) if p.kind == "StructProperty" => st = p.extra,
                _ => return false,
            }
        }
        let mut cur = Some(st);
        for _ in 0..8 {
            let Some(s) = cur.filter(|&s| s != 0 && s != class) else { return false };
            if self.object_name(mem, s).as_deref() == Some("GameplayAttributeData") {
                return true;
            }
            cur = self.super_of(mem, s);
        }
        false
    }

    /// The key that tells element `i` of these apart from the others: one of its properties
    /// (an object, a name, a string, then a number) that no other element shows the same.
    fn elem_key(&self, mem: &Mem, elems: &[(u64, u64)], i: usize) -> Key {
        let (e, st) = elems[i];
        let props = self.props(mem, st);
        let rank = |k: &str| match k {
            "ObjectProperty" => 0,
            "NameProperty" => 1,
            "StrProperty" => 2,
            _ => 3,
        };
        let mut keys: Vec<&Prop> = props.iter().filter(|p| p.dim == 1).collect();
        keys.sort_by_key(|p| rank(&p.kind));
        for k in keys {
            let Some(v) = self.key_text(mem, e + k.offset, k) else { continue };
            let unique = elems.iter().enumerate().all(|(j, &(o, ost))| j == i || ost != st || self.key_text(mem, o + k.offset, k).as_ref() != Some(&v));
            if unique {
                return Key::By(k.name.clone(), v);
            }
        }
        Key::Index(i)
    }

    /// Whether a struct holds arrays or maps (itself or in structs inside it).
    fn has_containers(&self, mem: &Mem, st: u64, depth: usize) -> bool {
        if let Some(&has) = self.containers.lock().unwrap().get(&st) {
            return has;
        }
        let has = depth < MAX_DEPTH
            && self.props(mem, st).iter().any(|p| match p.kind.as_str() {
                "ArrayProperty" | "MapProperty" => true,
                "StructProperty" => self.has_containers(mem, p.extra, depth + 1),
                _ => false,
            });
        self.containers.lock().unwrap().insert(st, has);
        has
    }

    /// The steps from the struct or object at `at` to `target`.
    fn find_in(&self, mem: &Mem, snap: &Snap, at: u64, st: u64, target: u64, depth: usize) -> Option<Vec<Step>> {
        if depth > MAX_DEPTH {
            return None;
        }
        let with = |first: Step, rest: Vec<Step>| Some(std::iter::once(first).chain(rest).collect());
        let props = self.props(mem, st);
        for p in props.iter() {
            let start = at + p.offset;
            let inside = target >= start && target < start + p.size * p.dim;
            match p.kind.as_str() {
                "StructProperty" if inside || self.has_containers(mem, p.extra, 0) => {
                    if let Some(rest) = self.find_in(mem, snap, start, p.extra, target, depth + 1) {
                        return with(Step::Field(p.name.clone()), rest);
                    }
                }
                k if numeric(k) && target == start => return Some(vec![Step::Field(p.name.clone())]),
                "ArrayProperty" => {
                    let Some(inner) = p.inner.as_deref() else { continue };
                    // Most arrays can't hold it: read their elements only when it's among them or
                    // they hold arrays in turn.
                    let Some((data, num)) = snap.head(mem, start) else { continue };
                    let deep = inner.kind == "StructProperty" && self.has_containers(mem, inner.extra, 0);
                    if !deep && (target < data || target >= data + num * inner.size) {
                        continue;
                    }
                    let elems = self.elements(mem, start, p);
                    for (i, &(e, est)) in elems.iter().enumerate() {
                        let here = inner.kind != "ObjectProperty" && target >= e && target < e + inner.size;
                        if est == 0 && target == e {
                            return Some(vec![Step::Elem(p.name.clone(), Key::Index(i))]);
                        }
                        if here || (inner.kind == "StructProperty" && self.has_containers(mem, est, 0)) {
                            if let Some(rest) = self.find_in(mem, &Snap::default(), e, est, target, depth + 1) {
                                return with(Step::Elem(p.name.clone(), self.elem_key(mem, &elems, i)), rest);
                            }
                        }
                    }
                }
                "MapProperty" => {
                    let (Some(kp), Some(vp)) = (p.inner.as_deref(), p.value.as_deref()) else { continue };
                    // An entry is the key, the value and two ints, padded.
                    let Some((data, num)) = snap.head(mem, start) else { continue };
                    if target < data || target >= data + num * (kp.size + vp.size + 24) {
                        continue;
                    }
                    for (k, v) in self.map_entries(mem, start, kp, vp) {
                        if target < v || target >= v + vp.size {
                            continue;
                        }
                        let Some(key) = self.key_text(mem, k, kp) else { continue };
                        match self.enter(mem, v, vp) {
                            Some((_, 0)) if target == v => return Some(vec![Step::Map(p.name.clone(), key)]),
                            Some((sa, sst)) if vp.kind == "StructProperty" => {
                                if let Some(rest) = self.find_in(mem, &Snap::default(), sa, sst, target, depth + 1) {
                                    return with(Step::Map(p.name.clone(), key), rest);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Paths from the game's objects to `target`, each with every place it leads to now, best
    /// (fewest places, then fewest steps) first.
    pub fn discover(&self, mem: &Mem, target: u64) -> Vec<(UePath, Vec<u64>)> {
        self.discover_all(mem, &[target]).pop().unwrap_or_default()
    }

    /// `discover` for several places in one pass over the objects.
    pub fn discover_all(&self, mem: &Mem, targets: &[u64]) -> Vec<Vec<(UePath, Vec<u64>)>> {
        let mut found: Vec<Vec<(UePath, Vec<u64>)>> = vec![Vec::new(); targets.len()];
        let mut sizes: HashMap<u64, u64> = HashMap::new();
        for obj in self.objects(mem) {
            let mut head = [0u8; 0x18];
            if mem.read_exact_at(&mut head, obj).is_err() {
                continue;
            }
            let flags = u32::from_le_bytes(head[8..12].try_into().unwrap());
            let class = u64::from_le_bytes(head[0x10..].try_into().unwrap());
            if flags & TEMPLATE_FLAGS != 0 || class == 0 {
                continue;
            }
            let size = *sizes.entry(class).or_insert_with(|| self.struct_size(mem, class));
            let inside = targets.iter().any(|&t| t >= obj && t < obj + size);
            if !inside && !self.has_containers(mem, class, 0) {
                continue;
            }
            let snap = Snap::read(mem, obj, size);
            for (i, &target) in targets.iter().enumerate() {
                let Some(steps) = self.find_in(mem, &snap, obj, class, target, 0) else { continue };
                let Some(class_name) = self.object_name(mem, class) else { continue };
                let name = self.object_name(mem, obj).filter(|n| !run_name(n));
                let outer = self.outer(mem, obj).and_then(|o| self.class_name(mem, o)).filter(|n| n != "Package");
                let also = self.twins(mem, obj, &steps, target);
                let path = UePath { class: class_name, name, outer, steps, also };
                let leads = self.walk(mem, &self.roots(mem, &path), &path);
                if leads.contains(&target) && !found[i].iter().any(|(p, _)| *p == path) {
                    found[i].push((path, leads));
                }
            }
        }
        for f in &mut found {
            f.sort_by_key(|(p, leads)| (leads.len(), p.steps.len()));
        }
        found
    }
}

// --- The player's numbers by name (the Browse tab): what a cheat table lists, found through the
// engine's own objects instead of a search. Every Unreal game has GameEngine -> GameInstance ->
// LocalPlayers -> PlayerController -> the pawn it controls and its PlayerState (where Cheat
// Engine tables start: Minecraft Dungeons' does). Listed: their numbers, those of the objects
// inside them (components, Gameplay Ability System attribute sets: health, currencies), of the
// actors they own (a weapon) and of the objects their fields point to (an inventory's slots).

/// Packages of the engine's and its plugins' classes and structs: their thousands of settings
/// aren't listed, only `ENGINE_FIELDS`.
const ENGINE_PACKAGES: &[&str] = &[
    "CoreUObject", "Engine", "GameplayAbilities", "GameplayTasks", "GameplayTags", "AIModule", "NavigationSystem", "UMG", "Slate",
    "SlateCore", "InputCore", "EnhancedInput", "PhysicsCore", "Chaos", "ChaosSolverEngine", "MovieScene", "MovieSceneTracks",
    "LevelSequence", "Niagara", "NiagaraCore", "CommonUI", "CommonInput", "ModularGameplay", "GameFeatures", "Paper2D", "AudioMixer",
    "NetCore", "DeveloperSettings", "AnimGraphRuntime", "CinematicCamera", "MediaAssets", "OnlineSubsystem", "OnlineSubsystemUtils",
    "Landscape", "Foliage", "AudioExtensions", "ControlRig", "IKRig", "MotionWarping", "GeometryCollectionEngine", "FieldSystemEngine",
    "StateTreeModule", "SmartObjectsModule", "MassEntity", "ModelViewViewModel", "ClothingSystemRuntimeInterface", "PhysicsControl",
    "EngineCameras", "GameplayCameras", "TemplateSequence",
];
/// The engine's fields worth changing, the same in every game using its classes: movement
/// (CharacterMovementComponent), jumps (Character), a player's score.
const ENGINE_FIELDS: &[&str] = &[
    "MaxWalkSpeed", "MaxWalkSpeedCrouched", "MaxSwimSpeed", "MaxFlySpeed", "MaxAcceleration", "JumpZVelocity", "GravityScale",
    "AirControl", "JumpMaxCount", "JumpMaxHoldTime", "Score",
];
/// The character's own speed (every actor has one: only the character's is listed).
const TIME_DILATION: &str = "CustomTimeDilation";
/// Objects left out with what's inside them: parts with a place in the world (meshes, cameras:
/// hundreds of rendering settings), the HUD's widgets (copies of numbers for the screen: setting
/// one changes nothing), input, animation, the world itself.
const LEFT_OUT: &[&str] =
    &["SceneComponent", "Widget", "InputComponent", "AnimInstance", "Level", "World", "PlayerCameraManager", "CameraModifier"];
/// The game's inner workings, listed only when the player asks for everything (`ListedObject.more`):
/// abilities (Minecraft Dungeons II has ~90 on the character, ~1900 of its 2377 numbers, each
/// an action's settings), gameplay cues and effects, subsystems (telemetry, audio, tutorials),
/// the settings file.
const MACHINERY: &[&str] =
    &["GameplayAbility", "AbilityTask", "GameplayCueNotify_Actor", "GameplayCueNotify_Static", "GameplayEffect", "Subsystem", "GameUserSettings"];
const MAX_GROUPS: usize = 300;
pub const MAX_LISTED: usize = 5000;
/// Elements of one array or map listed at most.
const LIST_ELEMENTS: usize = 64;

/// A number listed for the player: what it's called ("Health", "Slots[Item=Arrow].Count") and
/// where it is.
pub struct Listed {
    pub label: String,
    pub addr: u64,
    /// The helper's value type: i32, f32, f64 or u16.
    pub kind: &'static str,
}

/// An object whose numbers are listed, and how it belongs to the player.
pub struct ListedObject {
    pub name: String,
    pub class: String,
    /// character, state, controller, game (the game instance or state), owned (an actor the
    /// player's own: a weapon), part (an object inside one of those), linked (one their fields
    /// point to).
    pub role: &'static str,
    /// The object it belongs to, for owned, part and linked.
    pub owner: Option<String>,
    /// Not the player's own: the controller's and the game's objects and what's in them, and
    /// `MACHINERY` (attribute sets never are).
    pub more: bool,
    pub fields: Vec<Listed>,
}

/// Why there's no list.
#[derive(Debug, PartialEq)]
pub enum NoPlayer {
    /// No local player, or it controls no pawn now (a menu, a loading screen).
    NoCharacter,
}

fn listed_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "IntProperty" | "UInt32Property" => Some("i32"),
        "FloatProperty" => Some("f32"),
        "DoubleProperty" => Some("f64"),
        "Int16Property" | "UInt16Property" => Some("u16"),
        _ => None,
    }
}

/// Walks objects' fields for `Ue::player`.
struct Lister<'a> {
    ue: &'a Ue,
    mem: &'a Mem,
    engine: HashMap<u64, bool>,
    out: Vec<Listed>,
    /// Objects the fields point to (`links` on).
    links: Vec<u64>,
    total: usize,
    /// The fields are the character's own.
    character: bool,
}

impl Lister<'_> {
    fn push(&mut self, label: String, addr: u64, kind: &'static str) {
        if self.total < MAX_LISTED {
            self.total += 1;
            self.out.push(Listed { label, addr, kind });
        }
    }

    fn is_engine(&mut self, st: u64) -> bool {
        if let Some(&e) = self.engine.get(&st) {
            return e;
        }
        let e = self.ue.package(self.mem, st).and_then(|p| p.strip_prefix("/Script/").map(|m| ENGINE_PACKAGES.contains(&m))).unwrap_or(false);
        self.engine.insert(st, e);
        e
    }

    /// The numbers of the struct or object at `at`, each struct's own properties checked against
    /// its package.
    fn fields(&mut self, at: u64, st: u64, prefix: &str, depth: usize, links: bool) {
        let mut chain = vec![st];
        while let Some(s) = self.ue.super_of(self.mem, chain[chain.len() - 1]).filter(|_| chain.len() < 64) {
            chain.push(s);
        }
        // `props` lists parents' properties first: a struct's own come after its parent's.
        for i in (0..chain.len()).rev() {
            let from = chain.get(i + 1).map_or(0, |&s| self.ue.props(self.mem, s).len());
            let props = self.ue.props(self.mem, chain[i]);
            let engine = self.is_engine(chain[i]);
            for p in props.iter().skip(from) {
                self.field(at, p, prefix, depth, engine, links);
            }
        }
    }

    fn field(&mut self, at: u64, p: &Prop, prefix: &str, depth: usize, engine: bool, links: bool) {
        let label = if prefix.is_empty() { p.name.clone() } else { format!("{prefix}.{}", p.name) };
        let start = at + p.offset;
        let mem = self.mem;
        if engine {
            match listed_kind(&p.kind) {
                Some(kind) if p.dim == 1 && (ENGINE_FIELDS.contains(&p.name.as_str()) || self.character && p.name == TIME_DILATION) => {
                    self.push(label, start, kind)
                }
                // The ability system's attribute sets, when they aren't inside the actor (they
                // usually are).
                _ if links && p.name == "SpawnedAttributes" => {
                    self.links.extend(self.ue.elements(mem, start, p).into_iter().take(LIST_ELEMENTS).map(|(o, _)| o));
                }
                _ => {}
            }
            return;
        }
        // Only the first of a fixed-size array would be found again by name.
        if p.dim != 1 {
            return;
        }
        if let Some(kind) = listed_kind(&p.kind) {
            return self.push(label, start, kind);
        }
        match p.kind.as_str() {
            "StructProperty" => self.in_struct(start, p.extra, &label, depth),
            "ObjectProperty" if links => self.links.extend(ptr_at(mem, start)),
            "ArrayProperty" => {
                let Some(inner) = p.inner.as_deref() else { return };
                let elems = self.ue.elements(mem, start, p);
                let n = elems.len().min(LIST_ELEMENTS);
                if let Some(kind) = listed_kind(&inner.kind) {
                    for (i, &(e, _)) in elems[..n].iter().enumerate() {
                        self.push(format!("{label} #{i}"), e, kind);
                    }
                } else if inner.kind == "StructProperty" {
                    for i in 0..n {
                        let l = match self.ue.elem_key(mem, &elems, i) {
                            Key::Index(i) => format!("{label} #{i}"),
                            Key::By(k, v) => format!("{label}[{k}={v}]"),
                        };
                        self.in_struct(elems[i].0, elems[i].1, &l, depth);
                    }
                } else if inner.kind == "ObjectProperty" && links {
                    self.links.extend(elems[..n].iter().map(|&(o, _)| o));
                }
            }
            "MapProperty" => {
                let (Some(kp), Some(vp)) = (p.inner.as_deref(), p.value.as_deref()) else { return };
                for (k, v) in self.ue.map_entries(mem, start, kp, vp).into_iter().take(LIST_ELEMENTS) {
                    let Some(key) = self.ue.key_text(mem, k, kp) else { continue };
                    let l = format!("{label}{{{key}}}");
                    match listed_kind(&vp.kind) {
                        Some(kind) => self.push(l, v, kind),
                        None if vp.kind == "StructProperty" => self.in_struct(v, vp.extra, &l, depth),
                        None => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// A struct's numbers; a Gameplay Ability System attribute is one number (CurrentValue:
    /// what the game goes by; saving pairs BaseValue with it).
    fn in_struct(&mut self, at: u64, st: u64, label: &str, depth: usize) {
        if self.ue.is_attribute(self.mem, st) {
            if let Some(cur) = self.ue.find_prop(self.mem, st, "CurrentValue") {
                self.push(label.to_owned(), at + cur.offset, "f32");
            }
        } else if depth < 2 && !self.is_engine(st) {
            self.fields(at, st, label, depth + 1, false);
        }
    }
}

impl Ue {
    /// The name of the package an object (a class, a struct) is in: "/Script/Engine",
    /// "/Game/Blueprints/BP_Player".
    fn package(&self, mem: &Mem, obj: u64) -> Option<String> {
        let mut cur = obj;
        for _ in 0..16 {
            match self.outer(mem, cur) {
                Some(o) => cur = o,
                None => break,
            }
        }
        self.object_name(mem, cur)
    }

    /// Whether a struct is (or is made from) GameplayAttributeData.
    fn is_attribute(&self, mem: &Mem, st: u64) -> bool {
        let mut cur = Some(st);
        for _ in 0..8 {
            let Some(s) = cur else { return false };
            if self.object_name(mem, s).as_deref() == Some("GameplayAttributeData") {
                return true;
            }
            cur = self.super_of(mem, s);
        }
        false
    }

    /// The names of a class and its parents.
    fn lineage(&self, mem: &Mem, class: u64) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = Some(class);
        while let Some(c) = cur.filter(|_| out.len() < 64) {
            out.push(self.object_name(mem, c).unwrap_or_default());
            cur = self.super_of(mem, c);
        }
        out
    }

    /// The live object an object property of `obj` points to.
    fn object_in(&self, mem: &Mem, obj: u64, prop: &str) -> Option<u64> {
        let p = self.find_prop(mem, self.class_of(mem, obj)?, prop).filter(|p| p.kind == "ObjectProperty")?;
        ptr_at(mem, obj + p.offset).filter(|&o| self.class_of(mem, o).is_some() && !self.is_template(mem, o))
    }

    /// The player's numbers, by object: their character first, then the actors it owns, the
    /// player state, the controller, the game, and the objects their fields point to.
    pub fn player(&self, mem: &Mem) -> Result<Vec<ListedObject>, NoPlayer> {
        // Every live object's class and outer, read once.
        let mut heads: Vec<(u64, u64, u64)> = Vec::new();
        for obj in self.objects(mem) {
            let mut head = [0u8; 0x28];
            if mem.read_exact_at(&mut head, obj).is_err() {
                continue;
            }
            let word = |at: usize| u64::from_le_bytes(head[at..at + 8].try_into().unwrap());
            let flags = u32::from_le_bytes(head[8..12].try_into().unwrap());
            if flags & TEMPLATE_FLAGS == 0 && word(0x10) != 0 {
                heads.push((obj, word(0x10), word(0x20)));
            }
        }
        let mut lineages: HashMap<u64, Vec<String>> = HashMap::new();
        let mut is_a = |class: u64, names: &[&str]| lineages.entry(class).or_insert_with(|| self.lineage(mem, class)).iter().any(|n| names.contains(&n.as_str()));
        let instance = heads.iter().find(|h| is_a(h.1, &["GameEngine"])).and_then(|h| self.object_in(mem, h.0, "GameInstance"));
        let from_instance = instance.and_then(|gi| {
            let class = self.class_of(mem, gi)?;
            let p = self.find_prop(mem, class, "LocalPlayers").filter(|p| p.kind == "ArrayProperty")?;
            self.elements(mem, gi + p.offset, &p).into_iter().map(|(o, _)| o).find(|&o| o != 0)
        });
        let local = from_instance.or_else(|| heads.iter().find(|h| is_a(h.1, &["LocalPlayer"])).map(|h| h.0));
        let controller = local.and_then(|l| self.object_in(mem, l, "PlayerController"));
        let pawn = controller.and_then(|c| self.object_in(mem, c, "AcknowledgedPawn").or_else(|| self.object_in(mem, c, "Pawn")));
        let (Some(controller), Some(pawn)) = (controller, pawn) else { return Err(NoPlayer::NoCharacter) };
        let state = self.object_in(mem, controller, "PlayerState").or_else(|| self.object_in(mem, pawn, "PlayerState"));
        let mut world = self.outer(mem, pawn);
        for _ in 0..4 {
            match world {
                Some(w) if !self.class_of(mem, w).is_some_and(|c| is_a(c, &["World"])) => world = self.outer(mem, w),
                _ => break,
            }
        }
        let game_state = world.and_then(|w| self.object_in(mem, w, "GameState"));

        // A pawn that only steers another (Hell Pie possesses a camera rig, a plain Pawn, whose
        // ControlledPawn is the demon the player moves): that Character is theirs too, first.
        // Only through a field made to hold a pawn (the rig's AdditionalInputActor, an Actor
        // field, holds Cupid, also a Character), only a Character, and only when one is pointed
        // to: a field could be an enemy it targets.
        let pawn_class = self.class_of(mem, pawn).unwrap_or(0);
        let driven = if is_a(pawn_class, &["Character"]) {
            None
        } else {
            let mut found: Vec<u64> = Vec::new();
            let fields: Vec<Prop> = self.props(mem, pawn_class).iter().filter(|p| p.kind == "ObjectProperty" && p.dim == 1 && p.extra != 0).cloned().collect();
            let pawn_fields: Vec<Prop> = fields.into_iter().filter(|p| is_a(p.extra, &["Pawn"])).collect();
            for p in &pawn_fields {
                let Some(o) = ptr_at(mem, pawn + p.offset).filter(|&o| o != pawn && !found.contains(&o)) else { continue };
                if !self.is_template(mem, o) && self.class_of(mem, o).is_some_and(|c| is_a(c, &["Character"])) {
                    found.push(o);
                }
            }
            (found.len() == 1).then(|| found[0])
        };
        // (object, role, the group it belongs to)
        let mut groups: Vec<(u64, &'static str, Option<usize>)> = driven.into_iter().map(|d| (d, "character", None)).collect();
        groups.push((pawn, "character", None));
        let roots = [(state, "state"), (Some(controller), "controller"), (instance, "game"), (game_state, "game")];
        groups.extend(roots.into_iter().filter_map(|(o, role)| Some((o?, role, None))));
        let mut index: HashMap<u64, usize> = groups.iter().enumerate().map(|(i, g)| (g.0, i)).collect();
        // Actors the player's character, controller or state own.
        if let Some(owner) = self.find_prop(mem, self.class_of(mem, pawn).unwrap_or(0), "Owner").filter(|p| p.kind == "ObjectProperty") {
            for &(obj, class, _) in &heads {
                if index.contains_key(&obj) || !is_a(class, &["Actor"]) || is_a(class, LEFT_OUT) {
                    continue;
                }
                let by = ptr_at(mem, obj + owner.offset).and_then(|o| index.get(&o).copied()).filter(|&i| groups[i].2.is_none());
                if let Some(by) = by.filter(|_| groups.len() < MAX_GROUPS) {
                    index.insert(obj, groups.len());
                    groups.push((obj, "owned", Some(by)));
                }
            }
        }
        // Objects inside those (their outer chain reaches one), past no left-out object.
        let tops = groups.len();
        let outers: HashMap<u64, (u64, u64)> = heads.iter().map(|&(o, c, out)| (o, (c, out))).collect();
        for &(obj, class, outer) in &heads {
            if index.contains_key(&obj) || is_a(class, LEFT_OUT) || groups.len() >= MAX_GROUPS {
                continue;
            }
            let mut cur = outer;
            for _ in 0..3 {
                if let Some(&g) = index.get(&cur).filter(|&&g| g < tops) {
                    index.insert(obj, groups.len());
                    groups.push((obj, "part", Some(g)));
                    break;
                }
                match outers.get(&cur) {
                    Some(&(c, out)) if !is_a(c, LEFT_OUT) => cur = out,
                    _ => break,
                }
            }
        }
        let mut lister = Lister { ue: self, mem, engine: HashMap::new(), out: Vec::new(), links: Vec::new(), total: 0, character: false };
        let mut fields: Vec<Vec<Listed>> = Vec::new();
        let mut i = 0;
        while i < groups.len() {
            let (obj, role, _) = groups[i];
            let links = role != "linked";
            lister.character = role == "character";
            if let Some(class) = self.class_of(mem, obj) {
                lister.fields(obj, class, "", 0, links);
            }
            fields.push(std::mem::take(&mut lister.out));
            for o in std::mem::take(&mut lister.links) {
                if index.contains_key(&o) || groups.len() >= MAX_GROUPS || !links {
                    continue;
                }
                let Some(class) = self.class_of(mem, o).filter(|_| !self.is_template(mem, o)) else { continue };
                // Assets (an item's definition) are in a package of their own: the same for
                // every player, not theirs.
                let asset = self.outer(mem, o).is_some_and(|out| {
                    self.class_name(mem, out).as_deref() == Some("Package") && self.object_name(mem, out).as_deref() != Some("/Engine/Transient")
                });
                if !asset && !is_a(class, LEFT_OUT) && !is_a(class, &["Actor"]) {
                    index.insert(o, groups.len());
                    groups.push((o, "linked", Some(i)));
                }
            }
            i += 1;
        }
        // Owners come before what they own: their mark is known first.
        let mut more = vec![false; groups.len()];
        for g in 0..groups.len() {
            let (obj, role, owner) = groups[g];
            let class = self.class_of(mem, obj).unwrap_or(0);
            let (attributes, machinery) = (is_a(class, &["AttributeSet"]), is_a(class, MACHINERY));
            more[g] = !attributes && (machinery || matches!(role, "controller" | "game") || owner.is_some_and(|o| more[o]));
        }
        let name_of = |obj: u64| {
            let class = self.class_name(mem, obj).unwrap_or_default();
            self.object_name(mem, obj).filter(|n| !run_name(n)).unwrap_or(class)
        };
        // The group each one comes from at the top, for the order.
        let top = |mut g: usize| {
            while let Some(o) = groups[g].2 {
                g = o;
            }
            g
        };
        let mut order: Vec<usize> = (0..groups.len()).filter(|&g| !fields[g].is_empty()).collect();
        order.sort_by_key(|&g| {
            let (obj, role, owner) = groups[g];
            let attributes = self.class_of(mem, obj).is_some_and(|c| is_a(c, &["AttributeSet"]));
            // An owned actor's parts follow it; the top objects' parts follow the top.
            let near = match owner {
                _ if role == "owned" => g,
                Some(o) if groups[o].1 == "owned" => o,
                _ => top(g),
            };
            let kind = match role {
                "linked" => 3,
                _ if attributes => 0,
                "part" => 2,
                _ => 1,
            };
            (top(g), near, kind, g)
        });
        Ok(order
            .into_iter()
            .map(|g| {
                let (obj, role, owner) = groups[g];
                ListedObject {
                    name: name_of(obj),
                    class: self.class_name(mem, obj).unwrap_or_default(),
                    role,
                    owner: owner.map(|o| name_of(groups[o].0)),
                    more: more[g],
                    fields: std::mem::take(&mut fields[g]),
                }
            })
            .collect())
    }
}

/// The game's Unreal objects (found once per game, shared with the limits' thread); an error
/// for games that aren't Unreal (asked again after a while: the engine may still be starting).
pub fn shared(pid: u32, mem: &Mem, exe: &str, mods: impl FnOnce() -> Vec<Module>) -> Result<Arc<Ue>, String> {
    static FOUND: Mutex<Option<(u32, Result<Arc<Ue>, String>, std::time::Instant)>> = Mutex::new(None);
    let mut found = FOUND.lock().unwrap();
    if let Some((p, r, at)) = found.as_ref() {
        if *p == pid && (r.is_ok() || at.elapsed() < std::time::Duration::from_secs(30)) {
            return r.clone();
        }
    }
    let r = Ue::locate(pid, mem, &mods(), exe).map(Arc::new);
    *found = Some((pid, r.clone(), std::time::Instant::now()));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_read_back() {
        for text in [
            "ue:EHHandCraftingObject/PlayerContainer^BP_PersonCharacter_C.Items[Item=CarbonOreResource].Quantity",
            "ue:EHHandCraftingObject.ItemsLookupCounts{CarbonOreResource}",
            "ue:A.B[#3].C.D[=Some%20Name].E",
            "ue:EHHandCraftingObject.Items[Item=CarbonOreResource].Quantity+ItemsLookupCounts{CarbonOreResource}",
        ] {
            let p = UePath::parse(text).unwrap();
            assert_eq!(p.text(), text);
        }
        assert_eq!(
            UePath::parse("ue:EHHandCraftingObject/PlayerContainer^BP_PersonCharacter_C.Items[Item=CarbonOreResource].Quantity").unwrap().describe(),
            "Quantity of the Items whose Item is CarbonOreResource, in PlayerContainer (of a BP_PersonCharacter_C)"
        );
        assert!(UePath::parse("ue:A").is_none());
        assert!(UePath::parse("ue:A.B C").is_none());
    }

    #[test]
    fn run_names() {
        assert!(run_name("EHItemsContainer_2147479948"));
        assert!(!run_name("PlayerContainer"));
        assert!(!run_name("BP_InventoryComponent"));
        assert!(!run_name("Item_3"));
    }
}
