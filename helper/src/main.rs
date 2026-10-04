// Host-side helper, built as a separate static binary so it runs outside the
// flatpak runtime. Uses only std and libc.
mod anticheat;
mod gdscript;
mod godot;
mod helper;
mod json;
mod launchers;
mod mono;
mod names;
mod pointers;
mod steam;
mod trace;
mod ue;

fn main() {
    helper::run();
}
