//! Lowers lifted p-code into codegen IR (`CgFunction`). Lowering makes flags explicit, linearizes
//! read-modify-write instructions, corrects CMOV, CMPXCHG and SSE lane shapes, identifies string
//! instructions (`string`) and replaces x87 instructions with `X87Recipes` output (`x87`). `op`
//! defines the operation vocabulary and effect properties shared by every later stage, including
//! SSA-only operations; `regs` maps SLEIGH registers to guest state lanes. Unsupported shapes
//! become `Op::Slow` instead of failing lowering.

pub mod lower;
pub mod op;
pub mod regs;
pub mod string;
pub mod x87;

pub use lower::{lower, CgFunction, CgInsn, CgOp, FlagShape};
pub use op::{
    properties, CcKind, NoX87, Op, Properties, SlowFamily, SlowInsn, StateKey, StringOp, X87Recipes,
};
pub use regs::{field_lane, lanes, xmm_offset, Registers};
pub use x87::{Arithmetic, ArithmeticOp, X87Lowering, X87Op, X87Recipe};
