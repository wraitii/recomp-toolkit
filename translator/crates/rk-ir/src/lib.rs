//! ISA-neutral IR: lowers lifted p-code into codegen IR (`CgFunction`) through an `IsaLowering`
//! hook that the ISA plug-in implements (per-instruction ops, flag shape). `op` defines the
//! operation vocabulary and effect properties shared by every later stage, including SSA-only
//! operations; `regs` maps SLEIGH registers to guest state lanes; `wide` legalizes SIMD values
//! into lanes; `x87` holds the x87 operation vocabulary that `X87Recipes` produces. Unsupported
//! shapes become `Op::Slow` instead of failing lowering.

pub mod lower;
pub mod op;
pub mod regs;
pub mod wide;
pub mod x87;

pub use lower::{
    flag_recipes, lower, lower_with, CgFunction, CgInsn, CgOp, Cond, FlagRecipes, FlagShape,
    InstructionLowerer, IsaLowering, Lowered,
};
pub use op::{
    properties, Native, NoX87, Op, Properties, SimdOp, SlowFamily, SlowInsn, StateKey, X87Recipes,
};
pub use regs::{field_lane, lanes, xmm_offset, Arch, Registers, X86Registers};
pub use x87::{Arithmetic, ArithmeticOp, X87Lowering, X87Op, X87Recipe};

pub use regs::EFLAGS_MISC;
