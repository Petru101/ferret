// Reads numbers from captured frames with the bundled Tesseract.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use image::{imageops, GrayImage, Luma, RgbImage};

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

/// Runs Tesseract on an image, or on each image named in a list file: words with their page
/// (1-based, the position in the list).
fn tesseract(img: &Path, psm: u32, config: &[&str]) -> Result<Vec<(usize, Word)>, String> {
    let mut cmd = Command::new("tesseract");
    cmd.arg(img).arg("stdout").args(["--psm", &psm.to_string()]);
    for c in config {
        cmd.args(["-c", c]);
    }
    let out = cmd.arg("tsv").output().map_err(|e| format!("tesseract: {e}"))?;
    if !out.status.success() {
        return Err(format!("tesseract failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 12 || f[0] != "5" || f[11].trim().is_empty() {
                return None;
            }
            let n = |i: usize| f[i].parse::<u32>().ok();
            let word = Word {
                text: f[11].trim().to_owned(),
                rect: Rect { x: n(6)?, y: n(7)?, w: n(8)?, h: n(9)? },
                conf: f[10].parse().unwrap_or(0.0),
            };
            Some((f[1].parse().ok()?, word))
        })
        .collect())
}

pub fn digits(text: &str) -> Option<i64> {
    let d: String = text.chars().filter(char::is_ascii_digit).collect();
    d.parse().ok()
}

fn has_digit(w: &Word) -> bool {
    w.text.chars().any(|c| c.is_ascii_digit())
}

pub fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// Brightest channel minus the local average, so dim text on a dark HUD stands out as much as
/// bright text does. Returned 2x upscaled, dark text on white.
fn local_contrast(img: &RgbImage) -> GrayImage {
    const R: i64 = 12;
    let (w, h) = (img.width() as i64, img.height() as i64);
    let v: Vec<u8> = img.pixels().map(|p| p[0].max(p[1]).max(p[2])).collect();
    // Summed-area table for the box average.
    let mut sat = vec![0u64; ((w + 1) * (h + 1)) as usize];
    for y in 0..h {
        let mut row = 0u64;
        for x in 0..w {
            row += v[(y * w + x) as usize] as u64;
            sat[((y + 1) * (w + 1) + x + 1) as usize] = sat[(y * (w + 1) + x + 1) as usize] + row;
        }
    }
    let out = GrayImage::from_fn(w as u32, h as u32, |x, y| {
        let (x, y) = (x as i64, y as i64);
        let (x0, y0, x1, y1) = ((x - R).max(0), (y - R).max(0), (x + R + 1).min(w), (y + R + 1).min(h));
        let sum = sat[(y1 * (w + 1) + x1) as usize] + sat[(y0 * (w + 1) + x0) as usize]
            - sat[(y0 * (w + 1) + x1) as usize]
            - sat[(y1 * (w + 1) + x0) as usize];
        let mean = sum as f32 / ((x1 - x0) * (y1 - y0)) as f32;
        let d = (v[(y * w + x) as usize] as f32 - mean).abs() * 4.0;
        Luma([255 - d.min(255.0) as u8])
    });
    double(&out)
}

/// 2x bilinear upscale (imageops::resize takes over a second on a whole frame).
fn double(img: &GrayImage) -> GrayImage {
    let (w, h) = (img.width(), img.height());
    let at = |x: u32, y: u32| img.get_pixel(x.min(w - 1), y.min(h - 1))[0] as u16;
    GrayImage::from_fn(w * 2, h * 2, |x, y| {
        let (sx, sy) = (x / 2, y / 2);
        let (a, b) = (at(sx, sy), at(sx + x % 2, sy));
        let (c, d) = (at(sx, sy + y % 2), at(sx + x % 2, sy + y % 2));
        Luma([((a + b + c + d + 2) / 4) as u8])
    })
}

/// Every number in the frame. The plain pass handles ordinary text; an automatic-layout pass
/// finds text on light panels, an adaptive-threshold pass and a local-contrast pass catch dim
/// text. Those also read plenty of fake digits into game textures, so what only they see is
/// kept only if a careful re-read agrees.
pub fn numbers(frame: &Path, font: Option<&Font>) -> Result<Vec<Word>, String> {
    let img = image::open(frame).map_err(|e| e.to_string())?.to_rgb8();
    // The game's own digits find its numbers exactly; Tesseract adds what they don't read.
    let by_font = font.filter(|f| !f.is_empty()).map(|f| font_numbers(&img, f)).unwrap_or_default();
    // PGM: encoding a PNG this size takes longer than reading it.
    let contrast = frame.with_file_name("frame-contrast.pgm");
    let c = local_contrast(&img);
    let mut pgm = format!("P5\n{} {}\n255\n", c.width(), c.height()).into_bytes();
    pgm.extend_from_slice(c.as_raw());
    std::fs::write(&contrast, pgm).map_err(|e| e.to_string())?;
    let (plain, layout, adaptive, contrast) = thread::scope(|s| {
        let plain = s.spawn(|| tesseract(frame, 11, &[]));
        let layout = s.spawn(|| tesseract(frame, 3, &[]));
        let adaptive = s.spawn(|| tesseract(frame, 11, &["thresholding_method=1"]));
        let contrast = tesseract(&contrast, 11, &[]);
        (plain.join().unwrap(), layout.join().unwrap(), adaptive.join().unwrap(), contrast)
    });
    std::fs::remove_file(frame.with_file_name("frame-contrast.pgm")).ok();
    let halve = |(_, mut w): (usize, Word)| {
        w.rect = Rect { x: w.rect.x / 2, y: w.rect.y / 2, w: w.rect.w.div_ceil(2), h: w.rect.h.div_ceil(2) };
        w
    };
    let mut found: Vec<Word> = plain?.into_iter().map(|(_, w)| w).filter(has_digit).collect();
    let mut extra: Vec<Word> = Vec::new();
    let others = layout?.into_iter().chain(adaptive?).map(|(_, w)| w).chain(contrast?.into_iter().map(halve));
    for w in others.filter(has_digit) {
        if !found.iter().chain(&extra).any(|f| overlaps(f.rect, w.rect)) {
            extra.push(w);
        }
    }
    // Re-read everything with `read_areas`: it gets dim text and unit labels right, and a box it
    // can't read is no use for watching anyway. Words only the extra passes saw also need a
    // confident read.
    let areas: Vec<Rect> = found
        .iter()
        .chain(&extra)
        .map(|w| {
            // Generous: the other passes often box only part of the glyphs.
            let pad = w.rect.h.max(8);
            Rect { x: w.rect.x.saturating_sub(2 * pad), y: w.rect.y.saturating_sub(pad), w: w.rect.w + 4 * pad, h: w.rect.h + 2 * pad }
        })
        .collect();
    let dir = frame.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let reads = read_areas(&img, &areas, true, dir, font);
    remove_candidates(dir);
    let mut reads = reads?.into_iter();
    let plain: Vec<Word> = found.drain(..).collect();
    for (w, r) in plain.into_iter().zip(reads.by_ref()) {
        if let Some(r) = r.filter(|r| overlaps(r.rect, w.rect)) {
            found.push(Word { text: r.n.to_string(), rect: r.rect, conf: r.conf });
        }
    }
    for r in reads.flatten() {
        // Under 7 pixels it's a texture more often than a number.
        if r.conf >= 70.0 && r.rect.h >= 7 && !found.iter().any(|f| overlaps(f.rect, r.rect)) {
            found.push(Word { text: r.n.to_string(), rect: r.rect, conf: r.conf });
        }
    }
    found.retain(|w| !by_font.iter().any(|f| overlaps(f.rect, w.rect)));
    found.extend(by_font);
    Ok(found)
}

/// Numbers on the whole frame drawn in the learned digits: shapes of one flat colour that are
/// learned digits, side by side at the same height. A row that also has other shapes of that
/// colour and size is a word ("GOLD" has an O), not a number.
fn font_numbers(img: &RgbImage, font: &Font) -> Vec<Word> {
    let (w, h) = img.dimensions();
    let colour = |i: usize| {
        let p = img.as_raw();
        (p[i * 3] >> 5, p[i * 3 + 1] >> 5, p[i * 3 + 2] >> 5)
    };
    // Same-colour shapes (4-connected), as glyph candidates.
    struct Blob {
        colour: (u8, u8, u8),
        x0: u32,
        y0: u32,
        x1: u32,
        y1: u32,
        digit: Option<u8>,
    }
    let mut label = vec![0u32; (w * h) as usize];
    let mut blobs: Vec<Blob> = Vec::new();
    let mut stack = Vec::new();
    let mut pixels = Vec::new();
    for start in 0..label.len() {
        if label[start] != 0 {
            continue;
        }
        let c = colour(start);
        let id = blobs.len() as u32 + 1;
        label[start] = id;
        stack.push(start);
        pixels.clear();
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
        while let Some(i) = stack.pop() {
            pixels.push(i);
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
        }
        let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);
        // Glyph-sized and solid enough; not touching the frame's edge.
        let sized = (6..=80).contains(&bh) && bw * 2 <= bh * 3 && pixels.len() as u32 * 10 >= bw * bh * 3;
        let inside = x0 > 0 && y0 > 0 && x1 + 1 < w && y1 + 1 < h;
        let digit = (sized && inside).then(|| {
            let mut ink = vec![false; (bw * bh) as usize];
            for &i in &pixels {
                let (x, y) = ((i as u32) % w, (i as u32) / w);
                ink[((y - y0) * bw + x - x0) as usize] = true;
            }
            font.digit_of(&Glyph { x: x0, w: bw, h: bh, ink, cut: false }, 1)
        });
        blobs.push(Blob { colour: c, x0, y0, x1, y1, digit: digit.flatten() });
        if !(sized && inside) {
            blobs.last_mut().unwrap().x1 = u32::MAX; // not a glyph: never part of a row
        }
    }
    // Rows: glyphs of one colour, about the same top and height, close together.
    let mut glyphs: Vec<&Blob> = blobs.iter().filter(|b| b.x1 != u32::MAX).collect();
    glyphs.sort_by_key(|b| (b.colour, b.y0, b.x0));
    let next_to = |a: &Blob, b: &Blob| {
        let (ha, hb) = (a.y1 - a.y0 + 1, b.y1 - b.y0 + 1);
        a.colour == b.colour
            && ha.abs_diff(hb) <= ha / 4 + 1
            && a.y0.abs_diff(b.y0) <= ha / 4 + 1
            && b.x0 > a.x1
            && b.x0 - a.x1 <= ha * 4 / 5
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
        let digits: Option<String> = row.iter().map(|&j| glyphs[j].digit.map(|d| char::from(b'0' + d))).collect();
        let Some(text) = digits else { continue };
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

/// A black-on-white image of what might be the number, the height of its tallest glyph, where
/// its glyphs are (in the image, then in the frame) and the glyphs themselves, left to right.
struct Candidate {
    img: GrayImage,
    glyph_h: u32,
    rect: Rect,
    glyphs: Vec<Glyph>,
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
    let keep: Vec<bool> = blobs
        .iter()
        .map(|b| {
            let (left, right) = (b.0 == 0, b.2 == w - 1);
            !(left && right) && !(cut_off && (left || right))
        })
        .collect();
    let tallest = blobs.iter().zip(&keep).filter(|(_, k)| **k).map(|(b, _)| b.3 - b.1 + 1).max()?;
    let keep: Vec<bool> = blobs.iter().zip(keep).map(|(b, k)| k && (b.3 - b.1 + 1) * 2 >= tallest).collect();
    for (i, b) in blobs.iter().enumerate().filter(|(i, _)| keep[*i]) {
        unslash(&mut label, w, i as u32 + 1, *b);
    }
    let kept: Vec<_> = blobs.iter().zip(&keep).filter(|(_, k)| **k).map(|(b, _)| *b).collect();
    let (x0, y0) = (kept.iter().map(|b| b.0).min()?, kept.iter().map(|b| b.1).min()?);
    let (x1, y1) = (kept.iter().map(|b| b.2).max()?, kept.iter().map(|b| b.3).max()?);
    let rect = Rect { x: x0, y: y0, w: x1 - x0 + 1, h: y1 - y0 + 1 };
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
    // Tesseract wants a margin around the text.
    let mut img = GrayImage::from_pixel(w + 40, h + 40, Luma([255]));
    for (i, l) in label.iter().enumerate() {
        if *l != 0 && keep[*l as usize - 1] {
            img.put_pixel(20 + i as u32 % w, 20 + i as u32 / w, Luma([0]));
        }
    }
    Some(Candidate { img, glyph_h: tallest, rect, glyphs })
}

/// Turns a slashed zero (Ø, common in HUD fonts, and Tesseract can't read it) into a plain 0.
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

struct Read {
    n: i64,
    conf: f32,
    file: PathBuf,
    rect: Rect,
    /// Read by the learned digits rather than Tesseract.
    learned: bool,
}

/// Reads the number in each area; `padded` areas have room around the number, so anything
/// touching their edge is something else. Every candidate image goes to `dir`; the chosen one
/// is named in the result, the caller removes the rest.
fn read_areas(img: &RgbImage, areas: &[Rect], padded: bool, dir: &Path, font: Option<&Font>) -> Result<Vec<Option<Read>>, String> {
    let per_area: Vec<Vec<Candidate>> = thread::scope(|s| {
        let jobs: Vec<_> = areas.iter().map(|a| s.spawn(move || candidates(img, *a, padded))).collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });
    let mut known: Vec<Option<Read>> = Vec::new();
    let mut files: Vec<(usize, usize, u32, Rect, PathBuf)> = Vec::new(); // area, candidate, glyph height, glyphs, image
    for (a, cands) in per_area.iter().enumerate() {
        let mut paths = Vec::new();
        for (c, cand) in cands.iter().enumerate() {
            let file = dir.join(format!("ocr-{a}-{c}.png"));
            cand.img.save(&file).map_err(|e| e.to_string())?;
            paths.push(file);
        }
        // With the game's digits learned, a crop they read completely needs no Tesseract.
        let by_font = font.and_then(|f| font_read(f, cands));
        known.push(by_font.map(|(c, n)| Read { n, conf: 95.0, file: paths[c].clone(), rect: cands[c].rect, learned: true }));
        if known[a].is_none() {
            files.extend(cands.iter().zip(paths).enumerate().map(|(c, (cand, file))| (a, c, cand.glyph_h, cand.rect, file)));
        }
    }
    // A few Tesseract runs over lists of images: starting it costs more than reading a crop.
    let runs = thread::available_parallelism().map_or(4, |n| n.get()).min(files.len().div_ceil(4)).max(1);
    let chunk = files.len().div_ceil(runs).max(1);
    let texts: Vec<(Vec<(usize, Word)>, Vec<(usize, Word)>)> = thread::scope(|s| {
        let jobs: Vec<_> = files
            .chunks(chunk)
            .enumerate()
            .map(|(r, part)| {
                s.spawn(move || {
                    let list = dir.join(format!("ocr-list-{r}.txt"));
                    let names: String = part.iter().map(|f| format!("{}\n", f.4.display())).collect();
                    std::fs::write(&list, names).map_err(|e| e.to_string())?;
                    // Digits only reads stylised digits best, but also turns letters into digits;
                    // the free read tells which words are letters.
                    let (digits, free) = thread::scope(|s| {
                        let free = s.spawn(|| tesseract(&list, 7, &[]));
                        (tesseract(&list, 7, &["tessedit_char_whitelist=0123456789"]), free.join().unwrap())
                    });
                    std::fs::remove_file(&list).ok();
                    Ok((digits?, free?))
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect::<Result<_, String>>()
    })?;
    let mut reads: Vec<(Vec<Word>, Vec<Word>)> = vec![(Vec::new(), Vec::new()); files.len()];
    for (r, (digits, free)) in texts.into_iter().enumerate() {
        for (page, w) in digits {
            reads[r * chunk + page - 1].0.push(w);
        }
        for (page, w) in free {
            reads[r * chunk + page - 1].1.push(w);
        }
    }
    let mut found: Vec<Vec<(u32, Read)>> = (0..areas.len()).map(|_| Vec::new()).collect();
    for ((a, c, glyph_h, rect, file), (digit_words, free)) in files.into_iter().zip(reads) {
        let Some((n, conf)) = number_in(&digit_words, &free) else { continue };
        // Tesseract drops digits it can't read (a blocky "2" in "26" gives 6). When the crop has
        // more glyphs the size of the learned digits than the read has digits, and no letters,
        // it is missing some: better no number than a wrong one.
        let sized = font.map_or(0, |f| f.digit_sized(&per_area[a][c].glyphs, SCALE));
        let letters = free.iter().any(|w| w.text.chars().any(char::is_alphabetic));
        if sized > n.unsigned_abs().to_string().len() && !letters {
            continue;
        }
        // A digit the learned ones read differently: Tesseract misread it.
        if font.is_some_and(|f| !f.agrees(&per_area[a][c].glyphs, SCALE, n)) {
            continue;
        }
        found[a].push((glyph_h, Read { n, conf, file, rect, learned: false }));
    }
    // The number is normally the biggest text in the box; among reads of about that size (edge
    // pixels change the height a little), the most confident one.
    Ok(found
        .into_iter()
        .zip(known)
        .map(|(reads, known)| {
            if known.is_some() {
                return known;
            }
            let tallest = reads.iter().map(|r| r.0).max()?;
            reads.into_iter().filter(|r| r.0 * 4 >= tallest * 3).map(|r| r.1).max_by(|a, b| a.conf.total_cmp(&b.conf))
        })
        .collect())
}

/// The candidate the learned digits read completely, and its number: among the tallest text,
/// the one with the most digits, then the closest match.
fn font_read(font: &Font, cands: &[Candidate]) -> Option<(usize, i64)> {
    // With pixel-font grids any bar is a 1: bars around the number (slot borders) read as 1s.
    let bars = |r: &FontRead| font.has_grids() && r.glyphs == r.n.to_string().len() && r.n.to_string().bytes().all(|c| c == b'1');
    let reads: Vec<(usize, FontRead)> = cands
        .iter()
        .enumerate()
        .filter_map(|(i, c)| Some((i, font.read(&c.glyphs, SCALE)?)))
        .filter(|(_, r)| !bars(r))
        .collect();
    let tallest = reads.iter().map(|(i, _)| cands[*i].glyph_h).max()?;
    reads
        .into_iter()
        .filter(|(i, _)| cands[*i].glyph_h * 4 >= tallest * 3)
        .min_by_key(|(_, r)| (std::cmp::Reverse(r.glyphs), r.worst))
        .map(|(i, r)| (i, r.n))
}

/// The number in a crop and its confidence. When the free read is clear (confident numbers and
/// words of letters like "HP" or "BP"), its numbers; the digits-only read turns letters into
/// digits. Otherwise the digits-only read, leaving out the words the free read saw as letters.
/// If that leaves nothing, a confident free read means it really is just letters; if not, the
/// free read may have misread odd digits.
fn number_in(digit_words: &[Word], free: &[Word]) -> Option<(i64, f32)> {
    let is_letters = |f: &Word| f.text.chars().any(char::is_alphabetic) && !has_digit(f);
    let is_number = |f: &Word| f.conf >= 80.0 && f.text.chars().all(|c| c.is_ascii_digit() || ",.".contains(c));
    if free.iter().any(is_number) && free.iter().all(|f| is_letters(f) || is_number(f)) {
        let numbers: Vec<&Word> = free.iter().filter(|f| is_number(f)).collect();
        let text: String = numbers.iter().map(|w| w.text.as_str()).collect();
        return Some((digits(&text)?, numbers.iter().map(|w| w.conf).fold(f32::MAX, f32::min)));
    }
    let letters = |w: &&Word| {
        let seen = || free.iter().filter(|f| overlaps(f.rect, w.rect));
        seen().any(is_letters) && !seen().any(has_digit)
    };
    let mut kept: Vec<&Word> = digit_words.iter().filter(|w| !letters(w)).collect();
    if kept.is_empty() {
        if free.iter().all(|f| f.conf >= 80.0) {
            return None;
        }
        kept = digit_words.iter().collect();
    }
    let text: String = kept.iter().map(|w| w.text.as_str()).collect();
    // The digits-only read sometimes reports 0 for a word it got right; both reads agreeing is
    // as good as confidence.
    let conf = |w: &&Word| {
        let same = free.iter().filter(|f| f.text == w.text && overlaps(f.rect, w.rect));
        same.map(|f| f.conf).fold(w.conf, f32::max)
    };
    Some((digits(&text)?, kept.iter().map(conf).fold(f32::MAX, f32::min)))
}

fn remove_candidates(dir: &Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with("ocr-") {
            std::fs::remove_file(e.path()).ok();
        }
    }
}

/// Numbers the learned digits find on the whole frame (for examples/ocr.rs).
#[allow(dead_code)]
pub fn learned_numbers(frame: &Path, font: &Font) -> Result<Vec<Word>, String> {
    Ok(font_numbers(&image::open(frame).map_err(|e| e.to_string())?.to_rgb8(), font))
}

/// How far the numbers around `area` moved since the previous frame (`before`), when they moved
/// together: Forager's item info panel shifts the whole inventory sideways. Each of the numbers
/// nearest the area votes for where the same digits at the same height are now; the shift needs
/// two votes and more than "stayed put" gets (a menu over the number moves nothing).
fn layout_shift(before: &[Word], now: &[Word], area: Rect) -> Option<(i32, i32)> {
    let centre = |r: Rect| (2 * r.x as i64 + r.w as i64, 2 * r.y as i64 + r.h as i64);
    let (ax, ay) = centre(area);
    let mut near: Vec<&Word> = before.iter().collect();
    near.sort_by_key(|w| {
        let (x, y) = centre(w.rect);
        (x - ax).pow(2) + (y - ay).pow(2)
    });
    near.truncate(8);
    let mut votes: Vec<((i32, i32), usize)> = Vec::new();
    for b in near {
        for n in now.iter().filter(|n| n.text == b.text && n.rect.h.abs_diff(b.rect.h) <= 1) {
            let s = (n.rect.x as i32 - b.rect.x as i32, n.rect.y as i32 - b.rect.y as i32);
            match votes.iter_mut().find(|(v, _)| v.0.abs_diff(s.0) <= 2 && v.1.abs_diff(s.1) <= 2) {
                Some((_, count)) => *count += 1,
                None => votes.push((s, 1)),
            }
        }
    }
    let still = |s: (i32, i32)| s.0.abs() <= 2 && s.1.abs() <= 2;
    let stayed = votes.iter().filter(|(s, _)| still(*s)).map(|(_, n)| *n).sum::<usize>();
    let (shift, n) = votes.into_iter().filter(|(s, _)| !still(*s)).max_by_key(|(_, n)| *n)?;
    (n >= 2 && n > stayed).then_some(shift)
}

fn moved(r: Rect, (dx, dy): (i32, i32)) -> Rect {
    Rect { x: (r.x as i32 + dx).max(0) as u32, y: (r.y as i32 + dy).max(0) as u32, ..r }
}

/// Reads the number inside `area` of the frame, and whether the learned digits read it (rather
/// than Tesseract); `debug` receives the cleaned-up crop that was read. Also returns where to
/// watch from now on, when that changed. With learned digits the area is only a hint: of the
/// numbers they find on the whole frame (where other numbers vouch for a lone 1), the one
/// overlapping it most, whole even when the area cuts it; the area moves onto it. `before` has
/// the numbers they found in the previous frame: when the numbers around the area all moved
/// (see `layout_shift`), the area moves with them first. It gets this frame's numbers.
pub fn read_number_at(
    frame: &Path,
    mut area: Rect,
    debug: &Path,
    font: Option<&Font>,
    before: &mut Vec<Word>,
) -> Result<(Option<(i64, bool)>, Option<Rect>), String> {
    let img = image::open(frame).map_err(|e| e.to_string())?.to_rgb8();
    let mut moved_to = None;
    if let Some(f) = font.filter(|f| !f.is_empty()) {
        let now = font_numbers(&img, f);
        if let Some(shift) = layout_shift(before, &now, area) {
            area = moved(area, shift);
            moved_to = Some(area);
        }
        *before = now.clone();
        let inside = |r: Rect| {
            let w = (r.x + r.w).min(area.x + area.w).saturating_sub(r.x.max(area.x));
            let h = (r.y + r.h).min(area.y + area.h).saturating_sub(r.y.max(area.y));
            w * h
        };
        // Most of the number inside the area, then the most of it.
        let best = now
            .into_iter()
            .filter(|w| inside(w.rect) > 0)
            .max_by_key(|w| (inside(w.rect) * 100 / (w.rect.w * w.rect.h).max(1), inside(w.rect)));
        if let Some((w, n)) = best.and_then(|w| w.text.parse::<i64>().ok().map(|n| (w, n))) {
            let r = w.rect;
            let (x, y) = (r.x.saturating_sub(2), r.y.saturating_sub(2));
            let crop = imageops::crop_imm(&img, x, y, (r.w + 4).min(img.width() - x), (r.h + 4).min(img.height() - y)).to_image();
            imageops::resize(&crop, crop.width() * 4, crop.height() * 4, imageops::FilterType::Nearest).save(debug).ok();
            return Ok((Some((n, true)), Some(watch_area(r))));
        }
    }
    let dir = debug.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let read = read_areas(&img, &[area], false, dir, font)?.pop().flatten();
    // With nothing read, show the first candidate instead.
    let shown = read.as_ref().map_or_else(|| dir.join("ocr-0-0.png"), |r| r.file.clone());
    if std::fs::rename(&shown, debug).is_err() {
        std::fs::remove_file(debug).ok();
    }
    remove_candidates(dir);
    Ok((read.map(|r| (r.n, r.learned)), moved_to))
}

/// Learns the game's digits from `area` showing `n`. The glyphs must split into the number's
/// digits, or a couple more on the left (numbers shown with leading zeros, like "09"), all about
/// the same height. `trusted`: `n` comes from memory, not from the player. Returns what it did.
pub fn learn(frame: &Path, area: Rect, n: i64, font: &mut Font, trusted: bool) -> Result<String, String> {
    if n < 0 {
        return Err("only learns from numbers of 0 or more".into());
    }
    let img = image::open(frame).map_err(|e| e.to_string())?.to_rgb8();
    let text = n.to_string();
    // Glyphs the crop's edge cuts off are something else.
    let whole = |c: &Candidate| -> Vec<Glyph> { c.glyphs.iter().filter(|g| !g.cut).cloned().collect() };
    let fits = |c: &Candidate| {
        let glyphs = whole(c);
        let (lo, hi) = (glyphs.iter().map(|g| g.h).min()?, glyphs.iter().map(|g| g.h).max()?);
        let extra = glyphs.len().checked_sub(text.len())?;
        (lo * 4 >= hi * 3 && extra <= 2).then_some(extra)
    };
    let cands = candidates(&img, area, false);
    // Only the biggest text that could be the number: other colour groups can hold a piece of
    // it (the slash of a 0), or something taller that isn't (Forager's item slot border).
    // Of that, the solid glyphs: anti-aliased edges make a group of thin outlines.
    let tallest = cands.iter().filter(|c| fits(c).is_some()).map(|c| c.glyph_h).max().unwrap_or(0);
    let ink = |c: &Candidate| c.glyphs.iter().map(|g| g.ink.iter().filter(|p| **p).count()).sum::<usize>();
    let mut fitting: Vec<(&Candidate, usize)> =
        cands.iter().filter(|c| c.glyph_h * 4 >= tallest * 3).filter_map(|c| Some((c, fits(c)?))).collect();
    fitting.sort_by_key(|(c, extra)| (*extra, std::cmp::Reverse(ink(c))));
    // Labels near the number ("AMMO") can be as big as it: skip glyphs Tesseract reads as words.
    let dir = frame.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let file = dir.join("ocr-learn.png");
    let mut chosen = None;
    for (c, extra) in fitting {
        c.img.save(&file).map_err(|e| e.to_string())?;
        let words = tesseract(&file, 7, &[]);
        let is_word = |w: &Word| w.conf >= 60.0 && w.text.chars().filter(|c| c.is_alphabetic()).count() >= 2 && !has_digit(w);
        if words.is_ok_and(|w| !w.iter().any(|(_, w)| is_word(w))) {
            chosen = Some((c, extra));
            break;
        }
    }
    std::fs::remove_file(&file).ok();
    let (cand, extra) = chosen.ok_or(format!("could not split the number into the {} digits of {n}", text.len()))?;
    let glyphs = whole(cand);
    if !trusted {
        if let Some(r) = font.read(&glyphs, SCALE).filter(|r| r.n != n) {
            return Err(format!("the learned digits read {} there, not {n}; not learning from it", r.n));
        }
    }
    // Leading zeros are a guess. Glyphs that read as other digits aren't zeros; ones the font
    // can't read are taken as zeros only when the player typed the number and no 0 is known
    // yet (once it is, something that doesn't read as 0 is an icon or a symbol: Creeper World
    // got a junk "0" from the shape left of a typed 40).
    let zeros = match font.read(&glyphs[..extra], SCALE) {
        Some(r) => r.n == 0 && r.glyphs == extra,
        None => !trusted && font.shapes(0).is_empty(),
    };
    if extra > 0 && !zeros {
        return Err(format!("{extra} glyph(s) left of {n} that may not be zeros; not learning from it"));
    }
    let label = format!("{}{text}", "0".repeat(extra));
    let added = font.learn(&glyphs, SCALE, &label, trusted);
    Ok(format!("learned the digits of {label}: {added} new shapes (knows {})", font.known()))
}
