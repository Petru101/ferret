// Bars: how full a bar on screen is (health, shields or ammo shown without digits). A search for
// one goes by shares of the whole: the value now against the value before is the bar now against
// the bar before.

use image::RgbImage;

use crate::ocr::Rect;

/// Colours this close (the channels' differences added up, 0..765) are the same paint.
const SAME: u32 = 90;

pub struct Bar {
    /// The bar inside the picked box, without the outline around it.
    pub rect: Rect,
    /// Fills along its height (a box taller than wide).
    vertical: bool,
    /// The colour of the full part at each place across the bar (a gradient across it is fine).
    full: Vec<[u8; 3]>,
    /// Lines across the bar in neither the full nor the empty paint when picked (digits on a
    /// patch of their own: Shattered Pixel Dungeon's "20/20" in the middle of its health bar):
    /// they change with the value, so they are judged by the lines beside them.
    holes: Vec<bool>,
}

const NOT_A_BAR: &str = "The box doesn't look like a bar (one colour from one end, maybe another after it): draw it close around the bar.";

/// The first and last of `lines` (average colours) in the paint most of them share, when
/// nearly all of those between them are in it too.
fn paint(lines: &[[u8; 3]]) -> Option<(u32, u32)> {
    let median = median(lines);
    let like = |p: &[u8; 3]| diff(*p, median) <= SAME;
    let first = lines.iter().position(like)?;
    let last = lines.iter().rposition(like)?;
    let inside = &lines[first..=last];
    (inside.iter().filter(|p| like(p)).count() as f64 >= 0.8 * inside.len() as f64).then_some((first as u32, last as u32))
}

/// The colour most `lines` are near: each channel's median.
fn median(lines: &[[u8; 3]]) -> [u8; 3] {
    let mut sorted = lines.to_vec();
    std::array::from_fn(|c| {
        sorted.sort_by_key(|p| p[c]);
        sorted[sorted.len() / 2][c]
    })
}

fn diff(a: [u8; 3], b: [u8; 3]) -> u32 {
    a.iter().zip(&b).map(|(x, y)| x.abs_diff(*y) as u32).sum()
}

/// How colourful: empty parts of bars are dark or grey.
fn vivid(p: [u8; 3]) -> u32 {
    (*p.iter().max().unwrap() - *p.iter().min().unwrap()) as u32
}

fn mean(px: impl Iterator<Item = [u8; 3]>) -> [u8; 3] {
    let (mut sum, mut n) = ([0u64; 3], 0u64);
    for p in px {
        (0..3).for_each(|c| sum[c] += p[c] as u64);
        n += 1;
    }
    sum.map(|s| (s / n.max(1)) as u8)
}

/// Stretches of one paint along `slices`: (first, past the last, average colour). A sharp
/// step from one line to the next starts a new one (a black outline beside a dark empty part
/// is near it in colour, but not a smooth change); a gradient doesn't.
fn runs(slices: &[[u8; 3]]) -> Vec<(usize, usize, [u8; 3])> {
    let mut runs: Vec<(usize, usize, [u8; 3])> = Vec::new();
    for (i, &p) in slices.iter().enumerate() {
        match runs.last_mut() {
            Some(r) if diff(slices[i - 1], p) <= SAME / 2 => r.1 = i + 1,
            _ => runs.push((i, i + 1, p)),
        }
    }
    for r in &mut runs {
        r.2 = mean(slices[r.0..r.1].iter().copied());
    }
    runs
}

impl Bar {
    /// The bar in `area` of the frame, full or not: long and thin, one paint from one end (the
    /// full part) and maybe another after it (the empty part: the duller one, or the top or
    /// right one when both are as colourful). Age of War's base can't be refilled to pick it
    /// full. Lines of other colours at its ends (an outline, the box taking in a little
    /// background) are left out.
    pub fn pick(img: &RgbImage, area: Rect) -> Result<Bar, String> {
        let x0 = area.x.min(img.width());
        let y0 = area.y.min(img.height());
        let rect = Rect { x: x0, y: y0, w: area.w.min(img.width() - x0), h: area.h.min(img.height() - y0) };
        let vertical = rect.h > rect.w;
        let (long, short) = if vertical { (rect.h, rect.w) } else { (rect.w, rect.h) };
        if long < 12 || long < 3 * short {
            return Err("The box isn't long and thin like a bar.".into());
        }
        let at = |i: u32, j: u32| -> [u8; 3] {
            let (x, y) = if vertical { (rect.x + j, rect.y + i) } else { (rect.x + i, rect.y + j) };
            img.get_pixel(x, y).0
        };
        // Along the bar through the middle of the box (an outline or background above and below
        // would blur the bar's colours together).
        let middle = short / 3..(short - short / 3).max(short / 3 + 1);
        let slices: Vec<[u8; 3]> = (0..long).map(|i| mean(middle.clone().map(|j| at(i, j)))).collect();
        let runs = runs(&slices);
        let big = (long as usize / 50).max(3);
        let ends = (runs.iter().find(|r| r.1 - r.0 >= big), runs.iter().rev().find(|r| r.1 - r.0 >= big));
        let (Some(a), Some(b)) = ends else { return Err(NOT_A_BAR.into()) };
        let (first, last) = (a.0 as u32, b.1 as u32 - 1);
        if (first + (long - 1 - last)) as f64 > (0.1 * long as f64).max(4.0) {
            return Err(NOT_A_BAR.into());
        }
        let (fill, empty) = match diff(a.2, b.2) <= SAME {
            true => (a.2, None),
            false if vivid(a.2) > vivid(b.2) + 40 => (a.2, Some(b.2)),
            false if vivid(b.2) > vivid(a.2) + 40 => (b.2, Some(a.2)),
            // As colourful: bars fill from the bottom or the left.
            false if vertical => (b.2, Some(a.2)),
            false => (a.2, Some(b.2)),
        };
        // Lines in neither paint (digits) are few, and the full part is at one end.
        let kind: Vec<u8> = slices[first as usize..=last as usize]
            .iter()
            .map(|&p| if diff(p, fill) <= SAME { 1 } else if empty.is_some_and(|e| diff(p, e) <= SAME) { 2 } else { 0 })
            .collect();
        let n = kind.len();
        if kind.iter().filter(|&&k| k == 0).count() * 5 > n {
            return Err(NOT_A_BAR.into());
        }
        // Where the full part ends: the split with the fewest lines on the wrong side (dark
        // digits on the full part look like the empty one), from either end.
        let (mut edge, mut fill_first, mut wrong) = (n, true, usize::MAX);
        if empty.is_some() {
            let empties_before: Vec<usize> = std::iter::once(0).chain(kind.iter().scan(0, |c, &k| { *c += (k == 2) as usize; Some(*c) })).collect();
            let fills_before: Vec<usize> = std::iter::once(0).chain(kind.iter().scan(0, |c, &k| { *c += (k == 1) as usize; Some(*c) })).collect();
            for p in 0..=n {
                let a = empties_before[p] + fills_before[n] - fills_before[p];
                let b = fills_before[p] + empties_before[n] - empties_before[p];
                if a < wrong {
                    (edge, fill_first, wrong) = (p, true, a);
                }
                if b < wrong {
                    (edge, fill_first, wrong) = (p, false, b);
                }
            }
            if wrong * 10 > n {
                return Err(NOT_A_BAR.into());
            }
        }
        let full_side = |i: usize| (i < edge) == fill_first;
        // Across it: the outline's sides, from the full part.
        let full_lines: Vec<u32> = (0..n as u32).filter(|&i| kind[i as usize] == 1 && full_side(i as usize)).map(|i| first + i).collect();
        let across: Vec<[u8; 3]> = (0..short).map(|j| mean(full_lines.iter().map(|&i| at(i, j)))).collect();
        let (top, bottom) = paint(&across).unwrap_or((0, short - 1));
        let rect = match vertical {
            true => Rect { x: rect.x + top, w: bottom - top + 1, y: rect.y + first, h: last - first + 1 },
            false => Rect { x: rect.x + first, w: last - first + 1, y: rect.y + top, h: bottom - top + 1 },
        };
        let at = |i: u32, j: u32| -> [u8; 3] {
            let (x, y) = if vertical { (rect.x + j, rect.y + i) } else { (rect.x + i, rect.y + j) };
            img.get_pixel(x, y).0
        };
        let short = bottom - top + 1;
        let profile = |lines: &[u32]| -> Vec<[u8; 3]> {
            (0..short).map(|j| median(&lines.iter().map(|&i| at(i - first, j)).collect::<Vec<_>>())).collect()
        };
        let full = profile(&full_lines);
        let empty_lines: Vec<u32> = (0..n as u32).filter(|&i| kind[i as usize] == 2 && !full_side(i as usize)).map(|i| first + i).collect();
        let empty = (!empty_lines.is_empty()).then(|| profile(&empty_lines));
        let mostly = |i: u32, paint: &[[u8; 3]]| (0..short).filter(|&j| diff(at(i, j), paint[j as usize]) <= SAME).count() * 2 >= short as usize;
        // Lines not in their side's paint: digits, judged by the lines beside them. Widened a
        // little: other digits draw a wider patch (SPD's "20/20" against "15/20" made the lines
        // beside it read as empty, and the patch with them).
        let off: Vec<bool> = (0..n as u32)
            .map(|i| match (full_side(i as usize), &empty) {
                (true, _) | (false, None) => !mostly(i, &full),
                (false, Some(e)) => !mostly(i, e),
            })
            .collect();
        let margin = (n / 33).max(3);
        let holes = (0..n).map(|i| off[i.saturating_sub(margin)..(i + margin + 1).min(n)].iter().any(|&o| o)).collect();
        Ok(Bar { rect, vertical, full, holes })
    }

    /// How full the bar is in `img`, and how far off that may be either way (shares of the
    /// whole): the share of its length in the full part's paint, each line across it counted
    /// when most of its pixels are. Lines in a hole take the state
    /// of the lines on both sides; when those differ, the edge is somewhere in the hole: half of
    /// it counts, and the error grows to half of it.
    pub fn fill(&self, img: &RgbImage) -> (f64, f64) {
        let r = self.rect;
        if r.x + r.w > img.width() || r.y + r.h > img.height() {
            return (0.0, 1.0);
        }
        let (long, short) = if self.vertical { (r.h, r.w) } else { (r.w, r.h) };
        let state: Vec<Option<bool>> = (0..long)
            .map(|i| {
                if self.holes[i as usize] {
                    return None;
                }
                let same = (0..short)
                    .filter(|&j| {
                        let (x, y) = if self.vertical { (j, i) } else { (i, j) };
                        diff(img.get_pixel(r.x + x, r.y + y).0, self.full[j as usize]) <= SAME
                    })
                    .count();
                Some(same * 2 >= short as usize)
            })
            .collect();
        let (mut filled, mut err, mut i) = (0.0, self.error(), 0);
        while i < state.len() {
            if let Some(full) = state[i] {
                filled += full as u32 as f64;
                i += 1;
                continue;
            }
            let end = (i..state.len()).find(|&k| state[k].is_some()).unwrap_or(state.len());
            let before = state[..i].iter().rev().find_map(|s| *s);
            let after = state.get(end).copied().flatten();
            let n = (end - i) as f64;
            filled += match (before.or(after), after.or(before)) {
                (Some(true), Some(true)) => n,
                (Some(a), Some(b)) if a != b => {
                    err = err.max(n / 2.0 / long as f64);
                    n / 2.0
                }
                _ => 0.0,
            };
            i = end;
        }
        (filled / long as f64, err)
    }

    /// How far off a measure may be either way, as a share of the whole, away from holes.
    pub fn error(&self) -> f64 {
        let long = if self.vertical { self.rect.h } else { self.rect.w };
        (1.5 / long as f64).max(0.01)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    /// A 100x8 red bar with a 2 px black outline on grey, `hp` of 100 full (dark red empty).
    fn frame(hp: u32) -> RgbImage {
        let mut img = RgbImage::from_pixel(200, 40, Rgb([90, 90, 90]));
        for y in 10..22 {
            for x in 20..124 {
                let edge = !(12..20).contains(&y) || !(22..122).contains(&x);
                let p = if edge { [0, 0, 0] } else if x - 22 < hp { [200 + (y as u8 - 12) * 5, 30, 30] } else { [60, 10, 10] };
                img.put_pixel(x, y, Rgb(p));
            }
        }
        img
    }

    #[test]
    fn measures_a_bar_by_its_colour_when_full() {
        let bar = Bar::pick(&frame(100), Rect { x: 18, y: 8, w: 108, h: 16 }).unwrap();
        assert_eq!((bar.rect.x, bar.rect.w), (22, 100));
        assert_eq!(bar.fill(&frame(100)).0, 1.0);
        assert!((bar.fill(&frame(73)).0 - 0.73).abs() <= bar.error());
        assert!((bar.fill(&frame(5)).0 - 0.05).abs() <= bar.error());
        assert_eq!(bar.fill(&frame(0)).0, 0.0);
    }

    /// `frame`, with a dark patch of "digits" in the middle (x 62..82) that change with `hp`.
    fn with_digits(hp: u32) -> RgbImage {
        let mut img = frame(hp);
        for y in 12..20 {
            for x in 62..82 {
                let ink = (x + y + hp) % 3 == 0;
                img.put_pixel(x, y, Rgb(if ink { [230, 200, 210] } else { [40, 20, 25] }));
            }
        }
        img
    }

    #[test]
    fn judges_digits_on_the_bar_by_the_lines_beside_them() {
        let bar = Bar::pick(&with_digits(100), Rect { x: 18, y: 8, w: 108, h: 16 }).unwrap();
        for hp in [95, 70, 30, 5] {
            let (f, err) = bar.fill(&with_digits(hp));
            assert!((f - hp as f64 / 100.0).abs() <= err, "{hp}: {f} ± {err}");
        }
        // The edge inside the patch: half of it, the error as wide.
        let (f, err) = bar.fill(&with_digits(50));
        assert!((f - 0.5).abs() <= err && err >= 0.1, "{f} ± {err}");
    }

    #[test]
    fn refuses_a_box_that_is_not_a_bar() {
        assert!(Bar::pick(&frame(100), Rect { x: 18, y: 8, w: 20, h: 16 }).is_err());
    }

    #[test]
    fn measures_a_bar_picked_half_full() {
        let bar = Bar::pick(&frame(50), Rect { x: 18, y: 8, w: 108, h: 16 }).unwrap();
        assert_eq!((bar.rect.x, bar.rect.w), (22, 100));
        for hp in [50, 25, 80, 100] {
            let (f, err) = bar.fill(&frame(hp));
            assert!((f - hp as f64 / 100.0).abs() <= err, "{hp}: {f} ± {err}");
        }
    }
}


