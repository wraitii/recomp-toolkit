use rk_core::ir::Varnode;

use crate::op::Op;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithmeticOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arithmetic {
    pub operator: ArithmeticOp,
    pub integer: bool,
    pub reverse: bool,
    pub pops: u8,
    pub dst: u8,
    pub src: u8,
    pub bits: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X87Unary {
    Abs,
    Negate,
    Sqrt,
    Round,
    Sin,
    Cos,
    ExpM1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X87Environment {
    LoadCw,
    StoreCw,
    StoreSw,
    StoreEnv,
    LoadEnv,
    Save,
    Restore,
    EmptyTags(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X87Legacy {
    Save,
    Restore,
    StoreEnv,
    LoadEnv,
    BcdStore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X87Condition {
    B,
    E,
    Be,
    U,
    Nb,
    Ne,
    Nbe,
    Nu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X87Eager {
    Fptan,
    Fsincos,
    Fscale,
    Fpatan,
    Fyl2x,
    Fyl2xp1,
    Xtract,
    Fbstp,
    ClearExceptions,
    Fxam,
    Init,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X87Op {
    Constant(&'static str),
    Push {
        src: Option<u8>,
        bits: u16,
    },
    PushInteger {
        bits: u16,
    },
    Store {
        dst: Option<u8>,
        bits: u16,
        pop: bool,
        integer: bool,
    },
    Arithmetic(Arithmetic),
    Compare {
        quiet: bool,
        integer: bool,
        src: u8,
        bits: u16,
        pops: u8,
        zero: bool,
    },
    CompareFlags {
        quiet: bool,
        src: u8,
        pop: bool,
    },
    Unary(X87Unary),
    Remainder {
        ieee: bool,
    },
    Exchange {
        src: u8,
    },
    Free {
        src: u8,
    },
    Status,
    Environment(X87Environment),
    Clear,
    Init,
    Rotate(i8),
    Conditional(X87Condition, u8),
    Extract,
    LegacyEnvironment(X87Legacy),
    EmptyTags(bool),
    Nop,
    Eager(X87Eager),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryEffect {
    Read,
    Write,
}

impl X87Op {
    pub fn touches_memory(self) -> bool {
        self.memory().is_some()
    }

    pub fn memory(self) -> Option<MemoryEffect> {
        match self {
            X87Op::Push { src: None, .. }
            | X87Op::PushInteger { .. }
            | X87Op::Arithmetic(Arithmetic { bits: 1.., .. })
            | X87Op::Compare { bits: 1.., .. }
            | X87Op::Compare { zero: true, .. } => Some(MemoryEffect::Read),
            X87Op::Store {
                dst: None,
                bits: 1..,
                ..
            } => Some(MemoryEffect::Write),
            X87Op::Environment(
                X87Environment::StoreCw
                | X87Environment::StoreSw
                | X87Environment::StoreEnv
                | X87Environment::Save,
            ) => Some(MemoryEffect::Write),
            X87Op::Environment(
                X87Environment::LoadCw | X87Environment::LoadEnv | X87Environment::Restore,
            ) => Some(MemoryEffect::Read),
            X87Op::Eager(X87Eager::Fbstp) => Some(MemoryEffect::Write),
            X87Op::LegacyEnvironment(
                X87Legacy::Save | X87Legacy::StoreEnv | X87Legacy::BcdStore,
            ) => Some(MemoryEffect::Write),
            X87Op::LegacyEnvironment(X87Legacy::Restore | X87Legacy::LoadEnv) => {
                Some(MemoryEffect::Read)
            }
            _ => None,
        }
    }

    pub fn result_size(self) -> u8 {
        match self {
            X87Op::Status | X87Op::CompareFlags { .. } => 2,
            _ => 0,
        }
    }

    pub fn status_ax(self) -> bool {
        matches!(self, X87Op::Status)
    }

    pub fn compare_flags(self) -> bool {
        matches!(self, X87Op::CompareFlags { .. })
    }

    pub fn cw_sensitive(self) -> bool {
        match self {
            X87Op::Arithmetic(Arithmetic { integer: false, .. }) => !matches!(
                self,
                X87Op::Arithmetic(Arithmetic {
                    operator: ArithmeticOp::Div,
                    ..
                })
            ),
            X87Op::Arithmetic(Arithmetic { integer: true, .. })
            | X87Op::Unary(X87Unary::Sqrt | X87Unary::Round)
            | X87Op::Store { integer: true, .. }
            | X87Op::CompareFlags { .. } => true,
            X87Op::Store {
                dst: None,
                bits: 32,
                integer: false,
                ..
            } => true,
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct X87Recipe {
    pub op: Op,
    pub output: Option<Varnode>,
    pub inputs: Vec<Varnode>,
}

#[derive(Clone, Debug, Default)]
pub struct X87Lowering {
    pub recipes: Vec<X87Recipe>,
}
