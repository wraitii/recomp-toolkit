use std::sync::OnceLock;

use crate::ir::{Operand, Repeat};
use crate::program::Target;
use crate::{Addr, Error, Result};

pub struct Spec {
    pub processor: &'static str,
    pub sla: &'static [u8],
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
    pub diagnostic: &'static [&'static str],
    pub register_stack: &'static [&'static str],
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

pub trait Machine: Sync {
    fn target(&self) -> Target;
    fn spec(&self) -> Spec;
    fn registers(&self) -> &Registers;
    fn return_address(&self) -> ReturnAddress;
    fn max_length(&self) -> u8;
    fn no_op(&self, bytes: &[u8]) -> bool;
    fn internal_flow_allowed(&self, bytes: &[u8]) -> bool;
    fn decode(&self, addr: Addr, bytes: &[u8]) -> Option<Decoded>;
    fn shape(&self, addr: Addr, bytes: &[u8]) -> Option<Shape>;
    fn signature(&self, addr: Addr, bytes: &[u8]) -> Option<String>;
}

static MACHINE: OnceLock<&'static dyn Machine> = OnceLock::new();

pub fn install(machine: &'static dyn Machine) -> Result<()> {
    let installed = *MACHINE.get_or_init(|| machine);
    if installed.target() == machine.target() {
        Ok(())
    } else {
        Err(Error::input(format!(
            "conflicting guest targets: {:?} and {:?}",
            installed.target(),
            machine.target()
        )))
    }
}

pub fn installed() -> Option<&'static dyn Machine> {
    MACHINE.get().copied()
}

pub fn machine() -> &'static dyn Machine {
    installed().expect("guest machine is not installed")
}

pub fn target() -> Target {
    machine().target()
}
