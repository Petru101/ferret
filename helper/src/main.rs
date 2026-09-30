// Host-side helper, built as a separate static binary so it runs outside the
// flatpak runtime. Uses only std and libc.
mod helper;
mod trace;

fn main() {
    helper::run();
}
