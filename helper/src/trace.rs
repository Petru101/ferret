// Hardware breakpoints on every thread of the game through ptrace: "which
// instructions access this address" and "what is in the registers when this
// instruction runs". Nothing is written into the game's code.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::time::{Duration, Instant};

pub use libc::user_regs_struct as Regs;

/// offsetof(struct user, u_debugreg) on x86_64.
const DEBUGREG: usize = 848;

pub const DR7_ACCESS_4: u64 = 1 | (3 << 16) | (3 << 18);
pub const DR7_EXECUTE: u64 = 1;

pub struct Hit {
    pub regs: Regs,
}

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
                    if sig == libc::SIGTRAP && debugreg(tid, 6) & 1 != 0 {
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
        let tids = std::mem::take(&mut self.tids);
        for tid in tids {
            if self.stop(tid) && set_debugreg(tid, 0, addr) && set_debugreg(tid, 6, 0) && set_debugreg(tid, 7, dr7) {
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
    pub fn watch(&mut self, timeout: Duration, mut on_hit: impl FnMut(&Hit) -> bool) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            let Some((tid, st)) = wait(-1, libc::WNOHANG) else {
                std::thread::sleep(Duration::from_millis(1));
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
            if sig != libc::SIGTRAP || debugreg(tid, 6) & 1 == 0 {
                ptrace(libc::PTRACE_CONT, tid, 0, sig as usize);
                continue;
            }
            set_debugreg(tid, 6, 0);
            let mut regs: Regs = unsafe { std::mem::zeroed() };
            ptrace(libc::PTRACE_GETREGS, tid, 0, &mut regs as *mut Regs as usize);
            if !on_hit(&Hit { regs }) {
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
                set_debugreg(tid, 0, 0);
                ptrace(libc::PTRACE_DETACH, tid, 0, 0);
            }
        }
    }
}

/// Register number (x86 encoding order) to its value.
pub fn reg(regs: &Regs, n: u8) -> u64 {
    match n {
        0 => regs.rax,
        1 => regs.rcx,
        2 => regs.rdx,
        3 => regs.rbx,
        4 => regs.rsp,
        5 => regs.rbp,
        6 => regs.rsi,
        _ => regs.rdi,
    }
}

pub const REG_NAMES: [&str; 8] = ["eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi"];

/// A memory access of the form [base register + displacement].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Access {
    pub start: u64,
    pub len: usize,
    pub base: u8,
    pub disp: i64,
}

// Opcodes with a ModRM byte, and the size of the immediate that follows the displacement.
fn one_byte_opcode(op: u8) -> Option<usize> {
    match op {
        0x01 | 0x03 | 0x09 | 0x0B | 0x11 | 0x13 | 0x19 | 0x1B | 0x21 | 0x23 | 0x29 | 0x2B | 0x31 | 0x33 | 0x39
        | 0x3B | 0x85 | 0x87 | 0x89 | 0x8B | 0x8D | 0xFF => Some(0),
        // x87: fld/fst/fadd/fcomp ... on floats (D8, D9) and doubles (DC, DD), integers (DA, DB, DE, DF).
        0xD8..=0xDF => Some(0),
        0x83 | 0x6B | 0xC1 => Some(1),
        0x81 | 0xC7 | 0x69 => Some(4),
        _ => None,
    }
}

fn is_prefix(b: u8) -> bool {
    matches!(b, 0x66 | 0xF2 | 0xF3) || (0x40..=0x4F).contains(&b)
}

/// Works out the instruction that ends at `after` (a data breakpoint stops
/// right after the accessing instruction) and accessed `target`.
/// `code` holds the bytes from `code_addr`.
pub fn decode_access(code: &[u8], code_addr: u64, after: u64, target: u64, regs: &Regs) -> Option<Access> {
    let end = (after - code_addr) as usize;
    let mut found = None;
    for disp_size in [4usize, 1] {
        for sib in [false, true] {
            for imm in [0usize, 1, 4] {
                let disp_start = end.checked_sub(imm + disp_size)?;
                let modrm_at = disp_start.checked_sub(1 + sib as usize)?;
                let modrm = code[modrm_at];
                let (mode, rm) = (modrm >> 6, modrm & 7);
                if (disp_size == 4 && mode != 2) || (disp_size == 1 && mode != 1) || ((rm == 4) != sib) {
                    continue;
                }
                let base = if sib {
                    let s = code[disp_start - 1];
                    if (s >> 3) & 7 != 4 {
                        continue; // indexed addressing is out of scope here
                    }
                    s & 7
                } else {
                    rm
                };
                let d = &code[disp_start..disp_start + disp_size];
                let disp = if disp_size == 4 {
                    i32::from_le_bytes([d[0], d[1], d[2], d[3]]) as i64
                } else {
                    d[0] as i8 as i64
                };
                let Some(mut start) = modrm_at.checked_sub(1) else { continue };
                let op = code[start];
                let imm_ok = if start >= 1 && code[start - 1] == 0x0F {
                    start -= 1;
                    imm == 0
                } else {
                    one_byte_opcode(op) == Some(imm)
                };
                if !imm_ok {
                    continue;
                }
                while start > 0 && is_prefix(code[start - 1]) {
                    start -= 1;
                }
                let base_value = reg(regs, base);
                let dest = (modrm >> 3) & 7;
                let matches = base_value.wrapping_add(disp as u64) & 0xFFFF_FFFF == target & 0xFFFF_FFFF;
                // `mov reg, [reg+disp]` overwrites its own base register.
                let clobbered = op == 0x8B && dest == base;
                let access = Access { start: code_addr + start as u64, len: end - start, base, disp };
                if matches {
                    return Some(access);
                }
                if clobbered && found.is_none() {
                    found = Some(access);
                }
            }
        }
    }
    found
}
