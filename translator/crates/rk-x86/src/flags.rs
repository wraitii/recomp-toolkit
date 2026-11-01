use rk_core::contract::{Field, Fields};
use rk_core::ir::{OpCode, PcodeOp, Varnode};
use rk_ir::lower::{copy, op};
use rk_ir::{Cond, FlagRecipes, FlagShape, Registers};

pub(crate) struct X86Flags;

impl FlagRecipes for X86Flags {
    fn defines(&self, shape: FlagShape) -> Fields {
        match shape {
            FlagShape::Inc | FlagShape::Dec => {
                Fields::FLAGS.difference([Field::Cf].into_iter().collect())
            }
            _ => Fields::FLAGS,
        }
    }

    fn expand(
        &self,
        shape: FlagShape,
        inputs: &[Varnode],
        fresh: &mut dyn FnMut(u32) -> Varnode,
    ) -> Vec<PcodeOp> {
        let f = Registers::get().arch().x86;
        let (&result, operands) = inputs.split_last().expect("flag inputs");
        let (a, b) = (operands[0], operands[1]);
        let n = result.size;
        let zero = Varnode::constant(0, n);
        let mut ops = Vec::new();
        let sign_bit = 1u64 << (n * 8 - 1);
        let mut overflow = |[w, x]: [Varnode; 2], [y, z]: [Varnode; 2], ops: &mut Vec<PcodeOp>| {
            let (left, right, both) = (fresh(n), fresh(n), fresh(n));
            ops.extend([
                op(OpCode::IntXor, left, vec![w, x]),
                op(OpCode::IntXor, right, vec![y, z]),
                op(OpCode::IntAnd, both, vec![left, right]),
                op(OpCode::IntSLess, f.of, vec![both, zero]),
            ]);
        };
        match (shape, operands.get(2)) {
            (FlagShape::Adc | FlagShape::Sbb, Some(&c)) => {
                let (combine, carry, overflow) = if shape == FlagShape::Adc {
                    (OpCode::IntAdd, OpCode::IntCarry, OpCode::IntSCarry)
                } else {
                    (OpCode::IntSub, OpCode::IntLess, OpCode::IntSBorrow)
                };
                let partial = fresh(n);
                let (c1, c2, o1, o2) = (fresh(1), fresh(1), fresh(1), fresh(1));
                ops.extend([
                    op(combine, partial, vec![a, b]),
                    op(carry, c1, vec![a, b]),
                    op(carry, c2, vec![partial, c]),
                    op(OpCode::BoolOr, f.cf, vec![c1, c2]),
                    op(overflow, o1, vec![a, b]),
                    op(overflow, o2, vec![partial, c]),
                    op(OpCode::BoolXor, f.of, vec![o1, o2]),
                ]);
            }
            (FlagShape::Add | FlagShape::Adc, _) => {
                ops.push(op(OpCode::IntLess, f.cf, vec![result, a]));
                overflow([a, result], [b, result], &mut ops);
            }
            (FlagShape::Sub | FlagShape::Cmp | FlagShape::Sbb, _) => {
                ops.push(op(OpCode::IntLess, f.cf, vec![a, b]));
                overflow([a, b], [a, result], &mut ops);
            }
            (FlagShape::Inc, _) => ops.push(op(
                OpCode::IntEqual,
                f.of,
                vec![result, Varnode::constant(sign_bit, n)],
            )),
            (FlagShape::Dec, _) => ops.push(op(
                OpCode::IntEqual,
                f.of,
                vec![result, Varnode::constant(sign_bit - 1, n)],
            )),
            (FlagShape::Logic, _) => {
                ops.push(copy(f.cf, Varnode::constant(0, 1)));
                ops.push(copy(f.of, Varnode::constant(0, 1)));
            }
        }
        let low = fresh(n);
        let count = fresh(1);
        let parity = fresh(1);
        ops.extend([
            op(OpCode::IntSLess, f.sf, vec![result, zero]),
            op(OpCode::IntEqual, f.zf, vec![result, zero]),
            op(
                OpCode::IntAnd,
                low,
                vec![result, Varnode::constant(0xff, n)],
            ),
            op(OpCode::PopCount, count, vec![low]),
            op(OpCode::IntAnd, parity, vec![count, Varnode::constant(1, 1)]),
            op(
                OpCode::IntEqual,
                f.pf,
                vec![parity, Varnode::constant(0, 1)],
            ),
        ]);
        if shape == FlagShape::Logic {
            ops.push(copy(f.af, Varnode::constant(0, 1)));
        } else {
            let (x, y, z) = (fresh(n), fresh(n), fresh(n));
            ops.extend([
                op(OpCode::IntXor, x, vec![a, b]),
                op(OpCode::IntXor, y, vec![x, result]),
                op(OpCode::IntRight, z, vec![y, Varnode::constant(4, n)]),
                op(OpCode::IntAnd, f.af, vec![z, Varnode::constant(1, n)]),
            ]);
        }
        ops
    }

    fn reads(&self, cond: Cond) -> Vec<Varnode> {
        let f = Registers::get().arch().x86;
        match cond {
            Cond::Overflow | Cond::NoOverflow => vec![f.of],
            Cond::Below | Cond::AboveEqual => vec![f.cf],
            Cond::Equal | Cond::NotEqual => vec![f.zf],
            Cond::BelowEqual | Cond::Above => vec![f.cf, f.zf],
            Cond::Sign | Cond::NoSign => vec![f.sf],
            Cond::Parity | Cond::NoParity => vec![f.pf],
            Cond::Less | Cond::GreaterEqual => vec![f.of, f.sf],
            Cond::LessEqual | Cond::Greater => vec![f.zf, f.of, f.sf],
        }
    }

    fn condition(
        &self,
        cond: Cond,
        output: Varnode,
        fresh: &mut dyn FnMut(u32) -> Varnode,
    ) -> Vec<PcodeOp> {
        let f = Registers::get().arch().x86;
        let not = |input| op(OpCode::BoolNegate, output, vec![input]);
        match cond {
            Cond::Overflow => vec![copy(output, f.of)],
            Cond::NoOverflow => vec![not(f.of)],
            Cond::Below => vec![copy(output, f.cf)],
            Cond::AboveEqual => vec![not(f.cf)],
            Cond::Equal => vec![copy(output, f.zf)],
            Cond::NotEqual => vec![not(f.zf)],
            Cond::Sign => vec![copy(output, f.sf)],
            Cond::NoSign => vec![not(f.sf)],
            Cond::Parity => vec![copy(output, f.pf)],
            Cond::NoParity => vec![not(f.pf)],
            Cond::BelowEqual => vec![op(OpCode::BoolOr, output, vec![f.cf, f.zf])],
            Cond::Above => {
                let either = fresh(1);
                vec![op(OpCode::BoolOr, either, vec![f.cf, f.zf]), not(either)]
            }
            Cond::Less => vec![op(OpCode::IntNotEqual, output, vec![f.of, f.sf])],
            Cond::GreaterEqual => vec![op(OpCode::IntEqual, output, vec![f.of, f.sf])],
            Cond::LessEqual => {
                let less = fresh(1);
                vec![
                    op(OpCode::IntNotEqual, less, vec![f.of, f.sf]),
                    op(OpCode::BoolOr, output, vec![f.zf, less]),
                ]
            }
            Cond::Greater => {
                let nonzero = fresh(1);
                let same = fresh(1);
                vec![
                    op(OpCode::BoolNegate, nonzero, vec![f.zf]),
                    op(OpCode::IntEqual, same, vec![f.of, f.sf]),
                    op(OpCode::BoolAnd, output, vec![nonzero, same]),
                ]
            }
        }
    }

    fn direct(
        &self,
        shape: FlagShape,
        cond: Cond,
        inputs: &[Varnode],
        output: Varnode,
    ) -> Option<Vec<PcodeOp>> {
        let &[a, b, result] = inputs else { return None };
        let zero = Varnode::constant(0, result.size);
        let (code, x, y) = match (shape, cond) {
            (FlagShape::Logic, Cond::Equal | Cond::BelowEqual) => (OpCode::IntEqual, result, zero),
            (FlagShape::Logic, Cond::NotEqual | Cond::Above) => (OpCode::IntNotEqual, result, zero),
            (FlagShape::Logic, Cond::Sign | Cond::Less) => (OpCode::IntSLess, result, zero),
            (FlagShape::Logic, Cond::NoSign | Cond::GreaterEqual) => {
                (OpCode::IntSLessEqual, zero, result)
            }
            (FlagShape::Logic, Cond::LessEqual) => (OpCode::IntSLessEqual, result, zero),
            (FlagShape::Logic, Cond::Greater) => (OpCode::IntSLess, zero, result),
            (FlagShape::Sub | FlagShape::Cmp, Cond::Below) => (OpCode::IntLess, a, b),
            (FlagShape::Sub | FlagShape::Cmp, Cond::AboveEqual) => (OpCode::IntLessEqual, b, a),
            (FlagShape::Sub | FlagShape::Cmp, Cond::BelowEqual) => (OpCode::IntLessEqual, a, b),
            (FlagShape::Sub | FlagShape::Cmp, Cond::Above) => (OpCode::IntLess, b, a),
            (FlagShape::Sub | FlagShape::Cmp | FlagShape::Dec, cond) => match cond {
                Cond::Equal => (OpCode::IntEqual, a, b),
                Cond::NotEqual => (OpCode::IntNotEqual, a, b),
                Cond::Less => (OpCode::IntSLess, a, b),
                Cond::GreaterEqual => (OpCode::IntSLessEqual, b, a),
                Cond::LessEqual => (OpCode::IntSLessEqual, a, b),
                Cond::Greater => (OpCode::IntSLess, b, a),
                _ => return None,
            },
            _ => return None,
        };
        Some(vec![op(code, output, vec![x, y])])
    }
}

pub(crate) fn cond(mnemonic: &str) -> Option<Cond> {
    let suffix = ["CMOV", "SET", "J"]
        .iter()
        .find_map(|prefix| mnemonic.strip_prefix(prefix))?;
    Some(match suffix {
        "O" => Cond::Overflow,
        "NO" => Cond::NoOverflow,
        "B" | "C" | "NAE" => Cond::Below,
        "AE" | "NB" | "NC" => Cond::AboveEqual,
        "E" | "Z" => Cond::Equal,
        "NE" | "NZ" => Cond::NotEqual,
        "BE" | "NA" => Cond::BelowEqual,
        "A" | "NBE" => Cond::Above,
        "S" => Cond::Sign,
        "NS" => Cond::NoSign,
        "P" | "PE" => Cond::Parity,
        "NP" | "PO" => Cond::NoParity,
        "L" | "NGE" => Cond::Less,
        "GE" | "NL" => Cond::GreaterEqual,
        "LE" | "NG" => Cond::LessEqual,
        "G" | "NLE" => Cond::Greater,
        _ => return None,
    })
}
