// Host-side helper, built as a separate static binary so it runs outside the
// flatpak runtime. Uses only std and libc.
mod anticheat;
mod godot;
mod helper;
mod json;
mod launchers;
mod names;
mod pointers;
mod steam;
mod trace;

fn main() {
    helper::run();
}
