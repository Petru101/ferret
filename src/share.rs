// `.ferret` files: a game's saved values as plain text, to share with other players (paste
// it in a chat, attach the file) or keep as a backup. Only the ways of finding a value that
// work on another computer go in; what comes in is checked line by line and starts out
// unconfirmed (core.rs: nothing is written before the player says the game shows its number).

use crate::core::Kind;

/// The first line of every file; a newer format gets a higher number.
const HEADER: &str = "ferret";
const VERSION: u32 = 1;

/// More than any real game's values (Forager's profile is 1.5 MB, nearly all unconfirmed
/// candidates, which never go in): anything bigger isn't a Ferret file.
pub const MAX_BYTES: usize = 1 << 20;
const MAX_VALUES: usize = 200;
const MAX_LINE: usize = 1024;
const MAX_PER_VALUE: usize = 64;

/// One saved value as it travels.
#[derive(Clone, Debug, PartialEq)]
pub struct Shared {
    pub name: String,
    pub kind: Kind,
    pub decimals: u32,
    /// Code patterns, as profiles keep them: "<pattern> <instr offset> <base reg> <disp>".
    pub sites: Vec<String>,
    /// The game build the code patterns were traced or checked in.
    pub build: Option<String>,
    /// Confirmed pointer paths starting in the game's own files.
    pub paths: Vec<String>,
    /// Engine paths and other names (`gd:`, `mono:`, `ue:`, `{amount|id=...}`, ...).
    pub named: Vec<String>,
    /// "<min|-> <max|->" in the game's units: only a suggestion on import.
    pub limit: Option<String>,
}

#[derive(Debug)]
pub struct File {
    /// The game's program, as Ferret names it (`game_name`).
    pub game: String,
    /// Its Steam app id, when Ferret knew it: games share program names (RPG Maker's
    /// `Game.exe`), so two ids that differ are two games.
    pub steam: Option<String>,
    pub build: Option<String>,
    pub values: Vec<Shared>,
    /// Values left out, and why.
    pub skipped: Vec<String>,
}

/// Modules a pointer path may start in for it to work on other computers: the game's own
/// program and libraries. Graphics drivers, Wine's and Steam's files and stacks differ
/// between players (YAW's and Wolfenstein's paths started in the Nvidia driver).
pub fn shareable_path(path: &str) -> bool {
    let start = path.split_whitespace().next().unwrap_or_default();
    let module = start.rsplit_once(['+', '-']).map_or(start, |(m, _)| m).to_lowercase();
    let stem = module.trim_end_matches(".dll").trim_end_matches(".exe");
    const SYSTEM: [&str; 52] = [
        "ntdll", "kernel32", "kernelbase", "user32", "gdi32", "gdi32full", "win32u", "advapi32", "cfgmgr32", "combase", "ole32",
        "oleaut32", "rpcrt4", "sechost", "shell32", "shlwapi", "ucrtbase", "msvcrt", "ws2_32", "winmm", "dinput", "dinput8",
        "dxgi", "dxcore", "opengl32", "vulkan-1", "winevulkan", "wined3d", "setupapi", "imm32", "version", "bcrypt", "crypt32",
        "dbghelp", "dsound", "mmdevapi", "winhttp", "wininet", "iphlpapi", "nsi", "netapi32", "secur32", "uxtheme", "dwmapi",
        "hid", "powrprof", "steamclient", "steamclient64", "gameoverlayrenderer", "gameoverlayrenderer64", "explorer", "winex11",
    ];
    const SYSTEM_START: [&str; 6] = ["msvcp", "vcruntime", "api-ms-win", "xinput", "xaudio", "d3d"];
    !(module.is_empty()
        || module.starts_with('[')
        || module.contains(".so")
        || SYSTEM.contains(&stem)
        || SYSTEM_START.iter().any(|s| stem.starts_with(s)))
}

/// Pointer paths kept per value: an import takes at most `MAX_PER_VALUE` ways of finding one
/// (MEA's pistol ammo had 141 confirmed paths), with room for its names and code patterns.
pub const PATHS_KEPT: usize = 48;

/// The file's text. Values with nothing that travels are left out by the caller.
pub fn write(game: &str, steam: Option<&str>, build: Option<&str>, values: &[Shared]) -> String {
    let mut text = format!("{HEADER} {VERSION}\ngame {game}\n");
    if let Some(s) = steam {
        text += &format!("steam {s}\n");
    }
    if let Some(b) = build {
        text += &format!("build {b}\n");
    }
    for v in values {
        text += &format!("\nvalue {}\n", v.name);
        if v.kind != Kind::I32 {
            text += &format!("type {}\n", v.kind.name());
        }
        if v.decimals > 0 {
            text += &format!("decimals {}\n", v.decimals);
        }
        for n in &v.named {
            text += &format!("named {n}\n");
        }
        for s in &v.sites {
            text += &format!("site {s}\n");
        }
        if let (Some(b), false) = (&v.build, v.sites.is_empty()) {
            text += &format!("build {b}\n");
        }
        for p in v.paths.iter().take(PATHS_KEPT) {
            text += &format!("path {p}\n");
        }
        if let Some(l) = &v.limit {
            text += &format!("limit {l}\n");
        }
    }
    text
}

fn hex(s: &str) -> bool {
    let s = s.strip_prefix('-').unwrap_or(s);
    !s.is_empty() && s.len() <= 16 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Names are one word everywhere (helper commands split on spaces, `resolve-all` on " | ").
fn good_name(name: &str) -> bool {
    !name.is_empty() && name.chars().count() <= 64 && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// "<pattern> <instr offset> <base reg> <disp>", the pattern in hex bytes and `??`.
fn good_site(site: &str) -> bool {
    let f: Vec<&str> = site.split_whitespace().collect();
    let [pattern, offset, reg, disp] = f[..] else { return false };
    let bytes = pattern.as_bytes();
    bytes.len() % 2 == 0
        && (8..=256).contains(&bytes.len())
        && bytes.chunks(2).all(|b| b == b"??" || b.iter().all(u8::is_ascii_hexdigit))
        && offset.parse::<u32>().is_ok_and(|o| (o as usize) < bytes.len() / 2)
        && !reg.is_empty()
        && reg.len() <= 5
        && reg.chars().all(|c| c.is_ascii_alphanumeric())
        && hex(disp)
}

/// "<module>+<offset> <offset>...".
fn good_path(path: &str) -> bool {
    let mut f = path.split_whitespace();
    let Some(start) = f.next() else { return false };
    let Some((module, off)) = start.rsplit_once(['+', '-']) else { return false };
    let offsets: Vec<&str> = f.collect();
    !module.is_empty() && hex(off) && offsets.len() <= 16 && offsets.iter().all(|o| hex(o))
}

/// An engine path or another name, as the helper's `named` takes it: one token of printable
/// characters starting the way such paths do.
fn good_named(named: &str) -> bool {
    const STARTS: [&str; 6] = ["gd:", "mono:", "il2cpp:", "ue:", "{", "\""];
    named.len() <= 512 && STARTS.iter().any(|s| named.starts_with(s)) && !named.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn good_limit(limit: &str) -> bool {
    let f: Vec<&str> = limit.split_whitespace().collect();
    f.len() == 2 && f.iter().all(|b| *b == "-" || b.parse::<f64>().is_ok_and(f64::is_finite))
}

/// Reads a file or pasted text (a chat's code block fences and blank lines are fine). Values
/// with a line Ferret can't take are skipped whole, with the reason.
pub fn read(text: &str) -> Result<File, String> {
    if text.len() > MAX_BYTES {
        return Err("This is too big to be a Ferret file.".into());
    }
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("```"));
    let not_ours = || "This isn't a Ferret file: it should start with \"ferret 1\".".to_owned();
    let first = lines.next().ok_or_else(not_ours)?;
    let version: u32 = first.strip_prefix(HEADER).and_then(|v| v.trim().parse().ok()).ok_or_else(not_ours)?;
    if version > VERSION {
        return Err("This file was made by a newer Ferret: update Ferret to import it.".into());
    }
    let mut file = File { game: String::new(), steam: None, build: None, values: Vec::new(), skipped: Vec::new() };
    // The value being read, and why it can't be taken (it is still read to its end).
    let mut current: Option<(Shared, Option<String>)> = None;
    let finish = |file: &mut File, current: Option<(Shared, Option<String>)>| {
        let Some((v, bad)) = current else { return };
        let why = bad.or_else(|| (v.sites.is_empty() && v.paths.is_empty() && v.named.is_empty()).then(|| "no way to find it".into()));
        match why {
            Some(why) => file.skipped.push(format!("{}: {why}", v.name)),
            None if file.values.len() >= MAX_VALUES => file.skipped.push(format!("{}: too many values in one file", v.name)),
            None if file.values.iter().any(|o| o.name == v.name) => file.skipped.push(format!("{}: in the file twice", v.name)),
            None => file.values.push(v),
        }
    };
    for line in lines {
        let (key, rest) = line.split_once(' ').map_or((line, ""), |(k, r)| (k, r.trim()));
        if line.len() > MAX_LINE {
            match current.as_mut() {
                Some((_, bad)) => *bad = Some("a line too long".into()),
                None => return Err("This file has a line too long to be a Ferret file.".into()),
            }
            continue;
        }
        match (key, current.as_mut()) {
            ("value", _) => {
                finish(&mut file, current.take());
                let name = crate::core::one_word(rest);
                let bad = (!good_name(&name)).then(|| "a name Ferret can't use (letters, digits, _ and - only)".to_owned());
                let shared = Shared {
                    name: if name.is_empty() { "(no name)".into() } else { name },
                    kind: Kind::I32,
                    decimals: 0,
                    sites: Vec::new(),
                    build: None,
                    paths: Vec::new(),
                    named: Vec::new(),
                    limit: None,
                };
                current = Some((shared, bad));
            }
            ("game", None) => file.game = rest.to_owned(),
            ("steam", None) => file.steam = Some(rest.to_owned()).filter(|s| !s.is_empty() && s.len() <= 12 && s.bytes().all(|b| b.is_ascii_digit())),
            ("build", None) => file.build = Some(rest.to_owned()).filter(|b| good_name(b)),
            (_, None) => {}
            (_, Some((v, bad))) => {
                let mut problem = |what: &str| {
                    bad.get_or_insert_with(|| what.to_owned());
                };
                let room = v.sites.len() + v.paths.len() + v.named.len() < MAX_PER_VALUE;
                match key {
                    "type" => match Kind::parse(rest) {
                        Some(k) => v.kind = k,
                        None => problem(&format!("a value type this Ferret doesn't know ({rest})")),
                    },
                    "decimals" => match rest.parse() {
                        Ok(d) if d <= 6 => v.decimals = d,
                        _ => problem("a broken decimals line"),
                    },
                    "named" if good_named(rest) && room => v.named.push(rest.to_owned()),
                    "site" if good_site(rest) && room => v.sites.push(rest.to_owned()),
                    "path" if good_path(rest) && room => v.paths.push(rest.to_owned()),
                    "named" | "site" | "path" if !room => problem("too many ways to find it"),
                    "named" => problem("a broken name line"),
                    "site" => problem("a broken code pattern"),
                    "path" => problem("a broken pointer path"),
                    "build" if good_name(rest) => v.build = Some(rest.to_owned()),
                    "build" => problem("a broken build line"),
                    "limit" if good_limit(rest) => v.limit = Some(rest.to_owned()),
                    "limit" => problem("a broken limit line"),
                    // From a newer Ferret: what this one knows of the value still works.
                    _ => {}
                }
            }
        }
    }
    finish(&mut file, current);
    if file.game.is_empty() {
        return Err("This Ferret file doesn't say which game it is for.".into());
    }
    if file.values.is_empty() && file.skipped.is_empty() {
        return Err("This Ferret file has no values in it.".into());
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brotato() -> Vec<Shared> {
        let named = |n: &str, path: &str| Shared {
            name: n.into(),
            kind: Kind::I32,
            decimals: 0,
            sites: Vec::new(),
            build: None,
            paths: Vec::new(),
            named: vec![path.into()],
            limit: None,
        };
        let mut health = named("health", "gd:entities/units/player/player.gd.current_stats.health");
        health.limit = Some("100 -".into());
        vec![named("coins", "gd:singletons/run_data.gd.players_data[0].gold"), health]
    }

    #[test]
    fn reads_what_it_writes() {
        let mut energy = Shared {
            name: "energy".into(),
            kind: Kind::F32,
            decimals: 0,
            sites: vec!["02ffc605????????01488bbb58010000f30f11b35c030000488b0d????????f6 16 ebx 860".into()],
            build: Some("6f77dc4f206f317f".into()),
            paths: vec!["CW4.exe+1a2b30 18 -8 20".into()],
            named: vec!["il2cpp:CommandBase._ammo".into()],
            limit: None,
        };
        let mut values = brotato();
        values.push(energy.clone());
        let text = write("brotato.exe", Some("1942280"), Some("abc123"), &values);
        let file = read(&text).unwrap();
        assert_eq!(file.game, "brotato.exe");
        assert_eq!(file.build.as_deref(), Some("abc123"));
        assert_eq!(file.steam.as_deref(), Some("1942280"));
        assert_eq!(file.values, values);
        assert!(file.skipped.is_empty());
        // Its build only goes with code patterns.
        energy.sites.clear();
        let file = read(&write("cw4.exe", None, None, &[energy])).unwrap();
        assert_eq!(file.values[0].build, None);
    }

    #[test]
    fn takes_text_pasted_from_a_chat() {
        let text = format!("Here you go:\n```\n{}```\n", write("brotato.exe", None, None, &brotato()));
        assert!(read(&text).is_err(), "text before the header isn't a file");
        let text = format!("```\n  {}\n```", write("brotato.exe", None, None, &brotato()).replace('\n', "\n  "));
        assert_eq!(read(&text).unwrap().values.len(), 2);
    }

    #[test]
    fn skips_values_it_cant_take() {
        let text = "ferret 1\ngame g.exe\n\
            value good\nnamed gd:a.gd.b\n\
            value newer\ntype i128\nnamed gd:a.gd.c\n\
            value spaced out\nsite zz 0 eax 10\n\
            value nothing\nlimit 1 2\n\
            value injected\nnamed gd:a.gd.b | write 0x1000:i32 5\n\
            value good\nnamed gd:a.gd.d\n\
            value future\nnamed gd:a.gd.e\nwobble 3\n";
        let file = read(text).unwrap();
        let names: Vec<&str> = file.values.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["good", "future"]);
        assert_eq!(file.skipped.len(), 5, "{:?}", file.skipped);
        assert!(file.skipped[0].starts_with("newer: a value type"));
        assert!(file.skipped[1].starts_with("spaced_out: a broken code pattern"));
        assert!(file.skipped[2].starts_with("nothing: no way to find it"));
        assert!(file.skipped[3].starts_with("injected: a broken name line"));
        assert!(file.skipped[4].starts_with("good: in the file twice"));
    }

    #[test]
    fn refuses_what_isnt_a_ferret_file() {
        assert!(read("").is_err());
        assert!(read("hello\nferret 1\ngame g\nvalue a\nnamed gd:x").is_err());
        assert!(read("ferret 2\ngame g\nvalue a\nnamed gd:x").unwrap_err().contains("newer"));
        assert!(read("ferret 1\nvalue a\nnamed gd:x").unwrap_err().contains("which game"));
        assert!(read(&"x".repeat(MAX_BYTES + 1)).is_err());
    }

    #[test]
    fn only_paths_from_the_games_own_files_travel() {
        for p in ["Forager.exe+3a1f20 10 28", "GameAssembly.dll+2c3d40 b8 0", "Adobe%20AIR.dll+1f00 8", "EE2.exe-10 4"] {
            assert!(shareable_path(p), "{p}");
        }
        for p in [
            "[stack]-d568 8 8",
            "libGLX_nvidia.so.0+5a000 8",
            "libnvidia-gpucomp.so.615.71.09+10 8",
            "ntdll.so+100 8",
            "steamclient.dll+44 8",
            "KERNEL32.dll+10 0",
            "d3d11.dll+10 0",
            "msvcp140.dll+10 0",
        ] {
            assert!(!shareable_path(p), "{p}");
        }
    }
}
