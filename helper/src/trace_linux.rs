// The tracer on Linux: debug registers set through ptrace on every thread of the game.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::time::{Duration, Instant};

use super::{refuse_when_stopping, stopping, Hit, Regs, DR6_HITS, DR7_EXECUTE};

/// offsetof(struct user, u_debugreg) on x86_64.
const DEBUGREG: usize = 848;

pub struct Tracer {
    tids: Vec<i32>,
    // Threads we left stopped at a breakpoint; they need no interrupt before detaching.
    parked: HashSet<i32>,
}

fn ptrace(req: libc::c_uint, tid: i32, addr: usize, data: usize) -> libc::c_long {
    unsafe { libc::ptrace(req, tid, addr as *mut libc::c_void, data as *mut libc::c_void) }
}

fn wait(tid: i32, flags: libc::c_int) -> Option<(i32, libc::c_int)> {
    let mut status = 0;
    let r = unsafe { libc::waitpid(tid, &mut status, flags | libc::__WALL) };
    (r > 0).then_some((r, status))
}

/// The longest `Tracer::stop` waits for a thread to stop (a live one stops at once).
const STOP_WAIT: Duration = Duration::from_secs(2);

/// The thread has exited (a zombie, or already gone).
fn dead(tid: i32) -> bool {
    match fs::read_to_string(format!("/proc/{tid}/stat")) {
        Ok(stat) => stat.rsplit_once(") ").is_none_or(|(_, rest)| matches!(rest.as_bytes().first(), Some(b'Z' | b'X'))),
        Err(_) => true,
    }
}

fn is_event_stop(status: libc::c_int) -> bool {
    libc::WIFSTOPPED(status) && (status >> 16) == libc::PTRACE_EVENT_STOP
}

fn set_debugreg(tid: i32, n: usize, value: u64) -> bool {
    ptrace(libc::PTRACE_POKEUSER, tid, DEBUGREG + n * 8, value as usize) == 0
}

fn debugreg(tid: i32, n: usize) -> u64 {
    ptrace(libc::PTRACE_PEEKUSER, tid, DEBUGREG + n * 8, 0) as u64
}

impl Tracer {
    pub fn attach(pid: u32) -> io::Result<Self> {
        refuse_when_stopping()?;
        let tids: Vec<i32> = fs::read_dir(format!("/proc/{pid}/task"))?
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .filter(|&tid| ptrace(libc::PTRACE_SEIZE, tid, 0, 0) == 0)
            .collect();
        if tids.is_empty() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { tids, parked: HashSet::new() })
    }

    /// Stops one thread. Signals that arrive meanwhile are passed on to it. Never waits on a
    /// thread that has died, nor longer than `STOP_WAIT`: the game's main thread, dead while
    /// others aren't collected yet, is never reported, and those others can only be collected by
    /// this same tracer. Borderlands 4 quit during a limiter re-check, and the helper waited on
    /// its main thread for hours, holding the game's process (and Ferret) open.
    fn stop(&self, tid: i32) -> bool {
        if dead(tid) || ptrace(libc::PTRACE_INTERRUPT, tid, 0, 0) != 0 {
            // A dead thread other than the main one is collected here (it stays otherwise).
            wait(tid, libc::WNOHANG);
            return false;
        }
        let start = Instant::now();
        loop {
            let mut status = 0;
            let r = unsafe { libc::waitpid(tid, &mut status, libc::WNOHANG | libc::__WALL) };
            if r == 0 {
                if dead(tid) || start.elapsed() >= STOP_WAIT {
                    return false;
                }
                child_stopped(Duration::from_millis(5));
                continue;
            }
            match (r > 0).then_some((r, status)) {
                Some((_, st)) if is_event_stop(st) => return true,
                Some((_, st)) if libc::WIFSTOPPED(st) => {
                    let sig = libc::WSTOPSIG(st);
                    if sig == libc::SIGTRAP && debugreg(tid, 6) & DR6_HITS != 0 {
                        // Our own breakpoint: never pass it on to the game.
                        set_debugreg(tid, 6, 0);
                        set_debugreg(tid, 7, 0);
                        ptrace(libc::PTRACE_CONT, tid, 0, 0);
                    } else {
                        ptrace(libc::PTRACE_CONT, tid, 0, sig as usize);
                    }
                }
                _ => return false,
            }
        }
    }

    /// Sets breakpoint 0 on every thread.
    pub fn arm(&mut self, addr: u64, dr7: u64) -> usize {
        self.arm_slots(&[addr], dr7)
    }

    /// Execute breakpoints on up to 4 instructions at once (the CPU has 4 slots); a hit's
    /// `slot` is the instruction's index.
    pub fn arm_execute(&mut self, instrs: &[u64]) -> usize {
        let dr7 = (0..instrs.len().min(4)).fold(0, |dr7, n| dr7 | DR7_EXECUTE << (2 * n));
        self.arm_slots(instrs, dr7)
    }

    /// Breakpoints on up to 4 addresses with a DR7 of the caller's (data watchpoints of mixed
    /// sizes).
    pub fn arm_slots(&mut self, addrs: &[u64], dr7: u64) -> usize {
        let tids = std::mem::take(&mut self.tids);
        for tid in tids {
            let set = |tid| addrs.iter().take(4).enumerate().all(|(n, &a)| set_debugreg(tid, n, a));
            if self.stop(tid) && set(tid) && set_debugreg(tid, 6, 0) && set_debugreg(tid, 7, dr7) {
                ptrace(libc::PTRACE_CONT, tid, 0, 0);
                self.tids.push(tid);
            } else {
                ptrace(libc::PTRACE_DETACH, tid, 0, 0);
            }
        }
        self.tids.len()
    }

    /// Runs the game until `timeout`, calling `on_hit` for each breakpoint hit.
    /// When `on_hit` returns false, the thread stays stopped and waiting ends.
    pub fn watch(&mut self, timeout: Duration, on_hit: impl FnMut(&Hit) -> bool) {
        let start = Instant::now();
        self.watch_until(|| start.elapsed() >= timeout, on_hit)
    }

    /// Lets the game run, calling `on_hit` at every breakpoint hit (false = stop), until `done`
    /// or the game's threads are gone.
    pub fn watch_until(&mut self, mut done: impl FnMut() -> bool, mut on_hit: impl FnMut(&Hit) -> bool) {
        while !done() && !stopping() && !self.tids.is_empty() {
            let Some((tid, st)) = wait(-1, libc::WNOHANG) else {
                child_stopped(Duration::from_millis(5));
                continue;
            };
            if libc::WIFEXITED(st) || libc::WIFSIGNALED(st) {
                self.tids.retain(|&t| t != tid);
                continue;
            }
            if !libc::WIFSTOPPED(st) {
                continue;
            }
            if is_event_stop(st) {
                ptrace(libc::PTRACE_CONT, tid, 0, 0);
                continue;
            }
            let sig = libc::WSTOPSIG(st);
            let dr6 = if sig == libc::SIGTRAP { debugreg(tid, 6) & DR6_HITS } else { 0 };
            if dr6 == 0 {
                ptrace(libc::PTRACE_CONT, tid, 0, sig as usize);
                continue;
            }
            set_debugreg(tid, 6, 0);
            let mut r: libc::user_regs_struct = unsafe { std::mem::zeroed() };
            ptrace(libc::PTRACE_GETREGS, tid, 0, &mut r as *mut libc::user_regs_struct as usize);
            let gpr = [r.rax, r.rcx, r.rdx, r.rbx, r.rsp, r.rbp, r.rsi, r.rdi, r.r8, r.r9, r.r10, r.r11, r.r12, r.r13, r.r14, r.r15];
            // A 32-bit Windows game under Wine runs in a 32-bit code segment, even inside a
            // 64-bit process.
            let regs = Regs { gpr, rip: r.rip, is64: r.cs == 0x33 };
            if !on_hit(&Hit { regs, slot: dr6.trailing_zeros() as usize }) {
                self.parked.insert(tid);
                return;
            }
            ptrace(libc::PTRACE_CONT, tid, 0, 0);
        }
    }
}

impl Drop for Tracer {
    fn drop(&mut self) {
        for &tid in &self.tids {
            if self.parked.contains(&tid) || self.stop(tid) {
                set_debugreg(tid, 7, 0);
                for n in 0..4 {
                    set_debugreg(tid, n, 0);
                }
                ptrace(libc::PTRACE_DETACH, tid, 0, 0);
            }
        }
    }
}

/// Waits until a traced thread stops (SIGCHLD) or `most` passes. Every signal the game gets
/// while traced stops its thread until the helper passes it on: Minecraft Dungeons (Proton)
/// raises ~900 SIGILL a second on one thread and hundreds of SIGSYS (Wine's system calls) on
/// others, and polling every 1 ms held each of them up to a millisecond or more, so the thread
/// barely ran during a 2 s re-check and the game stuttered every 8 s. SIGCHLD is blocked in
/// every thread of the helper from the start (`block_signals`), so it waits here, pending.
fn child_stopped(most: Duration) {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGCHLD);
        let ts = libc::timespec { tv_sec: most.as_secs() as libc::time_t, tv_nsec: most.subsec_nanos() as libc::c_long };
        libc::sigtimedwait(&set, std::ptr::null_mut(), &ts);
    }
}

/// Windows only: a killed ptrace tracer's breakpoints stay too, but taking them off would mean
/// tracing every thread again (the stutter `saving.md` tells of).
pub fn clear_left(_pid: u32) -> usize {
    0
}

/// Blocks SIGCHLD in the calling thread and every thread it starts later, for `child_stopped`:
/// call before any other thread starts (a thread not blocking it would take it and drop it).
pub fn block_sigchld() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGCHLD);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

// --- Watching instructions without stopping the game (perf breakpoints) ---

/// Whether `Quiet` works here: 0 not tried, 1 yes, 2 no (`perf_event_paranoid` above 2, as on
/// Ubuntu and Debian, or `FERRET_NO_PERF` set for tests).
static QUIET: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

const PERF_TYPE_BREAKPOINT: u32 = 5;
const HW_BREAKPOINT_X: u32 = 4;
const PERF_SAMPLE_TID: u64 = 1 << 1;
const PERF_SAMPLE_REGS_USER: u64 = 1 << 12;
const PERF_RECORD_SAMPLE: u32 = 9;
const PERF_EVENT_IOC_SET_OUTPUT: libc::c_ulong = 0x2405;
/// ax bx cx dx si di bp sp ip, cs, r8-r15 (asm/perf_regs.h), sampled in this order.
const SAMPLED_REGS: u64 = 0x1ff | 1 << 10 | 0xff << 16;
const PAGE: usize = 4096;
/// Pages of samples per thread (a power of two), after the one page of bookkeeping.
const RING_PAGES: usize = 2;
const DATA_HEAD: usize = 1024;
const DATA_TAIL: usize = 1032;

/// perf_event_attr up to `sample_max_stack` (PERF_ATTR_SIZE_VER5, 112 bytes).
fn breakpoint_attr(addr: u64) -> [u64; 14] {
    let mut a = [0u64; 14];
    a[0] = PERF_TYPE_BREAKPOINT as u64 | 112 << 32;
    a[2] = 1; // sample_period: every hit
    a[3] = PERF_SAMPLE_TID | PERF_SAMPLE_REGS_USER;
    a[5] = 1 << 5 | 1 << 6; // exclude_kernel, exclude_hv
    a[6] = (HW_BREAKPOINT_X as u64) << 32; // wakeup_events 0, bp_type
    a[7] = addr;
    a[8] = 8; // bp_len: sizeof(long) for execute breakpoints
    a[10] = SAMPLED_REGS;
    a
}

fn perf_open(addr: u64, tid: i32) -> io::Result<i32> {
    let attr = breakpoint_attr(addr);
    let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, attr.as_ptr(), tid, -1, -1, 0) };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(fd as i32) }
}

/// Whether instructions can be watched without stopping the game (`Quiet`).
pub fn quiet() -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    if QUIET.load(Relaxed) == 0 {
        let ok = std::env::var_os("FERRET_NO_PERF").is_none()
            && perf_open(quiet as *const () as u64, 0).map(|fd| unsafe { libc::close(fd) }).is_ok();
        QUIET.store(if ok { 1 } else { 2 }, Relaxed);
    }
    QUIET.load(Relaxed) == 1
}

struct QuietThread {
    tid: i32,
    fds: Vec<i32>,
    ring: *mut u8,
}

impl QuietThread {
    fn close(&self) {
        unsafe {
            if !self.ring.is_null() {
                libc::munmap(self.ring as *mut libc::c_void, PAGE * (1 + RING_PAGES));
            }
            for &fd in &self.fds {
                libc::close(fd);
            }
        }
    }
}

/// Execute breakpoints on up to 4 instructions in every thread of the game, through the
/// kernel's perf events: each hit leaves the registers in a buffer and the thread runs on.
/// Unlike `Tracer` the game is never stopped, its signals aren't held up, and nothing is left
/// in it when the helper dies (the kernel takes the breakpoints off with the helper's files), so
/// it can watch for as long as it takes: a value whose code runs a few times an hour (Skyrim's
/// gold) is caught the first time. It shares the CPU's 4 breakpoints per thread with `Tracer`:
/// one or the other at a time (`TRACING`).
pub struct Quiet {
    pid: u32,
    instrs: Vec<u64>,
    threads: Vec<QuietThread>,
    looked_at: Instant,
}

impl Quiet {
    pub fn open(pid: u32, instrs: &[u64]) -> io::Result<Self> {
        refuse_when_stopping()?;
        if !quiet() {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "perf breakpoints aren't allowed here"));
        }
        // One file per thread and instruction: games have hundreds of threads.
        unsafe {
            let mut lim: libc::rlimit = std::mem::zeroed();
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 && lim.rlim_cur < lim.rlim_max {
                lim.rlim_cur = lim.rlim_max;
                libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
            }
        }
        let mut q = Self { pid, instrs: instrs.iter().take(4).copied().collect(), threads: Vec::new(), looked_at: Instant::now() };
        q.add_threads()?;
        match q.threads.is_empty() {
            true => Err(io::Error::new(io::ErrorKind::NotFound, "the game has no threads")),
            false => Ok(q),
        }
    }

    /// Breakpoints for the threads that don't have them yet (the game starts new ones). A thread
    /// that ended meanwhile is skipped; any other failure is an error (the caller falls back to
    /// `Tracer`).
    fn add_threads(&mut self) -> io::Result<()> {
        self.looked_at = Instant::now();
        let tids: Vec<i32> = fs::read_dir(format!("/proc/{}/task", self.pid))?.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect();
        self.threads.retain(|t| {
            let alive = tids.contains(&t.tid);
            if !alive {
                t.close();
            }
            alive
        });
        for tid in tids {
            if self.threads.iter().any(|t| t.tid == tid) {
                continue;
            }
            let mut t = QuietThread { tid, fds: Vec::new(), ring: std::ptr::null_mut() };
            let mut failed = None;
            for &instr in &self.instrs {
                match perf_open(instr, tid) {
                    Ok(fd) => t.fds.push(fd),
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
                let fd = *t.fds.last().unwrap();
                if t.fds.len() == 1 {
                    let ring = unsafe { libc::mmap(std::ptr::null_mut(), PAGE * (1 + RING_PAGES), libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
                    if ring == libc::MAP_FAILED {
                        failed = Some(io::Error::last_os_error());
                        break;
                    }
                    t.ring = ring as *mut u8;
                } else if unsafe { libc::ioctl(fd, PERF_EVENT_IOC_SET_OUTPUT, t.fds[0]) } != 0 {
                    failed = Some(io::Error::last_os_error());
                    break;
                }
            }
            match failed {
                None => self.threads.push(t),
                Some(e) => {
                    t.close();
                    if e.raw_os_error() != Some(libc::ESRCH) {
                        return Err(e);
                    }
                }
            }
        }
        Ok(())
    }

    /// The hits a thread's buffer holds, oldest first.
    fn drain(t: &QuietThread, instrs: &[u64], hits: &mut Vec<Hit>) {
        let size = (PAGE * RING_PAGES) as u64;
        unsafe {
            let head = std::ptr::read_volatile(t.ring.add(DATA_HEAD) as *const u64);
            std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
            let mut tail = std::ptr::read_volatile(t.ring.add(DATA_TAIL) as *const u64);
            let byte = |at: u64| *t.ring.add(PAGE + (at % size) as usize);
            let word = |at: u64| u64::from_le_bytes(std::array::from_fn(|i| byte(at + i as u64)));
            while tail < head {
                let header = word(tail);
                let (kind, len) = (header as u32, header >> 48);
                if len == 0 {
                    break;
                }
                // header, pid + tid, abi, 18 registers (abi 0: none were taken)
                if kind == PERF_RECORD_SAMPLE && len >= 8 * 21 && word(tail + 16) != 0 {
                    let v: [u64; 18] = std::array::from_fn(|i| word(tail + 24 + 8 * i as u64));
                    let gpr = [v[0], v[2], v[3], v[1], v[7], v[6], v[4], v[5], v[10], v[11], v[12], v[13], v[14], v[15], v[16], v[17]];
                    if let Some(slot) = instrs.iter().position(|&i| i == v[8]) {
                        hits.push(Hit { regs: Regs { gpr, rip: v[8], is64: v[9] == 0x33 }, slot });
                    }
                }
                tail += len;
            }
            std::ptr::write_volatile(t.ring.add(DATA_TAIL) as *mut u64, tail);
        }
    }

    /// Calls `on_hit` for each hit (false = stop), until `done`. The game runs all along, so the
    /// registers are the hit's but memory is read a moment later.
    pub fn watch_until(&mut self, mut done: impl FnMut() -> bool, mut on_hit: impl FnMut(&Hit) -> bool) {
        let mut hits = Vec::new();
        while !done() && !stopping() && !self.threads.is_empty() {
            for t in &self.threads {
                Self::drain(t, &self.instrs, &mut hits);
            }
            for hit in hits.drain(..) {
                if !on_hit(&hit) {
                    return;
                }
            }
            if self.looked_at.elapsed() >= Duration::from_millis(500) && self.add_threads().is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        for t in &self.threads {
            t.close();
        }
    }
}
