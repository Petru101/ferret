// Windows: version information in ferret-helper.exe (data/windows/winres.rs).
include!("../data/windows/winres.rs");

fn main() {
    windows_resources("ferret-helper", "Ferret's helper: reads and changes the memory of games you play", None);
}
