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
    pub fn watch(&mut self, timeout: Duration, on_hit: impl FnMut(&Hit) -> bool) {
        let start = Instant::now();
        self.watch_until(|| start.elapsed() >= timeout, on_hit)
    }

    /// Lets the game run, calling `on_hit` at every breakpoint hit (false = stop), until `done`
    /// or the game's threads are gone.
    pub fn watch_until(&mut self, mut done: impl FnMut() -> bool, mut on_hit: impl FnMut(&Hit) -> bool) {
        while !done() && !self.tids.is_empty() {
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
        | 0x3B | 0x63 | 0x85 | 0x87 | 0x89 | 0x8B | 0x8D | 0xFF => Some(0),
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
/// In 64-bit code a 0x4x byte before the opcode may be a REX prefix or the end of the previous
/// instruction, so this returns every reading that fits, the REX one first; only the real start
/// runs (an execute breakpoint tells them apart).
pub fn decode_access(code: &[u8], code_addr: u64, after: u64, target: u64, regs: &Regs) -> Vec<Access> {
    let end = (after - code_addr) as usize;
    let is64 = is_64bit(regs);
    let mut found = Vec::new();
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
                let Some(mut op_start) = modrm_at.checked_sub(1) else { continue };
                let op = code[op_start];
                let two_byte = op_start >= 1 && code[op_start - 1] == 0x0F;
                let imm_ok = if two_byte {
                    op_start -= 1;
                    imm == 0
                } else {
                    // 0x63 is movsxd (a 32-bit value into a 64-bit register, Mono's JIT) only in
                    // 64-bit code.
                    one_byte_opcode(op) == Some(imm) && (op != 0x63 || is64)
                };
                if !imm_ok {
                    continue;
                }
                // A REX prefix sits right before the opcode: B extends the base register, X the
                // SIB index, R the ModRM reg field.
                let maybe_rex = is64 && op_start >= 1 && (0x40..=0x4F).contains(&code[op_start - 1]);
                let rexes: &[u8] = if maybe_rex { &[code[op_start - 1], 0] } else { &[0] };
                let mut matched = Vec::new();
                let mut clobbered = Vec::new();
                for &rex in rexes {
                    if let Some(s) = sib_byte {
                        if (s >> 3) & 7 != 4 || rex & 0x02 != 0 {
                            continue; // indexed addressing is out of scope here
                        }
                    }
                    let base = low_base | (rex & 1) << 3;
                    // A REX prefix must come last, so only operand-size/SSE prefixes can precede it.
                    // (A 0x4x byte before those is the end of the previous instruction.)
                    let mut start = op_start - (rex != 0) as usize;
                    let mut legacy = 0;
                    while start > 0 && legacy < 2 && matches!(code[start - 1], 0x66 | 0xF2 | 0xF3) {
                        start -= 1;
                        legacy += 1;
                    }
                    let dest = (modrm >> 3) & 7 | (rex & 4) << 1;
                    let access = Access { start: code_addr + start as u64, len: end - start, base, disp };
                    if reg(regs, base).wrapping_add(disp as u64) & 0xFFFF_FFFF == target & 0xFFFF_FFFF {
                        matched.push(access);
                    } else if !two_byte && matches!(op, 0x8B | 0x63) && dest == base {
                        // `mov reg, [reg+disp]` (or movsxd) overwrites its own base register.
                        clobbered.push(access);
                    }
                }
                if !matched.is_empty() {
                    return matched;
                }
                if found.is_empty() {
                    found = clobbered;
                }
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_before_opcode_is_not_always_rex() {
        // mov edx,[rsp+0x4c]; mov eax,[rax+0xc] -- the 0x4c is not a REX prefix of the second.
        let code = [0x48, 0x8b, 0x05, 0xbd, 0x64, 0x00, 0x00, 0x8b, 0x54, 0x24, 0x4c, 0x8b, 0x40, 0x0c];
        let mut regs: Regs = unsafe { std::mem::zeroed() };
        regs.cs = 0x33;
        regs.rax = 1234; // the loaded value: the base is gone
        let after = 0x1000 + code.len() as u64;
        let found = decode_access(&code, 0x1000, after, 0x5000_000c, &regs);
        assert_eq!(found, vec![Access { start: 0x100b, len: 3, base: 0, disp: 12 }]);

        // mov ecx,[rax+0xc] after the same bytes: both readings hit the target.
        let code = [0x8b, 0x54, 0x24, 0x4c, 0x8b, 0x48, 0x0c];
        regs.rax = 0x5000_0000;
        let found = decode_access(&code, 0x1000, 0x1007, 0x5000_000c, &regs);
        let starts: Vec<u64> = found.iter().map(|a| a.start).collect();
        assert_eq!(starts, vec![0x1003, 0x1004]);

        // 32-bit code has no REX prefixes.
        regs.cs = 0x23;
        let found = decode_access(&code, 0x1000, 0x1007, 0x5000_000c, &regs);
        assert_eq!(found, vec![Access { start: 0x1004, len: 3, base: 0, disp: 12 }]);
    }

    #[test]
    fn movsxd() {
        // Valheim (64-bit Mono): mov rax,[rbp-0x238]; movsxd rax,[rax+0x38] -- loads over its base.
        let code = [0x48, 0x8b, 0x85, 0xc8, 0xfd, 0xff, 0xff, 0x48, 0x63, 0x40, 0x38];
        let mut regs: Regs = unsafe { std::mem::zeroed() };
        regs.cs = 0x33;
        regs.rax = 21;
        let found = decode_access(&code, 0x1000, 0x100b, 0x7f00_0000_0038, &regs);
        let starts: Vec<u64> = found.iter().map(|a| a.start).collect();
        assert_eq!(starts, vec![0x1007, 0x1008]);

        // movsxd rax,[rbx+0x38]
        let code = [0x3b, 0xc1, 0x7c, 0x07, 0x48, 0x63, 0x43, 0x38];
        regs.rbx = 0x7f00_0000_0000;
        let found = decode_access(&code, 0x1000, 0x1008, 0x7f00_0000_0038, &regs);
        assert_eq!(found[0], Access { start: 0x1004, len: 4, base: 3, disp: 0x38 });

        // 0x63 is arpl in 32-bit code.
        regs.cs = 0x23;
        assert!(decode_access(&code, 0x1000, 0x1008, 0x7f00_0000_0038, &regs).is_empty());
    }
}
