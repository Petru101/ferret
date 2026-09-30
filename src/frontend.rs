// The part that runs inside the flatpak sandbox. It cannot see host processes,
// so it starts cheat-poc-helper on the host via flatpak-spawn
// and forwards memory commands to it over stdin/stdout. Window capture and OCR
// happen here, inside the sandbox.

use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use crate::capture::WindowCapture;
use crate::ocr::{self, Rect};

fn flatpak_app_path() -> Option<String> {
    let info = fs::read_to_string("/.flatpak-info").ok()?;
    info.lines()
        .find_map(|l| l.strip_prefix("app-path="))
        .map(str::to_owned)
}

fn sandbox_report() {
    let pids: Vec<String> = fs::read_dir("/proc")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.chars().all(|c| c.is_ascii_digit()))
                .collect()
        })
        .unwrap_or_default();
    let ns = fs::read_link("/proc/self/ns/pid")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| e.to_string());
    println!("frontend in flatpak: {}", Path::new("/.flatpak-info").exists());
    println!("frontend pid namespace: {ns}");
    println!("processes visible to frontend: {} ({})", pids.len(), pids.join(" "));
}

fn cache_dir() -> PathBuf {
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache/cheat-poc"));
    fs::create_dir_all(&dir).ok();
    dir
}

struct Helper {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Helper {
    fn start() -> Self {
        let mut cmd = match flatpak_app_path() {
            Some(app) => {
                let mut c = Command::new("flatpak-spawn");
                c.args(["--host", "--watch-bus", &format!("{app}/bin/cheat-poc-helper")]);
                c
            }
            None => Command::new(std::env::current_exe().expect("current exe").with_file_name("cheat-poc-helper")),
        };
        let mut child = match cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("could not start host helper: {e}");
                std::process::exit(1);
            }
        };
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self { child, input, output }
    }

    fn call(&mut self, line: &str) -> Vec<String> {
        if writeln!(self.input, "{line}").and_then(|_| self.input.flush()).is_err() {
            eprintln!("host helper exited");
            std::process::exit(1);
        }
        let mut reply = Vec::new();
        loop {
            let mut l = String::new();
            match self.output.read_line(&mut l) {
                Ok(0) | Err(_) => {
                    eprintln!("host helper exited");
                    std::process::exit(1);
                }
                Ok(_) if l.trim_end() == "end" => return reply,
                Ok(_) => reply.push(l.trim_end().to_owned()),
            }
        }
    }
}

/// Match count from a helper `scan`/`next` reply ("... N matches ...").
fn match_count(reply: &[String]) -> Option<usize> {
    let line = reply.first()?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let i = words.iter().position(|w| *w == "matches")?;
    words.get(i.checked_sub(1)?)?.parse().ok()
}

// --- Profiles: named values that survive game restarts, one file per game.

fn profile_path(exe: &str) -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share/cheat-poc"));
    base.join("profiles").join(format!("{}.profile", exe.to_lowercase()))
}

/// Profile format: "entry <name>" followed by its "site ..." lines.
fn read_profile(exe: &str) -> Vec<(String, Vec<String>)> {
    let mut entries: Vec<(String, Vec<String>)> = Vec::new();
    for line in fs::read_to_string(profile_path(exe)).unwrap_or_default().lines() {
        if let Some(name) = line.strip_prefix("entry ") {
            entries.push((name.trim().to_owned(), Vec::new()));
        } else if let (Some(site), Some(e)) = (line.strip_prefix("site "), entries.last_mut()) {
            e.1.push(site.trim().to_owned());
        }
    }
    entries
}

fn write_profile(exe: &str, entries: &[(String, Vec<String>)]) -> Result<PathBuf, String> {
    let path = profile_path(exe);
    fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let mut text = String::new();
    for (name, sites) in entries {
        text.push_str(&format!("entry {name}\n"));
        for s in sites {
            text.push_str(&format!("site {s}\n"));
        }
    }
    fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(path)
}

struct Game {
    exe: String,
    entries: Vec<(String, u64)>,
}

/// The "survive restarts" button: finds the code that accesses the current
/// value and saves it under `name`, so the value can be found again next time.
fn cmd_save(game: &mut Option<Game>, helper: &mut Helper, name: &str) -> Result<(), String> {
    let game = game.as_mut().ok_or("attach to a game first")?;
    if name.is_empty() || name.contains(char::is_whitespace) {
        return Err("usage: save <name> (one word)".into());
    }
    let listed = helper.call("list");
    let [(addr, _)] = parse_values(&listed)[..] else {
        return Err("narrow down to exactly one address first".into());
    };
    let reply = helper.call(&format!("sites {addr:x}"));
    for l in reply.iter().filter(|l| !l.starts_with("site ")) {
        println!("{l}");
    }
    let sites: Vec<String> = reply.iter().filter_map(|l| l.strip_prefix("site ")).map(str::to_owned).collect();
    if sites.is_empty() {
        return Err("no usable code found; keep the game running (not paused) and try again".into());
    }
    let mut entries = read_profile(&game.exe);
    entries.retain(|(n, _)| n != name);
    entries.push((name.to_owned(), sites.clone()));
    let path = write_profile(&game.exe, &entries)?;
    game.entries.retain(|(n, _)| n != name);
    game.entries.push((name.to_owned(), addr));
    println!("saved {name} ({} code patterns) to {}", sites.len(), path.display());
    Ok(())
}

/// Finds every saved value of this game again.
fn cmd_restore(game: &mut Option<Game>, helper: &mut Helper) -> Result<(), String> {
    let game = game.as_mut().ok_or("attach to a game first")?;
    let entries = read_profile(&game.exe);
    if entries.is_empty() {
        return Err(format!("nothing saved for {}", game.exe));
    }
    let t = Instant::now();
    for (name, sites) in entries {
        let mut resolved = None;
        for site in &sites {
            let reply = helper.call(&format!("resolve {site}"));
            if let Some((addr, Some(v))) = parse_values(&reply).first() {
                resolved = Some((*addr, *v));
                break;
            }
            println!("{name}: {}", reply.join(" "));
        }
        match resolved {
            Some((addr, v)) => {
                println!("{name} = {v} (at 0x{addr:x})");
                game.entries.retain(|(n, _)| *n != name);
                game.entries.push((name, addr));
            }
            None => println!("{name}: not found yet, try `restore` again once the game has used it"),
        }
    }
    println!("restored in {} ms", t.elapsed().as_millis());
    Ok(())
}

fn cmd_values(game: &Option<Game>, helper: &mut Helper) -> Result<(), String> {
    let game = game.as_ref().ok_or("attach to a game first")?;
    let addrs: Vec<u64> = game.entries.iter().map(|(_, a)| *a).collect();
    for ((name, addr), v) in game.entries.iter().zip(peek(helper, &addrs)) {
        println!("{name:<12} {} (at 0x{addr:x})", v.map_or("??".into(), |v| v.to_string()));
    }
    Ok(())
}

struct Vision {
    capture: Option<WindowCapture>,
    words: Vec<ocr::Word>,
    area: Option<Rect>,
}

impl Vision {
    fn frame(&mut self) -> Result<PathBuf, String> {
        if self.capture.is_none() {
            self.capture = Some(WindowCapture::start()?);
        }
        let out = cache_dir().join("frame.png");
        self.capture.as_ref().unwrap().grab(&out)?;
        Ok(out)
    }

    fn read(&mut self) -> Result<Option<i64>, String> {
        let area = self.area.ok_or("no area picked; run `numbers` then `watch <n>`")?;
        let frame = self.frame()?;
        ocr::read_number(&frame, area, &cache_dir().join("area.png"))
    }
}

fn cmd_numbers(v: &mut Vision) -> Result<(), String> {
    let t = Instant::now();
    let frame = v.frame()?;
    v.words = ocr::numbers(&frame)?;
    for (i, w) in v.words.iter().enumerate() {
        let r = w.rect;
        println!("{i:>3}  {:<12} at {},{} {}x{}  conf {:.0}", w.text, r.x, r.y, r.w, r.h, w.conf);
    }
    println!("{} numbers found in {} ms (frame: {})", v.words.len(), t.elapsed().as_millis(), frame.display());
    Ok(())
}

fn cmd_watch(v: &mut Vision, arg: &str) -> Result<(), String> {
    let nums: Vec<u32> = arg.split_whitespace().filter_map(|n| n.parse().ok()).collect();
    let area = match nums.as_slice() {
        [i] => ocr::watch_area(v.words.get(*i as usize).ok_or("no such number, run `numbers` first")?.rect),
        [x, y, w, h] => Rect { x: *x, y: *y, w: *w, h: *h },
        _ => return Err("usage: watch <n> | watch <x> <y> <w> <h>".into()),
    };
    v.area = Some(area);
    println!("watching {},{} {}x{}", area.x, area.y, area.w, area.h);
    match v.read()? {
        Some(n) => println!("reads {n}"),
        None => println!("no number read there yet (crop: {})", cache_dir().join("area.png").display()),
    }
    Ok(())
}

/// Parses helper lines of the form "0x00003795366c = 96".
fn parse_values(lines: &[String]) -> Vec<(u64, Option<i64>)> {
    lines
        .iter()
        .filter_map(|l| {
            let (a, v) = l.split_once(" = ")?;
            Some((u64::from_str_radix(a.trim_start_matches("0x"), 16).ok()?, v.trim().parse().ok()))
        })
        .collect()
}

fn peek(helper: &mut Helper, addrs: &[u64]) -> Vec<Option<i64>> {
    let arg: Vec<String> = addrs.iter().map(|a| format!("{a:x}")).collect();
    let values = parse_values(&helper.call(&format!("peek {}", arg.join(" "))));
    addrs
        .iter()
        .map(|a| values.iter().find(|(b, _)| b == a).and_then(|(_, v)| *v))
        .collect()
}

/// Tells the real value apart from copies of it: writes a test value to one
/// candidate at a time and checks whether it sticks, whether the other
/// candidates follow it, and whether the screen shows it. Test writes are undone.
fn cmd_probe(v: &mut Vision, helper: &mut Helper) -> Result<(), String> {
    let listed = helper.call("list");
    if listed.iter().any(|l| l.starts_with("...")) {
        return Err("too many candidates to probe; narrow down first".into());
    }
    let addrs: Vec<u64> = parse_values(&listed).into_iter().map(|(a, _)| a).collect();
    if addrs.is_empty() {
        return Err("no candidates to probe".into());
    }
    for (i, &addr) in addrs.iter().enumerate() {
        let Some(orig) = peek(helper, &[addr])[0] else { continue };
        let test = orig + 10;
        helper.call(&format!("write {addr:x} {test}"));
        std::thread::sleep(Duration::from_millis(1500));
        let after = peek(helper, &addrs);
        let stuck = after[i] == Some(test);
        let followers = (0..addrs.len()).filter(|&j| j != i && after[j] == Some(test)).count();
        let screen = if v.area.is_some() { v.read().ok().flatten() } else { None };
        let shown = screen == Some(test);
        println!(
            "0x{addr:012x}: wrote {test}: {}, {followers} of {} others followed, screen shows {}",
            if stuck { "kept" } else { "game overwrote it" },
            addrs.len() - 1,
            screen.map_or("?".into(), |n| n.to_string()),
        );
        if after[i] == Some(test) {
            helper.call(&format!("write {addr:x} {orig}"));
        }
        if stuck && (followers > 0 || shown) {
            helper.call(&format!("keep {addr:x}"));
            println!("real value at 0x{addr:012x} (test write undone, set it with `set <n>`)");
            return Ok(());
        }
    }
    println!("no candidate behaved like the real value (is the game paused?)");
    Ok(())
}

/// The automated scan loop: read the number off the window, scan for it, and
/// keep narrowing down whenever it changes on screen.
fn cmd_auto(v: &mut Vision, helper: &mut Helper, arg: &str) -> Result<(), String> {
    let limit = Duration::from_secs(arg.parse().unwrap_or(600));
    let start = Instant::now();
    let read_stable = |v: &mut Vision| -> Result<Option<i64>, String> {
        let a = v.read()?;
        let b = v.read()?;
        Ok(if a == b { a } else { None })
    };
    let first = loop {
        if let Some(n) = read_stable(v)? {
            break n;
        }
        if start.elapsed() > limit {
            return Err("could not read the number".into());
        }
    };
    let reply = helper.call(&format!("scan {first}"));
    println!("screen shows {first}: {}", reply.join(" "));
    let Some(mut count) = match_count(&reply) else {
        return Err("scan failed".into());
    };
    let mut last = first;
    let mut unchanged_rounds = 0;
    while count > 1 && start.elapsed() < limit {
        std::thread::sleep(Duration::from_millis(300));
        let Some(now) = read_stable(v)? else { continue };
        if now == last {
            continue;
        }
        let reply = helper.call(&format!("next {now}"));
        let new_count = match_count(&reply).unwrap_or(0);
        println!("screen shows {now}: {}", reply.join(" "));
        if new_count == 0 {
            let reply = helper.call(&format!("scan {now}"));
            println!("lost it (the screen lags memory or OCR misread), rescanning: {}", reply.join(" "));
            count = match_count(&reply).unwrap_or(0);
            unchanged_rounds = 0;
        } else {
            unchanged_rounds = if new_count == count { unchanged_rounds + 1 } else { 0 };
            count = new_count;
        }
        last = now;
        if unchanged_rounds >= 3 {
            println!("{count} addresses keep following the value (likely the value plus copies of it)");
            break;
        }
    }
    if count > 1 && start.elapsed() >= limit {
        println!("time limit reached; the value never changed enough to narrow down to one address");
    }
    for l in helper.call("list") {
        println!("{l}");
    }
    if (2..=20).contains(&count) {
        println!("checking which one is the real value:");
        return cmd_probe(v, helper);
    }
    Ok(())
}

pub fn run() {
    let mut helper = Helper::start();
    let mut vision = Vision { capture: None, words: Vec::new(), area: None };
    let mut game: Option<Game> = None;

    let interactive = io::stdin().is_terminal();
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        if interactive {
            print!("> ");
            io::stdout().flush().ok();
        }
        let Some(Ok(line)) = lines.next() else { break };
        let line = line.trim().to_owned();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !interactive {
            println!("> {line}");
        }
        let (cmd, arg) = line.split_once(' ').unwrap_or((&line, ""));
        let res = match cmd {
            "quit" | "exit" => break,
            "sandbox" => {
                sandbox_report();
                Ok(())
            }
            "window" => WindowCapture::start().map(|c| {
                println!("capturing window, stream size {:?}", c.size);
                vision.capture = Some(c);
            }),
            "shot" => vision.frame().map(|f| println!("saved {}", f.display())),
            "numbers" => cmd_numbers(&mut vision),
            "watch" => cmd_watch(&mut vision, arg.trim()),
            "read" => vision.read().map(|n| println!("{n:?}")),
            "auto" => cmd_auto(&mut vision, &mut helper, arg.trim()),
            "probe" => cmd_probe(&mut vision, &mut helper),
            "save" => cmd_save(&mut game, &mut helper, arg.trim()),
            "restore" => cmd_restore(&mut game, &mut helper),
            "values" => cmd_values(&game, &mut helper),
            "attach" => {
                let reply = helper.call(&line);
                for l in &reply {
                    println!("{l}");
                }
                let exe = reply.iter().find_map(|l| l.strip_prefix("exe: "));
                game = exe.map(|exe| Game { exe: exe.to_owned(), entries: Vec::new() });
                match &game {
                    Some(g) if !read_profile(&g.exe).is_empty() => {
                        println!("found saved values for {}, restoring:", g.exe);
                        cmd_restore(&mut game, &mut helper)
                    }
                    _ => Ok(()),
                }
            }
            "set" if arg.split_whitespace().next().is_some_and(|n| n.parse::<i64>().is_err()) => {
                let mut it = arg.split_whitespace();
                let (name, value) = (it.next().unwrap_or_default(), it.next().unwrap_or_default());
                match game.as_ref().and_then(|g| g.entries.iter().find(|(n, _)| n == name)) {
                    Some((_, addr)) => {
                        for l in helper.call(&format!("write {addr:x} {value}")) {
                            println!("{l}");
                        }
                        Ok(())
                    }
                    None => Err(format!("no saved value called {name}")),
                }
            }
            _ => {
                for l in helper.call(&line) {
                    println!("{l}");
                }
                if cmd == "help" || cmd == "?" {
                    println!("vision: window, shot, numbers, watch <n>|<x y w h>, read, auto [seconds], probe\nrestarts: save <name>, restore, values, set <name> <n>");
                }
                Ok(())
            }
        };
        if let Err(e) = res {
            println!("error: {e}");
        }
        io::stdout().flush().ok();
    }
    drop(helper.input);
    helper.child.wait().ok();
}
