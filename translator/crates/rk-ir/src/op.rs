use rk_core::ir::{Insn, OpCode, SehEffect, Varnode};
use rk_core::program::Image;
use rk_core::Addr;

use crate::x87::{X87Lowering, X87Op};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StateKey {
    Register(u32),
    Memory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CcKind {
    Add,
    Sub,
    Logic,
    Inc,
    Dec,
    Adc,
    Sbb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlowInsn {
    pub addr: Addr,
    pub length: u8,
    pub family: SlowFamily,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlowFamily {
    Adc,
    Sbb,
    Rol,
    Ror,
    Rcl,
    Rcr,
    Shld,
    Shrd,
    String,
    Rdtsc,
    Cpuid,
    In,
    Out,
    Int,
    Iret,
    Retf,
    Hlt,
    Cli,
    Sti,
    Bound,
    Bcd,
    Mmx,
    Sse,
    Flags,
    Segment,
    X87,
    CallOther,
    Unmodelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StringOp(pub &'static str);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    P(OpCode),
    Call { target: Addr },
    CallInd,
    Tail { target: Addr },
    ImportTail { slot: Addr },
    Branch { target: Addr },
    CBranch { target: Addr },
    BranchInd,
    Return,
    Trap(Addr),
    Breakpoint,
    Seh(SehEffect),
    Div { addr: Addr, size: u8 },
    Idiv { addr: Addr, size: u8 },
    String(StringOp),
    Slow(u32),
    CallOther(u64),
    X87(X87Op),
    Input(StateKey),
    Const(u64),
    Target(Addr),
    Phi,
    Byte(u8),
    Pack,
    Memory,
    StateReload(StateKey),
    StateKeep(StateKey),
    CcKind(CcKind),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Properties {
    pub ordered: bool,
    pub observes: bool,
    pub faults: bool,
    pub memory: bool,
    pub terminator: bool,
    pub pure: bool,
    pub synthetic: bool,
    pub opaque: bool,
}

impl Properties {
    const fn new() -> Self {
        Self {
            ordered: false,
            observes: false,
            faults: false,
            memory: false,
            terminator: false,
            pure: false,
            synthetic: false,
            opaque: false,
        }
    }
    const fn pure() -> Self {
        Self {
            pure: true,
            ..Self::new()
        }
    }
    const fn effect() -> Self {
        Self {
            ordered: true,
            observes: true,
            faults: true,
            memory: true,
            opaque: true,
            ..Self::new()
        }
    }
}

pub fn properties(op: Op) -> Properties {
    match op {
        Op::P(code) => match code {
            OpCode::Load => Properties {
                ordered: true,
                observes: true,
                faults: true,
                memory: true,
                ..Properties::new()
            },
            OpCode::Store => Properties {
                ordered: true,
                observes: true,
                faults: true,
                memory: true,
                ..Properties::new()
            },
            OpCode::Branch | OpCode::CBranch => Properties {
                terminator: true,
                ..Properties::new()
            },
            OpCode::BranchInd | OpCode::Return => Properties {
                observes: true,
                terminator: true,
                ..Properties::new()
            },
            OpCode::Call | OpCode::CallInd => Properties::effect(),
            OpCode::CallOther => Properties {
                opaque: true,
                faults: true,
                ..Properties::new()
            },
            OpCode::Copy
            | OpCode::IntEqual
            | OpCode::IntNotEqual
            | OpCode::IntSLess
            | OpCode::IntSLessEqual
            | OpCode::IntLess
            | OpCode::IntLessEqual
            | OpCode::IntZExt
            | OpCode::IntSExt
            | OpCode::IntAdd
            | OpCode::IntSub
            | OpCode::IntCarry
            | OpCode::IntSCarry
            | OpCode::IntSBorrow
            | OpCode::Int2Comp
            | OpCode::IntNegate
            | OpCode::IntXor
            | OpCode::IntAnd
            | OpCode::IntOr
            | OpCode::IntLeft
            | OpCode::IntRight
            | OpCode::IntSRight
            | OpCode::IntMult
            | OpCode::IntDiv
            | OpCode::IntSDiv
            | OpCode::IntRem
            | OpCode::IntSRem
            | OpCode::BoolNegate
            | OpCode::BoolXor
            | OpCode::BoolAnd
            | OpCode::BoolOr
            | OpCode::FloatEqual
            | OpCode::FloatNotEqual
            | OpCode::FloatLess
            | OpCode::FloatLessEqual
            | OpCode::FloatNan
            | OpCode::FloatAdd
            | OpCode::FloatDiv
            | OpCode::FloatMult
            | OpCode::FloatSub
            | OpCode::FloatMin
            | OpCode::FloatMax
            | OpCode::Cvtss2si
            | OpCode::Cvttss2si
            | OpCode::Cvtsi2ss
            | OpCode::FloatNeg
            | OpCode::FloatAbs
            | OpCode::FloatSqrt
            | OpCode::FloatRcp
            | OpCode::FloatRsqrt
            | OpCode::FloatInt2Float
            | OpCode::FloatFloat2Float
            | OpCode::FloatTrunc
            | OpCode::FloatCeil
            | OpCode::FloatFloor
            | OpCode::FloatRound
            | OpCode::Piece
            | OpCode::SubPiece
            | OpCode::PopCount
            | OpCode::LzCount => Properties::pure(),
            OpCode::Unknown => Properties {
                opaque: true,
                faults: true,
                ..Properties::new()
            },
        },
        Op::Call { .. } | Op::CallInd => Properties::effect(),
        Op::Div { .. } | Op::Idiv { .. } | Op::String(_) => Properties::effect(),
        Op::Slow(_) | Op::CallOther(_) => Properties::effect(),
        Op::Tail { .. } | Op::ImportTail { .. } | Op::BranchInd | Op::Return => Properties {
            observes: true,
            terminator: true,
            ..Properties::new()
        },
        Op::Branch { .. } | Op::CBranch { .. } => Properties {
            terminator: true,
            ..Properties::new()
        },
        Op::Trap(_) => Properties {
            terminator: true,
            faults: true,
            opaque: true,
            ..Properties::new()
        },
        Op::Breakpoint => Properties {
            observes: true,
            terminator: true,
            faults: true,
            ..Properties::new()
        },
        Op::Seh(_) => Properties {
            ordered: true,
            observes: true,
            faults: true,
            memory: true,
            ..Properties::new()
        },
        Op::X87(op) => Properties {
            ordered: true,
            observes: true,
            faults: true,
            memory: op.touches_memory(),
            ..Properties::new()
        },
        Op::Input(_)
        | Op::Const(_)
        | Op::Target(_)
        | Op::Phi
        | Op::Byte(_)
        | Op::Pack
        | Op::Memory
        | Op::StateReload(_)
        | Op::StateKeep(_)
        | Op::CcKind(_) => Properties {
            pure: true,
            synthetic: true,
            ..Properties::new()
        },
    }
}

pub trait X87Recipes: Sync {
    fn lower(
        &self,
        insn: &Insn,
        image: &Image,
        fresh: &mut dyn FnMut(u32) -> Varnode,
    ) -> Option<X87Lowering>;
}

pub struct NoX87;

impl X87Recipes for NoX87 {
    fn lower(
        &self,
        _insn: &Insn,
        _image: &Image,
        _fresh: &mut dyn FnMut(u32) -> Varnode,
    ) -> Option<X87Lowering> {
        None
    }
}
