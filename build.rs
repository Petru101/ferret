// Windows: version information and the icon in ferret.exe (data/windows/winres.rs).
include!("data/windows/winres.rs");

fn main() {
    windows_resources("ferret", "Ferret", Some("data/windows/ferret.ico"));
}
