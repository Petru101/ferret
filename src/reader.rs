// Reading text with PaddleOCR (PP-OCRv6 small, ONNX, run by rten): DB text detection, then CTC
// recognition of each detected line. Pre- and post-processing follow PaddleOCR's own pipeline:
// BGR input, ImageNet mean/std for detection, (x - 0.5) / 0.5 for recognition, 48 px tall lines,
// the settings from the models' inference.yml.
//
// A picked number is read strictly inside the player's rectangle: anything around it (an icon,
// a label, a border) is what misreads come from (bench of 2026-10-03: 198 of 204 boxes right).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use image::{imageops, RgbImage};
use rten::Model;
use rten_tensor::prelude::*;
use rten_tensor::{NdTensor, Tensor};

use crate::ocr::{Rect, Shown, Word};

/// DB post-processing, from PP-OCRv6_small_det's inference.yml: pixel threshold, least mean
/// score of a box, how far a box grows (the detector marks a shrunk core of the text).
const THRESH: f32 = 0.2;
const BOX_THRESH: f32 = 0.45;
const UNCLIP: f32 = 1.4;
/// Recognition input height.
const LINE_H: u32 = 48;
/// A picked number's digits must each be read with at least this probability, else the read
/// doesn't count (a guess narrowed searches past the real value: Quake II RTX's chunky digits
/// read 19 as 10 with its 0 at 0.02 and 8 as 3 at 0.005; right digits there were >= 0.93).
const MIN_DIGIT_PROB: f32 = 0.6;

struct Reader {
    det: Model,
    rec: Model,
    /// Class i (from 1; 0 is the CTC blank) is chars[i - 1].
    chars: Vec<String>,
}

/// A recognised line, with each character's centre x (frame pixels).
struct Line {
    text: String,
    chars: Vec<(String, f32)>,
    rect: Rect,
    /// The probability of the least likely digit (1 without digits).
    weakest_digit: f32,
}

/// Where the models are: installed with the app, or `FERRET_OCR_MODELS` (tools run outside it).
fn models_dir() -> PathBuf {
    std::env::var_os("FERRET_OCR_MODELS").map_or_else(|| PathBuf::from("/app/share/ferret/ocr"), PathBuf::from)
}

fn reader() -> Result<&'static Reader, String> {
    static READER: OnceLock<Result<Reader, String>> = OnceLock::new();
    READER.get_or_init(|| Reader::load(&models_dir())).as_ref().map_err(Clone::clone)
}

impl Reader {
    fn load(dir: &Path) -> Result<Reader, String> {
        let model = |name: &str| Model::load_file(dir.join(name)).map_err(|e| format!("loading the OCR model {name}: {e}"));
        let yml = std::fs::read_to_string(dir.join("rec.yml")).map_err(|e| format!("reading the OCR dictionary: {e}"))?;
        let mut chars = dictionary(&yml);
        if chars.is_empty() {
            return Err("the OCR dictionary is empty".into());
        }
        chars.push(" ".into());
        Ok(Reader { det: model("det.onnx")?, rec: model("rec.onnx")?, chars })
    }

    /// Text boxes in the image.
    fn detect(&self, img: &RgbImage) -> Vec<Rect> {
        let w = ((img.width() as f32 / 32.0).round() as u32).max(1) * 32;
        let h = ((img.height() as f32 / 32.0).round() as u32).max(1) * 32;
        let small = imageops::resize(img, w, h, imageops::FilterType::Triangle);
        let (mean, std) = ([0.485f32, 0.456, 0.406], [0.229f32, 0.224, 0.225]);
        let mut data = vec![0f32; (3 * w * h) as usize];
        for (x, y, p) in small.enumerate_pixels() {
            // BGR, as PaddleOCR reads images with OpenCV.
            for c in 0..3 {
                data[(c as u32 * w * h + y * w + x) as usize] = (p[2 - c] as f32 / 255.0 - mean[c]) / std[c];
            }
        }
        let input = NdTensor::from_data([1, 3, h as usize, w as usize], data);
        let Some(out): Option<Tensor<f32>> = self.det.run_one(input.view().into(), None).ok().and_then(|o| o.try_into().ok()) else {
            return Vec::new();
        };
        let prob: Vec<f32> = out.iter().copied().collect();
        if prob.len() != (w * h) as usize {
            return Vec::new();
        }
        let (sx, sy) = (img.width() as f32 / w as f32, img.height() as f32 / h as f32);
        let mut seen = vec![false; prob.len()];
        let mut boxes = Vec::new();
        for start in 0..prob.len() {
            if seen[start] || prob[start] <= THRESH {
                continue;
            }
            seen[start] = true;
            let mut stack = vec![start];
            let (mut x0, mut y0, mut x1, mut y1, mut sum, mut n) = (w, h, 0, 0, 0f32, 0usize);
            while let Some(i) = stack.pop() {
                let (x, y) = (i as u32 % w, i as u32 / w);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                sum += prob[i];
                n += 1;
                for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                        continue;
                    }
                    let j = (ny as u32 * w + nx as u32) as usize;
                    if !seen[j] && prob[j] > THRESH {
                        seen[j] = true;
                        stack.push(j);
                    }
                }
            }
            let (bw, bh) = ((x1 - x0 + 1) as f32, (y1 - y0 + 1) as f32);
            if sum / (n as f32) < BOX_THRESH || bw.min(bh) < 3.0 {
                continue;
            }
            let d = bw * bh * UNCLIP / (2.0 * (bw + bh));
            let left = ((x0 as f32 - d) * sx).max(0.0) as u32;
            let top = ((y0 as f32 - d) * sy).max(0.0) as u32;
            let right = (((x1 as f32 + 1.0 + d) * sx).ceil() as u32).min(img.width());
            let bottom = (((y1 as f32 + 1.0 + d) * sy).ceil() as u32).min(img.height());
            boxes.push(Rect { x: left, y: top, w: right - left, h: bottom - top });
        }
        boxes
    }

    /// Reads one box as a line of text. `digits`: only digits and number separators may win a
    /// step (letters can't), for a box the player says holds a number.
    fn recognize(&self, img: &RgbImage, b: Rect, digits: bool) -> Option<Line> {
        if b.w < 2 || b.h < 2 || b.x + b.w > img.width() || b.y + b.h > img.height() {
            return None;
        }
        let crop = imageops::crop_imm(img, b.x, b.y, b.w, b.h).to_image();
        let w = ((LINE_H as f32 * b.w as f32 / b.h as f32).ceil() as u32).clamp(16, 3200);
        let line = imageops::resize(&crop, w, LINE_H, imageops::FilterType::Triangle);
        let mut data = vec![0f32; (3 * w * LINE_H) as usize];
        for (x, y, p) in line.enumerate_pixels() {
            for c in 0..3 {
                data[(c as u32 * w * LINE_H + y * w + x) as usize] = (p[2 - c] as f32 / 255.0 - 0.5) / 0.5;
            }
        }
        let input = NdTensor::from_data([1, 3, LINE_H as usize, w as usize], data);
        let out: Tensor<f32> = self.rec.run_one(input.view().into(), None).ok()?.try_into().ok()?;
        let shape = out.shape().to_vec();
        let (steps, classes) = (*shape.get(1)?, *shape.get(2)?);
        let probs: Vec<f32> = out.iter().copied().collect();
        let allowed = |c: usize| {
            !digits || c == 0 || self.chars.get(c - 1).is_some_and(|s| s.chars().all(|ch| ch.is_ascii_digit() || "/.:,".contains(ch)))
        };
        let mut chars = Vec::new();
        let mut weakest_digit = 1f32;
        let mut last = 0;
        for t in 0..steps {
            let row = &probs[t * classes..(t + 1) * classes];
            let best = (0..classes).filter(|c| allowed(*c)).max_by(|a, b| row[*a].total_cmp(&row[*b]))?;
            if best != 0 && best != last {
                if let Some(c) = self.chars.get(best - 1) {
                    if c.chars().all(|ch| ch.is_ascii_digit()) {
                        weakest_digit = weakest_digit.min(row[best]);
                    }
                    chars.push((c.clone(), b.x as f32 + (t as f32 + 0.5) / steps as f32 * b.w as f32));
                }
            }
            last = best;
        }
        Some(Line { text: chars.iter().map(|c| c.0.as_str()).collect(), chars, rect: b, weakest_digit })
    }
}

/// The character list in a PaddleOCR inference.yml (`character_dict:` entries, YAML-quoted
/// where needed).
fn dictionary(yml: &str) -> Vec<String> {
    let mut lines = yml.lines().skip_while(|l| l.trim() != "character_dict:").skip(1);
    let mut chars = Vec::new();
    for l in lines.by_ref() {
        let Some(v) = l.trim_start().strip_prefix("- ") else { break };
        let v = if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
            v[1..v.len() - 1].replace("''", "'")
        } else if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
            unescape(&v[1..v.len() - 1])
        } else {
            v.to_owned()
        };
        chars.push(v);
    }
    chars
}

/// A YAML double-quoted string's escapes (`\"`, `\\`, `\uXXXX`).
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('u') => {
                let hex: String = it.by_ref().take(4).collect();
                out.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
            }
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

/// The first number in a piece of text: digits with their separators ("ENERGY 13/40" -> 13).
fn first_number(text: &str) -> Option<Shown> {
    text.split(|c: char| !(c.is_ascii_digit() || ",.:/".contains(c)))
        .map(|t| t.trim_matches(|c: char| !c.is_ascii_digit()))
        .filter(|t| t.chars().any(|c| c.is_ascii_digit()))
        .find_map(|t| Shown::parse(t).or_else(|| crate::ocr::digits(t).and_then(|n| Shown::parse(&n.to_string()))))
}

/// `area` of the frame, clipped to it; None when nothing is left.
fn clip(img: &RgbImage, area: Rect) -> Option<Rect> {
    let x = area.x.min(img.width());
    let y = area.y.min(img.height());
    let (w, h) = (area.w.min(img.width() - x), area.h.min(img.height() - y));
    (w >= 2 && h >= 2).then_some(Rect { x, y, w, h })
}

/// The number shown inside `area`, read from that rectangle only: the text detected there
/// (enlarged 3x, small HUD text is under the detector's size), the tallest line with a number;
/// else the whole rectangle as one line of digits. Lines with a doubtful digit don't count.
pub fn read_box(img: &RgbImage, area: Rect) -> Result<Option<Shown>, String> {
    let r = reader()?;
    let Some(a) = clip(img, area) else { return Ok(None) };
    let crop = imageops::crop_imm(img, a.x, a.y, a.w, a.h).to_image();
    let big = imageops::resize(&crop, a.w * 3, a.h * 3, imageops::FilterType::Triangle);
    let sure = |l: &Line| l.weakest_digit >= MIN_DIGIT_PROB;
    let mut lines: Vec<Line> = r.detect(&big).into_iter().filter_map(|b| r.recognize(&big, b, false)).filter(sure).collect();
    lines.sort_by_key(|l| std::cmp::Reverse(l.rect.h));
    if let Some(n) = lines.iter().find_map(|l| first_number(&l.text)) {
        return Ok(Some(n));
    }
    let whole = Rect { x: 0, y: 0, w: a.w, h: a.h };
    Ok(r.recognize(&crop, whole, true).filter(sure).and_then(|l| first_number(&l.text)))
}

/// What `area` says, letters and all (the learner skips labels like "AMMO").
pub fn text(img: &RgbImage, area: Rect) -> Result<String, String> {
    let r = reader()?;
    let Some(a) = clip(img, area) else { return Ok(String::new()) };
    let crop = imageops::crop_imm(img, a.x, a.y, a.w, a.h).to_image();
    Ok(r.recognize(&crop, Rect { x: 0, y: 0, w: a.w, h: a.h }, false).map(|l| l.text).unwrap_or_default())
}

/// The numbers on the whole frame, each boxed around its own characters (a line "Mission Score
/// 7614" gives 7614 alone), for the player to click.
pub fn numbers(img: &RgbImage) -> Result<Vec<Word>, String> {
    let r = reader()?;
    let mut found = Vec::new();
    for l in r.detect(img).into_iter().filter_map(|b| r.recognize(img, b, false)) {
        // Character spacing, for the box's left and right ends.
        let step = if l.chars.len() > 1 {
            (l.chars[l.chars.len() - 1].1 - l.chars[0].1) / (l.chars.len() - 1) as f32
        } else {
            l.rect.h as f32 * 0.6
        };
        let mut run: Vec<&(String, f32)> = Vec::new();
        let mut flush = |run: &mut Vec<&(String, f32)>| {
            let text: String = run.iter().map(|c| c.0.as_str()).collect();
            if let (Some(n), Some(first), Some(last)) = (first_number(&text), run.first(), run.last()) {
                let x0 = (first.1 - step / 2.0).max(l.rect.x as f32) as u32;
                let x1 = ((last.1 + step / 2.0) as u32).min(l.rect.x + l.rect.w).max(x0 + 1);
                found.push(Word { text: n.to_string(), rect: Rect { x: x0, y: l.rect.y, w: x1 - x0, h: l.rect.h }, conf: 90.0 });
            }
            run.clear();
        };
        for c in &l.chars {
            if c.0.chars().all(|ch| ch.is_ascii_digit() || ",.:/".contains(ch)) {
                run.push(c);
            } else {
                flush(&mut run);
            }
        }
        flush(&mut run);
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_dictionary_out_of_inference_yml() {
        let yml = "PostProcess:\n  name: CTCLabelDecode\n  character_dict:\n  - '!'\n  - ''''\n  - \"\\u00B0\"\n  - '0'\n  - a\n  - ','\nPreProcess:\n";
        assert_eq!(dictionary(yml), ["!", "'", "°", "0", "a", ","]);
    }

    #[test]
    fn finds_the_number_in_text() {
        let n = |t: &str| first_number(t).map(|s| s.to_string());
        assert_eq!(n("ENERGY 13/40"), Some("13".into()));
        assert_eq!(n("$8,810"), Some("8810".into()));
        assert_eq!(n("x57"), Some("57".into()));
        assert_eq!(n("Lv.17"), Some("17".into()));
        assert_eq!(n("3:17"), Some("3:17".into()));
        assert_eq!(n("no digits"), None);
    }
}
