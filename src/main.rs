mod bar;
mod capture;
mod cli;
mod core;
mod font;
mod gui;
mod ocr;
mod reader;
mod shapes;
mod share;

fn main() {
    if std::env::args().any(|a| a == "--cli") {
        cli::run();
    } else {
        gui::run();
    }
}
