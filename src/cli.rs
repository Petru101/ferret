// Text interface (`ferret --cli`): one command per line, used by the test scripts.

use std::io::{self, BufRead, IsTerminal, Write};
use std::time::Duration;

use crate::core::{self, AutoResult, Core};
use crate::ocr::Rect;

const HELP: &str = "vision: window, shot, numbers, watch <n>|<x y w h>, read, auto [seconds], type <n>, reset, probe
restarts: save <name>, restore, values, set <name> <n>, limit <name> [min] <max>|off";

fn bound(v: &str) -> Result<Option<i64>, String> {
    match v {
        "-" => Ok(None),
        v => v.parse().map(Some).map_err(|_| format!("not a number: {v}")),
    }
}

fn run_command(core: &mut Core, line: &str) -> Result<(), String> {
    let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
    let arg = arg.trim();
    match cmd {
        "sandbox" => core::sandbox_report().iter().for_each(|l| println!("{l}")),
        "games" => {
            for g in core.games() {
                let id = g.app_id.map(|id| format!("  [SteamAppId={id}]")).unwrap_or_default();
                let ac = g.anti_cheat.map(|ac| format!("  [{ac}]")).unwrap_or_default();
                println!("{:>7}  {}{id}{ac}", g.pid, g.exe);
            }
        }
        "attach" => {
            let pid = arg.parse().map_err(|_| "usage: attach <pid>")?;
            core.attach(pid)?;
        }
        "window" => core.start_capture().map(|_| println!("capturing window"))?,
        "shot" => println!("saved {}", core.frame()?.display()),
        "numbers" => {
            let (frame, words) = core.numbers()?;
            for (i, w) in words.iter().enumerate() {
                let r = w.rect;
                println!("{i:>3}  {:<12} at {},{} {}x{}  conf {:.0}", w.text, r.x, r.y, r.w, r.h, w.conf);
            }
            println!("{} numbers found (frame: {})", words.len(), frame.display());
        }
        "watch" => {
            let nums: Vec<u32> = arg.split_whitespace().filter_map(|n| n.parse().ok()).collect();
            let area = match nums.as_slice() {
                [i] => core.word_area(*i as usize).ok_or("no such number, run `numbers` first")?,
                [x, y, w, h] => Rect { x: *x, y: *y, w: *w, h: *h },
                _ => return Err("usage: watch <n> | watch <x> <y> <w> <h>".into()),
            };
            core.set_area(area);
            println!("watching {},{} {}x{}", area.x, area.y, area.w, area.h);
            match core.read()? {
                Some(n) => println!("reads {n}"),
                None => println!("no number read there yet (crop: {})", core::cache_dir().join("area.png").display()),
            }
        }
        "read" => println!("{:?}", core.read()?),
        "auto" => match core.auto(Duration::from_secs(arg.parse().unwrap_or(600)))? {
            AutoResult::Found(loc) => println!("found it at 0x{:x} ({})", loc.addr, loc.kind.describe()),
            AutoResult::Several(n) => println!("{n} candidates left"),
        },
        "type" => match core.typed(arg.parse().map_err(|_| "usage: type <n>")?)? {
            AutoResult::Found(loc) => println!("found it at 0x{:x} ({})", loc.addr, loc.kind.describe()),
            AutoResult::Several(n) => println!("{n} candidates left"),
        },
        "reset" => core.reset(),
        "probe" => {
            core.probe()?;
        }
        "save" => {
            core.save(arg)?;
        }
        "restore" => core.restore()?,
        "values" => {
            for v in core.values()? {
                let limit = v
                    .limit_state
                    .map(|s| format!("  kept {}, {s}", core::limit_text(v.min, v.max)))
                    .unwrap_or_default();
                let value = v.value.map_or("??".into(), |v| v.to_string());
                println!("{:<12} {value} (at 0x{:x}, {}){limit}", v.name, v.addr, v.kind.describe());
            }
        }
        "limit" => {
            let f: Vec<&str> = arg.split_whitespace().collect();
            let usage = "usage: limit <name> <max> | limit <name> <min> <max> | limit <name> off";
            let (min, max) = match f.get(1..).unwrap_or_default() {
                ["off"] => (None, None),
                [max] => (None, bound(max)?),
                [min, max] => (bound(min)?, bound(max)?),
                _ => return Err(usage.into()),
            };
            core.limit(f.first().ok_or(usage)?, min, max)?;
        }
        "set" if arg.split_whitespace().next().is_some_and(|n| n.parse::<i64>().is_err()) => {
            let mut it = arg.split_whitespace();
            let name = it.next().unwrap_or_default();
            let value = it.next().and_then(|v| v.parse().ok()).ok_or("usage: set <name> <n>")?;
            core.set(name, value)?;
        }
        _ => {
            for l in core.raw(line) {
                println!("{l}");
            }
            if cmd == "help" || cmd == "?" {
                println!("{HELP}");
            }
        }
    }
    Ok(())
}

pub fn run() {
    let mut core = match Core::new(Box::new(|msg| println!("{msg}"))) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
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
        if line == "quit" || line == "exit" {
            break;
        }
        if let Err(e) = run_command(&mut core, &line) {
            println!("error: {e}");
        }
        io::stdout().flush().ok();
    }
}
