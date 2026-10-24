// Host-side helper, built as a separate static binary so it runs outside the
// flatpak runtime. Uses only std and libc.
mod air;
mod anticheat;
mod gdscript;
mod gml;
mod lua;
mod mem;
mod java;
mod python;
mod godot;
mod helper;
mod js;
mod json;
mod launchers;
mod mono;
mod names;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod os;
#[cfg(windows)]
#[path = "windows.rs"]
mod os;
mod pointers;
mod steam;
mod trace;
mod tweaks;
mod ue;
#[cfg(windows)]
mod win32;

fn main() {
    // `--as <uid>:<gid>:<home>`: who to become when started as root through pkexec (which wipes
    // the environment, HOME included). Checked against PKEXEC_UID in `drop_root`.
    let mut args = std::env::args().skip(1);
    let mut as_user = None;
    while let Some(a) = args.next() {
        if a == "--as" {
            as_user = args.next();
        }
    }
    helper::run(as_user.as_deref());
}
