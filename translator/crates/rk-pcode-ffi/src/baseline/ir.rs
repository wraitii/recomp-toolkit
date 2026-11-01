use std::collections::BTreeSet;

use rk_core::ir::{OpCode, Space, Varnode};
use rk_ir::{CgOp, Cond, FlagShape, Op as IrOp};

use super::abi::FloatOp;
use super::helper::Helper;
use super::state::{classify, Register};
use crate::lift::{is_internal_branch, Lifted};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Branch,
    Call,
    Return,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Direct(u32),
    Indirect(Varnode),
}

#[derive(Clone, Debug)]
pub enum Op {
    Unary {
        code: OpCode,
        out: Varnode,
        a: Varnode,
    },
    Binary {
        code: OpCode,
        out: Varnode,
        a: Varnode,
        b: Varnode,
    },
    Divide {
        code: OpCode,
        out: Varnode,
        a: Varnode,
        b: Varnode,
    },
    DivMod {
        signed: bool,
        size: u32,
        out: Varnode,
        a: Varnode,
        b: Varnode,
    },
    Float {
        helper: FloatOp,
        out: Varnode,
        a: Varnode,
        b: Option<Varnode>,
    },
    Load {
        out: Varnode,
        addr: Varnode,
    },
    Store {
        addr: Varnode,
        value: Varnode,
    },
    Jump {
        target: usize,
        condition: Option<Varnode>,
    },
    Exit {
        exit: Exit,
        target: Target,
        condition: Option<Varnode>,
    },
    Helper {
        helper: Helper,
        argument: Option<Varnode>,
        out: Option<Varnode>,
    },
    Flags {
        shape: FlagShape,
        inputs: Vec<Varnode>,
    },
    Cond {
        cond: Cond,
        out: Varnode,
    },
}

pub struct Instruction {
    pub length: u8,
    pub mnemonic: String,
    pub ops: Vec<Op>,
    pub internal_control: bool,
}

impl Instruction {
    pub fn terminal(&self) -> bool {
        matches!(
            self.ops.last(),
            Some(Op::Exit {
                condition: None,
                ..
            })
        )
    }

    pub fn mentions(&self, test: impl Fn(Varnode) -> bool) -> bool {
        let mut found = false;
        for op in &self.ops {
            op.for_each_varnode(|v| found |= test(v));
        }
        found
    }

    pub fn uniques(&self) -> BTreeSet<u64> {
        let mut bytes = BTreeSet::new();
        for op in &self.ops {
            op.for_each_varnode(|v| {
                if v.space == Space::Unique {
                    bytes.extend((0..v.size).map(|i| v.offset.wrapping_add(u64::from(i))));
                }
            });
        }
        bytes
    }
}

impl Op {
    fn for_each_varnode(&self, mut f: impl FnMut(Varnode)) {
        let mut all = |vs: &[Option<Varnode>]| vs.iter().flatten().for_each(|v| f(*v));
        match *self {
            Self::Unary { out, a, .. } => all(&[Some(out), Some(a)]),
            Self::Binary { out, a, b, .. }
            | Self::Divide { out, a, b, .. }
            | Self::DivMod { out, a, b, .. } => all(&[Some(out), Some(a), Some(b)]),
            Self::Float { out, a, b, .. } => all(&[Some(out), Some(a), b]),
            Self::Load { out, addr } => all(&[Some(out), Some(addr)]),
            Self::Store { addr, value } => all(&[Some(addr), Some(value)]),
            Self::Jump { condition, .. } => all(&[condition]),
            Self::Exit {
                target, condition, ..
            } => all(&[
                match target {
                    Target::Indirect(v) => Some(v),
                    Target::Direct(_) => None,
                },
                condition,
            ]),
            Self::Helper { argument, out, .. } => all(&[argument, out]),
            Self::Flags { ref inputs, .. } => inputs.iter().for_each(|v| f(*v)),
            Self::Cond { out, .. } => f(out),
        }
    }
}

pub struct Rejected(pub String);

fn reject<T>(detail: impl Into<String>) -> Result<T, Rejected> {
    Err(Rejected(detail.into()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    SameAsInput,
    Bool,
    NoWiderThanInput,
    Free,
    Float32,
    SumOfInputs,
}

fn unary(code: OpCode) -> Option<Shape> {
    use OpCode::*;
    Some(match code {
        Copy | Int2Comp | IntNegate => Shape::SameAsInput,
        IntZExt | IntSExt | PopCount | LzCount => Shape::Free,
        BoolNegate => Shape::Bool,
        FloatNeg | FloatAbs => Shape::Float32,
        _ => return None,
    })
}

fn binary(code: OpCode) -> Option<(Shape, bool)> {
    use OpCode::*;
    Some(match code {
        IntEqual | IntNotEqual | IntSLess | IntSLessEqual | IntLess | IntLessEqual | IntCarry
        | IntSCarry | IntSBorrow | BoolXor | BoolAnd | BoolOr => (Shape::Bool, true),
        IntAdd | IntSub | IntXor | IntOr | IntMult => (Shape::SameAsInput, true),
        IntAnd => (Shape::NoWiderThanInput, true),
        IntLeft | IntRight | IntSRight => (Shape::SameAsInput, false),
        Piece => (Shape::SumOfInputs, false),
        SubPiece => (Shape::NoWiderThanInput, false),
        _ => return None,
    })
}

fn float(code: OpCode) -> Option<(FloatOp, usize, u32)> {
    use FloatOp as F;
    use OpCode::*;
    Some(match code {
        FloatAdd => (F::Add, 2, 4),
        FloatSub => (F::Sub, 2, 4),
        FloatMult => (F::Mul, 2, 4),
        FloatDiv => (F::Div, 2, 4),
        FloatMin => (F::Min, 2, 4),
        FloatMax => (F::Max, 2, 4),
        FloatSqrt => (F::Sqrt, 1, 4),
        FloatRcp => (F::Rcp, 1, 4),
        FloatRsqrt => (F::Rsqrt, 1, 4),
        FloatEqual => (F::Equal, 2, 1),
        FloatNotEqual => (F::NotEqual, 2, 1),
        FloatLess => (F::Less, 2, 1),
        FloatLessEqual => (F::LessEqual, 2, 1),
        FloatNan => (F::Nan, 1, 1),
        _ => return None,
    })
}

fn shape_fits(shape: Shape, out: Varnode, inputs: &[Varnode]) -> bool {
    match shape {
        Shape::SameAsInput => out.size == inputs[0].size,
        Shape::Bool => out.size == 1,
        Shape::NoWiderThanInput => out.size <= inputs[0].size,
        Shape::Free => true,
        Shape::Float32 => out.size == 4 && inputs[0].size == 4,
        Shape::SumOfInputs => inputs[0].size + inputs[1].size == out.size,
    }
}

struct Decoder {
    defined: BTreeSet<u64>,
    internal_control: bool,
    op_count: usize,
}

impl Decoder {
    fn check(&self, v: Varnode, write: bool) -> Result<Varnode, Rejected> {
        if !(1..=8).contains(&v.size) {
            return reject(format!("operand width {} is not modelled", v.size));
        }
        let supported = match v.space {
            Space::Const => !write,
            Space::Ram => v.offset <= u64::from(u32::MAX),
            Space::Register => classify(v).is_some(),
            Space::Unique => {
                write
                    || self.internal_control
                    || (0..v.size)
                        .all(|i| self.defined.contains(&v.offset.wrapping_add(u64::from(i))))
            }
        };
        if !supported {
            return reject(format!(
                "unsupported {} {v:?}",
                if write { "output" } else { "input" }
            ));
        }
        Ok(v)
    }

    fn read(&self, v: Varnode) -> Result<Varnode, Rejected> {
        self.check(v, false)
    }

    fn write(&mut self, v: Varnode) -> Result<Varnode, Rejected> {
        self.check(v, true)?;
        if v.space == Space::Unique {
            self.defined
                .extend((0..v.size).map(|i| v.offset.wrapping_add(u64::from(i))));
        }
        Ok(v)
    }

    fn inputs(&self, op: &CgOp, count: usize) -> Result<Vec<Varnode>, Rejected> {
        if op.inputs.len() != count {
            return reject(format!("{:?} has {} inputs", op.op, op.inputs.len()));
        }
        op.inputs.iter().map(|v| self.read(*v)).collect()
    }

    fn output(&mut self, op: &CgOp) -> Result<Varnode, Rejected> {
        let out = op
            .output
            .ok_or_else(|| Rejected(format!("{:?} has no output", op.op)))?;
        self.write(out)
    }

    fn no_output(op: &CgOp) -> Result<(), Rejected> {
        match op.output {
            Some(_) => reject(format!("{:?} has an output", op.op)),
            None => Ok(()),
        }
    }

    fn exit(&self, op: &CgOp, index: usize) -> Result<Op, Rejected> {
        Self::no_output(op)?;
        if index + 1 != self.op_count {
            return reject("control transfer before the end of the instruction");
        }
        let direct = |target: rk_core::Addr| {
            u32::try_from(target)
                .map(Target::Direct)
                .map_err(|_| Rejected("control target exceeds 32 bits".into()))
        };
        let (exit, target, condition) = match op.op {
            IrOp::Branch { target } => {
                self.inputs(op, 0)?;
                (Exit::Branch, direct(target)?, None)
            }
            IrOp::CBranch { target } => {
                let inputs = self.inputs(op, 1)?;
                (Exit::Branch, direct(target)?, Some(inputs[0]))
            }
            IrOp::Call { target } => {
                self.inputs(op, 0)?;
                (Exit::Call, direct(target)?, None)
            }
            IrOp::BranchInd | IrOp::CallInd | IrOp::Return => {
                let inputs = self.inputs(op, 1)?;
                let exit = match op.op {
                    IrOp::BranchInd => Exit::Branch,
                    IrOp::CallInd => Exit::Call,
                    _ => Exit::Return,
                };
                (exit, Target::Indirect(inputs[0]), None)
            }
            _ => unreachable!(),
        };
        Ok(Op::Exit {
            exit,
            target,
            condition,
        })
    }

    fn jump(&self, op: &CgOp, index: usize) -> Result<Op, Rejected> {
        Self::no_output(op)?;
        let conditional = op.op == IrOp::P(OpCode::CBranch);
        if op.inputs.len() != 1 + usize::from(conditional) {
            return reject("malformed internal branch");
        }
        let relative = op.inputs[0];
        if !(1..=8).contains(&relative.size) {
            return reject("malformed internal branch target");
        }
        let shift = 64 - relative.size * 8;
        let delta = ((relative.offset << shift) as i64) >> shift;
        let target = (index as i64)
            .checked_add(delta)
            .filter(|t| (0..=self.op_count as i64).contains(t))
            .ok_or_else(|| Rejected("internal branch leaves the instruction".into()))?;
        let condition = if conditional {
            Some(self.read(op.inputs[1])?)
        } else {
            None
        };
        Ok(Op::Jump {
            target: target as usize,
            condition,
        })
    }

    fn helper(&mut self, op: &CgOp, helper: Helper) -> Result<Op, Rejected> {
        let Helper::X87(call) = helper else {
            return Ok(Op::Helper {
                helper,
                argument: None,
                out: None,
            });
        };
        let inputs = self.inputs(op, usize::from(call.reads_memory_operand()))?;
        let out = if call.produces_status() {
            let out = self.output(op)?;
            if out.size != 2 {
                return reject("x87 status output must be 16 bits");
            }
            Some(out)
        } else {
            Self::no_output(op)?;
            None
        };
        Ok(Op::Helper {
            helper,
            argument: inputs.first().copied(),
            out,
        })
    }

    fn op(&mut self, op: &CgOp, index: usize, mnemonic: &str) -> Result<Op, Rejected> {
        if let Some(helper) = Helper::from_op(op, mnemonic) {
            return self.helper(op, helper.map_err(Rejected)?);
        }
        if is_internal_branch(op) {
            return self.jump(op, index);
        }
        let code = match op.op {
            IrOp::Branch { .. }
            | IrOp::CBranch { .. }
            | IrOp::Call { .. }
            | IrOp::BranchInd
            | IrOp::CallInd
            | IrOp::Return => return self.exit(op, index),
            IrOp::DivMod { signed, size } => {
                let size = u32::from(size);
                let [a, b] = op
                    .inputs
                    .get(..2)
                    .and_then(|i| i.try_into().ok())
                    .ok_or_else(|| Rejected("DivMod needs two inputs".into()))?;
                let (a, b) = (self.read(a)?, self.read(b)?);
                if !matches!(size, 1 | 2 | 4) || a.size != size * 2 || b.size != a.size {
                    return reject(format!("DivMod of width {size} is not modelled"));
                }
                let out = self.output(op)?;
                if out.size != 8 {
                    return reject("DivMod output must be 8 bytes");
                }
                return Ok(Op::DivMod {
                    signed,
                    size,
                    out,
                    a,
                    b,
                });
            }
            IrOp::Flags(shape) => {
                Self::no_output(op)?;
                let count = if matches!(shape, FlagShape::Adc | FlagShape::Sbb) {
                    4
                } else {
                    3
                };
                let inputs = self.inputs(op, count)?;
                if inputs
                    .iter()
                    .any(|v| v.size != inputs[count - 1].size || v.size > 4)
                {
                    return reject(format!("{shape:?} flag operand widths are not modelled"));
                }
                return Ok(Op::Flags { shape, inputs });
            }
            IrOp::Cond(cond) => {
                let out = self.output(op)?;
                if out.size != 1 {
                    return reject("condition output must be one byte");
                }
                return Ok(Op::Cond { cond, out });
            }
            IrOp::P(code) => code,
            other => return reject(format!("unsupported operation {other:?}")),
        };
        match code {
            OpCode::Load => {
                let inputs = self.inputs(op, 1)?;
                let out = self.output(op)?;
                return Ok(Op::Load {
                    out,
                    addr: inputs[0],
                });
            }
            OpCode::Store => {
                Self::no_output(op)?;
                let inputs = self.inputs(op, 2)?;
                return Ok(Op::Store {
                    addr: inputs[0],
                    value: inputs[1],
                });
            }
            OpCode::IntDiv | OpCode::IntSDiv | OpCode::IntRem | OpCode::IntSRem => {
                let inputs = self.inputs(op, 2)?;
                let out = self.output(op)?;
                let size = inputs[0].size;
                if out.size != size || inputs[1].size != size {
                    return reject(format!("{code:?} operand widths differ"));
                }
                if matches!(code, OpCode::IntDiv | OpCode::IntSDiv) && !matches!(size, 2 | 4 | 8) {
                    return reject("x86 division operand width is not modelled");
                }
                return Ok(Op::Divide {
                    code,
                    out,
                    a: inputs[0],
                    b: inputs[1],
                });
            }
            _ => {}
        }
        if let Some((helper, count, out_size)) = float(code) {
            let inputs = self.inputs(op, count)?;
            let out = self.output(op)?;
            if out.size != out_size || inputs.iter().any(|v| v.size != 4) {
                return reject(format!("{code:?} operand widths are not modelled"));
            }
            return Ok(Op::Float {
                helper,
                out,
                a: inputs[0],
                b: inputs.get(1).copied(),
            });
        }
        if let Some(shape) = unary(code) {
            let inputs = self.inputs(op, 1)?;
            let out = self.output(op)?;
            if !shape_fits(shape, out, &inputs) {
                return reject(format!("{code:?} operand widths are not modelled"));
            }
            return Ok(Op::Unary {
                code,
                out,
                a: inputs[0],
            });
        }
        if let Some((shape, same_inputs)) = binary(code) {
            let inputs = self.inputs(op, 2)?;
            let out = self.output(op)?;
            if !shape_fits(shape, out, &inputs) || (same_inputs && inputs[0].size != inputs[1].size)
            {
                return reject(format!("{code:?} operand widths are not modelled"));
            }
            return Ok(Op::Binary {
                code,
                out,
                a: inputs[0],
                b: inputs[1],
            });
        }
        reject(format!("unsupported p-code {code:?}"))
    }
}

pub fn decode(lifted: Lifted) -> Result<Instruction, Rejected> {
    let mut decoder = Decoder {
        defined: BTreeSet::new(),
        internal_control: lifted.ops.iter().any(is_internal_branch),
        op_count: lifted.ops.len(),
    };
    let ops = lifted
        .ops
        .iter()
        .enumerate()
        .map(|(index, op)| decoder.op(op, index, &lifted.mnemonic))
        .collect::<Result<_, _>>()?;
    Ok(Instruction {
        length: lifted.length,
        mnemonic: lifted.mnemonic,
        ops,
        internal_control: decoder.internal_control,
    })
}

pub fn register(v: Varnode) -> Register {
    classify(v).expect("decoded register")
}
