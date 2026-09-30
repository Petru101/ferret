// Everything Ferret does, independent of the interface: the host helper,
// window capture and OCR, profiles and limits. Progress messages go to a log
// callback; results come back as values.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::capture::WindowCapture;
use crate::ocr::{self, Rect, Word};

fn flatpak_app_path() -> Option<String> {
    let info = fs::read_to_string("/.flatpak-info").ok()?;
    info.lines()
        .find_map(|l| l.strip_prefix("app-path="))
        .map(str::to_owned)
}

pub fn sandbox_report() -> Vec<String> {
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
    vec![
        format!("frontend in flatpak: {}", Path::new("/.flatpak-info").exists()),
        format!("frontend pid namespace: {ns}"),
        format!("processes visible to frontend: {} ({})", pids.len(), pids.join(" ")),
    ]
}

pub fn cache_dir() -> PathBuf {
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache/ferret"));
    fs::create_dir_all(&dir).ok();
    dir
}

/// The host helper, started through flatpak-spawn so it can see the game.
struct Helper {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}

impl Helper {
    fn start() -> Result<Self, String> {
        let mut cmd = match flatpak_app_path() {
            Some(app) => {
                let mut c = Command::new("flatpak-spawn");
                c.args(["--host", "--watch-bus", &format!("{app}/bin/ferret-helper")]);
                c
            }
            None => Command::new(std::env::current_exe().map_err(|e| e.to_string())?.with_file_name("ferret-helper")),
        };
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start host helper: {e}"))?;
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap());
        Ok(Self { child, input, output })
    }

    fn call(&mut self, line: &str) -> Vec<String> {
        let Some(input) = self.input.as_mut() else {
            return vec!["error: host helper exited".into()];
        };
        if writeln!(input, "{line}").and_then(|_| input.flush()).is_err() {
            return vec!["error: host helper exited".into()];
        }
        let mut reply = Vec::new();
        loop {
            let mut l = String::new();
            match self.output.read_line(&mut l) {
                Ok(0) | Err(_) => {
                    reply.push("error: host helper exited".into());
                    return reply;
                }
                Ok(_) if l.trim_end() == "end" => return reply,
                Ok(_) => reply.push(l.trim_end().to_owned()),
            }
        }
    }
}

impl Drop for Helper {
    // Closing its input ends the helper, which also stops any limits.
    fn drop(&mut self) {
        self.input.take();
        self.child.wait().ok();
    }
}

/// Match count from a helper `scan`/`next` reply ("... N matches ...").
fn match_count(reply: &[String]) -> Option<usize> {
    let line = reply.first()?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let i = words.iter().position(|w| *w == "matches")?;
    words.get(i.checked_sub(1)?)?.parse().ok()
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

fn first_error(reply: &[String]) -> Option<String> {
    reply.iter().find_map(|l| l.strip_prefix("error: ")).map(str::to_owned)
}

// --- Profiles: named values that survive game restarts, one file per game.

fn profile_path(exe: &str) -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share/ferret"));
    base.join("profiles").join(format!("{}.profile", exe.to_lowercase()))
}

struct Entry {
    name: String,
    sites: Vec<String>,
    /// "<min|-> <max|->" when the value is kept within a range.
    limit: Option<String>,
}

/// Profile format: "entry <name>" followed by its "site ..." lines and an
/// optional "limit <min|-> <max|->" line.
fn read_profile(exe: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in fs::read_to_string(profile_path(exe)).unwrap_or_default().lines() {
        if let Some(name) = line.strip_prefix("entry ") {
            entries.push(Entry { name: name.trim().to_owned(), sites: Vec::new(), limit: None });
        } else if let (Some(site), Some(e)) = (line.strip_prefix("site "), entries.last_mut()) {
            e.sites.push(site.trim().to_owned());
        } else if let (Some(limit), Some(e)) = (line.strip_prefix("limit "), entries.last_mut()) {
            e.limit = Some(limit.trim().to_owned());
        }
    }
    entries
}

fn write_profile(exe: &str, entries: &[Entry]) -> Result<PathBuf, String> {
    let path = profile_path(exe);
    fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let mut text = String::new();
    for e in entries {
        text.push_str(&format!("entry {}\n", e.name));
        for s in &e.sites {
            text.push_str(&format!("site {s}\n"));
        }
        if let Some(l) = &e.limit {
            text.push_str(&format!("limit {l}\n"));
        }
    }
    fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(path)
}

fn parse_range(limit: &str) -> (Option<i64>, Option<i64>) {
    let mut f = limit.split_whitespace().map(|v| v.parse().ok());
    (f.next().flatten(), f.next().flatten())
}

fn range_text(min: Option<i64>, max: Option<i64>) -> String {
    let show = |b: Option<i64>| b.map_or("-".to_owned(), |b| b.to_string());
    format!("{} {}", show(min), show(max))
}

pub fn limit_text(min: Option<i64>, max: Option<i64>) -> String {
    match (min, max) {
        (None, Some(max)) => format!("at most {max}"),
        (Some(min), None) => format!("at least {min}"),
        (Some(min), Some(max)) => format!("between {min} and {max}"),
        (None, None) => "unlimited".into(),
    }
}

// --- What the interfaces get back.

pub struct GameProcess {
    pub pid: u32,
    pub exe: String,
    pub app_id: Option<String>,
    pub anti_cheat: Option<String>,
}

pub struct ValueRow {
    pub name: String,
    pub addr: u64,
    pub value: Option<i64>,
    pub min: Option<i64>,
    pub max: Option<i64>,
    /// "fixed N times, restored M times, active" while a limit is enforced.
    pub limit_state: Option<String>,
}

pub enum AutoResult {
    /// One address left: the value.
    Found(u64),
    /// Several addresses still follow the value and none could be confirmed.
    Several(usize),
}

struct Game {
    exe: String,
    entries: Vec<(String, u64)>,
}

pub struct Core {
    helper: Helper,
    capture: Option<WindowCapture>,
    words: Vec<Word>,
    area: Option<Rect>,
    game: Option<Game>,
    log: Box<dyn FnMut(&str) + Send>,
    /// Set to stop a running `auto`.
    pub cancel: Arc<AtomicBool>,
}

impl Core {
    pub fn new(log: Box<dyn FnMut(&str) + Send>) -> Result<Self, String> {
        Ok(Self {
            helper: Helper::start()?,
            capture: None,
            words: Vec::new(),
            area: None,
            game: None,
            log,
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    fn say(&mut self, msg: &str) {
        (self.log)(msg);
    }

    /// Sends a command straight to the host helper.
    pub fn raw(&mut self, line: &str) -> Vec<String> {
        self.helper.call(line)
    }

    fn game(&mut self) -> Result<&mut Game, String> {
        self.game.as_mut().ok_or_else(|| "attach to a game first".into())
    }

    // --- Games

    pub fn games(&mut self) -> Vec<GameProcess> {
        self.helper
            .call("games")
            .iter()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                let opt = |s: &str| (s != "-").then(|| s.to_owned());
                Some(GameProcess {
                    pid: f.first()?.parse().ok()?,
                    exe: f.get(1)?.to_string(),
                    app_id: opt(f.get(2)?),
                    anti_cheat: opt(f.get(3)?),
                })
            })
            .collect()
    }

    /// Attaches and, when the game has a profile, finds its saved values again.
    pub fn attach(&mut self, pid: u32) -> Result<String, String> {
        let reply = self.helper.call(&format!("attach {pid}"));
        for l in &reply {
            self.say(l);
        }
        if let Some(e) = first_error(&reply) {
            return Err(e);
        }
        let exe = reply.iter().find_map(|l| l.strip_prefix("exe: ")).ok_or("no program name")?.to_owned();
        self.game = Some(Game { exe: exe.clone(), entries: Vec::new() });
        if !read_profile(&exe).is_empty() {
            self.say(&format!("found saved values for {exe}, restoring:"));
            self.restore()?;
        }
        Ok(exe)
    }

    // --- Saved values

    fn peek(&mut self, addrs: &[u64]) -> Vec<Option<i64>> {
        let arg: Vec<String> = addrs.iter().map(|a| format!("{a:x}")).collect();
        let values = parse_values(&self.helper.call(&format!("peek {}", arg.join(" "))));
        addrs
            .iter()
            .map(|a| values.iter().find(|(b, _)| b == a).and_then(|(_, v)| *v))
            .collect()
    }

    /// The "survive restarts" button: finds the code that accesses the current
    /// value and saves it under `name`, so the value can be found again next time.
    pub fn save(&mut self, name: &str) -> Result<PathBuf, String> {
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err("the name must be one word".into());
        }
        self.game()?;
        let listed = self.helper.call("list");
        let [(addr, _)] = parse_values(&listed)[..] else {
            return Err("narrow down to exactly one address first".into());
        };
        let reply = self.helper.call(&format!("sites {addr:x}"));
        for l in reply.iter().filter(|l| !l.starts_with("site ")) {
            self.say(l);
        }
        let sites: Vec<String> = reply.iter().filter_map(|l| l.strip_prefix("site ")).map(str::to_owned).collect();
        if sites.is_empty() {
            return Err("no usable code found; keep the game running (not paused) and try again".into());
        }
        let count = sites.len();
        let game = self.game()?;
        let mut entries = read_profile(&game.exe);
        entries.retain(|e| e.name != name);
        entries.push(Entry { name: name.to_owned(), sites, limit: None });
        let path = write_profile(&game.exe, &entries)?;
        game.entries.retain(|(n, _)| n != name);
        game.entries.push((name.to_owned(), addr));
        self.say(&format!("saved {name} ({count} code patterns) to {}", path.display()));
        Ok(path)
    }

    /// Hands a saved limit to the helper, which enforces it in the background
    /// and finds the value again through its code patterns if the game moves it.
    fn apply_limit(&mut self, entry: &Entry) -> Result<(), String> {
        let limit = entry.limit.as_deref().ok_or("no limit set")?;
        let game = self.game()?;
        let (_, addr) = *game.entries.iter().find(|(n, _)| *n == entry.name).ok_or("value not found in this run")?;
        if entry.sites.is_empty() {
            return Err("no saved code pattern".into());
        }
        let sites: Vec<String> =
            entry.sites.iter().map(|s| s.split_whitespace().collect::<Vec<_>>().join(":")).collect();
        let reply = self.helper.call(&format!("limit {} {addr:x} {limit} {}", entry.name, sites.join(" ")));
        first_error(&reply).map_or(Ok(()), Err)
    }

    /// Limits the helper enforces: name, current address, and the rest of its line.
    fn limits(&mut self) -> Vec<(String, u64, String)> {
        self.helper
            .call("limits")
            .iter()
            .filter_map(|l| {
                let mut f = l.splitn(3, ' ');
                let name = f.next()?.to_owned();
                let addr = u64::from_str_radix(f.next()?.trim_start_matches("0x"), 16).ok()?;
                Some((name, addr, f.next()?.to_owned()))
            })
            .collect()
    }

    /// The helper may have found a limited value at a new address; use that one.
    fn sync_addresses(&mut self) {
        let limits = self.limits();
        if let Some(game) = self.game.as_mut() {
            for (name, addr, _) in limits {
                if let Some(e) = game.entries.iter_mut().find(|(n, _)| *n == name) {
                    e.1 = addr;
                }
            }
        }
    }

    /// Keeps a saved value within a range; `None` on both sides turns the limit off.
    pub fn limit(&mut self, name: &str, min: Option<i64>, max: Option<i64>) -> Result<(), String> {
        let exe = self.game()?.exe.clone();
        let mut entries = read_profile(&exe);
        let i = entries.iter().position(|e| e.name == name).ok_or(format!("no saved value called {name}"))?;
        entries[i].limit = (min.is_some() || max.is_some()).then(|| range_text(min, max));
        if entries[i].limit.is_some() {
            self.apply_limit(&entries[i])?;
            self.say(&format!("{name} is kept {} (written only when the game goes past it)", limit_text(min, max)));
        } else {
            self.helper.call(&format!("unlimit {name}"));
            self.say(&format!("{name} is no longer limited"));
        }
        write_profile(&exe, &entries)?;
        Ok(())
    }

    /// Finds every saved value of this game again, and re-applies saved limits.
    pub fn restore(&mut self) -> Result<(), String> {
        let exe = self.game()?.exe.clone();
        let entries = read_profile(&exe);
        if entries.is_empty() {
            return Err(format!("nothing saved for {exe}"));
        }
        let t = Instant::now();
        for entry in &entries {
            let name = &entry.name;
            let mut resolved = None;
            for site in &entry.sites {
                let reply = self.helper.call(&format!("resolve {site}"));
                if let Some((addr, Some(v))) = parse_values(&reply).first() {
                    resolved = Some((*addr, *v));
                    break;
                }
                self.say(&format!("{name}: {}", reply.join(" ")));
            }
            match resolved {
                Some((addr, v)) => {
                    self.say(&format!("{name} = {v} (at 0x{addr:x})"));
                    let game = self.game()?;
                    game.entries.retain(|(n, _)| n != name);
                    game.entries.push((name.clone(), addr));
                    if let Some(l) = &entry.limit {
                        let (min, max) = parse_range(l);
                        match self.apply_limit(entry) {
                            Ok(()) => self.say(&format!("{name} is kept {}", limit_text(min, max))),
                            Err(e) => self.say(&format!("{name}: limit not applied: {e}")),
                        }
                    }
                }
                None => self.say(&format!("{name}: not found yet, try restore again once the game has used it")),
            }
        }
        self.say(&format!("restored in {} ms", t.elapsed().as_millis()));
        Ok(())
    }

    pub fn values(&mut self) -> Result<Vec<ValueRow>, String> {
        let exe = self.game()?.exe.clone();
        self.sync_addresses();
        let entries: Vec<(String, u64)> = self.game()?.entries.clone();
        let addrs: Vec<u64> = entries.iter().map(|(_, a)| *a).collect();
        let values = self.peek(&addrs);
        let limits = self.limits();
        let saved = read_profile(&exe);
        Ok(entries
            .into_iter()
            .zip(values)
            .map(|((name, addr), value)| {
                let (min, max) = saved
                    .iter()
                    .find(|e| e.name == name)
                    .and_then(|e| e.limit.as_deref())
                    .map_or((None, None), parse_range);
                let limit_state = limits
                    .iter()
                    .find(|(n, _, _)| *n == name)
                    .and_then(|(_, _, rest)| rest.split_once(" fixed ").map(|(_, s)| format!("fixed {s}")));
                ValueRow { name, addr, value, min, max, limit_state }
            })
            .collect())
    }

    pub fn set(&mut self, name: &str, value: i64) -> Result<(), String> {
        self.sync_addresses();
        let (_, addr) = *self
            .game()?
            .entries
            .iter()
            .find(|(n, _)| n == name)
            .ok_or(format!("no saved value called {name}"))?;
        let reply = self.helper.call(&format!("write {addr:x} {value}"));
        for l in &reply {
            self.say(l);
        }
        first_error(&reply).map_or(Ok(()), Err)
    }

    // --- Reading numbers off the game window

    /// Starts window capture; the first time, the desktop asks which window.
    pub fn start_capture(&mut self) -> Result<(), String> {
        if self.capture.is_none() {
            self.capture = Some(WindowCapture::start()?);
        }
        Ok(())
    }

    pub fn frame(&mut self) -> Result<PathBuf, String> {
        self.start_capture()?;
        let out = cache_dir().join("frame.png");
        self.capture.as_ref().unwrap().grab(&out)?;
        Ok(out)
    }

    /// Captures a frame and finds every number in it.
    pub fn numbers(&mut self) -> Result<(PathBuf, Vec<Word>), String> {
        let frame = self.frame()?;
        self.words = ocr::numbers(&frame)?;
        Ok((frame, self.words.clone()))
    }

    pub fn word_area(&self, i: usize) -> Option<Rect> {
        self.words.get(i).map(|w| ocr::watch_area(w.rect))
    }

    pub fn set_area(&mut self, area: Rect) {
        self.area = Some(area);
    }

    /// Reads the number inside the watched area of a fresh frame.
    pub fn read(&mut self) -> Result<Option<i64>, String> {
        let area = self.area.ok_or("no area picked yet")?;
        let frame = self.frame()?;
        ocr::read_number(&frame, area, &cache_dir().join("area.png"))
    }

    /// Current scan candidates (at most 20 are listed by the helper).
    pub fn candidates(&mut self) -> Vec<(u64, Option<i64>)> {
        parse_values(&self.helper.call("list"))
    }

    /// Tells the real value apart from copies of it: writes a test value to one
    /// candidate at a time and checks whether it sticks, whether the other
    /// candidates follow it, and whether the screen shows it. Test writes are undone.
    pub fn probe(&mut self) -> Result<Option<u64>, String> {
        let listed = self.helper.call("list");
        if listed.iter().any(|l| l.starts_with("...")) {
            return Err("too many candidates to probe; narrow down first".into());
        }
        let addrs: Vec<u64> = parse_values(&listed).into_iter().map(|(a, _)| a).collect();
        if addrs.is_empty() {
            return Err("no candidates to probe".into());
        }
        for (i, &addr) in addrs.iter().enumerate() {
            let Some(orig) = self.peek(&[addr])[0] else { continue };
            let test = orig + 10;
            self.helper.call(&format!("write {addr:x} {test}"));
            std::thread::sleep(Duration::from_millis(1500));
            let after = self.peek(&addrs);
            let stuck = after[i] == Some(test);
            let followers = (0..addrs.len()).filter(|&j| j != i && after[j] == Some(test)).count();
            let screen = if self.area.is_some() { self.read().ok().flatten() } else { None };
            let shown = screen == Some(test);
            self.say(&format!(
                "0x{addr:012x}: wrote {test}: {}, {followers} of {} others followed, screen shows {}",
                if stuck { "kept" } else { "game overwrote it" },
                addrs.len() - 1,
                screen.map_or("?".into(), |n| n.to_string()),
            ));
            if stuck {
                self.helper.call(&format!("write {addr:x} {orig}"));
            }
            if stuck && (followers > 0 || shown) {
                self.helper.call(&format!("keep {addr:x}"));
                self.say(&format!("real value at 0x{addr:012x} (test write undone)"));
                return Ok(Some(addr));
            }
        }
        self.say("no candidate behaved like the real value (is the game paused?)");
        Ok(None)
    }

    fn read_stable(&mut self) -> Result<Option<i64>, String> {
        let a = self.read()?;
        let b = self.read()?;
        Ok(if a == b { a } else { None })
    }

    /// The automated scan loop: read the number off the window, scan for it, and
    /// keep narrowing down whenever it changes on screen. Stops early when
    /// `cancel` is set.
    pub fn auto(&mut self, limit: Duration) -> Result<AutoResult, String> {
        self.cancel.store(false, Ordering::Relaxed);
        let cancelled = |c: &Arc<AtomicBool>| c.load(Ordering::Relaxed);
        let start = Instant::now();
        let first = loop {
            if let Some(n) = self.read_stable()? {
                break n;
            }
            if start.elapsed() > limit || cancelled(&self.cancel) {
                return Err("could not read the number".into());
            }
        };
        let reply = self.helper.call(&format!("scan {first}"));
        self.say(&format!("screen shows {first}: {}", reply.join(" ")));
        let Some(mut count) = match_count(&reply) else {
            return Err(first_error(&reply).unwrap_or("scan failed".into()));
        };
        let mut last = first;
        let mut unchanged_rounds = 0;
        while count > 1 && start.elapsed() < limit && !cancelled(&self.cancel) {
            std::thread::sleep(Duration::from_millis(300));
            let Some(now) = self.read_stable()? else { continue };
            if now == last {
                continue;
            }
            let reply = self.helper.call(&format!("next {now}"));
            let new_count = match_count(&reply).unwrap_or(0);
            self.say(&format!("screen shows {now}: {}", reply.join(" ")));
            if new_count == 0 {
                let reply = self.helper.call(&format!("scan {now}"));
                self.say(&format!("lost it (the screen lags memory or OCR misread), rescanning: {}", reply.join(" ")));
                count = match_count(&reply).unwrap_or(0);
                unchanged_rounds = 0;
            } else {
                unchanged_rounds = if new_count == count { unchanged_rounds + 1 } else { 0 };
                count = new_count;
            }
            last = now;
            if unchanged_rounds >= 3 {
                self.say(&format!("{count} addresses keep following the value (likely the value plus copies of it)"));
                break;
            }
        }
        if cancelled(&self.cancel) {
            self.say("stopped");
        } else if count > 1 && start.elapsed() >= limit {
            self.say("time limit reached; the value never changed enough to narrow down to one address");
        }
        for l in self.helper.call("list") {
            self.say(&l);
        }
        if count == 1 {
            return Ok(AutoResult::Found(self.candidates()[0].0));
        }
        if (2..=20).contains(&count) && !cancelled(&self.cancel) {
            self.say("checking which one is the real value:");
            if let Some(addr) = self.probe()? {
                return Ok(AutoResult::Found(addr));
            }
        }
        Ok(AutoResult::Several(count))
    }
}
