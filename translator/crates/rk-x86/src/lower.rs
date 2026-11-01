use rk_core::ir::{Insn, OpCode, PcodeOp, Space, Varnode};
use rk_core::{Addr, Reason};
use rk_ir::lower::{cg, copy, from_pcode, normalize_direct_ram, op, prepare_rmw};
use rk_ir::{
    CgOp, FlagShape, InstructionLowerer, IsaLowering, Lowered, Op, SlowFamily, X87Recipes,
    EFLAGS_MISC,
};

use crate::correct::{
    correct_arithmetic, correct_bsr, correct_div_cg, correct_extra, correct_neg, correct_sar,
    correct_shift, correct_sse, defer_flags, with_af_zero,
};
use crate::string;

pub struct Lowering<'a> {
    pub x87: &'a dyn X87Recipes,
}

fn plain(ops: Vec<CgOp>) -> Lowered {
    Lowered {
        ops,
        x87: false,
        flag_operands: None,
        apply_policy: false,
    }
}

fn policy(ops: Vec<CgOp>) -> Lowered {
    Lowered {
        apply_policy: true,
        ..plain(ops)
    }
}

impl IsaLowering for Lowering<'_> {
    fn lower(
        &self,
        insn: &Insn,
        import_tail: Option<Addr>,
        cx: &mut InstructionLowerer,
    ) -> rk_core::Result<Lowered> {
        let regs = cx.regs();
        let mnem = insn.mnemonic.to_ascii_uppercase();
        if mnem == "WAIT" || mnem == "NOP" || mnem == "PAUSE" {
            return Ok(plain(Vec::new()));
        }
        if matches!(mnem.as_str(), "PUSHFD" | "POPFD" | "PUSHF" | "POPF") {
            let push = mnem.starts_with("PUSH");
            let size = if mnem.ends_with('D') { 4 } else { 2 };
            let stack = regs.arch().stack_pointer;
            let r = regs.arch().x86;
            let flags = [
                (r.cf, 0),
                (r.pf, 2),
                (r.af, 4),
                (r.zf, 6),
                (r.sf, 7),
                (r.df, 10),
                (r.of, 11),
            ];
            let mut ops = Vec::new();
            if push {
                let mut value = cx.fresh(4);
                ops.push(op(
                    OpCode::IntAnd,
                    value,
                    vec![EFLAGS_MISC, Varnode::constant(!0xcd5u32 as u64, 4)],
                ));
                let fixed = cx.fresh(4);
                ops.push(op(
                    OpCode::IntOr,
                    fixed,
                    vec![value, Varnode::constant(2, 4)],
                ));
                value = fixed;
                for (flag, bit) in flags {
                    let wide = cx.fresh(4);
                    let shifted = cx.fresh(4);
                    let combined = cx.fresh(4);
                    ops.push(op(OpCode::IntZExt, wide, vec![flag]));
                    ops.push(op(
                        OpCode::IntLeft,
                        shifted,
                        vec![wide, Varnode::constant(bit, 4)],
                    ));
                    ops.push(op(OpCode::IntOr, combined, vec![value, shifted]));
                    value = combined;
                }
                let new_stack = cx.fresh(4);
                ops.push(op(
                    OpCode::IntSub,
                    new_stack,
                    vec![stack, Varnode::constant(size, 4)],
                ));
                ops.push(copy(stack, new_stack));
                if size == 2 {
                    let short = cx.fresh(2);
                    ops.push(op(
                        OpCode::SubPiece,
                        short,
                        vec![value, Varnode::constant(0, 4)],
                    ));
                    value = short;
                }
                ops.push(PcodeOp {
                    opcode: OpCode::Store,
                    output: None,
                    inputs: vec![new_stack, value],
                    userop: None,
                });
            } else {
                let value = cx.fresh(size as u32);
                ops.push(op(OpCode::Load, value, vec![stack]));
                let wide = if size == 2 {
                    let extended = cx.fresh(4);
                    let high = cx.fresh(4);
                    let combined = cx.fresh(4);
                    ops.push(op(OpCode::IntZExt, extended, vec![value]));
                    ops.push(op(
                        OpCode::IntAnd,
                        high,
                        vec![EFLAGS_MISC, Varnode::constant(0xffff0000, 4)],
                    ));
                    ops.push(op(OpCode::IntOr, combined, vec![extended, high]));
                    combined
                } else {
                    value
                };
                let misc = cx.fresh(4);
                ops.push(op(
                    OpCode::IntAnd,
                    misc,
                    vec![wide, Varnode::constant(!0xcd5u32 as u64, 4)],
                ));
                ops.push(op(
                    OpCode::IntOr,
                    EFLAGS_MISC,
                    vec![misc, Varnode::constant(2, 4)],
                ));
                for (flag, bit) in flags {
                    let shifted = cx.fresh(4);
                    let byte = cx.fresh(1);
                    ops.push(op(
                        OpCode::IntRight,
                        shifted,
                        vec![wide, Varnode::constant(bit, 4)],
                    ));
                    ops.push(op(
                        OpCode::SubPiece,
                        byte,
                        vec![shifted, Varnode::constant(0, 4)],
                    ));
                    ops.push(op(
                        OpCode::IntAnd,
                        flag,
                        vec![byte, Varnode::constant(1, 1)],
                    ));
                }
                ops.push(op(
                    OpCode::IntAdd,
                    stack,
                    vec![stack, Varnode::constant(size, 4)],
                ));
            }
            return Ok(plain(ops.into_iter().map(from_pcode).collect()));
        }
        if mnem == "INT3" {
            return Ok(plain(vec![cg(Op::Breakpoint)]));
        }
        if mnem == "MOV"
            && insn
                .operands
                .split(',')
                .next()
                .is_some_and(|op| op.trim().eq_ignore_ascii_case("CS"))
        {
            let op = cx.slow(insn, SlowFamily::Segment);
            return Ok(plain(vec![cg(op)]));
        }
        if matches!(mnem.as_str(), "FXSAVE" | "STMXCSR" | "LDMXCSR" | "SFENCE") {
            let op = cx.slow(insn, SlowFamily::Sse);
            return Ok(plain(vec![cg(op)]));
        }
        if insn.x87 || mnem.starts_with('F') || mnem == "EMMS" {
            let lowering = self.x87.lower(insn, &mut |size| cx.fresh(size));
            if let Some(lowering) = lowering {
                let ops = lowering
                    .recipes
                    .into_iter()
                    .map(|recipe| CgOp {
                        op: recipe.op,
                        output: recipe.output,
                        inputs: recipe.inputs,
                    })
                    .collect();
                return Ok(Lowered {
                    x87: true,
                    ..plain(ops)
                });
            }
            let op = cx.slow(insn, SlowFamily::X87);
            return Ok(Lowered {
                x87: true,
                ..plain(vec![cg(op)])
            });
        }
        let raw = &insn.raw;
        if let Some(helper) = string::helper(raw) {
            return Ok(plain(vec![cg(Op::Native(helper))]));
        }
        if let Some(slot) = import_tail {
            return Ok(plain(vec![cg(Op::ImportTail { slot })]));
        }
        let mut stripped = Vec::new();
        let raw_ops: &[PcodeOp] = if insn.ops.iter().any(|op| {
            op.opcode == OpCode::CallOther
                && matches!(regs.userop(op.userop), Some("LOCK" | "UNLOCK"))
        }) {
            stripped.extend(
                insn.ops
                    .iter()
                    .filter(|op| {
                        !(op.opcode == OpCode::CallOther
                            && matches!(regs.userop(op.userop), Some("LOCK" | "UNLOCK")))
                    })
                    .cloned(),
            );
            &stripped
        } else {
            &insn.ops
        };
        if let Some(call) = raw_ops.iter().find(|op| op.opcode == OpCode::CallOther) {
            let handled = matches!(
                mnem.as_str(),
                "MINPS"
                    | "MAXPS"
                    | "DIVPS"
                    | "SQRTPS"
                    | "RCPPS"
                    | "RSQRTPS"
                    | "RCPSS"
                    | "RSQRTSS"
                    | "MOVMSKPS"
            );
            if !handled {
                let family = call_other_family(&mnem, regs.userop(call.userop));
                let op = cx.slow(insn, family);
                return Ok(plain(vec![cg(op)]));
            }
        }
        let divide_by_zero = matches!(mnem.as_str(), "AAM") && insn.raw.last() == Some(&0);
        if divide_by_zero || !SUPPORTED.contains(&mnem.as_str()) {
            let family = unmodelled_family(&mnem);
            let op = cx.reject(
                insn,
                Reason::Unmodelled {
                    mnemonic: mnem.clone(),
                },
                family,
            );
            return Ok(plain(vec![cg(op)]));
        }
        let ops = match normalize_direct_ram(raw_ops, &mut |size| cx.fresh(size)) {
            Ok(ops) => ops,
            Err(reason) => {
                let op = cx.reject(insn, reason, SlowFamily::Unmodelled);
                return Ok(plain(vec![cg(op)]));
            }
        };
        if matches!(mnem.as_str(), "SHUFPS" | "PSHUFD") {
            if let Some(shuffled) = lower_shuffle(&mnem, &ops, insn, &mut |size| cx.fresh(size)) {
                let cut = shuffled.len() - 4;
                let lanes = &shuffled[cut..];
                if let Some(first) = lanes[0].output {
                    if lanes.iter().enumerate().all(|(index, lane)| {
                        lane.opcode == OpCode::Copy
                            && lane.output
                                == Some(Varnode::new(
                                    Space::Register,
                                    first.offset + index as u64 * 4,
                                    4,
                                ))
                            && lane.inputs.len() == 1
                    }) {
                        let snapshots: Vec<&PcodeOp> = shuffled[..cut]
                            .iter()
                            .filter(|item| {
                                item.opcode == OpCode::Copy
                                    && item
                                        .output
                                        .is_some_and(|v| v.size == 4 && v.space == Space::Unique)
                                    && item.inputs.len() == 1
                                    && item.inputs[0].size == 4
                            })
                            .take(if mnem == "SHUFPS" { 8 } else { 4 })
                            .collect();
                        let wide_sources: Option<Vec<Varnode>> = snapshots
                            .chunks(4)
                            .map(|group| {
                                let first = group.first()?.inputs[0];
                                if group.len() != 4
                                    || !matches!(first.space, Space::Register | Space::Unique)
                                    || !group.iter().enumerate().all(|(i, item)| {
                                        item.inputs[0]
                                            == Varnode::new(
                                                first.space,
                                                first.offset + 4 * i as u64,
                                                4,
                                            )
                                    })
                                {
                                    return None;
                                }
                                Some(Varnode::new(first.space, first.offset, 16))
                            })
                            .collect();
                        let selectors: Option<Vec<u8>> = lanes
                            .iter()
                            .map(|lane| {
                                snapshots
                                    .iter()
                                    .position(|item| item.output == Some(lane.inputs[0]))
                                    .map(|i| i as u8)
                            })
                            .collect();
                        let (mut result, op, inputs): (Vec<CgOp>, _, _) =
                            if let (Some(inputs), Some(selectors)) = (wide_sources, selectors) {
                                let result = shuffled[..cut]
                                    .iter()
                                    .filter(|item| {
                                        !snapshots
                                            .iter()
                                            .any(|snapshot| std::ptr::eq(*snapshot, *item))
                                    })
                                    .cloned()
                                    .map(|op| cx.convert(op))
                                    .collect();
                                (
                                    result,
                                    rk_ir::SimdOp::Shuffle {
                                        lane_bytes: 4,
                                        lanes: selectors.try_into().unwrap(),
                                    },
                                    inputs,
                                )
                            } else {
                                (
                                    shuffled[..cut]
                                        .iter()
                                        .cloned()
                                        .map(|op| cx.convert(op))
                                        .collect(),
                                    rk_ir::SimdOp::ShuffleScalars { lane_bytes: 4 },
                                    lanes.iter().map(|lane| lane.inputs[0]).collect(),
                                )
                            };
                        result.push(CgOp {
                            op: Op::Simd(op),
                            output: Some(Varnode::new(Space::Register, first.offset, 16)),
                            inputs,
                        });
                        return Ok(policy(result));
                    }
                }
            }
        }
        if mnem == "PSRLDQ" {
            let shift = sse_immediate(insn).and_then(|value| u8::try_from(value).ok());
            let item = ops.iter().find(|op| {
                op.opcode == OpCode::IntRight && op.output.is_some_and(|v| v.size == 16)
            });
            if let (Some(shift), Some(item)) = (shift, item) {
                if let (Some(output), Some(source)) = (item.output, item.inputs.first()) {
                    return Ok(policy(vec![CgOp {
                        op: Op::Simd(rk_ir::SimdOp::ShiftBytesRight { bytes: shift }),
                        output: Some(output),
                        inputs: vec![*source],
                    }]));
                }
            }
            let op = cx.reject(
                insn,
                Reason::Unsupported {
                    detail: "unsupported PSRLDQ p-code shape".into(),
                },
                SlowFamily::Unmodelled,
            );
            return Ok(plain(vec![cg(op)]));
        }
        let ops = match correct_sse(&mnem, &ops, regs, insn, &mut |size| cx.fresh(size)) {
            Ok(ops) => ops,
            Err(reason) => {
                let op = cx.reject(insn, reason, SlowFamily::Unmodelled);
                return Ok(plain(vec![cg(op)]));
            }
        };
        let ops = match correct_extra(&mnem, &ops, regs, insn, &mut |size| cx.fresh(size)) {
            Ok(ops) => ops,
            Err(reason) => {
                let op = cx.reject(insn, reason, SlowFamily::Unmodelled);
                return Ok(plain(vec![cg(op)]));
            }
        };
        if mnem == "DIV" || mnem == "IDIV" {
            let (converted, has_address) =
                match correct_div_cg(&mnem, &ops, regs, insn.addr, &mut |size| cx.fresh(size)) {
                    Ok(ops) => (ops, true),
                    Err(reason) => {
                        let op = cx.reject(insn, reason, SlowFamily::Unmodelled);
                        (vec![cg(op)], false)
                    }
                };
            return Ok(Lowered {
                ops: converted,
                x87: false,
                flag_operands: None,
                apply_policy: has_address,
            });
        }
        let rewrites_memory = matches!(
            mnem.as_str(),
            "ADD"
                | "SUB"
                | "CMP"
                | "INC"
                | "DEC"
                | "ADC"
                | "SBB"
                | "SHL"
                | "SHR"
                | "SAR"
                | "NEG"
                | "AND"
                | "OR"
                | "XOR"
                | "NOT"
                | "ROL"
                | "ROR"
                | "RCL"
                | "RCR"
                | "SHLD"
                | "SHRD"
        ) && ops.iter().any(|op| op.opcode == OpCode::Store);
        let prepared = if rewrites_memory {
            prepare_rmw(&ops, &mut |size| cx.fresh(size))
        } else {
            Ok(ops.clone())
        };
        let mut flag_operands = None;
        let mut lazy_flags = None;
        let corrected = prepared.and_then(|ops| {
            let corrected = match mnem.as_str() {
                "ADD" | "SUB" | "CMP" | "INC" | "DEC" | "ADC" | "SBB" => correct_arithmetic(
                    &mnem,
                    &ops,
                    regs,
                    &mut |size| cx.fresh(size),
                    &mut flag_operands,
                    &mut lazy_flags,
                ),
                "SHL" | "SHR" => correct_shift(&mnem, &ops, regs, &mut |size| cx.fresh(size)),
                "SAR" => correct_sar(&ops, regs, &mut |size| cx.fresh(size)),
                "BSR" => correct_bsr(&ops, regs, &mut |size| cx.fresh(size)),
                "NEG" => correct_neg(&ops, regs, &mut |size| cx.fresh(size)),
                "AND" | "OR" | "XOR" | "TEST" => Ok(with_af_zero(&ops, regs)),
                _ => Ok(ops.clone()),
            }?;
            Ok(if rewrites_memory {
                defer_flags(corrected, regs, &mut |size| cx.fresh(size))
            } else {
                corrected
            })
        });
        let ops = match corrected {
            Ok(ops) => ops,
            Err(reason) => {
                let family = match mnem.as_str() {
                    "ADC" => SlowFamily::Adc,
                    "SBB" => SlowFamily::Sbb,
                    "ROL" => SlowFamily::Rol,
                    "ROR" => SlowFamily::Ror,
                    "RCL" => SlowFamily::Rcl,
                    "RCR" => SlowFamily::Rcr,
                    "SHLD" => SlowFamily::Shld,
                    "SHRD" => SlowFamily::Shrd,
                    _ => SlowFamily::Unmodelled,
                };
                let op = cx.reject(insn, reason, family);
                return Ok(plain(vec![cg(op)]));
            }
        };
        let mut converted: Vec<CgOp> = ops
            .into_iter()
            .map(|op| {
                let mut converted = cx.convert(op);
                if matches!(converted.op, Op::CallOther(_)) {
                    if let Some(simd) = crate::wide::semantic(&mnem) {
                        converted.op = Op::Simd(simd);
                    }
                }
                converted
            })
            .collect();
        if let Some(cond) = crate::flags::cond(&mnem) {
            converted = with_cond(converted, cond, cx.fresh(1));
        }
        if let (Some(inputs), Some(shape)) = (lazy_flags, self.flag_shape(insn)) {
            converted.push(CgOp {
                op: Op::Flags(shape),
                output: None,
                inputs,
            });
        }
        Ok(Lowered {
            ops: converted,
            x87: false,
            flag_operands,
            apply_policy: true,
        })
    }

    fn flag_recipes(&self) -> &'static dyn rk_ir::FlagRecipes {
        &crate::flags::X86Flags
    }

    fn flag_shape(&self, insn: &Insn) -> Option<FlagShape> {
        match insn.mnemonic.to_ascii_uppercase().as_str() {
            "ADD" => Some(FlagShape::Add),
            "SUB" => Some(FlagShape::Sub),
            "CMP" => Some(FlagShape::Cmp),
            "INC" => Some(FlagShape::Inc),
            "DEC" => Some(FlagShape::Dec),
            "ADC" => Some(FlagShape::Adc),
            "SBB" => Some(FlagShape::Sbb),
            "AND" | "OR" | "XOR" | "TEST" => Some(FlagShape::Logic),
            _ => None,
        }
    }
}

fn with_cond(ops: Vec<CgOp>, cond: rk_ir::Cond, output: Varnode) -> Vec<CgOp> {
    let f = rk_ir::Registers::get().arch().x86;
    let mut derived = vec![f.cf, f.pf, f.af, f.zf, f.sf, f.of];
    let mut out = vec![CgOp {
        op: Op::Cond(cond),
        output: Some(output),
        inputs: rk_ir::flag_recipes().reads(cond),
    }];
    for mut op in ops {
        let from_flags = op.output.is_some_and(|v| v.size == 1)
            && op.inputs.iter().any(|v| derived.contains(v))
            && op
                .inputs
                .iter()
                .all(|v| v.space == Space::Const || derived.contains(v));
        match op.output {
            Some(target) if from_flags && target.space == Space::Unique => {
                derived.push(target);
                continue;
            }
            Some(target) if from_flags => out.push(CgOp {
                op: Op::P(OpCode::Copy),
                output: Some(target),
                inputs: vec![output],
            }),
            _ => {
                for input in &mut op.inputs {
                    if derived.contains(input) {
                        *input = output;
                    }
                }
                out.push(op);
            }
        }
    }
    out
}

const SUPPORTED: &[&str] = &[
    "MOV",
    "MOVSX",
    "MOVZX",
    "XCHG",
    "NOT",
    "BSWAP",
    "BSR",
    "LEAVE",
    "LEA",
    "PUSH",
    "POP",
    "RET",
    "NOP",
    "ADD",
    "SUB",
    "CMP",
    "INC",
    "DEC",
    "ADC",
    "SBB",
    "SHL",
    "SHR",
    "SAR",
    "DIV",
    "IDIV",
    "NEG",
    "MUL",
    "IMUL",
    "CDQ",
    "CWD",
    "CBW",
    "CWDE",
    "CLD",
    "STD",
    "SAHF",
    "LAHF",
    "TEST",
    "AND",
    "OR",
    "XOR",
    "ROL",
    "ROR",
    "RCL",
    "RCR",
    "SHLD",
    "SHRD",
    "JMP",
    "JZ",
    "JNZ",
    "JE",
    "JNE",
    "JA",
    "JAE",
    "JB",
    "JBE",
    "JC",
    "JNC",
    "JG",
    "JGE",
    "JL",
    "JLE",
    "JS",
    "JNS",
    "JO",
    "JNO",
    "JP",
    "JNP",
    "JPE",
    "JPO",
    "CALL",
    "SETA",
    "SETAE",
    "SETB",
    "SETBE",
    "SETC",
    "SETE",
    "SETG",
    "SETGE",
    "SETL",
    "SETLE",
    "SETNA",
    "SETNAE",
    "SETNB",
    "SETNBE",
    "SETNC",
    "SETNE",
    "SETNG",
    "SETNGE",
    "SETNL",
    "SETNLE",
    "SETNO",
    "SETNP",
    "SETNS",
    "SETNZ",
    "SETO",
    "SETP",
    "SETPE",
    "SETPO",
    "SETS",
    "SETZ",
    "XLAT",
    "BT",
    "BTS",
    "BTR",
    "BTC",
    "CLC",
    "STC",
    "CMC",
    "SALC",
    "AAA",
    "AAS",
    "AAM",
    "AAD",
    "DAA",
    "DAS",
    "ARPL",
    "PUSHAD",
    "POPAD",
    "LOOP",
    "LOOPE",
    "LOOPNE",
    "JECXZ",
    "JCXZ",
    "MOVSS",
    "MOVAPS",
    "ADDSS",
    "SUBSS",
    "MULSS",
    "DIVSS",
    "MINSS",
    "MAXSS",
    "ADDPS",
    "SUBPS",
    "MULPS",
    "DIVPS",
    "MINPS",
    "MAXPS",
    "SQRTPS",
    "RCPPS",
    "RSQRTPS",
    "RCPSS",
    "RSQRTSS",
    "CMPEQPS",
    "CMPLTPS",
    "CMPLEPS",
    "CMPUNORDPS",
    "CMPNEQPS",
    "CMPNLTPS",
    "CMPNLEPS",
    "CMPORDPS",
    "SHUFPS",
    "UNPCKLPS",
    "UNPCKHPS",
    "MOVLHPS",
    "MOVHLPS",
    "MOVLPS",
    "MOVHPS",
    "MOVMSKPS",
    "MOVUPS",
    "MOVNTPS",
    "MOVDQA",
    "MOVDQU",
    "MOVAPD",
    "PSHUFD",
    "PUNPCKLQDQ",
    "PUNPCKHQDQ",
    "PSRLDQ",
    "COMISS",
    "UCOMISS",
    "CVTTSS2SI",
    "CVTSS2SI",
    "CVTSI2SS",
    "XORPS",
    "ANDPS",
    "ORPS",
    "ANDNPS",
    "ORPD",
    "XORPD",
    "ANDPD",
    "ANDNPD",
    "PAND",
    "PANDN",
    "POR",
    "PXOR",
    "CMOVA",
    "CMOVAE",
    "CMOVB",
    "CMOVBE",
    "CMOVC",
    "CMOVE",
    "CMOVG",
    "CMOVGE",
    "CMOVL",
    "CMOVLE",
    "CMOVNA",
    "CMOVNAE",
    "CMOVNB",
    "CMOVNBE",
    "CMOVNC",
    "CMOVNE",
    "CMOVNG",
    "CMOVNGE",
    "CMOVNL",
    "CMOVNLE",
    "CMOVNO",
    "CMOVNP",
    "CMOVNS",
    "CMOVNZ",
    "CMOVO",
    "CMOVP",
    "CMOVPE",
    "CMOVPO",
    "CMOVS",
    "CMOVZ",
    "XADD",
    "CMPXCHG",
    "CMPXCHG8B",
];

fn is_media(mnemonic: &str) -> bool {
    matches!(
        mnemonic,
        "MOVD"
            | "MOVQ"
            | "MOVNTQ"
            | "EMMS"
            | "FEMMS"
            | "PSHUFW"
            | "PADDB"
            | "PADDW"
            | "PADDD"
            | "PADDQ"
            | "PSUBB"
            | "PSUBW"
            | "PSUBD"
            | "PSUBQ"
            | "PADDSB"
            | "PADDSW"
            | "PSUBSB"
            | "PSUBSW"
            | "PADDUSB"
            | "PADDUSW"
            | "PSUBUSB"
            | "PSUBUSW"
            | "PMULLW"
            | "PMULHW"
            | "PMADDWD"
            | "PCMPEQB"
            | "PCMPEQW"
            | "PCMPEQD"
            | "PCMPGTB"
            | "PCMPGTW"
            | "PCMPGTD"
            | "PACKSSWB"
            | "PACKSSDW"
            | "PACKUSWB"
            | "PMINUB"
            | "PMAXUB"
            | "PUNPCKLBW"
            | "PUNPCKLWD"
            | "PUNPCKLDQ"
            | "PUNPCKHBW"
            | "PUNPCKHWD"
            | "PUNPCKHDQ"
            | "PSRLW"
            | "PSRLD"
            | "PSRLQ"
            | "PSRAW"
            | "PSRAD"
            | "PSLLW"
            | "PSLLD"
            | "PSLLQ"
            | "CVTPI2PS"
            | "CVTPS2PI"
            | "CVTTPS2DQ"
            | "CVTSD2SI"
            | "CVTTSD2SI"
            | "PEXTRW"
            | "MOVDQ2Q"
    )
}

fn call_other_family(mnemonic: &str, userop: Option<&str>) -> SlowFamily {
    if is_media(mnemonic) {
        return SlowFamily::Mmx;
    }
    match userop {
        Some("in") => SlowFamily::In,
        Some("out") => SlowFamily::Out,
        Some("swi") => SlowFamily::Int,
        Some("cpuid") => SlowFamily::Cpuid,
        Some("LocalDescriptorTableRegister") => SlowFamily::Segment,
        Some("rdtsc") => SlowFamily::Rdtsc,
        _ => SlowFamily::CallOther,
    }
}

fn unmodelled_family(mnemonic: &str) -> SlowFamily {
    if is_media(mnemonic) {
        return SlowFamily::Mmx;
    }
    match mnemonic {
        "LDS" | "LES" | "SLDT" => SlowFamily::Segment,
        "HLT" => SlowFamily::Hlt,
        "CLI" => SlowFamily::Cli,
        "STI" => SlowFamily::Sti,
        "INT" | "INT1" | "INTO" => SlowFamily::Int,
        "IRET" => SlowFamily::Iret,
        "RETF" => SlowFamily::Retf,
        "BOUND" => SlowFamily::Bound,
        "RDTSC" => SlowFamily::Rdtsc,
        "CPUID" => SlowFamily::Cpuid,
        "IN" | "INSB" | "INSD" => SlowFamily::In,
        "OUT" | "OUTSB" | "OUTSD" => SlowFamily::Out,
        _ => SlowFamily::Unmodelled,
    }
}

fn sse_immediate(insn: &Insn) -> Option<u64> {
    let text = insn.operands.rsplit(',').next()?.trim();
    match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

fn lower_shuffle(
    mnem: &str,
    ops: &[PcodeOp],
    insn: &Insn,
    _fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Option<Vec<PcodeOp>> {
    let imm = sse_immediate(insn)? as u32;
    let cut = ops.iter().position(|op| op.opcode == OpCode::IntEqual)?;
    if cut + 4 > ops.len() {
        return None;
    }
    let snapshots: Vec<Varnode> = ops[..cut]
        .iter()
        .filter(|op| {
            op.opcode == OpCode::Copy
                && op
                    .output
                    .is_some_and(|v| v.size == 4 && v.space == Space::Unique)
                && op.inputs.len() == 1
                && op.inputs[0].size == 4
        })
        .filter_map(|op| op.output)
        .collect();
    let dest: Vec<Varnode> = ops[ops.len() - 4..]
        .iter()
        .filter_map(|op| {
            op.output
                .filter(|v| v.size == 4 && v.space == Space::Register)
        })
        .collect();
    if dest.len() != 4 || snapshots.len() < if mnem == "SHUFPS" { 8 } else { 4 } {
        return None;
    }
    let mut out = ops[..cut].to_vec();
    for (lane, &target) in dest.iter().enumerate() {
        let select = ((imm >> (2 * lane)) & 3) as usize;
        let source = if mnem == "SHUFPS" {
            if lane < 2 {
                snapshots[4 + select]
            } else {
                snapshots[select]
            }
        } else {
            snapshots[select]
        };
        out.push(copy(target, source));
    }
    Some(out)
}
