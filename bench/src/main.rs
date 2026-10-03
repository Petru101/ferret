// Compares OCR engines on the test set (ocrtest/truth.txt): the app's own pipeline ("app": no
// learned digits, what a first read sees; its models from FERRET_OCR_MODELS, e.g. models/app),
// ocrs and PaddleOCR variants. Run inside the Ferret flatpak (any runtime with the libs works):
//   bench/run.sh build --release
//   flatpak run --filesystem=$HOME/Projects/ferret --filesystem=<ocrtest> \
//       --command=$HOME/Projects/ferret/bench/target/release/ocr-bench io.github.Petru101.Ferret \
//       <ocrtest>/truth.txt <models dir> [tess] [ocrs]

#[path = "../../src/font.rs"]
#[allow(dead_code)]
mod font;
#[path = "../../src/ocr.rs"]
#[allow(dead_code)]
mod ocr;
#[path = "../../src/reader.rs"]
#[allow(dead_code)]
mod reader;
mod paddle;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use image::{imageops, RgbImage};
use ocr::Shown;
use ocrs::{ImageSource, OcrEngine, OcrEngineParams, TextItem};
use rten_imageproc::{Point, RectF, RotatedRect};

enum Case {
    Read { area: ocr::Rect, shown: Shown },
    Find { shown: Shown, x: i64, y: i64 },
}

/// A number found on a frame, with its centre.
struct Found {
    shown: Shown,
    x: i64,
    y: i64,
}

/// The numbers in a piece of text: runs of digits with their separators ("ENERGY 13/40" -> 13).
fn numbers_in(text: &str) -> Vec<Shown> {
    text.split(|c: char| !(c.is_ascii_digit() || ",.:/".contains(c)))
        .map(|t| t.trim_matches(|c: char| !c.is_ascii_digit()))
        .filter(|t| t.chars().any(|c| c.is_ascii_digit()))
        .filter_map(|t| Shown::parse(t).or_else(|| ocr::digits(t).and_then(|n| Shown::parse(&n.to_string()))))
        .collect()
}

fn rect(top: f32, left: f32, bottom: f32, right: f32) -> RectF {
    RectF::new(Point::from_yx(top, left), Point::from_yx(bottom, right))
}

struct Ocrs {
    /// Any text: finds numbers among words.
    free: OcrEngine,
    /// Digits and separators only: a box the player says holds a number.
    digits: OcrEngine,
}

impl Ocrs {
    fn load(models: &Path) -> Ocrs {
        let engine = |allowed: Option<&str>| {
            OcrEngine::new(OcrEngineParams {
                detection_model: Some(rten::Model::load_file(models.join("text-detection.rten")).expect("detection model")),
                recognition_model: Some(rten::Model::load_file(models.join("text-recognition.rten")).expect("recognition model")),
                allowed_chars: allowed.map(str::to_owned),
                ..Default::default()
            })
            .expect("ocrs engine")
        };
        Ocrs { free: engine(None), digits: engine(Some("0123456789/.:,")) }
    }

    /// Detected words with their boxes (frame pixels, before `scale`).
    fn words(&self, img: &RgbImage, scale: f32) -> Vec<(String, RectF)> {
        let src = ImageSource::from_bytes(img.as_raw(), img.dimensions()).unwrap();
        let input = self.free.prepare_input(src).unwrap();
        let rects = self.free.detect_words(&input).unwrap();
        let lines = self.free.find_text_lines(&input, &rects);
        let texts = self.free.recognize_text(&input, &lines).unwrap();
        texts
            .iter()
            .flatten()
            .flat_map(|l| l.words().map(|w| (w.to_string(), w.bounding_rect().to_f32())).collect::<Vec<_>>())
            .map(|(t, r)| (t, rect(r.top() / scale, r.left() / scale, r.bottom() / scale, r.right() / scale)))
            .collect()
    }

    fn find(&self, img: &RgbImage, scale: u32) -> Vec<Found> {
        let big;
        let img = if scale > 1 {
            big = imageops::resize(img, img.width() * scale, img.height() * scale, imageops::FilterType::CatmullRom);
            &big
        } else {
            img
        };
        self.words(img, scale as f32)
            .into_iter()
            .flat_map(|(t, r)| {
                let (x, y) = ((r.left() + r.right()) as i64 / 2, (r.top() + r.bottom()) as i64 / 2);
                numbers_in(&t).into_iter().map(move |shown| Found { shown, x, y })
            })
            .collect()
    }

    /// The box, padded and scaled up (the detector misses text under ~10 px).
    fn crop(img: &RgbImage, a: &ocr::Rect, scale: u32) -> RgbImage {
        Self::crop_with(img, a, scale, imageops::FilterType::Nearest)
    }

    fn crop_with(img: &RgbImage, a: &ocr::Rect, scale: u32, filter: imageops::FilterType) -> RgbImage {
        let pad: u32 = std::env::var("BOX_PAD").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
        let (x, y) = (a.x.saturating_sub(pad), a.y.saturating_sub(pad));
        let (w, h) = ((a.w + 2 * pad).min(img.width() - x), (a.h + 2 * pad).min(img.height() - y));
        let c = imageops::crop_imm(img, x, y, w, h).to_image();
        imageops::resize(&c, w * scale, h * scale, filter)
    }

    /// Detection inside the box, then the tallest number.
    fn read_detect(&self, img: &RgbImage, a: &ocr::Rect) -> Option<Shown> {
        let c = Self::crop(img, a, 3);
        let mut words = self.words(&c, 1.0);
        words.sort_by(|a, b| (b.1.height()).total_cmp(&a.1.height()));
        words.iter().find_map(|(t, _)| numbers_in(t).into_iter().next())
    }

    /// The whole box as one line of text, digits only; `scale` > 1 smooths pixel fonts.
    fn read_line(&self, img: &RgbImage, a: &ocr::Rect, scale: u32) -> Option<Shown> {
        let c = Self::crop_with(img, a, scale, imageops::FilterType::CatmullRom);
        let src = ImageSource::from_bytes(c.as_raw(), c.dimensions()).unwrap();
        let input = self.digits.prepare_input(src).unwrap();
        let line = vec![RotatedRect::from_rect(rect(0.0, 0.0, c.height() as f32, c.width() as f32))];
        let text = self.digits.recognize_text(&input, &[line]).unwrap();
        text.into_iter().flatten().find_map(|l| numbers_in(&l.to_string()).into_iter().next())
    }
}

/// `list <dir> <models>`: every number each engine finds on each frame, for writing truth.txt.
fn list(dir: &Path, models: &Path) {
    let o = Ocrs::load(models);
    let v6 = models.join("v6");
    let pd = paddle::Paddle::load(
        &v6.join("medium_det.onnx"),
        &v6.join("medium_rec.onnx"),
        &v6.join("medium_dict.txt"),
        paddle::DetSettings { thresh: 0.2, box_thresh: 0.45, unclip: 1.4 },
    );
    let mut frames: Vec<PathBuf> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "png")).collect();
    frames.sort();
    for path in frames {
        let img = image::open(&path).unwrap().to_rgb8();
        println!("== {} {}x{}", path.file_name().unwrap().to_string_lossy(), img.width(), img.height());
        let tess: Vec<String> = ocr::numbers(&path, None)
            .unwrap_or_default()
            .iter()
            .map(|w| format!("{}@{},{}+{}x{}", w.text, w.rect.x, w.rect.y, w.rect.w, w.rect.h))
            .collect();
        println!("  tess: {}", tess.join(" "));
        let words: Vec<String> = o
            .words(&img, 1.0)
            .iter()
            .filter(|(t, _)| t.chars().any(|c| c.is_ascii_digit()))
            .map(|(t, r)| format!("{t}@{},{}+{}x{}", r.left() as i64, r.top() as i64, r.width() as i64, r.height() as i64))
            .collect();
        println!("  ocrs: {}", words.join(" "));
        let lines = pd.read_all(&img, 1.0);
        let texts: Vec<String> = lines.iter().filter(|l| l.text.chars().any(|c| c.is_ascii_digit())).map(|l| {
            let x = l.chars.iter().map(|c| c.1).sum::<f32>() / l.chars.len().max(1) as f32;
            format!("{:?}@{},{}", l.text, x as i64, l.y as i64)
        }).collect();
        println!("  v6m: {}", texts.join(" "));
    }
}

/// The numbers on PaddleOCR's lines, each at the centre of its own characters.
fn paddle_found(lines: &[paddle::Line]) -> Vec<Found> {
    let mut found = Vec::new();
    for l in lines {
        let mut run: Vec<&(String, f32)> = Vec::new();
        let flush = |run: &mut Vec<&(String, f32)>, found: &mut Vec<Found>| {
            let text: String = run.iter().map(|c| c.0.as_str()).collect();
            if let Some(shown) = numbers_in(&text).into_iter().next() {
                let x = run.iter().map(|c| c.1).sum::<f32>() / run.len() as f32;
                found.push(Found { shown, x: x as i64, y: l.y as i64 });
            }
            run.clear();
        };
        for c in &l.chars {
            if c.0.chars().all(|ch| ch.is_ascii_digit() || ",.:/".contains(ch)) {
                run.push(c);
            } else {
                flush(&mut run, &mut found);
            }
        }
        flush(&mut run, &mut found);
    }
    found
}

/// The box padded like ocrs' reads, scaled up `scale` times.
fn padded(img: &RgbImage, a: &ocr::Rect, scale: u32) -> RgbImage {
    Ocrs::crop_with(img, a, scale, imageops::FilterType::Triangle)
}

/// Detection in the box, then the number on the tallest line.
fn paddle_read_detect(p: &paddle::Paddle, img: &RgbImage, a: &ocr::Rect, digits: bool) -> Option<Shown> {
    let c = padded(img, a, 3);
    let mut lines = p.read_all_as(&c, 1.0, digits);
    lines.sort_by(|a, b| b.h.total_cmp(&a.h));
    lines.iter().find_map(|l| numbers_in(&l.text).into_iter().next())
}

/// The whole box as one line.
fn paddle_read_line(p: &paddle::Paddle, img: &RgbImage, a: &ocr::Rect, digits: bool) -> Option<Shown> {
    let c = padded(img, a, 1);
    let l = p.recognize_as(&c, (0.0, 0.0, c.width() as f32, c.height() as f32), digits)?;
    numbers_in(&l.text).into_iter().next()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "list") {
        return list(Path::new(&args[1]), Path::new(&args[2]));
    }
    let truth = PathBuf::from(args.first().expect("truth.txt"));
    let models = PathBuf::from(args.get(1).expect("models dir"));
    let engines: Vec<&str> = if args.len() > 2 { args[2..].iter().map(String::as_str).collect() } else { vec!["app", "ocrs", "pd", "v6t", "v6s", "v6m"] };
    let dir = truth.parent().unwrap().to_owned();
    // Frames in file order, with their group ("own forager", "web valheim").
    let mut frames: Vec<(String, String, Vec<Case>)> = Vec::new();
    let mut group = String::from("other");
    for l in std::fs::read_to_string(&truth).unwrap().lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
        if let Some(g) = l.strip_prefix("group ") {
            group = g.trim().to_owned();
            continue;
        }
        let f: Vec<&str> = l.split_whitespace().collect();
        let n = |i: usize| f[i].parse::<u32>().unwrap();
        let case = match f[1] {
            "read" => Case::Read { area: ocr::Rect { x: n(2), y: n(3), w: n(4), h: n(5) }, shown: Shown::parse(f[6]).unwrap() },
            "find" => Case::Find { shown: Shown::parse(f[2]).unwrap(), x: n(3) as i64, y: n(4) as i64 },
            _ => panic!("bad line: {l}"),
        };
        match frames.iter_mut().find(|(name, _, _)| name == f[0]) {
            Some(fr) => fr.2.push(case),
            None => frames.push((f[0].to_owned(), group.clone(), vec![case])),
        }
    }
    let ocrs = engines.contains(&"ocrs").then(|| Ocrs::load(&models));
    let (pm, v6) = (models.join("paddle"), models.join("v6"));
    let v5_settings = paddle::DetSettings { thresh: 0.3, box_thresh: 0.6, unclip: 1.5 };
    let v6_settings = |box_thresh| paddle::DetSettings { thresh: 0.2, box_thresh, unclip: 1.4 };
    // (name for finding, box read with detection, box read as one line, model).
    // Box reads with digits only: "<name>-det#", "<name>-line#".
    let digit_methods = |name: &str| -> (&'static str, &'static str) {
        match name {
            "pd" => ("pd-det#", "pd-line#"),
            "v6t" => ("v6t-det#", "v6t-line#"),
            "v6s" => ("v6s-det#", "v6s-line#"),
            _ => ("v6m-det#", "v6m-line#"),
        }
    };
    let paddles: Vec<(&'static str, &'static str, &'static str, paddle::Paddle)> = [
        ("pd", "pd-det", "pd-line"),
        ("v6t", "v6t-det", "v6t-line"),
        ("v6s", "v6s-det", "v6s-line"),
        ("v6m", "v6m-det", "v6m-line"),
    ]
    .into_iter()
    .filter(|(name, _, _)| engines.contains(name))
    .map(|(name, det_m, line_m)| {
        let p = match name {
            "pd" => paddle::Paddle::load(
                &pm.join("mobile_det.onnx"),
                &pm.join("languages_english_rec.onnx"),
                &pm.join("languages_english_dict.txt"),
                v5_settings,
            ),
            tier => {
                let t = match tier {
                    "v6t" => "tiny",
                    "v6s" => "small",
                    _ => "medium",
                };
                paddle::Paddle::load(
                    &v6.join(format!("{t}_det.onnx")),
                    &v6.join(format!("{t}_rec.onnx")),
                    &v6.join(format!("{t}_dict.txt")),
                    v6_settings(if t == "tiny" { 0.4 } else { 0.45 }),
                )
            }
        };
        println!("{name}: {}", p.describe());
        (name, det_m, line_m, p)
    })
    .collect();
    let debug = std::env::temp_dir().join("bench-crop.png");
    // (group, method) -> (right, total, time).
    let mut score: BTreeMap<(String, &str), (usize, usize, Duration)> = BTreeMap::new();
    let show = |r: &Option<Shown>| r.as_ref().map_or("-".to_owned(), |s| s.to_string());
    for (frame, group, cases) in &frames {
        let mut tally = |m: &'static str, ok: bool, t: Duration| {
            for g in [group.clone(), group.split(' ').next().unwrap().to_owned() + " (all)"] {
                let s = score.entry((g, m)).or_default();
                s.0 += ok as usize;
                s.1 += 1;
                s.2 += t;
            }
        };
        let path = dir.join(frame);
        let img = image::open(&path).unwrap().to_rgb8();
        println!("== {frame}");
        let finds: Vec<&Case> = cases.iter().filter(|c| matches!(c, Case::Find { .. })).collect();
        let near = |found: &[Found], shown: &Shown, x: i64, y: i64| {
            found.iter().any(|f| f.shown == *shown && (f.x - x).abs() <= 40 && (f.y - y).abs() <= 25)
        };
        let list = |m: &'static str, found: Vec<Found>, t: Duration, tally: &mut dyn FnMut(&'static str, bool, Duration)| {
            let mut missed = Vec::new();
            for c in &finds {
                let Case::Find { shown, x, y } = c else { continue };
                let ok = near(&found, shown, *x, *y);
                if !ok {
                    missed.push(format!("{shown}@{x},{y}"));
                }
                tally(m, ok, t / finds.len() as u32);
            }
            println!("  find {m:<5} {:.1}s missed {} of {}: {}", t.as_secs_f32(), missed.len(), finds.len(), missed.join(" "));
        };
        if !finds.is_empty() {
            if engines.contains(&"app") {
                let start = Instant::now();
                let found = ocr::numbers(&path, None)
                    .unwrap_or_else(|e| {
                        println!("  app error: {e}");
                        Vec::new()
                    })
                    .iter()
                    .filter_map(|w| {
                        let shown = numbers_in(&w.text).into_iter().next()?;
                        Some(Found { shown, x: (w.rect.x + w.rect.w / 2) as i64, y: (w.rect.y + w.rect.h / 2) as i64 })
                    })
                    .collect();
                list("app", found, start.elapsed(), &mut tally);
            }
            if let Some(o) = &ocrs {
                let start = Instant::now();
                let found = o.find(&img, 1);
                list("ocrs", found, start.elapsed(), &mut tally);
            }
            for (m, _, _, p) in &paddles {
                let start = Instant::now();
                let found = paddle_found(&p.read_all(&img, 1.0));
                list(m, found, start.elapsed(), &mut tally);
            }
        }
        for c in cases {
            let Case::Read { area, shown } = c else { continue };
            let mut line = format!("  read {},{} {}x{} = {shown}:", area.x, area.y, area.w, area.h);
            let mut results: Vec<(&'static str, bool)> = Vec::new();
            let mut run = |m: &'static str, f: &mut dyn FnMut() -> Option<Shown>| {
                let start = Instant::now();
                let r = f();
                let ok = r.as_ref() == Some(shown);
                tally(m, ok, start.elapsed());
                results.push((m, ok));
                line += &format!("  {m} {}{}", show(&r), if ok { "" } else { " ✗" });
            };
            if engines.contains(&"app") {
                run("app-read", &mut || ocr::read_number_at(&path, *area, &debug, None).ok().flatten().map(|(n, _)| n));
            }
            if let Some(o) = &ocrs {
                run("ocrs-line", &mut || o.read_line(&img, area, 1));
                run("ocrs-det", &mut || o.read_detect(&img, area));
            }
            for (name, det_m, line_m, p) in &paddles {
                run(det_m, &mut || paddle_read_detect(p, &img, area, false));
                run(line_m, &mut || paddle_read_line(p, &img, area, false));
                let (dd, dl) = digit_methods(name);
                run(dd, &mut || paddle_read_detect(p, &img, area, true));
                run(dl, &mut || paddle_read_line(p, &img, area, true));
            }
            // Upper bound for combining engines: some engine read it.
            tally("any", results.iter().any(|r| r.1), Duration::ZERO);
            println!("{line}");
        }
    }
    let methods: Vec<&str> = [
        "app", "ocrs", "pd", "v6t", "v6s", "v6m", "app-read", "ocrs-line", "ocrs-det", "pd-det", "pd-line", "pd-det#", "pd-line#", "v6t-det",
        "v6t-line", "v6t-det#", "v6t-line#", "v6s-det", "v6s-line", "v6s-det#", "v6s-line#", "v6m-det", "v6m-line", "v6m-det#", "v6m-line#", "any",
    ]
        .into_iter()
        .filter(|m| score.keys().any(|k| k.1 == *m))
        .collect();
    let mut groups: Vec<String> = score.keys().map(|k| k.0.clone()).collect();
    groups.dedup();
    groups.sort_by_key(|g| (g.ends_with("(all)"), g.clone()));
    println!("\n== score (any = some read method right)");
    print!("{:<28}", "");
    for m in &methods {
        print!("{m:>10}");
    }
    println!();
    for g in &groups {
        print!("{g:<28}");
        for m in &methods {
            match score.get(&(g.clone(), *m)) {
                Some((ok, n, _)) => print!("{:>10}", format!("{ok}/{n}")),
                None => print!("{:>10}", ""),
            }
        }
        println!();
    }
    println!("\ntime per number:");
    for m in methods.iter().filter(|m| **m != "any") {
        let (n, t) = score.iter().filter(|(k, _)| k.1 == *m && k.0.ends_with("(all)")).fold((0, Duration::ZERO), |a, (_, v)| (a.0 + v.1, a.1 + v.2));
        if n > 0 {
            println!("  {m:<10} {:.2}s", t.as_secs_f32() / n as f32);
        }
    }
}
