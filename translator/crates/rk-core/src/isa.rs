use std::sync::OnceLock;

use crate::ir::{Operand, Repeat};
use crate::{mask, parse_hex, Addr, Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isa {
    X86,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub isa: Isa,
    pub address_bytes: u8,
    pub endian: Endian,
}

impl Target {
    pub const X86_32: Self = Self {
        isa: Isa::X86,
        address_bytes: 4,
        endian: Endian::Little,
    };

    pub fn address_mask(self) -> u64 {
        mask(u32::from(self.address_bytes))
    }

    pub fn wrap(self, value: u64) -> Addr {
        value & self.address_mask()
    }

    pub fn hex(self, addr: Addr) -> String {
        format!(
            "{addr:0width$x}",
            width = 2 * usize::from(self.address_bytes)
        )
    }

    pub fn parse_addr(self, text: &str) -> Option<Addr> {
        parse_hex(text).filter(|&addr| addr <= self.address_mask())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReturnAddress {
    Stack,
    Register(&'static str),
}

pub struct Registers {
    pub stack_pointer: &'static str,
    pub frame_pointer: &'static str,
    pub program_counter: &'static str,
    pub thread_base: &'static str,
    pub return_address: ReturnAddress,
    pub diagnostic: &'static [&'static str],
    pub register_stack: &'static [&'static str],
    pub vector: &'static [&'static str],
}

pub struct Spec {
    pub processor: &'static str,
    pub sla: &'static [u8],
    pub registers: Registers,
    pub no_op: fn(&[u8]) -> bool,
    pub internal_flow_allowed: fn(&[u8]) -> bool,
}

#[derive(Clone, Debug)]
pub struct Decoded {
    pub length: u8,
    pub address_size: u8,
    pub mnemonic: String,
    pub operands: Vec<Operand>,
    pub repeat: Option<Repeat>,
    pub text: String,
}

pub type Reg = u16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    IndexLoad {
        dest: Reg,
        source: Option<Reg>,
        table: Addr,
    },
    Compare {
        reg: Reg,
        bound: u32,
    },
    Exclusive,
    Inclusive,
    Other,
}

#[derive(Clone, Debug)]
pub struct Shape {
    pub length: u8,
    pub step: Step,
    pub written: Option<Reg>,
    pub pointer: Option<Addr>,
    pub slot: Option<Addr>,
    pub table_jump: Option<(Reg, Addr)>,
    pub immediate: Option<(u8, Addr)>,
    pub displacement: Option<(u8, Addr)>,
    pub counter: bool,
    pub padding: bool,
}

pub trait Decoder: Sync {
    fn decode(&self, addr: Addr, bytes: &[u8]) -> Option<Decoded>;
    fn shape(&self, addr: Addr, bytes: &[u8]) -> Option<Shape>;
    fn signature(&self, addr: Addr, bytes: &[u8]) -> Option<String>;
}

pub struct Machine {
    pub target: Target,
    pub max_length: u8,
    pub spec: Spec,
    pub decoder: &'static dyn Decoder,
}

static MACHINE: OnceLock<&'static Machine> = OnceLock::new();

pub fn install(machine: &'static Machine) -> Result<()> {
    let installed = *MACHINE.get_or_init(|| machine);
    if installed.target == machine.target {
        Ok(())
    } else {
        Err(Error::input(format!(
            "conflicting guest targets: {:?} and {:?}",
            installed.target, machine.target
        )))
    }
}

pub fn machine() -> &'static Machine {
    MACHINE
        .get()
        .copied()
        .expect("guest machine is not installed")
}
