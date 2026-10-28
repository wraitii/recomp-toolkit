use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::Addr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Space {
    Const,
    Ram,
    Register,
    Unique,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Varnode {
    pub space: Space,
    pub offset: u64,
    pub size: u32,
}

impl Varnode {
    pub const fn new(space: Space, offset: u64, size: u32) -> Self {
        Self {
            space,
            offset,
            size,
        }
    }

    pub const fn constant(value: u64, size: u32) -> Self {
        Self::new(Space::Const, value, size)
    }

    pub const fn register(offset: u64, size: u32) -> Self {
        Self::new(Space::Register, offset, size)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum OpCode {
    Copy,
    Load,
    Store,
    Branch,
    CBranch,
    BranchInd,
    Call,
    CallInd,
    CallOther,
    Return,
    IntEqual,
    IntNotEqual,
    IntSLess,
    IntSLessEqual,
    IntLess,
    IntLessEqual,
    IntZExt,
    IntSExt,
    IntAdd,
    IntSub,
    IntCarry,
    IntSCarry,
    IntSBorrow,
    Int2Comp,
    IntNegate,
    IntXor,
    IntAnd,
    IntOr,
    IntLeft,
    IntRight,
    IntSRight,
    IntMult,
    IntDiv,
    IntSDiv,
    IntRem,
    IntSRem,
    BoolNegate,
    BoolXor,
    BoolAnd,
    BoolOr,
    FloatEqual,
    FloatNotEqual,
    FloatLess,
    FloatLessEqual,
    FloatNan,
    FloatAdd,
    FloatDiv,
    FloatMult,
    FloatSub,
    FloatMin,
    FloatMax,
    Cvtss2si,
    Cvttss2si,
    Cvtsi2ss,
    FloatNeg,
    FloatAbs,
    FloatSqrt,
    FloatRcp,
    FloatRsqrt,
    FloatInt2Float,
    FloatFloat2Float,
    FloatTrunc,
    FloatCeil,
    FloatFloor,
    FloatRound,
    Piece,
    SubPiece,
    PopCount,
    LzCount,
    Unknown,
}

impl OpCode {
    pub fn is_branch(self) -> bool {
        matches!(
            self,
            OpCode::Branch
                | OpCode::CBranch
                | OpCode::BranchInd
                | OpCode::Call
                | OpCode::CallInd
                | OpCode::Return
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PcodeOp {
    pub opcode: OpCode,
    pub output: Option<Varnode>,
    pub inputs: Vec<Varnode>,
    #[serde(default)]
    pub userop: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Part {
    L,
    H,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperandKind {
    Reg {
        gpr: u8,
        part: Option<Part>,
    },
    Xmm(u8),
    Mm(u8),
    St(u8),
    Sreg {
        name: String,
        selector: u32,
    },
    Imm(i64),
    Mem {
        base: Option<u8>,
        index: Option<u8>,
        scale: u8,
        disp: i64,
        seg: Option<String>,
    },
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operand {
    pub kind: OperandKind,
    pub size: Option<u16>,
    pub display: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Repeat {
    Rep,
    Repne,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Insn {
    pub addr: Addr,
    pub length: u8,
    pub address_size: u8,
    pub mnemonic: String,
    pub operands: String,
    pub typed_operands: Vec<Operand>,
    pub repeat: Option<Repeat>,
    pub text: String,
    pub ops: Vec<PcodeOp>,
    pub x87: bool,
}

impl Insn {
    pub fn end(&self) -> Addr {
        self.addr + self.length as Addr
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SehEffect {
    Landing,
    Enter,
    Leave,
    Adopt,
    Orphan,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FunctionIr {
    pub addr: Addr,
    pub insns: Vec<Insn>,
    pub succ: Vec<Vec<u32>>,
    pub exits: BTreeMap<u32, Vec<Addr>>,
    pub tables: BTreeSet<u32>,
    pub dynamic: BTreeSet<u32>,
    pub noreturn: BTreeMap<u32, Addr>,
    pub entries: Vec<Addr>,
    pub dead: BTreeSet<u32>,
    #[serde(default)]
    pub call_returns: BTreeSet<Addr>,
    #[serde(default)]
    pub intra_calls: BTreeMap<u32, Addr>,
    #[serde(default)]
    pub intra_call_returns: BTreeSet<Addr>,
    #[serde(default)]
    pub seh: BTreeMap<u32, Vec<SehEffect>>,
    #[serde(default)]
    pub seh_handlers: BTreeMap<u32, Addr>,
    #[serde(default)]
    pub seh_unwind_only: BTreeSet<u32>,
    #[serde(default)]
    pub import_tails: BTreeMap<u32, Addr>,
}

impl FunctionIr {
    pub fn index_of(&self, addr: Addr) -> Option<u32> {
        self.insns
            .binary_search_by_key(&addr, |insn| insn.addr)
            .ok()
            .map(|i| i as u32)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Policy {
    #[default]
    Optimizing,
    Conservative,
}
