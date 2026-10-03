// Anti-cheat that ships with a game: its files are in the game's folder even before it's
// loaded (some games start it only when going online).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Lowercase file or folder name prefixes, and exact names.
const KNOWN: &[(&str, &[&str], &[&str])] = &[
    ("Easy Anti-Cheat", &["easyanticheat"], &["start_protected_game.exe"]),
    ("BattlEye", &["battleye", "beclient", "beservice"], &[]),
    ("PunkBuster", &["pnkbstr"], &["pbcl.dll", "pbsv.dll"]),
    ("nProtect GameGuard", &["npgg", "gamemon"], &["gameguard"]),
    ("XIGNCODE3", &["xigncode"], &["x3.xem"]),
    ("Anti-Cheat Expert", &["anticheatexpert", "sguard"], &[]),
    ("HoYoverse anti-cheat", &["mhyprot", "hoyokprotect"], &[]),
    ("EQU8", &["equ8"], &[]),
    ("EA Javelin", &["eaanticheat"], &[]),
    ("Denuvo Anti-Cheat", &["denuvo-anti-cheat", "denuvoanticheat"], &[]),
];

/// Folders a game's program sits in below the game's own folder (Unreal: Binaries/Win64).
const PROGRAM_DIRS: &[&str] =
    &["binaries", "win64", "win32", "linux", "linux64", "linux32", "x64", "x86", "x86_64", "bin", "bin64", "bin32"];

const DEPTH: usize = 5;
const MAX_ENTRIES: usize = 50_000;

static FOLDERS: Mutex<Option<HashMap<PathBuf, Option<&'static str>>>> = Mutex::new(None);

pub fn named(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    KNOWN
        .iter()
        .find(|(_, prefixes, exact)| prefixes.iter().any(|p| name.starts_with(p)) || exact.contains(&name.as_str()))
        .map(|(ac, ..)| *ac)
}

/// Any part of the path (a mapped library, say) named like anti-cheat.
pub fn in_path(path: &str) -> Option<&'static str> {
    path.split(['/', '\\']).find_map(named)
}

/// The game's folder for a program at `program`: up past the folders games keep programs in.
pub fn game_folder(program: &Path) -> PathBuf {
    let mut dir = program.parent().unwrap_or(Path::new("/")).to_path_buf();
    while dir.file_name().is_some_and(|n| PROGRAM_DIRS.contains(&n.to_string_lossy().to_ascii_lowercase().as_str())) {
        dir.pop();
    }
    dir
}

/// Searches `dir` a few levels deep, once per folder (the games list asks every second).
pub fn in_folder(dir: &Path) -> Option<&'static str> {
    let mut cache = FOLDERS.lock().unwrap();
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some(found) = cache.get(dir) {
        return *found;
    }
    let mut seen = 0;
    let found = search(dir, DEPTH, &mut seen);
    cache.insert(dir.to_path_buf(), found);
    found
}

fn search(dir: &Path, depth: usize, seen: &mut usize) -> Option<&'static str> {
    let mut subdirs = Vec::new();
    for e in fs::read_dir(dir).ok()?.flatten() {
        *seen += 1;
        if *seen > MAX_ENTRIES {
            return None;
        }
        if let Some(ac) = named(&e.file_name().to_string_lossy()) {
            return Some(ac);
        }
        // Not through symlinks: Wine prefixes link back up to / and the home folder.
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            subdirs.push(e.path());
        }
    }
    if depth == 0 {
        return None;
    }
    subdirs.iter().find_map(|d| search(d, depth - 1, seen))
}
