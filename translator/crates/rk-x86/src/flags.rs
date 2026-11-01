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
        let flags = Registers::get().arch().x86;
        let &[a, b, result] = inputs else {
            panic!("{shape:?} flags take a, b and the result");
        };
        let (carry, overflow) = match shape {
            FlagShape::Add => (OpCode::IntCarry, OpCode::IntSCarry),
            FlagShape::Sub | FlagShape::Cmp => (OpCode::IntLess, OpCode::IntSBorrow),
            _ => panic!("{shape:?} flags are not lazy"),
        };
        let zero = Varnode::constant(0, result.size);
        let low = fresh(result.size);
        let count = fresh(1);
        let parity = fresh(1);
        let x = fresh(result.size);
        let y = fresh(result.size);
        let z = fresh(result.size);
        vec![
            op(carry, flags.cf, vec![a, b]),
            op(overflow, flags.of, vec![a, b]),
            op(OpCode::IntSLess, flags.sf, vec![result, zero]),
            op(OpCode::IntEqual, flags.zf, vec![result, zero]),
            op(
                OpCode::IntAnd,
                low,
                vec![result, Varnode::constant(0xff, result.size)],
            ),
            PcodeOp {
                opcode: OpCode::PopCount,
                output: Some(count),
                inputs: vec![low],
                userop: None,
            },
            op(OpCode::IntAnd, parity, vec![count, Varnode::constant(1, 1)]),
            op(
                OpCode::IntEqual,
                flags.pf,
                vec![parity, Varnode::constant(0, 1)],
            ),
            op(OpCode::IntXor, x, vec![a, b]),
            op(OpCode::IntXor, y, vec![x, result]),
            op(
                OpCode::IntRight,
                z,
                vec![y, Varnode::constant(4, result.size)],
            ),
            op(
                OpCode::IntAnd,
                flags.af,
                vec![z, Varnode::constant(1, result.size)],
            ),
        ]
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
        if !matches!(shape, FlagShape::Sub | FlagShape::Cmp) {
            return None;
        }
        let &[a, b, _] = inputs else { return None };
        let (code, x, y) = match cond {
            Cond::Equal => (OpCode::IntEqual, a, b),
            Cond::NotEqual => (OpCode::IntNotEqual, a, b),
            Cond::Below => (OpCode::IntLess, a, b),
            Cond::AboveEqual => (OpCode::IntLessEqual, b, a),
            Cond::BelowEqual => (OpCode::IntLessEqual, a, b),
            Cond::Above => (OpCode::IntLess, b, a),
            Cond::Less => (OpCode::IntSLess, a, b),
            Cond::GreaterEqual => (OpCode::IntSLessEqual, b, a),
            Cond::LessEqual => (OpCode::IntSLessEqual, a, b),
            Cond::Greater => (OpCode::IntSLess, b, a),
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
