use rk_ir::x87::{ArithmeticOp, X87Eager, X87Environment, X87Unary};
use rk_ir::{CgOp, Op, X87Op};

use super::abi::{Repeat, StringFamily, X87Call, X87Kind, X87Operand};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StringOp {
    pub family: StringFamily,
    pub width_log2: u8,
    pub repeat: Repeat,
}

impl StringOp {
    fn parse(name: &str) -> Option<Self> {
        let (repeat, base) = if let Some(base) = name.strip_prefix("repne_") {
            (Repeat::WhileNotEqual, base)
        } else if let Some(base) = name
            .strip_prefix("repe_")
            .or_else(|| name.strip_prefix("rep_"))
        {
            (Repeat::WhileEqual, base)
        } else {
            (Repeat::Once, name)
        };
        let (root, suffix) = base.split_at_checked(4)?;
        let family = match root {
            "movs" => StringFamily::Movs,
            "stos" => StringFamily::Stos,
            "lods" => StringFamily::Lods,
            "cmps" => StringFamily::Cmps,
            "scas" => StringFamily::Scas,
            _ => return None,
        };
        let width_log2 = match suffix {
            "b" => 0,
            "w" => 1,
            "d" => 2,
            _ => return None,
        };
        Some(Self {
            family,
            width_log2,
            repeat,
        })
    }
}

impl X87Call {
    fn new(op: X87Op) -> Option<Self> {
        type K = X87Kind;
        let call = |kind, operand: u64, bits: u64, flags: u64, extra: u64| Self {
            kind,
            operand: operand as u8,
            bits: bits as u8,
            flags: flags as u8,
            extra: extra as u8,
        };
        let memory_or =
            |register: Option<u8>| u64::from(register.unwrap_or(X87Operand::Memory.repr));
        Some(match op {
            X87Op::Constant(name) => {
                let index = [
                    "FLDZ", "FLD1", "FLDPI", "FLDLN2", "FLDL2E", "FLDLG2", "FLDL2T",
                ]
                .iter()
                .position(|candidate| *candidate == name)?;
                call(K::Constant, index as u64, 0, 0, 0)
            }
            X87Op::Environment(X87Environment::StoreSw) => call(K::StoreStatus, 0, 16, 0, 0),
            X87Op::Eager(X87Eager::Fxam) => call(K::Examine, 0, 0, 0, 0),
            X87Op::Remainder { ieee } => call(K::Remainder, ieee.into(), 0, 0, 0),
            X87Op::Eager(X87Eager::Fyl2x) => call(K::Yl2x, 0, 0, 0, 0),
            X87Op::Eager(X87Eager::Fscale) => call(K::Scale, 0, 0, 0, 0),
            X87Op::Eager(X87Eager::Fptan) => call(K::Tangent, 0, 0, 0, 0),
            X87Op::Push { src, bits } => call(K::Push, memory_or(src), bits.into(), 0, 0),
            X87Op::PushInteger { bits } => call(K::PushInteger, 0, bits.into(), 0, 0),
            X87Op::Store {
                dst,
                bits,
                pop,
                integer,
            } => call(
                K::Store,
                memory_or(dst),
                bits.into(),
                pop.into(),
                integer.into(),
            ),
            X87Op::Arithmetic(v) => {
                let operator = match v.operator {
                    ArithmeticOp::Add => 0,
                    ArithmeticOp::Sub => 1,
                    ArithmeticOp::Mul => 2,
                    ArithmeticOp::Div => 3,
                };
                call(
                    K::Arithmetic,
                    u64::from(v.dst) | u64::from(v.src) << 4,
                    v.bits.into(),
                    operator | u64::from(v.reverse) << 2 | u64::from(v.integer) << 3,
                    v.pops.into(),
                )
            }
            X87Op::Compare {
                quiet,
                integer,
                src,
                bits,
                pops,
                zero,
            } => call(
                K::Compare,
                src.into(),
                bits.into(),
                u64::from(quiet) | u64::from(integer) << 1 | u64::from(zero) << 2,
                pops.into(),
            ),
            X87Op::CompareFlags { quiet, src, pop } => {
                call(K::CompareFlags, src.into(), 0, quiet.into(), pop.into())
            }
            X87Op::Exchange { src } => call(K::Exchange, src.into(), 0, 0, 0),
            X87Op::Free { src } => call(K::Free, src.into(), 0, 0, 0),
            X87Op::Status => call(K::Status, 0, 0, 0, 0),
            X87Op::Init => call(K::Init, 0, 0, 0, 0),
            X87Op::Rotate(n) => call(K::Rotate, (n > 0).into(), 0, 0, 0),
            X87Op::Nop => call(K::Nop, 0, 0, 0, 0),
            X87Op::Unary(v) => {
                let index = match v {
                    X87Unary::Abs => 0,
                    X87Unary::Negate => 1,
                    X87Unary::Sqrt => 2,
                    X87Unary::Round => 3,
                    X87Unary::Sin => 4,
                    X87Unary::Cos => 5,
                    X87Unary::ExpM1 => 6,
                };
                call(K::Unary, index, 0, 0, 0)
            }
            _ => return None,
        })
    }

    pub fn reads_memory_operand(self) -> bool {
        match self.kind {
            X87Kind::Push | X87Kind::Store => self.operand == X87Operand::Memory.repr,
            X87Kind::PushInteger | X87Kind::StoreStatus => true,
            X87Kind::Arithmetic | X87Kind::Compare => self.bits != 0,
            _ => false,
        }
    }

    pub fn produces_status(self) -> bool {
        matches!(self.kind, X87Kind::CompareFlags | X87Kind::Status)
    }

    pub fn specializable(self) -> bool {
        matches!(
            self.kind,
            X87Kind::Push | X87Kind::PushInteger | X87Kind::Store | X87Kind::Arithmetic
        )
    }

    pub fn specialized_operand(self) -> u32 {
        u32::from(self.operand) | u32::from(self.extra) << 8
    }

    pub fn memory_bytes(self) -> u32 {
        u32::from(self.bits).div_ceil(8)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Helper {
    String(StringOp),
    Rdtsc,
    Cpuid,
    X87Clear,
    X87Empty { reset_top: bool },
    X87(X87Call),
}

impl Helper {
    pub fn from_op(op: &CgOp, mnemonic: &str) -> Option<Result<Self, String>> {
        Some(Ok(match op.op {
            Op::Native(native) => {
                return Some(
                    StringOp::parse(native.name)
                        .map(Self::String)
                        .ok_or_else(|| format!("unknown native helper {}", native.name)),
                )
            }
            Op::Slow(_) if mnemonic == "RDTSC" => Self::Rdtsc,
            Op::Slow(_) if mnemonic == "CPUID" => Self::Cpuid,
            Op::X87(X87Op::Clear) => Self::X87Clear,
            Op::X87(X87Op::EmptyTags(reset_top)) => Self::X87Empty { reset_top },
            Op::X87(effect) => {
                return Some(
                    X87Call::new(effect)
                        .map(Self::X87)
                        .ok_or_else(|| format!("{mnemonic}: x87 operation is not modelled")),
                )
            }
            _ => return None,
        }))
    }
}
