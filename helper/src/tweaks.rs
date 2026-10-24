//! One-click tweaks (helper `tweaks`, the Browse tab's Tweaks): named changes an engine's own
//! code honours in every game made with it, as the places they change and how. Each engine
//! finds its own (js.rs: RPG Maker; ue.rs: Unreal); the frontend saves them by name and keeps
//! them like any value.

use std::io::{self, Write};

/// What a tweak does to one place.
pub enum How {
    /// Kept from going down: at least the most it has held since the tweak was turned on (HP
    /// that healing brings up to full stays full).
    Rise,
    /// Kept at `on`; turned off, set to `off` (else back to what it held before).
    Set { on: f64, off: Option<f64> },
}

pub struct Part {
    pub addr: u64,
    /// The helper's value type ("jsbool", "bit1", "smi", "u8"...).
    pub kind: &'static str,
    pub how: How,
}

pub struct Tweak {
    /// Unreal: godmode, fly; RPG Maker: heroes (its god mode), walls, battles, speed.
    pub id: &'static str,
    pub parts: Vec<Part>,
}

/// "tweak <id>", then a line per part: "part <hex>:<type> rise" or "part <hex>:<type> set <on>
/// [<off>]".
pub fn print(out: &mut impl Write, tweaks: &[Tweak]) -> io::Result<()> {
    for t in tweaks.iter().filter(|t| !t.parts.is_empty()) {
        writeln!(out, "tweak {}", t.id)?;
        for p in &t.parts {
            match p.how {
                How::Rise => writeln!(out, "part 0x{:x}:{} rise", p.addr, p.kind)?,
                How::Set { on, off: Some(off) } => writeln!(out, "part 0x{:x}:{} set {on} {off}", p.addr, p.kind)?,
                How::Set { on, off: None } => writeln!(out, "part 0x{:x}:{} set {on}", p.addr, p.kind)?,
            }
        }
    }
    Ok(())
}
