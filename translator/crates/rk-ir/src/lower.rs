use std::collections::{BTreeMap, BTreeSet};

use rk_core::ir::{FunctionIr, Insn, OpCode, PcodeOp, SehEffect, Space, Varnode};
use rk_core::program::Image;
use rk_core::{Addr, Diagnostic, Reason};

use crate::op::{Op, SlowFamily, SlowInsn, StringOp, X87Recipes};
use crate::regs::Registers;
use crate::string;

#[derive(Clone, Debug)]
pub struct CgOp {
    pub op: Op,
    pub output: Option<Varnode>,
    pub inputs: Vec<Varnode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagShape {
    Add,
    Sub,
    Cmp,
    Inc,
    Dec,
    Adc,
    Sbb,
    Logic,
}

#[derive(Clone, Debug)]
pub struct CgInsn {
    pub insn: u32,
    pub addr: Addr,
    pub length: u8,
    pub ops: Vec<CgOp>,
    pub x87: bool,
    pub flag: Option<FlagShape>,
    pub flag_operands: Option<(Varnode, Varnode)>,
}

#[derive(Clone, Debug)]
pub struct CgFunction {
    pub addr: Addr,
    pub insns: Vec<CgInsn>,
    pub succ: Vec<Vec<u32>>,
    pub exits: BTreeMap<u32, Vec<Addr>>,
    pub entries: Vec<Addr>,
    pub tables: BTreeSet<u32>,
    pub dynamic: BTreeSet<u32>,
    pub noreturn: BTreeMap<u32, Addr>,
    pub import_tails: BTreeMap<u32, Addr>,
    pub seh: BTreeMap<u32, Vec<SehEffect>>,
    pub slow: Vec<SlowInsn>,
    pub diagnostics: Vec<Diagnostic>,
}

impl CgFunction {
    pub fn index_of(&self, addr: Addr) -> Option<u32> {
        self.insns
            .binary_search_by_key(&addr, |insn| insn.addr)
            .ok()
            .map(|n| n as u32)
    }
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
            | "ORPD"
            | "XORPD"
            | "ANDPD"
            | "ANDNPD"
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
            | "PAND"
            | "PANDN"
            | "POR"
            | "PXOR"
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
            | "CVTTSD2SI"
            | "PEXTRW"
            | "MOVDQ2Q"
    )
}

fn call_other_family(mnemonic: &str, userop: Option<u64>) -> SlowFamily {
    if is_media(mnemonic) {
        return SlowFamily::Mmx;
    }
    match userop {
        Some(1) => SlowFamily::In,
        Some(2) => SlowFamily::Out,
        Some(16) => SlowFamily::Int,
        Some(44) => SlowFamily::Cpuid,
        Some(63) => SlowFamily::Segment,
        Some(72) => SlowFamily::Rdtsc,
        _ => SlowFamily::CallOther,
    }
}

fn unmodelled_family(mnemonic: &str) -> SlowFamily {
    if is_media(mnemonic) {
        return SlowFamily::Mmx;
    }
    match mnemonic {
        "PUSHFD" | "POPFD" => SlowFamily::Flags,
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

struct Lowerer<'a> {
    regs: &'static Registers,
    image: &'a Image,
    x87: &'a dyn X87Recipes,
    unique: u64,
    slow: Vec<SlowInsn>,
    diagnostics: Vec<Diagnostic>,
}

pub fn lower(fir: &FunctionIr, image: &Image, x87: &dyn X87Recipes) -> rk_core::Result<CgFunction> {
    let mut lowerer = Lowerer {
        regs: Registers::get(),
        image,
        x87,
        unique: 0x8000_0000,
        slow: Vec::new(),
        diagnostics: Vec::new(),
    };
    let mut insns = Vec::with_capacity(fir.insns.len());
    for (index, insn) in fir.insns.iter().enumerate() {
        let (mut ops, is_x87, flag_operands) = lowerer.lower_insn(fir, index as u32, insn)?;
        if let Some(&target) = fir.intra_calls.get(&(index as u32)) {
            for op in &mut ops {
                if matches!(op.op, Op::Call { .. }) {
                    op.op = Op::Branch { target };
                }
            }
        }
        lowerer.add_tail(fir, index as u32, &mut ops);
        let flag = if is_x87 {
            None
        } else {
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
        };
        insns.push(CgInsn {
            insn: index as u32,
            addr: insn.addr,
            length: insn.length,
            ops,
            x87: is_x87,
            flag,
            flag_operands,
        });
    }
    Ok(CgFunction {
        addr: fir.addr,
        insns,
        succ: fir.succ.clone(),
        exits: fir.exits.clone(),
        entries: fir.entries.clone(),
        tables: fir.tables.clone(),
        dynamic: fir.dynamic.clone(),
        noreturn: fir.noreturn.clone(),
        import_tails: fir.import_tails.clone(),
        seh: fir.seh.clone(),
        slow: lowerer.slow,
        diagnostics: lowerer.diagnostics,
    })
}

impl Lowerer<'_> {
    fn fresh(&mut self, size: u32) -> Varnode {
        let out = Varnode::new(Space::Unique, self.unique, size);
        self.unique += 0x10;
        out
    }

    fn slow(&mut self, insn: &Insn, family: SlowFamily) -> Op {
        let index = self.slow.len() as u32;
        self.slow.push(SlowInsn {
            addr: insn.addr,
            length: insn.length,
            family,
        });
        Op::Slow(index)
    }

    fn reject(&mut self, insn: &Insn, reason: Reason, family: SlowFamily) -> Op {
        self.diagnostics.push(Diagnostic::new(insn.addr, reason));
        self.slow(insn, family)
    }

    fn lower_insn(
        &mut self,
        fir: &FunctionIr,
        index: u32,
        insn: &Insn,
    ) -> rk_core::Result<(Vec<CgOp>, bool, Option<(Varnode, Varnode)>)> {
        let mnem = insn.mnemonic.to_ascii_uppercase();
        if mnem == "WAIT" || mnem == "NOP" || mnem == "PAUSE" {
            return Ok((Vec::new(), false, None));
        }
        if mnem == "INT3" {
            return Ok((vec![cg(Op::Breakpoint)], false, None));
        }
        if mnem == "MOV"
            && insn
                .operands
                .split(',')
                .next()
                .is_some_and(|op| op.trim().eq_ignore_ascii_case("CS"))
        {
            let op = self.slow(insn, SlowFamily::Segment);
            return Ok((vec![cg(op)], false, None));
        }
        if matches!(mnem.as_str(), "FXSAVE" | "STMXCSR" | "LDMXCSR" | "SFENCE") {
            let op = self.slow(insn, SlowFamily::Sse);
            return Ok((vec![cg(op)], false, None));
        }
        if insn.x87 || mnem.starts_with('F') {
            let mut unique = self.unique;
            let lowering = {
                let mut fresh = |size| {
                    let out = Varnode::new(Space::Unique, unique, size);
                    unique += 0x10;
                    out
                };
                self.x87.lower(insn, self.image, &mut fresh)
            };
            self.unique = unique;
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
                return Ok((ops, true, None));
            }
            let op = self.slow(insn, SlowFamily::X87);
            return Ok((vec![cg(op)], true, None));
        }
        let raw = self
            .image
            .bytes(insn.addr, insn.length as usize)
            .unwrap_or(&[]);
        if let Some(helper) = string::helper(raw) {
            return Ok((vec![cg(Op::String(StringOp(helper)))], false, None));
        }
        if let Some(slot) = fir.import_tails.get(&index) {
            return Ok((vec![cg(Op::ImportTail { slot: *slot })], false, None));
        }
        let mut stripped = Vec::new();
        let raw_ops: &[PcodeOp] = if insn
            .ops
            .iter()
            .any(|op| op.opcode == OpCode::CallOther && matches!(op.userop, Some(17 | 18)))
        {
            stripped.extend(
                insn.ops
                    .iter()
                    .filter(|op| {
                        !(op.opcode == OpCode::CallOther && matches!(op.userop, Some(17 | 18)))
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
                    | "RSQRTSS"
                    | "MOVMSKPS"
            );
            if !handled {
                let family = call_other_family(&mnem, call.userop);
                let op = self.slow(insn, family);
                return Ok((vec![cg(op)], false, None));
            }
        }
        let divide_by_zero = matches!(mnem.as_str(), "AAM")
            && self
                .image
                .bytes(insn.addr, insn.length as usize)
                .and_then(|b| b.last())
                == Some(&0);
        if divide_by_zero || !SUPPORTED.contains(&mnem.as_str()) {
            let family = unmodelled_family(&mnem);
            let op = self.reject(
                insn,
                Reason::Unmodelled {
                    mnemonic: mnem.clone(),
                },
                family,
            );
            return Ok((vec![cg(op)], false, None));
        }
        let ops = match normalize_direct_ram(raw_ops, &mut |size| self.fresh(size)) {
            Ok(ops) => ops,
            Err(reason) => {
                let op = self.reject(insn, reason, SlowFamily::Unmodelled);
                return Ok((vec![cg(op)], false, None));
            }
        };
        let ops = match correct_sse(&mnem, &ops, self.regs, insn, &mut |size| self.fresh(size)) {
            Ok(ops) => ops,
            Err(reason) => {
                let op = self.reject(insn, reason, SlowFamily::Unmodelled);
                return Ok((vec![cg(op)], false, None));
            }
        };
        let ops = match correct_extra(&mnem, &ops, self.regs, insn, &mut |size| self.fresh(size)) {
            Ok(ops) => ops,
            Err(reason) => {
                let op = self.reject(insn, reason, SlowFamily::Unmodelled);
                return Ok((vec![cg(op)], false, None));
            }
        };
        if mnem == "DIV" || mnem == "IDIV" {
            let (mut converted, has_address) =
                match correct_div_cg(&mnem, &ops, self.regs, insn.addr, &mut |size| {
                    self.fresh(size)
                }) {
                    Ok(ops) => (ops, true),
                    Err(reason) => {
                        let op = self.reject(insn, reason, SlowFamily::Unmodelled);
                        (vec![cg(op)], false)
                    }
                };
            if has_address {
                if let Some(kinds) = fir.seh.get(&index) {
                    apply_seh(&mut converted, kinds);
                }
                if let Some(target) = fir.noreturn.get(&index) {
                    converted.push(cg(Op::Trap(*target)));
                }
                return Ok((converted, false, None));
            }
            return Ok((converted, false, None));
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
            prepare_rmw(&ops, &mut |size| self.fresh(size))
        } else {
            Ok(ops.clone())
        };
        let mut flag_operands = None;
        let corrected = prepared.and_then(|ops| {
            let corrected = match mnem.as_str() {
                "ADD" | "SUB" | "CMP" | "INC" | "DEC" | "ADC" | "SBB" => correct_arithmetic(
                    &mnem,
                    &ops,
                    self.regs,
                    &mut |size| self.fresh(size),
                    &mut flag_operands,
                ),
                "SHL" | "SHR" => {
                    correct_shift(&mnem, &ops, self.regs, &mut |size| self.fresh(size))
                }
                "SAR" => correct_sar(&ops, self.regs, &mut |size| self.fresh(size)),
                "BSR" => correct_bsr(&ops, self.regs, &mut |size| self.fresh(size)),
                "NEG" => correct_neg(&ops, self.regs, &mut |size| self.fresh(size)),
                "AND" | "OR" | "XOR" | "TEST" => Ok(with_af_zero(&ops, self.regs)),
                _ => Ok(ops.clone()),
            }?;
            Ok(if rewrites_memory {
                defer_flags(corrected, self.regs, &mut |size| self.fresh(size))
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
                let op = self.reject(insn, reason, family);
                return Ok((vec![cg(op)], false, None));
            }
        };
        let mut converted: Vec<CgOp> = ops.into_iter().map(from_pcode).collect();
        if let Some(kinds) = fir.seh.get(&index) {
            apply_seh(&mut converted, kinds);
        }
        if let Some(target) = fir.noreturn.get(&index) {
            converted.push(cg(Op::Trap(*target)));
        }
        Ok((converted, false, flag_operands))
    }

    fn add_tail(&self, fir: &FunctionIr, index: u32, out: &mut Vec<CgOp>) {
        if out.iter().any(|op| matches!(op.op, Op::CBranch { .. })) {
            let insn = &fir.insns[index as usize];
            let target = insn.addr + Addr::from(insn.length);
            if fir
                .exits
                .get(&index)
                .is_some_and(|targets| targets.contains(&target))
            {
                out.push(cg(Op::Tail { target }));
            }
            return;
        }
        if fir.succ.get(index as usize).is_some_and(|s| !s.is_empty()) {
            return;
        }
        if let Some(targets) = fir.exits.get(&index) {
            for target in targets {
                out.push(cg(Op::Tail { target: *target }));
            }
        }
    }
}

fn cg(op: Op) -> CgOp {
    CgOp {
        op,
        output: None,
        inputs: Vec::new(),
    }
}

fn from_pcode(op: PcodeOp) -> CgOp {
    match op.opcode {
        OpCode::Branch => {
            let target = op.inputs.first().and_then(target_addr);
            CgOp {
                op: target.map_or(Op::P(OpCode::Branch), |target| Op::Branch { target }),
                output: op.output,
                inputs: Vec::new(),
            }
        }
        OpCode::CBranch => {
            let target = op.inputs.first().and_then(target_addr);
            let inputs = if target.is_some() {
                op.inputs.get(1..).unwrap_or(&[]).to_vec()
            } else {
                op.inputs
            };
            CgOp {
                op: target.map_or(Op::P(OpCode::CBranch), |target| Op::CBranch { target }),
                output: op.output,
                inputs,
            }
        }
        OpCode::Call => {
            let target = op.inputs.first().and_then(target_addr);
            CgOp {
                op: target.map_or(Op::P(OpCode::Call), |target| Op::Call { target }),
                output: op.output,
                inputs: Vec::new(),
            }
        }
        OpCode::CallInd => CgOp {
            op: Op::CallInd,
            output: op.output,
            inputs: op.inputs,
        },
        OpCode::Return => CgOp {
            op: Op::Return,
            output: op.output,
            inputs: Vec::new(),
        },
        OpCode::BranchInd => CgOp {
            op: Op::BranchInd,
            output: op.output,
            inputs: op.inputs,
        },
        OpCode::CallOther => CgOp {
            op: Op::CallOther(op.userop.unwrap_or(0)),
            output: op.output,
            inputs: op.inputs,
        },
        other => CgOp {
            op: Op::P(other),
            output: op.output,
            inputs: op.inputs,
        },
    }
}

fn target_addr(v: &Varnode) -> Option<Addr> {
    match v.space {
        Space::Const | Space::Ram => Some(v.offset as Addr),
        _ => None,
    }
}

fn is_control(op: &PcodeOp) -> bool {
    matches!(
        op.opcode,
        OpCode::Branch
            | OpCode::CBranch
            | OpCode::Call
            | OpCode::CallInd
            | OpCode::BranchInd
            | OpCode::CallOther
    )
}

fn correct_sse(
    mnem: &str,
    ops: &[PcodeOp],
    regs: &Registers,
    insn: &Insn,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: format!("unsupported {mnem} p-code shape"),
    };
    if matches!(mnem, "MINSS" | "MAXSS") {
        let at = ops.len().checked_sub(3).ok_or_else(unsupported)?;
        let (compare, branch, copy) = (&ops[at], &ops[at + 1], &ops[at + 2]);
        let dest = copy.output.ok_or_else(unsupported)?;
        let source = *copy.inputs.first().ok_or_else(unsupported)?;
        let expected = if mnem == "MINSS" {
            (dest, source)
        } else {
            (source, dest)
        };
        if copy.opcode != OpCode::Copy
            || dest.space != Space::Register
            || dest.size != 4
            || source.size != 4
            || compare.opcode != OpCode::FloatLess
            || compare.output.is_none_or(|v| v.size != 1)
            || compare.inputs != [expected.0, expected.1]
            || branch.opcode != OpCode::CBranch
            || branch.output.is_some()
            || branch.inputs.len() != 2
            || branch.inputs[0].space != Space::Ram
            || branch.inputs[0].offset != (insn.addr + insn.length as Addr) as u64
            || Some(branch.inputs[1]) != compare.output
            || ops[..at].iter().any(|op| {
                matches!(
                    op.opcode,
                    OpCode::Branch | OpCode::CBranch | OpCode::BranchInd | OpCode::Store
                )
            })
        {
            return Err(unsupported());
        }
        let mut out = ops[..at].to_vec();
        let code = if mnem == "MINSS" {
            OpCode::FloatMin
        } else {
            OpCode::FloatMax
        };
        out.push(op(code, dest, vec![dest, source]));
        return Ok(out);
    }
    if matches!(mnem, "COMISS" | "UCOMISS") {
        if ops.iter().any(|op| op.opcode == OpCode::Store) {
            return Err(unsupported());
        }
        let mut out = Vec::with_capacity(ops.len());
        let mut load = None;
        for op in ops {
            if op.opcode == OpCode::Load {
                let key = (op.inputs.clone(), op.output);
                if load.as_ref() == Some(&key) {
                    continue;
                }
                load = Some(key);
            }
            out.push(op.clone());
        }
        return Ok(out);
    }
    if mnem == "CVTTSS2SI" {
        return Ok(ops
            .iter()
            .map(|item| {
                if item.opcode == OpCode::FloatTrunc
                    && item.output.is_some_and(|v| v.size == 4)
                    && item.inputs.len() == 1
                    && item.inputs[0].size == 4
                {
                    op(OpCode::Cvttss2si, item.output.unwrap(), item.inputs.clone())
                } else {
                    item.clone()
                }
            })
            .collect());
    }
    if mnem == "CVTSS2SI" {
        let at = ops.len().checked_sub(2).ok_or_else(unsupported)?;
        let (round, trunc) = (&ops[at], &ops[at + 1]);
        let rounded = round.output.ok_or_else(unsupported)?;
        let dest = trunc.output.ok_or_else(unsupported)?;
        if round.opcode != OpCode::FloatRound
            || rounded.space != Space::Unique
            || rounded.size != 4
            || round.inputs.len() != 1
            || round.inputs[0].size != 4
            || trunc.opcode != OpCode::FloatTrunc
            || dest.space != Space::Register
            || dest.size != 4
            || trunc.inputs != [rounded]
        {
            return Err(unsupported());
        }
        let mut out = ops[..at].to_vec();
        out.push(op(OpCode::Cvtss2si, dest, round.inputs.clone()));
        return Ok(out);
    }
    if mnem == "CVTSI2SS" {
        return Ok(ops
            .iter()
            .map(|item| {
                if item.opcode == OpCode::FloatInt2Float
                    && item.output.is_some_and(|v| v.size == 4)
                    && item.inputs.len() == 1
                    && item.inputs[0].size == 4
                {
                    op(OpCode::Cvtsi2ss, item.output.unwrap(), item.inputs.clone())
                } else {
                    item.clone()
                }
            })
            .collect());
    }
    if mnem == "RSQRTSS" {
        let mut out = Vec::with_capacity(ops.len());
        for item in ops {
            if item.opcode == OpCode::CallOther && item.userop == Some(0xa6) {
                let output = item.output.ok_or_else(unsupported)?;
                let source = *item.inputs.last().ok_or_else(unsupported)?;
                if output.size == 16 && source.size == 16 {
                    let output_lanes = wide_output(output, regs, fresh)?;
                    let source_lanes = wide_lanes(source, &BTreeMap::new(), regs, fresh)?;
                    out.push(op(
                        OpCode::FloatRsqrt,
                        output_lanes[0],
                        vec![source_lanes[0]],
                    ));
                } else if output.size == 4 && source.size == 4 {
                    out.push(op(OpCode::FloatRsqrt, output, vec![source]));
                } else {
                    return Err(unsupported());
                }
            } else {
                out.push(item.clone());
            }
        }
        return Ok(out);
    }
    if mnem == "PSRLDQ" {
        return split_psrldq(ops, insn, regs, fresh);
    }
    if matches!(mnem, "SHUFPS" | "PSHUFD") {
        return match lower_shuffle(mnem, ops, insn, fresh) {
            Some(out) => Ok(out),
            None => split_wide(ops, mnem, regs, fresh),
        };
    }
    if mnem.starts_with("CMP") && mnem.ends_with("PS") {
        let rewritten = rewrite_temps(ops, regs, fresh);
        return split_wide(&rewritten, mnem, regs, fresh);
    }
    if matches!(
        mnem,
        "MOVAPS"
            | "XORPS"
            | "ANDPS"
            | "ORPS"
            | "ANDNPS"
            | "ADDPS"
            | "SUBPS"
            | "MULPS"
            | "DIVPS"
            | "MINPS"
            | "MAXPS"
            | "SQRTPS"
            | "RCPPS"
            | "RSQRTPS"
            | "CMPEQPS"
            | "CMPLTPS"
            | "CMPLEPS"
            | "CMPUNORDPS"
            | "CMPNEQPS"
            | "CMPNLTPS"
            | "CMPNLEPS"
            | "CMPORDPS"
            | "UNPCKLPS"
            | "UNPCKHPS"
            | "MOVLHPS"
            | "MOVHLPS"
            | "MOVLPS"
            | "MOVHPS"
            | "MOVMSKPS"
            | "MOVUPS"
            | "MOVNTPS"
            | "MOVDQA"
            | "MOVDQU"
            | "MOVAPD"
            | "PUNPCKLQDQ"
            | "PUNPCKHQDQ"
    ) {
        return split_wide(ops, mnem, regs, fresh);
    }
    Ok(ops.to_vec())
}

fn rewrite_temps(
    ops: &[PcodeOp],
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Vec<PcodeOp> {
    let is_temp = |value: Varnode| {
        value.space == Space::Register
            && regs
                .covering(value.offset)
                .is_some_and(|(name, _)| name.starts_with("xmmTmp"))
    };
    let mut values: BTreeMap<Varnode, Varnode> = BTreeMap::new();
    let mut out = Vec::with_capacity(ops.len());
    for item in ops {
        let mut item = item.clone();
        for input in &mut item.inputs {
            if is_temp(*input) {
                if let Some(value) = values.get(input) {
                    *input = *value;
                }
            }
        }
        if let Some(output) = item.output.filter(|value| is_temp(*value)) {
            let value = fresh(output.size);
            values.insert(output, value);
            item.output = Some(value);
        }
        out.push(item);
    }
    out
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

fn split_psrldq(
    ops: &[PcodeOp],
    insn: &Insn,
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: "unsupported PSRLDQ p-code shape".into(),
    };
    let shift = sse_immediate(insn).ok_or_else(unsupported)? as usize & 0xff;
    let item = ops
        .iter()
        .find(|op| op.opcode == OpCode::IntRight && op.output.is_some_and(|v| v.size == 16))
        .ok_or_else(unsupported)?;
    let source = wide_lanes(
        *item.inputs.first().ok_or_else(unsupported)?,
        &BTreeMap::new(),
        regs,
        fresh,
    )?;
    let output = wide_output(item.output.ok_or_else(unsupported)?, regs, fresh)?;
    let mut out = Vec::new();
    let mut snapshot = [Varnode::constant(0, 4); 4];
    for lane in 0..4 {
        snapshot[lane] = fresh(4);
        out.push(copy(snapshot[lane], source[lane]));
    }
    for lane in 0..4 {
        let byte = lane * 4 + shift;
        if byte >= 16 {
            out.push(copy(output[lane], Varnode::constant(0, 4)));
            continue;
        }
        let index = byte / 4;
        let bits = (byte % 4) as u64 * 8;
        let value = if bits == 0 {
            snapshot[index]
        } else {
            let down = fresh(4);
            let up = fresh(4);
            let merged = fresh(4);
            out.push(op(
                OpCode::IntRight,
                down,
                vec![snapshot[index], Varnode::constant(bits, 4)],
            ));
            let high = if index < 3 {
                snapshot[index + 1]
            } else {
                Varnode::constant(0, 4)
            };
            out.push(op(
                OpCode::IntLeft,
                up,
                vec![high, Varnode::constant(32 - bits, 4)],
            ));
            out.push(op(OpCode::IntOr, merged, vec![down, up]));
            merged
        };
        out.push(copy(output[lane], value));
    }
    Ok(out)
}

fn correct_extra(
    mnem: &str,
    ops: &[PcodeOp],
    regs: &Registers,
    insn: &Insn,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: format!("unsupported {mnem} p-code shape"),
    };
    if mnem.starts_with("CMOV") {
        return correct_cmov(ops, insn, fresh).map_err(|_| unsupported());
    }
    match mnem {
        "XADD" => correct_xadd(ops, regs, fresh),
        "CMPXCHG" => correct_cmpxchg(ops, regs, insn, fresh),
        "CMPXCHG8B" => correct_cmpxchg8b(ops, regs, insn, fresh),
        _ => Ok(ops.to_vec()),
    }
}

fn select_value(
    condition: Varnode,
    when_true: Varnode,
    when_false: Varnode,
    output: Varnode,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Vec<PcodeOp> {
    let size = when_false.size;
    let extended = fresh(size);
    let mask = fresh(size);
    let changed = fresh(size);
    let selected = fresh(size);
    vec![
        op(OpCode::IntZExt, extended, vec![condition]),
        op(OpCode::Int2Comp, mask, vec![extended]),
        op(OpCode::IntXor, changed, vec![when_true, when_false]),
        op(OpCode::IntAnd, selected, vec![changed, mask]),
        op(OpCode::IntXor, output, vec![when_false, selected]),
    ]
}

fn add_af(
    a: Varnode,
    b: Varnode,
    result: Varnode,
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Vec<PcodeOp> {
    let x = fresh(result.size);
    let y = fresh(result.size);
    let z = fresh(result.size);
    vec![
        op(OpCode::IntXor, x, vec![a, b]),
        op(OpCode::IntXor, y, vec![x, result]),
        op(
            OpCode::IntRight,
            z,
            vec![y, Varnode::constant(4, result.size)],
        ),
        op(
            OpCode::IntAnd,
            flag(regs, "AF"),
            vec![z, Varnode::constant(1, result.size)],
        ),
    ]
}

fn internal_target(target: Varnode, end: Addr) -> bool {
    target.space == Space::Const || (target.space == Space::Ram && target.offset == end as u64)
}

fn correct_cmov(
    ops: &[PcodeOp],
    insn: &Insn,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: "unsupported CMOV p-code shape".into(),
    };
    let at = ops.len().checked_sub(4).ok_or_else(unsupported)?;
    let (source_copy, negate, branch, dest_copy) =
        (&ops[at], &ops[at + 1], &ops[at + 2], &ops[at + 3]);
    let dest = dest_copy.output.ok_or_else(unsupported)?;
    let source = *source_copy.inputs.first().ok_or_else(unsupported)?;
    let condition = *negate.inputs.first().ok_or_else(unsupported)?;
    if source_copy.opcode != OpCode::Copy
        || source_copy.output.is_none()
        || source.size != dest.size
        || negate.opcode != OpCode::BoolNegate
        || negate.output.is_none_or(|v| v.size != 1)
        || branch.opcode != OpCode::CBranch
        || branch.output.is_some()
        || branch.inputs.len() != 2
        || branch.inputs[0].space != Space::Ram
        || branch.inputs[0].offset != (insn.addr + insn.length as Addr) as u64
        || branch.inputs[1] != negate.output.unwrap()
        || dest_copy.opcode != OpCode::Copy
        || dest.space != Space::Register
        || !matches!(dest.size, 2 | 4)
        || dest_copy.inputs != [source_copy.output.unwrap()]
        || ops[..at].iter().any(|op| {
            matches!(
                op.opcode,
                OpCode::Branch | OpCode::CBranch | OpCode::BranchInd | OpCode::Store
            )
        })
    {
        return Err(unsupported());
    }
    let mut out = ops[..at].to_vec();
    out.extend(select_value(condition, source, dest, dest, fresh));
    Ok(out)
}

fn correct_xadd(
    ops: &[PcodeOp],
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: "unsupported XADD p-code shape".into(),
    };
    let primary = ops
        .iter()
        .rposition(|op| {
            op.opcode == OpCode::IntAdd
                && op.output.is_some_and(|v| matches!(v.size, 1 | 2 | 4))
                && op.inputs.len() == 2
        })
        .ok_or_else(unsupported)?;
    let result = ops[primary].output.unwrap();
    let old = ops[primary].inputs[0];
    let source = ops[primary].inputs[1];
    let stores: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| op.opcode == OpCode::Store)
        .map(|(index, _)| index)
        .collect();
    if old.size != result.size || source.size != result.size || stores.len() > 1 {
        return Err(unsupported());
    }
    let mut address = None;
    if let Some(&store_index) = stores.first() {
        let store = &ops[store_index];
        let load_index = ops
            .iter()
            .position(|op| op.opcode == OpCode::Load && op.output == Some(old))
            .ok_or_else(unsupported)?;
        if store.inputs.len() != 2
            || store.inputs[1].size != result.size
            || ops[load_index].inputs != store.inputs[..1]
        {
            return Err(unsupported());
        }
        address = Some((store.inputs[0], load_index));
    }
    let source_write = ops[primary + 1..]
        .iter()
        .any(|op| op.opcode == OpCode::Copy && op.output == Some(source) && op.inputs == [old]);
    let destination_write = if stores.is_empty() {
        ops[primary + 1..]
            .iter()
            .any(|op| op.opcode == OpCode::Copy && op.output == Some(old) && op.inputs == [result])
    } else {
        let store = &ops[stores[0]];
        ops[primary + 1..].iter().any(|op| {
            (op.opcode == OpCode::Copy
                && op.output == Some(store.inputs[1])
                && op.inputs == [result])
                || (op.opcode == OpCode::Store && op.inputs[1] == result)
        })
    };
    if !source_write || !destination_write {
        return Err(unsupported());
    }
    let captured = address.map(|(base, load_index)| {
        let unique = fresh(4);
        (base, load_index, unique)
    });
    let mut out = Vec::with_capacity(ops.len() + 6);
    let mut seen_load = false;
    for (index, item) in ops.iter().enumerate() {
        let mut item = item.clone();
        if let Some((base, load_index, unique)) = captured {
            if index == load_index {
                out.push(copy(unique, base));
            }
            if item.opcode == OpCode::Load && item.output == Some(old) {
                item.inputs[0] = unique;
                if seen_load {
                    continue;
                }
                seen_load = index == load_index;
            } else if item.opcode == OpCode::Store {
                item.inputs[0] = unique;
            }
        }
        out.push(item);
        if index == primary {
            out.extend(add_af(old, source, result, regs, fresh));
        }
    }
    Ok(out)
}

fn correct_cmpxchg(
    ops: &[PcodeOp],
    regs: &Registers,
    insn: &Insn,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: "unsupported CMPXCHG p-code shape".into(),
    };
    let end = insn.addr + insn.length as Addr;
    let if_index = ops
        .iter()
        .position(|op| op.opcode == OpCode::CBranch)
        .ok_or_else(unsupported)?;
    let branch_index = ops[if_index + 1..]
        .iter()
        .position(|op| op.opcode == OpCode::Branch)
        .map(|index| index + if_index + 1)
        .ok_or_else(unsupported)?;
    let cmp_index = ops[..if_index]
        .iter()
        .rposition(|op| {
            op.opcode == OpCode::IntSub
                && op.output.is_some_and(|v| matches!(v.size, 1 | 2 | 4))
                && op.inputs.len() == 2
        })
        .ok_or_else(unsupported)?;
    let result = ops[cmp_index].output.unwrap();
    let acc = ops[cmp_index].inputs[0];
    let old = ops[cmp_index].inputs[1];
    let condition = *ops[if_index].inputs.get(1).ok_or_else(unsupported)?;
    if !internal_target(ops[if_index].inputs[0], end)
        || !internal_target(ops[branch_index].inputs[0], end)
        || acc.space != Space::Register
        || acc.size != old.size
        || result.size != acc.size
        || condition.size != 1
        || ops[..if_index]
            .iter()
            .any(|op| matches!(op.opcode, OpCode::Branch | OpCode::CBranch | OpCode::Store))
    {
        return Err(unsupported());
    }
    let old_copy = ops[..if_index]
        .iter()
        .find(|op| op.opcode == OpCode::Copy && op.output == Some(old) && op.inputs.len() == 1)
        .ok_or_else(unsupported)?;
    let destination = old_copy.inputs[0];
    if !ops[if_index + 1..branch_index]
        .iter()
        .any(|op| op.opcode == OpCode::Copy && op.output == Some(acc) && op.inputs == [old])
    {
        return Err(unsupported());
    }
    let after = &ops[branch_index + 1..];
    let stores: Vec<usize> = after
        .iter()
        .enumerate()
        .filter(|(_, op)| op.opcode == OpCode::Store)
        .map(|(index, _)| index)
        .collect();
    if stores.len() > 1 {
        return Err(unsupported());
    }
    let (source, memory) = if let Some(&store_index) = stores.first() {
        let store = &after[store_index];
        let stored = *store.inputs.get(1).ok_or_else(unsupported)?;
        let success = after[..store_index]
            .iter()
            .find(|op| {
                op.opcode == OpCode::Copy && op.output == Some(stored) && op.inputs.len() == 1
            })
            .ok_or_else(unsupported)?;
        let load_index = ops[..if_index]
            .iter()
            .position(|op| op.opcode == OpCode::Load && op.output == Some(destination))
            .ok_or_else(unsupported)?;
        if store.inputs.len() != 2
            || store.inputs[1].size != result.size
            || ops[load_index].inputs[0] != store.inputs[0]
        {
            return Err(unsupported());
        }
        (success.inputs[0], Some((store.inputs[0], load_index)))
    } else {
        let success = after
            .iter()
            .find(|op| {
                op.opcode == OpCode::Copy && op.output == Some(destination) && op.inputs.len() == 1
            })
            .ok_or_else(unsupported)?;
        if destination.space != Space::Register {
            return Err(unsupported());
        }
        (success.inputs[0], None)
    };
    if source.size != result.size {
        return Err(unsupported());
    }
    let mut out = Vec::with_capacity(ops.len() + 20);
    let captured = memory.map(|(base, load_index)| {
        let unique = fresh(4);
        (base, load_index, unique)
    });
    for (index, item) in ops[..if_index].iter().enumerate() {
        let mut item = item.clone();
        if let Some((base, load_index, unique)) = captured {
            if index == load_index {
                out.push(copy(unique, base));
                item.inputs[0] = unique;
            }
        }
        out.push(item);
    }
    out.extend(add_af(acc, old, result, regs, fresh));
    out.extend(select_value(condition, acc, old, acc, fresh));
    if let Some((_, _, unique)) = captured {
        // x86 writes the old value back on mismatch; Ghidra's p-code skips that store.
        let selected = fresh(result.size);
        out.extend(select_value(condition, source, old, selected, fresh));
        out.push(PcodeOp {
            opcode: OpCode::Store,
            output: None,
            inputs: vec![unique, selected],
            userop: None,
        });
    } else {
        out.extend(select_value(condition, source, old, destination, fresh));
    }
    Ok(out)
}

fn correct_cmpxchg8b(
    ops: &[PcodeOp],
    regs: &Registers,
    insn: &Insn,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: "unsupported CMPXCHG8B p-code shape".into(),
    };
    let end = insn.addr + insn.length as Addr;
    let zf = flag(regs, "ZF");
    let zf_index = ops
        .iter()
        .position(|op| {
            op.opcode == OpCode::IntEqual && op.output == Some(zf) && op.inputs.len() == 2
        })
        .ok_or_else(unsupported)?;
    let if_index = ops[zf_index + 1..]
        .iter()
        .position(|op| op.opcode == OpCode::CBranch)
        .map(|index| index + zf_index + 1)
        .ok_or_else(unsupported)?;
    let branch_index = ops[if_index + 1..]
        .iter()
        .position(|op| op.opcode == OpCode::Branch)
        .map(|index| index + if_index + 1)
        .ok_or_else(unsupported)?;
    let load_index = ops[..zf_index]
        .iter()
        .position(|op| op.opcode == OpCode::Load && op.output.is_some_and(|v| v.size == 8))
        .ok_or_else(unsupported)?;
    let loaded = ops[load_index].output.unwrap();
    let old = ops[zf_index].inputs[1];
    let after = &ops[branch_index + 1..];
    let store = after
        .iter()
        .find(|op| op.opcode == OpCode::Store)
        .ok_or_else(unsupported)?;
    let eax = regs.require("EAX");
    let ecx = regs.require("ECX");
    let edx = regs.require("EDX");
    let ebx = regs.require("EBX");
    if !internal_target(ops[if_index].inputs[0], end)
        || !internal_target(ops[branch_index].inputs[0], end)
        || old.size != 8
        || !ops[..zf_index]
            .iter()
            .any(|op| op.opcode == OpCode::Copy && op.output == Some(old) && op.inputs == [loaded])
        || store.inputs.len() != 2
        || store.inputs[1].size != 8
        || store.inputs[0] != ops[load_index].inputs[0]
        || !after
            .iter()
            .any(|op| op.opcode == OpCode::IntZExt && op.inputs == [ecx])
        || !after
            .iter()
            .any(|op| op.opcode == OpCode::IntZExt && op.inputs == [ebx])
        || ops[..zf_index]
            .iter()
            .any(|op| matches!(op.opcode, OpCode::Branch | OpCode::CBranch | OpCode::Store))
    {
        return Err(unsupported());
    }
    let base = store.inputs[0];
    let address = fresh(4);
    let mut out = Vec::with_capacity(ops.len() + 20);
    for (index, item) in ops[..=zf_index].iter().enumerate() {
        let mut item = item.clone();
        if index == load_index {
            out.push(copy(address, base));
            item.inputs[0] = address;
        }
        out.push(item);
    }
    let high = fresh(8);
    let low = fresh(8);
    let shifted = fresh(8);
    let desired = fresh(8);
    out.extend([
        op(OpCode::IntZExt, high, vec![ecx]),
        op(
            OpCode::IntLeft,
            shifted,
            vec![high, Varnode::constant(32, 4)],
        ),
        op(OpCode::IntZExt, low, vec![ebx]),
        op(OpCode::IntOr, desired, vec![shifted, low]),
    ]);
    let selected = fresh(8);
    // x86 writes the old value back on mismatch; Ghidra's p-code skips that store.
    out.extend(select_value(zf, desired, old, selected, fresh));
    out.push(PcodeOp {
        opcode: OpCode::Store,
        output: None,
        inputs: vec![address, selected],
        userop: None,
    });
    let original_low = fresh(4);
    let original_high = fresh(4);
    out.extend([
        op(
            OpCode::SubPiece,
            original_low,
            vec![old, Varnode::constant(0, 4)],
        ),
        op(
            OpCode::SubPiece,
            original_high,
            vec![old, Varnode::constant(4, 4)],
        ),
    ]);
    out.extend(select_value(zf, eax, original_low, eax, fresh));
    out.extend(select_value(zf, edx, original_high, edx, fresh));
    Ok(out)
}

fn wide_lanes(
    value: Varnode,
    lanes: &BTreeMap<Varnode, [Varnode; 4]>,
    regs: &Registers,
    _fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<[Varnode; 4], Reason> {
    if let Some(lanes) = lanes.get(&value) {
        return Ok(*lanes);
    }
    if value.space == Space::Register {
        if !regs
            .covering(value.offset)
            .is_some_and(|(name, _)| name.starts_with("XMM"))
        {
            return Err(Reason::Unsupported {
                detail: "wide non-XMM register".into(),
            });
        }
        return Ok(std::array::from_fn(|lane| {
            Varnode::new(Space::Register, value.offset + lane as u64 * 4, 4)
        }));
    }
    if value.space == Space::Const {
        return Ok(std::array::from_fn(|lane| {
            Varnode::constant(
                value.offset.checked_shr(lane as u32 * 32).unwrap_or(0) & 0xffff_ffff,
                4,
            )
        }));
    }
    Err(Reason::Unsupported {
        detail: "wide value without dword lanes".into(),
    })
}

fn wide_output(
    value: Varnode,
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<[Varnode; 4], Reason> {
    if value.space == Space::Register {
        wide_lanes(value, &BTreeMap::new(), regs, fresh)
    } else if value.space == Space::Unique {
        Ok(std::array::from_fn(|_| fresh(4)))
    } else {
        Err(Reason::Unsupported {
            detail: "wide output space".into(),
        })
    }
}

fn lane_address(
    address: Varnode,
    lane: usize,
    fresh: &mut dyn FnMut(u32) -> Varnode,
    out: &mut Vec<PcodeOp>,
) -> Result<Varnode, Reason> {
    if address.size != 4 {
        return Err(Reason::Unsupported {
            detail: "wide memory address width".into(),
        });
    }
    if address.space == Space::Const {
        return Ok(Varnode::constant(address.offset + lane as u64 * 4, 4));
    }
    let offset = Varnode::constant(lane as u64 * 4, address.size);
    let result = fresh(address.size);
    out.push(op(OpCode::IntAdd, result, vec![address, offset]));
    Ok(result)
}

fn half_lanes(
    value: Varnode,
    lanes: &BTreeMap<Varnode, [Varnode; 2]>,
    regs: &Registers,
) -> Result<[Varnode; 2], Reason> {
    if let Some(lanes) = lanes.get(&value) {
        return Ok(*lanes);
    }
    if value.space == Space::Register {
        if !regs
            .covering(value.offset)
            .is_some_and(|(name, _)| name.starts_with("XMM"))
        {
            return Err(Reason::Unsupported {
                detail: "wide non-XMM register".into(),
            });
        }
        return Ok(std::array::from_fn(|lane| {
            Varnode::new(Space::Register, value.offset + lane as u64 * 4, 4)
        }));
    }
    if value.space == Space::Const {
        return Ok(std::array::from_fn(|lane| {
            Varnode::constant((value.offset >> (lane * 32)) & 0xffff_ffff, 4)
        }));
    }
    Err(Reason::Unsupported {
        detail: "wide value without dword lanes".into(),
    })
}

fn half_output(
    value: Varnode,
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<[Varnode; 2], Reason> {
    if value.space == Space::Register {
        half_lanes(value, &BTreeMap::new(), regs)
    } else if value.space == Space::Unique {
        Ok(std::array::from_fn(|_| fresh(4)))
    } else {
        Err(Reason::Unsupported {
            detail: "wide output space".into(),
        })
    }
}

fn split_wide(
    ops: &[PcodeOp],
    mnem: &str,
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: "unsupported wide SSE shape".into(),
    };
    let mut out = Vec::with_capacity(ops.len() * 4);
    let mut lanes: BTreeMap<Varnode, [Varnode; 4]> = BTreeMap::new();
    let mut halves: BTreeMap<Varnode, [Varnode; 2]> = BTreeMap::new();
    for item in ops {
        if item.opcode == OpCode::CallOther {
            if mnem == "MOVMSKPS" {
                let output = item.output.ok_or_else(unsupported)?;
                let source = wide_lanes(
                    *item.inputs.last().ok_or_else(unsupported)?,
                    &lanes,
                    regs,
                    fresh,
                )?;
                let mut acc = Varnode::constant(0, 4);
                for lane in 0..4 {
                    let shifted = fresh(4);
                    let bit = fresh(4);
                    let placed = fresh(4);
                    let merged = fresh(4);
                    out.push(op(
                        OpCode::IntRight,
                        shifted,
                        vec![source[lane], Varnode::constant(31, 4)],
                    ));
                    out.push(op(
                        OpCode::IntAnd,
                        bit,
                        vec![shifted, Varnode::constant(1, 4)],
                    ));
                    out.push(op(
                        OpCode::IntLeft,
                        placed,
                        vec![bit, Varnode::constant(lane as u64, 4)],
                    ));
                    out.push(op(OpCode::IntOr, merged, vec![acc, placed]));
                    acc = merged;
                }
                out.push(copy(output, acc));
                continue;
            }
            if matches!(
                mnem,
                "MINPS" | "MAXPS" | "DIVPS" | "SQRTPS" | "RCPPS" | "RSQRTPS" | "RSQRTSS"
            ) {
                let output = item.output.ok_or_else(unsupported)?;
                let source = *item.inputs.last().ok_or_else(unsupported)?;
                if output.size == 16 {
                    let output_lanes = wide_output(output, regs, fresh)?;
                    if matches!(mnem, "MINPS" | "MAXPS" | "DIVPS") {
                        let vectors: Vec<Varnode> = item
                            .inputs
                            .iter()
                            .copied()
                            .filter(|value| value.size == 16)
                            .collect();
                        if vectors.len() < 2 {
                            return Err(unsupported());
                        }
                        let left = wide_lanes(vectors[0], &lanes, regs, fresh)?;
                        let right = wide_lanes(vectors[1], &lanes, regs, fresh)?;
                        let code = match mnem {
                            "MINPS" => OpCode::FloatMin,
                            "MAXPS" => OpCode::FloatMax,
                            _ => OpCode::FloatDiv,
                        };
                        for lane in 0..4 {
                            out.push(op(code, output_lanes[lane], vec![left[lane], right[lane]]));
                        }
                    } else {
                        let source_lanes = wide_lanes(source, &lanes, regs, fresh)?;
                        let code = match mnem {
                            "SQRTPS" => OpCode::FloatSqrt,
                            "RCPPS" => OpCode::FloatRcp,
                            _ => OpCode::FloatRsqrt,
                        };
                        for lane in 0..4 {
                            out.push(op(code, output_lanes[lane], vec![source_lanes[lane]]));
                        }
                    }
                    lanes.insert(output, output_lanes);
                } else if output.size == 4 && source.size == 4 {
                    out.push(op(OpCode::FloatRsqrt, output, vec![source]));
                } else {
                    return Err(unsupported());
                }
                continue;
            }
        }
        if item.opcode == OpCode::SubPiece
            && item.output.is_some_and(|v| v.size == 4)
            && item.inputs.len() == 2
            && item.inputs[0].size == 16
            && item.inputs[1].space == Space::Const
        {
            let offset = item.inputs[1].offset;
            if offset % 4 != 0 || offset >= 16 {
                return Err(unsupported());
            }
            let source = wide_lanes(item.inputs[0], &lanes, regs, fresh)?;
            out.push(copy(item.output.unwrap(), source[offset as usize / 4]));
            continue;
        }
        let half_output_value = item.output.filter(|v| v.size == 8);
        let half_inputs = item
            .inputs
            .iter()
            .filter(|v| v.size == 8)
            .copied()
            .collect::<Vec<_>>();
        if half_output_value.is_some() || !half_inputs.is_empty() {
            match item.opcode {
                OpCode::Load if half_output_value.is_some() && item.inputs.len() == 1 => {
                    let output = half_output(half_output_value.unwrap(), regs, fresh)?;
                    for lane in 0..2 {
                        let address = lane_address(item.inputs[0], lane, fresh, &mut out)?;
                        out.push(PcodeOp {
                            opcode: OpCode::Load,
                            output: Some(output[lane]),
                            inputs: vec![address],
                            userop: None,
                        });
                    }
                    halves.insert(half_output_value.unwrap(), output);
                }
                OpCode::Copy if half_output_value.is_some() && half_inputs.len() == 1 => {
                    let source = half_lanes(half_inputs[0], &halves, regs)?;
                    let output = half_output(half_output_value.unwrap(), regs, fresh)?;
                    for lane in 0..2 {
                        out.push(copy(output[lane], source[lane]));
                    }
                    halves.insert(half_output_value.unwrap(), output);
                }
                OpCode::Store if half_output_value.is_none() && item.inputs.len() == 2 => {
                    let source = half_lanes(item.inputs[1], &halves, regs)?;
                    for lane in 0..2 {
                        let address = lane_address(item.inputs[0], lane, fresh, &mut out)?;
                        out.push(PcodeOp {
                            opcode: OpCode::Store,
                            output: None,
                            inputs: vec![address, source[lane]],
                            userop: None,
                        });
                    }
                }
                _ => return Err(unsupported()),
            }
            continue;
        }
        let wide_output_value = item.output.filter(|v| v.size == 16);
        let wide_inputs = item
            .inputs
            .iter()
            .filter(|v| v.size == 16)
            .copied()
            .collect::<Vec<_>>();
        if wide_output_value.is_none() && wide_inputs.is_empty() {
            out.push(item.clone());
            continue;
        }
        match item.opcode {
            OpCode::Load if wide_output_value.is_some() && item.inputs.len() == 1 => {
                let output = wide_output(wide_output_value.unwrap(), regs, fresh)?;
                for lane in 0..4 {
                    let address = lane_address(item.inputs[0], lane, fresh, &mut out)?;
                    out.push(PcodeOp {
                        opcode: OpCode::Load,
                        output: Some(output[lane]),
                        inputs: vec![address],
                        userop: None,
                    });
                }
                lanes.insert(wide_output_value.unwrap(), output);
            }
            OpCode::Copy if wide_output_value.is_some() && wide_inputs.len() == 1 => {
                let source = wide_lanes(wide_inputs[0], &lanes, regs, fresh)?;
                let output = wide_output(wide_output_value.unwrap(), regs, fresh)?;
                for lane in 0..4 {
                    out.push(copy(output[lane], source[lane]));
                }
                lanes.insert(wide_output_value.unwrap(), output);
            }
            OpCode::Store if wide_output_value.is_none() && item.inputs.len() == 2 => {
                let source = wide_lanes(item.inputs[1], &lanes, regs, fresh)?;
                for lane in 0..4 {
                    let address = lane_address(item.inputs[0], lane, fresh, &mut out)?;
                    out.push(PcodeOp {
                        opcode: OpCode::Store,
                        output: None,
                        inputs: vec![address, source[lane]],
                        userop: None,
                    });
                }
            }
            OpCode::IntAnd | OpCode::IntOr | OpCode::IntXor
                if wide_output_value.is_some() && wide_inputs.len() == 2 =>
            {
                let left = wide_lanes(wide_inputs[0], &lanes, regs, fresh)?;
                let right = wide_lanes(wide_inputs[1], &lanes, regs, fresh)?;
                let output = wide_output(wide_output_value.unwrap(), regs, fresh)?;
                for lane in 0..4 {
                    out.push(op(item.opcode, output[lane], vec![left[lane], right[lane]]));
                }
                lanes.insert(wide_output_value.unwrap(), output);
            }
            OpCode::IntNegate if wide_output_value.is_some() && wide_inputs.len() == 1 => {
                let source = wide_lanes(wide_inputs[0], &lanes, regs, fresh)?;
                let output = wide_output(wide_output_value.unwrap(), regs, fresh)?;
                for lane in 0..4 {
                    out.push(op(item.opcode, output[lane], vec![source[lane]]));
                }
                lanes.insert(wide_output_value.unwrap(), output);
            }
            _ => return Err(unsupported()),
        }
    }
    Ok(out)
}

fn normalize_direct_ram(
    ops: &[PcodeOp],
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let mut ops = ops.to_vec();
    if let Some(index) = ops.iter().position(|op| {
        op.opcode == OpCode::BranchInd && op.inputs.first().is_some_and(|v| v.space == Space::Ram)
    }) {
        let target = ops[index].inputs[0];
        let unique = fresh(target.size);
        ops[index].inputs[0] = unique;
        ops.insert(
            index,
            PcodeOp {
                opcode: OpCode::Load,
                output: Some(unique),
                inputs: vec![Varnode::constant(target.offset, 4)],
                userop: None,
            },
        );
    }
    let touched: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| {
            !is_control(op)
                && std::iter::once(op.output.as_ref())
                    .chain(op.inputs.iter().map(Some))
                    .flatten()
                    .any(|v| v.space == Space::Ram)
        })
        .map(|(index, _)| index)
        .collect();
    if touched.is_empty() {
        return Ok(ops);
    }
    let mut operands = BTreeSet::new();
    for &index in &touched {
        let op = &ops[index];
        for v in std::iter::once(op.output.as_ref())
            .chain(op.inputs.iter().map(Some))
            .flatten()
        {
            if v.space == Space::Ram {
                operands.insert((v.offset, v.size));
            }
        }
    }
    if operands.len() != 1 {
        return Err(Reason::UnsupportedDirectRam {
            detail: "multiple direct-ram operands".into(),
        });
    }
    let (offset, size) = operands.pop_first().unwrap();
    let address = Varnode::constant(offset, 4);
    let has_write = touched
        .iter()
        .any(|&i| ops[i].output.is_some_and(|v| v.space == Space::Ram));
    let has_read = touched
        .iter()
        .any(|&i| ops[i].inputs.iter().any(|v| v.space == Space::Ram));
    if has_write && has_read {
        let writes = touched
            .iter()
            .filter(|&&i| ops[i].output.is_some_and(|v| v.space == Space::Ram))
            .count();
        if writes != 1 {
            return Err(Reason::UnsupportedDirectRam {
                detail: "unsupported direct-ram read-modify-write".into(),
            });
        }
        let operand = fresh(size);
        let mut out = Vec::with_capacity(ops.len() + 2);
        for mut op in ops {
            if is_control(&op) {
                out.push(op);
                continue;
            }
            let mut changed = false;
            for input in &mut op.inputs {
                if input.space == Space::Ram {
                    *input = operand;
                    changed = true;
                }
            }
            if changed {
                out.push(PcodeOp {
                    opcode: OpCode::Load,
                    output: Some(operand),
                    inputs: vec![address],
                    userop: None,
                });
            }
            if op.output.is_some_and(|v| v.space == Space::Ram) {
                op.output = Some(operand);
                out.push(op);
                out.push(PcodeOp {
                    opcode: OpCode::Store,
                    output: None,
                    inputs: vec![address, operand],
                    userop: None,
                });
            } else {
                out.push(op);
            }
        }
        return Ok(out);
    }
    if has_write {
        for &index in touched.iter().rev() {
            let dst = fresh(size);
            ops[index].output = Some(dst);
            ops.insert(
                index + 1,
                PcodeOp {
                    opcode: OpCode::Store,
                    output: None,
                    inputs: vec![address, dst],
                    userop: None,
                },
            );
        }
        return Ok(ops);
    }
    let src = fresh(size);
    let first = touched[0];
    let mut out = Vec::with_capacity(ops.len() + 1);
    for (index, mut op) in ops.into_iter().enumerate() {
        if index == first {
            out.push(PcodeOp {
                opcode: OpCode::Load,
                output: Some(src),
                inputs: vec![address],
                userop: None,
            });
        }
        if !is_control(&op) {
            for input in &mut op.inputs {
                if input.space == Space::Ram {
                    *input = src;
                }
            }
        }
        out.push(op);
    }
    Ok(out)
}

fn flag(regs: &Registers, name: &str) -> Varnode {
    regs.require(name)
}
fn copy(output: Varnode, input: Varnode) -> PcodeOp {
    PcodeOp {
        opcode: OpCode::Copy,
        output: Some(output),
        inputs: vec![input],
        userop: None,
    }
}
fn op(opcode: OpCode, output: Varnode, inputs: Vec<Varnode>) -> PcodeOp {
    PcodeOp {
        opcode,
        output: Some(output),
        inputs,
        userop: None,
    }
}

fn with_af_zero(ops: &[PcodeOp], regs: &Registers) -> Vec<PcodeOp> {
    let mut out = ops.to_vec();
    out.push(copy(flag(regs, "AF"), Varnode::constant(0, 1)));
    out
}

fn correct_bsr(
    ops: &[PcodeOp],
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: "unsupported BSR p-code shape".into(),
    };
    let zf = flag(regs, "ZF");
    let at = ops
        .iter()
        .position(|op| op.output == Some(zf))
        .ok_or_else(unsupported)?;
    let zero = &ops[at];
    let source = *zero.inputs.first().ok_or_else(unsupported)?;
    let last = ops.last().ok_or_else(unsupported)?;
    let dst = last.output.ok_or_else(unsupported)?;
    if zero.opcode != OpCode::IntEqual
        || zero.inputs.len() != 2
        || zero.inputs[1] != Varnode::constant(0, source.size)
        || last.opcode != OpCode::Copy
        || dst.space != Space::Register
        || !matches!(dst.size, 2 | 4)
        || source.size != dst.size
        || ops[..at].iter().any(is_control)
    {
        return Err(unsupported());
    }
    let leading = fresh(dst.size);
    let index = fresh(dst.size);
    let extended = fresh(dst.size);
    let mask = fresh(dst.size);
    let changed = fresh(dst.size);
    let selected = fresh(dst.size);
    let mut out = ops[..=at].to_vec();
    out.extend([
        op(OpCode::LzCount, leading, vec![source]),
        op(
            OpCode::IntSub,
            index,
            vec![
                Varnode::constant((dst.size * 8 - 1) as u64, dst.size),
                leading,
            ],
        ),
        op(OpCode::IntZExt, extended, vec![zf]),
        op(
            OpCode::IntSub,
            mask,
            vec![extended, Varnode::constant(1, dst.size)],
        ),
        op(OpCode::IntXor, changed, vec![dst, index]),
        op(OpCode::IntAnd, selected, vec![changed, mask]),
        op(OpCode::IntXor, dst, vec![dst, selected]),
    ]);
    Ok(out)
}

fn prepare_rmw(
    ops: &[PcodeOp],
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let stores: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| op.opcode == OpCode::Store)
        .map(|(index, _)| index)
        .collect();
    if stores.is_empty() {
        return Ok(ops.to_vec());
    }
    let loads: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| op.opcode == OpCode::Load)
        .map(|(index, _)| index)
        .collect();
    if stores.len() != 1 || loads.is_empty() {
        return Err(Reason::UnsupportedReadModifyWrite {
            detail: "unsupported memory operand shape".into(),
        });
    }
    let load = &ops[loads[0]];
    if loads
        .iter()
        .any(|&index| ops[index].inputs != load.inputs || ops[index].output != load.output)
    {
        return Err(Reason::UnsupportedReadModifyWrite {
            detail: "repeated memory operand loads disagree".into(),
        });
    }
    let store = &ops[stores[0]];
    let loaded = load
        .output
        .ok_or_else(|| Reason::UnsupportedReadModifyWrite {
            detail: "memory load without an output".into(),
        })?;
    if store.inputs.len() != 2
        || store.inputs[0] != load.inputs[0]
        || store.inputs[1].size != loaded.size
    {
        return Err(Reason::UnsupportedReadModifyWrite {
            detail: "read-modify-write addresses disagree".into(),
        });
    }
    let old = fresh(loaded.size);
    let stored = fresh(store.inputs[1].size);
    let mut out = Vec::with_capacity(ops.len() + 2);
    let mut seen_load = false;
    let mut seen_store = false;
    for op in ops {
        match op.opcode {
            OpCode::Load => {
                if !seen_load {
                    out.push(op.clone());
                    out.push(copy(old, op.output.unwrap()));
                    seen_load = true;
                } else {
                    out.push(copy(
                        op.output.unwrap(),
                        if seen_store { stored } else { old },
                    ));
                }
            }
            OpCode::Store => {
                out.push(copy(stored, op.inputs[1]));
                out.push(op.clone());
                seen_store = true;
            }
            _ => out.push(op.clone()),
        }
    }
    Ok(out)
}

fn defer_flags(
    ops: Vec<PcodeOp>,
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Vec<PcodeOp> {
    let mut names = BTreeMap::new();
    let mut deferred = Vec::new();
    let mut out = Vec::with_capacity(ops.len() + 8);
    for op in ops {
        let mut op = op;
        for input in &mut op.inputs {
            if let Some(&replacement) = names.get(input) {
                *input = replacement;
            }
        }
        if let Some(output) = op.output {
            let is_flag = output.space == Space::Register
                && ["CF", "PF", "AF", "ZF", "SF", "OF"].iter().any(|name| {
                    let flag = regs.require(name);
                    output.offset == flag.offset && output.size == flag.size
                });
            if is_flag {
                let replacement = fresh(output.size);
                op.output = Some(replacement);
                deferred.push((output, replacement));
                names.insert(output, replacement);
            }
        }
        out.push(op);
    }
    out.extend(
        deferred
            .into_iter()
            .map(|(register, value)| copy(register, value)),
    );
    out
}

fn correct_arithmetic(
    mnem: &str,
    ops: &[PcodeOp],
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
    flag_operands: &mut Option<(Varnode, Varnode)>,
) -> Result<Vec<PcodeOp>, Reason> {
    let mut ops = ops.to_vec();
    let (_, a, b, result, insert) = if mnem == "ADC" || mnem == "SBB" {
        let opcode = if mnem == "ADC" {
            OpCode::IntAdd
        } else {
            OpCode::IntSub
        };
        let primary = ops
            .iter()
            .rposition(|op| {
                op.opcode == opcode
                    && op
                        .output
                        .is_some_and(|v| matches!(v.space, Space::Register | Space::Unique))
            })
            .ok_or_else(|| Reason::UnsupportedArithmetic {
                mnemonic: mnem.into(),
            })?;
        let result = ops[primary].output.unwrap();
        let first_cf = ops
            .iter()
            .position(|op| op.output == Some(flag(regs, "CF")))
            .ok_or_else(|| Reason::UnsupportedArithmetic {
                mnemonic: mnem.into(),
            })?;
        let expected = if mnem == "ADC" {
            OpCode::IntCarry
        } else {
            OpCode::IntLess
        };
        let inputs = if ops[first_cf].opcode == expected {
            ops[first_cf].inputs.clone()
        } else if ops[first_cf].opcode == OpCode::Copy
            && ops[first_cf].inputs == [Varnode::constant(0, 1)]
        {
            vec![result, result]
        } else {
            return Err(Reason::UnsupportedArithmetic {
                mnemonic: mnem.into(),
            });
        };
        if inputs.len() < 2 {
            return Err(Reason::UnsupportedArithmetic {
                mnemonic: mnem.into(),
            });
        }
        let a = fresh(result.size);
        let b = fresh(result.size);
        *flag_operands = Some((a, b));
        ops.splice(first_cf..first_cf, [copy(a, inputs[0]), copy(b, inputs[1])]);
        (primary + 2, a, b, result, first_cf + 3)
    } else {
        let opcode = if mnem == "ADD" || mnem == "INC" {
            OpCode::IntAdd
        } else {
            OpCode::IntSub
        };
        let Some(primary) = ops.iter().position(|op| {
            op.opcode == opcode
                && op
                    .output
                    .is_some_and(|v| v.space == Space::Register || v.space == Space::Unique)
        }) else {
            let is_flag = |v: Varnode| {
                ["CF", "PF", "AF", "ZF", "SF", "OF"].iter().any(|name| {
                    let flag = regs.require(name);
                    v.space == Space::Register && v.offset == flag.offset && v.size == flag.size
                })
            };
            let zero_result = ops.iter().any(|op| {
                op.opcode == OpCode::Copy
                    && op
                        .inputs
                        .first()
                        .is_some_and(|v| v.space == Space::Const && v.offset == 0)
                    && op
                        .output
                        .is_some_and(|v| v.space == Space::Register && !is_flag(v))
            });
            if (mnem == "SUB" || mnem == "CMP") && zero_result {
                let mut out = ops.to_vec();
                let af = regs.require("AF");
                if !out.iter().any(|op| op.output == Some(af)) {
                    out.push(copy(af, Varnode::constant(0, 1)));
                }
                return Ok(out);
            }
            return Err(Reason::UnsupportedArithmetic {
                mnemonic: mnem.into(),
            });
        };
        let result = ops[primary].output.unwrap();
        let inputs = ops[primary].inputs.clone();
        if inputs.len() < 2 {
            return Err(Reason::UnsupportedArithmetic {
                mnemonic: mnem.into(),
            });
        }
        let a = fresh(result.size);
        let b = fresh(result.size);
        ops.splice(primary..primary, [copy(a, inputs[0]), copy(b, inputs[1])]);
        (primary + 2, a, b, result, primary + 3)
    };
    let x = fresh(result.size);
    let y = fresh(result.size);
    let z = fresh(result.size);
    let recipe = vec![
        op(OpCode::IntXor, x, vec![a, b]),
        op(OpCode::IntXor, y, vec![x, result]),
        op(
            OpCode::IntRight,
            z,
            vec![y, Varnode::constant(4, result.size)],
        ),
        op(
            OpCode::IntAnd,
            flag(regs, "AF"),
            vec![z, Varnode::constant(1, result.size)],
        ),
    ];
    ops.splice(insert..insert, recipe);
    Ok(ops)
}

// OF is undefined for counts above 1; this and correct_sar match the runtime's
// shl/shr/sar _f helpers (docs/ir.md, Lazy flags) instead of Ghidra's preserved OF.
fn correct_shift(
    mnem: &str,
    ops: &[PcodeOp],
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let mut ops = ops.to_vec();
    let opcode = if mnem == "SHL" {
        OpCode::IntLeft
    } else {
        OpCode::IntRight
    };
    let index = ops
        .iter()
        .position(|op| {
            op.opcode == opcode
                && op.output.is_some_and(|v| {
                    matches!(v.space, Space::Register | Space::Unique)
                        && matches!(v.size, 1 | 2 | 4)
                })
        })
        .ok_or_else(|| Reason::UnsupportedShift {
            mnemonic: mnem.into(),
        })?;
    let source = ops[index].output.unwrap();
    let count = ops[index].inputs[1];
    let original = fresh(source.size);
    let old_of = fresh(1);
    ops.insert(index, copy(original, ops[index].inputs[0]));
    ops.insert(0, copy(old_of, flag(regs, "OF")));
    let nonzero = fresh(1);
    let zero = fresh(1);
    let sign = fresh(1);
    let changed = fresh(1);
    let kept = fresh(1);
    let updated = fresh(1);
    let sign_source = if mnem == "SHL" { source } else { original };
    let mut recipe = vec![
        op(
            OpCode::IntNotEqual,
            nonzero,
            vec![count, Varnode::constant(0, count.size)],
        ),
        PcodeOp {
            opcode: OpCode::BoolNegate,
            output: Some(zero),
            inputs: vec![nonzero],
            userop: None,
        },
        op(
            OpCode::IntSLess,
            sign,
            vec![sign_source, Varnode::constant(0, sign_source.size)],
        ),
    ];
    if mnem == "SHL" {
        let x = fresh(1);
        recipe.push(op(OpCode::IntXor, x, vec![sign, flag(regs, "CF")]));
        recipe.push(op(OpCode::IntAnd, changed, vec![nonzero, x]));
    } else {
        recipe.push(op(OpCode::IntAnd, changed, vec![nonzero, sign]));
    }
    recipe.push(op(OpCode::IntAnd, kept, vec![zero, old_of]));
    recipe.push(op(OpCode::IntOr, updated, vec![changed, kept]));
    recipe.push(copy(flag(regs, "OF"), updated));
    ops.extend(recipe);
    Ok(ops)
}

fn correct_sar(
    ops: &[PcodeOp],
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let mut ops = ops.to_vec();
    let index = ops
        .iter()
        .position(|op| {
            op.opcode == OpCode::IntSRight
                && op.output.is_some_and(|v| {
                    matches!(v.space, Space::Register | Space::Unique)
                        && matches!(v.size, 1 | 2 | 4)
                })
        })
        .ok_or_else(|| Reason::UnsupportedShift {
            mnemonic: "SAR".into(),
        })?;
    let count = ops[index].inputs[1];
    let old_of = fresh(1);
    let nonzero = fresh(1);
    let zero = fresh(1);
    let updated = fresh(1);
    ops.insert(0, copy(old_of, flag(regs, "OF")));
    ops.extend([
        op(
            OpCode::IntNotEqual,
            nonzero,
            vec![count, Varnode::constant(0, count.size)],
        ),
        PcodeOp {
            opcode: OpCode::BoolNegate,
            output: Some(zero),
            inputs: vec![nonzero],
            userop: None,
        },
        op(OpCode::IntAnd, updated, vec![zero, old_of]),
        copy(flag(regs, "OF"), updated),
    ]);
    Ok(ops)
}

fn correct_neg(
    ops: &[PcodeOp],
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let mut ops = ops.to_vec();
    let result = ops
        .iter()
        .find(|op| {
            op.opcode == OpCode::Int2Comp
                && op
                    .output
                    .is_some_and(|v| matches!(v.space, Space::Register | Space::Unique))
        })
        .and_then(|op| op.output)
        .ok_or_else(|| Reason::UnsupportedArithmetic {
            mnemonic: "NEG".into(),
        })?;
    let low = fresh(result.size);
    let af = fresh(1);
    ops.extend([
        op(
            OpCode::IntAnd,
            low,
            vec![result, Varnode::constant(0xf, result.size)],
        ),
        op(
            OpCode::IntNotEqual,
            af,
            vec![low, Varnode::constant(0, result.size)],
        ),
        copy(flag(regs, "AF"), af),
    ]);
    Ok(ops)
}

fn correct_div_cg(
    mnem: &str,
    ops: &[PcodeOp],
    regs: &Registers,
    addr: Addr,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<CgOp>, Reason> {
    let signed = mnem == "IDIV";
    let div = if signed {
        OpCode::IntSDiv
    } else {
        OpCode::IntDiv
    };
    let rem = if signed {
        OpCode::IntSRem
    } else {
        OpCode::IntRem
    };
    let unsupported = || Reason::UnsupportedDivide {
        mnemonic: mnem.into(),
    };
    let index = ops
        .iter()
        .position(|op| op.opcode == div)
        .ok_or_else(unsupported)?;
    let division = &ops[index];
    let quotient = division.output.ok_or_else(unsupported)?;
    let remainder = ops
        .iter()
        .find(|op| op.opcode == rem && op.inputs == division.inputs)
        .ok_or_else(unsupported)?;
    let remainder_value = remainder.output.ok_or_else(unsupported)?;
    let width = match quotient.size {
        2 => 1u32,
        4 => 2u32,
        8 => 4u32,
        _ => return Err(unsupported()),
    };
    if division.inputs.iter().any(|v| v.size != quotient.size) {
        return Err(unsupported());
    }
    let outputs: [(&str, Varnode, u64); 2] = match width {
        1 => [("AL", quotient, 0), ("AH", remainder_value, 1)],
        2 => [("AX", quotient, 0), ("DX", remainder_value, 4)],
        _ => [("EAX", quotient, 0), ("EDX", remainder_value, 4)],
    };
    let mut emitted = [false; 2];
    for op in &ops[index..] {
        if op.opcode == div || op.opcode == rem {
            continue;
        }
        let matched = outputs
            .iter()
            .enumerate()
            .position(|(n, (name, source, _))| {
                let register = regs.require(name);
                !emitted[n]
                    && op.opcode == OpCode::SubPiece
                    && op.output == Some(register)
                    && op.inputs.len() == 2
                    && op.inputs[0] == *source
                    && op.inputs[1].space == Space::Const
                    && op.inputs[1].offset == 0
            });
        match matched {
            Some(n) => emitted[n] = true,
            None => return Err(unsupported()),
        }
    }
    if emitted.iter().any(|found| !found) {
        return Err(unsupported());
    }
    let result = fresh(8);
    let mut inputs = division.inputs.clone();
    inputs.push(Varnode::constant(addr as u64, 4));
    let opcode = if signed {
        Op::Idiv {
            addr,
            size: width as u8,
        }
    } else {
        Op::Div {
            addr,
            size: width as u8,
        }
    };
    let mut out: Vec<CgOp> = ops[..index].iter().cloned().map(from_pcode).collect();
    out.push(CgOp {
        op: opcode,
        output: Some(result),
        inputs,
    });
    out.extend(outputs.into_iter().map(|(name, _, offset)| CgOp {
        op: Op::P(OpCode::SubPiece),
        output: Some(regs.require(name)),
        inputs: vec![result, Varnode::constant(offset, 4)],
    }));
    Ok(out)
}

fn apply_seh(ops: &mut Vec<CgOp>, kinds: &[SehEffect]) {
    for kind in kinds {
        match kind {
            SehEffect::Landing => ops.insert(0, cg(Op::Seh(*kind))),
            SehEffect::Orphan => {
                if let Some(at) = ops.iter().position(|op| op.op == Op::Return) {
                    ops.insert(at, cg(Op::Seh(*kind)));
                } else {
                    ops.push(cg(Op::Seh(*kind)));
                }
            }
            _ => ops.push(cg(Op::Seh(*kind))),
        }
    }
}
