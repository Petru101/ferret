// Reads numbers from captured frames with the bundled Tesseract.

use std::path::Path;
use std::process::Command;

use image::{imageops, GrayImage, Luma};

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

fn tesseract(img: &Path, psm: u32, digits_only: bool) -> Result<Vec<Word>, String> {
    let mut cmd = Command::new("tesseract");
    cmd.arg(img).arg("stdout").args(["--psm", &psm.to_string()]);
    if digits_only {
        cmd.args(["-c", "tessedit_char_whitelist=0123456789"]);
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
            Some(Word {
                text: f[11].trim().to_owned(),
                rect: Rect { x: n(6)?, y: n(7)?, w: n(8)?, h: n(9)? },
                conf: f[10].parse().unwrap_or(0.0),
            })
        })
        .collect())
}

pub fn digits(text: &str) -> Option<i64> {
    let d: String = text.chars().filter(char::is_ascii_digit).collect();
    d.parse().ok()
}

/// Every word in the frame that contains a digit.
pub fn numbers(frame: &Path) -> Result<Vec<Word>, String> {
    Ok(tesseract(frame, 11, false)?
        .into_iter()
        .filter(|w| w.text.chars().any(|c| c.is_ascii_digit()))
        .collect())
}

/// Grows a word's box so the number still fits when it gains digits.
pub fn watch_area(r: Rect) -> Rect {
    let pad = r.h.max(8);
    Rect { x: r.x.saturating_sub(2 * pad), y: r.y.saturating_sub(pad / 2), w: r.w + 4 * pad, h: r.h + pad }
}

fn otsu(img: &GrayImage) -> u8 {
    let mut hist = [0u64; 256];
    for p in img.pixels() {
        hist[p[0] as usize] += 1;
    }
    let total = img.pixels().len() as f64;
    let sum: f64 = hist.iter().enumerate().map(|(i, c)| i as f64 * *c as f64).sum();
    let (mut sum_b, mut w_b, mut best, mut threshold) = (0.0, 0.0, 0.0, 128u8);
    for (t, c) in hist.iter().enumerate() {
        w_b += *c as f64;
        if w_b == 0.0 || w_b == total {
            continue;
        }
        sum_b += t as f64 * *c as f64;
        let m_b = sum_b / w_b;
        let m_f = (sum - sum_b) / (total - w_b);
        let between = w_b * (total - w_b) * (m_b - m_f).powi(2);
        if between > best {
            best = between;
            threshold = t as u8;
        }
    }
    threshold
}

/// Reads the number inside `area` of the frame. `debug` receives the cleaned-up crop.
pub fn read_number(frame: &Path, area: Rect, debug: &Path) -> Result<Option<i64>, String> {
    let img = image::open(frame).map_err(|e| e.to_string())?.to_luma8();
    let x = area.x.min(img.width().saturating_sub(1));
    let y = area.y.min(img.height().saturating_sub(1));
    let w = area.w.min(img.width() - x);
    let h = area.h.min(img.height() - y);
    let crop = imageops::crop_imm(&img, x, y, w, h).to_image();
    let big = imageops::resize(&crop, w * 4, h * 4, imageops::FilterType::CatmullRom);
    let t = otsu(&big);
    let mut bw = GrayImage::from_fn(big.width(), big.height(), |x, y| {
        Luma([if big.get_pixel(x, y)[0] > t { 255 } else { 0 }])
    });
    // Tesseract wants dark text on a light background; text is the minority colour.
    let light = bw.pixels().filter(|p| p[0] == 255).count();
    if light * 2 < bw.pixels().len() {
        imageops::invert(&mut bw);
    }
    let bordered = {
        let mut canvas = GrayImage::from_pixel(bw.width() + 40, bw.height() + 40, Luma([255]));
        imageops::overlay(&mut canvas, &bw, 20, 20);
        canvas
    };
    bordered.save(debug).map_err(|e| e.to_string())?;
    let words = tesseract(debug, 7, true)?;
    let text: String = words.iter().map(|w| w.text.as_str()).collect();
    Ok(digits(&text))
}
