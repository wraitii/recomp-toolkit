use capstone::arch::x86::{ArchMode, X86OperandType};
use capstone::prelude::*;
use capstone::{Capstone, Insn};
use rk_core::isa::Decoded;
use rk_core::{Addr, Result};

use rk_core::ir::{Operand, OperandKind, Part, Repeat};

pub const REG32: [&str; 8] = ["EAX", "ECX", "EDX", "EBX", "ESP", "EBP", "ESI", "EDI"];
pub const REG16: [&str; 8] = ["AX", "CX", "DX", "BX", "SP", "BP", "SI", "DI"];
pub const REG8L: [&str; 4] = ["AL", "CL", "DL", "BL"];
pub const REG8H: [&str; 4] = ["AH", "CH", "DH", "BH"];
pub const JCC: [&str; 35] = [
    "JA", "JAE", "JB", "JBE", "JC", "JE", "JECXZ", "JCXZ", "JG", "JGE", "JL", "JLE", "JNA", "JNAE",
    "JNB", "JNBE", "JNC", "JNE", "JNG", "JNGE", "JNL", "JNLE", "JNO", "JNP", "JNS", "JNZ", "JO",
    "JP", "JPE", "JPO", "JS", "JZ", "LOOP", "LOOPE", "LOOPNE",
];
pub const STRING_MNEM: [&str; 19] = [
    "STOSB", "STOSW", "STOSD", "MOVSB", "MOVSW", "MOVSD", "LODSB", "LODSW", "LODSD", "SCASB",
    "SCASW", "SCASD", "CMPSB", "CMPSW", "CMPSD", "INSB", "INSW", "INSD", "OUTSB",
];
const STRING_ROOTS: [&str; 7] = ["MOVS", "STOS", "LODS", "CMPS", "SCAS", "INS", "OUTS"];
const LEGACY_PREFIXES: [u8; 12] = [
    0xF0, 0xF2, 0xF3, 0x26, 0x2E, 0x36, 0x3E, 0x64, 0x65, 0x66, 0x67, 0x00,
];
const REP_PREFIX: [(&str, Repeat); 5] = [
    ("rep", Repeat::Rep),
    ("repe", Repeat::Rep),
    ("repz", Repeat::Rep),
    ("repne", Repeat::Repne),
    ("repnz", Repeat::Repne),
];
const X87_INT: [&str; 20] = [
    "FILD", "FIST", "FISTP", "FIADD", "FISUB", "FISUBR", "FIMUL", "FIDIV", "FIDIVR", "FICOM",
    "FICOMP", "FNSTSW", "FSTSW", "FNSTCW", "FSTCW", "FLDCW", "FNSTENV", "FLDENV", "FNSAVE",
    "FSAVE",
];
const STRUCTURED: [&str; 6] = ["FNSTENV", "FSTENV", "FLDENV", "FNSAVE", "FSAVE", "FRSTOR"];

pub(crate) struct Decoder {
    md: Capstone,
}

impl Decoder {
    pub(crate) fn new() -> Result<Self> {
        let md = Capstone::new()
            .x86()
            .mode(ArchMode::Mode32)
            .detail(true)
            .build()
            .map_err(|e| rk_core::Error::input(format!("cannot initialize capstone: {e}")))?;
        Ok(Self { md })
    }

    pub(crate) fn decode_span(&self, addr: Addr, raw: &[u8]) -> Option<Decoded> {
        let decoded = self.md.disasm_count(raw, addr, 2).ok()?;
        let instructions: Vec<&Insn> = decoded.iter().collect();
        if instructions.len() == 1 && instructions[0].len() == raw.len() {
            return self.record(instructions[0], raw, addr, None);
        }
        if instructions.len() == 2
            && matches!(instructions[0].mnemonic(), Some("wait" | "fwait"))
            && instructions[1]
                .mnemonic()
                .is_some_and(|m| m.starts_with("fn"))
            && instructions[0].len() + instructions[1].len() == raw.len()
        {
            let ci = instructions[1];
            let suffix = ci.mnemonic()?[2..].to_uppercase();
            let mnemonic = alias(&format!("F{suffix}"));
            return self.record(ci, raw, addr, Some(&mnemonic));
        }
        None
    }

    fn record(
        &self,
        ci: &Insn,
        source_bytes: &[u8],
        address: Addr,
        mnemonic_override: Option<&str>,
    ) -> Option<Decoded> {
        let raw_mnemonic = ci.mnemonic()?;
        let mut words: Vec<&str> = raw_mnemonic.split_whitespace().collect();
        let rep = words
            .first()
            .and_then(|w| REP_PREFIX.iter().find(|(p, _)| p == w).map(|(_, r)| *r));
        if rep.is_some() {
            words.remove(0);
        }
        let mut mnemonic = words.last()?.to_uppercase();
        mnemonic = alias(&mnemonic);
        if let Some(over) = mnemonic_override {
            mnemonic = over.to_owned();
        }
        let detail = self.md.insn_detail(ci).ok()?;
        let ArchDetail::X86Detail(x86) = detail.arch_detail();
        let address_size = x86.addr_size();
        let mut operands = Vec::new();
        for op in x86.operands() {
            let size = (op.size > 0).then_some(op.size as u16 * 8);
            let (kind, display) = match op.op_type {
                X86OperandType::Reg(reg) => {
                    let name = self.md.reg_name(reg)?.to_uppercase();
                    let kind = reg_kind(&name);
                    let display = if name.starts_with("ST(") && name.ends_with(')') {
                        format!("ST{}", &name[3..name.len() - 1])
                    } else {
                        name.clone()
                    };
                    (kind, display)
                }
                X86OperandType::Imm(value) => {
                    let display = if value < 0 {
                        format!("-0x{:x}", -(value as i128))
                    } else {
                        format!("0x{value:x}")
                    };
                    (OperandKind::Imm(value), display)
                }
                X86OperandType::Mem(mem) => {
                    let base = self
                        .md
                        .reg_name(mem.base())
                        .and_then(|n| gpr_index(&n.to_uppercase()));
                    let index = self
                        .md
                        .reg_name(mem.index())
                        .and_then(|n| gpr_index(&n.to_uppercase()));
                    let seg_name = self.md.reg_name(mem.segment()).map(|s| s.to_uppercase());
                    let kind = OperandKind::Mem {
                        base,
                        index,
                        scale: mem.scale() as u8,
                        disp: mem.disp(),
                        seg: seg_name.filter(|s| !s.is_empty() && s != "INVALID"),
                    };
                    let display = self.mem_text(&mnemonic, size, &kind, address_size);
                    (kind, display)
                }
                X86OperandType::Invalid => return None,
            };
            operands.push(Operand {
                kind,
                size,
                display,
            });
        }
        let mut rep = rep;
        if rep.is_none()
            && STRING_ROOTS.contains(&mnemonic.trim_end_matches(['B', 'W', 'D']))
            && !operands
                .iter()
                .any(|o| matches!(o.kind, OperandKind::Xmm(_)))
        {
            for &byte in source_bytes {
                if !LEGACY_PREFIXES.contains(&byte) {
                    break;
                }
                if byte == 0xF2 {
                    rep = Some(Repeat::Repne);
                } else if byte == 0xF3 {
                    rep = Some(Repeat::Rep);
                }
            }
        }
        if mnemonic == "XLATB" {
            mnemonic = "XLAT".to_owned();
        }
        if mnemonic == "XCHG"
            && source_bytes
                .first()
                .is_some_and(|b| (0x90..=0x97).contains(b))
            && operands.len() == 2
        {
            operands.reverse();
        }
        let hidden = rep.is_some()
            || (STRING_MNEM.contains(&mnemonic.as_str())
                && !operands
                    .iter()
                    .any(|o| matches!(o.kind, OperandKind::Xmm(_))));
        let operands_text = if hidden {
            String::new()
        } else {
            operands
                .iter()
                .map(|o| o.display.clone())
                .collect::<Vec<_>>()
                .join(",")
        };
        let text = format!(
            "{address:08x}  {mnemonic}{}{}",
            rep.map(|r| match r {
                Repeat::Rep => ".REP",
                Repeat::Repne => ".REPNE",
            })
            .unwrap_or(""),
            if operands_text.is_empty() {
                String::new()
            } else {
                format!(" {operands_text}")
            }
        );
        let operands = if hidden { Vec::new() } else { operands };
        let length = u8::try_from(source_bytes.len()).ok().filter(|&n| n <= 15)?;
        Some(Decoded {
            length,
            address_size,
            mnemonic,
            operands,
            repeat: rep,
            text,
        })
    }

    fn mem_text(
        &self,
        mnemonic: &str,
        size: Option<u16>,
        kind: &OperandKind,
        address_size: u8,
    ) -> String {
        let OperandKind::Mem {
            base,
            index,
            scale,
            disp,
            seg,
        } = kind
        else {
            return String::new();
        };
        let structured = STRUCTURED.contains(&mnemonic);
        let word = if mnemonic.starts_with('F') && !X87_INT.contains(&mnemonic) {
            match size.map(|s| s / 8) {
                Some(4) => Some("float"),
                Some(8) => Some("double"),
                Some(10) => Some("extended double"),
                _ => None,
            }
        } else {
            match size.map(|s| s / 8) {
                Some(1) => Some("byte"),
                Some(2) => Some("word"),
                Some(4) => Some("dword"),
                Some(8) => Some("qword"),
                Some(10) => Some("extended double"),
                Some(16) => Some("xmmword"),
                _ => None,
            }
        };
        let mut parts = Vec::new();
        let registers = if address_size == 2 {
            &REG16[..]
        } else {
            &REG32[..]
        };
        if let Some(reg) = base {
            parts.push(registers[*reg as usize].to_owned());
        }
        if let Some(reg) = index {
            parts.push(format!("{}*0x{:x}", registers[*reg as usize], scale));
        }
        if *disp != 0 || parts.is_empty() {
            if *disp < 0 {
                parts.push(format!("-0x{:x}", -(*disp as i128)));
            } else {
                parts.push(format!("0x{disp:x}"));
            }
        }
        let prefix = if structured {
            String::new()
        } else {
            word.map(|w| format!("{w} ptr ")).unwrap_or_default()
        };
        let segment = seg.as_ref().map(|s| format!("{s}:")).unwrap_or_default();
        format!("{prefix}{segment}[{}]", parts.join(" + "))
    }
}

fn alias(name: &str) -> String {
    match name {
        "POPAL" => "POPAD",
        "PUSHAL" => "PUSHAD",
        "POPFL" => "POPFD",
        "PUSHFL" => "PUSHFD",
        "CWTL" => "CWDE",
        "CLTD" => "CDQ",
        "CBTW" => "CBW",
        "FCOMPI" => "FCOMIP",
        "FUCOMPI" => "FUCOMIP",
        "IRETD" => "IRET",
        other => other,
    }
    .to_owned()
}

fn reg_kind(name: &str) -> OperandKind {
    if let Some(index) = REG32.iter().position(|r| *r == name) {
        return OperandKind::Reg {
            gpr: index as u8,
            part: None,
        };
    }
    if let Some(index) = REG16.iter().position(|r| *r == name) {
        return OperandKind::Reg {
            gpr: index as u8,
            part: None,
        };
    }
    if let Some(index) = REG8L.iter().position(|r| *r == name) {
        return OperandKind::Reg {
            gpr: index as u8,
            part: Some(Part::L),
        };
    }
    if let Some(index) = REG8H.iter().position(|r| *r == name) {
        return OperandKind::Reg {
            gpr: index as u8,
            part: Some(Part::H),
        };
    }
    if let Some(index) = name.strip_prefix("XMM").and_then(|n| n.parse().ok()) {
        return OperandKind::Xmm(index);
    }
    if let Some(index) = name.strip_prefix("MM").and_then(|n| n.parse().ok()) {
        return OperandKind::Mm(index);
    }
    if let Some(index) = name.strip_prefix("ST").and_then(|n| n.parse().ok()) {
        return OperandKind::St(index);
    }
    let selector = match name {
        "CS" => Some(0x1b),
        "DS" | "ES" | "SS" => Some(0x23),
        "FS" => Some(0x3b),
        "GS" => Some(0x00),
        _ => None,
    };
    if let Some(selector) = selector {
        return OperandKind::Sreg {
            name: name.to_owned(),
            selector,
        };
    }
    OperandKind::Other(name.to_owned())
}

pub fn gpr_index(name: &str) -> Option<u8> {
    REG32
        .iter()
        .position(|r| *r == name)
        .or_else(|| REG16.iter().position(|r| *r == name))
        .map(|i| i as u8)
}

pub fn gpr32(op: &Operand) -> Option<u8> {
    match op.kind {
        OperandKind::Reg { gpr, part: None } if op.size == Some(32) => Some(gpr),
        _ => None,
    }
}

pub fn imm(op: &Operand) -> Option<i64> {
    match op.kind {
        OperandKind::Imm(value) => Some(value),
        _ => None,
    }
}

pub type MemOperand<'a> = (Option<u8>, Option<u8>, u8, i64, Option<&'a str>);

pub fn mem<'a>(op: &'a Operand) -> Option<MemOperand<'a>> {
    match &op.kind {
        OperandKind::Mem {
            base,
            index,
            scale,
            disp,
            seg,
        } => Some((*base, *index, *scale, *disp, seg.as_deref())),
        _ => None,
    }
}

pub fn is_xmm(op: &Operand) -> bool {
    matches!(op.kind, OperandKind::Xmm(_))
}
