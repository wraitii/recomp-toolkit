use rk_core::ir::{Insn, OpCode, OperandKind, Space, Varnode};
use rk_ir::x87::{
    Arithmetic, ArithmeticOp, X87Condition, X87Eager, X87Environment, X87Legacy, X87Op, X87Recipe,
};
use rk_ir::{Op, Registers, X87Lowering, X87Recipes};

#[derive(Clone, Copy)]
enum Operand {
    St(u8),
    Ax,
    Mem {
        bits: u16,
        base: Option<u8>,
        index: Option<u8>,
        scale: u8,
        disp: i64,
        seg: bool,
    },
}

pub struct Recipes;

impl X87Recipes for Recipes {
    fn lower(&self, insn: &Insn, fresh: &mut dyn FnMut(u32) -> Varnode) -> Option<X87Lowering> {
        lower_record(insn, fresh)
    }
}

fn register(name: &str) -> Option<Varnode> {
    Registers::get().register(name)
}

fn address(
    operand: Operand,
    address_size: u8,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Option<(u16, Varnode, Vec<X87Recipe>)> {
    let Operand::Mem {
        bits,
        base,
        index,
        scale,
        disp,
        seg,
    } = operand
    else {
        return None;
    };
    if seg {
        return None;
    }
    let (registers, size, mask) = match address_size {
        2 => (&rk_x86::REG16[..], 2, 0xffffu64),
        4 => (&rk_x86::REG32[..], 4, 0xffff_ffffu64),
        _ => return None,
    };
    if address_size == 2 && scale != 1 && index.is_some() {
        return None;
    }
    let mut recipes = Vec::new();
    let mut value = Varnode::constant(disp as u64 & mask, size);
    for (index, factor) in [(base, 1u8), (index, scale)] {
        let Some(index) = index else { continue };
        let mut source = register(*registers.get(index as usize)?)?;
        if factor != 1 {
            let out = fresh(size);
            recipes.push(X87Recipe {
                op: Op::P(OpCode::IntMult),
                output: Some(out),
                inputs: vec![source, Varnode::constant(factor as u64, size)],
            });
            source = out;
        }
        let out = fresh(size);
        recipes.push(X87Recipe {
            op: Op::P(OpCode::IntAdd),
            output: Some(out),
            inputs: vec![value, source],
        });
        value = out;
    }
    Some((bits, value, recipes))
}

fn lowering(
    mut recipes: Vec<X87Recipe>,
    effect: X87Op,
    output: Option<Varnode>,
    inputs: Vec<Varnode>,
) -> X87Lowering {
    recipes.push(X87Recipe {
        op: Op::X87(effect),
        output,
        inputs,
    });
    X87Lowering { recipes }
}

fn lower_record(record: &Insn, fresh: &mut dyn FnMut(u32) -> Varnode) -> Option<X87Lowering> {
    let mnem = record.mnemonic.to_ascii_uppercase();
    let mut operands = Vec::new();
    for operand in &record.typed_operands {
        match &operand.kind {
            OperandKind::St(index) => operands.push(Operand::St(*index)),
            OperandKind::Reg { .. } if operand.display == "AX" => operands.push(Operand::Ax),
            OperandKind::Reg { .. } => return None,
            OperandKind::Mem {
                base,
                index,
                scale,
                disp,
                seg,
            } => operands.push(Operand::Mem {
                bits: operand.size?,
                base: *base,
                index: *index,
                scale: *scale,
                disp: *disp,
                seg: seg.as_deref().is_some_and(|seg| seg == "FS" || seg == "GS"),
            }),
            OperandKind::Other(text) => {
                let index = text
                    .strip_prefix("ST(")
                    .and_then(|text| text.strip_suffix(')'))
                    .and_then(|text| text.parse().ok())?;
                operands.push(Operand::St(index));
            }
            _ => return None,
        }
    }
    let memory = operands
        .iter()
        .find(|operand| matches!(operand, Operand::Mem { .. }))
        .copied();
    let slots: Vec<u8> = operands
        .iter()
        .filter_map(|operand| match operand {
            Operand::St(index) => Some(*index),
            _ => None,
        })
        .collect();
    let has_memory = memory.is_some();
    let address = memory.and_then(|operand| address(operand, record.address_size, fresh));
    if has_memory && address.is_none() {
        return None;
    }
    let (bits, inputs, recipes) = match address {
        Some((bits, address, recipes)) => (bits, vec![address], recipes),
        None => (0, Vec::new(), Vec::new()),
    };

    if matches!(
        mnem.as_str(),
        "FLD1" | "FLDZ" | "FLDPI" | "FLDLN2" | "FLDL2E" | "FLDLG2" | "FLDL2T"
    ) {
        return Some(lowering(
            recipes,
            X87Op::Constant(static_name(&mnem)),
            None,
            Vec::new(),
        ));
    }
    if mnem == "FLD" {
        let effect = match (slots.first(), memory) {
            (Some(index), _) => X87Op::Push {
                src: Some(*index),
                bits: 0,
            },
            (None, Some(_)) if matches!(bits, 32 | 64 | 80) => X87Op::Push { src: None, bits },
            _ => return None,
        };
        return Some(lowering(recipes, effect, None, inputs));
    }
    if mnem == "FILD" && matches!(bits, 16 | 32 | 64) {
        return Some(lowering(recipes, X87Op::PushInteger { bits }, None, inputs));
    }
    if matches!(mnem.as_str(), "FST" | "FSTP") {
        let pop = mnem.ends_with('P') && mnem != "FST";
        let effect = match (slots.first(), memory) {
            (Some(index), _) => X87Op::Store {
                dst: Some(*index),
                bits: 0,
                pop,
                integer: false,
            },
            (None, Some(_)) if matches!(bits, 32 | 64 | 80) => X87Op::Store {
                dst: None,
                bits,
                pop,
                integer: false,
            },
            _ => return None,
        };
        return Some(lowering(recipes, effect, None, inputs));
    }
    if matches!(mnem.as_str(), "FIST" | "FISTP") && matches!(bits, 16 | 32 | 64) {
        let pop = mnem == "FISTP";
        return Some(lowering(
            recipes,
            X87Op::Store {
                dst: None,
                bits,
                pop,
                integer: true,
            },
            None,
            inputs,
        ));
    }
    if let Some(arith) = arithmetic(&mnem) {
        let effect = if has_memory {
            X87Op::Arithmetic(Arithmetic {
                operator: arith.0,
                integer: arith.1,
                reverse: arith.2,
                pops: arith.3,
                dst: 0,
                src: 0,
                bits,
            })
        } else {
            let pops = arith.3;
            let dst = if slots.len() == 2 || pops != 0 {
                slots.first().copied().unwrap_or(0)
            } else {
                0
            };
            let src = if slots.len() == 2 {
                slots[1]
            } else if pops != 0 {
                0
            } else {
                slots.first().copied().unwrap_or(0)
            };
            X87Op::Arithmetic(Arithmetic {
                operator: arith.0,
                integer: arith.1,
                reverse: arith.2,
                pops,
                dst,
                src,
                bits: 0,
            })
        };
        return Some(lowering(recipes, effect, None, inputs));
    }
    if matches!(
        mnem.as_str(),
        "FCOM" | "FCOMP" | "FUCOM" | "FUCOMP" | "FCOMPP" | "FUCOMPP" | "FICOM" | "FICOMP" | "FTST"
    ) {
        let quiet = mnem.starts_with("FU");
        let integer = mnem.starts_with("FI");
        let pops = if mnem.ends_with("PP") {
            2
        } else if mnem.ends_with('P') && mnem != "FTST" {
            1
        } else {
            0
        };
        let effect = if mnem == "FTST" {
            X87Op::Compare {
                quiet,
                integer,
                src: 0,
                bits: 0,
                pops,
                zero: true,
            }
        } else if has_memory {
            X87Op::Compare {
                quiet,
                integer,
                src: 0,
                bits,
                pops,
                zero: false,
            }
        } else {
            X87Op::Compare {
                quiet,
                integer,
                src: slots.last().copied().unwrap_or(1),
                bits: 0,
                pops,
                zero: false,
            }
        };
        return Some(lowering(recipes, effect, None, inputs));
    }
    if matches!(mnem.as_str(), "FCOMI" | "FCOMIP" | "FUCOMI" | "FUCOMIP") {
        let out = fresh(2);
        let effect = X87Op::CompareFlags {
            quiet: mnem.starts_with("FU"),
            src: slots.last().copied().unwrap_or(1),
            pop: mnem.ends_with("IP"),
        };
        let lowering = lowering(recipes, effect, Some(out), inputs);
        return Some(post_flags(lowering, out, fresh));
    }
    if let Some(unary) = unary(&mnem) {
        return Some(lowering(recipes, X87Op::Unary(unary), None, Vec::new()));
    }
    if matches!(mnem.as_str(), "FPREM" | "FPREM1") {
        return Some(lowering(
            recipes,
            X87Op::Remainder {
                ieee: mnem == "FPREM1",
            },
            None,
            Vec::new(),
        ));
    }
    if mnem == "FXCH" {
        return Some(lowering(
            recipes,
            X87Op::Exchange {
                src: slots.last().copied().unwrap_or(1),
            },
            None,
            Vec::new(),
        ));
    }
    if mnem == "FFREE" {
        return Some(lowering(
            recipes,
            X87Op::Free {
                src: slots.last().copied().unwrap_or(0),
            },
            None,
            Vec::new(),
        ));
    }
    if matches!(mnem.as_str(), "FNSTSW" | "FSTSW")
        && operands.len() == 1
        && matches!(operands[0], Operand::Ax)
    {
        let out = fresh(2);
        let lowering = lowering(recipes, X87Op::Status, Some(out), Vec::new());
        let mut lowering = lowering;
        let ax = Registers::get().arch().eax.piece(0, 2);
        lowering.recipes.push(X87Recipe {
            op: Op::P(OpCode::Copy),
            output: Some(ax),
            inputs: vec![out],
        });
        return Some(lowering);
    }
    if matches!(mnem.as_str(), "FNSTSW" | "FSTSW") && bits == 16 {
        return Some(lowering(
            recipes,
            X87Op::Environment(X87Environment::StoreSw),
            None,
            inputs,
        ));
    }
    if matches!(mnem.as_str(), "FNSTCW" | "FSTCW") && bits == 16 {
        return Some(lowering(
            recipes,
            X87Op::Environment(X87Environment::StoreCw),
            None,
            inputs,
        ));
    }
    if mnem == "FLDCW" && bits == 16 {
        return Some(lowering(
            recipes,
            X87Op::Environment(X87Environment::LoadCw),
            None,
            inputs,
        ));
    }
    if mnem == "FLDENV" {
        return Some(lowering(
            recipes,
            X87Op::Environment(X87Environment::LoadEnv),
            None,
            inputs,
        ));
    }
    if matches!(mnem.as_str(), "FNSTENV" | "FSTENV") {
        return Some(lowering(
            recipes,
            X87Op::Environment(X87Environment::StoreEnv),
            None,
            inputs,
        ));
    }
    if matches!(mnem.as_str(), "FNSAVE" | "FSAVE") {
        return Some(lowering(
            recipes,
            X87Op::Environment(X87Environment::Save),
            None,
            inputs,
        ));
    }
    if mnem == "FRSTOR" {
        return Some(lowering(
            recipes,
            X87Op::Environment(X87Environment::Restore),
            None,
            inputs,
        ));
    }
    if matches!(mnem.as_str(), "FNINIT" | "FINIT") {
        return Some(lowering(recipes, X87Op::Init, None, Vec::new()));
    }
    if mnem == "FDECSTP" {
        return Some(lowering(recipes, X87Op::Rotate(-1), None, Vec::new()));
    }
    if mnem == "FINCSTP" {
        return Some(lowering(recipes, X87Op::Rotate(1), None, Vec::new()));
    }
    if matches!(mnem.as_str(), "FNCLEX" | "FCLEX") {
        return Some(lowering(recipes, X87Op::Clear, None, Vec::new()));
    }
    if mnem == "FNOP" {
        return Some(lowering(recipes, X87Op::Nop, None, Vec::new()));
    }
    if matches!(mnem.as_str(), "FBSTP") && bits == 80 {
        return Some(lowering(
            recipes,
            X87Op::LegacyEnvironment(X87Legacy::BcdStore),
            None,
            inputs,
        ));
    }
    if mnem == "FEMMS" || mnem == "EMMS" {
        return Some(lowering(
            recipes,
            X87Op::EmptyTags(mnem == "FEMMS"),
            None,
            Vec::new(),
        ));
    }
    if let Some(condition) = condition(&mnem) {
        return Some(lowering(
            recipes,
            X87Op::Conditional(condition, slots.last().copied().unwrap_or(0)),
            None,
            Vec::new(),
        ));
    }
    if let Some(eager) = eager(&mnem, bits) {
        return Some(lowering(recipes, X87Op::Eager(eager), None, inputs));
    }
    None
}

fn static_name(name: &str) -> &'static str {
    match name {
        "FLD1" => "FLD1",
        "FLDZ" => "FLDZ",
        "FLDPI" => "FLDPI",
        "FLDLN2" => "FLDLN2",
        "FLDL2E" => "FLDL2E",
        "FLDLG2" => "FLDLG2",
        "FLDL2T" => "FLDL2T",
        _ => "FLDZ",
    }
}

fn arithmetic(name: &str) -> Option<(ArithmeticOp, bool, bool, u8)> {
    let pops = name.ends_with('P');
    let base = if pops { &name[..name.len() - 1] } else { name };
    let integer = base.starts_with("FI");
    let base = if integer {
        format!("F{}", &base[2..])
    } else {
        base.to_owned()
    };
    let reverse = base.ends_with('R');
    let op = match base.as_str() {
        "FADD" => ArithmeticOp::Add,
        "FSUB" | "FSUBR" => ArithmeticOp::Sub,
        "FMUL" => ArithmeticOp::Mul,
        "FDIV" | "FDIVR" => ArithmeticOp::Div,
        _ => return None,
    };
    Some((op, integer, reverse, pops as u8))
}

fn unary(name: &str) -> Option<rk_ir::x87::X87Unary> {
    Some(match name {
        "FABS" => rk_ir::x87::X87Unary::Abs,
        "FCHS" => rk_ir::x87::X87Unary::Negate,
        "FSQRT" => rk_ir::x87::X87Unary::Sqrt,
        "FRNDINT" => rk_ir::x87::X87Unary::Round,
        "FSIN" => rk_ir::x87::X87Unary::Sin,
        "FCOS" => rk_ir::x87::X87Unary::Cos,
        "F2XM1" => rk_ir::x87::X87Unary::ExpM1,
        _ => return None,
    })
}

fn condition(name: &str) -> Option<X87Condition> {
    Some(match name {
        "FCMOVB" => X87Condition::B,
        "FCMOVE" => X87Condition::E,
        "FCMOVBE" => X87Condition::Be,
        "FCMOVU" => X87Condition::U,
        "FCMOVNB" => X87Condition::Nb,
        "FCMOVNE" => X87Condition::Ne,
        "FCMOVNBE" => X87Condition::Nbe,
        "FCMOVNU" => X87Condition::Nu,
        _ => return None,
    })
}

fn eager(name: &str, bits: u16) -> Option<X87Eager> {
    Some(match name {
        "FPTAN" => X87Eager::Fptan,
        "FSINCOS" => X87Eager::Fsincos,
        "FSCALE" => X87Eager::Fscale,
        "FPATAN" => X87Eager::Fpatan,
        "FYL2X" => X87Eager::Fyl2x,
        "FYL2XP1" => X87Eager::Fyl2xp1,
        "FXTRACT" => X87Eager::Xtract,
        "FBSTP" if bits == 80 => X87Eager::Fbstp,
        "FXAM" => X87Eager::Fxam,
        _ => return None,
    })
}

fn post_flags(
    mut lowering: X87Lowering,
    out: Varnode,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> X87Lowering {
    let arch = Registers::get().arch();
    let (cf, pf, zf, of, sf, af) = (arch.cf, arch.pf, arch.zf, arch.of, arch.sf, arch.af);
    let one = Varnode::constant(1, 1);
    let zero = Varnode::constant(0, 1);
    let mut bit = |lowering: &mut X87Lowering, shift: u64, out: Varnode| {
        let shifted = if shift == 0 {
            out
        } else {
            let value = fresh(2);
            lowering.recipes.push(X87Recipe {
                op: Op::P(OpCode::IntRight),
                output: Some(value),
                inputs: vec![out, Varnode::constant(shift, 2)],
            });
            value
        };
        let value = fresh(1);
        lowering.recipes.push(X87Recipe {
            op: Op::P(OpCode::IntAnd),
            output: Some(value),
            inputs: vec![shifted, one],
        });
        value
    };
    let cf_value = bit(&mut lowering, 0, out);
    let pf_value = bit(&mut lowering, 2, out);
    let zf_value = bit(&mut lowering, 6, out);
    for (target, value) in [(cf, cf_value), (pf, pf_value), (zf, zf_value)] {
        lowering.recipes.push(X87Recipe {
            op: Op::P(OpCode::Copy),
            output: Some(target),
            inputs: vec![value],
        });
    }
    for target in [of, sf, af] {
        lowering.recipes.push(X87Recipe {
            op: Op::P(OpCode::Copy),
            output: Some(target),
            inputs: vec![zero],
        });
    }
    let _ = Space::Register;
    lowering
}
