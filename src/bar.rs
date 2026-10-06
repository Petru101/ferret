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
    /// The bar as picked (full), row by row.
    full: Vec<[u8; 3]>,
}

const NOT_FULL: &str = "The box isn't one colour along its length: pick the bar while it's full, with a box close around it.";

/// The first and last of `lines` (average colours) in the paint most of them share, when
/// nearly all of those between them are in it too.
fn paint(lines: &[[u8; 3]]) -> Option<(u32, u32)> {
    let mut sorted = lines.to_vec();
    let median: [u8; 3] = std::array::from_fn(|c| {
        sorted.sort_by_key(|p| p[c]);
        sorted[sorted.len() / 2][c]
    });
    let like = |p: &[u8; 3]| diff(*p, median) <= SAME;
    let first = lines.iter().position(like)?;
    let last = lines.iter().rposition(like)?;
    let inside = &lines[first..=last];
    (inside.iter().filter(|p| like(p)).count() as f64 >= 0.8 * inside.len() as f64).then_some((first as u32, last as u32))
}

fn diff(a: [u8; 3], b: [u8; 3]) -> u32 {
    a.iter().zip(&b).map(|(x, y)| x.abs_diff(*y) as u32).sum()
}

fn mean(px: impl Iterator<Item = [u8; 3]>) -> [u8; 3] {
    let (mut sum, mut n) = ([0u64; 3], 0u64);
    for p in px {
        (0..3).for_each(|c| sum[c] += p[c] as u64);
        n += 1;
    }
    sum.map(|s| (s / n.max(1)) as u8)
}

impl Bar {
    /// The bar in `area` of the frame, picked while full: it has to be long, thin and mostly
    /// one paint along its length. Lines of other colours at its ends (an outline, the box
    /// taking in a little background) are left out.
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
        // would blur the bar's colours together), then across it (the outline's top and bottom).
        let middle = short / 3..(short - short / 3).max(short / 3 + 1);
        let slices: Vec<[u8; 3]> = (0..long).map(|i| mean(middle.clone().map(|j| at(i, j)))).collect();
        let (first, last) = paint(&slices).ok_or(NOT_FULL)?;
        if (first + (long - 1 - last)) as f64 > (0.1 * long as f64).max(4.0) {
            return Err(NOT_FULL.into());
        }
        let lines: Vec<[u8; 3]> = (0..short).map(|j| mean((first..=last).map(|i| at(i, j)))).collect();
        let (top, bottom) = paint(&lines).unwrap_or((0, short - 1));
        let rect = match vertical {
            true => Rect { x: rect.x + top, w: bottom - top + 1, y: rect.y + first, h: last - first + 1 },
            false => Rect { x: rect.x + first, w: last - first + 1, y: rect.y + top, h: bottom - top + 1 },
        };
        let full = (0..rect.h).flat_map(|y| (0..rect.w).map(move |x| (x, y))).map(|(x, y)| img.get_pixel(rect.x + x, rect.y + y).0).collect();
        Ok(Bar { rect, vertical, full })
    }

    /// How full the bar is in `img`: the share of its length that still looks as when picked,
    /// each line across it counted when most of its pixels do (digits drawn over the bar change
    /// a few).
    pub fn fill(&self, img: &RgbImage) -> f64 {
        let r = self.rect;
        if r.x + r.w > img.width() || r.y + r.h > img.height() {
            return 0.0;
        }
        let (long, short) = if self.vertical { (r.h, r.w) } else { (r.w, r.h) };
        let filled = (0..long)
            .filter(|&i| {
                let same = (0..short)
                    .filter(|&j| {
                        let (x, y) = if self.vertical { (j, i) } else { (i, j) };
                        diff(img.get_pixel(r.x + x, r.y + y).0, self.full[(y * r.w + x) as usize]) <= SAME
                    })
                    .count();
                same * 2 >= short as usize
            })
            .count();
        filled as f64 / long as f64
    }

    /// How far off a measure may be either way, as a share of the whole.
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
        assert_eq!(bar.fill(&frame(100)), 1.0);
        assert!((bar.fill(&frame(73)) - 0.73).abs() <= bar.error());
        assert!((bar.fill(&frame(5)) - 0.05).abs() <= bar.error());
        assert_eq!(bar.fill(&frame(0)), 0.0);
    }

    #[test]
    fn refuses_a_box_that_is_not_a_bar() {
        assert!(Bar::pick(&frame(100), Rect { x: 18, y: 8, w: 20, h: 16 }).is_err());
        // Half full: two colours along it.
        assert!(Bar::pick(&frame(50), Rect { x: 18, y: 8, w: 108, h: 16 }).is_err());
    }
}
