// A game's digits, learned from numbers the player types and from values found in memory.
// Reading a number then matches each glyph against these shapes; Tesseract is the fallback.

use std::fs;
use std::path::Path;

/// Glyphs are compared on a grid of this size, stretched to fill it.
const GW: usize = 16;
const GH: usize = 20;
/// Cells that may differ (after allowing a one-cell shift) for two glyphs to be the same digit.
/// A 0 and an 8 in a blocky font differ by about ten.
const SAME: usize = 4;
/// Samples kept per digit, newest last.
const PER_DIGIT: usize = 8;

/// One glyph cut out of a mask: its box in the mask, which pixels of the box are ink, and
/// whether the crop's left or right edge cuts it (then it may be part of something else).
#[derive(Clone)]
pub struct Glyph {
    pub x: u32,
    pub w: u32,
    pub h: u32,
    pub ink: Vec<bool>,
    pub cut: bool,
}

impl Glyph {
    /// The glyph shrunk to its ink, or `None` if there is none.
    fn trimmed(&self) -> Option<Glyph> {
        let at = |x: u32, y: u32| self.ink[(y * self.w + x) as usize];
        let cols: Vec<u32> = (0..self.w).filter(|&x| (0..self.h).any(|y| at(x, y))).collect();
        let rows: Vec<u32> = (0..self.h).filter(|&y| (0..self.w).any(|x| at(x, y))).collect();
        let (x0, x1, y0, y1) = (*cols.first()?, *cols.last()?, *rows.first()?, *rows.last()?);
        let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
        let ink = (0..w * h).map(|i| at(x0 + i % w, y0 + i / w)).collect();
        Some(Glyph { x: self.x + x0, w, h, ink, cut: self.cut })
    }

    /// `n` equal-width slices, for digits drawn touching each other.
    fn split(&self, n: u32) -> Vec<Glyph> {
        (0..n)
            .filter_map(|i| {
                let (x0, x1) = (self.w * i / n, self.w * (i + 1) / n);
                let w = x1 - x0;
                let ink = (0..w * self.h).map(|j| self.ink[((j / w) * self.w + x0 + j % w) as usize]).collect();
                Glyph { x: self.x + x0, w, h: self.h, ink, cut: self.cut }.trimmed()
            })
            .collect()
    }
}

#[derive(Clone)]
struct Shape {
    /// Height in frame pixels and width / height.
    height: f32,
    aspect: f32,
    cells: Vec<bool>,
}

impl Shape {
    /// `scale`: mask pixels per frame pixel.
    fn of(g: &Glyph, scale: u32) -> Shape {
        // The pixels of cell `i` of `n` along a side of `len` pixels (at least one).
        let span = |i: usize, n: usize, len: u32| {
            let a = i as u32 * len / n as u32;
            (a, ((i as u32 + 1) * len / n as u32).max(a + 1))
        };
        let mut cells = vec![false; GW * GH];
        for (cy, row) in cells.chunks_mut(GW).enumerate() {
            let (y0, y1) = span(cy, GH, g.h);
            for (cx, cell) in row.iter_mut().enumerate() {
                let (x0, x1) = span(cx, GW, g.w);
                let (mut on, mut all) = (0, 0);
                for y in y0..y1 {
                    for x in x0..x1 {
                        on += g.ink[(y * g.w + x) as usize] as u32;
                        all += 1;
                    }
                }
                *cell = on * 2 > all;
            }
        }
        Shape { height: g.h as f32 / scale as f32, aspect: g.w as f32 / g.h as f32, cells }
    }

    /// Cells of `self` that `other` has no match for within one cell.
    fn missing_in(&self, other: &Shape) -> usize {
        (0..GW * GH)
            .filter(|&i| {
                let (x, y) = ((i % GW) as i32, (i / GW) as i32);
                let v = self.cells[i];
                !(-1..=1).any(|dy| {
                    (-1..=1).any(|dx| {
                        let (nx, ny) = (x + dx, y + dy);
                        nx >= 0 && ny >= 0 && nx < GW as i32 && ny < GH as i32 && other.cells[(ny * GW as i32 + nx) as usize] == v
                    })
                })
            })
            .count()
    }

    /// Too little ink to be a glyph: an outline from anti-aliasing, or noise.
    fn is_faint(&self) -> bool {
        self.cells.iter().filter(|c| **c).count() < GW * GH / 10
    }

    /// How many cells differ, or `None` when size or proportions rule it out.
    fn distance(&self, other: &Shape) -> Option<usize> {
        let size = (self.height / other.height).ln().abs();
        let proportion = (self.aspect / other.aspect).ln().abs();
        (size < 0.3 && proportion < 0.3).then(|| self.missing_in(other).max(other.missing_in(self)))
    }

    fn to_hex(&self) -> String {
        self.cells.chunks(4).map(|c| format!("{:x}", c.iter().fold(0, |a, b| a * 2 + *b as u32))).collect()
    }

    fn from_hex(height: f32, aspect: f32, hex: &str) -> Option<Shape> {
        let cells: Vec<bool> = hex
            .chars()
            .map(|c| c.to_digit(16))
            .collect::<Option<Vec<u32>>>()?
            .into_iter()
            .flat_map(|d| (0..4).rev().map(move |b| d >> b & 1 == 1))
            .collect();
        (cells.len() == GW * GH).then_some(Shape { height, aspect, cells })
    }
}

#[derive(Default)]
pub struct Font {
    samples: Vec<(u8, Shape)>,
}

/// A number read through the font: its value, and the worst glyph's distance.
pub struct FontRead {
    pub n: i64,
    pub worst: usize,
    pub glyphs: usize,
}

impl Font {
    /// File format: one learned glyph per line, "<digit> <height> <aspect> <cells as hex>".
    pub fn load(path: &Path) -> Font {
        let text = fs::read_to_string(path).unwrap_or_default();
        let samples = text
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                let [d, h, a, hex] = f[..] else { return None };
                let d: u8 = d.parse().ok().filter(|d| *d <= 9)?;
                Shape::from_hex(h.parse().ok()?, a.parse().ok()?, hex).filter(|s| !s.is_faint()).map(|s| (d, s))
            })
            .collect();
        Font { samples }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text: String =
            self.samples.iter().map(|(d, s)| format!("{d} {:.2} {:.3} {}\n", s.height, s.aspect, s.to_hex())).collect();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        fs::write(path, text).map_err(|e| e.to_string())
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The digits it knows, e.g. "0 1 3 9".
    pub fn known(&self) -> String {
        let mut d: Vec<u8> = self.samples.iter().map(|(d, _)| *d).collect();
        d.sort();
        d.dedup();
        d.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(" ")
    }

    /// The closest digit and its distance, if close enough.
    fn digit(&self, s: &Shape) -> Option<(u8, usize)> {
        if s.is_faint() {
            return None;
        }
        self.samples
            .iter()
            .filter_map(|(d, t)| Some((*d, s.distance(t)?)))
            .min_by_key(|(_, dist)| *dist)
            .filter(|(_, dist)| *dist <= SAME)
    }

    /// How many glyphs (not cut off) are the size of a known digit.
    pub fn digit_sized(&self, glyphs: &[Glyph], scale: u32) -> usize {
        let sized = |g: &&Glyph| self.samples.iter().any(|(_, s)| (g.h as f32 / scale as f32 / s.height).ln().abs() < 0.3);
        glyphs.iter().filter(|g| !g.cut).filter(sized).count()
    }

    /// Reads glyphs (left to right) as a number, when every one of them is a known digit, except
    /// glyphs cut off by the crop's edge, which are left out when they aren't digits.
    /// A glyph wider than any known digit may be digits drawn touching: tried as equal slices.
    pub fn read(&self, glyphs: &[Glyph], scale: u32) -> Option<FontRead> {
        if self.samples.is_empty() || glyphs.is_empty() {
            return None;
        }
        let widest = self.samples.iter().map(|(_, s)| s.aspect).fold(0.0, f32::max);
        let mut text = String::new();
        let mut worst = 0;
        let mut count = 0;
        for g in glyphs {
            let read = self.digit(&Shape::of(g, scale)).map(|r| vec![r]).or_else(|| {
                let parts = (g.w as f32 / g.h as f32 / widest).round() as u32;
                (2..=4).contains(&parts).then(|| g.split(parts).iter().map(|p| self.digit(&Shape::of(p, scale))).collect::<Option<Vec<_>>>()).flatten()
            });
            let Some(read) = read else {
                if g.cut {
                    continue;
                }
                return None;
            };
            for (d, dist) in read {
                text.push(char::from(b'0' + d));
                worst = worst.max(dist);
                count += 1;
            }
        }
        Some(FontRead { n: text.parse().ok()?, worst, glyphs: count })
    }

    /// Learns glyphs as the digits of `label` (same count, left to right). `trusted` labels come
    /// from memory: samples of other digits that look like them are wrong and get dropped.
    /// Returns how many new shapes were kept.
    pub fn learn(&mut self, glyphs: &[Glyph], scale: u32, label: &str, trusted: bool) -> usize {
        let mut added = 0;
        for (g, c) in glyphs.iter().zip(label.bytes()) {
            let d = c - b'0';
            let s = Shape::of(g, scale);
            if s.is_faint() {
                continue;
            }
            if trusted {
                self.samples.retain(|(od, t)| *od == d || s.distance(t).is_none_or(|x| x > SAME));
            }
            if self.samples.iter().any(|(od, t)| *od == d && s.distance(t).is_some_and(|x| x <= 1)) {
                continue;
            }
            self.samples.push((d, s));
            added += 1;
            let same: Vec<usize> = (0..self.samples.len()).filter(|&i| self.samples[i].0 == d).collect();
            if same.len() > PER_DIGIT {
                self.samples.remove(same[0]);
            }
        }
        added
    }
}
