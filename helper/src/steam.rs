// What Steam knows about a game (appcache/appinfo.vdf): its name, whether it uses Valve
// Anti-Cheat, whether it's played with other people (an MMO always is) and whether it has
// Steam's online leaderboards.

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Clone, Default)]
pub struct App {
    pub name: String,
    pub vac: bool,
    pub multiplayer: bool,
    pub single_player: bool,
    pub mmo: bool,
    pub leaderboards: bool,
}

impl App {
    /// Played only with other people: an MMO, or not listed as single-player.
    pub fn online_only(&self) -> bool {
        self.mmo || self.multiplayer && !self.single_player
    }
}

/// Store categories: 2 = Single-player, 8 = Valve Anti-Cheat enabled; 1 = Multi-player,
/// 20 = MMO, 36 = Online PvP, 38 = Online Co-op; 25 = Steam Leaderboards.
const SINGLE_PLAYER: &str = "category_2";
const VAC: &str = "category_8";
const LEADERBOARDS: &str = "category_25";
const MMO: &str = "category_20";
const MULTIPLAYER: &[&str] = &["category_1", "category_20", "category_36", "category_38"];

/// The games list asks every second: answers are kept until appinfo.vdf changes.
static CACHE: Mutex<Option<HashMap<(PathBuf, u32), (SystemTime, Option<App>)>>> = Mutex::new(None);

/// `steam` = the Steam folder the game was started from (STEAM_COMPAT_CLIENT_INSTALL_PATH).
pub fn app(steam: Option<&str>, id: u32) -> Option<App> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let usual = [".local/share/Steam", ".steam/steam", ".var/app/com.valvesoftware.Steam/.local/share/Steam"];
    let roots = steam.map(PathBuf::from).into_iter().chain(usual.iter().map(|d| home.join(d)));
    for root in roots {
        let path = root.join("appcache/appinfo.vdf");
        let Ok(mtime) = path.metadata().and_then(|m| m.modified()) else { continue };
        let mut cache = CACHE.lock().unwrap();
        let cache = cache.get_or_insert_with(HashMap::new);
        let key = (path, id);
        match cache.get(&key) {
            Some((t, app)) if *t == mtime => return app.clone(),
            _ => {}
        }
        let app = read(&key.0, id);
        cache.insert(key, (mtime, app.clone()));
        return app;
    }
    None
}

/// Entries are <app id> <size> <header> <binary key-values>, ending with app id 0. Version 29
/// keeps the key names in a string table at the end (its offset follows the magic).
fn read(path: &Path, id: u32) -> Option<App> {
    let f = File::open(path).ok()?;
    let bytes = |off: u64, n: usize| {
        let mut b = vec![0; n];
        f.read_exact_at(&mut b, off).ok().map(|_| b)
    };
    let u32_at = |off: u64| bytes(off, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let (mut off, header, indexed) = match u32_at(0)? {
        0x0756_4427 => (8, 40, false),
        0x0756_4428 => (8, 60, false),
        0x0756_4429 => (16, 60, true),
        _ => return None,
    };
    loop {
        let (app, size) = (u32_at(off)?, u64::from(u32_at(off + 4)?));
        if app == 0 {
            return None;
        }
        if app == id {
            let data = bytes(off + 8 + header, size.checked_sub(header)? as usize)?;
            let table = if indexed { strings(&f)? } else { Vec::new() };
            return parse(&data, indexed.then_some(&table[..]));
        }
        off += 8 + size;
    }
}

fn strings(f: &File) -> Option<Vec<String>> {
    let mut b = [0; 8];
    f.read_exact_at(&mut b, 8).ok()?;
    let start = u64::from_le_bytes(b);
    let len = f.metadata().ok()?.len().checked_sub(start)?;
    let mut data = vec![0; len as usize];
    f.read_exact_at(&mut data, start).ok()?;
    let count = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
    Some(data[4..].split(|b| *b == 0).take(count).map(|s| String::from_utf8_lossy(s).into_owned()).collect())
}

/// Walks the key-values: type byte, key (table index or C string), value; 0 opens a
/// section and 8 closes one.
fn parse(d: &[u8], table: Option<&[String]>) -> Option<App> {
    let mut p = 0;
    let cstr = |p: &mut usize| {
        let end = *p + d.get(*p..)?.iter().position(|b| *b == 0)?;
        let s = String::from_utf8_lossy(&d[*p..end]).into_owned();
        *p = end + 1;
        Some(s)
    };
    let mut path: Vec<String> = Vec::new();
    let mut app = App::default();
    loop {
        let t = *d.get(p)?;
        p += 1;
        if t == 8 {
            if path.pop().is_none() {
                return Some(app);
            }
            continue;
        }
        let key = match table {
            Some(table) => {
                let i = u32::from_le_bytes(d.get(p..p + 4)?.try_into().ok()?) as usize;
                p += 4;
                table.get(i)?.clone()
            }
            None => cstr(&mut p)?,
        };
        let at = |want: &[&str]| path.iter().map(String::as_str).eq(want.iter().copied());
        if at(&["appinfo", "common", "category"]) {
            app.vac |= key == VAC;
            app.single_player |= key == SINGLE_PLAYER;
            app.mmo |= key == MMO;
            app.leaderboards |= key == LEADERBOARDS;
            app.multiplayer |= MULTIPLAYER.contains(&key.as_str());
        }
        match t {
            0 => path.push(key),
            1 => {
                let s = cstr(&mut p)?;
                if at(&["appinfo", "common"]) && key == "name" {
                    app.name = s;
                }
            }
            2 | 3 | 4 | 6 => p += 4,
            7 | 10 => p += 8,
            _ => return None,
        }
    }
}
