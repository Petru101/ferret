mod capture;
mod cli;
mod core;
mod font;
mod gui;
mod ocr;

fn main() {
    if std::env::args().any(|a| a == "--cli") {
        cli::run();
    } else {
        gui::run();
    }
}
