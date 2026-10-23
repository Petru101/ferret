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
// array (4.10 and older: chunks of 16384 UObject pointers, `ObjectList::Pointers`). Offsets move between minor versions (UStruct has FStructBaseChain from 4.21 or so,
// UProperty's Offset_Internal moved): every known form is tried. Epic's source, 4.13 and 4.22.
// The layout is checked on the engine's own structs: Guid (A, B, C, D: four ints), Transform
// (Quat, Vector, Vector) and Actor's Tags (an array of names), in each known form.

use std::collections::HashMap;
use crate::mem::Mem;
use std::sync::{Arc, Mutex};

use crate::helper::{maps, Region};
use crate::pointers::Module;

const CHUNK_ITEMS: u64 = 65536;
const OBJ_CLASS: u64 = 0x10;
const OBJ_NAME: u64 = 0x18;

/// Where a UObject keeps its header fields, and a pointer's width: 64-bit games, or 32-bit ones
/// (4-byte pointers: vtable, flags @4, InternalIndex @8, Class @0xC, Name @0x10, Outer @0x18).
#[derive(Clone, Copy, Debug)]
struct Header {
    w: u64,
    flags: u64,
    index: u64,
    class: u64,
    name: u64,
    outer: u64,
    /// An FUObjectItem in the chunked and the flat object lists (4.13 and later; 4.11-4.12's
    /// is a word shorter).
    item: u64,
}
const HEAD64: Header = Header { w: 8, flags: 0x8, index: 0xC, class: OBJ_CLASS, name: OBJ_NAME, outer: 0x20, item: 24 };
const HEAD32: Header = Header { w: 4, flags: 0x4, index: 0x8, class: 0xC, name: 0x10, outer: 0x18, item: 16 };
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

const LAYOUTS: [Layout; 8] = [
    // 4.25 to 5.3.
    FFIELD,
    // 5.4 and later: FField's Owner is one tagged pointer, everything after it 8 bytes earlier.
    Layout { name: 0x20, next: 0x18, dim: 0x30, size: 0x34, offset: 0x44, extra: 0x70, inner: 0x70, value: 0x78, ..FFIELD },
    // Minecraft Dungeons II.
    Layout { name: 0x20, next: 0x18, dim: 0x30, size: 0x34, offset: 0x48, extra: 0x78, inner: 0x80, value: 0x80, ..FFIELD },
    // UProperty games (4.11 to 4.24), the struct and property halves of 4.22 and Styx's 4.13 in
    // each combination. Seen: 4.13-4.17 `UPROPERTY_OLD`; 4.18 and 4.19 the fourth; 4.22
    // `UPROPERTY`; 4.8-4.10 the last.
    UPROPERTY,
    UPROPERTY_OLD,
    Layout { offset: 0x50, extra: 0x78, inner: 0x78, value: 0x80, ..UPROPERTY },
    Layout { offset: 0x44, extra: 0x70, inner: 0x70, value: 0x78, ..UPROPERTY_OLD },
    // Epic's 4.8-4.13 source as written: Offset_Internal 0x4C, 0x70 bytes (TwinStickShooterDemo,
    // 4.10).
    Layout { offset: 0x4C, extra: 0x70, inner: 0x70, value: 0x78, ..UPROPERTY_OLD },
];
/// 32-bit UProperty games, from Epic's source with 4-byte pointers: UField.Next @0x1C; UStruct
/// SuperStruct @0x20 (4.21 and older: no FStructBaseChain), Children @0x24, PropertiesSize
/// @0x28, MinAlignment @0x2C; UProperty ArrayDim @0x20, ElementSize @0x24, PropertyFlags @0x28,
/// Offset_Internal @0x3C (4.13) or @0x34 (4.18 on), 0x50 bytes.
const UPROPERTY32: Layout = Layout {
    sup: 0x20,
    props: 0x24,
    struct_size: 0x28,
    align: 0x2C,
    class: 0xC,
    class_name: 0x10,
    name: 0x10,
    next: 0x1C,
    dim: 0x20,
    size: 0x24,
    offset: 0x3C,
    extra: 0x50,
    inner: 0x50,
    value: 0x54,
};
/// 4.22 on: FStructBaseChain (a pointer and a count) before SuperStruct.
const UPROPERTY32_CHAIN: Layout = Layout { sup: 0x28, props: 0x2C, struct_size: 0x30, align: 0x34, ..UPROPERTY32 };
const LAYOUTS32: [Layout; 7] = [
    UPROPERTY32,
    // XCOW (4.17): the source's offset, but 0x54 bytes in all.
    Layout { extra: 0x54, inner: 0x54, value: 0x58, ..UPROPERTY32 },
    Layout { offset: 0x34, ..UPROPERTY32 },
    Layout { offset: 0x40, extra: 0x58, inner: 0x58, value: 0x5C, ..UPROPERTY32 },
    Layout { offset: 0x34, ..UPROPERTY32_CHAIN },
    UPROPERTY32_CHAIN,
    Layout { offset: 0x40, extra: 0x58, inner: 0x58, value: 0x5C, ..UPROPERTY32_CHAIN },
];

impl Layout {
    /// The same in a build with stats compiled in (development builds: Injected, a 4.15 jam
    /// game's NGJ17.exe): UObjectBase gets a TStatId (a pointer) after Outer, so everything a
    /// UObject keeps after its header is a pointer later (UStruct's fields; all of a UProperty's).
    const fn with_stats(self, h: Header) -> Layout {
        let prop = if self.class == h.class { h.w } else { 0 };
        let w = h.w;
        Layout {
            sup: self.sup + w,
            props: self.props + w,
            struct_size: self.struct_size + w,
            align: self.align + w,
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

/// A pointer-sized word.
fn word_at(mem: &Mem, at: u64, w: u64) -> Option<u64> {
    if w == 4 { u32_at(mem, at).map(u64::from) } else { u64_at(mem, at) }
}

/// Whether `p` can be a pointer `w` bytes wide, before reading anything.
fn ptr_ok(p: u64, w: u64) -> bool {
    p >= 0x10000 && p % w == 0 && p < if w == 4 { 1 << 32 } else { 1 << 47 }
}

fn ptr_w(mem: &Mem, at: u64, w: u64) -> Option<u64> {
    word_at(mem, at, w).filter(|&p| ptr_ok(p, w))
}

/// The static data around a candidate table, read once: words and ints by byte offset.
struct Raw<'a> {
    b: &'a [u8],
    w: usize,
}

impl Raw<'_> {
    fn p(&self, i: usize) -> u64 {
        match self.b.get(i..i + self.w) {
            Some(x) if self.w == 4 => u32::from_le_bytes(x.try_into().unwrap()) as u64,
            Some(x) => u64::from_le_bytes(x.try_into().unwrap()),
            None => 0,
        }
    }

    fn int(&self, i: usize) -> u64 {
        self.b.get(i..i + 4).map_or(0, |x| u32::from_le_bytes(x.try_into().unwrap()) as u64)
    }
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

/// How the game lists its objects.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ObjectList {
    /// FChunkedFixedUObjectArray (4.20 and later): a pointer to chunks of FUObjectItems, 65536 a
    /// chunk (66560 in 4.20: Shoot'n'Slaughter).
    Chunked(u64),
    /// FFixedUObjectArray (4.11 to 4.20): one array of FUObjectItems, 24 bytes each (16 in 4.11
    /// and 4.12: { Object, ClusterAndFlags, SerialNumber }, Epic's 4.12 source; Rollio 2D).
    Flat(u64),
    /// TStaticIndirectArrayThreadSafeRead<UObjectBase> (4.10 and older): { UObjectBase
    /// **Chunks[512]; i32 NumElements, NumChunks }, 16384 object pointers a chunk, all in the
    /// exe's static data (Epic's 4.8 and 4.10 UObjectArray.h).
    Pointers,
    /// TArray<UObjectBase*> (4.7 and older): { UObjectBase **Data; i32 Num, Max } in the exe's
    /// static data (Epic's 4.7 UObjectArray.h; Ghost Defender).
    Array,
}
/// Object pointers per chunk of `ObjectList::Pointers`, and its chunk slots.
const POINTER_CHUNK: u64 = 16384;
const POINTER_SLOTS: usize = 512;

/// Where the game keeps its objects and names (found once per attach).
pub struct Ue {
    h: Header,
    objects: u64,
    list: ObjectList,
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
        let h = if crate::pointers::pointer_width(pid, mem, mods, exe) == 4 { HEAD32 } else { HEAD64 };
        let w = h.w as usize;
        let (mut objects, mut flat, mut pointers, mut array, mut names) = (None, None, None, None, None);
        let mut words_of = Vec::new();
        for &(start, end) in &data {
            let mut buf = vec![0u8; (end - start) as usize];
            let Ok(n) = mem.read_at(&mut buf, start) else { continue };
            buf.truncate(n);
            for off in (0..n.saturating_sub(4 * w)).step_by(w) {
                let at = start + off as u64;
                let r = Raw { b: &buf[off..], w };
                if objects.is_none() {
                    objects = [CHUNK_ITEMS, 65 * 1024].into_iter().find(|&n| is_object_array(mem, &r, n, h)).map(|n| (at, n));
                }
                if objects.is_none() && flat.is_none() {
                    flat = [h.item, h.item - h.w].into_iter().find(|&item| is_flat_object_array(mem, &r, item, h)).map(|item| (at, item));
                }
                if objects.is_none() && flat.is_none() && pointers.is_none() && is_pointer_chunks(mem, &r, h) {
                    pointers = Some(at);
                }
                if objects.is_none() && flat.is_none() && pointers.is_none() && array.is_none() && is_pointer_array(mem, &r, h) {
                    array = Some(at);
                }
                if names.is_none() && off >= 8 && is_name_blocks(mem, &Raw { b: &buf[off - 8..], w }) {
                    names = Some(Names::Pool(at));
                }
            }
            words_of.push(buf);
        }
        // TNameEntryArray is on the heap: a pointer to it among the static data. Looked for only
        // when there's no name pool (one more read for every heap pointer there).
        if names.is_none() {
            let (lo, hi) = data.iter().fold((u64::MAX, 0), |(lo, hi), &(s, e)| (lo.min(s), hi.max(e)));
            let words = words_of.iter().flat_map(|b| (0..b.len() / w).map(move |i| Raw { b, w }.p(i * w)));
            names = words.filter(|&p| (p < lo || p >= hi) && ptr_ok(p, h.w)).find_map(|p| name_array(mem, p, h));
        }
        let list = [
            objects.map(|(o, n)| (o, ObjectList::Chunked(n))),
            flat.map(|(o, item)| (o, ObjectList::Flat(item))),
            pointers.map(|o| (o, ObjectList::Pointers)),
            array.map(|o| (o, ObjectList::Array)),
        ];
        let (objects, list) = list.into_iter().flatten().next().ok_or("no Unreal object list found")?;
        let names_at = names.ok_or("no Unreal name pool found")?;
        let mut ue = Ue { h, objects, list, names_at, layout: LAYOUTS[0], names: Mutex::default(), props: Mutex::default(), containers: Mutex::default() };
        // The first layout that reads all three right; else the first that reads Guid right (all
        // a layout had to pass before Transform and Tags were checked: no game that worked
        // then loses it).
        // The error to tell: the layout that read the most of Guid.
        let (mut why, mut fallback) = ((0, String::new()), None);
        let layouts: &[Layout] = if h.w == 4 { &LAYOUTS32 } else { &LAYOUTS };
        for layout in layouts.iter().flat_map(|l| [*l, l.with_stats(h)]) {
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
        let Some(guid) = find("Guid", "ScriptStruct") else {
            let seen: Vec<String> = objects.iter().take(4).map(|&o| format!("{:?} of {:?}", self.object_name(mem, o), self.class_name(mem, o))).collect();
            return Err((0, format!("no Guid struct among the {} objects (another engine version?; {:?} at 0x{:x}), first: {}", objects.len(), self.list, self.objects, seen.join(", "))));
        };
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
        let w = self.h.w;
        let at = match self.list {
            ObjectList::Chunked(_) => 2 * w + 4,
            ObjectList::Flat(_) => w + 4,
            ObjectList::Pointers => w * POINTER_SLOTS as u64,
            ObjectList::Array => w,
        };
        i32_at(mem, self.objects + at).unwrap_or(0).max(0) as usize
    }

    /// Every object alive now.
    pub fn objects(&self, mem: &Mem) -> Vec<u64> {
        let (num, w) = (self.object_count(mem) as u64, self.h.w);
        // Every `stride` bytes from `at`, `n` times: the object pointer that starts each.
        let firsts = |at: u64, n: u64, stride: u64, out: &mut Vec<u64>| {
            let mut buf = vec![0u8; (n * stride) as usize];
            let Ok(got) = mem.read_at(&mut buf, at) else { return };
            let raw = Raw { b: &buf[..got], w: w as usize };
            out.extend((0..got as u64 / stride).map(|i| raw.p((i * stride) as usize)).filter(|&p| p != 0));
        };
        let mut out = Vec::with_capacity(num as usize);
        match self.list {
            ObjectList::Pointers => {
                for c in 0..num.div_ceil(POINTER_CHUNK) {
                    if let Some(chunk) = self.ptr(mem, self.objects + w * c) {
                        firsts(chunk, (num - c * POINTER_CHUNK).min(POINTER_CHUNK), w, &mut out);
                    }
                }
            }
            ObjectList::Array => firsts(self.ptr(mem, self.objects).unwrap_or(0), num, w, &mut out),
            ObjectList::Flat(item) => firsts(self.ptr(mem, self.objects).unwrap_or(0), num, item, &mut out),
            ObjectList::Chunked(per) => {
                let Some(table) = self.ptr(mem, self.objects) else { return out };
                for c in 0..num.div_ceil(per) {
                    if let Some(chunk) = self.ptr(mem, table + w * c) {
                        firsts(chunk, (num - c * per).min(per), self.h.item, &mut out);
                    }
                }
            }
        }
        out
    }

    /// A pointer in the game, as wide as its pointers.
    fn ptr(&self, mem: &Mem, at: u64) -> Option<u64> {
        ptr_w(mem, at, self.h.w)
    }

    /// A pointer-sized word (0 for none).
    fn word(&self, mem: &Mem, at: u64) -> Option<u64> {
        word_at(mem, at, self.h.w)
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
                let chunk = self.ptr(mem, table + self.h.w * (index as u64 / NAME_CHUNK))?;
                let entry = self.ptr(mem, chunk + self.h.w * (index as u64 % NAME_CHUNK))?;
                let wide = u32_at(mem, entry + at)? & 1 == 1;
                return entry_text(mem, entry + chars, wide);
            }
        };
        let block = self.ptr(mem, blocks + self.h.w * (index >> 16) as u64)?;
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
        self.fname(mem, obj + self.h.name)
    }

    pub fn class_of(&self, mem: &Mem, obj: u64) -> Option<u64> {
        self.ptr(mem, obj + self.h.class)
    }

    pub fn class_name(&self, mem: &Mem, obj: u64) -> Option<String> {
        self.object_name(mem, self.class_of(mem, obj)?)
    }

    pub fn outer(&self, mem: &Mem, obj: u64) -> Option<u64> {
        self.ptr(mem, obj + self.h.outer)
    }

    pub fn struct_size(&self, mem: &Mem, st: u64) -> u64 {
        i32_at(mem, st + self.layout.struct_size).unwrap_or(0).max(0) as u64
    }

    /// The struct or class `st` derives from.
    pub fn super_of(&self, mem: &Mem, st: u64) -> Option<u64> {
        self.ptr(mem, st + self.layout.sup)
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
            let mut f = self.ptr(mem, s + self.layout.props);
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
                f = self.ptr(mem, field + self.layout.next);
            }
        }
        let out = Arc::new(out);
        self.props.lock().unwrap().insert(st, out.clone());
        out
    }

    /// The property described by the FField at `field`.
    pub fn prop(&self, mem: &Mem, field: u64) -> Option<Prop> {
        let l = self.layout;
        let kind = self.fname(mem, self.ptr(mem, field + l.class)? + l.class_name)?;
        let extra = if matches!(kind.as_str(), "ArrayProperty" | "SetProperty") { l.inner } else { l.extra };
        Some(Prop {
            name: self.fname(mem, field + l.name)?,
            offset: i32_at(mem, field + l.offset)?.max(0) as u64,
            size: i32_at(mem, field + l.size)?.max(0) as u64,
            dim: i32_at(mem, field + l.dim)?.max(1) as u64,
            extra: self.word(mem, field + extra).unwrap_or(0),
            extra2: self.word(mem, field + l.value).unwrap_or(0),
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

/// The first `n` objects of a list in order: their indices (InternalIndex) are 0, 1, ...
fn indexed(mem: &Mem, h: Header, objects: impl IntoIterator<Item = Option<u64>>) -> bool {
    objects.into_iter().enumerate().all(|(i, o)| o.is_some_and(|o| u32_at(mem, o + h.index) == Some(i as u32)))
}

/// FChunkedFixedUObjectArray: { Objects (chunk table), PreAllocatedObjects, i32 Max, Num,
/// MaxChunks, NumChunks }: counts that fit together with `per` objects a chunk, and its first
/// two objects with indices 0 and 1.
fn is_object_array(mem: &Mem, r: &Raw, per: u64, h: Header) -> bool {
    let w = r.w;
    let (max, num, max_chunks, num_chunks) = (r.int(2 * w), r.int(2 * w + 4), r.int(2 * w + 8), r.int(2 * w + 12));
    if num == 0 || num > max || max > 1 << 26 || num_chunks == 0 {
        return false;
    }
    if max_chunks != max.div_ceil(per) || num_chunks != num.div_ceil(per) {
        return false;
    }
    let Some(chunk) = Some(r.p(0)).filter(|&p| ptr_ok(p, h.w)).and_then(|t| ptr_w(mem, t, h.w)) else { return false };
    indexed(mem, h, [ptr_w(mem, chunk, h.w), ptr_w(mem, chunk + h.item, h.w)])
}

/// FFixedUObjectArray (4.20 and older): { items, i32 max, i32 num }, its first two objects
/// with indices 0 and 1, the items `item` bytes apart.
fn is_flat_object_array(mem: &Mem, r: &Raw, item: u64, h: Header) -> bool {
    let (items, max, num) = (r.p(0), r.int(r.w), r.int(r.w + 4));
    if num < 2 || num > max || max > 1 << 26 || !ptr_ok(items, h.w) {
        return false;
    }
    indexed(mem, h, [ptr_w(mem, items, h.w), ptr_w(mem, items + item, h.w)])
}

/// The object list of 4.10 and older (`ObjectList::Pointers`): as many chunk pointers as its
/// count needs, then none up to its 512 slots, then the counts; its first two objects with
/// indices 0 and 1.
fn is_pointer_chunks(mem: &Mem, r: &Raw, h: Header) -> bool {
    let w = r.w;
    if r.b.len() < POINTER_SLOTS * w + 8 || !ptr_ok(r.p(0), h.w) {
        return false;
    }
    let (num, chunks) = (r.int(POINTER_SLOTS * w), r.int(POINTER_SLOTS * w + 4) as usize);
    if num < 2 || num > 1 << 26 || chunks == 0 || chunks as u64 != num.div_ceil(POINTER_CHUNK) {
        return false;
    }
    if (0..chunks).any(|c| !ptr_ok(r.p(c * w), h.w)) || (chunks..POINTER_SLOTS).any(|c| r.p(c * w) != 0) {
        return false;
    }
    indexed(mem, h, [ptr_w(mem, r.p(0), h.w), ptr_w(mem, r.p(0) + h.w, h.w)])
}

/// The object list of 4.7 and older (`ObjectList::Array`): TArray { data, i32 num, i32 max },
/// its first three objects with indices 0, 1 and 2. A plain TArray is common: it must hold at
/// least 1000 objects (Ghost Defender: 21918; a stray one in Shoot'n'Slaughter had 6).
fn is_pointer_array(mem: &Mem, r: &Raw, h: Header) -> bool {
    let (data, num, max) = (r.p(0), r.int(r.w), r.int(r.w + 4));
    if num < 1000 || num > max || max > 1 << 26 || !ptr_ok(data, h.w) {
        return false;
    }
    indexed(mem, h, (0..3).map(|i| ptr_w(mem, data + h.w * i, h.w)))
}

/// TNameEntryArray at `p` (4.22 and older): { FNameEntry **Chunks[N]; i32 NumElements, NumChunks }
/// with N = 128 (4.13: 2M names) or 256 (4.22: 4M), chunks as many as the count needs, and its
/// first entries "None" and "ByteProperty", in either entry layout: { HashNext, Index, chars }
/// (4.22) or { Index, HashNext, chars } (4.13).
fn name_array(mem: &Mem, p: u64, h: Header) -> Option<Names> {
    let w = h.w;
    let chunk = ptr_w(mem, p, w)?;
    for slots in [128u64, 256] {
        let (Some(num), Some(chunks)) = (i32_at(mem, p + w * slots), i32_at(mem, p + w * slots + 4)) else { continue };
        let chunks = chunks as u64;
        if num < 2 || chunks == 0 || chunks > slots || chunks != (num as u64).div_ceil(NAME_CHUNK) {
            continue;
        }
        if ptr_w(mem, p + w * (chunks - 1), w).is_none() || (chunks < slots && word_at(mem, p + w * chunks, w) != Some(0)) {
            continue;
        }
        let (Some(none), Some(byte)) = (ptr_w(mem, chunk, w), ptr_w(mem, chunk + w, w)) else { continue };
        for (index, chars) in [(w, w + 4), (0, 2 * w)] {
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

/// FNamePool's blocks, after CurrentBlock and CurrentByteCursor (where `r` starts): as many as
/// the current block says, and the first starts with "None" then "ByteProperty".
fn is_name_blocks(mem: &Mem, r: &Raw) -> bool {
    let (current, cursor) = (r.int(0) as usize, r.int(4));
    if current >= 8192 || cursor == 0 || cursor > 0x20000 || r.b.len() < 8 + (current + 2) * r.w {
        return false;
    }
    let block = |i: usize| r.p(8 + i * r.w);
    if (0..=current).any(|i| block(i) == 0) || block(current + 1) != 0 {
        return false;
    }
    let mut b = [0u8; 20];
    if block(0) % 4 != 0 || mem.read_exact_at(&mut b, block(0)).is_err() {
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
                self.word(mem, at).map(|o| if o == 0 { "null".into() } else { format!("0x{o:x} {}", self.object_name(mem, o).unwrap_or_default()) })
            }
            "StructProperty" => self.object_name(mem, p.extra).map(|s| format!("<{s}>")),
            "ArrayProperty" | "MapProperty" | "SetProperty" => {
                let data = self.word(mem, at).unwrap_or(0);
                let num = i32_at(mem, at + self.h.w).unwrap_or(0);
                Some(format!("0x{data:x} [{num}]"))
            }
            _ => None,
        };
        shown.unwrap_or_else(|| "?".into())
    }

    /// An FString's text (TArray of UTF-16 units, the last one 0).
    pub fn fstring(&self, mem: &Mem, at: u64) -> Option<String> {
        let data = self.word(mem, at)?;
        let num = i32_at(mem, at + self.h.w)?;
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
                "objects 0x{:x} ({:?}), names {:x?}, layout {:x?}, {} objects",
                self.objects,
                self.list,
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

    /// An array's or a map's data pointer and element count (nothing when empty), pointers `w`
    /// bytes wide.
    fn head(&self, mem: &Mem, at: u64, w: u64) -> Option<(u64, u64)> {
        let mut h = [0u8; 12];
        let len = w as usize + 4;
        match at.checked_sub(self.base).filter(|o| o + len as u64 <= self.bytes.len() as u64) {
            Some(o) => h[..len].copy_from_slice(&self.bytes[o as usize..][..len]),
            None => mem.read_exact_at(&mut h[..len], at).ok()?,
        }
        let raw = Raw { b: &h, w: w as usize };
        let (data, num) = (raw.p(0), raw.int(w as usize) as i32);
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
        u32_at(mem, obj + self.h.flags).is_none_or(|f| f & TEMPLATE_FLAGS != 0)
    }

    fn find_prop(&self, mem: &Mem, st: u64, name: &str) -> Option<Prop> {
        self.props(mem, st).iter().find(|p| p.name == name).cloned()
    }

    /// How a property's value reads as a key: an object's name, a name, a string, a number.
    fn key_text(&self, mem: &Mem, at: u64, p: &Prop) -> Option<String> {
        match p.kind.as_str() {
            "ObjectProperty" | "ClassProperty" => self.object_name(mem, self.ptr(mem, at)?),
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
        let (Some(data), Some(num)) = (self.word(mem, at), i32_at(mem, at + self.h.w)) else { return Vec::new() };
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
                        let o = Raw { b: &buf, w: self.h.w as usize }.p((i * inner.size) as usize);
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
        let w = self.h.w;
        let (Some(data), Some(num)) = (self.word(mem, at), i32_at(mem, at + w)) else { return Vec::new() };
        if data == 0 || num <= 0 || num as u64 > MAX_ELEMENTS {
            return Vec::new();
        }
        // After the elements' TArray: the allocation bits, 4 ints inline, then a pointer to them
        // when there are more.
        let inline = at + w + 8;
        let secondary = self.word(mem, inline + 16).unwrap_or(0);
        let bits_at = if secondary != 0 { secondary } else { inline };
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
                let o = self.ptr(mem, at)?;
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
                    let Some((data, num)) = snap.head(mem, start, self.h.w) else { continue };
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
                    let Some((data, num)) = snap.head(mem, start, self.h.w) else { continue };
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
            if mem.read_exact_at(&mut head[..(self.h.class + self.h.w) as usize], obj).is_err() {
                continue;
            }
            let raw = Raw { b: &head, w: self.h.w as usize };
            let (flags, class) = (raw.int(self.h.flags as usize) as u32, raw.p(self.h.class as usize));
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
            "ObjectProperty" if links => self.links.extend(self.ue.ptr(mem, start)),
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
        self.ptr(mem, obj + p.offset).filter(|&o| self.class_of(mem, o).is_some() && !self.is_template(mem, o))
    }

    /// The player's numbers, by object: their character first, then the actors it owns, the
    /// player state, the controller, the game, and the objects their fields point to.
    pub fn player(&self, mem: &Mem) -> Result<Vec<ListedObject>, NoPlayer> {
        // Every live object's class and outer, read once.
        let mut heads: Vec<(u64, u64, u64)> = Vec::new();
        for obj in self.objects(mem) {
            let mut head = [0u8; 0x28];
            if mem.read_exact_at(&mut head[..(self.h.outer + self.h.w) as usize], obj).is_err() {
                continue;
            }
            let raw = Raw { b: &head, w: self.h.w as usize };
            let (flags, class) = (raw.int(self.h.flags as usize) as u32, raw.p(self.h.class as usize));
            if flags & TEMPLATE_FLAGS == 0 && class != 0 {
                heads.push((obj, class, raw.p(self.h.outer as usize)));
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
                let Some(o) = self.ptr(mem, pawn + p.offset).filter(|&o| o != pawn && !found.contains(&o)) else { continue };
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
                let by = self.ptr(mem, obj + owner.offset).and_then(|o| index.get(&o).copied()).filter(|&i| groups[i].2.is_none());
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
