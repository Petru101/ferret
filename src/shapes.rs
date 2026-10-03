// The memory around values found in a game ("shapes"). A game keeps its values of one kind the
// same way (every inventory item is a record like the next one: Lumencraft's lumen and iron
// records match word for word), so a scan for the next value tries the places shaped like an
// earlier find first. Saved per game next to its profile; the helper does the matching.

use std::fs;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Word {
    Exact(u32),
    /// A pointer into mapped memory (where it points changes between runs).
    Pointer,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Shape {
    /// The value's type as the helper names it (i32, f32, f64, xor).
    kind: String,
    /// Offsets from the value, ascending.
    words: Vec<(i64, Word)>,
}

/// Fewer words than this that aren't zero (pointers, other numbers) fit too many places to be
/// worth preferring.
const DISTINCT: usize = 3;
/// A find agreeing with a shape on at least this share of its words narrows it to what they
/// share; less, and it's another kind of record.
const AGREE: f64 = 0.6;
const MAX_SHAPES: usize = 8;

impl Shape {
    /// The helper's text: "<type> <offset>=<hex word>|p ...".
    pub fn parse(text: &str) -> Option<Shape> {
        let mut it = text.split_whitespace();
        let kind = it.next()?.to_owned();
        let mut words = Vec::new();
        for w in it {
            let (off, v) = w.split_once('=')?;
            let word = if v == "p" { Word::Pointer } else { Word::Exact(u32::from_str_radix(v, 16).ok()?) };
            words.push((off.parse().ok()?, word));
        }
        words.sort_by_key(|&(off, _)| off);
        Some(Shape { kind, words })
    }

    pub fn text(&self) -> String {
        let words = self.words.iter().map(|&(off, w)| match w {
            Word::Exact(v) => format!("{off}={v:x}"),
            Word::Pointer => format!("{off}=p"),
        });
        std::iter::once(self.kind.clone()).chain(words).collect::<Vec<_>>().join(" ")
    }

    /// Enough non-zero words to pick out a kind of record rather than any spot near zeros.
    pub fn distinct(&self) -> bool {
        self.words.iter().filter(|(_, w)| *w != Word::Exact(0)).count() >= DISTINCT
    }

    /// The words `other` has too.
    fn shared(&self, other: &Shape) -> Shape {
        let words = self.words.iter().filter(|w| other.words.contains(w)).copied().collect();
        Shape { kind: self.kind.clone(), words }
    }
}

/// What a new find taught.
#[derive(PartialEq, Debug)]
pub enum Learned {
    /// It fits a shape already known.
    Known,
    /// A shape it mostly agrees with now keeps only the words they share.
    Narrowed,
    /// Another kind of record.
    New,
    /// Too little around it to tell places apart.
    Plain,
}

/// Adds what the surroundings of a newly found value say. `found` has every word around it; a
/// shape learned from one find still has the fields of that one item, a second find of the same
/// kind drops them.
pub fn learn(shapes: &mut Vec<Shape>, found: Shape) -> Learned {
    let same_kind = |s: &Shape| s.kind == found.kind;
    if shapes.iter().filter(|s| same_kind(s)).any(|s| s.words.iter().all(|w| found.words.contains(w))) {
        return Learned::Known;
    }
    let best = shapes
        .iter()
        .enumerate()
        .filter(|(_, s)| same_kind(s))
        .map(|(i, s)| (i, s.shared(&found)))
        .max_by_key(|(_, shared)| shared.words.len());
    if let Some((i, shared)) = best {
        if shared.distinct() && shared.words.len() as f64 >= AGREE * shapes[i].words.len() as f64 {
            shapes[i] = shared;
            return Learned::Narrowed;
        }
    }
    if !found.distinct() {
        return Learned::Plain;
    }
    if shapes.len() >= MAX_SHAPES {
        shapes.remove(0);
    }
    shapes.push(found);
    Learned::New
}

/// One shape per line; lines this build can't read are dropped.
pub fn load(path: &Path) -> Vec<Shape> {
    fs::read_to_string(path).unwrap_or_default().lines().filter_map(Shape::parse).collect()
}

pub fn save(path: &Path, shapes: &[Shape]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text: String = shapes.iter().map(|s| s.text() + "\n").collect();
    fs::write(path, text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Lumencraft's lumen and iron, as the helper describes them (Godot 3 dictionary entries).
    const LUMEN: &str = "i32 -32=60 -28=0 -24=41 -20=0 -16=p -8=2 -4=0 4=0 8=p 16=p 24=p 32=3";
    const IRON: &str = "i32 -32=60 -28=0 -24=41 -20=0 -16=p -8=2 -4=0 4=0 8=p 16=p 24=p 32=7";

    #[test]
    fn round_trip() {
        let s = Shape::parse(LUMEN).unwrap();
        assert_eq!(s.text(), LUMEN);
        assert!(s.distinct());
        assert_eq!(Shape::parse("i32 -8=zz"), None);
    }

    #[test]
    fn second_find_drops_the_first_ones_fields() {
        let mut shapes = Vec::new();
        assert_eq!(learn(&mut shapes, Shape::parse(LUMEN).unwrap()), Learned::New);
        assert_eq!(learn(&mut shapes, Shape::parse(IRON).unwrap()), Learned::Narrowed);
        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].text(), "i32 -32=60 -28=0 -24=41 -20=0 -16=p -8=2 -4=0 4=0 8=p 16=p 24=p");
        // A third item of the same kind changes nothing.
        let wood = "i32 -32=60 -28=0 -24=41 -20=0 -16=p -8=2 -4=0 4=0 8=p 16=p 24=p 32=9";
        assert_eq!(learn(&mut shapes, Shape::parse(wood).unwrap()), Learned::Known);
    }

    #[test]
    fn other_records_and_plain_spots_stay_apart() {
        let mut shapes = vec![Shape::parse(LUMEN).unwrap()];
        // A float in a player object: another shape.
        let hp = "f32 -16=p -8=p -4=42c80000 4=3f800000 8=p";
        assert_eq!(learn(&mut shapes, Shape::parse(hp).unwrap()), Learned::New);
        assert_eq!(shapes.len(), 2);
        // Zeros all around: nothing to go by.
        assert_eq!(learn(&mut shapes, Shape::parse("i32 -8=0 -4=0 4=0 8=0").unwrap()), Learned::Plain);
        assert_eq!(shapes.len(), 2);
    }
}
