// What the helper asks Linux about other programs: everything through /proc. The rest of the
// helper goes through these (as `os::`), so another OS only has to supply the same functions.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::helper::Region;
use crate::mem::Mem;

pub fn maps(pid: u32) -> io::Result<Vec<Region>> {
    let text = fs::read_to_string(format!("/proc/{pid}/maps"))?;
    Ok(text
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (s, e) = it.next()?.split_once('-')?;
            let perms = it.next()?.to_owned();
            let path = it.skip(3).collect::<Vec<_>>().join(" ");
            Some(Region {
                start: u64::from_str_radix(s, 16).ok()?,
                end: u64::from_str_radix(e, 16).ok()?,
                perms,
                path,
            })
        })
        .collect())
}

/// Windows' heap key (see windows.rs): glibc's chunk headers are plain sizes, the same for
/// records of one kind, and Wine's heap is its own.
pub fn heap_key(_pid: u32, _addr: u64) -> Option<u64> {
    None
}

pub fn environ(pid: u32) -> HashMap<String, String> {
    fs::read(format!("/proc/{pid}/environ"))
        .unwrap_or_default()
        .split(|b| *b == 0)
        .filter_map(|kv| {
            let kv = String::from_utf8_lossy(kv);
            let (k, v) = kv.split_once('=')?;
            Some((k.to_owned(), v.to_owned()))
        })
        .collect()
}

/// The command line, the program as started (argv[0]) first; empty when it can't be read.
pub fn argv(pid: u32) -> Vec<String> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let raw = raw.strip_suffix(&[0]).unwrap_or(&raw);
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(|b| *b == 0).map(|a| String::from_utf8_lossy(a).into_owned()).collect()
}

/// The folder the program runs in.
pub fn cwd(pid: u32) -> String {
    fs::read_link(format!("/proc/{pid}/cwd")).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The program's file (a native one's; under Wine, Wine's loader).
pub fn exe(pid: u32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// The process that started it.
pub fn parent(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// A file as the program sees it: through its root, so files inside its sandbox
/// (pressure-vessel, flatpak) are found too.
pub fn inside(pid: u32, path: &Path) -> PathBuf {
    Path::new("/proc").join(pid.to_string()).join("root").join(path.strip_prefix("/").unwrap_or(path))
}

/// Where the main thread's stack starts (`startstack` in /proc/<pid>/stat: the address of argc;
/// argv's and the environment's strings are above it, the frames below).
pub fn stack_start(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the name (which may hold spaces); startstack is the 28th field.
    stat.rsplit_once(')')?.1.split_whitespace().nth(25)?.parse().ok().filter(|&a| a != 0)
}

/// The player's own processes, by pid.
pub fn own_pids() -> Vec<u32> {
    let uid = my_uid();
    let Ok(dir) = fs::read_dir("/proc") else { return Vec::new() };
    let mut pids: Vec<u32> =
        dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).filter(|pid| owner_uid(*pid) == Some(uid)).collect();
    pids.sort();
    pids
}

fn my_uid() -> u32 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|v| v.split_whitespace().next()?.parse().ok())
        })
        .unwrap_or(u32::MAX)
}

fn owner_uid(pid: u32) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(format!("/proc/{pid}")).ok().map(|m| m.uid())
}

/// A file inside an open `/proc/<pid>` directory. The directory handle stays that process's
/// even if its pid is reused meanwhile, so what was checked is what gets opened.
fn open_in(dir: &File, name: &str, flags: libc::c_int) -> io::Result<File> {
    use std::os::unix::io::{AsRawFd, FromRawFd};
    let name = std::ffi::CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// The real user id the process behind an open `/proc/<pid>` runs as (the directory itself
/// belongs to root when the process can't be dumped).
fn real_uid(proc_dir: &File) -> Option<u32> {
    use std::io::Read;
    let mut status = String::new();
    open_in(proc_dir, "status", libc::O_RDONLY).ok()?.read_to_string(&mut status).ok()?;
    status.lines().find_map(|l| l.strip_prefix("Uid:"))?.split_whitespace().next()?.parse().ok()
}

/// The game's memory, for reading and writing; the error is the reply's text.
pub fn open_mem(pid: u32) -> Result<Mem, String> {
    // With CAP_SYS_PTRACE the kernel would open anyone's: only the player's own programs, and
    // the memory is opened through the same directory handle the owner was checked on.
    let proc_dir = File::open(format!("/proc/{pid}")).map_err(|e| format!("cannot open /proc/{pid}: {e}"))?;
    if real_uid(&proc_dir) != Some(my_uid()) {
        return Err("refusing to attach: not your program".into());
    }
    open_in(&proc_dir, "mem", libc::O_RDWR).map(Mem::new).map_err(|e| match ptrace_scope() {
        // Yama, not file permissions: the game is the user's own.
        Some(scope @ 1..) if e.kind() == io::ErrorKind::PermissionDenied && owner_uid(pid) == Some(my_uid()) => {
            format!("blocked by ptrace_scope {scope}")
        }
        _ => format!("cannot open /proc/{pid}/mem: {e}"),
    })
}

fn ptrace_scope() -> Option<u8> {
    fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").ok()?.trim().parse().ok()
}

/// `info`: what the helper can see, for bug reports.
pub fn info(out: &mut impl Write) -> io::Result<()> {
    let ns = fs::read_link("/proc/self/ns/pid").map(|p| p.display().to_string()).unwrap_or_default();
    let scope = fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").unwrap_or_else(|_| "n/a".into());
    let visible = fs::read_dir("/proc")?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()))
        .count();
    writeln!(out, "helper pid namespace: {ns}")?;
    writeln!(out, "helper uid: {}", my_uid())?;
    writeln!(out, "yama ptrace_scope: {}", scope.trim())?;
    writeln!(out, "privileged: {}", if PRIVILEGED.load(Ordering::Relaxed) { "yes" } else { "no" })?;
    writeln!(out, "processes visible to helper: {visible}")
}

/// Running with CAP_SYS_PTRACE after `drop_root` (started through pkexec).
static PRIVILEGED: AtomicBool = AtomicBool::new(false);

const CAP_SYS_PTRACE: u32 = 19;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Started as root through pkexec (kernel.yama.ptrace_scope 1 or 2 keeps the player's own
/// programs from being opened): becomes the player again at once, keeping only CAP_SYS_PTRACE,
/// which is all that reading another program's memory needs. `as_user` is `<uid>:<gid>:<home>`
/// from Ferret; the uid must be the one pkexec says asked. Never serves as root: anything that
/// fails here ends the helper.
pub fn drop_root(as_user: Option<&str>) -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let asked: u32 = std::env::var("PKEXEC_UID").ok().and_then(|v| v.parse().ok()).ok_or("started as root, but not through pkexec")?;
    let mut parts = as_user.ok_or("started as root without --as <uid>:<gid>:<home>")?.splitn(3, ':');
    let uid: u32 = parts.next().and_then(|v| v.parse().ok()).ok_or("bad --as uid")?;
    let gid: u32 = parts.next().and_then(|v| v.parse().ok()).ok_or("bad --as gid")?;
    let home = parts.next().ok_or("bad --as home")?;
    if uid != asked || uid == 0 {
        return Err(format!("--as {uid} isn't the user who asked ({asked})"));
    }
    let check = |ok: bool, what: &str| if ok { Ok(()) } else { Err(format!("{what}: {}", io::Error::last_os_error())) };
    unsafe {
        check(libc::setgroups(0, std::ptr::null()) == 0, "setgroups")?;
        check(libc::setresgid(gid, gid, gid) == 0, "setresgid")?;
        // Permitted capabilities survive the uid change; effective ones are cleared by it.
        check(libc::prctl(8 /* PR_SET_KEEPCAPS */, 1) == 0, "keepcaps")?;
        check(libc::setresuid(uid, uid, uid) == 0, "setresuid")?;
        let header = CapHeader { version: 0x2008_0522 /* _LINUX_CAPABILITY_VERSION_3 */, pid: 0 };
        let bit = 1u32 << CAP_SYS_PTRACE;
        let data = [CapData { effective: bit, permitted: bit, inheritable: 0 }, CapData { effective: 0, permitted: 0, inheritable: 0 }];
        check(libc::syscall(libc::SYS_capset, &header, data.as_ptr()) == 0, "capset")?;
        // Nothing this process starts can gain privileges either.
        check(libc::prctl(38 /* PR_SET_NO_NEW_PRIVS */, 1, 0, 0, 0) == 0, "no_new_privs")?;
        check(libc::geteuid() == uid && libc::getegid() == gid, "still not the user")?;
    }
    std::env::set_var("HOME", home);
    PRIVILEGED.store(true, Ordering::Relaxed);
    Ok(())
}

/// Where Steam keeps its files (appcache/appinfo.vdf): native, the older link, the flatpak.
pub fn steam_roots() -> Vec<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    [".local/share/Steam", ".steam/steam", ".var/app/com.valvesoftware.Steam/.local/share/Steam"].iter().map(|d| home.join(d)).collect()
}

/// A line from the frontend is waiting on stdin.
pub fn stdin_ready() -> bool {
    let mut fd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut fd, 1, 0) > 0 }
}

/// Losing Ferret (it closed or crashed) ends stdin: test values are put back then. The SIGINT
/// flatpak-spawn's --watch-bus sends at the same time would end the helper first.
pub fn ignore_interrupt() {
    unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
}

/// SIGTERM (systemd stopping the session, `kill`) calls `end` (`exit_untraced`): blocked in
/// every thread (they inherit it from this one; the helper starts no programs, which would
/// inherit it as well) and taken by one thread that waits for it.
pub fn on_terminate(end: fn() -> !) {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        std::thread::spawn(move || {
            let mut sig = 0;
            if libc::sigwait(&set, &mut sig) == 0 {
                end();
            }
        });
    }
}
