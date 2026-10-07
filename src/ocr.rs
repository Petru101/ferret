// Reads numbers from captured frames: PaddleOCR (`reader`) inside the player's rectangle, and the
// game's own digits once learned (`font`), which also find its numbers on the whole frame.

use std::path::Path;

use image::{imageops, RgbImage};

use crate::font::{Font, FontRead, Glyph};

/// Crops are enlarged this much before they are split into glyphs.
const SCALE: u32 = 4;

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone)]
pub struct Word {
    pub text: String,
    pub rect: Rect,
    pub conf: f32,
}

/// A number as the game shows it: "1250", "1.2" (a decimal) or "3:17" (minutes:seconds).
#[derive(Clone, Debug, PartialEq)]
pub struct Shown(String);

impl Shown {
    /// "1,250" and "1'250" (thousands separators go), "1.2", "3:17", "1:02:03", "-5"; "13/40" (current/most)
    /// is its first part, or its second when the first is missing ("/40"). None for anything else.
    pub fn parse(text: &str) -> Option<Shown> {
        let text = text.trim();
        if let Some((a, b)) = text.split_once('/') {
            return Shown::parse(a).or_else(|| a.trim().is_empty().then(|| Shown::parse(b)).flatten());
        }
        let t: String = text.chars().filter(|c| !matches!(c, ',' | '\'' | '\u{2019}')).collect();
        let (sign, body) = t.strip_prefix('-').map_or(("", t.as_str()), |b| ("-", b));
        let digits = |p: &str| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit());
        let ok = if body.contains(':') {
            let parts: Vec<&str> = body.split(':').collect();
            sign.is_empty() && (2..=3).contains(&parts.len()) && digits(parts[0]) && parts[1..].iter().all(|p| p.len() == 2 && digits(p))
        } else if let Some((a, b)) = body.split_once('.') {
            digits(a) && digits(b)
        } else {
            digits(body)
        };
        // Leading zeros ("01", "007") are drawn, not part of the value: learning takes glyphs
        // left of the digits for zeros.
        let lead = body.len() - body.trim_start_matches('0').len();
        let lead = lead.min(body.find([':', '.']).unwrap_or(body.len()).saturating_sub(1));
        ok.then(|| Shown(format!("{sign}{}", &body[lead..])))
    }

    pub fn whole(n: i64) -> Shown {
        Shown(n.to_string())
    }

    /// The digits as drawn, for learning their shapes: "1.2" -> "12", "3:17" -> "317".
    pub fn digits(&self) -> String {
        self.0.chars().filter(char::is_ascii_digit).collect()
    }

    pub fn decimals(&self) -> u32 {
        self.0.split_once('.').map_or(0, |(_, b)| b.len() as u32)
    }

    /// The value it stands for: "1.2" -> 1.2, "3:17" -> 197 (seconds; games keep timers that way).
    pub fn value(&self) -> f64 {
        if self.0.contains(':') {
            return self.0.split(':').fold(0.0, |t, p| t * 60.0 + p.parse::<f64>().unwrap_or(0.0));
        }
        self.0.parse().unwrap_or(0.0)
    }

    /// What the helper searches for: "1.2" (floats near it, and 12 in whole numbers), "197".
    pub fn search(&self) -> String {
        if self.0.contains(':') { (self.value() as i64).to_string() } else { self.0.clone() }
    }

    /// The digits as one whole number: "1.2" -> 12 (how a game may keep tenths).
    pub fn scaled(&self) -> i64 {
        let n: i64 = self.digits().parse().unwrap_or(0);
        if self.0.starts_with('-') { -n } else { n }
    }
}

impl std::fmt::Display for Shown {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What small marks between two digits make, from their boxes and the digits' top and bottom:
/// "." (one dot at the bottom) or ":" (one in the middle and one at the bottom).
fn separator(marks: &[(u32, u32, u32, u32)], top: u32, bottom: u32) -> Option<char> {
    let h = bottom - top + 1;
    let dots: Vec<&(u32, u32, u32, u32)> =
        marks.iter().filter(|m| m.2 - m.0 < h / 3 + 1 && m.3 - m.1 < h / 3 + 1 && m.1 >= top && m.3 <= bottom + 1).collect();
    let low = dots.iter().any(|m| m.3 + h / 6 >= bottom);
    let middle = dots.iter().any(|m| m.1 > top + h / 5 && m.3 < top + h * 2 / 3);
    match (low, middle) {
        (true, true) => Some(':'),
        (true, false) => Some('.'),
        _ => None,
    }
}

pub fn digits(text: &str) -> Option<i64> {
    let d: String = text.chars().filter(char::is_ascii_digit).collect();
    d.parse().ok()
}

pub fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// Every number in the frame, for the player to click: the learned digits' (exact, the game's
/// own font) and PaddleOCR's where those found none.
pub fn numbers(frame: &Path, font: Option<&Font>) -> Result<Vec<Word>, String> {
    let img = image::open(frame).map_err(|e| e.to_string())?.to_rgb8();
    let by_font = font.filter(|f| !f.is_empty()).map(|f| font_numbers(&img, f)).unwrap_or_default();
    let mut found: Vec<Word> = crate::reader::numbers(&img)?.into_iter().filter(|w| !by_font.iter().any(|f| overlaps(f.rect, w.rect))).collect();
    found.extend(by_font);
    Ok(found)
}

/// Numbers on the whole frame drawn in the learned digits: shapes of one flat colour that are
/// learned digits, side by side at the same height. A row that also has other shapes of that
/// colour and size is a word ("GOLD" has an O), not a number.
fn font_numbers(img: &RgbImage, font: &Font) -> Vec<Word> {
    let blobs = glyph_blobs(img, font);
    font_rows(&blobs)
}

/// A shape of one quantised colour (8-connected), as a glyph candidate.
struct Blob {
    colour: (u8, u8, u8),
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    /// Glyph-sized and solid enough, not touching the frame's edge; else never part of a row.
    glyph: Option<Glyph>,
    digit: Option<u8>,
}

/// The frame's same-colour shapes, glyph-sized ones read by the learned digits.
fn glyph_blobs(img: &RgbImage, font: &Font) -> Vec<Blob> {
    let (w, h) = img.dimensions();
    let colour = |i: usize| {
        let p = img.as_raw();
        (p[i * 3] >> 5, p[i * 3 + 1] >> 5, p[i * 3 + 2] >> 5)
    };
    // Pieces: colour, box, pixel count.
    let mut label = vec![0u32; (w * h) as usize];
    let mut pieces: Vec<((u8, u8, u8), u32, u32, u32, u32, u32)> = Vec::new();
    let mut stack = Vec::new();
    for start in 0..label.len() {
        if label[start] != 0 {
            continue;
        }
        let c = colour(start);
        let id = pieces.len() as u32 + 1;
        label[start] = id;
        stack.push(start);
        let (mut x0, mut y0, mut x1, mut y1, mut n) = (w, h, 0, 0, 0);
        while let Some(i) = stack.pop() {
            n += 1;
            let (x, y) = ((i as u32) % w, (i as u32) / w);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            let mut visit = |j: usize| {
                if label[j] == 0 && colour(j) == c {
                    label[j] = id;
                    stack.push(j);
                }
            };
            if x > 0 {
                visit(i - 1);
            }
            if x + 1 < w {
                visit(i + 1);
            }
            if y > 0 {
                visit(i - w as usize);
            }
            if y + 1 < h {
                visit(i + w as usize);
            }
            // Corners too: anti-aliased diagonals (a slash, the 4's stroke) only touch at corners
            // in the flat colour, and would fall apart into slivers.
            let wu = w as usize;
            if x > 0 && y > 0 {
                visit(i - wu - 1);
            }
            if x + 1 < w && y > 0 {
                visit(i - wu + 1);
            }
            if x > 0 && y + 1 < h {
                visit(i + wu - 1);
            }
            if x + 1 < w && y + 1 < h {
                visit(i + wu + 1);
            }
        }
        pieces.push((c, x0, y0, x1, y1, n));
    }
    // Pieces of one colour stacked with at most 2 empty rows between them make one shape: a
    // small font's flat-colour core breaks where its strokes meet (Creeper World's 8: a top
    // loop, two sides and a bottom bar). A colon's dots are further apart.
    let mut root: Vec<usize> = (0..pieces.len()).collect();
    fn find(root: &mut [usize], mut i: usize) -> usize {
        while root[i] != i {
            root[i] = root[root[i]];
            i = root[i];
        }
        i
    }
    let small = |p: &((u8, u8, u8), u32, u32, u32, u32, u32)| p.3 - p.1 < 120 && p.4 - p.2 < 80;
    let mut by_colour: std::collections::HashMap<(u8, u8, u8), Vec<usize>> = Default::default();
    for (i, p) in pieces.iter().enumerate().filter(|(_, p)| small(p)) {
        by_colour.entry(p.0).or_default().push(i);
    }
    for list in by_colour.values_mut() {
        list.sort_by_key(|&i| pieces[i].2);
        for (k, &i) in list.iter().enumerate() {
            let a = pieces[i];
            let start = k + list[k..].partition_point(|&j| pieces[j].2 <= a.4);
            for &j in list[start..].iter().take_while(|&&j| pieces[j].2 <= a.4 + 3) {
                let b = pieces[j];
                // Within 2 px sideways: the 8's bottom bar sits between its two sides.
                if b.1 <= a.3 + 2 && a.1 <= b.3 + 2 && b.4.max(a.4) - b.2.min(a.2) < 80 {
                    let (ra, rb) = (find(&mut root, i), find(&mut root, j));
                    root[ra] = rb;
                }
            }
        }
    }
    let mut groups: std::collections::HashMap<usize, (u32, u32, u32, u32, u32)> = Default::default();
    for i in 0..pieces.len() {
        let p = pieces[i];
        let g = groups.entry(find(&mut root, i)).or_insert((w, h, 0, 0, 0));
        *g = (g.0.min(p.1), g.1.min(p.2), g.2.max(p.3), g.3.max(p.4), g.4 + p.5);
    }
    let mut blobs = Vec::with_capacity(groups.len());
    for (r, (x0, y0, x1, y1, n)) in groups {
        let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);
        // Glyph-sized and solid enough; not touching the frame's edge.
        // 20%: anti-aliased fonts with 2-px strokes leave only a thin core of flat colour
        // (Creeper World's 4 fills 23% of its box).
        let sized = (6..=80).contains(&bh) && bw * 2 <= bh * 3 && n * 10 >= bw * bh * 2;
        let inside = x0 > 0 && y0 > 0 && x1 + 1 < w && y1 + 1 < h;
        let glyph = (sized && inside).then(|| {
            let ink = (0..bw * bh)
                .map(|k| {
                    let l = label[((y0 + k / bw) * w + x0 + k % bw) as usize];
                    l != 0 && find(&mut root, l as usize - 1) == r
                })
                .collect();
            Glyph { x: x0, w: bw, h: bh, ink, cut: false }
        });
        let digit = glyph.as_ref().and_then(|g| font.digit_of(g, 1));
        blobs.push(Blob { colour: pieces[r].0, x0, y0, x1, y1, glyph, digit });
    }
    blobs
}

/// A slash: a thin stroke leaning right, from the bottom left to the top right ("13/40").
fn is_slash(g: &Glyph) -> bool {
    let (w, h) = (g.w as usize, g.h as usize);
    if w * 10 > h * 7 || w < 3 {
        return false;
    }
    // Each row's ink: narrow, and its middle moving left going down.
    let mut mids = Vec::new();
    for y in 0..h {
        let xs: Vec<usize> = (0..w).filter(|&x| g.ink[y * w + x]).collect();
        let (Some(&a), Some(&b)) = (xs.first(), xs.last()) else { continue };
        if b - a + 1 > (w / 2).max(3) {
            return false;
        }
        mids.push((a + b) as i32);
    }
    mids.len() * 5 >= h * 4 && mids.windows(2).all(|m| m[1] <= m[0] + 1) && mids[0] - mids[mids.len() - 1] >= (w as i32 - 1)
}

/// The colour-group reader's candidates for `area` and their glyphs, in frame pixels, "/" for
/// slashes (for examples/ocr.rs).
#[allow(dead_code)]
pub fn crop_glyphs(frame: &Path, area: Rect) -> Result<Vec<String>, String> {
    let img = image::open(frame).map_err(|e| e.to_string())?.to_rgb8();
    Ok(candidates(&img, area, false)
        .iter()
        .map(|c| {
            let g: Vec<String> = c
                .glyphs
                .iter()
                .map(|g| format!("{}{}+{}x{}{}", if is_slash(g) { "/" } else { "" }, area.x + g.x / SCALE, g.w / SCALE, g.h / SCALE, if g.cut { "cut" } else { "" }))
                .collect();
            format!("glyph h {}: {}", c.glyph_h / SCALE, g.join(" "))
        })
        .collect())
}

/// The ways to read glyphs as one number: all of them, or for "13/40" (current/most) the part
/// before the slash, then the part after it. Glyphs are in reading order.
fn number_parts(glyphs: &[Glyph]) -> Vec<Vec<Glyph>> {
    let Some(s) = glyphs.iter().position(|g| !g.cut && is_slash(g)) else { return vec![glyphs.to_vec()] };
    [&glyphs[..s], &glyphs[s + 1..]].into_iter().filter(|p| p.iter().any(|g| !g.cut)).map(<[Glyph]>::to_vec).collect()
}

/// The glyphs of the number a crop shows: for "13/40" the first part (the current value).
fn number_glyphs(glyphs: &[Glyph]) -> Vec<Glyph> {
    number_parts(glyphs).into_iter().next().unwrap_or_default()
}

/// The glyph-sized shapes inside `region` of a frame: where, colour, and the digit they read
/// as (for examples/ocr.rs, to see why a number isn't found).
#[allow(dead_code)]
pub fn glyphs_in(frame: &Path, font: &Font, region: Rect) -> Result<Vec<String>, String> {
    let img = image::open(frame).map_err(|e| e.to_string())?.to_rgb8();
    let inside = |b: &Blob| b.x0 >= region.x && b.y0 >= region.y && b.x1 < region.x + region.w && b.y1 < region.y + region.h;
    Ok(glyph_blobs(&img, font)
        .iter()
        .filter(|b| b.glyph.is_some() && inside(b))
        .map(|b| {
            let g = b.glyph.as_ref().unwrap();
            let fill = g.ink.iter().filter(|i| **i).count() * 100 / g.ink.len();
            let digit = b.digit.map_or(if is_slash(g) { "/".into() } else { "-".into() }, |d| d.to_string());
            format!("{},{} {}x{} colour {:?} fill {fill}% digit {digit}", b.x0, b.y0, g.w, g.h, b.colour)
        })
        .collect())
}

/// Rows of glyphs that are all learned digits: the numbers on the frame.
fn font_rows(blobs: &[Blob]) -> Vec<Word> {
    // Rows: glyphs of one colour, about the same top and height, close together.
    // Slashes and other ink of a glyph's colour and about its height (too thin to be a glyph)
    // separate numbers: "13/40" is 13 and 40, never 1340, and not a word either.
    let slash = |b: &Blob| b.digit.is_none() && b.glyph.as_ref().is_some_and(is_slash);
    let mut glyphs: Vec<&Blob> = blobs.iter().filter(|b| b.glyph.is_some() && !slash(b)).collect();
    glyphs.sort_by_key(|b| (b.colour, b.y0, b.x0));
    let mut marks: std::collections::HashMap<(u8, u8, u8), Vec<&Blob>> = Default::default();
    for b in blobs.iter().filter(|b| (b.glyph.is_none() || slash(b)) && (6..=80).contains(&(b.y1 - b.y0 + 1))) {
        marks.entry(b.colour).or_default().push(b);
    }
    // Small marks of a glyph's colour: dots of a decimal point or a colon. Sorted by x.
    let mut dots: std::collections::HashMap<(u8, u8, u8), Vec<&Blob>> = Default::default();
    for b in blobs.iter().filter(|b| b.glyph.is_none() && b.y1 - b.y0 < 27 && b.x1 - b.x0 < 27) {
        dots.entry(b.colour).or_default().push(b);
    }
    dots.values_mut().for_each(|d| d.sort_by_key(|b| b.x0));
    let sep = |a: &Blob, b: &Blob| {
        let d = dots.get(&a.colour)?;
        let from = d.partition_point(|m| m.x0 <= a.x1);
        let marks: Vec<(u32, u32, u32, u32)> =
            d[from..].iter().take_while(|m| m.x1 < b.x0).map(|m| (m.x0, m.y0, m.x1, m.y1)).collect();
        separator(&marks, a.y0.min(b.y0), a.y1.max(b.y1))
    };
    let between = |a: &Blob, b: &Blob| {
        let h = a.y1 - a.y0 + 1;
        marks.get(&a.colour).is_some_and(|m| {
            m.iter().any(|s| s.x0 > a.x1 && s.x1 < b.x0 && s.y1 - s.y0 + 1 >= h / 2 && s.y0 <= a.y1 && s.y1 >= a.y0)
        })
    };
    let next_to = |a: &Blob, b: &Blob| {
        let (ha, hb) = (a.y1 - a.y0 + 1, b.y1 - b.y0 + 1);
        a.colour == b.colour
            && ha.abs_diff(hb) <= ha / 4 + 1
            && a.y0.abs_diff(b.y0) <= ha / 4 + 1
            && b.x0 > a.x1
            && (b.x0 - a.x1 <= ha * 4 / 5 || b.x0 - a.x1 <= ha * 3 / 2 && sep(a, b).is_some())
            && !between(a, b)
    };
    let mut used = vec![false; glyphs.len()];
    let mut words = Vec::new();
    let mut ones = Vec::new(); // rows of only 1s, with their colour: a 1 is a plain bar
    for i in 0..glyphs.len() {
        if used[i] || glyphs[i].digit.is_none() {
            continue;
        }
        // Walk left to the start of the row, then right along it.
        let mut first = i;
        while let Some(j) = (0..glyphs.len()).find(|&j| !used[j] && next_to(glyphs[j], glyphs[first])) {
            first = j;
        }
        let mut row = vec![first];
        while let Some(j) = (0..glyphs.len()).find(|&j| !used[j] && !row.contains(&j) && next_to(glyphs[*row.last().unwrap()], glyphs[j])) {
            row.push(j);
        }
        row.iter().for_each(|&j| used[j] = true);
        let mut text = String::new();
        for (k, &j) in row.iter().enumerate() {
            let Some(d) = glyphs[j].digit else { break };
            if k > 0 {
                text.extend(sep(glyphs[row[k - 1]], glyphs[j]));
            }
            text.push(char::from(b'0' + d));
        }
        // A glyph that isn't a digit makes it a word; "1.2.3" isn't a number either.
        if text.chars().filter(char::is_ascii_digit).count() != row.len() || Shown::parse(&text).is_none() {
            continue;
        }
        let (x0, y0) = (row.iter().map(|&j| glyphs[j].x0).min().unwrap(), row.iter().map(|&j| glyphs[j].y0).min().unwrap());
        let (x1, y1) = (row.iter().map(|&j| glyphs[j].x1).max().unwrap(), row.iter().map(|&j| glyphs[j].y1).max().unwrap());
        let word = Word { text, rect: Rect { x: x0, y: y0, w: x1 - x0 + 1, h: y1 - y0 + 1 }, conf: 99.0 };
        if word.text.bytes().all(|c| c == b'1') {
            ones.push((glyphs[first].colour, word));
        } else {
            words.push((glyphs[first].colour, word));
        }
    }
    // Any solid bar reads as a 1: keep those only when a number with other digits, in the same
    // colour and height, says that's how numbers look here.
    let kept: Vec<(u8, u8, u8, u32)> = words.iter().map(|(c, w)| (c.0, c.1, c.2, w.rect.h)).collect();
    let vouched = |c: &(u8, u8, u8), w: &Word| kept.iter().any(|k| (k.0, k.1, k.2) == *c && k.3.abs_diff(w.rect.h) <= 1);
    ones.retain(|(c, w)| vouched(c, w));
    words.into_iter().chain(ones).map(|(_, w)| w).collect()
}

/// Grows a word's box so the number still fits when it gains digits.
pub fn watch_area(r: Rect) -> Rect {
    let pad = r.h.max(8);
    Rect { x: r.x.saturating_sub(2 * pad), y: r.y.saturating_sub(pad / 2), w: r.w + 4 * pad, h: r.h + pad }
}

fn dist2(a: [f32; 3], b: [f32; 3]) -> f32 {
    (0..3).map(|i| (a[i] - b[i]).powi(2)).sum()
}

fn nearest(p: [f32; 3], centers: &[[f32; 3]]) -> usize {
    (0..centers.len()).min_by(|&a, &b| dist2(p, centers[a]).total_cmp(&dist2(p, centers[b]))).unwrap()
}

/// Groups the crop's colours (k-means, at most 4). The first centre is the background, the most
/// common colour on the crop's border, and stays fixed.
fn colour_groups(img: &RgbImage) -> Vec<[f32; 3]> {
    let px: Vec<[f32; 3]> = img.pixels().map(|p| [p[0] as f32, p[1] as f32, p[2] as f32]).collect();
    let (w, h) = (img.width(), img.height());
    let mut buckets: std::collections::HashMap<[u8; 3], ([f32; 3], usize)> = Default::default();
    for (_, _, p) in img.enumerate_pixels().filter(|(x, y, _)| *x == 0 || *y == 0 || *x == w - 1 || *y == h - 1) {
        let b = buckets.entry([p[0] >> 4, p[1] >> 4, p[2] >> 4]).or_default();
        (0..3).for_each(|i| b.0[i] += p[i] as f32);
        b.1 += 1;
    }
    let (sum, n) = buckets.into_values().max_by_key(|b| b.1).unwrap();
    let mut centers = vec![[sum[0] / n as f32, sum[1] / n as f32, sum[2] / n as f32]];
    // Farthest-point start: each new centre is the pixel least like the existing ones.
    while centers.len() < 6 {
        let (p, d) = px
            .iter()
            .map(|p| (*p, centers.iter().map(|c| dist2(*p, *c)).fold(f32::MAX, f32::min)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        if d < 40.0 * 40.0 {
            break;
        }
        centers.push(p);
    }
    for _ in 0..8 {
        let mut sum = vec![([0.0f32; 3], 0usize); centers.len()];
        for p in &px {
            let s = &mut sum[nearest(*p, &centers)];
            (0..3).for_each(|i| s.0[i] += p[i]);
            s.1 += 1;
        }
        for (c, (s, n)) in centers.iter_mut().zip(sum).skip(1) {
            if n > 0 {
                *c = [s[0] / n as f32, s[1] / n as f32, s[2] / n as f32];
            }
        }
    }
    centers
}

/// What might be the number in one colour group: the height of its tallest glyph, where
/// its glyphs are (in the image, then in the frame) and the glyphs themselves, left to right.
struct Candidate {
    glyph_h: u32,
    rect: Rect,
    glyphs: Vec<Glyph>,
    /// Marks too small to be glyphs (crop pixels x0, y0, x1, y1), by x: dots of "1.2" or "3:17".
    marks: Vec<(u32, u32, u32, u32)>,
    /// Top and bottom of the glyphs, crop pixels.
    band: (u32, u32),
}

impl Candidate {
    /// The separator between two of this candidate's glyphs: "." or ":".
    fn separator(&self, a: &Glyph, b: &Glyph) -> Option<char> {
        let (from, to) = (a.x + a.w, b.x);
        let marks: Vec<(u32, u32, u32, u32)> = self.marks.iter().filter(|m| m.0 >= from && m.2 < to).copied().collect();
        separator(&marks, self.band.0, self.band.1)
    }

    /// A read of `glyphs` as the number shown, with the separators between the glyphs its
    /// digits come from (touching digits come from one glyph and have none).
    fn shown(&self, glyphs: &[Glyph], r: &FontRead) -> Option<Shown> {
        let mut text = String::new();
        for (i, c) in format!("{:0w$}", r.n, w = r.glyphs).chars().enumerate() {
            if i > 0 && r.from[i - 1] != r.from[i] {
                text.extend(self.separator(&glyphs[r.from[i - 1]], &glyphs[r.from[i]]));
            }
            text.push(c);
        }
        Shown::parse(&text)
    }
}

/// Keeps the glyph-like blobs of a mask (4x crop size): drops lines running across the whole
/// crop and anything under half the height of the tallest blob (unit labels, dots, noise).
/// With `cut_off`, also blobs touching the left or right edge: parts of neighbouring words.
fn glyphs(mask: &[bool], w: u32, h: u32, cut_off: bool) -> Option<Candidate> {
    let mut label = vec![0u32; mask.len()];
    let mut blobs: Vec<(u32, u32, u32, u32)> = Vec::new(); // x0, y0, x1, y1
    for start in 0..mask.len() {
        if !mask[start] || label[start] != 0 {
            continue;
        }
        blobs.push((w, h, 0, 0));
        let id = blobs.len() as u32;
        let mut stack = vec![start];
        label[start] = id;
        while let Some(i) = stack.pop() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            let b = blobs.last_mut().unwrap();
            *b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
            for (dx, dy) in [(-1i32, -1i32), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                    continue;
                }
                let j = (ny as u32 * w + nx as u32) as usize;
                if mask[j] && label[j] == 0 {
                    label[j] = id;
                    stack.push(j);
                }
            }
        }
    }
    let whole: Vec<bool> = blobs
        .iter()
        .map(|b| {
            let (left, right) = (b.0 == 0, b.2 == w - 1);
            !(left && right) && !(cut_off && (left || right))
        })
        .collect();
    let tallest = blobs.iter().zip(&whole).filter(|(_, k)| **k).map(|(b, _)| b.3 - b.1 + 1).max()?;
    let mut keep: Vec<bool> = blobs.iter().zip(&whole).map(|(b, k)| *k && (b.3 - b.1 + 1) * 2 >= tallest).collect();
    // A thin bar taller than the glyphs beside it is a border (Graveyard Keeper's item slots), not
    // a 1: a 1 is as tall as the other digits.
    let bar = |b: &(u32, u32, u32, u32)| !crate::font::could_be(0, b.2 - b.0 + 1, b.3 - b.1 + 1);
    let text_h = blobs.iter().zip(&keep).filter(|(b, k)| **k && !bar(b)).map(|(b, _)| b.3 - b.1 + 1).max();
    if let Some(t) = text_h {
        for (b, k) in blobs.iter().zip(keep.iter_mut()) {
            *k = *k && !(bar(b) && (b.3 - b.1 + 1) * 3 > t * 4);
        }
    }
    let tallest = blobs.iter().zip(&keep).filter(|(_, k)| **k).map(|(b, _)| b.3 - b.1 + 1).max()?;
    for (i, b) in blobs.iter().enumerate().filter(|(i, _)| keep[*i]) {
        unslash(&mut label, w, i as u32 + 1, *b);
    }
    let kept: Vec<_> = blobs.iter().zip(&keep).filter(|(_, k)| **k).map(|(b, _)| *b).collect();
    let (x0, y0) = (kept.iter().map(|b| b.0).min()?, kept.iter().map(|b| b.1).min()?);
    let (x1, y1) = (kept.iter().map(|b| b.2).max()?, kept.iter().map(|b| b.3).max()?);
    let rect = Rect { x: x0, y: y0, w: x1 - x0 + 1, h: y1 - y0 + 1 };
    let mut marks: Vec<(u32, u32, u32, u32)> = blobs.iter().zip(&keep).filter(|(_, k)| !**k).map(|(b, _)| *b).collect();
    marks.sort();
    // Blobs above one another (a digit broken in two) make one glyph.
    let mut boxes: Vec<((u32, u32, u32, u32), Vec<u32>)> = Vec::new();
    let mut order: Vec<usize> = (0..blobs.len()).filter(|i| keep[*i]).collect();
    order.sort_by_key(|i| blobs[*i].0);
    for i in order {
        let b = blobs[i];
        match boxes.last_mut() {
            Some((m, ids)) if b.0.max(m.0) + (b.2 - b.0).min(m.2 - m.0) / 2 <= b.2.min(m.2) => {
                *m = (m.0.min(b.0), m.1.min(b.1), m.2.max(b.2), m.3.max(b.3));
                ids.push(i as u32 + 1);
            }
            _ => boxes.push((b, vec![i as u32 + 1])),
        }
    }
    let glyphs = boxes
        .into_iter()
        .map(|((bx0, by0, bx1, by1), ids)| {
            let (gw, gh) = (bx1 - bx0 + 1, by1 - by0 + 1);
            let ink = (0..gw * gh).map(|j| ids.contains(&label[((by0 + j / gw) * w + bx0 + j % gw) as usize])).collect();
            Glyph { x: bx0, w: gw, h: gh, ink, cut: bx0 == 0 || bx1 == w - 1 }
        })
        .collect();
    Some(Candidate { glyph_h: tallest, rect, glyphs, marks, band: (y0, y1) })
}

/// Turns a slashed zero (Ø, common in HUD fonts) into a plain 0.
/// It has two holes placed diagonally; an 8 has them one above the other. Clearing the area
/// spanned by both holes removes the slash and leaves the ring.
fn unslash(label: &mut [u32], w: u32, id: u32, (x0, y0, x1, y1): (u32, u32, u32, u32)) {
    let (bw, bh) = (x1 - x0 + 3, y1 - y0 + 3); // the glyph's box with a 1-pixel margin
    let at = |x: u32, y: u32| (y0 + y - 1) as usize * w as usize + (x0 + x - 1) as usize;
    let inside = |x: u32, y: u32| x >= 1 && y >= 1 && x <= bw - 2 && y <= bh - 2;
    let is_glyph = |label: &[u32], x: u32, y: u32| inside(x, y) && label[at(x, y)] == id;
    // Background reachable from outside the glyph; whatever background is left is a hole.
    let mut outside = vec![false; (bw * bh) as usize];
    let mut stack = vec![(0u32, 0u32)];
    outside[0] = true;
    while let Some((x, y)) = stack.pop() {
        for (nx, ny) in [(x.wrapping_sub(1), y), (x + 1, y), (x, y.wrapping_sub(1)), (x, y + 1)] {
            if nx < bw && ny < bh && !outside[(ny * bw + nx) as usize] && !is_glyph(label, nx, ny) {
                outside[(ny * bw + nx) as usize] = true;
                stack.push((nx, ny));
            }
        }
    }
    let mut seen = outside.clone();
    let mut holes: Vec<(u32, u32, u32, u32, usize, f32, f32)> = Vec::new(); // box, size, centroid
    for start in 0..(bw * bh) {
        let (sx, sy) = (start % bw, start / bw);
        if seen[start as usize] || is_glyph(label, sx, sy) {
            continue;
        }
        let mut hole = (sx, sy, sx, sy, 0, 0.0, 0.0);
        let mut stack = vec![(sx, sy)];
        seen[start as usize] = true;
        while let Some((x, y)) = stack.pop() {
            hole = (hole.0.min(x), hole.1.min(y), hole.2.max(x), hole.3.max(y), hole.4 + 1, hole.5 + x as f32, hole.6 + y as f32);
            for (nx, ny) in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)] {
                if !seen[(ny * bw + nx) as usize] && !is_glyph(label, nx, ny) {
                    seen[(ny * bw + nx) as usize] = true;
                    stack.push((nx, ny));
                }
            }
        }
        holes.push(hole);
    }
    // Specks from anti-aliasing aren't holes.
    let big = ((bw * bh) as usize / 50).max(4);
    holes.retain(|h| h.4 >= big);
    let [a, b] = holes[..] else { return };
    let dx = (a.5 / a.4 as f32 - b.5 / b.4 as f32).abs();
    let dy = (a.6 / a.4 as f32 - b.6 / b.4 as f32).abs();
    if dx < dy / 2.0 || dy < dx / 2.0 {
        return;
    }
    for y in a.1.min(b.1)..=a.3.max(b.3) {
        for x in a.0.min(b.0)..=a.2.max(b.2) {
            if is_glyph(label, x, y) {
                label[at(x, y)] = 0;
            }
        }
    }
}

/// Clean images of what might be the number in `area`: one per colour group, plus everything
/// that isn't background (text drawn in two colours, or anti-aliased onto a gradient).
fn candidates(img: &RgbImage, area: Rect, cut_off: bool) -> Vec<Candidate> {
    let x = area.x.min(img.width().saturating_sub(1));
    let y = area.y.min(img.height().saturating_sub(1));
    let w = area.w.min(img.width() - x).max(1);
    let h = area.h.min(img.height() - y).max(1);
    let crop = imageops::crop_imm(img, x, y, w, h).to_image();
    let centers = colour_groups(&crop);
    let big = imageops::resize(&crop, w * SCALE, h * SCALE, imageops::FilterType::CatmullRom);
    let group: Vec<usize> =
        big.pixels().map(|p| nearest([p[0] as f32, p[1] as f32, p[2] as f32], &centers)).collect();
    let mut masks: Vec<Vec<bool>> = (1..centers.len()).map(|g| group.iter().map(|c| *c == g).collect()).collect();
    if centers.len() > 2 {
        masks.push(group.iter().map(|c| *c != 0).collect());
    }
    let mut cands: Vec<Candidate> = masks.iter().filter_map(|m| glyphs(m, w * SCALE, h * SCALE, cut_off)).collect();
    for c in &mut cands {
        let r = c.rect;
        c.rect = Rect { x: x + r.x / SCALE, y: y + r.y / SCALE, w: r.w.div_ceil(SCALE), h: r.h.div_ceil(SCALE) };
    }
    cands
}

/// Whether the rectangle's edge cuts a glyph right next to the number: it may be one of its
/// digits ("13" of "138"). One further away is something else.
fn cut_beside(c: &Candidate) -> bool {
    let glyphs = number_glyphs(&c.glyphs);
    let whole: Vec<&Glyph> = glyphs.iter().filter(|g| !g.cut).collect();
    let (Some(x0), Some(x1), Some(h)) =
        (whole.iter().map(|g| g.x).min(), whole.iter().map(|g| g.x + g.w).max(), whole.iter().map(|g| g.h).max())
    else {
        return false;
    };
    glyphs.iter().any(|g| g.cut && g.x + g.w + h / 2 >= x0 && g.x <= x1 + h / 2)
}

/// The candidate the learned digits read completely, and its number: among the tallest text,
/// the one with the most digits, then the closest match.
fn font_read(font: &Font, cands: &[Candidate]) -> Option<(usize, Shown)> {
    // With pixel-font grids any bar is a 1: bars around the number (slot borders) read as 1s.
    let bars = |r: &FontRead| font.has_grids() && r.glyphs == r.n.to_string().len() && r.n.to_string().bytes().all(|c| c == b'1');
    let reads: Vec<(usize, FontRead)> = cands
        .iter()
        .enumerate()
        .filter_map(|(i, c)| Some((i, font.read(&number_glyphs(&c.glyphs), SCALE)?)))
        .filter(|(_, r)| !bars(r))
        .collect();
    // Heights without glyphs the crop's edge cuts: a bar cut off at the edge (Creeper World's
    // energy bar) is taller than the number.
    let text_h = |c: &Candidate| c.glyphs.iter().filter(|g| !g.cut).map(|g| g.h).max().unwrap_or(0);
    let tallest = reads.iter().map(|(i, _)| text_h(&cands[*i])).max()?;
    // A candidate showing more glyphs of that size than a read has (one the digits couldn't
    // read): the read is part of the number ("1" of "1.8").
    let whole = |c: &Candidate| number_glyphs(&c.glyphs).iter().filter(|g| !g.cut).count();
    let most = cands.iter().filter(|c| text_h(c) * 4 >= tallest * 3 && text_h(c) * 3 <= tallest * 4).map(whole).max()?;
    reads
        .into_iter()
        .filter(|(i, _)| text_h(&cands[*i]) * 4 >= tallest * 3)
        .filter(|(_, r)| r.glyphs >= most)
        .min_by_key(|(_, r)| (std::cmp::Reverse(r.glyphs), r.worst))
        .and_then(|(i, r)| {
            let glyphs = number_glyphs(&cands[i].glyphs);
            Some((i, cands[i].shown(&glyphs, &r)?))
        })
}

/// Numbers the learned digits find on the whole frame (for examples/ocr.rs).
#[allow(dead_code)]
pub fn learned_numbers(frame: &Path, font: &Font) -> Result<Vec<Word>, String> {
    Ok(font_numbers(&image::open(frame).map_err(|e| e.to_string())?.to_rgb8(), font))
}

/// The colours inside a rectangle of a frame (512 bins, 3 bits a channel), to tell whether it
/// still shows what the player picked: a menu or inventory closed over the game leaves the box
/// on scenery, which the readers turn into numbers. A number changing or the text shifting a
/// little keeps most colours (Forager's inventory: 0.47-1.00 alike), scenery doesn't (0.00-0.02).
pub struct Look(Vec<f32>);

impl Look {
    pub fn of(frame: &Path, area: Rect) -> Result<Look, String> {
        let img = image::open(frame).map_err(|e| e.to_string())?.to_rgb8();
        let mut bins = vec![0f32; 512];
        let (x1, y1) = ((area.x + area.w).min(img.width()), (area.y + area.h).min(img.height()));
        for y in area.y.min(y1)..y1 {
            for x in area.x.min(x1)..x1 {
                let p = img.get_pixel(x, y);
                bins[((p[0] as usize >> 5) << 6) | ((p[1] as usize >> 5) << 3) | (p[2] as usize >> 5)] += 1.0;
            }
        }
        let n = bins.iter().sum::<f32>().max(1.0);
        bins.iter_mut().for_each(|b| *b /= n);
        Ok(Look(bins))
    }

    /// Whether `other` looks like the same thing: at least a quarter of the colours shared.
    pub fn like(&self, other: &Look) -> bool {
        self.0.iter().zip(&other.0).map(|(a, b)| a.min(*b)).sum::<f32>() >= 0.25
    }
}

/// Reads the number inside `area` of the frame, strictly inside it: what is around the
/// rectangle (an icon, a label, a slot border) is where misreads come from. PaddleOCR reads it;
/// the read is sure (true) when the learned digits read the same there. When they read another
/// number, or PaddleOCR none, theirs wins: they are the game's own font, confirmed by the
/// player. `debug` receives the crop.
pub fn read_number_at(frame: &Path, area: Rect, debug: &Path, font: Option<&Font>) -> Result<Option<(Shown, bool)>, String> {
    let img = image::open(frame).map_err(|e| e.to_string())?.to_rgb8();
    let x = area.x.min(img.width().saturating_sub(1));
    let y = area.y.min(img.height().saturating_sub(1));
    let crop = imageops::crop_imm(&img, x, y, area.w.min(img.width() - x).max(1), area.h.min(img.height() - y).max(1)).to_image();
    imageops::resize(&crop, crop.width() * SCALE, crop.height() * SCALE, imageops::FilterType::Nearest).save(debug).ok();
    let read = crate::reader::read_box(&img, area)?;
    let Some(font) = font.filter(|f| !f.is_empty()) else { return Ok(read.map(|n| (n, false))) };
    // Not where the rectangle's edge cuts a glyph beside the number.
    let cands: Vec<Candidate> = candidates(&img, area, false).into_iter().filter(|c| !cut_beside(c)).collect();
    let reads_as = |c: &Candidate, n: &Shown| {
        let glyphs = number_glyphs(&c.glyphs);
        font.read(&glyphs, SCALE).and_then(|r| c.shown(&glyphs, &r)).as_ref() == Some(n)
    };
    if let Some(n) = read.as_ref().filter(|n| cands.iter().any(|c| reads_as(c, n))) {
        return Ok(Some((n.clone(), true)));
    }
    // Digits the learned ones read that are only part of PaddleOCR's number, with ink over half
    // the number's height beside them, missed the rest (Quake II RTX's "17" read as "1": its 7
    // breaks into pieces of other colours, none a whole glyph). Small letters beside them are
    // not digits (Infested Planet's "09BP", where PaddleOCR reads the B as 8).
    let part_of = |f: &Shown, p: &Shown| f.digits() != p.digits() && p.digits().contains(&f.digits());
    let ink_beside = |i: usize| {
        let read = number_glyphs(&cands[i].glyphs);
        let (Some(x0), Some(x1), Some(h)) =
            (read.iter().map(|g| g.x).min(), read.iter().map(|g| g.x + g.w).max(), read.iter().map(|g| g.h).max())
        else {
            return false;
        };
        cands.iter().flat_map(|c| &c.glyphs).any(|g| !g.cut && (g.x >= x1 || g.x + g.w <= x0) && g.h * 2 > h)
    };
    match (font_read(font, &cands), read) {
        (Some((i, f)), Some(p)) if part_of(&f, &p) && ink_beside(i) => Ok(Some((p, false))),
        (Some((_, f)), _) => Ok(Some((f, true))),
        (None, p) => Ok(p.map(|n| (n, false))),
    }
}

/// Learns the game's digits from `area` showing `n`. The glyphs must split into the number's
/// digits, or a couple more on the left (numbers shown with leading zeros, like "09"), or more
/// that all look alike and like the number's own zeros (a score padded to a width: Jazz
/// Jackrabbit 2's "0000100"), all about the same height. `trusted`: `n` comes from memory, not
/// from the player. Returns what it did.
pub fn learn(frame: &Path, area: Rect, n: &Shown, font: &mut Font, trusted: bool) -> Result<String, String> {
    let img = image::open(frame).map_err(|e| e.to_string())?.to_rgb8();
    let text = n.digits();
    let (glyphs, extra) = learnable(&img, area, n, font, trusted)?;
    // Leading zeros are a guess. Glyphs that read as other digits aren't zeros; ones the font
    // can't read are taken as zeros only while no 0 is known (once it is, something that
    // doesn't read as 0 is an icon or a symbol: Creeper World got a junk "0" from the shape left
    // of a typed 40), and only when they ring a hole, as a 0 does (OpenTTD's "£" before a typed
    // 97518 was learned as a 0). Found in memory as well as typed: Fallout 2's HP counter
    // always shows 3 digits ("044"), so its first find could never learn them.
    let zeros = match font.read(&glyphs[..extra], SCALE) {
        Some(r) => r.n == 0 && r.glyphs == extra,
        None => font.shapes(0).is_empty() && glyphs[..extra].iter().all(|g| g.has_hole(SCALE)),
    };
    // Ones that can't be a 0 are a sign before the number ("£", the "x" of "x53"): left out.
    let sign = extra > 0 && !zeros && glyphs[..extra].iter().all(|g| !g.has_hole(SCALE));
    if extra > 0 && !zeros && !sign {
        return Err(format!("{extra} glyph(s) left of {n} that may not be zeros; not learning from it"));
    }
    let (glyphs, extra) = if sign { (glyphs[extra..].to_vec(), 0) } else { (glyphs, extra) };
    let label = format!("{}{text}", "0".repeat(extra));
    let mut added = font.learn(&glyphs, SCALE, &label, trusted);
    if let Some(seen) = finder_glyphs(&img, font, area, &glyphs) {
        added += font.learn_shapes(&seen, 1, &label, trusted);
    }
    let shown = if extra > 0 { label } else { n.to_string() };
    Ok(format!("learned the digits of {shown}: {added} new shapes (knows {})", font.known()))
}

/// The glyphs in `area` that show `n` (the number's digits, leading zeros or a sign first) and
/// how many come before its digits.
fn learnable(img: &RgbImage, area: Rect, n: &Shown, font: &Font, trusted: bool) -> Result<(Vec<Glyph>, usize), String> {
    if n.value() < 0.0 {
        return Err("only learns from numbers of 0 or more".into());
    }
    let text = n.digits();
    // Glyphs the crop's edge cuts off are something else. "13/40" is two numbers: the part
    // the learned digits don't contradict, the current value (before the slash) first.
    let parts = |c: &Candidate| -> Vec<Vec<Glyph>> {
        number_parts(&c.glyphs)
            .into_iter()
            .map(|p| p.into_iter().filter(|g| !g.cut).collect::<Vec<_>>())
            .filter(|p| trusted || font.agrees(p, SCALE, &text))
            .collect()
    };
    let fits_part = |glyphs: &[Glyph]| {
        let (lo, hi) = (glyphs.iter().map(|g| g.h).min()?, glyphs.iter().map(|g| g.h).max()?);
        let extra = glyphs.len().checked_sub(text.len())?;
        let digits = std::iter::repeat_n(b'0', extra).chain(text.bytes());
        let shaped = glyphs.iter().zip(digits).all(|(g, c)| crate::font::could_be(c - b'0', g.w, g.h));
        let padded = || {
            let own = glyphs[extra..].iter().zip(text.bytes()).filter(|(_, c)| *c == b'0').map(|(g, _)| g);
            let zeros: Vec<&Glyph> = glyphs[..extra].iter().chain(own).collect();
            zeros.iter().all(|g| crate::font::alike(g, zeros[0], SCALE))
        };
        (lo * 4 >= hi * 3 && (extra <= 2 || padded()) && shaped).then_some(extra)
    };
    let fit = |c: &Candidate| parts(c).into_iter().find_map(|p| Some((fits_part(&p)?, p)));
    let fits = |c: &Candidate| fit(c).map(|(extra, _)| extra);
    // Labels near the number ("AMMO") can be as big as it, or bigger: leave out glyphs PaddleOCR
    // reads as a word before picking the biggest text.
    let is_word = |t: &str| t.chars().filter(|c| c.is_alphabetic()).count() >= 2 && !t.chars().any(|c| c.is_ascii_digit());
    // PaddleOCR reads round digits as letters: Jazz Jackrabbit 2's score "0000100" as "ooooloo".
    // Text that spells the glyphs' digits in look-alikes is the number, not a label.
    let spells = |t: &str, c: &Candidate| {
        let as_digits: Option<String> = t.chars().filter(|ch| !ch.is_whitespace()).map(look_alike).collect();
        fits(c).is_some_and(|extra| as_digits == Some(format!("{}{text}", "0".repeat(extra))))
    };
    // The columns the glyphs that would be learned take up (glyph x is in crop pixels).
    let columns = |c: &Candidate| {
        let (_, p) = fit(c)?;
        let x0 = p.iter().map(|g| g.x).min()? / SCALE;
        let x1 = p.iter().map(|g| g.x + g.w).max()?.div_ceil(SCALE);
        Some(Rect { x: area.x + x0, y: c.rect.y, w: x1 - x0, h: c.rect.h })
    };
    // Not where the rectangle's edge cuts a glyph beside the number (the reads skip those too).
    let cands: Vec<Candidate> = candidates(&img, area, false)
        .into_iter()
        .filter(|c| !cut_beside(c))
        .filter(|c| columns(c).is_none_or(|r| !crate::reader::text(&img, r).is_ok_and(|t| is_word(&t) && !spells(&t, c))))
        .collect();
    // Only the biggest text that could be the number: other colour groups can hold a piece of
    // it (the slash of a 0), or something taller that isn't (Forager's item slot border).
    // Of that, the solid glyphs: anti-aliased edges make a group of thin outlines.
    // Heights of the glyphs that would be learned: a cut-off shape at the edge (Creeper World's
    // energy bar) can be taller than the number.
    let height = |c: &Candidate| fit(c).and_then(|(_, p)| p.iter().map(|g| g.h).max());
    let tallest = cands.iter().filter_map(height).max().unwrap_or(0);
    let ink = |c: &Candidate| fit(c).map_or(0, |(_, p)| p.iter().map(|g| g.ink.iter().filter(|p| **p).count()).sum::<usize>());
    let chosen = cands
        .iter()
        .filter(|c| height(c).is_some_and(|h| h * 4 >= tallest * 3))
        .filter_map(|c| Some((c, fits(c)?)))
        .min_by_key(|(c, extra)| (*extra, std::cmp::Reverse(ink(c))));
    let (cand, extra) = chosen.ok_or(format!("could not split the number into the {} digits of {n}", text.len()))?;
    let glyphs = fit(cand).map(|(_, p)| p).unwrap_or_default();
    if !trusted {
        if let Some(r) = font.read(&glyphs, SCALE).filter(|r| format!("{:0w$}", r.n, w = r.glyphs) != text) {
            return Err(format!("the learned digits read {} there, not {n}; not learning from it", r.n));
        }
    }
    Ok((glyphs, extra))
}

/// Whether the glyphs that show `n` on frame `b` looked different on frame `a`, taken a moment
/// before: a display that lags memory (Fallout 2's rolling HP counter, SuperTux's counting
/// coins) shows something in between until it settles, and those shapes aren't the number's
/// digits. False when `b` has no such glyphs (learning says why).
pub fn changed(a: &Path, b: &Path, area: Rect, n: &Shown, font: &Font) -> bool {
    let glyphs = |p: &Path| {
        let img = image::open(p).ok()?.to_rgb8();
        learnable(&img, area, n, font, true).ok().map(|(g, _)| g)
    };
    let Some(b) = glyphs(b) else { return false };
    glyphs(a).is_none_or(|a| a.len() != b.len() || !a.iter().zip(&b).all(|(a, b)| a.x.abs_diff(b.x) <= SCALE && crate::font::alike(a, b, SCALE)))
}

/// The digit a character OCR may read in its place, if any.
fn look_alike(c: char) -> Option<char> {
    match c {
        '0'..='9' => Some(c),
        'o' | 'O' | 'D' | 'Q' => Some('0'),
        'l' | 'I' | 'i' | '|' => Some('1'),
        'z' | 'Z' => Some('2'),
        's' | 'S' => Some('5'),
        'B' => Some('8'),
        _ => None,
    }
}

/// The full-frame finder's glyphs (`glyph_blobs`) where the learned `glyphs` (4x crop
/// pixels of `area`) are, one per glyph, all of one colour. Learned too, so numbers are found
/// on the whole frame: for anti-aliased text the finder only sees the flat-colour core, thinner
/// than the crop's shapes (Creeper World's 3 and 4 didn't match).
fn finder_glyphs(img: &RgbImage, font: &Font, area: Rect, glyphs: &[Glyph]) -> Option<Vec<Glyph>> {
    // A margin, so glyphs at the area's edge don't touch the crop's.
    let x = area.x.min(img.width().saturating_sub(1)).saturating_sub(4);
    let y = area.y.min(img.height().saturating_sub(1)).saturating_sub(4);
    let (w, h) = ((area.w + 8).min(img.width() - x), (area.h + 8).min(img.height() - y));
    let crop = imageops::crop_imm(img, x, y, w, h).to_image();
    let blobs = glyph_blobs(&crop, font);
    let ax = area.x.min(img.width().saturating_sub(1)) - x;
    let spots: Vec<(u32, u32)> = glyphs.iter().map(|g| (ax + g.x / SCALE, ax + (g.x + g.w).div_ceil(SCALE))).collect();
    let tall = glyphs.iter().map(|g| g.h / SCALE).max().unwrap_or(0);
    let at = |colour: (u8, u8, u8), (lo, hi): (u32, u32)| {
        blobs
            .iter()
            .filter(|b| b.colour == colour && b.x0 + 1 >= lo && b.x1 <= hi + 1)
            // The whole digit, not a piece of it.
            .filter(|b| (b.y1 - b.y0 + 1) * 4 >= tall * 3)
            .filter_map(|b| b.glyph.as_ref())
            .filter(|g| !is_slash(g))
            .max_by_key(|g| g.ink.iter().filter(|i| **i).count())
    };
    let mut colours: Vec<(u8, u8, u8)> = blobs.iter().filter(|b| b.glyph.is_some()).map(|b| b.colour).collect();
    colours.sort();
    colours.dedup();
    colours
        .into_iter()
        .filter_map(|c| spots.iter().map(|s| at(c, *s).cloned()).collect::<Option<Vec<Glyph>>>())
        .max_by_key(|gs| gs.iter().map(|g| g.ink.iter().filter(|i| **i).count()).sum::<usize>())
}

#[cfg(test)]
mod shown_tests {
    use super::Shown;

    #[test]
    fn parses_what_games_show() {
        let p = |t: &str| Shown::parse(t).map(|s| (s.to_string(), s.digits(), s.value(), s.search()));
        assert_eq!(p("1,250"), Some(("1250".into(), "1250".into(), 1250.0, "1250".into())));
        assert_eq!(p("53'731"), Some(("53731".into(), "53731".into(), 53731.0, "53731".into())));
        assert_eq!(p("1.2"), Some(("1.2".into(), "12".into(), 1.2, "1.2".into())));
        assert_eq!(p("0.05").map(|s| s.1), Some("005".into()));
        assert_eq!(p("3:17"), Some(("3:17".into(), "317".into(), 197.0, "197".into())));
        assert_eq!(p("1:02:03").map(|s| s.2), Some(3723.0));
        assert_eq!(p("13/40").map(|s| s.0), Some("13".into()));
        assert_eq!(p("/40").map(|s| s.0), Some("40".into()));
        assert_eq!(p("-5").map(|s| s.2), Some(-5.0));
        assert_eq!(p("09").map(|s| s.0), Some("9".into()));
        assert_eq!(p("000").map(|s| s.0), Some("0".into()));
        assert_eq!(p("0.5").map(|s| s.0), Some("0.5".into()));
        assert_eq!(p("03:07").map(|s| s.0), Some("3:07".into()));
        for bad in ["", "1.2.3", "3:7", "1:", ".5", "12a", "3:17.5"] {
            assert_eq!(p(bad), None, "{bad}");
        }
        assert_eq!(Shown::parse("1.25").map(|s| (s.decimals(), s.scaled())), Some((2, 125)));
    }
}

#[cfg(test)]
mod font_read_tests {
    use super::{Candidate, Rect};
    use crate::font::{FontRead, Glyph};

    #[test]
    fn touching_digits_come_from_one_glyph() {
        let glyph = |x| Glyph { x, w: 8, h: 10, ink: vec![true; 80], cut: false };
        let c = Candidate { glyph_h: 10, rect: Rect { x: 0, y: 0, w: 40, h: 10 }, glyphs: vec![], marks: vec![(10, 8, 11, 9)], band: (0, 9) };
        // "22" drawn as one wide glyph, split in two.
        let r = FontRead { n: 22, worst: 0, glyphs: 2, from: vec![0, 0] };
        assert_eq!(c.shown(&[glyph(0)], &r).map(|s| s.to_string()), Some("22".into()));
        // A dot between two glyphs, the second holding two touching digits: "1.25".
        let r = FontRead { n: 125, worst: 0, glyphs: 3, from: vec![0, 1, 1] };
        assert_eq!(c.shown(&[glyph(0), glyph(14)], &r).map(|s| s.to_string()), Some("1.25".into()));
    }
}

#[cfg(test)]
mod look_tests {
    use super::{Look, Rect};
    use image::{Rgb, RgbImage};

    #[test]
    fn tells_the_picked_box_from_scenery() {
        let dir = std::env::temp_dir().join(format!("ferret-look-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A white "22" on a dark slot, then the same with another number, then grass.
        let frame = |f: &dyn Fn(u32, u32) -> Rgb<u8>, name: &str| {
            let path = dir.join(name);
            RgbImage::from_fn(64, 20, |x, y| f(x, y)).save(&path).unwrap();
            path
        };
        let slot = |n: u32| move |x: u32, y: u32| if (x / 4 + y / 4 + n) % 5 == 0 { Rgb([250, 250, 250]) } else { Rgb([30, 34, 40]) };
        let area = Rect { x: 0, y: 0, w: 64, h: 20 };
        let picked = Look::of(&frame(&slot(0), "a.png"), area).unwrap();
        assert!(Look::of(&frame(&slot(2), "b.png"), area).unwrap().like(&picked));
        let grass = |x: u32, y: u32| if x > y * 2 { Rgb([90, 160, 60]) } else { Rgb([150, 170, 70]) };
        assert!(!Look::of(&frame(&grass, "c.png"), area).unwrap().like(&picked));
        std::fs::remove_dir_all(&dir).ok();
    }
}
