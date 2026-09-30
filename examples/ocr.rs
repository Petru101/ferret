// Runs the OCR on a saved frame, for checking it against new games without capturing:
//   cargo build --release --example ocr
//   flatpak run --filesystem=$HOME/Projects/ferret --command=target/release/examples/ocr \
//       io.github.Petru101.Ferret numbers frame.png [digits-file]
//   ... read frame.png <x> <y> <w> <h> crop.png [digits-file]
//   ... learn frame.png <x> <y> <w> <h> <n> digits-file
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
            match ocr::read_number(Path::new(&args[1]), area, Path::new(&args[6]), font(7).as_ref()) {
                Ok(Some((n, learned))) => println!("{n}{}", if learned { " (learned digits)" } else { "" }),
                r => println!("{r:?}"),
            }
        }
        Some("learn") if args.len() == 8 => {
            let (Some(x), Some(y), Some(w), Some(h), Ok(v)) = (n(2), n(3), n(4), n(5), args[6].parse::<i64>()) else {
                return eprintln!("x y w h n must be numbers");
            };
            let path = Path::new(&args[7]);
            let mut f = font::Font::load(path);
            let r = ocr::learn(Path::new(&args[1]), ocr::Rect { x, y, w, h }, v, &mut f, false);
            println!("{r:?}");
            f.save(path).unwrap_or_else(|e| panic!("{e}"));
        }
        _ => eprintln!("usage: ocr numbers <frame.png> [digits] | ocr read <frame.png> <x> <y> <w> <h> <crop.png> [digits] | ocr learn <frame.png> <x> <y> <w> <h> <n> <digits>"),
    }
}
