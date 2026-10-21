// The tracer on Windows: the helper attaches as the game's debugger (DebugActiveProcess) for
// as long as a trace runs, sets the debug registers of every thread, and takes the
// single-step exceptions they raise. Everything else the game raises goes back to the game.
// Like ptrace, debugging belongs to the thread that attached: a Tracer is made, used and
// dropped on one thread.

use std::io;
use std::time::{Duration, Instant};

use super::{refuse_when_stopping, stopping, Hit, Regs, DR6_HITS, DR7_EXECUTE};
use crate::win32::*;

pub struct Tracer {
    pid: u32,
    /// A 32-bit game (its code raises STATUS_WX86_SINGLE_STEP; registers through WOW64).
    wow64: bool,
    tids: Vec<u32>,
    /// A hit `on_hit` said to stop at: the game stays frozen at it until the Tracer goes.
    parked: Option<u32>,
    gone: bool,
}

const ACCESS: u32 = THREAD_GET_CONTEXT | THREAD_SET_CONTEXT | THREAD_SUSPEND_RESUME | THREAD_QUERY_INFORMATION;
/// EFLAGS' resume flag: the instruction an execute breakpoint stopped at runs once without it
/// firing again.
const RF: u32 = 1 << 16;

fn thread(tid: u32) -> Option<Handle> {
    Handle::valid(unsafe { OpenThread(ACCESS, 0, tid) })
}

fn threads_of(pid: u32) -> Vec<u32> {
    let Some(snap) = Handle::valid(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else { return Vec::new() };
    let mut tids = Vec::new();
    let mut e: THREADENTRY32 = unsafe { std::mem::zeroed() };
    e.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut ok = unsafe { Thread32First(snap.0, &mut e) } != 0;
    while ok {
        if e.th32OwnerProcessID == pid {
            tids.push(e.th32ThreadID);
        }
        ok = unsafe { Thread32Next(snap.0, &mut e) } != 0;
    }
    tids
}

/// Sets a thread's debug registers (it must be stopped: suspended, or frozen at an event).
fn set_debug(t: &Handle, addrs: &[u64], dr7: u64) -> bool {
    let mut c = CONTEXT::new(CONTEXT_DEBUG_REGISTERS);
    if unsafe { GetThreadContext(t.0, &mut *c) } == 0 {
        return false;
    }
    for n in 0..4 {
        c.set_u64(CTX_DR[n], addrs.get(n).copied().unwrap_or(0));
    }
    c.set_u64(CTX_DR6, 0);
    c.set_u64(CTX_DR7, dr7);
    unsafe { SetThreadContext(t.0, &*c) != 0 }
}

fn continue_event(pid: u32, tid: u32, status: u32) {
    unsafe { ContinueDebugEvent(pid, tid, status) };
}

/// The next debug event, waiting at most `wait`.
fn next_event(wait: Duration) -> Option<DEBUG_EVENT> {
    let mut ev: DEBUG_EVENT = unsafe { std::mem::zeroed() };
    (unsafe { WaitForDebugEvent(&mut ev, wait.as_millis() as u32) } != 0).then_some(ev)
}

impl Tracer {
    pub fn attach(pid: u32) -> io::Result<Self> {
        refuse_when_stopping()?;
        let wow64 = crate::os::is_wow64(pid);
        if unsafe { DebugActiveProcess(pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // The helper ending (or being ended) must never take the game with it.
        unsafe { DebugSetProcessKillOnExit(0) };
        let mut t = Tracer { pid, wow64, tids: Vec::new(), parked: None, gone: false };
        // Attaching sends the process's, its threads' and its modules' creation, then a
        // breakpoint in a thread Windows starts for it: everything up to that is let through.
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            let Some(ev) = next_event(Duration::from_millis(100)) else { continue };
            let mut status = DBG_CONTINUE;
            let mut done = false;
            match ev.dwDebugEventCode {
                CREATE_PROCESS_DEBUG_EVENT | LOAD_DLL_DEBUG_EVENT => {
                    if !ev.file_handle().is_null() {
                        unsafe { CloseHandle(ev.file_handle()) };
                    }
                }
                EXCEPTION_DEBUG_EVENT => match ev.exception_code() {
                    EXCEPTION_BREAKPOINT | STATUS_WX86_BREAKPOINT => done = true,
                    _ => status = DBG_EXCEPTION_NOT_HANDLED,
                },
                EXIT_PROCESS_DEBUG_EVENT => {
                    t.gone = true;
                    done = true;
                }
                _ => {}
            }
            continue_event(ev.dwProcessId, ev.dwThreadId, status);
            if done {
                break;
            }
        }
        t.tids = threads_of(pid);
        if t.tids.is_empty() || t.gone {
            return Err(io::Error::new(io::ErrorKind::NotFound, "the game has no threads"));
        }
        Ok(t)
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
        let addrs: Vec<u64> = addrs.iter().take(4).copied().collect();
        self.tids.retain(|&tid| {
            let Some(t) = thread(tid) else { return false };
            if unsafe { SuspendThread(t.0) } == u32::MAX {
                return false;
            }
            let ok = set_debug(&t, &addrs, dr7);
            unsafe { ResumeThread(t.0) };
            ok
        });
        self.tids.len()
    }

    /// Runs the game until `timeout`, calling `on_hit` for each breakpoint hit.
    /// When `on_hit` returns false, the thread stays stopped and waiting ends.
    pub fn watch(&mut self, timeout: Duration, on_hit: impl FnMut(&Hit) -> bool) {
        let start = Instant::now();
        self.watch_until(|| start.elapsed() >= timeout, on_hit)
    }

    /// Lets the game run, calling `on_hit` at every breakpoint hit (false = stop), until `done`
    /// or the game is gone.
    pub fn watch_until(&mut self, mut done: impl FnMut() -> bool, mut on_hit: impl FnMut(&Hit) -> bool) {
        while !done() && !stopping() && !self.gone && self.parked.is_none() {
            let Some(ev) = next_event(Duration::from_millis(5)) else { continue };
            let (pid, tid) = (ev.dwProcessId, ev.dwThreadId);
            match ev.dwDebugEventCode {
                CREATE_PROCESS_DEBUG_EVENT | LOAD_DLL_DEBUG_EVENT => {
                    if !ev.file_handle().is_null() {
                        unsafe { CloseHandle(ev.file_handle()) };
                    }
                    continue_event(pid, tid, DBG_CONTINUE);
                }
                EXIT_PROCESS_DEBUG_EVENT => {
                    self.gone = true;
                    continue_event(pid, tid, DBG_CONTINUE);
                }
                EXIT_THREAD_DEBUG_EVENT => {
                    self.tids.retain(|&t| t != tid);
                    continue_event(pid, tid, DBG_CONTINUE);
                }
                EXCEPTION_DEBUG_EVENT => {
                    let code = ev.exception_code();
                    if code != EXCEPTION_SINGLE_STEP && code != STATUS_WX86_SINGLE_STEP {
                        // The game's own exceptions (C++ exceptions, its own breakpoints, faults it
                        // handles) go back to it.
                        continue_event(pid, tid, DBG_EXCEPTION_NOT_HANDLED);
                        continue;
                    }
                    match self.take_hit(tid, code) {
                        Some(hit) => {
                            if !on_hit(&hit) {
                                self.parked = Some(tid);
                                return;
                            }
                            continue_event(pid, tid, DBG_CONTINUE);
                        }
                        // A single step with no breakpoint in DR6: one of ours whose DR6 was
                        // cleared when the breakpoints were set again (Drop's comment). Games
                        // don't single-step themselves; passing it on would end the game.
                        None => continue_event(pid, tid, DBG_CONTINUE),
                    }
                }
                _ => continue_event(pid, tid, DBG_CONTINUE),
            }
        }
    }

    /// A single step at `tid`: one of ours when DR6 says a breakpoint fired. Clears DR6 and,
    /// for an execute breakpoint, sets RF so the instruction runs on.
    fn take_hit(&self, tid: u32, code: u32) -> Option<Hit> {
        let t = thread(tid)?;
        let mut d = CONTEXT::new(CONTEXT_DEBUG_REGISTERS);
        if unsafe { GetThreadContext(t.0, &mut *d) } == 0 {
            return None;
        }
        let dr6 = d.u64_at(CTX_DR6) & DR6_HITS;
        if dr6 == 0 {
            return None;
        }
        let slot = dr6.trailing_zeros() as usize;
        let execute = (d.u64_at(CTX_DR7) >> (16 + 4 * slot)) & 3 == 0;
        d.set_u64(CTX_DR6, 0);
        unsafe { SetThreadContext(t.0, &*d) };
        let mut gpr = [0u64; 16];
        let regs = if self.wow64 && code == STATUS_WX86_SINGLE_STEP {
            let mut w = WOW64_CONTEXT::new(WOW64_CONTEXT_CONTROL | WOW64_CONTEXT_INTEGER);
            if unsafe { Wow64GetThreadContext(t.0, &mut *w) } == 0 {
                return None;
            }
            for (n, off) in WOW_GPR.iter().enumerate() {
                gpr[n] = w.u32_at(*off) as u64;
            }
            if execute {
                w.set_u32(0, WOW64_CONTEXT_CONTROL);
                w.set_u32(WOW_EFLAGS, w.u32_at(WOW_EFLAGS) | RF);
                unsafe { Wow64SetThreadContext(t.0, &*w) };
            }
            Regs { gpr, rip: w.u32_at(WOW_EIP) as u64, is64: false }
        } else {
            let mut c = CONTEXT::new(CONTEXT_CONTROL | CONTEXT_INTEGER);
            if unsafe { GetThreadContext(t.0, &mut *c) } == 0 {
                return None;
            }
            for (n, off) in CTX_GPR.iter().enumerate() {
                gpr[n] = c.u64_at(*off);
            }
            if execute {
                c.set_u32(0x30, CONTEXT_CONTROL);
                c.set_u32(CTX_EFLAGS, c.u32_at(CTX_EFLAGS) | RF);
                unsafe { SetThreadContext(t.0, &*c) };
            }
            Regs { gpr, rip: c.u64_at(CTX_RIP), is64: true }
        };
        Some(Hit { regs, slot })
    }
}

impl Drop for Tracer {
    fn drop(&mut self) {
        if !self.gone {
            // Clear every thread's breakpoints, then let through what is still queued. Every
            // single step is ours: a thread whose hit was on its way when its registers were
            // cleared (DR6 with them) can't be told apart, and handing it to the game kills it
            // (EXCEPTION_SINGLE_STEP, unhandled: it happened to the stand-in under load). A
            // hit can take a while to arrive in a busy game, so the queue is waited on until
            // it stays empty for 100 ms.
            self.arm_slots(&[], 0);
            if let Some(tid) = self.parked.take() {
                continue_event(self.pid, tid, DBG_CONTINUE);
            }
            while let Some(ev) = next_event(Duration::from_millis(100)) {
                let status = match ev.dwDebugEventCode {
                    CREATE_PROCESS_DEBUG_EVENT | LOAD_DLL_DEBUG_EVENT => {
                        if !ev.file_handle().is_null() {
                            unsafe { CloseHandle(ev.file_handle()) };
                        }
                        DBG_CONTINUE
                    }
                    EXCEPTION_DEBUG_EVENT => match ev.exception_code() {
                        EXCEPTION_SINGLE_STEP | STATUS_WX86_SINGLE_STEP => {
                            self.take_hit(ev.dwThreadId, ev.exception_code());
                            DBG_CONTINUE
                        }
                        _ => DBG_EXCEPTION_NOT_HANDLED,
                    },
                    _ => DBG_CONTINUE,
                };
                continue_event(ev.dwProcessId, ev.dwThreadId, status);
            }
        }
        unsafe { DebugActiveProcessStop(self.pid) };
    }
}

/// Takes off breakpoints a helper left on the game's threads when it was killed in the middle
/// of a trace (Task Manager, taskkill /f): Windows detaches a dead debugger but keeps the
/// threads' debug registers, and the next hit, with no debugger to take it, ends the game.
/// Nothing is touched while a debugger is attached (a trace running in another helper).
/// Returns how many threads had some.
pub fn clear_left(pid: u32) -> usize {
    let Some(p) = Handle::valid(unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid) }) else { return 0 };
    let mut debugged = 0;
    if unsafe { CheckRemoteDebuggerPresent(p.0, &mut debugged) } == 0 || debugged != 0 {
        return 0;
    }
    let mut cleared = 0;
    for tid in threads_of(pid) {
        let Some(t) = thread(tid) else { continue };
        if unsafe { SuspendThread(t.0) } == u32::MAX {
            continue;
        }
        let mut c = CONTEXT::new(CONTEXT_DEBUG_REGISTERS);
        // DR7's enable bits, 2 per breakpoint.
        if unsafe { GetThreadContext(t.0, &mut *c) } != 0 && c.u64_at(CTX_DR7) & 0xff != 0 && set_debug(&t, &[], 0) {
            cleared += 1;
        }
        unsafe { ResumeThread(t.0) };
    }
    cleared
}

/// Linux waits for SIGCHLD; nothing to set up here.
pub fn block_sigchld() {}
