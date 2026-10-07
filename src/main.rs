mod bar;
mod capture;
mod cli;
mod core;
mod feedback;
mod font;
mod gui;
mod library;
mod ocr;
mod online;
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
