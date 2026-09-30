mod capture;
mod cli;
mod core;
mod ocr;

fn main() {
    if std::env::args().any(|a| a == "--cli") {
        cli::run();
    } else {
        cli::run();
    }
}
