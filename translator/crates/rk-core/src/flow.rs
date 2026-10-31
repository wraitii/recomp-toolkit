use std::collections::BTreeMap;

use crate::ir::{OpCode, PcodeOp, Space, Varnode};
use crate::Addr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Direct(Addr),
    Indirect(Varnode),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Fall,
    Jump(Target),
    Branch(Addr),
    Call(Target),
    Return,
    Trap,
}

impl Flow {
    pub fn of(ops: &[PcodeOp], addr: Addr, end: Addr) -> Self {
        let inside = |v: &Varnode| {
            v.space == Space::Const
                || (v.space == Space::Ram && addr <= v.offset && v.offset <= end)
        };
        let links = ops.iter().any(|op| {
            op.opcode == OpCode::Store
                && op
                    .inputs
                    .get(1)
                    .is_some_and(|v| v.space == Space::Const && v.offset == end)
        });
        let last = ops.len().saturating_sub(1);
        let looped = ops[..last].iter().any(|op| {
            matches!(op.opcode, OpCode::Branch | OpCode::CBranch)
                && op.inputs.first().is_some_and(inside)
        });
        let trapped = |v: &Varnode| {
            ops.iter()
                .any(|op| op.opcode == OpCode::CallOther && op.output == Some(*v))
        };
        if ops.iter().any(|op| {
            op.opcode == OpCode::Return
                && op.inputs.first().is_some_and(|v| v.space == Space::Const)
        }) {
            return Flow::Trap;
        }
        for (index, op) in ops.iter().enumerate() {
            let Some(&target) = op.inputs.first() else {
                continue;
            };
            let internal = |v: &Varnode| inside(v) && (index < last || looped);
            return match op.opcode {
                OpCode::Branch if !internal(&target) && links => {
                    Flow::Call(Target::Direct(target.offset))
                }
                OpCode::Branch if !internal(&target) => Flow::Jump(Target::Direct(target.offset)),
                OpCode::CBranch if !internal(&target) => Flow::Branch(target.offset),
                OpCode::BranchInd if trapped(&target) => Flow::Trap,
                OpCode::BranchInd => Flow::Jump(Target::Indirect(target)),
                OpCode::Call => Flow::Call(Target::Direct(target.offset)),
                OpCode::CallInd if !trapped(&target) => Flow::Call(Target::Indirect(target)),
                OpCode::Return => Flow::Return,
                _ => continue,
            };
        }
        Flow::Fall
    }

    pub fn target(self) -> Option<Addr> {
        match self {
            Flow::Jump(Target::Direct(target))
            | Flow::Call(Target::Direct(target))
            | Flow::Branch(target) => Some(target),
            _ => None,
        }
    }

    pub fn indirect(self) -> Option<Varnode> {
        match self {
            Flow::Jump(Target::Indirect(target)) | Flow::Call(Target::Indirect(target)) => {
                Some(target)
            }
            _ => None,
        }
    }

    pub fn is_jump(self) -> bool {
        matches!(self, Flow::Jump(_))
    }

    pub fn is_call(self) -> bool {
        matches!(self, Flow::Call(_))
    }

    pub fn ends_block(self) -> bool {
        matches!(self, Flow::Jump(_) | Flow::Return)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    Const(u64),
    Stack(i64),
    Frame(i64),
    Slot(i64),
    Unknown,
}

#[derive(Clone, Debug, Default)]
pub struct Effect {
    pub writes: BTreeMap<Varnode, Value>,
    pub stores: Vec<(Value, Value, u32)>,
}

impl Effect {
    pub fn of(ops: &[PcodeOp], stack: Varnode, frame: Varnode) -> Self {
        let mut effect = Effect::default();
        let mut temps: BTreeMap<Varnode, Value> = BTreeMap::new();
        let read = |effect: &Effect, temps: &BTreeMap<Varnode, Value>, v: &Varnode| match v.space {
            Space::Const => Value::Const(v.offset),
            Space::Unique => temps.get(v).copied().unwrap_or(Value::Unknown),
            Space::Register => effect.writes.get(v).copied().unwrap_or(if *v == stack {
                Value::Stack(0)
            } else if *v == frame {
                Value::Frame(0)
            } else {
                Value::Unknown
            }),
            Space::Ram => Value::Unknown,
        };
        for op in ops {
            let input = |n: usize| {
                op.inputs
                    .get(n)
                    .map_or(Value::Unknown, |v| read(&effect, &temps, v))
            };
            let value = match op.opcode {
                OpCode::Copy => input(0),
                OpCode::IntAdd => offset(input(0), input(1), stack.size)
                    .or_else(|| offset(input(1), input(0), stack.size))
                    .unwrap_or(Value::Unknown),
                OpCode::IntSub => match input(1) {
                    Value::Const(amount) => {
                        offset(input(0), Value::Const(amount.wrapping_neg()), stack.size)
                    }
                    _ => None,
                }
                .unwrap_or(Value::Unknown),
                OpCode::Load => match input(0) {
                    Value::Stack(at) => Value::Slot(at),
                    _ => Value::Unknown,
                },
                OpCode::Store => {
                    let size = op.inputs.get(1).map_or(0, |v| v.size);
                    effect.stores.push((input(0), input(1), size));
                    continue;
                }
                _ => Value::Unknown,
            };
            let Some(output) = op.output else {
                continue;
            };
            let value = if output.size == stack.size {
                value
            } else {
                Value::Unknown
            };
            match output.space {
                Space::Unique => {
                    temps.insert(output, value);
                }
                Space::Register => {
                    for tracked in [stack, frame] {
                        if output != tracked && overlaps(output, tracked) {
                            effect.writes.insert(tracked, Value::Unknown);
                        }
                    }
                    effect.writes.insert(output, value);
                }
                _ => {}
            }
        }
        effect
    }

    pub fn value(&self, register: Varnode) -> Option<Value> {
        self.writes.get(&register).copied()
    }

    pub fn pushed_constant(&self, stack: Varnode) -> Option<u64> {
        let Some(Value::Stack(top)) = self.value(stack) else {
            return None;
        };
        match self.stores.as_slice() {
            [(Value::Stack(at), Value::Const(value), size)]
                if *at == top && *size == stack.size =>
            {
                Some(*value)
            }
            _ => None,
        }
    }
}

fn offset(base: Value, amount: Value, size: u32) -> Option<Value> {
    let Value::Const(amount) = amount else {
        return None;
    };
    let shift = 64 - 8 * size.min(8);
    let amount = ((amount << shift) as i64) >> shift;
    Some(match base {
        Value::Stack(at) => Value::Stack(at + amount),
        Value::Frame(at) => Value::Frame(at + amount),
        _ => return None,
    })
}

fn overlaps(a: Varnode, b: Varnode) -> bool {
    a.space == b.space
        && a.offset < b.offset + u64::from(b.size)
        && b.offset < a.offset + u64::from(a.size)
}
