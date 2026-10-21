// The tracer on Linux: debug registers set through ptrace on every thread of the game.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::time::{Duration, Instant};

use super::{Hit, Regs, DR6_HITS, DR7_EXECUTE};

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
        let tids: Vec<i32> = fs::read_dir(format!("/proc/{pid}/task"))?
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .filter(|&tid| ptrace(libc::PTRACE_SEIZE, tid, 0, 0) == 0)
            .collect();
        if tids.is_empty() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { tids, parked: HashSet::new() })
    }

    /// Stops one thread. Signals that arrive meanwhile are passed on to it.
    fn stop(&self, tid: i32) -> bool {
        if ptrace(libc::PTRACE_INTERRUPT, tid, 0, 0) != 0 {
            return false;
        }
        loop {
            match wait(tid, 0) {
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
        while !done() && !self.tids.is_empty() {
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
