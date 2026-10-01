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
use crate::font::{self, Font};
use crate::ocr::{self, Rect, Word};

fn flatpak_info(key: &str) -> Option<String> {
    let info = fs::read_to_string("/.flatpak-info").ok()?;
    info.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=')).map(str::to_owned)
}

fn flatpak_app_path() -> Option<String> {
    flatpak_info("app-path")
}

const APP_ID: &str = "io.github.Petru101.Ferret";

/// Whether a newer build of Ferret was installed since this one started: a running app keeps
/// its build until it's restarted, and an old build drops what it doesn't know when it saves.
/// The running build is `<deploy dir>/<commit>/files`; `<deploy dir>/active` is the installed one.
pub fn newer_install() -> bool {
    let (Some(path), Some(commit)) = (flatpak_app_path(), flatpak_info("app-commit")) else { return false };
    let Some(active) = Path::new(&path).parent().and_then(Path::parent).map(|d| d.join("active")) else { return false };
    let Ok(out) = Command::new("flatpak-spawn").args(["--host", "readlink"]).arg(&active).output() else { return false };
    let installed = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    out.status.success() && !installed.is_empty() && installed != commit
}

/// Starts Ferret again on the host once this instance has left the session bus (a second
/// instance would only hand over to this one); call right before quitting. Detached on the
/// host, so flatpak-spawn returns at once instead of keeping this sandbox alive meanwhile.
pub fn restart_after_quit() -> Result<(), String> {
    let script = format!(
        "for i in $(seq 50); do gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
         --method org.freedesktop.DBus.NameHasOwner {APP_ID} | grep -q true || break; sleep 0.2; done; exec flatpak run {APP_ID}"
    );
    Command::new("flatpak-spawn")
        .args(["--host", "setsid", "-f", "sh", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
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

/// How the game stores a value.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    I32,
    F32,
    F64,
    /// XOR-encoded 4-byte integer (the next word is the key).
    Xor,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::I32 => "i32",
            Kind::F32 => "f32",
            Kind::F64 => "f64",
            Kind::Xor => "xor",
        }
    }

    fn parse(s: &str) -> Option<Kind> {
        [Kind::I32, Kind::F32, Kind::F64, Kind::Xor].into_iter().find(|k| k.name() == s)
    }

    fn with_article(self) -> String {
        let d = self.describe();
        format!("{} {d}", if d.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" })
    }

    pub fn describe(self) -> &'static str {
        match self {
            Kind::I32 => "whole number",
            Kind::F32 => "float",
            Kind::F64 => "double",
            Kind::Xor => "encoded whole number",
        }
    }
}

/// Where a value is and how it is stored; shown the way the helper takes it: "<hex>:<type>".
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Loc {
    pub addr: u64,
    pub kind: Kind,
}

impl std::fmt::Display for Loc {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:x}:{}", self.addr, self.kind.name())
    }
}

fn parse_loc(a: &str) -> Option<Loc> {
    let (addr, kind) = a.split_once(':').map_or((a, Some(Kind::I32)), |(a, k)| (a, Kind::parse(k)));
    Some(Loc { addr: u64::from_str_radix(addr.trim_start_matches("0x"), 16).ok()?, kind: kind? })
}

/// Parses helper lines of the form "0x00003795366c:f32 = 96.5". Values are shown as whole
/// numbers, cut off the way games usually display them.
fn parse_values(lines: &[String]) -> Vec<(Loc, Option<i64>)> {
    lines
        .iter()
        .filter_map(|l| {
            let (a, v) = l.split_once(" = ")?;
            let v = v.trim().parse::<f64>().ok().filter(|v| v.is_finite()).map(|v| (v + 1e-6).floor() as i64);
            Some((parse_loc(a)?, v))
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

/// The game's learned digit shapes, next to its profile.
fn digits_path(exe: &str) -> PathBuf {
    profile_path(exe).with_extension("digits")
}

struct Entry {
    name: String,
    kind: Kind,
    sites: Vec<String>,
    /// Pointer paths that led to the value in more than one run of the game,
    /// "<module>+<offset> <offset>...".
    paths: Vec<String>,
    /// Pointer paths from a scan, best first, not confirmed by another run yet: most of them
    /// go through things that change when the game restarts. Saving the value again in a
    /// later run keeps the ones that still lead to it as `paths`.
    candidates: Vec<String>,
    /// The game process the candidates were found in.
    run: Option<u32>,
    /// "<min|-> <max|->" when the value is kept within a range.
    limit: Option<String>,
    /// Lines this build doesn't understand (from a newer one), saved back unchanged.
    other: Vec<String>,
}

/// Profile format: "entry <name>" followed by an optional "type f32|f64|xor" line (i32 when
/// missing), its "site ...", "path ..." and "candidate ..." lines, "run <pid>" (where the
/// candidates came from) and an optional "limit <min|-> <max|->" line. Other lines are kept
/// with their entry, so a build older than the profile doesn't drop what it doesn't know.
fn read_profile(exe: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in fs::read_to_string(profile_path(exe)).unwrap_or_default().lines() {
        if let Some(name) = line.strip_prefix("entry ") {
            entries.push(Entry {
                name: name.trim().to_owned(),
                kind: Kind::I32,
                sites: Vec::new(),
                paths: Vec::new(),
                candidates: Vec::new(),
                run: None,
                limit: None,
                other: Vec::new(),
            });
        } else if let (Some(kind), Some(e)) = (line.strip_prefix("type ").and_then(|k| Kind::parse(k.trim())), entries.last_mut()) {
            e.kind = kind;
        } else if let (Some(site), Some(e)) = (line.strip_prefix("site "), entries.last_mut()) {
            e.sites.push(site.trim().to_owned());
        } else if let (Some(path), Some(e)) = (line.strip_prefix("path "), entries.last_mut()) {
            e.paths.push(path.trim().to_owned());
        } else if let (Some(path), Some(e)) = (line.strip_prefix("candidate "), entries.last_mut()) {
            e.candidates.push(path.trim().to_owned());
        } else if let (Some(pid), Some(e)) = (line.strip_prefix("run "), entries.last_mut()) {
            e.run = pid.trim().parse().ok();
        } else if let (Some(limit), Some(e)) = (line.strip_prefix("limit "), entries.last_mut()) {
            e.limit = Some(limit.trim().to_owned());
        } else if let (false, Some(e)) = (line.trim().is_empty(), entries.last_mut()) {
            e.other.push(line.to_owned());
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
        if e.kind != Kind::I32 {
            text.push_str(&format!("type {}\n", e.kind.name()));
        }
        for s in &e.sites {
            text.push_str(&format!("site {s}\n"));
        }
        for p in &e.paths {
            text.push_str(&format!("path {p}\n"));
        }
        if let Some(pid) = e.run {
            text.push_str(&format!("run {pid}\n"));
        }
        for p in &e.candidates {
            text.push_str(&format!("candidate {p}\n"));
        }
        if let Some(l) = &e.limit {
            text.push_str(&format!("limit {l}\n"));
        }
        for l in &e.other {
            text.push_str(&format!("{l}\n"));
        }
    }
    fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(path)
}

/// How many leading parts (start, then offsets) a path has in common with the closest of `known`.
fn shared_start(path: &str, known: &[&String]) -> usize {
    let common = |k: &String| path.split_whitespace().zip(k.split_whitespace()).take_while(|(a, b)| a == b).count();
    known.iter().map(|k| common(k)).max().unwrap_or(0)
}

/// A game's values mostly hang off the same few structures (in GameMaker games, its variable
/// list): unconfirmed paths starting the way another value's confirmed ones do (the same start
/// and at least two offsets) are much more likely to be the real ones.
const SHARED_START: usize = 3;

impl Entry {
    /// Confirmed paths of the game's other values.
    fn known<'a>(&self, all: &'a [Entry]) -> Vec<&'a String> {
        all.iter().filter(|e| e.name != self.name).flat_map(|e| &e.paths).collect()
    }

    /// The pointer paths to follow, as the helper takes them: the confirmed ones, else the
    /// candidates that start like other values' confirmed paths, else all candidates.
    fn followed(&self, all: &[Entry]) -> Vec<String> {
        if !self.paths.is_empty() {
            return self.paths.iter().map(|p| path_arg(p)).collect();
        }
        let known = self.known(all);
        let likely: Vec<&String> = self.candidates.iter().filter(|p| shared_start(p, &known) >= SHARED_START).collect();
        let paths = if likely.is_empty() { self.candidates.iter().collect() } else { likely };
        paths.into_iter().map(|p| path_arg(p)).collect()
    }

    /// Saved by a newer build with a value type this one doesn't know: reading or writing it
    /// as an i32 would be wrong.
    fn foreign_type(&self) -> Option<&str> {
        self.other.iter().find_map(|l| l.strip_prefix("type ")).map(str::trim)
    }

    fn unconfirmed(&self) -> bool {
        self.paths.is_empty() && !self.candidates.is_empty()
    }
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
    pub kind: Kind,
    pub value: Option<i64>,
    pub min: Option<i64>,
    pub max: Option<i64>,
    /// "fixed N times, restored M times, active" while a limit is enforced.
    pub limit_state: Option<String>,
    /// Found through pointer paths no other run of the game has confirmed yet.
    pub unconfirmed: bool,
    /// Why Ferret won't write it right now: its pointer paths don't agree on where it is.
    pub doubtful: Option<String>,
}

pub enum AutoResult {
    /// One address left: the value.
    Found(Loc),
    /// Several addresses still follow the value and none could be confirmed.
    Several(usize),
}

struct Game {
    pid: u32,
    exe: String,
    entries: Vec<(String, Loc)>,
    /// Values found through pointer paths (as the helper takes them), followed again on every
    /// look: the game may have moved them.
    paths: Vec<(String, Vec<String>)>,
    /// How those paths agreed when last followed.
    votes: Vec<(String, Votes)>,
}

/// How a value's pointer paths agreed when last followed (the helper's "votes" line).
#[derive(Clone, Copy)]
struct Votes {
    agree: usize,
    /// Paths leading to the most common other address.
    elsewhere: usize,
    total: usize,
    /// Clear enough to write there (see helper/src/pointers.rs `Vote::clear`).
    clear: bool,
}

impl Votes {
    fn parse(reply: &[String]) -> Option<Votes> {
        let line = reply.iter().find_map(|l| l.strip_prefix("votes "))?;
        let f: Vec<&str> = line.split_whitespace().collect();
        let [agree, elsewhere, total, clear] = f[..] else { return None };
        Some(Votes { agree: agree.parse().ok()?, elsewhere: elsewhere.parse().ok()?, total: total.parse().ok()?, clear: clear == "clear" })
    }

    fn doubt(&self) -> String {
        if self.elsewhere > 0 {
            format!("its pointer paths disagree ({} of {} lead to one place, {} to another)", self.agree, self.total, self.elsewhere)
        } else {
            format!("only {} of its {} pointer paths leads anywhere", self.agree, self.total)
        }
    }
}

/// A search that could only end on a display copy.
const COPY_ONLY: &str = "only found a copy the game redraws its display from: it keeps the value itself in a form \
    Ferret can't search for yet (for example a 2-byte number or an encoded one)";

/// Most pointer paths kept from a scan. The real one can rank far down (Forager's gems: 590th
/// of 81235), and only a later run tells which it is.
const MAX_CANDIDATES: usize = 3000;

/// A profile path line as the helper takes it: "Forager.exe+177a64c 44 2c" -> "Forager.exe+177a64c,44,2c".
fn path_arg(p: &str) -> String {
    p.split_whitespace().collect::<Vec<_>>().join(",")
}

pub struct Core {
    helper: Helper,
    capture: Option<WindowCapture>,
    words: Vec<Word>,
    area: Option<Rect>,
    game: Option<Game>,
    /// The attached game's digits, as learned so far.
    font: Font,
    /// The search in progress (Start or typed numbers, which continue each other): matches
    /// left, and how many typed numbers in a row left the count unchanged. None = the next
    /// number starts a new scan.
    search: Option<(usize, usize)>,
    log: Box<dyn FnMut(&str) + Send>,
    /// Set to stop a running `auto`.
    pub cancel: Arc<AtomicBool>,
    /// The last read `read_stable` ignored, to log it once.
    ignored: Option<i64>,
    /// Numbers the learned digits found in the last frame read, to follow the watched number
    /// when the layout shifts.
    seen: Vec<Word>,
}

impl Core {
    pub fn new(log: Box<dyn FnMut(&str) + Send>) -> Result<Self, String> {
        fs::write(cache_dir().join("ferret.log"), "").ok();
        Ok(Self {
            helper: Helper::start()?,
            capture: None,
            words: Vec::new(),
            area: None,
            game: None,
            font: Font::default(),
            search: None,
            log,
            cancel: Arc::new(AtomicBool::new(false)),
            ignored: None,
            seen: Vec::new(),
        })
    }

    fn say(&mut self, msg: &str) {
        (self.log)(msg);
        // Also kept on disk, for looking into problems after the fact.
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(cache_dir().join("ferret.log")) {
            let _ = writeln!(f, "{msg}");
        }
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
        self.game = Some(Game { pid, exe: exe.clone(), entries: Vec::new(), paths: Vec::new(), votes: Vec::new() });
        self.search = None;
        self.font = Font::load(&digits_path(&exe));
        if !self.font.is_empty() {
            self.say(&format!("knows how {exe} draws the digits {}", self.font.known()));
        }
        if !read_profile(&exe).is_empty() {
            self.say(&format!("found saved values for {exe}, restoring:"));
            self.restore()?;
        }
        Ok(exe)
    }

    // --- Saved values

    fn peek(&mut self, addrs: &[Loc]) -> Vec<Option<i64>> {
        let arg: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
        let values = parse_values(&self.helper.call(&format!("peek {}", arg.join(" "))));
        addrs
            .iter()
            .map(|a| values.iter().find(|(b, _)| b == a).and_then(|(_, v)| *v))
            .collect()
    }

    /// The "survive restarts" button: finds the code that accesses the current
    /// value and saves it under `name`, so the value can be found again next time.
    /// Returns false when that still needs confirming in a later run (unconfirmed pointer paths).
    pub fn save(&mut self, name: &str) -> Result<bool, String> {
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err("the name must be one word".into());
        }
        self.game()?;
        let listed = self.helper.call("list");
        let [(loc, _)] = parse_values(&listed)[..] else {
            return Err("narrow down to exactly one address first".into());
        };
        let reply = self.helper.call(&format!("sites {loc}"));
        for l in reply.iter().filter(|l| !l.starts_with("site ")) {
            self.say(l);
        }
        let sites: Vec<String> = reply.iter().filter_map(|l| l.strip_prefix("site ")).map(str::to_owned).collect();
        let (mut paths, mut candidates) = (Vec::new(), Vec::new());
        let pid = self.game()?.pid;
        if sites.is_empty() {
            let accessed = reply.iter().find_map(|l| l.strip_suffix(" instructions accessed it")).and_then(|n| n.parse::<usize>().ok());
            let shared = reply.iter().any(|l| l.contains(": shared code,"));
            let why = match accessed {
                Some(0) => "nothing in the game touched the value while Ferret watched",
                _ if shared => "the game only reads this value through code it shares with other values (GameMaker games do this)",
                _ => "the game uses the value in a way Ferret can't save by code",
            };
            self.say(&format!("{why}; looking for pointers that lead to it instead"));
            let exe = self.game()?.exe.clone();
            if let Some(saved) = read_profile(&exe).into_iter().find(|e| e.name == name) {
                // Candidates from this same run all still lead here: that proves nothing.
                let mut old = saved.paths;
                if saved.run != Some(pid) {
                    old.extend(saved.candidates);
                }
                paths = self.proven_paths(loc, &old);
            }
            if paths.is_empty() {
                candidates = self.pointer_paths(loc).map_err(|e| format!("{why}, and {e}"))?;
            }
        }
        let how = if !sites.is_empty() {
            format!("{} code patterns", sites.len())
        } else if !paths.is_empty() {
            format!("{} pointer paths that held up since the last run", paths.len())
        } else {
            format!(
                "{} pointer paths; after the next restart, if it's wrong, find it again and save it as {name}: that picks the right path",
                candidates.len()
            )
        };
        let game = self.game()?;
        let mut entries = read_profile(&game.exe);
        entries.retain(|e| e.name != name);
        let mut entry = Entry { name: name.to_owned(), kind: loc.kind, sites, paths, candidates, run: Some(pid), limit: None, other: Vec::new() };
        let known = entry.known(&entries);
        entry.candidates.sort_by_key(|p| std::cmp::Reverse(shared_start(p, &known)));
        let likely = entry.followed(&entries).len();
        let (followed, confirmed) = (entry.followed(&entries), !entry.unconfirmed());
        entries.push(entry);
        let path = write_profile(&game.exe, &entries)?;
        game.entries.retain(|(n, _)| n != name);
        game.entries.push((name.to_owned(), loc));
        game.paths.retain(|(n, _)| n != name);
        game.votes.retain(|(n, _)| n != name);
        if !followed.is_empty() {
            game.paths.push((name.to_owned(), followed));
        }
        if !confirmed && likely < entries.last().map_or(0, |e| e.candidates.len()) {
            self.say(&format!("{likely} of them start like other saved values' confirmed paths: Ferret follows those"));
        }
        self.say(&format!("saved {name} ({how}) to {}", path.display()));
        Ok(confirmed)
    }

    /// Pointer paths from the game's static memory to the value (see helper/src/pointers.rs),
    /// in profile form, best first. Paths through memory that changes within a few seconds
    /// would not survive a restart either, so they are dropped.
    fn pointer_paths(&mut self, loc: Loc) -> Result<Vec<String>, String> {
        let reply = self.helper.call(&format!("ptrscan {loc} 5 1000 {MAX_CANDIDATES}"));
        if let Some(e) = first_error(&reply) {
            return Err(format!("the pointer scan failed: {e}"));
        }
        for l in reply.iter().filter(|l| !l.starts_with("path ")) {
            self.say(l);
        }
        let mut paths: Vec<String> = reply.iter().filter_map(|l| l.strip_prefix("path ")).map(str::to_owned).collect();
        if paths.is_empty() {
            return Err("no pointer from the game's own memory leads to it".into());
        }
        std::thread::sleep(Duration::from_secs(3));
        let (ends, _) = self.follow(loc.kind, &paths);
        let before = paths.len();
        paths = paths.into_iter().zip(ends).filter(|(_, e)| *e == Some(loc.addr)).map(|(p, _)| p).collect();
        if paths.is_empty() {
            return Err("every pointer path to it changed within seconds".into());
        }
        self.say(&format!("{} of {before} pointer paths still lead to it after 3 s", paths.len()));
        Ok(paths.iter().map(|p| p.replace(',', " ")).collect())
    }

    /// Saving a value again after it moved (a restart, a new level) keeps the saved pointer
    /// paths that lead to where it is now: they held up through the move, which fresh ones
    /// haven't yet.
    fn proven_paths(&mut self, loc: Loc, saved: &[String]) -> Vec<String> {
        if saved.is_empty() {
            return Vec::new();
        }
        let args: Vec<String> = saved.iter().map(|p| path_arg(p)).collect();
        let (ends, _) = self.follow(loc.kind, &args);
        let kept: Vec<String> = saved.iter().zip(ends).filter(|(_, e)| *e == Some(loc.addr)).map(|(p, _)| p.clone()).collect();
        let then = if kept.is_empty() { ", scanning again" } else { ", keeping those" };
        self.say(&format!("{} of {} saved pointer paths lead to it here{then}", kept.len(), saved.len()));
        kept
    }

    /// Where each pointer path (helper form) leads now, and the address most of them agree on.
    fn follow(&mut self, kind: Kind, paths: &[String]) -> (Vec<Option<u64>>, Option<(Loc, Option<i64>)>) {
        let (ends, best, _) = self.follow_votes(kind, paths);
        (ends, best)
    }

    /// `follow`, plus how strongly the paths agree.
    fn follow_votes(&mut self, kind: Kind, paths: &[String]) -> (Vec<Option<u64>>, Option<(Loc, Option<i64>)>, Option<Votes>) {
        let reply = self.helper.call(&format!("follow {} {}", kind.name(), paths.join(" ")));
        let ends = (0..paths.len())
            .map(|i| {
                let line = reply.iter().find_map(|l| l.strip_prefix(&format!("{i}: ")))?;
                Some(parse_values(&[line.to_owned()]).first()?.0.addr)
            })
            .collect();
        let best = reply.iter().find_map(|l| l.strip_prefix("best ")).and_then(|l| parse_values(&[l.to_owned()]).first().copied());
        (ends, best, Votes::parse(&reply))
    }

    fn set_votes(&mut self, name: &str, votes: Option<Votes>) {
        if let Some(game) = self.game.as_mut() {
            game.votes.retain(|(n, _)| n != name);
            game.votes.extend(votes.map(|v| (name.to_owned(), v)));
        }
    }

    /// The latest vote of a value found through pointer paths, when it wasn't clear.
    fn doubtful(&self, name: &str) -> Option<Votes> {
        let game = self.game.as_ref()?;
        game.paths.iter().any(|(n, _)| n == name).then_some(())?;
        game.votes.iter().find(|(n, _)| n == name).map(|(_, v)| *v).filter(|v| !v.clear)
    }

    /// Follows the pointer paths of values found through them again: the game may have moved them.
    fn refresh_paths(&mut self) {
        let Some(game) = self.game.as_ref() else { return };
        let followed = game.paths.clone();
        for (name, paths) in followed {
            let Some(kind) = self.game.as_ref().and_then(|g| g.entries.iter().find(|(n, _)| *n == name)).map(|(_, l)| l.kind) else {
                continue;
            };
            let (_, best, votes) = self.follow_votes(kind, &paths);
            self.set_votes(&name, votes);
            if let Some(e) = self.game.as_mut().and_then(|g| g.entries.iter_mut().find(|(n, _)| *n == name)) {
                e.1.addr = best.map_or(0, |(loc, _)| loc.addr);
            }
        }
    }

    /// Hands a saved limit to the helper, which enforces it in the background
    /// and finds the value again through its code patterns if the game moves it.
    fn apply_limit(&mut self, entry: &Entry) -> Result<(), String> {
        let limit = entry.limit.as_deref().ok_or("no limit set")?;
        let game = self.game()?;
        let (_, loc) = *game.entries.iter().find(|(n, _)| *n == entry.name).ok_or("value not found in this run")?;
        let followed = entry.followed(&read_profile(&game.exe));
        if entry.sites.is_empty() && followed.is_empty() {
            return Err("no saved code pattern or pointer path".into());
        }
        let sites: Vec<String> = entry
            .sites
            .iter()
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(":"))
            .chain(followed)
            .collect();
        let reply = self.helper.call(&format!("limit {} {loc} {limit} {}", entry.name, sites.join(" ")));
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
                    e.1.addr = addr;
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
            if let Some(kind) = entry.foreign_type() {
                self.say(&format!("{name}: saved by a newer Ferret (value type {kind}), which this one can't read; restart Ferret"));
                continue;
            }
            let mut resolved = None;
            for site in &entry.sites {
                let reply = self.helper.call(&format!("resolve {site} {}", entry.kind.name()));
                if let Some((loc, Some(v))) = parse_values(&reply).first() {
                    resolved = Some((*loc, *v));
                    break;
                }
                self.say(&format!("{name}: {}", reply.join(" ")));
            }
            let paths = entry.followed(&entries);
            if resolved.is_none() && !paths.is_empty() {
                let (ends, best, votes) = self.follow_votes(entry.kind, &paths);
                self.set_votes(name, votes);
                // Keep following them: before a save is loaded they may lead nowhere yet.
                let game = self.game()?;
                game.paths.retain(|(n, _)| n != name);
                game.paths.push((name.clone(), paths.clone()));
                match best {
                    Some((loc, Some(v))) => {
                        let agree = ends.iter().filter(|e| **e == Some(loc.addr)).count();
                        self.say(&format!("{name}: {agree} of {} pointer paths lead to it", paths.len()));
                        if let Some(v) = votes.filter(|v| !v.clear) {
                            self.say(&format!("{name}: {}: Ferret won't write it until they agree", v.doubt()));
                        }
                        if entry.unconfirmed() {
                            if paths.len() < entry.candidates.len() {
                                self.say(&format!("{name}: following the {} unconfirmed paths that start like other values' confirmed ones", paths.len()));
                            }
                            self.say(&format!("{name}: its pointer paths aren't confirmed yet; if the number is wrong, find it again and save it as {name}"));
                        }
                        resolved = Some((loc, v));
                    }
                    _ => {
                        self.say(&format!("{name}: its pointer paths lead nowhere yet (load a save?), Ferret keeps following them"));
                        let game = self.game()?;
                        game.entries.retain(|(n, _)| n != name);
                        game.entries.push((name.clone(), Loc { addr: 0, kind: entry.kind }));
                        if entry.limit.is_some() {
                            if let Err(e) = self.apply_limit(entry) {
                                self.say(&format!("{name}: limit not applied: {e}"));
                            }
                        }
                        continue;
                    }
                }
            }
            match resolved {
                Some((loc, v)) => {
                    self.say(&format!("{name} = {v} (at 0x{:x}, {})", loc.addr, loc.kind.describe()));
                    let game = self.game()?;
                    game.entries.retain(|(n, _)| n != name);
                    game.entries.push((name.clone(), loc));
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
        self.refresh_paths();
        self.sync_addresses();
        let entries: Vec<(String, Loc)> = self.game()?.entries.clone();
        let addrs: Vec<Loc> = entries.iter().map(|(_, a)| *a).collect();
        let values = self.peek(&addrs);
        let limits = self.limits();
        let saved = read_profile(&exe);
        Ok(entries
            .into_iter()
            .zip(values)
            .map(|((name, loc), value)| {
                let (min, max) = saved
                    .iter()
                    .find(|e| e.name == name)
                    .and_then(|e| e.limit.as_deref())
                    .map_or((None, None), parse_range);
                let limit_state = limits
                    .iter()
                    .find(|(n, _, _)| *n == name)
                    .and_then(|(_, _, rest)| rest.split_once(" fixed ").map(|(_, s)| format!("fixed {s}")));
                let unconfirmed = saved.iter().any(|e| e.name == name && e.unconfirmed());
                let doubtful = self.doubtful(&name).map(|v| v.doubt());
                ValueRow { name, addr: loc.addr, kind: loc.kind, value, min, max, limit_state, unconfirmed, doubtful }
            })
            .collect())
    }

    pub fn set(&mut self, name: &str, value: i64) -> Result<(), String> {
        self.refresh_paths();
        self.sync_addresses();
        let (_, loc) = *self
            .game()?
            .entries
            .iter()
            .find(|(n, _)| n == name)
            .ok_or(format!("no saved value called {name}"))?;
        if loc.addr == 0 {
            return Err(format!("{name} can't be found right now (is a save loaded?)"));
        }
        if let Some(v) = self.doubtful(name) {
            return Err(format!(
                "not written: {}, so it could land in the wrong place. If the number shown is wrong, find it again and save it as {name}",
                v.doubt()
            ));
        }
        let reply = self.helper.call(&format!("write {loc} {value}"));
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
        self.words = ocr::numbers(&frame, Some(&self.font))?;
        Ok((frame, self.words.clone()))
    }

    pub fn word_area(&self, i: usize) -> Option<Rect> {
        self.words.get(i).map(|w| ocr::watch_area(w.rect))
    }

    /// Picks the number to watch. A different number (not just a tighter box around the same
    /// one) starts the next search over.
    pub fn set_area(&mut self, area: Rect) {
        if self.search.is_some() && self.area.is_some_and(|a| !ocr::overlaps(a, area)) {
            self.say("another number picked: the next search starts over");
            self.search = None;
        }
        self.area = Some(area);
        self.seen.clear();
    }

    /// Forgets the search in progress, so the next number starts from scratch.
    pub fn reset(&mut self) {
        self.search = None;
        self.helper.call("track");
        self.say("search cleared");
    }

    /// Reads the number inside the watched area of a fresh frame.
    pub fn read(&mut self) -> Result<Option<i64>, String> {
        Ok(self.read_learned()?.map(|(n, _)| n))
    }

    /// Reads the number inside the watched area, and whether the learned digits read it.
    fn read_learned(&mut self) -> Result<Option<(i64, bool)>, String> {
        let frame = self.frame()?;
        self.read_frame(&frame)
    }

    /// Reads the watched number in `frame`. When the learned digits find it, the watched area
    /// follows it: the whole number with room to grow, wherever the old area cut it, and along
    /// with the numbers around it when they all move.
    fn read_frame(&mut self, frame: &Path) -> Result<Option<(i64, bool)>, String> {
        let area = self.area.ok_or("no area picked yet")?;
        let (read, moved) = ocr::read_number_at(frame, area, &cache_dir().join("area.png"), Some(&self.font), &mut self.seen)?;
        if let Some(to) = moved {
            if !ocr::overlaps(to, area) {
                self.say("the numbers around the watched one moved: following them");
            }
            self.area = Some(to);
        }
        Ok(read)
    }

    /// The area being watched (it follows the number).
    pub fn watched(&self) -> Option<Rect> {
        self.area
    }

    /// Reads the number just picked, and whether the game's learned digits read it. A read
    /// they didn't make is a guess to confirm; its frame is kept for `confirm`.
    pub fn read_picked(&mut self) -> Result<Option<(i64, bool)>, String> {
        self.area.ok_or("no area picked yet")?;
        let frame = self.frame()?;
        let read = self.read_frame(&frame)?;
        fs::rename(&frame, cache_dir().join("picked.png")).map_err(|e| e.to_string())?;
        Ok(read)
    }

    /// The player says the picked number read right: learn the game's digits from it.
    pub fn confirm(&mut self, n: i64) {
        self.learn(&cache_dir().join("picked.png"), n, false);
    }

    /// Learns the game's digits from the watched area of `frame`, which shows `n`. Failing to
    /// learn only gets logged.
    fn learn(&mut self, frame: &Path, n: i64, trusted: bool) {
        let (Some(area), Some(game)) = (self.area, self.game.as_ref()) else { return };
        let path = digits_path(&game.exe);
        let msg = match ocr::learn(frame, area, n, &mut self.font, trusted) {
            Ok(msg) => self.font.save(&path).map(|_| msg).unwrap_or_else(|e| format!("could not save the digits: {e}")),
            Err(e) => format!("digits not learned: {e}"),
        };
        self.say(&msg);
    }

    /// The attached game's learned digits: the shapes of 0 to 9 (empty = not learned yet).
    pub fn digits(&self) -> Vec<Vec<font::DigitShape>> {
        (0..10).map(|d| self.font.shapes(d)).collect()
    }

    /// Forgets the shapes learned for one digit (a wrongly learned one reads numbers wrong).
    pub fn forget_digit(&mut self, d: u8) -> Result<(), String> {
        let game = self.game.as_ref().ok_or("attach to a game first")?;
        let path = digits_path(&game.exe);
        let n = self.font.forget(d);
        self.font.save(&path)?;
        self.say(&format!("forgot the {n} learned shapes of {d}"));
        Ok(())
    }

    /// With the value's address known, memory tells what the screen shows: learn from that.
    fn learn_from_memory(&mut self, loc: Loc) {
        if self.area.is_none() {
            return;
        }
        let before = self.peek(&[loc])[0];
        let Ok(frame) = self.frame() else { return };
        // A value that changed while the frame was taken could show either.
        match (before, self.peek(&[loc])[0]) {
            (Some(a), Some(b)) if a == b => self.learn(&frame, a, true),
            _ => {}
        }
    }

    /// Current scan candidates (at most 20 are listed by the helper).
    pub fn candidates(&mut self) -> Vec<(Loc, Option<i64>)> {
        parse_values(&self.helper.call("list"))
    }

    /// Tells the real value apart from copies of it: writes a test value to one
    /// candidate at a time and checks whether it sticks, whether the other
    /// candidates follow it, and whether the screen shows it. Test writes are undone.
    pub fn probe(&mut self) -> Result<Option<Loc>, String> {
        let listed = self.helper.call("list");
        if listed.iter().any(|l| l.starts_with("...")) {
            return Err("too many candidates to probe; narrow down first".into());
        }
        let addrs: Vec<Loc> = parse_values(&listed).into_iter().map(|(a, _)| a).collect();
        if addrs.is_empty() {
            return Err("no candidates to probe".into());
        }
        for (i, &loc) in addrs.iter().enumerate() {
            let addr = loc.addr;
            // The exact original, fraction included, to put back afterwards.
            let listed = self.helper.call(&format!("peek {loc}"));
            let Some(orig) = listed.first().and_then(|l| l.split_once(" = ")).map(|(_, v)| v.trim().to_owned()) else {
                continue;
            };
            let Some(Some(shown)) = self.peek(&[loc]).first().copied() else { continue };
            let test = shown + 10;
            self.helper.call(&format!("write {loc} {test}"));
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
                self.helper.call(&format!("write {loc} {orig}"));
            }
            if stuck && (followers > 0 || shown) {
                self.helper.call(&format!("keep {addr:x}"));
                self.say(&format!("real value at 0x{addr:012x} (test write undone)"));
                return Ok(Some(loc));
            }
        }
        self.say("no candidate behaved like the real value (is the game paused?)");
        Ok(None)
    }

    /// Two reads in a row that agree. Once the learned digits know every digit, a number only
    /// Tesseract reads is something else in the watched spot (a menu opened over it): no read.
    fn read_stable(&mut self) -> Result<Option<i64>, String> {
        let a = self.read_learned()?;
        let b = self.read_learned()?;
        let (Some((n, learned)), true) = (a, a == b) else { return Ok(None) };
        if !learned && self.font.knows_all() {
            if self.ignored != Some(n) {
                self.say(&format!("the watched spot shows something the learned digits don't read (Tesseract: {n}), ignored"));
            }
            self.ignored = Some(n);
            return Ok(None);
        }
        self.ignored = None;
        Ok(Some(n))
    }

    /// Whether the one match left is the value itself: a test write (undone right after) has to
    /// stick. A game that keeps its own copy of a value for the HUD rewrites that copy every
    /// frame, and a search for a value stored some way Ferret doesn't look for (a 2-byte int,
    /// another encoding) ends on that copy; writing it changes nothing and saving it fails.
    fn sticks(&mut self, loc: Loc) -> bool {
        let listed = self.helper.call(&format!("peek {loc}"));
        let (Some(orig), Some(Some(shown))) =
            (listed.first().and_then(|l| l.split_once(" = ")).map(|(_, v)| v.trim().to_owned()), self.peek(&[loc]).first().copied())
        else {
            return true;
        };
        let test = shown + 10;
        self.helper.call(&format!("write {loc} {test}"));
        std::thread::sleep(Duration::from_millis(1500));
        let now = self.peek(&[loc])[0];
        if now == Some(test) {
            self.helper.call(&format!("write {loc} {orig}"));
            return true;
        }
        // The game changed it meanwhile (the player gathered something): still the value, nearer
        // the test value than where a copy would be put back to. Undo only the test's +10.
        if let Some(v) = now.filter(|v| (v - test).abs() < (v - shown).abs()) {
            self.helper.call(&format!("write {loc} {}", v - 10));
            return true;
        }
        self.say(&format!(
            "wrote {test} to the last match as a test and the game put back {}: it's a copy the game keeps \
             refreshing (for its display), not where it keeps the value",
            now.map_or("something else".into(), |v| v.to_string())
        ));
        false
    }

    /// Whether the one match left holds what the screen shows (`n`); a match reached through
    /// misreads can be anything.
    fn holds(&mut self, loc: Loc, n: i64) -> bool {
        let v = self.peek(&[loc])[0];
        let ok = v.is_some_and(|v| (v - n).abs() <= 1);
        if !ok {
            self.say(&format!(
                "the last match holds {} but the screen shows {n}: not it, starting over",
                v.map_or("nothing".into(), |v| v.to_string())
            ));
        }
        ok
    }

    /// The automated scan loop: read the number off the window, scan for it, and
    /// keep narrowing down whenever it changes on screen. Stops early when
    /// `cancel` is set. Continues a search that was stopped or typed into.
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
        let mut count = match self.search {
            Some((before, _)) => {
                let reply = self.helper.call(&format!("next {first}"));
                self.say(&format!("screen shows {first}, continuing from {before} matches: {}", reply.join(" ")));
                match_count(&reply).unwrap_or(0)
            }
            None => 0,
        };
        if count == 0 {
            let reply = self.helper.call(&format!("scan {first}"));
            self.say(&format!("screen shows {first}: {}", reply.join(" ")));
            count = match_count(&reply).ok_or_else(|| first_error(&reply).unwrap_or("scan failed".into()))?;
        }
        self.search = Some((count, 0));
        let mut last = first;
        let mut unchanged_rounds = 0;
        // A number nothing fits: a misread or something covering the number, unless it's read
        // again right after.
        let mut unfit = None;
        while count > 1 && start.elapsed() < limit && !cancelled(&self.cancel) {
            std::thread::sleep(Duration::from_millis(300));
            // Values that change while the screen is read may show either end of that change.
            self.helper.call("mark");
            let Some(now) = self.read_stable()? else { continue };
            if now == last {
                continue;
            }
            let reply = self.helper.call(&format!("next {now}"));
            let new_count = match_count(&reply).unwrap_or(0);
            self.say(&format!("screen shows {now}: {}", reply.join(" ")));
            if new_count == 0 {
                if unfit != Some(now) {
                    self.say(&format!("nothing fits {now} (a misread?), keeping the {count} matches"));
                    unfit = Some(now);
                    continue;
                }
                let reply = self.helper.call(&format!("scan {now}"));
                self.say(&format!("{now} again: lost it, rescanning: {}", reply.join(" ")));
                count = match_count(&reply).unwrap_or(0);
                unchanged_rounds = 0;
            } else {
                unchanged_rounds = if new_count == count { unchanged_rounds + 1 } else { 0 };
                count = new_count;
            }
            unfit = None;
            self.search = Some((count, 0));
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
        if count == 0 {
            self.search = None;
        }
        if count == 1 {
            self.search = None;
            let loc = self.candidates()[0].0;
            if !self.holds(loc, last) {
                self.helper.call("track");
                return Err("the only match left doesn't hold the number on screen (misreads?); press Start to search again".into());
            }
            if !self.sticks(loc) {
                self.helper.call("track");
                return Err(COPY_ONLY.into());
            }
            self.say(&format!("stored as {}", loc.kind.with_article()));
            self.learn_from_memory(loc);
            return Ok(AutoResult::Found(loc));
        }
        if (2..=20).contains(&count) && !cancelled(&self.cancel) {
            self.say("checking which one is the real value:");
            if let Some(loc) = self.probe()? {
                self.search = None;
                self.say(&format!("stored as {}", loc.kind.with_article()));
                self.learn_from_memory(loc);
                return Ok(AutoResult::Found(loc));
            }
        }
        Ok(AutoResult::Several(count))
    }

    /// The same search driven by numbers the player types, for when the screen can't be read:
    /// the first number starts it, each one after narrows it down. With a watched area, the
    /// number also teaches Ferret how the game draws its digits.
    pub fn typed(&mut self, n: i64) -> Result<AutoResult, String> {
        self.game()?;
        if self.area.is_some() {
            let frame = self.frame()?;
            self.learn(&frame, n, false);
        }
        self.typed_search(n)
    }

    fn typed_search(&mut self, n: i64) -> Result<AutoResult, String> {
        let (count, unchanged) = match self.search {
            Some((before, unchanged)) => {
                let reply = self.helper.call(&format!("next {n}"));
                self.say(&format!("typed {n}: {}", reply.join(" ")));
                let count = match_count(&reply).ok_or_else(|| first_error(&reply).unwrap_or("scan failed".into()))?;
                if count == 0 {
                    self.say("nothing went from the last number to this one, starting over");
                    self.search = None;
                    return self.typed_search(n);
                }
                (count, if count == before { unchanged + 1 } else { 0 })
            }
            None => {
                let reply = self.helper.call(&format!("scan {n}"));
                self.say(&format!("typed {n}: {}", reply.join(" ")));
                (match_count(&reply).ok_or_else(|| first_error(&reply).unwrap_or("scan failed".into()))?, 0)
            }
        };
        self.search = Some((count, unchanged));
        let found = match count {
            0 => {
                self.search = None;
                return Err(format!("{n} is nowhere in the game's memory"));
            }
            1 => {
                let loc = self.candidates()[0].0;
                if !self.holds(loc, n) {
                    self.search = None;
                    self.helper.call("track");
                    return Err(format!("the only match left doesn't hold {n}; type the number again to start over"));
                }
                if !self.sticks(loc) {
                    self.search = None;
                    self.helper.call("track");
                    return Err(COPY_ONLY.into());
                }
                Some(loc)
            }
            // The same few keep following the value: copies of it. Find the real one.
            2..=20 if unchanged >= 2 => {
                self.say("checking which one is the real value:");
                self.probe()?
            }
            _ => None,
        };
        match found {
            Some(loc) => {
                self.search = None;
                self.say(&format!("stored as {}", loc.kind.with_article()));
                self.learn_from_memory(loc);
                Ok(AutoResult::Found(loc))
            }
            None => Ok(AutoResult::Several(count)),
        }
    }
}