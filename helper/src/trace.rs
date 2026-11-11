// Hardware breakpoints on every thread of the game: "which instructions access this address"
// and "what is in the registers when this instruction runs". Nothing is written into the
// game's code. Setting them is the OS's part (`Tracer`: ptrace on Linux, the debugger API on
// Windows); working out the instruction is the same everywhere.

#[cfg(target_os = "linux")]
#[path = "trace_linux.rs"]
mod tracer;
#[cfg(windows)]
#[path = "trace_windows.rs"]
mod tracer;
pub use tracer::*;

use std::sync::atomic::{AtomicBool, Ordering};

/// The helper is being ended (the session ends, SIGTERM): traces stop at once and no new one
/// starts, so it leaves the game without breakpoints soon enough.
static STOPPING: AtomicBool = AtomicBool::new(false);

pub fn stop_all() {
    STOPPING.store(true, Ordering::SeqCst);
}

/// Anti-cheat started in the game while a command runs: traces end at once (and none starts)
/// until the next command has dropped the session.
static LETTING_GO: AtomicBool = AtomicBool::new(false);

pub fn let_go(on: bool) {
    LETTING_GO.store(on, Ordering::SeqCst);
}

pub fn letting_go() -> bool {
    LETTING_GO.load(Ordering::Relaxed)
}

fn stopping() -> bool {
    STOPPING.load(Ordering::Relaxed) || letting_go()
}

fn refuse_when_stopping() -> std::io::Result<()> {
    match stopping() {
        true => Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "the helper is ending")),
        false => Ok(()),
    }
}

/// Watches up to 4 instructions until `done`, calling `on_hit` at each run of one (false =
/// stop): without stopping the game where that works (Linux's `Quiet`), else with `Tracer`.
#[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
pub fn watch_execute(pid: u32, instrs: &[u64], mut done: impl FnMut() -> bool, mut on_hit: impl FnMut(&Hit) -> bool) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    if let Ok(mut quiet) = Quiet::open(pid, instrs) {
        quiet.watch_until(&mut done, &mut on_hit);
        return Ok(());
    }
    let mut tracer = Tracer::attach(pid)?;
    tracer.arm_execute(instrs);
    tracer.watch_until(done, on_hit);
    Ok(())
}

pub const DR7_ACCESS_4: u64 = 1 | (3 << 16) | (3 << 18);
pub const DR7_ACCESS_2: u64 = 1 | (3 << 16) | (1 << 18);
pub const DR7_EXECUTE: u64 = 1;

/// Debug status bits of breakpoints 0-3: set when one of ours fired.
const DR6_HITS: u64 = 0xf;

/// A thread's registers at a breakpoint: the general ones in x86 encoding order (eax/rax,
/// ecx, ... r15), the instruction pointer, and whether it ran 64-bit code.
#[derive(Clone, Copy, Default, Debug)]
pub struct Regs {
    pub gpr: [u64; 16],
    pub rip: u64,
    pub is64: bool,
}

pub struct Hit {
    pub regs: Regs,
    /// Which breakpoint fired (0-3, as armed).
    pub slot: usize,
}

/// Register number (x86 encoding order) to its value.
pub fn reg(regs: &Regs, n: u8) -> u64 {
    regs.gpr[(n & 15) as usize]
}

/// Saved in profiles; r8 to r15 only exist in 64-bit code.
pub const REG_NAMES: [&str; 16] =
    ["eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];

/// Whether the thread was running 64-bit code.
pub fn is_64bit(regs: &Regs) -> bool {
    regs.is64
}

/// A memory access of the form [base register + displacement] (displacement may be 0). For
/// [base + index * scale + displacement] the displacement is where it went from the base this
/// time (target - base): arrays indexed by item type keep each item at its own place in the
/// object (Quake II's `client->pers.inventory[item]`, `dec [rcx+rax*4+0x2e8]`), so the same
/// instruction finds this item again from the object.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Access {
    pub start: u64,
    pub len: usize,
    pub base: u8,
    pub disp: i64,
}

// Opcodes with a ModRM byte, and the size of the immediate that follows the displacement
// (`reg`: the ModRM reg field, which picks the instruction in the F6/F7 group).
fn one_byte_opcode(op: u8, reg: u8) -> Option<usize> {
    match op {
        0x01 | 0x03 | 0x09 | 0x0B | 0x11 | 0x13 | 0x19 | 0x1B | 0x21 | 0x23 | 0x29 | 0x2B | 0x31 | 0x33 | 0x39
        | 0x3B | 0x63 | 0x85 | 0x87 | 0x89 | 0x8B | 0x8D | 0xFF => Some(0),
        // The same on one byte (a flag tested with `cmp byte [r+d],0`), and shifts by 1 or cl.
        0x00 | 0x02 | 0x08 | 0x0A | 0x20 | 0x22 | 0x28 | 0x2A | 0x30 | 0x32 | 0x38 | 0x3A | 0x84 | 0x86 | 0x88 | 0x8A | 0xFE => Some(0),
        0xD0..=0xD3 => Some(0),
        // x87: fld/fst/fadd/fcomp ... on floats (D8, D9) and doubles (DC, DD), integers (DA, DB, DE, DF).
        0xD8..=0xDF => Some(0),
        0x80 | 0x83 | 0x6B | 0xC0 | 0xC1 | 0xC6 => Some(1),
        0x81 | 0xC7 | 0x69 => Some(4),
        // test (/0, /1) takes an immediate; not, neg, mul, imul, div, idiv don't.
        0xF6 => Some(if reg < 2 { 1 } else { 0 }),
        0xF7 => Some(if reg < 2 { 4 } else { 0 }),
        _ => None,
    }
}

/// The immediate after a two-byte (0F xx) opcode's operands: a compare's predicate, a shuffle's
/// order, a bit number, a shift count.
fn two_byte_imm(op: u8) -> usize {
    matches!(op, 0x70..=0x73 | 0xA4 | 0xAC | 0xBA | 0xC2 | 0xC4 | 0xC5 | 0xC6) as usize
}

/// How the opcode before a ModRM byte is written.
#[derive(Clone, Copy, PartialEq)]
enum Form {
    /// One opcode byte.
    Plain,
    /// 0F xx.
    Two,
    /// 0F 38 xx (2) or 0F 3A xx (3, with an immediate byte).
    Three(u8),
    /// A VEX prefix (AVX: `vmovss xmm0,[rax+8]` = c5 fa 10 40 08) of 2 (C5) or 3 (C4) bytes: the
    /// opcode map (1 = 0F, 2 = 0F 38, 3 = 0F 3A), the base and index registers' fourth bits,
    /// 256-bit operands, and the legacy prefix it stands for (0 none, 1 = 66, 2 = F3, 3 = F2).
    Vex { len: usize, map: u8, b: u8, x: u8, wide: bool, pp: u8 },
}

/// How many bytes the instruction reads or writes at its memory operand, as far as that tells
/// whether a watched value inside it was touched (`mov rax,[rcx+0x20]` loads two ints at once,
/// `movups [rax+0x10],xmm0` copies 16 bytes of a struct). `prefix`: 0 none, 1 = 66, 2 = F3,
/// 3 = F2; `rex_w`: 64-bit operands.
fn access_width(form: Form, op: u8, prefix: u8, rex_w: bool) -> u64 {
    let general = if rex_w { 8 } else { 4 };
    let sse = |op: u8, prefix: u8| match op {
        // moves and arithmetic: packed (16 bytes), or one float (F3) or double (F2)
        0x10 | 0x11 | 0x51..=0x5F | 0xC2 => match prefix {
            2 => 4,
            3 => 8,
            _ => 16,
        },
        0x12 | 0x13 | 0x16 | 0x17 | 0xD6 => 8,
        0x28 | 0x29 | 0x2B | 0xE7 | 0xC6 => 16,
        0x2E | 0x2F => if prefix == 1 { 8 } else { 4 },
        0x6F | 0x7F => if prefix == 0 { 8 } else { 16 },
        0x7E => if prefix == 2 { 8 } else { general },
        0x60..=0x6D | 0x70 | 0xD0..=0xFE => 16,
        _ => general,
    };
    match form {
        Form::Plain => match op {
            0xDC | 0xDD => 8,
            0xD8..=0xDF => 4,
            _ => general,
        },
        Form::Two => sse(op, prefix),
        Form::Three(_) => 16,
        Form::Vex { map: 1, wide, pp, .. } => sse(op, pp) * if wide && sse(op, pp) == 16 { 2 } else { 1 },
        Form::Vex { wide, .. } => if wide { 32 } else { 16 },
    }
}

/// Works out the instruction that ends at `after` (a data breakpoint stops
/// right after the accessing instruction) and accessed `target`: at it, or over it with a wider
/// read or write that starts before (the place is then kept as where the value is from the
/// base register, `target - base`).
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
                let Some(op_at) = modrm_at.checked_sub(1) else { continue };
                let op = code[op_at];
                let before = |n: usize| op_at.checked_sub(n).map(|i| code[i]);
                // In 32-bit code C4 and C5 are `les` and `lds` unless the next byte's top two
                // bits are set (no register pair is a far pointer).
                let vex_ok = |next: u8| is64 || next & 0xC0 == 0xC0;
                // AVX forms of the SSE instructions that take a memory operand.
                let sse_op = matches!(op, 0x10..=0x17 | 0x28..=0x2F | 0x51..=0x7F | 0xC2 | 0xC6 | 0xD0..=0xFE);
                // Every way the bytes before the ModRM byte read as an opcode taking this
                // immediate, the longest first.
                let mut forms: Vec<Form> = Vec::new();
                if let (Some(0xC4), Some(b1), Some(b2)) = (before(3), before(2), before(1)) {
                    let map = b1 & 0x1F;
                    let imm_fits = imm == if map == 3 { 1 } else if map == 1 { two_byte_imm(op) } else { 0 };
                    if vex_ok(b1) && (1..=3).contains(&map) && (map != 1 || sse_op) && imm_fits {
                        forms.push(Form::Vex { len: 3, map, b: (!b1 >> 5) & 1, x: (!b1 >> 6) & 1, wide: b2 & 4 != 0, pp: b2 & 3 });
                    }
                }
                if let (Some(0xC5), Some(b1)) = (before(2), before(1)) {
                    if vex_ok(b1) && sse_op && imm == two_byte_imm(op) {
                        forms.push(Form::Vex { len: 2, map: 1, b: 0, x: 0, wide: b1 & 4 != 0, pp: b1 & 3 });
                    }
                }
                if let (Some(0x0F), Some(map @ (0x38 | 0x3A))) = (before(2), before(1)) {
                    if imm == (map == 0x3A) as usize {
                        forms.push(Form::Three(if map == 0x38 { 2 } else { 3 }));
                    }
                }
                if before(1) == Some(0x0F) {
                    if imm == two_byte_imm(op) {
                        forms.push(Form::Two);
                    }
                // 0x63 is movsxd (a 32-bit value into a 64-bit register, Mono's JIT) only in
                // 64-bit code.
                } else if one_byte_opcode(op, (modrm >> 3) & 7) == Some(imm) && (op != 0x63 || is64) {
                    forms.push(Form::Plain);
                }
                for form in forms {
                    let two_byte = form == Form::Two;
                    let plain = form == Form::Plain;
                    let op_start = match form {
                        Form::Plain => op_at,
                        Form::Two => op_at - 1,
                        Form::Three(_) => op_at - 2,
                        Form::Vex { len, .. } => op_at - len,
                    };
                    let vex = matches!(form, Form::Vex { .. });
                    // A REX prefix sits right before the opcode: B extends the base register, X the
                    // SIB index, R the ModRM reg field. A VEX prefix holds those bits itself and
                    // takes no prefix before it.
                    let maybe_rex = !vex && is64 && op_start >= 1 && (0x40..=0x4F).contains(&code[op_start - 1]);
                    let rexes: &[u8] = if maybe_rex { &[code[op_start - 1], 0] } else { &[0] };
                    let mut matched = Vec::new();
                    let mut clobbered = Vec::new();
                    for &rex in rexes {
                        let (b, x) = match form {
                            Form::Vex { b, x, .. } => (b, x),
                            _ => (rex & 1, (rex & 2) >> 1),
                        };
                        // SIB index 4 without REX.X means no index register.
                        let index = sib_byte.map(|s| (s >> 3) & 7 | x << 3).filter(|&i| i != 4);
                        let scale = sib_byte.map_or(1, |s| 1u64 << (s >> 6));
                        let base = low_base | b << 3;
                        // A REX prefix must come last, so only operand-size/SSE prefixes can precede it.
                        // (A 0x4x byte before those is the end of the previous instruction.)
                        let mut start = op_start - (rex != 0) as usize;
                        let mut legacy = 0;
                        let mut prefix = 0;
                        while !vex && start > 0 && legacy < 2 && matches!(code[start - 1], 0x66 | 0xF2 | 0xF3) {
                            start -= 1;
                            legacy += 1;
                            prefix = match code[start] {
                                0x66 => prefix.max(1),
                                0xF3 => 2,
                                _ => 3,
                            };
                        }
                        let dest = (modrm >> 3) & 7 | (rex & 4) << 1;
                        // Loads into a register: it holds the value afterwards, not what it held.
                        let loads = |r: u8| dest == r && if two_byte { matches!(op, 0xB6 | 0xB7 | 0xBE | 0xBF) } else { plain && matches!(op, 0x8B | 0x63 | 0x8A) };
                        let low = |a: u64| a & 0xFFFF_FFFF;
                        let from_base = target.wrapping_sub(reg(regs, base)) as i64;
                        let (hits, disp) = match index {
                            None => {
                                // How far into the access the value starts.
                                let inside = low(target.wrapping_sub(reg(regs, base).wrapping_add(disp as u64)));
                                let hits = inside < access_width(form, op, prefix, rex & 8 != 0) && (inside == 0 || !loads(base));
                                (hits, if hits { disp + inside as i64 } else { disp })
                            }
                            Some(i) => {
                                let step = from_base.wrapping_sub(disp);
                                let fits = step.rem_euclid(scale as i64) == 0 && step.unsigned_abs() < 1 << 24;
                                // A loaded-over index can't be checked; the rest of the address can.
                                let same = loads(i) || low(reg(regs, i).wrapping_mul(scale).wrapping_add(disp as u64)) == low(step.wrapping_add(disp) as u64);
                                (fits && same && !loads(base), from_base)
                            }
                        };
                        let access = Access { start: code_addr + start as u64, len: end - start, base, disp };
                        if hits {
                            matched.push(access);
                        } else if index.is_none() && plain && matches!(op, 0x8B | 0x63) && dest == base {
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
        let mut regs = Regs::default();
        regs.is64 = true;
        regs.gpr[0] = 1234; // the loaded value: the base is gone
        let after = 0x1000 + code.len() as u64;
        let found = decode_access(&code, 0x1000, after, 0x5000_000c, &regs);
        assert_eq!(found, vec![Access { start: 0x100b, len: 3, base: 0, disp: 12 }]);

        // mov ecx,[rax+0xc] after the same bytes: both readings hit the target.
        let code = [0x8b, 0x54, 0x24, 0x4c, 0x8b, 0x48, 0x0c];
        regs.gpr[0] = 0x5000_0000;
        let found = decode_access(&code, 0x1000, 0x1007, 0x5000_000c, &regs);
        let starts: Vec<u64> = found.iter().map(|a| a.start).collect();
        assert_eq!(starts, vec![0x1003, 0x1004]);

        // 32-bit code has no REX prefixes.
        regs.is64 = false;
        let found = decode_access(&code, 0x1000, 0x1007, 0x5000_000c, &regs);
        assert_eq!(found, vec![Access { start: 0x1004, len: 3, base: 0, disp: 12 }]);
    }

    #[test]
    fn movsxd() {
        // Valheim (64-bit Mono): mov rax,[rbp-0x238]; movsxd rax,[rax+0x38] -- loads over its base.
        let code = [0x48, 0x8b, 0x85, 0xc8, 0xfd, 0xff, 0xff, 0x48, 0x63, 0x40, 0x38];
        let mut regs = Regs::default();
        regs.is64 = true;
        regs.gpr[0] = 21;
        let found = decode_access(&code, 0x1000, 0x100b, 0x7f00_0000_0038, &regs);
        let starts: Vec<u64> = found.iter().map(|a| a.start).collect();
        assert_eq!(starts, vec![0x1007, 0x1008]);

        // movsxd rax,[rbx+0x38]
        let code = [0x3b, 0xc1, 0x7c, 0x07, 0x48, 0x63, 0x43, 0x38];
        regs.gpr[3] = 0x7f00_0000_0000;
        let found = decode_access(&code, 0x1000, 0x1008, 0x7f00_0000_0038, &regs);
        assert_eq!(found[0], Access { start: 0x1004, len: 4, base: 3, disp: 0x38 });

        // 0x63 is arpl in 32-bit code.
        regs.is64 = false;
        assert!(decode_access(&code, 0x1000, 0x1008, 0x7f00_0000_0038, &regs).is_empty());
    }

    #[test]
    fn indexed() {
        // Quake II RTX firing the shotgun: movsxd rax,[rcx+0xdc8]; dec dword [rcx+rax*4+0x2e8]
        // (the shells in client->pers.inventory[item]): kept as the place from the object.
        let code = [0x48, 0x63, 0x81, 0xc8, 0x0d, 0x00, 0x00, 0xff, 0x8c, 0x81, 0xe8, 0x02, 0x00, 0x00];
        let mut regs = Regs::default();
        regs.is64 = true;
        regs.gpr[1] = 0x24b_0000;
        regs.gpr[0] = 7;
        let target = 0x24b_0000 + 0x2e8 + 7 * 4;
        let found = decode_access(&code, 0x1000, 0x100e, target, &regs);
        assert_eq!(found, vec![Access { start: 0x1007, len: 7, base: 1, disp: 0x2e8 + 28 }]);

        // Another index reaches another place: not this one.
        regs.gpr[0] = 8;
        assert!(decode_access(&code, 0x1000, 0x100e, target, &regs).is_empty());

        // movzx eax, word [rdx+rax*4+0x2e8] loads over its index: the rest still has to fit.
        let code = [0x0f, 0xb7, 0x84, 0x82, 0xe8, 0x02, 0x00, 0x00];
        regs.gpr[2] = 0x24b_0000;
        regs.gpr[0] = 0x55;
        let found = decode_access(&code, 0x1000, 0x1008, target, &regs);
        assert_eq!(found, vec![Access { start: 0x1000, len: 8, base: 2, disp: 0x2e8 + 28 }]);
        assert!(decode_access(&code, 0x1000, 0x1008, target + 2, &regs).is_empty());
    }

    fn regs64(base: usize, value: u64) -> Regs {
        let mut regs = Regs::default();
        regs.is64 = true;
        regs.gpr[base] = value;
        regs
    }

    fn one(code: &[u8], target: u64, regs: &Regs) -> Vec<Access> {
        decode_access(code, 0x1000, 0x1000 + code.len() as u64, target, regs)
    }

    #[test]
    fn avx() {
        // vmovss xmm0,[rax+8]
        let regs = regs64(0, 0x5000_0000);
        assert_eq!(one(&[0xc5, 0xfa, 0x10, 0x40, 0x08], 0x5000_0008, &regs), vec![Access { start: 0x1000, len: 5, base: 0, disp: 8 }]);
        // vaddsd xmm1,xmm1,[rbx+0x10] after an unrelated byte
        let regs = regs64(3, 0x5000_0000);
        assert_eq!(one(&[0x90, 0xc5, 0xf3, 0x58, 0x4b, 0x10], 0x5000_0010, &regs), vec![Access { start: 0x1001, len: 5, base: 3, disp: 0x10 }]);
        // vmovss xmm0,[r8+8]: the 3-byte prefix holds the base register's fourth bit, inverted
        let regs = regs64(8, 0x5000_0000);
        assert_eq!(one(&[0xc4, 0xc1, 0x7a, 0x10, 0x40, 0x08], 0x5000_0008, &regs), vec![Access { start: 0x1000, len: 6, base: 8, disp: 8 }]);
        // vcmpltss xmm0,xmm0,[rax+8]: an immediate after the displacement
        let regs = regs64(0, 0x5000_0000);
        assert_eq!(one(&[0xc5, 0xfa, 0xc2, 0x40, 0x08, 0x01], 0x5000_0008, &regs), vec![Access { start: 0x1000, len: 6, base: 0, disp: 8 }]);
        // 32-bit code: c5 f8 ... is VEX (top bits set), c5 40 08 is `lds eax,[eax+8]`
        let mut regs32 = regs64(0, 0x5000_0000);
        regs32.is64 = false;
        assert_eq!(one(&[0xc5, 0xfa, 0x10, 0x40, 0x08], 0x5000_0008, &regs32), vec![Access { start: 0x1000, len: 5, base: 0, disp: 8 }]);
        assert!(one(&[0x90, 0xc5, 0x40, 0x10, 0x40, 0x08], 0x5000_0008, &regs32).is_empty());
    }

    #[test]
    fn immediates_after_two_byte_opcodes() {
        // cmpss xmm0,[rbx+8],1 and bt dword [rbx+8],3
        let regs = regs64(3, 0x5000_0000);
        assert_eq!(one(&[0xf3, 0x0f, 0xc2, 0x43, 0x08, 0x01], 0x5000_0008, &regs), vec![Access { start: 0x1000, len: 6, base: 3, disp: 8 }]);
        assert_eq!(one(&[0x0f, 0xba, 0x63, 0x08, 0x03], 0x5000_0008, &regs), vec![Access { start: 0x1000, len: 5, base: 3, disp: 8 }]);
        // roundss xmm0,[rbx+8],4 (66 0f 3a 0a)
        assert_eq!(one(&[0x66, 0x0f, 0x3a, 0x0a, 0x43, 0x08, 0x04], 0x5000_0008, &regs), vec![Access { start: 0x1000, len: 7, base: 3, disp: 8 }]);
    }

    #[test]
    fn group_opcodes_and_bytes() {
        let regs = regs64(3, 0x5000_0000);
        // neg dword [rbx+8]; test dword [rbx+8],0x100; shl dword [rbx+8],1; cmp byte [rbx+8],0
        assert_eq!(one(&[0xf7, 0x5b, 0x08], 0x5000_0008, &regs), vec![Access { start: 0x1000, len: 3, base: 3, disp: 8 }]);
        assert_eq!(one(&[0xf7, 0x43, 0x08, 0x00, 0x01, 0x00, 0x00], 0x5000_0008, &regs), vec![Access { start: 0x1000, len: 7, base: 3, disp: 8 }]);
        assert_eq!(one(&[0xd1, 0x63, 0x08], 0x5000_0008, &regs), vec![Access { start: 0x1000, len: 3, base: 3, disp: 8 }]);
        assert_eq!(one(&[0x80, 0x7b, 0x08, 0x00], 0x5000_0008, &regs), vec![Access { start: 0x1000, len: 4, base: 3, disp: 8 }]);
    }

    #[test]
    fn wider_than_the_value() {
        // mov rax,[rcx+0x20] loads the int at +0x24 too; mov eax,[rcx+0x20] doesn't.
        let regs = regs64(1, 0x5000_0000);
        assert_eq!(one(&[0x48, 0x8b, 0x41, 0x20], 0x5000_0024, &regs)[0], Access { start: 0x1000, len: 4, base: 1, disp: 0x24 });
        assert!(one(&[0x90, 0x8b, 0x41, 0x20], 0x5000_0024, &regs).is_empty());
        // movups [rax+0x10],xmm0 copies 16 bytes: the value at +0x18 is inside, +0x20 isn't.
        let regs = regs64(0, 0x5000_0000);
        assert_eq!(one(&[0x0f, 0x11, 0x40, 0x10], 0x5000_0018, &regs), vec![Access { start: 0x1000, len: 4, base: 0, disp: 0x18 }]);
        assert!(one(&[0x0f, 0x11, 0x40, 0x10], 0x5000_0020, &regs).is_empty());
        // movss xmm0,[rax+0x10] reads 4 bytes only.
        assert!(one(&[0xf3, 0x0f, 0x10, 0x40, 0x10], 0x5000_0014, &regs).is_empty());
        // vmovups ymm0,[rax+0x10]: 32 bytes.
        assert_eq!(one(&[0xc5, 0xfc, 0x10, 0x40, 0x10], 0x5000_002c, &regs), vec![Access { start: 0x1000, len: 5, base: 0, disp: 0x2c }]);
    }
}
