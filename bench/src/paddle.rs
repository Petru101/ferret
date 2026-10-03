// PaddleOCR (PP-OCRv5) through rten's ONNX loader: DB text detection, then CTC recognition of
// each detected box. Pre- and post-processing follow PaddleOCR's own pipeline (BGR input,
// ImageNet mean/std for detection, (x - 0.5) / 0.5 for recognition, 48 px tall text lines).

use std::path::Path;

use image::{imageops, RgbImage};
use rten::Model;
use rten_tensor::prelude::*;
use rten_tensor::{NdTensor, Tensor};

/// DB post-processing settings, as in each model's inference.yml: pixel threshold, minimum mean
/// score of a box, unclip ratio.
#[derive(Clone, Copy)]
pub struct DetSettings {
    pub thresh: f32,
    pub box_thresh: f32,
    pub unclip: f32,
}

pub struct Paddle {
    det: Model,
    rec: Model,
    /// Class i (from 1; 0 is the CTC blank) is chars[i - 1].
    chars: Vec<String>,
    settings: DetSettings,
}

/// A recognised text line, with each character's centre (frame pixels).
pub struct Line {
    pub text: String,
    pub chars: Vec<(String, f32)>,
    pub y: f32,
    pub h: f32,
}

/// x0, y0, x1, y1 in frame pixels.
pub type Box4 = (f32, f32, f32, f32);

impl Paddle {
    pub fn load(det: &Path, rec: &Path, dict: &Path, settings: DetSettings) -> Paddle {
        let mut chars: Vec<String> = std::fs::read_to_string(dict).unwrap().lines().map(str::to_owned).collect();
        chars.push(" ".into());
        Paddle { det: Model::load_file(det).expect("det model"), rec: Model::load_file(rec).expect("rec model"), chars, settings }
    }

    pub fn describe(&self) -> String {
        let shape = |m: &Model| {
            let id = m.input_ids()[0];
            format!("{:?}", m.node_info(id).and_then(|i| i.shape()))
        };
        format!("det input {}, rec input {}, {} classes", shape(&self.det), shape(&self.rec), self.chars.len() + 1)
    }

    /// Text boxes; `scale` resizes the frame first (detection rounds to multiples of 32).
    pub fn detect(&self, img: &RgbImage, scale: f32) -> Vec<Box4> {
        let w = ((img.width() as f32 * scale / 32.0).round() as u32).max(1) * 32;
        let h = ((img.height() as f32 * scale / 32.0).round() as u32).max(1) * 32;
        let small = imageops::resize(img, w, h, imageops::FilterType::Triangle);
        let (mean, std) = ([0.485f32, 0.456, 0.406], [0.229f32, 0.224, 0.225]);
        let mut data = vec![0f32; (3 * w * h) as usize];
        for (x, y, p) in small.enumerate_pixels() {
            // BGR, as PaddleOCR reads images with OpenCV.
            for c in 0..3 {
                let v = p[2 - c] as f32 / 255.0;
                data[(c as u32 * w * h + y * w + x) as usize] = (v - mean[c]) / std[c];
            }
        }
        let input = NdTensor::from_data([1, 3, h as usize, w as usize], data);
        let out: Tensor<f32> = self.det.run_one(input.view().into(), None).unwrap().try_into().unwrap();
        let prob: Vec<f32> = out.iter().copied().collect();
        let (sx, sy) = (img.width() as f32 / w as f32, img.height() as f32 / h as f32);
        let mut seen = vec![false; prob.len()];
        let mut boxes = Vec::new();
        for start in 0..prob.len() {
            let t = self.settings.thresh;
            if seen[start] || prob[start] <= t {
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
                    if !seen[j] && prob[j] > t {
                        seen[j] = true;
                        stack.push(j);
                    }
                }
            }
            let (bw, bh) = ((x1 - x0 + 1) as f32, (y1 - y0 + 1) as f32);
            if sum / (n as f32) < self.settings.box_thresh || bw.min(bh) < 3.0 {
                continue;
            }
            // Unclip: the detector marks the text's shrunk core.
            let d = bw * bh * self.settings.unclip / (2.0 * (bw + bh));
            let b = (
                ((x0 as f32 - d) * sx).max(0.0),
                ((y0 as f32 - d) * sy).max(0.0),
                ((x1 as f32 + 1.0 + d) * sx).min(img.width() as f32),
                ((y1 as f32 + 1.0 + d) * sy).min(img.height() as f32),
            );
            boxes.push(b);
        }
        boxes
    }

    /// Reads one box as a line of text. `digits`: only digits and number separators may win at each step (letters can't).
    pub fn recognize_as(&self, img: &RgbImage, b: Box4, digits: bool) -> Option<Line> {
        let (x0, y0) = (b.0.floor() as u32, b.1.floor() as u32);
        let (cw, ch) = ((b.2.ceil() as u32).min(img.width()) - x0, (b.3.ceil() as u32).min(img.height()) - y0);
        if cw < 2 || ch < 2 {
            return None;
        }
        let crop = imageops::crop_imm(img, x0, y0, cw, ch).to_image();
        let h = 48u32;
        let w = ((h as f32 * cw as f32 / ch as f32).ceil() as u32).clamp(16, 3200);
        let line = imageops::resize(&crop, w, h, imageops::FilterType::Triangle);
        let mut data = vec![0f32; (3 * w * h) as usize];
        for (x, y, p) in line.enumerate_pixels() {
            for c in 0..3 {
                data[(c as u32 * w * h + y * w + x) as usize] = (p[2 - c] as f32 / 255.0 - 0.5) / 0.5;
            }
        }
        let input = NdTensor::from_data([1, 3, h as usize, w as usize], data);
        let out: Tensor<f32> = self.rec.run_one(input.view().into(), None).ok()?.try_into().ok()?;
        let shape = out.shape().to_vec();
        let (steps, classes) = (shape[1], shape[2]);
        let probs: Vec<f32> = out.iter().copied().collect();
        let mut chars = Vec::new();
        let mut last = 0;
        for t in 0..steps {
            let row = &probs[t * classes..(t + 1) * classes];
            let allowed = |c: usize| !digits || c == 0 || self.chars.get(c - 1).is_some_and(|s| s.chars().all(|ch| ch.is_ascii_digit() || "/.:,".contains(ch)));
            let best = (0..classes).filter(|c| allowed(*c)).max_by(|a, b| row[*a].total_cmp(&row[*b])).unwrap();
            if best != 0 && best != last {
                if let Some(c) = self.chars.get(best - 1) {
                    chars.push((c.clone(), x0 as f32 + (t as f32 + 0.5) / steps as f32 * cw as f32));
                }
            }
            last = best;
        }
        Some(Line { text: chars.iter().map(|c| c.0.as_str()).collect(), chars, y: y0 as f32 + ch as f32 / 2.0, h: ch as f32 })
    }

    pub fn read_all(&self, img: &RgbImage, scale: f32) -> Vec<Line> {
        self.read_all_as(img, scale, false)
    }

    pub fn read_all_as(&self, img: &RgbImage, scale: f32, digits: bool) -> Vec<Line> {
        self.detect(img, scale).into_iter().filter_map(|b| self.recognize_as(img, b, digits)).collect()
    }
}
