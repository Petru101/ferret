// A game's digits, learned from numbers the player types and from values found in memory.
// Reading a number then matches each glyph against these shapes; Tesseract is the fallback.
// Pixel fonts are also kept on their own grid of font pixels, which reads them at any size
// (the player resizes the game window; a game draws the same font at several sizes).

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

/// A glyph of a pixel font, one cell per font pixel.
#[derive(Clone, PartialEq)]
struct Grid {
    w: u32,
    h: u32,
    cells: Vec<bool>,
}

impl Grid {
    /// The glyph as `rows` font pixels tall, if it is drawn on such a grid: redrawn from its
    /// cells, it must give back the glyph (anti-aliased or smooth fonts don't). Pixels within
    /// half a frame pixel of a cell edge aren't checked: scaled pixel art rounds font pixels to
    /// 2 or 3 screen pixels. `scale`: mask pixels per frame pixel.
    fn of(g: &Glyph, rows: u32, scale: u32) -> Option<Grid> {
        if rows == 0 || g.h < rows {
            return None;
        }
        let uy = g.h as f32 / rows as f32;
        let cols = ((g.w as f32 / uy).round() as u32).max(1);
        let ux = g.w as f32 / cols as f32;
        if (ux / uy).ln().abs() > 0.35 {
            return None;
        }
        let at = |x: u32, y: u32| g.ink[(y.min(g.h - 1) * g.w + x.min(g.w - 1)) as usize];
        let cells: Vec<bool> = (0..rows * cols)
            .map(|i| at(((i % cols) as f32 * ux + ux / 2.0) as u32, ((i / cols) as f32 * uy + uy / 2.0) as u32))
            .collect();
        let margin = scale as f32 / 2.0;
        // The cell a pixel is in, unless it's near an edge.
        let inner = |p: u32, u: f32, n: u32| {
            let c = p as f32 + 0.5;
            let i = ((c / u) as u32).min(n - 1);
            (c - i as f32 * u >= margin && (i + 1) as f32 * u - c >= margin).then_some(i)
        };
        let (mut checked, mut wrong) = (0, 0);
        for i in 0..g.w * g.h {
            let (Some(cx), Some(cy)) = (inner(i % g.w, ux, cols), inner(i / g.w, uy, rows)) else { continue };
            checked += 1;
            wrong += (g.ink[i as usize] != cells[(cy * cols + cx) as usize]) as usize;
        }
        (checked * 5 >= (g.w * g.h) as usize && wrong * 20 <= checked).then_some(Grid { w: cols, h: rows, cells })
    }

    /// Cells that differ; `None` when the sizes do.
    fn distance(&self, other: &Grid) -> Option<usize> {
        (self.w == other.w && self.h == other.h).then(|| self.cells.iter().zip(&other.cells).filter(|(a, b)| a != b).count())
    }

    /// Differences still counted as the same digit: none on small grids (a 0 and an 8 of a 3x5
    /// font differ by one cell).
    fn tolerance(&self) -> usize {
        self.cells.len() / 40
    }

    /// All ink: a bar. Only a 1 looks like that; anything else is a mislearned edge or border.
    fn is_bar(&self) -> bool {
        self.cells.iter().all(|c| *c)
    }

    fn to_text(&self) -> String {
        let bits: String = self.cells.iter().map(|c| if *c { '1' } else { '0' }).collect();
        format!("{}x{} {bits}", self.w, self.h)
    }

    fn from_text(size: &str, bits: &str) -> Option<Grid> {
        let (w, h) = size.split_once('x')?;
        let (w, h): (u32, u32) = (w.parse().ok()?, h.parse().ok()?);
        let cells: Vec<bool> = bits.chars().map(|c| c == '1').collect();
        (cells.len() == (w * h) as usize && w > 0 && h > 0).then_some(Grid { w, h, cells })
    }
}

#[derive(Default)]
pub struct Font {
    samples: Vec<(u8, Shape)>,
    grids: Vec<(u8, Grid)>,
    /// Lines this build doesn't understand (from a newer one), saved back unchanged.
    other: Vec<String>,
}

/// A learned shape, for showing: one cell per font pixel for pixel fonts, else stretched to 16x20.
#[derive(Clone)]
pub struct DigitShape {
    pub w: u32,
    pub h: u32,
    pub cells: Vec<bool>,
}

/// A number read through the font: its value, and the worst glyph's distance.
pub struct FontRead {
    pub n: i64,
    pub worst: usize,
    pub glyphs: usize,
}

impl Font {
    /// File format: one learned glyph per line, "<digit> <height> <aspect> <cells as hex>", and
    /// pixel-font glyphs as "<digit> grid <w>x<h> <cells as 0/1>" (older builds skip those).
    /// Other lines are kept as they are.
    pub fn load(path: &Path) -> Font {
        let text = fs::read_to_string(path).unwrap_or_default();
        let mut font = Font::default();
        for l in text.lines().filter(|l| !l.trim().is_empty()) {
            let f: Vec<&str> = l.split_whitespace().collect();
            let known = match f[..] {
                [d, "grid", a, hex] => d.parse::<u8>().ok().filter(|d| *d <= 9).zip(Grid::from_text(a, hex)).map(|(d, g)| {
                    if d == 1 || !g.is_bar() {
                        font.grids.push((d, g));
                    }
                }),
                [d, h, a, hex] => {
                    let d = d.parse::<u8>().ok().filter(|d| *d <= 9);
                    let shape = h.parse().ok().zip(a.parse().ok()).and_then(|(h, a)| Shape::from_hex(h, a, hex));
                    d.zip(shape).map(|(d, s)| {
                        if !s.is_faint() {
                            font.samples.push((d, s));
                        }
                    })
                }
                _ => None,
            };
            if known.is_none() {
                font.other.push(l.to_owned());
            }
        }
        font
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text: String = self
            .samples
            .iter()
            .map(|(d, s)| format!("{d} {:.2} {:.3} {}\n", s.height, s.aspect, s.to_hex()))
            .chain(self.grids.iter().map(|(d, g)| format!("{d} grid {}\n", g.to_text())))
            .chain(self.other.iter().map(|l| format!("{l}\n")))
            .collect();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        fs::write(path, text).map_err(|e| e.to_string())
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty() && self.grids.is_empty()
    }

    /// Has pixel-font grids: then any solid bar of any size reads as a 1.
    pub fn has_grids(&self) -> bool {
        !self.grids.is_empty()
    }

    /// The digit a glyph is, if any.
    pub fn digit_of(&self, g: &Glyph, scale: u32) -> Option<u8> {
        self.digit(g, scale).map(|(d, _)| d)
    }

    /// The shapes learned for `d`, grids first.
    pub fn shapes(&self, d: u8) -> Vec<DigitShape> {
        let grids = self.grids.iter().filter(|(e, _)| *e == d).map(|(_, g)| DigitShape { w: g.w, h: g.h, cells: g.cells.clone() });
        let shapes = self.samples.iter().filter(|(e, _)| *e == d).map(|(_, s)| DigitShape { w: GW as u32, h: GH as u32, cells: s.cells.clone() });
        grids.chain(shapes).collect()
    }

    /// Drops every shape learned for `d`; returns how many there were.
    pub fn forget(&mut self, d: u8) -> usize {
        let before = self.samples.len() + self.grids.len();
        self.samples.retain(|(e, _)| *e != d);
        self.grids.retain(|(e, _)| *e != d);
        self.other.retain(|l| l.split_whitespace().next() != Some(&d.to_string()));
        before - self.samples.len() - self.grids.len()
    }

    /// Whether a number Tesseract read agrees with the glyphs the learned digits do know (when
    /// the glyphs line up with its digits; otherwise there's nothing to compare).
    pub fn agrees(&self, glyphs: &[Glyph], scale: u32, digits: &str) -> bool {
        let whole: Vec<&Glyph> = glyphs.iter().filter(|g| !g.cut).collect();
        let text = digits;
        whole.len() != text.len()
            || whole.iter().zip(text.bytes()).all(|(g, c)| self.digit(g, scale).is_none_or(|(d, _)| d == c - b'0'))
    }

    /// Knows every digit: then what it can't read isn't a number (a menu over the watched spot).
    pub fn knows_all(&self) -> bool {
        self.known().len() == 19
    }

    /// The digits it knows, e.g. "0 1 3 9".
    pub fn known(&self) -> String {
        let mut d: Vec<u8> = self.samples.iter().map(|(d, _)| *d).chain(self.grids.iter().map(|(d, _)| *d)).collect();
        d.sort();
        d.dedup();
        d.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(" ")
    }

    /// Row counts of the learned grids, most common first.
    fn grid_rows(&self) -> Vec<u32> {
        let mut rows: Vec<u32> = self.grids.iter().map(|(_, g)| g.h).collect();
        rows.sort();
        rows.dedup();
        rows.sort_by_key(|r| std::cmp::Reverse(self.grids.iter().filter(|(_, g)| g.h == *r).count()));
        rows
    }

    /// The closest digit and its distance, if close enough: on the grid when it's a pixel font
    /// (any size), else by shape (about the size it was learned at).
    fn digit(&self, g: &Glyph, scale: u32) -> Option<(u8, usize)> {
        let mut best: Option<(u8, usize)> = None;
        for grid in self.grid_rows().into_iter().filter_map(|rows| Grid::of(g, rows, scale)) {
            for (d, t) in &self.grids {
                if let Some(dist) = grid.distance(t).filter(|&x| x <= grid.tolerance()) {
                    if best.is_none_or(|(_, b)| dist < b) {
                        best = Some((*d, dist));
                    }
                }
            }
        }
        if best.is_some() {
            return best;
        }
        let s = Shape::of(g, scale);
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
        let rows = self.grid_rows();
        let on_grid = |g: &Glyph| {
            rows.iter().filter_map(|r| Grid::of(g, *r, scale)).any(|a| self.grids.iter().any(|(_, b)| a.w == b.w && a.h == b.h))
        };
        let sized =
            |g: &&Glyph| self.samples.iter().any(|(_, s)| (g.h as f32 / scale as f32 / s.height).ln().abs() < 0.3) || on_grid(g);
        glyphs.iter().filter(|g| !g.cut).filter(sized).count()
    }

    /// Reads glyphs (left to right) as a number, when every one of them is a known digit, except
    /// glyphs cut off by the crop's edge, which are left out when they aren't digits.
    /// A glyph wider than any known digit may be digits drawn touching: tried as equal slices.
    pub fn read(&self, glyphs: &[Glyph], scale: u32) -> Option<FontRead> {
        if self.is_empty() || glyphs.is_empty() {
            return None;
        }
        let widest = self
            .samples
            .iter()
            .map(|(_, s)| s.aspect)
            .chain(self.grids.iter().map(|(_, g)| g.w as f32 / g.h as f32))
            .fold(0.0, f32::max);
        let mut text = String::new();
        let mut worst = 0;
        let mut count = 0;
        for g in glyphs {
            let read = self.digit(g, scale).map(|r| vec![r]).or_else(|| {
                let parts = (g.w as f32 / g.h as f32 / widest).round() as u32;
                (2..=4).contains(&parts).then(|| g.split(parts).iter().map(|p| self.digit(p, scale)).collect::<Option<Vec<_>>>()).flatten()
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
        self.learn_as(glyphs, scale, label, trusted, true)
    }

    /// `learn` without pixel-font grids: for the full-frame finder's shapes, where a smooth font's
    /// thin flat-colour cores fit a coarse grid by chance.
    pub fn learn_shapes(&mut self, glyphs: &[Glyph], scale: u32, label: &str, trusted: bool) -> usize {
        self.learn_as(glyphs, scale, label, trusted, false)
    }

    fn learn_as(&mut self, glyphs: &[Glyph], scale: u32, label: &str, trusted: bool, grids: bool) -> usize {
        let mut added = 0;
        // Pixel font: the fewest rows every glyph fits on. Bars (a 1) fit any, so they don't
        // count; a lone 1 goes on the rows the font already uses.
        let fits = |rows: u32| {
            let grids: Option<Vec<Grid>> = glyphs.iter().map(|g| Grid::of(g, rows, scale)).collect();
            grids.is_some_and(|grids| grids.iter().any(|g| !g.is_bar()))
        };
        let rows = if grids { (3..=16).find(|&r| fits(r)).or(self.grid_rows().first().copied()) } else { None };
        for (g, c) in glyphs.iter().zip(label.bytes()) {
            let d = c - b'0';
            if let Some(grid) = rows.and_then(|r| Grid::of(g, r, scale)).filter(|grid| d == 1 || !grid.is_bar()) {
                if trusted {
                    self.grids.retain(|(od, t)| *od == d || grid.distance(t).is_none_or(|x| x > grid.tolerance()));
                }
                if !self.grids.iter().any(|(od, t)| *od == d && *t == grid) {
                    self.grids.push((d, grid));
                    added += 1;
                    let same: Vec<usize> = (0..self.grids.len()).filter(|&i| self.grids[i].0 == d).collect();
                    if same.len() > PER_DIGIT {
                        self.grids.remove(same[0]);
                    }
                }
            }
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

#[cfg(test)]
mod grid_tests {
    use super::*;

    /// Digits of a 3x5 pixel font drawn with font pixels `u` screen pixels wide (nearest
    /// neighbour, so pixels round to uneven sizes), 4 mask pixels per screen pixel.
    fn draw(rows: [&str; 5], u: f32) -> Glyph {
        let (w, h) = ((3.0 * u).round() as u32 * 4, (5.0 * u).round() as u32 * 4);
        let ink = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32 / 4.0, (i / w) as f32 / 4.0);
                rows[((y / u) as usize).min(4)].as_bytes()[((x / u) as usize).min(2)] == b'#'
            })
            .collect();
        Glyph { x: 0, w, h, ink, cut: false }
    }

    const TWO: [&str; 5] = ["###", "..#", "###", "#..", "###"];
    const FIVE: [&str; 5] = ["###", "#..", "###", "..#", "###"];
    const EIGHT: [&str; 5] = ["###", "#.#", "###", "#.#", "###"];
    const ZERO: [&str; 5] = ["###", "#.#", "#.#", "#.#", "###"];

    #[test]
    fn reads_a_pixel_font_at_other_sizes() {
        let mut font = Font::default();
        let at = |u: f32| [draw(TWO, u), draw(FIVE, u), draw(EIGHT, u), draw(ZERO, u)];
        font.learn(&at(3.2), 4, "2580", false);
        assert_eq!(font.grids.len(), 4);
        for u in [2.2, 3.0, 4.6, 7.0] {
            let read = font.read(&at(u), 4).map(|r| r.n);
            assert_eq!(read, Some(2580), "font pixel {u}");
        }
        // 0 and 8 differ by one cell: never confused.
        assert_eq!(font.read(&[draw(ZERO, 5.0), draw(EIGHT, 5.0)], 4).map(|r| r.n), Some(8));
    }

    #[test]
    fn keeps_grids_in_the_file() {
        let path = std::env::temp_dir().join("ferret-grid-test.digits");
        let mut font = Font::default();
        font.learn(&[draw(TWO, 3.2), draw(FIVE, 3.2)], 4, "25", false);
        font.save(&path).unwrap();
        let back = Font::load(&path);
        std::fs::remove_file(&path).ok();
        assert_eq!(back.read(&[draw(FIVE, 6.0), draw(TWO, 6.0)], 4).map(|r| r.n), Some(52));
    }

    #[test]
    fn keeps_lines_from_newer_builds() {
        let path = std::env::temp_dir().join("ferret-other-test.digits");
        let mut font = Font::default();
        font.learn(&[draw(TWO, 3.2), draw(FIVE, 3.2)], 4, "25", false);
        font.save(&path).unwrap();
        let future = "2 vector 3 1,2,3\n5 vector 3 4,5,6\n";
        std::fs::write(&path, std::fs::read_to_string(&path).unwrap() + future).unwrap();
        let mut back = Font::load(&path);
        assert_eq!(back.grids.len(), 2);
        back.save(&path).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with(future));
        back.forget(5);
        back.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(text.contains("2 vector") && !text.contains("5 vector") && !text.contains("5 grid"));
    }
}
