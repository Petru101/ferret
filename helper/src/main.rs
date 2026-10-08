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
