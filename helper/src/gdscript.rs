// Godot 3 script variables: a value kept in a variable of a GDScript object (Brotato's
// materials: the `gold` of the PlayerRunData in the RunData singleton's `players_data`). The
// game replaces such objects as it runs (Brotato at every wave, keeping the old one around as a
// snapshot), the interpreter is shared code, and pointer paths go through run-time memory; the
// scripts' paths and their variables' names stay.
// Memory (64-bit builds; the offsets are found on the game itself, `Layout`): an Object points
// to its ScriptInstance; a GDScriptInstance points to its owner Object and its GDScript and
// keeps the variables as a Vector<Variant> (a pointer to the first one; the count as a u32
// 4 bytes before it). A Variant is { u32 type; 4 bytes; 16 bytes of data }: 2 = int (int64),
// 3 = real (double), 17 = Object (data: a pointer, for References the second word),
// 19 = Array (data: a pointer to { refcount; Vector<Variant> } with the Vector's data pointer
// at +0x10). The GDScript keeps its path ("res://singletons/run_data.gd", wchar_t: UTF-16 on
// Windows, UTF-32 on Linux) and member_indices, a Map<StringName, MemberInfo> (red-black tree:
// { root, nil, size }, elements { color, right, left, parent, next, prev, key, value }, the
// index first in the value); a StringName points to { refcount, const char *cname, String name }.
// Godot 4 (found on Godot 4.7 and the 4.6 source): strings are UTF-32 everywhere, Vectors and
// Strings keep their size as a u64 8 bytes before the data (4.3, 4.4) or 16 bytes before it,
// after the capacity (4.5+; a u32 just before it up to 4.2, as in Godot 3), member_indices is a
// HashMap ({ elements, hashes, head, tail, u32 capacity index, u32 size }; elements { next,
// prev, key, value }, in insertion order from head), a StringName's String is at +8 (4.5+;
// earlier a C string there and the String at +0x10), Object is Variant type 24 and Array 28,
// and a GDScriptInstance keeps its owner's ObjectID before the owner.
// Text form: gd:singletons/run_data.gd.players_data[0].gold = the variable "gold" of element 0
// of the variable "players_data" of the (only) object running res://singletons/run_data.gd.
// gd:entities/enemy.gd@/root/Main/Boss.current_stats.health = the same from the object running
// enemy.gd at that place in the scene tree (for scripts many objects run).
// The scene tree: a Node points to its parent and keeps its children (Godot 3: a Vector<Node *>,
// Godot 4: a HashMap<StringName, Node *>) and its name (a StringName); the root is named
// "root". Learned from any node Ferret meets, it finds objects again by pointer reads.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::names::Heap;
use crate::helper::Kind;

const VARIANT: u64 = 24;
const INT: u32 = 2;
const REAL: u32 = 3;
/// Variant types in Godot 3 and in Godot 4.
const OBJECT: [u32; 2] = [17, 24];
const ARRAY: [u32; 2] = [19, 28];
/// Where a Vector's or String's size is, in bytes before its data: a u32 right before it
/// (Godot 3, 4.0-4.2), a u64 8 bytes before (4.3, 4.4) or 16 bytes before (4.5+).
const SIZES: [u64; 3] = [4, 8, 16];
/// Variables per script and elements per array Ferret looks through.
const MAX_MEMBERS: u64 = 1024;
const MAX_ELEMENTS: u64 = 1 << 16;
/// Bytes of a GDScript object searched for its path and its member table.
const SCRIPT_SIZE: u64 = 0x1000;
/// Objects above the value's own.
const MAX_UP: usize = 4;
/// A path leading to more places than this is too loose to write through.
const MAX_PLACES: usize = 16;
const MAX_NAME: usize = 128;

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum Step {
    Member(String),
    Index(u64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScriptPath {
    /// The script's path without "res://".
    script: String,
    /// Only the object at this place in the scene tree (names below /root; None = any child,
    /// for names the engine made up, "@Node@12"): for a script many objects run (enemy.gd and
    /// the boss among them).
    nodes: Option<Vec<Option<String>>>,
    steps: Vec<Step>,
}

fn good_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= MAX_NAME && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn good_script(s: &str) -> bool {
    s.ends_with(".gd") && s.len() <= 4 * MAX_NAME && s.chars().all(|c| c.is_ascii_alphanumeric() || "_-/.".contains(c))
}

impl ScriptPath {
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix("gd:")?;
        let (script, nodes, steps_text) = match rest.split_once('@') {
            Some((script, tail)) => {
                let (place, steps) = tail.split_once('.')?;
                let node = |n: &str| match n {
                    "*" => Some(None),
                    _ => decode(n).filter(|n| good_node(n)).map(Some),
                };
                let nodes = match place.strip_prefix("/root")? {
                    "" => Vec::new(),
                    below => below.strip_prefix('/')?.split('/').map(node).collect::<Option<_>>()?,
                };
                (script, Some(nodes), steps)
            }
            None => {
                let at = rest.find(".gd.")? + 3;
                (&rest[..at], None, &rest[at + 1..])
            }
        };
        let mut steps = Vec::new();
        for part in steps_text.split('.') {
            let (name, mut idx) = part.split_once('[').map_or((part, ""), |(n, i)| (n, i));
            if !good_name(name) {
                return None;
            }
            steps.push(Step::Member(name.to_owned()));
            while !idx.is_empty() {
                let (n, tail) = idx.split_once(']')?;
                steps.push(Step::Index(n.parse().ok()?));
                idx = tail.strip_prefix('[').unwrap_or(tail);
                if !tail.is_empty() && !tail.starts_with('[') {
                    return None;
                }
            }
        }
        (good_script(script) && matches!(steps.last(), Some(Step::Member(_)))).then(|| ScriptPath { script: script.to_owned(), nodes, steps })
    }

    /// What tells the objects it starts from: the script and the place.
    fn roots_key(&self) -> String {
        format!("{}{}", self.script, self.nodes.as_ref().map(|n| format!("@{}", place_text(n))).unwrap_or_default())
    }

    pub fn text(&self) -> String {
        let place = self.nodes.as_ref().map(|n| format!("@{}", place_text(n))).unwrap_or_default();
        format!("gd:{}{place}{}", self.script, self.steps_text())
    }

    fn steps_text(&self) -> String {
        self.steps
            .iter()
            .map(|s| match s {
                Step::Member(n) => format!(".{n}"),
                Step::Index(i) => format!("[{i}]"),
            })
            .collect()
    }

    /// In words: "players_data[0].gold in run_data.gd".
    pub fn describe(&self) -> String {
        let file = self.script.rsplit('/').next().unwrap_or(&self.script);
        let place = self.nodes.as_ref().map(|n| format!(" at {}", place_words(n))).unwrap_or_default();
        format!("{} in {file}{place}", &self.steps_text()[1..])
    }
}

/// "/root/Main/*" (names percent-encoded).
fn place_text(nodes: &[Option<String>]) -> String {
    let below: String = nodes.iter().map(|n| format!("/{}", n.as_deref().map_or("*".to_owned(), encode))).collect();
    format!("/root{below}")
}

/// "/root/Main/Big Boss", for people.
fn place_words(nodes: &[Option<String>]) -> String {
    let below: String = nodes.iter().map(|n| format!("/{}", n.as_deref().unwrap_or("*"))).collect();
    format!("/root{below}")
}

/// A node name as Godot allows it (no "." ":" "@" "/" "%" '"'), not made up by the engine.
fn good_node(n: &str) -> bool {
    !n.is_empty() && n.len() <= MAX_NAME && !n.chars().any(|c| c.is_control() || ".:@/%\"".contains(c))
}

fn encode(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else {
            let mut b = [0u8; 4];
            for byte in c.encode_utf8(&mut b).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
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
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Where the engine keeps things, as found on the running game.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Layout {
    /// GDScriptInstance -> its Object, its GDScript, its variables.
    owner: u64,
    script: u64,
    members: u64,
    /// Object -> its ScriptInstance.
    instance: u64,
    /// GDScript -> member_indices, its path's characters.
    indices: u64,
    path: u64,
    /// Bytes per character: 2 (Godot 3 on Windows) or 4.
    wide: u64,
    /// Where sizes are (`SIZES`).
    cow: u64,
    /// Godot 4: member_indices is a HashMap, Variant types are Godot 4's.
    v4: bool,
}

/// The layout found last, by process.
static LAYOUT: Mutex<Option<(u32, Layout)>> = Mutex::new(None);

/// Script instances found lately in memory, by process and script (and place): values of one
/// script restored together share one search (Lumencraft's hp and stamina in Player.gd: ~5 s
/// each on 2 GB).
static FOUND: Mutex<Vec<(u32, String, Instant, Vec<u64>)>> = Mutex::new(Vec::new());
const FOUND_FOR: Duration = Duration::from_secs(10);
/// None found (between Brotato's waves, no player): soon stale, a wave starts any moment.
const NONE_FOUND_FOR: Duration = Duration::from_secs(2);
/// The GDScript objects of each script path found before, per process: a script stays loaded
/// while objects running it come and go (Brotato's player, every wave), so finding the new
/// ones takes one pass over memory instead of three.
static SCRIPTS: Mutex<Vec<(u32, String, Vec<u64>)>> = Mutex::new(Vec::new());

/// Where a Node keeps its place in the scene tree, found on the running game: its parent, its
/// children (Godot 3: a Vector<Node *>, `children` = where its data pointer is; Godot 4: a
/// HashMap<StringName, Node *>) and its name (a StringName).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Tree {
    parent: u64,
    children: u64,
    map: bool,
    name: u64,
}

/// The scene tree's root (the main Viewport or Window, named "root") and the layout, by
/// process: the root lives as long as the game, so objects in the tree are found again by
/// pointer reads, not passes over memory.
static TREE: Mutex<Option<(u32, u64, Tree)>> = Mutex::new(None);
/// Scripts whose objects were found in the scene tree, by process: they are looked for there
/// only (Brotato's player is gone during the shop; searching memory for it cost a pass).
static IN_TREE: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());
/// Where each script's objects were found in the tree last (their parents), by process, and
/// when the whole tree was looked through for them.
static HOMES: Mutex<Vec<(u32, String, Instant, Vec<u64>)>> = Mutex::new(Vec::new());
const WHOLE_TREE_EVERY: Duration = Duration::from_secs(1);
const MAX_CHILDREN: u64 = 1 << 16;
const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 1 << 17;

struct Godot<'a> {
    heap: &'a Heap<'a>,
    lay: Layout,
    /// member_indices by script: name -> index.
    indices: RefCell<HashMap<u64, Option<HashMap<String, u64>>>>,
}

fn mem_u32(mem: &File, at: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    mem.read_exact_at(&mut b, at).ok()?;
    Some(u32::from_le_bytes(b))
}

fn mem_u64(mem: &File, at: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    mem.read_exact_at(&mut b, at).ok()?;
    Some(u64::from_le_bytes(b))
}

/// The size of the Vector or String whose data is at `data`.
fn cow_size(mem: &File, data: u64, cow: u64) -> Option<u64> {
    match cow {
        4 => mem_u32(mem, data.checked_sub(4)?).map(u64::from),
        _ => mem_u64(mem, data.checked_sub(cow)?),
    }
}

/// A Godot string's characters at `p` (its size counts the terminating 0).
fn read_chars(mem: &File, p: u64, wide: u64, cow: u64) -> Option<String> {
    let n = cow_size(mem, p, cow)?;
    if !(2..=4 * MAX_NAME as u64).contains(&n) {
        return None;
    }
    let mut b = vec![0u8; ((n - 1) * wide) as usize];
    mem.read_exact_at(&mut b, p).ok()?;
    match wide {
        2 => String::from_utf16(&b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()).ok(),
        _ => b.chunks(4).map(|c| char::from_u32(u32::from_le_bytes([c[0], c[1], c[2], c[3]]))).collect(),
    }
}

/// The entries of a Godot 4 HashMap at `map`: (key, address of the value), in order.
fn hash_entries(mem: &File, map: u64, most: u64) -> Option<Vec<(u64, u64)>> {
    let (head, tail) = (mem_u64(mem, map + 0x10)?, mem_u64(mem, map + 0x18)?);
    let (capacity, size) = (mem_u32(mem, map + 0x20)?, mem_u32(mem, map + 0x24)? as u64);
    if head == 0 || tail == 0 || capacity > 40 || !(1..=most).contains(&size) {
        return None;
    }
    let (mut out, mut e, mut prev) = (Vec::new(), head, 0);
    while e != 0 {
        if out.len() as u64 >= size || mem_u64(mem, e + 8)? != prev {
            return None;
        }
        out.push((mem_u64(mem, e + 0x10)?, e + 0x18));
        prev = e;
        e = mem_u64(mem, e)?;
    }
    (out.len() as u64 == size && prev == tail).then_some(out)
}

/// The entries of a Map at `map` ({ root, nil, size }): (key, address of the value), in order.
fn map_entries(mem: &File, map: u64, most: u64) -> Option<Vec<(u64, u64)>> {
    let (root, nil, size) = (mem_u64(mem, map)?, mem_u64(mem, map + 8)?, mem_u32(mem, map + 16)? as u64);
    if root == 0 || nil == 0 || root == nil || !(1..=most).contains(&size) {
        return None;
    }
    let mut e = mem_u64(mem, root + 0x10)?;
    for _ in 0..64 {
        match mem_u64(mem, e + 0x10)? {
            l if l == nil => break,
            l => e = l,
        }
    }
    let mut out = Vec::new();
    while e != nil && e != 0 {
        if out.len() as u64 >= size {
            return None;
        }
        out.push((mem_u64(mem, e + 0x30)?, e + 0x38));
        e = mem_u64(mem, e + 0x20)?;
    }
    (out.len() as u64 == size).then_some(out)
}

/// A StringName's text: its String (at +0x10, or +8 in Godot 4.5+), else its C string.
fn string_name(mem: &File, sn: u64, wide: u64, cow: u64) -> Option<String> {
    string_name_if(mem, sn, wide, cow, good_name)
}

fn string_name_if(mem: &File, sn: u64, wide: u64, cow: u64, good: impl Fn(&str) -> bool) -> Option<String> {
    for at in [0x10, 8] {
        if let Some(t) = mem_u64(mem, sn + at).filter(|&p| p != 0).and_then(|p| read_chars(mem, p, wide, cow)) {
            if good(&t) {
                return Some(t);
            }
        }
    }
    let c = mem_u64(mem, sn + 8).filter(|&p| p != 0)?;
    let mut b = [0u8; MAX_NAME];
    let n = mem.read_at(&mut b, c).ok()?;
    let t = std::str::from_utf8(b[..n].split(|&x| x == 0).next()?).ok()?;
    good(t).then(|| t.to_owned())
}

/// member_indices at `map`: every index 0..n-1 once, each under a name.
fn member_table(mem: &File, map: u64, lay: &Layout) -> Option<HashMap<String, u64>> {
    let entries = match lay.v4 {
        true => hash_entries(mem, map, MAX_MEMBERS)?,
        false => map_entries(mem, map, MAX_MEMBERS)?,
    };
    let mut seen = vec![false; entries.len()];
    let mut out = HashMap::new();
    for (key, value) in entries {
        let i = mem_u32(mem, value)? as usize;
        if i >= seen.len() || seen[i] {
            return None;
        }
        seen[i] = true;
        out.insert(string_name(mem, key, lay.wide, lay.cow)?, i as u64);
    }
    Some(out)
}

/// The layout, when `inst` is a GDScriptInstance whose variables are at `members` (`count` of
/// them, the size found `cow` bytes before them): its owner points back to it, its script has a
/// member table of `count` names and a path.
fn detect(heap: &Heap, inst: u64, members: u64, count: u64, cow: u64) -> Option<Layout> {
    let mem = heap.file();
    let m = (8..0x40).step_by(8).find(|&m| mem_u64(mem, inst + m) == Some(members))?;
    for owner in [8u64, 0x10, 0x18].into_iter().filter(|&o| o < m) {
        let Some(obj) = mem_u64(mem, inst + owner).filter(|&p| heap.is_pointer(p)) else { continue };
        let Some(instance) = (8..0x100).step_by(8).find(|&k| mem_u64(mem, obj + k) == Some(inst)) else { continue };
        for script in [8u64, 0x10, 0x18].into_iter().filter(|&s| s != owner && s < m) {
            let Some(gd) = mem_u64(mem, inst + script).filter(|&p| heap.is_pointer(p)) else { continue };
            let lay = Layout { owner, script, members: m, instance, indices: 0, path: 0, wide: 0, cow, v4: false };
            if let Some(lay) = detect_script(heap, gd, count, lay) {
                return Some(lay);
            }
        }
    }
    None
}

/// The rest of the layout from a GDScript: its path's characters and member table.
fn detect_script(heap: &Heap, gd: u64, count: u64, lay: Layout) -> Option<Layout> {
    let mem = heap.file();
    for wide in [2u64, 4] {
        let Some(path) = (0..SCRIPT_SIZE).step_by(8).find(|&p| {
            mem_u64(mem, gd + p)
                .filter(|&c| heap.is_pointer(c))
                .and_then(|c| read_chars(mem, c, wide, lay.cow))
                .is_some_and(|t| t.strip_prefix("res://").is_some_and(good_script))
        }) else {
            continue;
        };
        // A Map's size is at +16, a HashMap's at +0x24.
        for (v4, size) in [(false, 16), (true, 0x24)] {
            let lay = Layout { path, wide, v4, ..lay };
            let indices = (0..SCRIPT_SIZE).step_by(8).find(|&j| {
                mem_u32(mem, gd + j + size) == Some(count as u32) && member_table(mem, gd + j, &lay).is_some_and(|t| t.len() as u64 == count)
            });
            if let Some(indices) = indices {
                return Some(Layout { indices, ..lay });
            }
        }
        return None;
    }
    None
}

impl<'a> Godot<'a> {
    fn new(heap: &'a Heap<'a>, lay: Layout) -> Self {
        Godot { heap, lay, indices: RefCell::default() }
    }

    fn mem(&self) -> &File {
        self.heap.file()
    }

    fn u32_at(&self, at: u64) -> Option<u32> {
        mem_u32(self.mem(), at)
    }

    fn ptr(&self, at: u64) -> Option<u64> {
        mem_u64(self.mem(), at).filter(|&p| self.heap.is_pointer(p))
    }

    fn size(&self, data: u64) -> Option<u64> {
        cow_size(self.mem(), data, self.lay.cow)
    }

    /// The Variants of a Vector: (first, count).
    fn vector(&self, data: u64, most: u64) -> Option<(u64, u64)> {
        let n = self.size(data)?;
        (n >= 1 && n <= most).then_some((data, n))
    }

    /// The script instance of the Object at `obj`, when it runs a GDScript.
    fn instance_of(&self, obj: u64) -> Option<u64> {
        let inst = self.ptr(obj + self.lay.instance)?;
        (mem_u64(self.mem(), inst + self.lay.owner)? == obj).then_some(inst)
    }

    fn script_of(&self, inst: u64) -> Option<u64> {
        self.ptr(inst + self.lay.script)
    }

    fn members(&self, inst: u64) -> Option<(u64, u64)> {
        self.vector(self.ptr(inst + self.lay.members)?, MAX_MEMBERS)
    }

    fn script_path(&self, gd: u64) -> Option<String> {
        let t = read_chars(self.mem(), self.ptr(gd + self.lay.path)?, self.lay.wide, self.lay.cow)?;
        t.strip_prefix("res://").filter(|s| good_script(s)).map(str::to_owned)
    }

    fn table(&self, gd: u64) -> Option<HashMap<String, u64>> {
        if let Some(t) = self.indices.borrow().get(&gd) {
            return t.clone();
        }
        let t = member_table(self.mem(), gd + self.lay.indices, &self.lay);
        self.indices.borrow_mut().insert(gd, t.clone());
        t
    }

    /// The Variant of the variable `name` of the script instance `inst`.
    fn member(&self, inst: u64, name: &str) -> Option<u64> {
        let i = *self.table(self.script_of(inst)?)?.get(name)?;
        let (first, n) = self.members(inst)?;
        (i < n).then_some(first + i * VARIANT)
    }

    fn member_name(&self, inst: u64, i: u64) -> Option<String> {
        self.table(self.script_of(inst)?)?.into_iter().find(|(_, x)| *x == i).map(|(n, _)| n)
    }

    /// The script instance of the Object a Variant holds.
    fn object(&self, v: u64) -> Option<u64> {
        if self.u32_at(v)? != OBJECT[self.lay.v4 as usize] {
            return None;
        }
        // References in the second word; other objects in the first, or behind it.
        let words = [mem_u64(self.mem(), v + 16), mem_u64(self.mem(), v + 8)];
        let mut objs: Vec<u64> = words.iter().flatten().copied().filter(|&p| self.heap.is_pointer(p)).collect();
        if let Some(behind) = mem_u64(self.mem(), v + 8).filter(|&p| self.heap.is_pointer(p)).and_then(|p| self.ptr(p)) {
            objs.push(behind);
        }
        objs.into_iter().find_map(|o| self.instance_of(o))
    }

    /// The Array a Variant holds: (its private part, its elements, their count).
    fn array(&self, v: u64) -> Option<(u64, u64, u64)> {
        if self.u32_at(v)? != ARRAY[self.lay.v4 as usize] {
            return None;
        }
        let private = self.ptr(v + 8)?;
        let (first, n) = self.vector(self.ptr(private + 0x10)?, MAX_ELEMENTS)?;
        Some((private, first, n))
    }

    fn number(&self, v: u64) -> bool {
        matches!(self.u32_at(v), Some(INT | REAL))
    }

    /// Whether a script instance is still in use: a freed object stays readable a while, but
    /// Godot clears its link to the script instance when it frees it (Brotato frees the player
    /// every wave).
    fn alive(&self, inst: u64) -> bool {
        self.ptr(inst + self.lay.owner).and_then(|o| self.instance_of(o)) == Some(inst)
    }

    /// The values the path leads to from these script instances.
    fn walk(&self, roots: &[u64], steps: &[Step]) -> Vec<u64> {
        let mut out = Vec::new();
        'root: for &root in roots {
            if !self.alive(root) {
                continue;
            }
            let mut inst = Some(root);
            let mut v = None;
            for step in steps {
                v = match (step, inst, v) {
                    (Step::Member(n), Some(i), _) => self.member(i, n),
                    (Step::Member(n), None, Some(prev)) => self.object(prev).and_then(|i| self.member(i, n)),
                    (Step::Index(e), _, Some(prev)) => self.array(prev).filter(|a| *e < a.2).map(|a| a.1 + e * VARIANT),
                    _ => None,
                };
                inst = None;
                if v.is_none() {
                    continue 'root;
                }
            }
            if let Some(v) = v.filter(|&v| self.number(v)) {
                out.push(v + 8);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Every script instance running one of `gds`.
    fn instances(&self, gds: &[u64]) -> Vec<u64> {
        self.instances_at(self.heap.referrers(gds))
    }

    /// The script instances among the places that point to scripts.
    fn instances_at(&self, refs: impl IntoIterator<Item = u64>) -> Vec<u64> {
        let mut out: Vec<u64> = refs
            .into_iter()
            .filter_map(|at| at.checked_sub(self.lay.script))
            .filter(|&inst| self.ptr(inst + self.lay.owner).and_then(|o| self.instance_of(o)) == Some(inst))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Where the Variant at `v` is kept: (the script instance, the variable's index) when it
    /// is a variable, (the Array's private part, the element) when it is in an array.
    fn holders(&self, vs: &[u64]) -> (Vec<(u64, u64, u64)>, Vec<(u64, u64, u64)>) {
        let mut starts: Vec<(u64, u64, u64)> = Vec::new();
        for &v in vs {
            for i in 0..MAX_ELEMENTS.min(256) {
                let Some(s) = v.checked_sub(i * VARIANT) else { break };
                if self.size(s).is_some_and(|n| n > i && n <= MAX_ELEMENTS) {
                    starts.push((s, v, i));
                }
            }
        }
        let targets: Vec<u64> = starts.iter().map(|s| s.0).collect();
        let (mut members, mut arrays) = (Vec::new(), Vec::new());
        for at in self.heap.referrers(&targets) {
            let Some(data) = mem_u64(self.mem(), at) else { continue };
            for &(_, v, i) in starts.iter().filter(|s| s.0 == data) {
                let Some(inst) = at.checked_sub(self.lay.members) else { continue };
                if self.ptr(inst + self.lay.owner).and_then(|o| self.instance_of(o)) == Some(inst) {
                    members.push((inst, i, v));
                } else if let Some(private) = at.checked_sub(0x10) {
                    arrays.push((private, i, v));
                }
            }
        }
        (members, arrays)
    }

    /// The children of a node (none: empty).
    fn kids(&self, t: &Tree, node: u64) -> Option<Vec<u64>> {
        let (mem, at) = (self.mem(), node + t.children);
        if t.map {
            if mem_u64(mem, at + 0x10)? == 0 {
                return (mem_u32(mem, at + 0x24)? == 0).then(Vec::new);
            }
            return hash_entries(mem, at, MAX_CHILDREN)?.into_iter().map(|(_, v)| self.ptr(v)).collect();
        }
        match mem_u64(mem, at)? {
            0 => Some(Vec::new()),
            data => {
                let (first, n) = self.vector(self.heap.is_pointer(data).then_some(data)?, MAX_CHILDREN)?;
                (0..n).map(|i| self.ptr(first + 8 * i)).collect()
            }
        }
    }

    /// A node's name, made-up ones ("@Node@12") too.
    fn node_name(&self, t: &Tree, node: u64) -> Option<String> {
        let sn = self.ptr(node + t.name)?;
        string_name_if(self.mem(), sn, self.lay.wide, self.lay.cow, |n| !n.is_empty() && n.len() <= MAX_NAME && !n.chars().any(char::is_control))
    }

    /// The nodes from `node` up to the one without a parent (each in its parent's children).
    fn climb(&self, t: &Tree, node: u64) -> Option<Vec<u64>> {
        let mut chain = vec![node];
        while chain.len() <= MAX_DEPTH {
            let x = *chain.last()?;
            let up = mem_u64(self.mem(), x + t.parent)?;
            if up == 0 {
                return Some(chain);
            }
            if !self.heap.is_pointer(up) || !self.kids(t, up)?.contains(&x) {
                return None;
            }
            chain.push(up);
        }
        None
    }

    /// The layout and the root, when `obj` is a node in the scene tree: a field points to a
    /// node whose children (a field soon after) hold it, and so on up to a node named "root".
    fn detect_tree(&self, obj: u64) -> Option<(Tree, u64)> {
        let mut b = vec![0u8; 0x400];
        self.mem().read_exact_at(&mut b, obj).ok()?;
        let word = |o: u64| u64::from_le_bytes(b[o as usize..o as usize + 8].try_into().unwrap());
        for parent in (8..0x3c0).step_by(8) {
            let q = word(parent);
            if q == obj || !self.heap.is_pointer(q) {
                continue;
            }
            for children in (parent + 8..parent + 0x40).step_by(8) {
                for map in [true, false] {
                    let t = Tree { parent, children, map, name: 0 };
                    if !self.kids(&t, q).is_some_and(|k| k.contains(&obj)) {
                        continue;
                    }
                    let Some(chain) = self.climb(&t, obj) else { continue };
                    let root = *chain.last()?;
                    for name in (children + 8..children + 0x100).step_by(8) {
                        let t = Tree { name, ..t };
                        if self.node_name(&t, root).as_deref() == Some("root") && chain.iter().all(|&n| self.node_name(&t, n).is_some()) {
                            return Some((t, root));
                        }
                    }
                }
            }
        }
        None
    }

    /// The scene tree: known, or found from one of these objects (when one is a node in it).
    fn tree(&self, objs: &[u64]) -> Option<(Tree, u64)> {
        let known = *TREE.lock().unwrap();
        if let Some((_, root, t)) = known.filter(|k| k.0 == self.heap.pid) {
            if self.node_name(&t, root).as_deref() == Some("root") {
                return Some((t, root));
            }
        }
        let (t, root) = objs.iter().take(8).find_map(|&o| self.detect_tree(o))?;
        *TREE.lock().unwrap() = Some((self.heap.pid, root, t));
        Some((t, root))
    }

    /// The names from below the root down to `obj` (None for names the engine made up), when
    /// it is in the tree.
    fn place(&self, t: &Tree, root: u64, obj: u64) -> Option<Vec<Option<String>>> {
        let chain = self.climb(t, obj)?;
        if *chain.last()? != root {
            return None;
        }
        chain.iter().rev().skip(1).map(|&n| self.node_name(t, n).map(|name| good_node(&name).then_some(name))).collect()
    }

    /// The nodes at this place below the root (None: any child).
    fn at_place(&self, t: &Tree, root: u64, nodes: &[Option<String>]) -> Vec<u64> {
        let mut level = vec![root];
        for want in nodes {
            let mut next = Vec::new();
            for &n in &level {
                for k in self.kids(t, n).unwrap_or_default() {
                    if want.as_ref().is_none_or(|w| self.node_name(t, k).as_ref() == Some(w)) {
                        next.push(k);
                    }
                }
                if next.len() > MAX_NODES {
                    break;
                }
            }
            level = next;
        }
        level
    }

    /// Every node in the tree.
    fn all_nodes(&self, t: &Tree, root: u64) -> Vec<u64> {
        let mut out = vec![root];
        let mut i = 0;
        while i < out.len() && out.len() < MAX_NODES {
            out.extend(self.kids(t, out[i]).unwrap_or_default());
            i += 1;
        }
        out
    }

    /// The script instances of these objects that run `script`.
    fn running(&self, objs: &[u64], script: &str) -> Vec<u64> {
        let mut ours: HashMap<u64, bool> = HashMap::new();
        objs.iter()
            .filter_map(|&o| {
                let inst = self.instance_of(o)?;
                let gd = self.script_of(inst)?;
                (*ours.entry(gd).or_insert_with(|| self.script_path(gd).as_deref() == Some(script))).then_some(inst)
            })
            .collect()
    }

    /// The objects the path starts from, found in the scene tree (None: the tree isn't known).
    fn tree_roots(&self, path: &ScriptPath) -> Option<Vec<u64>> {
        let (t, root) = self.tree(&[])?;
        let roots = match &path.nodes {
            Some(n) => self.running(&self.at_place(&t, root, n), &path.script),
            None => self.anywhere(&t, root, &path.script),
        };
        if !roots.is_empty() {
            self.mark_in_tree(&path.script);
        }
        Some(roots)
    }

    /// The script's objects anywhere in the tree: first among the children of the nodes they
    /// were last found under (a new wave's player comes back there), the whole tree at most
    /// every `WHOLE_TREE_EVERY` (Lumencraft: 11.5k nodes, ~70 ms).
    fn anywhere(&self, t: &Tree, root: u64, script: &str) -> Vec<u64> {
        let pid = self.heap.pid;
        let home = HOMES.lock().unwrap().iter().find(|h| h.0 == pid && h.1 == script).map(|h| (h.2, h.3.clone()));
        if let Some((walked, parents)) = home {
            let in_tree = |&p: &u64| self.climb(t, p).is_some_and(|c| c.last() == Some(&root));
            let near: Vec<u64> = parents.iter().filter(|p| in_tree(p)).flat_map(|&p| self.kids(t, p).unwrap_or_default()).collect();
            let roots = self.running(&near, script);
            if !roots.is_empty() || walked.elapsed() < WHOLE_TREE_EVERY {
                return roots;
            }
        }
        let roots = self.running(&self.all_nodes(t, root), script);
        self.remember_home(t, script, &roots, true);
        roots
    }

    /// Notes the nodes these objects of the script are under (and that the whole tree was
    /// just looked through).
    fn remember_home(&self, t: &Tree, script: &str, roots: &[u64], walked: bool) {
        let pid = self.heap.pid;
        let mut parents: Vec<u64> = roots.iter().filter_map(|&r| mem_u64(self.mem(), mem_u64(self.mem(), r + self.lay.owner)? + t.parent)).collect();
        parents.sort_unstable();
        parents.dedup();
        let mut homes = HOMES.lock().unwrap();
        let old = homes.iter().position(|h| h.0 == pid && h.1 == script).map(|i| homes.remove(i));
        let at = match (walked, &old) {
            (false, Some(h)) => h.2,
            (false, None) => Instant::now() - WHOLE_TREE_EVERY,
            (true, _) => Instant::now(),
        };
        if parents.is_empty() {
            parents = old.map(|h| h.3).unwrap_or_default();
        }
        homes.retain(|h| h.0 == pid);
        homes.push((pid, script.to_owned(), at, parents));
    }

    fn mark_in_tree(&self, script: &str) {
        let mut known = IN_TREE.lock().unwrap();
        if !known.iter().any(|(pid, s)| *pid == self.heap.pid && *s == script) {
            known.retain(|(pid, _)| *pid == self.heap.pid);
            known.push((self.heap.pid, script.to_owned()));
        }
    }

    /// After a search of memory: learns the scene tree from the objects found, and keeps those
    /// at the path's place.
    fn settle(&self, path: &ScriptPath, roots: Vec<u64>) -> Vec<u64> {
        let owner = |r: u64| mem_u64(self.mem(), r + self.lay.owner).unwrap_or(0);
        let objs: Vec<u64> = roots.iter().map(|&r| owner(r)).collect();
        let Some((t, root)) = self.tree(&objs) else {
            return if path.nodes.is_some() { Vec::new() } else { roots };
        };
        if objs.iter().any(|&o| self.climb(&t, o).is_some_and(|c| c.last() == Some(&root))) {
            self.mark_in_tree(&path.script);
            self.remember_home(&t, &path.script, &roots, false);
        }
        match &path.nodes {
            Some(n) => {
                let here = self.at_place(&t, root, n);
                roots.into_iter().filter(|&r| here.contains(&owner(r))).collect()
            }
            None => roots,
        }
    }
}

/// The scene tree as text, a node a line with its script ("/root/Main/Boss entities/enemy.gd"),
/// once a script path was found or followed in this run (the layouts come from there).
pub fn tree_text(heap: &Heap) -> Option<Vec<String>> {
    let g = Godot::new(heap, known_layout(heap.pid)?);
    let mut out = vec![format!("layout {:?}", g.lay)];
    let Some((t, root)) = g.tree(&[]) else {
        out.push("no scene tree known yet".into());
        return Some(out);
    };
    out.push(format!("tree {t:?}, root 0x{root:x}"));
    let t0 = Instant::now();
    let all = g.all_nodes(&t, root).len();
    out.push(format!("{all} nodes, read in {} ms", t0.elapsed().as_millis()));
    let mut stack = vec![(root, "/root".to_owned())];
    while let Some((n, place)) = stack.pop() {
        let script = g.instance_of(n).and_then(|i| g.script_of(i)).and_then(|gd| g.script_path(gd)).unwrap_or_default();
        out.push(format!("{place} {script}").trim_end().to_owned());
        if out.len() >= 20000 {
            break;
        }
        for k in g.kids(&t, n).unwrap_or_default().into_iter().rev() {
            stack.push((k, format!("{place}/{}", g.node_name(&t, k).unwrap_or_else(|| "?".into()))));
        }
    }
    Some(out)
}

/// Whether the path's objects are looked for in the scene tree only (pointer reads), not in
/// all of memory.
pub fn in_tree(pid: u32, path: &ScriptPath) -> bool {
    knows_tree(pid) && (path.nodes.is_some() || IN_TREE.lock().unwrap().iter().any(|(p, s)| *p == pid && *s == path.script))
}

/// Whether this process's scene tree is known: looking there is cheap to repeat.
pub fn knows_tree(pid: u32) -> bool {
    TREE.lock().unwrap().is_some_and(|t| t.0 == pid)
}

/// A way down from a script instance to the value.
#[derive(Clone)]
struct Partial {
    inst: u64,
    steps: Vec<Step>,
}

/// Finds the layout from the value at `target` (an int or real variable of a script), and the
/// instance and variable it is.
fn start(heap: &Heap, target: u64) -> Option<(Layout, u64, u64)> {
    let mem = heap.file();
    let v = target.checked_sub(8)?;
    if !matches!(mem_u32(mem, v), Some(INT | REAL)) {
        return None;
    }
    // (where the variables start, the size there, which one the value is, where the size is)
    let starts: Vec<(u64, u64, u64, u64)> = SIZES
        .iter()
        .flat_map(|&cow| (0..MAX_MEMBERS.min(256)).map(move |i| (cow, i)))
        .filter_map(|(cow, i)| {
            let s = v.checked_sub(i * VARIANT)?;
            let n = cow_size(mem, s, cow).filter(|&n| n > i && n <= MAX_MEMBERS)?;
            Some((s, n, i, cow))
        })
        .collect();
    let mut targets: Vec<u64> = starts.iter().map(|s| s.0).collect();
    targets.sort_unstable();
    targets.dedup();
    for at in heap.referrers(&targets) {
        let Some(data) = mem_u64(mem, at) else { continue };
        for &(s, count, i, cow) in starts.iter().filter(|s| s.0 == data) {
            for m in (8..0x40).step_by(8) {
                if let Some(lay) = at.checked_sub(m).and_then(|inst| detect(heap, inst, s, count, cow)) {
                    return Some((lay, at - lay.members, i));
                }
            }
        }
    }
    None
}

/// The layout of this process's Godot, when it was found before.
fn known_layout(pid: u32) -> Option<Layout> {
    LAYOUT.lock().unwrap().filter(|(p, _)| *p == pid).map(|(_, l)| l)
}

fn remember(pid: u32, lay: Layout) {
    *LAYOUT.lock().unwrap() = Some((pid, lay));
}

/// A path to the value at `target` when it is a variable of a script: up from its object
/// through the variables and arrays holding it, to a script only one object runs (a
/// singleton). With the places it leads to now.
pub fn discover(heap: &Heap, target: u64) -> Option<(ScriptPath, Vec<u64>)> {
    let (lay, inst, i) = start(heap, target)?;
    remember(heap.pid, lay);
    let g = Godot::new(heap, lay);
    let own = Partial { inst, steps: vec![Step::Member(g.member_name(inst, i)?)] };
    // What a new run of the game finds: the objects running the script with this path (its
    // characters must be where the search looks, in writable memory).
    let path_of = |p: &Partial| {
        let gd = g.script_of(p.inst)?;
        g.ptr(gd + lay.path)?;
        Some(ScriptPath { script: g.script_path(gd)?, nodes: None, steps: p.steps.clone() })
    };
    let owner = |p: &Partial| mem_u64(heap.file(), p.inst + lay.owner);
    let mut level = vec![own.clone()];
    let mut levels = Vec::new();
    let mut own_runs = Vec::new();
    for depth in 0..=MAX_UP {
        // One pass: the objects running each script here, and what holds each object.
        let mut gds: Vec<u64> = level.iter().filter_map(|p| g.script_of(p.inst)).collect();
        gds.sort_unstable();
        gds.dedup();
        let objs: Vec<u64> = level.iter().filter_map(|p| mem_u64(heap.file(), p.inst + lay.owner)).collect();
        let refs = heap.referrers(&[gds.as_slice(), objs.as_slice()].concat());
        let points_to = |at: u64, set: &[u64]| mem_u64(heap.file(), at).is_some_and(|v| set.contains(&v));
        let runs = g.instances_at(refs.iter().copied().filter(|&at| points_to(at, &gds)));
        if depth == 0 {
            own_runs = runs.clone();
        }
        // A script only one object runs (a singleton) is where the path starts.
        let single = |gd: u64| runs.iter().filter(|&&r| g.script_of(r) == Some(gd)).count() == 1;
        for p in level.iter().filter(|p| g.script_of(p.inst).is_some_and(single)) {
            if let Some(path) = path_of(p) {
                let leads = g.walk(&[p.inst], &path.steps);
                if leads == [target] {
                    // Its object found again in the scene tree when it is a node there.
                    g.tree(&owner(p).into_iter().collect::<Vec<_>>());
                    return Some((path, leads));
                }
            }
        }
        levels.push(level.clone());
        if depth == MAX_UP {
            break;
        }
        // Who holds each object: a variable of another script, or an array in one.
        let vars: Vec<(u64, u64)> = refs
            .iter()
            .filter(|&&at| points_to(at, &objs))
            .flat_map(|&at| [at.wrapping_sub(16), at.wrapping_sub(8)])
            .filter_map(|v| Some((v, g.object(v)?)))
            .collect();
        let vs: Vec<u64> = vars.iter().map(|v| v.0).collect();
        let (members, arrays) = g.holders(&vs);
        let down = |v: u64| level.iter().find(|p| vars.iter().any(|&(x, i)| x == v && i == p.inst)).map(|p| p.steps.clone());
        let mut next = Vec::new();
        for (inst, i, v) in members {
            let (Some(name), Some(rest)) = (g.member_name(inst, i), down(v)) else { continue };
            next.push(Partial { inst, steps: [vec![Step::Member(name)], rest].concat() });
        }
        // Arrays: the Variant holding each, a variable of a script.
        let privates: Vec<u64> = arrays.iter().map(|a| a.0).collect();
        let array_vars: Vec<u64> = match privates.is_empty() {
            true => Vec::new(),
            false => heap.referrers(&privates).into_iter().filter_map(|at| at.checked_sub(8)).filter(|&v| g.array(v).is_some()).collect(),
        };
        let (owners, _) = g.holders(&array_vars);
        for (inst, i, av) in owners {
            let (Some((private, first, _)), Some(name)) = (g.array(av), g.member_name(inst, i)) else { continue };
            for &(_, e, v) in arrays.iter().filter(|a| a.0 == private && first + a.1 * VARIANT == a.2) {
                let Some(rest) = down(v) else { continue };
                next.push(Partial { inst, steps: [vec![Step::Member(name.clone()), Step::Index(e)], rest].concat() });
            }
        }
        next.sort_by(|a, b| (a.inst, &a.steps).partial_cmp(&(b.inst, &b.steps)).unwrap());
        next.dedup_by(|a, b| a.inst == b.inst && a.steps == b.steps);
        next.truncate(64);
        if next.is_empty() {
            break;
        }
        level = next;
    }
    // No singleton above it: the object at its place in the scene tree among those running
    // the script (the boss among enemies), the value's own object first.
    for p in levels.iter().flatten() {
        let Some(obj) = owner(p) else { continue };
        let Some((t, root)) = g.tree(&[obj]) else { continue };
        let (Some(nodes), Some(mut path)) = (g.place(&t, root, obj), path_of(p)) else { continue };
        let leads = g.walk(&g.running(&g.at_place(&t, root, &nodes), &path.script), &path.steps);
        if leads == [target] {
            path.nodes = Some(nodes);
            return Some((path, leads));
        }
    }
    // Else the value's own variable in every object running its script, when they are few.
    let path = path_of(&own)?;
    let leads = g.walk(&own_runs, &path.steps);
    (leads.contains(&target) && leads.len() <= MAX_PLACES).then_some((path, leads))
}

/// What the places at `addrs` are when they are script variables, in words: "gold in
/// player_data.gd, in players_data[0] of run_data.gd" (the variable, then what holds its object,
/// one level up), or "gold in player_data.gd (an object no script variable holds)". Brotato
/// keeps a snapshot of the player's data each wave whose gold looked just like the live one in
/// the matches list. All of them at once: a few passes over memory, not a climb each.
pub fn about_all(heap: &Heap, addrs: &[(u64, Kind)]) -> Vec<Option<String>> {
    let mem = heap.file();
    let mut out = vec![None; addrs.len()];
    // A number Variant: an int64 (the match is its low half, the high half its sign) or a
    // double. Junk passing this would each cost a detect per pointer to it (61 s for 20 places
    // matching a common number on the stand-in).
    let variant = |a: u64, kind: Kind| -> Option<u64> {
        let v = a.checked_sub(8)?;
        let ok = match (mem_u32(mem, v)?, kind) {
            (INT, Kind::I32) => {
                let (lo, hi) = (mem_u32(mem, a)?, mem_u32(mem, a + 4)?);
                hi == if (lo as i32) < 0 { u32::MAX } else { 0 }
            }
            (REAL, Kind::F64) => true,
            _ => false,
        };
        ok.then_some(v)
    };
    // (which address, where the variables would start, which one it is, where the size is)
    let mut starts: Vec<(usize, u64, u64, u64)> = Vec::new();
    let known = known_layout(heap.pid);
    for (k, &(a, kind)) in addrs.iter().enumerate() {
        let Some(v) = variant(a, kind) else { continue };
        for &cow in SIZES.iter().filter(|&&c| known.is_none_or(|l| l.cow == c)) {
            for i in 0..MAX_MEMBERS.min(256) {
                let Some(s) = v.checked_sub(i * VARIANT) else { break };
                if cow_size(mem, s, cow).is_some_and(|n| n > i && n <= MAX_MEMBERS) {
                    starts.push((k, s, i, cow));
                }
            }
        }
    }
    if starts.is_empty() {
        return out;
    }
    let mut targets: Vec<u64> = starts.iter().map(|s| s.1).collect();
    targets.sort_unstable();
    targets.dedup();
    // (which address, its script instance, which variable)
    let mut vars: Vec<(usize, u64, u64)> = Vec::new();
    let mut lay = known;
    for at in heap.referrers(&targets) {
        let Some(data) = mem_u64(mem, at) else { continue };
        for &(k, s, i, cow) in starts.iter().filter(|s| s.1 == data) {
            if vars.iter().any(|v| v.0 == k) {
                continue;
            }
            // With the layout known, an instance is checked by its owner pointing back to it.
            if let Some(l) = lay {
                let g = Godot::new(heap, l);
                let inst = at.wrapping_sub(l.members);
                if g.ptr(inst + l.owner).and_then(|o| g.instance_of(o)) == Some(inst) {
                    vars.push((k, inst, i));
                }
                continue;
            }
            let Some(count) = cow_size(mem, s, cow) else { continue };
            let found = (8..0x40).step_by(8).find_map(|m| detect(heap, at.checked_sub(m)?, s, count, cow));
            if let Some(l) = found {
                lay = Some(l);
                vars.push((k, at - l.members, i));
            }
        }
    }
    let Some(lay) = lay else { return out };
    remember(heap.pid, lay);
    let g = Godot::new(heap, lay);
    // Script names with their object's place in the scene tree when it is a node with a name
    // there ("enemy.gd at /root/Main/Boss": three objects run enemy.gd).
    let at = |inst: u64| -> String {
        let obj = mem_u64(mem, inst + lay.owner).unwrap_or(0);
        let place = g.tree(&[obj]).and_then(|(t, root)| g.place(&t, root, obj));
        place.filter(|p| !p.is_empty() && p.iter().all(Option::is_some)).map(|p| format!(" at {}", place_words(&p))).unwrap_or_default()
    };
    let file = |inst: u64| g.script_of(inst).and_then(|gd| g.script_path(gd)).map(|p| format!("{}{}", p.rsplit('/').next().unwrap_or(&p), at(inst)));
    // One level up: the Variants holding each object, and the variables (or arrays in them)
    // those are.
    let objs: Vec<u64> = vars.iter().filter_map(|v| mem_u64(mem, v.1 + lay.owner)).collect();
    let held: Vec<(u64, u64)> = heap
        .referrers(&objs)
        .into_iter()
        .flat_map(|at| [at.wrapping_sub(16), at.wrapping_sub(8)])
        .filter_map(|v| Some((v, g.object(v)?)))
        .collect();
    let vs: Vec<u64> = held.iter().map(|h| h.0).collect();
    let (members, arrays) = if vs.is_empty() { (Vec::new(), Vec::new()) } else { g.holders(&vs) };
    let privates: Vec<u64> = arrays.iter().map(|a| a.0).collect();
    let array_vars: Vec<u64> = match privates.is_empty() {
        true => Vec::new(),
        false => heap.referrers(&privates).into_iter().filter_map(|at| at.checked_sub(8)).filter(|&v| g.array(v).is_some()).collect(),
    };
    let (owners, _) = if array_vars.is_empty() { (Vec::new(), Vec::new()) } else { g.holders(&array_vars) };
    let holder = |inst: u64| -> Option<String> {
        let vs: Vec<u64> = held.iter().filter(|h| h.1 == inst).map(|h| h.0).collect();
        if let Some(&(h, i, _)) = members.iter().find(|m| vs.contains(&m.2)) {
            return Some(format!("{} of {}", g.member_name(h, i)?, file(h)?));
        }
        let &(private, e, _) = arrays.iter().find(|a| vs.contains(&a.2))?;
        let &(h, i, _) = owners.iter().find(|o| g.array(o.2).is_some_and(|a| a.0 == private))?;
        Some(format!("{}[{e}] of {}", g.member_name(h, i)?, file(h)?))
    };
    for &(k, inst, i) in &vars {
        let (Some(name), Some(script)) = (g.member_name(inst, i), file(inst)) else { continue };
        out[k] = Some(match holder(inst) {
            Some(h) => format!("{name} in {script}, in {h}"),
            None if !at(inst).is_empty() => format!("{name} in {script}"),
            None => format!("{name} in {script} (an object no script variable holds)"),
        });
    }
    out
}

/// The objects running the path's script, found by the script's path (a few passes over
/// memory). Finds the layout first when this process's isn't known yet.
fn find_roots_with(g: &Godot, path: &ScriptPath) -> Vec<u64> {
    let ours = |gd: &u64| g.script_path(*gd).as_deref() == Some(path.script.as_str());
    let known: Vec<u64> = SCRIPTS
        .lock()
        .unwrap()
        .iter()
        .find(|(pid, script, _)| *pid == g.heap.pid && *script == path.script)
        .map(|(_, _, gds)| gds.iter().copied().filter(ours).collect())
        .unwrap_or_default();
    if !known.is_empty() {
        return g.instances(&known);
    }
    let full = format!("res://{}", path.script);
    let encoded: Vec<u8> = match g.lay.wide {
        2 => full.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect(),
        _ => full.chars().map(|c| c as u32).chain([0]).flat_map(u32::to_le_bytes).collect(),
    };
    let strings = find_bytes(g.heap, &encoded);
    let scripts: Vec<u64> = g.heap.referrers(&strings).into_iter().filter_map(|at| at.checked_sub(g.lay.path)).collect();
    let scripts: Vec<u64> = scripts.into_iter().filter(ours).collect();
    let mut known = SCRIPTS.lock().unwrap();
    known.retain(|(pid, script, _)| *pid == g.heap.pid && *script != path.script);
    known.push((g.heap.pid, path.script.clone(), scripts.clone()));
    drop(known);
    g.instances(&scripts)
}

/// Every place these bytes are in writable memory (4-aligned: Godot's strings are).
fn find_bytes(heap: &Heap, bytes: &[u8]) -> Vec<u64> {
    let mut found = Vec::new();
    let Some(head) = bytes.get(..4).map(|h| u32::from_le_bytes([h[0], h[1], h[2], h[3]])) else { return found };
    heap.each_chunk(bytes.len(), |addr, b, fresh| {
        for at in (0..fresh).step_by(4) {
            if b.len() >= at + bytes.len() && u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) == head && &b[at..at + bytes.len()] == bytes {
                found.push(addr + at as u64);
            }
        }
    });
    found
}

/// The script instances the path starts from (searches the game's memory, unless a search for
/// the same script just did).
pub fn find_roots(heap: &Heap, path: &ScriptPath) -> Vec<u64> {
    // In the scene tree once it's known: pointer reads. Found nothing there: the script's
    // objects may not be nodes (or not be there yet), so memory too, unless they were found in
    // the tree before.
    if let Some(lay) = known_layout(heap.pid) {
        if let Some(roots) = Godot::new(heap, lay).tree_roots(path) {
            if !roots.is_empty() || in_tree(heap.pid, path) {
                return roots;
            }
        }
    }
    let key = path.roots_key();
    let fresh = |(pid, k, at, roots): &(u32, String, Instant, Vec<u64>)| {
        *pid == heap.pid && *k == key && at.elapsed() < if roots.is_empty() { NONE_FOUND_FOR } else { FOUND_FOR }
    };
    // Only while they're all alive: the game may have freed one (a new wave's player).
    let alive = |roots: &[u64]| known_layout(heap.pid).is_some_and(|lay| roots.iter().all(|&r| Godot::new(heap, lay).alive(r)));
    if let Some((_, _, _, roots)) = FOUND.lock().unwrap().iter().find(|f| fresh(f) && alive(&f.3)) {
        return roots.clone();
    }
    let roots = search_roots(heap, path);
    let mut found = FOUND.lock().unwrap();
    found.retain(|(pid, k, at, _)| *pid == heap.pid && at.elapsed() < FOUND_FOR && *k != key);
    found.push((heap.pid, key, Instant::now(), roots.clone()));
    roots
}

/// Searches memory for the objects running the path's script (a few passes).
fn search_roots(heap: &Heap, path: &ScriptPath) -> Vec<u64> {
    if let Some(lay) = known_layout(heap.pid) {
        let g = Godot::new(heap, lay);
        return g.settle(path, find_roots_with(&g, path));
    }
    // A new run of the game: the script's path, then the objects pointing to it, then the
    // instances pointing to those; the layout comes from the first that fits. Characters are
    // 4 bytes but in Godot 3 Windows builds: each try reads all of the game's memory.
    let full = format!("res://{}", path.script);
    let windows = crate::helper::exe_name(heap.pid).to_lowercase().ends_with(".exe");
    for wide in if windows { [2u64, 4] } else { [4, 2] } {
        let encoded: Vec<u8> = match wide {
            2 => full.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect(),
            _ => full.chars().map(|c| c as u32).chain([0]).flat_map(u32::to_le_bytes).collect(),
        };
        let strings = find_bytes(heap, &encoded);
        if strings.is_empty() {
            continue;
        }
        let refs = heap.referrers(&strings);
        let mut scripts: Vec<u64> = refs.iter().flat_map(|&at| (0..SCRIPT_SIZE).step_by(8).filter_map(move |p| at.checked_sub(p))).collect();
        scripts.sort_unstable();
        scripts.dedup();
        let ats = heap.referrers(&scripts);
        let mut seen = 0;
        let lay = ats.iter().find_map(|&at| {
            seen = at;
            let gd = mem_u64(heap.file(), at)?;
            [8u64, 0x10, 0x18].into_iter().find_map(|script| {
                let inst = at.checked_sub(script)?;
                (script + 8..0x40).step_by(8).find_map(|m| {
                    let members = mem_u64(heap.file(), inst + m).filter(|&p| heap.is_pointer(p))?;
                    SIZES.iter().find_map(|&cow| {
                        let count = cow_size(heap.file(), members, cow).filter(|n| (1..=MAX_MEMBERS).contains(n))?;
                        detect(heap, inst, members, count, cow).filter(|l| l.script == script && mem_u64(heap.file(), inst + l.script) == Some(gd))
                    })
                })
            })
        });
        let Some(lay) = lay else { continue };
        remember(heap.pid, lay);
        // The same places that point to scripts hold the instances.
        let g = Godot::new(heap, lay);
        let ours = |at: &u64| {
            mem_u64(heap.file(), *at)
                .filter(|&gd| refs.iter().any(|&r| r == gd + lay.path))
                .is_some_and(|gd| g.script_path(gd).as_deref() == Some(path.script.as_str()))
        };
        let roots = g.instances_at(ats.iter().copied().filter(ours));
        // The scene tree also from the object the layout came from, when the script's own
        // aren't around (Brotato's shop: no player).
        if roots.is_empty() {
            let inst = seen.wrapping_sub(lay.script);
            g.tree(&mem_u64(heap.file(), inst + lay.owner).into_iter().collect::<Vec<_>>());
        }
        return g.settle(path, roots);
    }
    Vec::new()
}

/// Where the path leads from the script instances found before (no search).
pub fn walk(heap: &Heap, roots: &[u64], path: &ScriptPath) -> Vec<u64> {
    let Some(lay) = known_layout(heap.pid) else { return Vec::new() };
    let g = Godot::new(heap, lay);
    // A root is still one when it still runs the script.
    let roots: Vec<u64> =
        roots.iter().copied().filter(|&r| g.script_of(r).and_then(|gd| g.script_path(gd)).as_deref() == Some(path.script.as_str())).collect();
    g.walk(&roots, &path.steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip() {
        let t = "gd:singletons/run_data.gd.players_data[0].gold";
        let p = ScriptPath::parse(t).unwrap();
        assert_eq!(p.script, "singletons/run_data.gd");
        assert_eq!(p.steps, vec![Step::Member("players_data".into()), Step::Index(0), Step::Member("gold".into())]);
        assert_eq!(p.text(), t);
        assert_eq!(p.describe(), "players_data[0].gold in run_data.gd");
        assert_eq!(ScriptPath::parse("gd:a/b.gd.x[1][2].y").unwrap().text(), "gd:a/b.gd.x[1][2].y");
        assert!(ScriptPath::parse("gd:a/b.gd.x[0]").is_none());
        assert!(ScriptPath::parse("gd:a/b.gd").is_none());
        assert!(ScriptPath::parse("gd:a b.gd.x").is_none());
        assert!(ScriptPath::parse("{amount|id=0}").is_none());
    }

    #[test]
    fn place_round_trip() {
        let t = "gd:entities/enemy.gd@/root/Main/*/Big%20Boss.current_stats.health";
        let p = ScriptPath::parse(t).unwrap();
        assert_eq!(p.script, "entities/enemy.gd");
        assert_eq!(p.nodes, Some(vec![Some("Main".into()), None, Some("Big Boss".into())]));
        assert_eq!(p.steps, vec![Step::Member("current_stats".into()), Step::Member("health".into())]);
        assert_eq!(p.text(), t);
        assert_eq!(p.describe(), "current_stats.health in enemy.gd at /root/Main/*/Big Boss");
        assert_eq!(ScriptPath::parse("gd:a.gd@/root.x").unwrap().nodes, Some(vec![]));
        assert!(ScriptPath::parse("gd:a.gd@/Main.x").is_none());
        assert!(ScriptPath::parse("gd:a.gd@/root/A%2EB.x").is_none());
        assert!(ScriptPath::parse("gd:a.gd@/root/A").is_none());
    }
}
