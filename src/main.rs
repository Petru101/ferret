// A windowed program on Windows: no console window beside Ferret's (`--cli` attaches to the
// console it was started from: `attach_console`).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod bar;
mod capture;
mod cli;
mod core;
mod feedback;
mod font;
mod gui;
mod i18n;
mod library;
mod ocr;
mod online;
mod reader;
mod shapes;
mod share;
#[cfg(windows)]
mod update;

fn main() {
    #[cfg(windows)]
    update::on_start();
    if std::env::args().any(|a| a == "--cli") {
        #[cfg(windows)]
        attach_console();
        cli::run();
    } else {
        i18n::init();
        gui::run();
        #[cfg(windows)]
        update::apply_if_asked();
    }
}

/// The text interface of a windowed program: the pipes it was given (tests), else the console
/// of whatever started it (a terminal), its handles opened as CONIN$ and CONOUT$.
#[cfg(windows)]
fn attach_console() {
    use std::os::windows::io::IntoRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(pid: u32) -> i32;
        fn GetStdHandle(which: u32) -> isize;
        fn SetStdHandle(which: u32, handle: isize) -> i32;
    }
    const STD_INPUT: u32 = -10i32 as u32;
    const STD_OUTPUT: u32 = -11i32 as u32;
    const STD_ERROR: u32 = -12i32 as u32;
    const PARENT: u32 = u32::MAX;
    // SAFETY: plain Win32 calls; the handles opened are given to the process for good.
    unsafe {
        let given = |which| GetStdHandle(which) != 0 && GetStdHandle(which) != -1;
        if given(STD_OUTPUT) || AttachConsole(PARENT) == 0 {
            return;
        }
        let open = |name: &str, write: bool| std::fs::OpenOptions::new().read(!write).write(write).open(name).ok().map(|f| f.into_raw_handle() as isize);
        if let Some(h) = open("CONOUT$", true) {
            SetStdHandle(STD_OUTPUT, h);
            SetStdHandle(STD_ERROR, h);
        }
        if !given(STD_INPUT) {
            if let Some(h) = open("CONIN$", false) {
                SetStdHandle(STD_INPUT, h);
            }
        }
    }
}
