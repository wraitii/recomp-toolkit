use rk_core::ir::{OpCode, Varnode};
use rk_ir::x87::{ArithmeticOp, X87Eager, X87Environment, X87Unary};
use rk_ir::X87Op;

use crate::{op_code, rp_op, RpOp};

pub fn encode(
    op: X87Op,
    output: Option<Varnode>,
    inputs: &[Varnode],
) -> Option<Result<RpOp, String>> {
    let (kind, a, b, c, d) = match op {
        X87Op::Constant(name) => (
            14,
            match name {
                "FLDZ" => 0,
                "FLD1" => 1,
                "FLDPI" => 2,
                "FLDLN2" => 3,
                "FLDL2E" => 4,
                "FLDLG2" => 5,
                "FLDL2T" => 6,
                _ => return None,
            },
            0,
            0,
            0,
        ),
        X87Op::Environment(X87Environment::StoreSw) => (15, 0, 16, 0, 0),
        X87Op::Eager(X87Eager::Fxam) => (16, 0, 0, 0, 0),
        X87Op::Remainder { ieee } => (17, ieee as u64, 0, 0, 0),
        X87Op::Eager(X87Eager::Fyl2x) => (18, 0, 0, 0, 0),
        X87Op::Eager(X87Eager::Fscale) => (19, 0, 0, 0, 0),
        X87Op::Eager(X87Eager::Fptan) => (20, 0, 0, 0, 0),
        X87Op::Push { src, bits } => (1, src.map_or(8, u64::from), bits as u64, 0, 0),
        X87Op::PushInteger { bits } => (2, 0, bits as u64, 0, 0),
        X87Op::Store {
            dst,
            bits,
            pop,
            integer,
        } => (
            3,
            dst.map_or(8, u64::from),
            bits as u64,
            pop as u64,
            integer as u64,
        ),
        X87Op::Arithmetic(v) => (
            4,
            v.dst as u64 | ((v.src as u64) << 4),
            v.bits as u64,
            (match v.operator {
                ArithmeticOp::Add => 0,
                ArithmeticOp::Sub => 1,
                ArithmeticOp::Mul => 2,
                ArithmeticOp::Div => 3,
            }) | ((v.reverse as u64) << 2)
                | ((v.integer as u64) << 3),
            v.pops as u64,
        ),
        X87Op::Compare {
            quiet,
            integer,
            src,
            bits,
            pops,
            zero,
        } => (
            5,
            src as u64,
            bits as u64,
            quiet as u64 | ((integer as u64) << 1) | ((zero as u64) << 2),
            pops as u64,
        ),
        X87Op::CompareFlags { quiet, src, pop } => (6, src as u64, 0, quiet as u64, pop as u64),
        X87Op::Exchange { src } => (7, src as u64, 0, 0, 0),
        X87Op::Free { src } => (8, src as u64, 0, 0, 0),
        X87Op::Status => (9, 0, 0, 0, 0),
        X87Op::Init => (10, 0, 0, 0, 0),
        X87Op::Rotate(n) => (11, (n > 0) as u64, 0, 0, 0),
        X87Op::Nop => (12, 0, 0, 0, 0),
        X87Op::Unary(v) => (
            13,
            match v {
                X87Unary::Abs => 0,
                X87Unary::Negate => 1,
                X87Unary::Sqrt => 2,
                X87Unary::Round => 3,
                X87Unary::Sin => 4,
                X87Unary::Cos => 5,
                X87Unary::ExpM1 => 6,
            },
            0,
            0,
            0,
        ),
        _ => return None,
    };
    let id = 0x100000000u64 | kind | (a << 8) | (b << 16) | (c << 24) | (d << 28);
    let mut args = vec![Varnode::constant(id, 8)];
    args.extend_from_slice(inputs);
    Some(rp_op(op_code(OpCode::CallOther), output, &args, Some(id)))
}
