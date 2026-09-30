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
        7 => regs.rdi,
        8 => regs.r8,
        9 => regs.r9,
        10 => regs.r10,
        11 => regs.r11,
        12 => regs.r12,
        13 => regs.r13,
        14 => regs.r14,
        _ => regs.r15,
    }
}

/// Saved in profiles; r8 to r15 only exist in 64-bit code.
pub const REG_NAMES: [&str; 16] =
    ["eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];

/// Whether the thread was running 64-bit code (a 32-bit Windows game under Wine runs in a
/// 32-bit code segment, even inside a 64-bit process).
pub fn is_64bit(regs: &Regs) -> bool {
    regs.cs == 0x33
}

/// A memory access of the form [base register + displacement] (displacement may be 0).
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

/// Works out the instruction that ends at `after` (a data breakpoint stops
/// right after the accessing instruction) and accessed `target`.
/// `code` holds the bytes from `code_addr`.
pub fn decode_access(code: &[u8], code_addr: u64, after: u64, target: u64, regs: &Regs) -> Option<Access> {
    let end = (after - code_addr) as usize;
    let is64 = is_64bit(regs);
    let mut found = None;
    // (displacement size, ModRM mode): [reg+disp32], [reg+disp8], [reg]
    for (disp_size, want_mode) in [(4usize, 2u8), (1, 1), (0, 0)] {
        for sib in [false, true] {
            for imm in [0usize, 1, 4] {
                let Some(disp_start) = end.checked_sub(imm + disp_size) else { continue };
                let Some(modrm_at) = disp_start.checked_sub(1 + sib as usize) else { continue };
                let modrm = code[modrm_at];
                let (mode, rm) = (modrm >> 6, modrm & 7);
                if mode != want_mode || ((rm == 4) != sib) {
                    continue;
                }
                // Mode 0 with rm 5 is an absolute (32-bit) or RIP-relative (64-bit) address.
                if mode == 0 && rm == 5 {
                    continue;
                }
                let sib_byte = sib.then(|| code[disp_start - 1]);
                if let Some(s) = sib_byte {
                    // Mode 0 with SIB base 5 has no base register, only a disp32.
                    if mode == 0 && s & 7 == 5 {
                        continue;
                    }
                }
                let low_base = sib_byte.map_or(rm, |s| s & 7);
                let d = &code[disp_start..disp_start + disp_size];
                let disp = match disp_size {
                    4 => i32::from_le_bytes([d[0], d[1], d[2], d[3]]) as i64,
                    1 => d[0] as i8 as i64,
                    _ => 0,
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
                // A REX prefix sits right before the opcode: B extends the base register, X the
                // SIB index, R the ModRM reg field.
                let rex = if is64 && start >= 1 && (0x40..=0x4F).contains(&code[start - 1]) { code[start - 1] } else { 0 };
                if let Some(s) = sib_byte {
                    if (s >> 3) & 7 != 4 || rex & 0x02 != 0 {
                        continue; // indexed addressing is out of scope here
                    }
                }
                let base = low_base | (rex & 1) << 3;
                // A REX prefix must come last, so only operand-size/SSE prefixes can precede it.
                // (A 0x4x byte before those is the end of the previous instruction.)
                if rex != 0 {
                    start -= 1;
                }
                let mut legacy = 0;
                while start > 0 && legacy < 2 && matches!(code[start - 1], 0x66 | 0xF2 | 0xF3) {
                    start -= 1;
                    legacy += 1;
                }
                let base_value = reg(regs, base);
                let dest = (modrm >> 3) & 7 | (rex & 4) << 1;
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
