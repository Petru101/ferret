// Runs the OCR on a saved frame, for checking it against new games without capturing:
//   cargo build --release --example ocr
//   flatpak run --filesystem=$HOME/Projects/ferret --command=target/release/examples/ocr \
//       io.github.Petru101.Ferret numbers frame.png [digits-file]
//   ... read frame.png <x> <y> <w> <h> crop.png [digits-file]
//   ... learn frame.png <x> <y> <w> <h> <n> digits-file
//   ... follow <x> <y> <w> <h> digits-file frame.png... (watched reads over a series of frames)
//   ... fontnumbers frame.png digits-file
//   ... crop frame.png <x> <y> <w> <h> (the colour-group reader's candidates and glyphs)
//   ... glyphs frame.png <x> <y> <w> <h> digits-file (the glyph-sized shapes there, and what they read as)
// A digits file holds learned digit shapes (as in profiles/<game>.digits).

#[path = "../src/font.rs"]
#[allow(dead_code)]
mod font;
#[path = "../src/ocr.rs"]
#[allow(dead_code)]
mod ocr;

use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n = |i: usize| args.get(i).and_then(|v| v.parse().ok());
    let font = |i: usize| args.get(i).map(|p| font::Font::load(Path::new(p)));
    match args.first().map(String::as_str) {
        Some("numbers") if (2..=3).contains(&args.len()) => {
            let start = std::time::Instant::now();
            for w in ocr::numbers(Path::new(&args[1]), font(2).as_ref()).unwrap_or_else(|e| panic!("{e}")) {
                let r = w.rect;
                println!("{:<10} at {},{} {}x{}  conf {:.0}", w.text, r.x, r.y, r.w, r.h, w.conf);
            }
            println!("took {:.2?}", start.elapsed());
        }
        Some("read") if (7..=8).contains(&args.len()) => {
            let (Some(x), Some(y), Some(w), Some(h)) = (n(2), n(3), n(4), n(5)) else {
                return eprintln!("x y w h must be numbers");
            };
            let area = ocr::Rect { x, y, w, h };
            match ocr::read_number_at(Path::new(&args[1]), area, Path::new(&args[6]), font(7).as_ref(), &mut Vec::new()) {
                Ok((Some((n, learned)), _)) => println!("{n}{}", if learned { " (learned digits)" } else { "" }),
                r => println!("{r:?}"),
            }
        }
        Some("follow") if args.len() >= 7 => {
            let (Some(x), Some(y), Some(w), Some(h)) = (n(1), n(2), n(3), n(4)) else {
                return eprintln!("x y w h must be numbers");
            };
            let (mut area, f, mut seen) = (ocr::Rect { x, y, w, h }, font(5), Vec::new());
            for frame in &args[6..] {
                let crop = std::env::temp_dir().join("ocr-follow.png");
                let (read, to) = ocr::read_number_at(Path::new(frame), area, &crop, f.as_ref(), &mut seen).unwrap_or_else(|e| panic!("{e}"));
                let moved = to.is_some_and(|to| !ocr::overlaps(to, area));
                area = to.unwrap_or(area);
                let read = read.map_or("-".into(), |(n, l)| format!("{n}{}", if l { "" } else { " (tesseract)" }));
                println!("{frame}: {read:<12} area {},{} {}x{}{}", area.x, area.y, area.w, area.h, if moved { "  MOVED" } else { "" });
            }
        }
        Some("glyphs") if args.len() == 7 => {
            let (Some(x), Some(y), Some(w), Some(h)) = (n(2), n(3), n(4), n(5)) else {
                return eprintln!("x y w h must be numbers");
            };
            let f = font(6).unwrap();
            for l in ocr::glyphs_in(Path::new(&args[1]), &f, ocr::Rect { x, y, w, h }).unwrap_or_else(|e| panic!("{e}")) {
                println!("{l}");
            }
        }
        Some("crop") if args.len() == 6 => {
            let (Some(x), Some(y), Some(w), Some(h)) = (n(2), n(3), n(4), n(5)) else {
                return eprintln!("x y w h must be numbers");
            };
            for l in ocr::crop_glyphs(Path::new(&args[1]), ocr::Rect { x, y, w, h }).unwrap_or_else(|e| panic!("{e}")) {
                println!("{l}");
            }
        }
        Some("fontnumbers") if args.len() == 3 => {
            let f = font(2).unwrap();
            let mut words = ocr::learned_numbers(Path::new(&args[1]), &f).unwrap_or_else(|e| panic!("{e}"));
            words.sort_by_key(|w| (w.rect.y / 20, w.rect.x));
            let list: Vec<String> = words.iter().map(|w| format!("{}@{},{}", w.text, w.rect.x, w.rect.y)).collect();
            println!("{}", list.join(" "));
        }
        Some("learn") if args.len() == 8 => {
            let (Some(x), Some(y), Some(w), Some(h), Some(v)) = (n(2), n(3), n(4), n(5), ocr::Shown::parse(&args[6])) else {
                return eprintln!("x y w h n must be numbers");
            };
            let path = Path::new(&args[7]);
            let mut f = font::Font::load(path);
            let r = ocr::learn(Path::new(&args[1]), ocr::Rect { x, y, w, h }, &v, &mut f, false);
            println!("{r:?}");
            f.save(path).unwrap_or_else(|e| panic!("{e}"));
        }
        _ => eprintln!("usage: ocr numbers <frame.png> [digits] | ocr read <frame.png> <x> <y> <w> <h> <crop.png> [digits] | ocr learn <frame.png> <x> <y> <w> <h> <n> <digits>"),
    }
}
