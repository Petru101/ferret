// What the helper asks Windows about other programs: the same functions as linux.rs (as
// `os::`). Paths come with forward slashes ("C:/Games/x/x.exe"), so the rest of the helper,
// written for Linux and Wine's paths, splits them the same way.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::helper::Region;
use crate::mem::Mem;
use crate::win32::*;

fn open(pid: u32, access: u32) -> Option<Handle> {
    Handle::valid(unsafe { OpenProcess(access, 0, pid) })
}

/// Reads `buf.len()` bytes at `addr` of an open process.
fn read(p: &Handle, addr: u64, buf: &mut [u8]) -> bool {
    let mut n = 0;
    unsafe { ReadProcessMemory(p.0, addr as *const c_void, buf.as_mut_ptr().cast(), buf.len(), &mut n) != 0 && n == buf.len() }
}

fn read_u64(p: &Handle, addr: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    read(p, addr, &mut b).then(|| u64::from_le_bytes(b))
}

/// A UNICODE_STRING (64-bit layout) at `addr` of the process.
fn read_unicode(p: &Handle, addr: u64) -> Option<String> {
    let mut head = [0u8; 16];
    if !read(p, addr, &mut head) {
        return None;
    }
    let len = u16::from_le_bytes([head[0], head[1]]) as usize;
    let buf = u64::from_le_bytes(head[8..16].try_into().unwrap());
    let mut b = vec![0u8; len];
    if len > 0 && !read(p, buf, &mut b) {
        return None;
    }
    let w: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    Some(String::from_utf16_lossy(&w))
}

/// The address of a process's RTL_USER_PROCESS_PARAMETERS (the 64-bit one, also for 32-bit
/// games: WOW64 processes have both).
fn params(p: &Handle) -> Option<u64> {
    let mut pbi: PROCESS_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<PROCESS_BASIC_INFORMATION>() as u32;
    let status = unsafe { NtQueryInformationProcess(p.0, 0, (&mut pbi as *mut PROCESS_BASIC_INFORMATION).cast(), size, std::ptr::null_mut()) };
    if status != 0 || pbi.PebBaseAddress == 0 {
        return None;
    }
    read_u64(p, pbi.PebBaseAddress as u64 + 0x20)
}

/// Device paths ("\Device\HarddiskVolume3") to drive letters ("C:").
fn drives() -> &'static Vec<(String, String)> {
    static DRIVES: OnceLock<Vec<(String, String)>> = OnceLock::new();
    DRIVES.get_or_init(|| {
        (b'A'..=b'Z')
            .filter_map(|l| {
                let drive = format!("{}:", l as char);
                let mut target = [0u16; 512];
                let n = unsafe { QueryDosDeviceW(wide(&drive).as_ptr(), target.as_mut_ptr(), target.len() as u32) };
                (n > 0).then(|| (from_wide(&target), drive))
            })
            .collect()
    })
}

/// "\Device\HarddiskVolume3\Games\x.exe" -> "C:/Games/x.exe".
fn dos_path(device: &str) -> String {
    let path = drives()
        .iter()
        .find_map(|(dev, drive)| device.strip_prefix(dev.as_str()).filter(|rest| rest.starts_with('\\')).map(|rest| format!("{drive}{rest}")))
        .unwrap_or_else(|| device.to_owned());
    path.replace('\\', "/")
}

fn perms(protect: u32) -> String {
    if protect & PAGE_GUARD != 0 || protect & 0xff == PAGE_NOACCESS {
        return "---p".into();
    }
    let (r, w, x) = match protect & 0xff {
        PAGE_READONLY => (true, false, false),
        PAGE_READWRITE | PAGE_WRITECOPY => (true, true, false),
        PAGE_EXECUTE => (false, false, true),
        PAGE_EXECUTE_READ => (true, false, true),
        PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY => (true, true, true),
        _ => (false, false, false),
    };
    format!("{}{}{}p", if r { 'r' } else { '-' }, if w { 'w' } else { '-' }, if x { 'x' } else { '-' })
}

/// The game's committed memory, region by region, with the file each is mapped from.
pub fn maps(pid: u32) -> io::Result<Vec<Region>> {
    let p = open(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ).ok_or_else(io::Error::last_os_error)?;
    let mut regions = Vec::new();
    let mut addr = 0u64;
    let mut names: HashMap<usize, String> = HashMap::new();
    loop {
        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let n = unsafe { VirtualQueryEx(p.0, addr as *const c_void, &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
        if n == 0 {
            break;
        }
        let (start, end) = (mbi.BaseAddress as u64, mbi.BaseAddress as u64 + mbi.RegionSize as u64);
        if mbi.State == MEM_COMMIT {
            let path = if mbi.Type == MEM_IMAGE || mbi.Type == MEM_MAPPED {
                names
                    .entry(mbi.AllocationBase)
                    .or_insert_with(|| {
                        let mut name = [0u16; 1024];
                        let len = unsafe { K32GetMappedFileNameW(p.0, mbi.AllocationBase as *const c_void, name.as_mut_ptr(), name.len() as u32) };
                        if len == 0 { String::new() } else { dos_path(&from_wide(&name)) }
                    })
                    .clone()
            } else {
                String::new()
            };
            regions.push(Region { start, end, perms: perms(mbi.Protect), path });
        }
        if end <= addr {
            break;
        }
        addr = end;
    }
    Ok(regions)
}

/// The environment block, read from the game's process parameters.
pub fn environ(pid: u32) -> HashMap<String, String> {
    (|| {
        let p = open(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)?;
        let params = params(&p)?;
        let env = read_u64(&p, params + 0x80)?;
        let size = read_u64(&p, params + 0x3F0)?.min(1 << 20) as usize;
        let mut b = vec![0u8; size & !1];
        if !read(&p, env, &mut b) {
            return None;
        }
        let w: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Some(
            w.split(|&c| c == 0)
                .take_while(|kv| !kv.is_empty())
                .filter_map(|kv| {
                    let kv = String::from_utf16_lossy(kv);
                    // "=C:=C:\..." entries (a drive's folder) start with '='.
                    let first = kv.chars().next()?.len_utf8();
                    let (k, v) = kv[first..].split_once('=')?;
                    Some((format!("{}{k}", &kv[..first]), v.to_owned()))
                })
                .collect(),
        )
    })()
    .unwrap_or_default()
}

/// The command line split the way programs split it (CommandLineToArgvW's rules), the program
/// first.
pub fn argv(pid: u32) -> Vec<String> {
    let line = (|| {
        let p = open(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)?;
        read_unicode(&p, params(&p)? + 0x70)
    })();
    line.map_or_else(Vec::new, |l| split_command_line(&l))
}

pub fn split_command_line(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    // The program: up to the next space, or between quotes; backslashes are plain there.
    let mut program = String::new();
    if chars.peek() == Some(&'"') {
        chars.next();
        for c in chars.by_ref() {
            if c == '"' {
                break;
            }
            program.push(c);
        }
    }
    while let Some(&c) = chars.peek() {
        if c == ' ' || c == '\t' {
            break;
        }
        program.push(c);
        chars.next();
    }
    args.push(program);
    loop {
        while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let (mut arg, mut quoted) = (String::new(), false);
        while let Some(&c) = chars.peek() {
            match c {
                ' ' | '\t' if !quoted => break,
                '\\' => {
                    let mut n = 0;
                    while chars.peek() == Some(&'\\') {
                        chars.next();
                        n += 1;
                    }
                    if chars.peek() == Some(&'"') {
                        arg.extend(std::iter::repeat('\\').take(n / 2));
                        if n % 2 == 1 {
                            arg.push('"');
                            chars.next();
                        }
                    } else {
                        arg.extend(std::iter::repeat('\\').take(n));
                    }
                }
                '"' => {
                    chars.next();
                    if quoted && chars.peek() == Some(&'"') {
                        arg.push('"');
                        chars.next();
                    } else {
                        quoted = !quoted;
                    }
                }
                _ => {
                    arg.push(c);
                    chars.next();
                }
            }
        }
        args.push(arg);
    }
    args
}

/// The folder the program runs in.
pub fn cwd(pid: u32) -> String {
    (|| {
        let p = open(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)?;
        read_unicode(&p, params(&p)? + 0x38)
    })()
    .map(|d| d.trim_end_matches('\\').replace('\\', "/"))
    .unwrap_or_default()
}

/// The program's file.
pub fn exe(pid: u32) -> Option<PathBuf> {
    let p = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut name = [0u16; 1024];
    let mut len = name.len() as u32;
    (unsafe { QueryFullProcessImageNameW(p.0, 0, name.as_mut_ptr(), &mut len) } != 0)
        .then(|| PathBuf::from(from_wide(&name[..len as usize]).replace('\\', "/")))
}

fn snapshot_processes() -> Vec<PROCESSENTRY32W> {
    let Some(snap) = Handle::valid(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else { return Vec::new() };
    let mut all = Vec::new();
    let mut e: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut ok = unsafe { Process32FirstW(snap.0, &mut e) } != 0;
    while ok {
        all.push(unsafe { std::ptr::read(&e) });
        ok = unsafe { Process32NextW(snap.0, &mut e) } != 0;
    }
    all
}

/// What runs on the computer, for anti-cheat that runs beside games rather than in them: every
/// process's program name (any user's: the snapshot names them all) and every active service
/// and kernel driver by its service name (BEDaisy, vgk, EasyAntiCheat_EOS). Neither needs
/// administrator rights.
pub fn running_programs() -> Vec<String> {
    let mut names: Vec<String> = snapshot_processes().iter().map(|e| from_wide(&e.szExeFile)).collect();
    let Some(scm) = Handle::valid(unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ENUMERATE_SERVICE) }) else {
        return names;
    };
    let scm = std::mem::ManuallyDrop::new(scm);
    let (mut needed, mut count, mut resume) = (0u32, 0u32, 0u32);
    let mut buf: Vec<u64> = vec![0; 1 << 15];
    loop {
        let size = (buf.len() * 8) as u32;
        let ok = unsafe {
            EnumServicesStatusExW(scm.0, 0, SERVICE_DRIVERS_AND_WIN32, SERVICE_ACTIVE, buf.as_mut_ptr().cast(), size, &mut needed, &mut count, &mut resume, std::ptr::null())
        };
        let more = ok == 0 && std::io::Error::last_os_error().raw_os_error() == Some(ERROR_MORE_DATA);
        if ok == 0 && !more {
            break;
        }
        let entries = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, count as usize) };
        for e in entries {
            let len = (0..).take_while(|&i| unsafe { *e.lpServiceName.add(i) } != 0).count();
            names.push(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(e.lpServiceName, len) }));
        }
        if !more {
            break;
        }
        if count == 0 {
            buf.resize((needed as usize).div_ceil(8) + 1, 0);
        }
    }
    unsafe { CloseServiceHandle(scm.0) };
    names
}

/// The process that started it.
pub fn parent(pid: u32) -> Option<u32> {
    snapshot_processes().iter().find(|e| e.th32ProcessID == pid).map(|e| e.th32ParentProcessID)
}

/// A file as the program sees it: Windows has no per-process views of files.
pub fn inside(_pid: u32, path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Windows games' threads run on stacks of their own (as under Wine): no main-thread roots.
pub fn stack_start(_pid: u32) -> Option<u64> {
    None
}

/// The player's own programs: those in this desktop session whose memory the helper may read
/// (another user's, services and protected processes can't be opened).
pub fn own_pids() -> Vec<u32> {
    let me = unsafe { GetCurrentProcessId() };
    let session = |pid: u32| {
        let mut s = 0;
        (unsafe { ProcessIdToSessionId(pid, &mut s) } != 0).then_some(s)
    };
    let mine = session(me);
    let mut pids: Vec<u32> = snapshot_processes()
        .iter()
        .map(|e| e.th32ProcessID)
        .filter(|&pid| pid != 0 && pid != me && session(pid) == mine)
        .filter(|&pid| open(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ).is_some())
        .collect();
    pids.sort();
    pids
}

/// Whether the process shows a window (a visible top-level one).
pub fn has_window(pid: u32) -> bool {
    struct Search {
        pid: u32,
        found: bool,
    }
    unsafe extern "system" fn each(hwnd: HWND, data: isize) -> BOOL {
        let s = unsafe { &mut *(data as *mut Search) };
        let mut owner = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut owner) };
        if owner == s.pid && unsafe { IsWindowVisible(hwnd) } != 0 {
            s.found = true;
            return 0;
        }
        1
    }
    let mut s = Search { pid, found: false };
    unsafe { EnumWindows(each, &mut s as *mut Search as isize) };
    s.found
}

/// Whether the process runs 32-bit code (WOW64).
pub fn is_wow64(pid: u32) -> bool {
    let Some(p) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else { return false };
    let mut wow = 0;
    unsafe { IsWow64Process(p.0, &mut wow) != 0 && wow != 0 }
}

/// The game's memory, for reading and writing; the error is the reply's text.
pub fn open_mem(pid: u32) -> Result<Mem, String> {
    if !own_pids().contains(&pid) {
        return Err("refusing to attach: not your program".into());
    }
    let access = PROCESS_VM_READ | PROCESS_VM_WRITE | PROCESS_VM_OPERATION | PROCESS_QUERY_INFORMATION;
    match open(pid, access) {
        Some(h) => Ok(Mem::new(h)),
        None => Err(format!("cannot open process {pid}: {}", io::Error::last_os_error())),
    }
}

/// `info`: what the helper can see, for bug reports.
pub fn info(out: &mut impl Write) -> io::Result<()> {
    let mut session = 0;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    writeln!(out, "helper session: {session}")?;
    writeln!(out, "privileged: no")?;
    writeln!(out, "processes visible to helper: {}", snapshot_processes().len())?;
    writeln!(out, "programs, services and drivers running: {}", running_programs().len())
}

/// Elevation is never asked for on Windows: the player's own games open without it.
pub fn drop_root(_as_user: Option<&str>) -> Result<(), String> {
    Ok(())
}

/// A line from the frontend is waiting on stdin (a pipe from Ferret).
pub fn stdin_ready() -> bool {
    let mut avail = 0u32;
    let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    unsafe { PeekNamedPipe(h, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut avail, std::ptr::null_mut()) != 0 && avail > 0 }
}

/// Ferret going away ends stdin; nothing else to catch.
pub fn ignore_interrupt() {}

/// Windows ends a process without a signal it could take: nothing to set up.
pub fn on_terminate(_end: fn() -> !) {}

#[cfg(test)]
mod tests {
    use super::split_command_line;

    #[test]
    fn splits_like_windows() {
        let a = split_command_line(r#""C:\Program Files\Game\game.exe" -windowed "a b" c\\"d" e\"f"#);
        assert_eq!(a, [r"C:\Program Files\Game\game.exe", "-windowed", "a b", r"c\d", r#"e"f"#]);
        assert_eq!(split_command_line(r"C:\Games\x.exe"), [r"C:\Games\x.exe"]);
        assert_eq!(split_command_line(r#"x.exe "" y"#), ["x.exe", "", "y"]);
    }
}
