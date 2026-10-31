//! Shared vocabulary of the translator: guest addresses, the lifted p-code IR (`ir`), CPU-field
//! call contracts (`contract`), the program model (`program`), named diagnostics (`diag`), the
//! resolved translation settings (`settings`), the validated `recomp-analysis-facts` format
//! (`facts`), and lookup and phase-timing helpers. Output must not depend on hashing order, so
//! `clippy.toml` bans hash maps and `lookup` offers maps that cannot be iterated.

pub mod contract;
pub mod diag;
pub mod facts;
pub mod flow;
pub mod ir;
pub mod isa;
pub mod lookup;
pub mod memory;
pub mod program;
pub mod settings;
pub mod timing;

pub use diag::{Diagnostic, Error, Reason, Result};
pub use lookup::{Lookup, LookupSet};

pub type Addr = u64;

pub fn wrap_addr(value: u64) -> Addr {
    value & mask(u32::from(isa::target().address_bytes))
}

pub const fn mask(size: u32) -> u64 {
    if size >= 8 {
        u64::MAX
    } else {
        (1u64 << (size * 8)) - 1
    }
}

fn address_digits() -> Option<usize> {
    isa::installed().map(|machine| 2 * usize::from(machine.target().address_bytes))
}

pub struct Hex(pub Addr);

impl std::fmt::Display for Hex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:0width$x}",
            self.0,
            width = address_digits().unwrap_or(8)
        )
    }
}

pub fn parse_hex(text: &str) -> Option<Addr> {
    let digits = text.strip_prefix("0x").unwrap_or(text);
    if digits.is_empty() || digits.len() > address_digits().unwrap_or(16) {
        return None;
    }
    Addr::from_str_radix(digits, 16).ok()
}
