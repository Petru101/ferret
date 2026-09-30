// Runs the OCR on a saved frame, for checking it against new games without capturing:
//   cargo build --release --example ocr
//   flatpak run --filesystem=$HOME/Projects/ferret --command=target/release/examples/ocr \
//       io.github.Petru101.Ferret numbers frame.png
//   ... read frame.png <x> <y> <w> <h> crop.png

#[path = "../src/ocr.rs"]
#[allow(dead_code)]
mod ocr;

use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n = |i: usize| args.get(i).and_then(|v| v.parse().ok());
    match args.first().map(String::as_str) {
        Some("numbers") if args.len() == 2 => {
            let start = std::time::Instant::now();
            for w in ocr::numbers(Path::new(&args[1])).unwrap_or_else(|e| panic!("{e}")) {
                let r = w.rect;
                println!("{:<10} at {},{} {}x{}  conf {:.0}", w.text, r.x, r.y, r.w, r.h, w.conf);
            }
            println!("took {:.2?}", start.elapsed());
        }
        Some("read") if args.len() == 7 => {
            let (Some(x), Some(y), Some(w), Some(h)) = (n(2), n(3), n(4), n(5)) else {
                return eprintln!("x y w h must be numbers");
            };
            let area = ocr::Rect { x, y, w, h };
            println!("{:?}", ocr::read_number(Path::new(&args[1]), area, Path::new(&args[6])));
        }
        _ => eprintln!("usage: ocr numbers <frame.png> | ocr read <frame.png> <x> <y> <w> <h> <crop.png>"),
    }
}
