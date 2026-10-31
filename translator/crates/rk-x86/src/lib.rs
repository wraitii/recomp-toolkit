//! The x86-32 plug-in: the pinned Ghidra SLEIGH specification and its lifting quirks, the
//! Capstone structural decoder behind listings, typed operands, SEH idioms and x87 recipes, and
//! the iced-x86 operand shapes discovery reads. No other crate names Capstone, iced or the spec.

mod decode;
mod shape;

use std::cell::RefCell;

use rk_core::isa::{Decoded, Decoder, Machine, Registers, ReturnAddress, Shape, Spec, Target};
use rk_core::Addr;

pub use decode::{
    gpr32, gpr_index, imm, is_xmm, mem, MemOperand, JCC, REG16, REG32, REG8H, REG8L, STRING_MNEM,
};
pub use rk_core::ir::{Operand, OperandKind, Part, Repeat};

const WAIT: u8 = 0x9b;

pub static MACHINE: Machine = Machine {
    target: Target::X86_32,
    max_length: 15,
    spec: Spec {
        processor: sleigh_config::processor_x86::PSPEC_X86,
        sla: sleigh_config::processor_x86::SLA_X86,
        registers: Registers {
            stack_pointer: "ESP",
            frame_pointer: "EBP",
            program_counter: "EIP",
            thread_base: "FS_OFFSET",
            return_address: ReturnAddress::Stack,
            diagnostic: &[
                "FPUInstructionPointer",
                "FPUDataPointer",
                "FPULastInstructionOpcode",
                "FPUPointerSelector",
                "FPUDataSelector",
            ],
            register_stack: &["ST0", "ST1", "ST2", "ST3", "ST4", "ST5", "ST6", "ST7"],
        },
        no_op: |bytes| bytes == [WAIT],
        internal_flow_allowed: |bytes| matches!(bytes, [0xda | 0xdb, 0xc0..=0xdf]),
    },
    decoder: &X86,
};

struct X86;

thread_local! {
    static DECODER: RefCell<Option<decode::Decoder>> = const { RefCell::new(None) };
}

impl Decoder for X86 {
    fn decode(&self, addr: Addr, bytes: &[u8]) -> Option<Decoded> {
        DECODER.with(|slot| {
            let mut slot = slot.borrow_mut();
            let decoder = match slot.as_mut() {
                Some(decoder) => decoder,
                None => slot.insert(decode::Decoder::new().ok()?),
            };
            decoder.decode_span(addr, bytes)
        })
    }

    fn shape(&self, addr: Addr, bytes: &[u8]) -> Option<Shape> {
        shape::shape(addr, bytes)
    }

    fn signature(&self, addr: Addr, bytes: &[u8]) -> Option<String> {
        shape::signature(addr, bytes)
    }
}

pub fn install() -> rk_core::Result<()> {
    rk_core::isa::install(&MACHINE)
}
