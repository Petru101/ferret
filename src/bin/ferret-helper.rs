// Host-side helper, built as a separate static binary so it runs outside the
// flatpak runtime. Uses only std and libc.
#[path = "../helper.rs"]
mod helper;
#[path = "../trace.rs"]
mod trace;

fn main() {
    helper::run();
}
