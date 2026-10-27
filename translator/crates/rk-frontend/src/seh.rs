use std::collections::{BTreeMap, BTreeSet};

use rk_core::ir::SehEffect;
use rk_core::program::{Image, Program};
use rk_core::{Addr, Diagnostic, Result};
use rk_decode::{gpr32, imm, mem, DecodedInsn, Operand, OperandKind, JCC};

use crate::cfg::RawFunction;
use crate::{R_EAX, R_EBP, R_ECX, R_EDX, R_ESP};

const MSVC_SEH_PROLOG_HEX: &str = "68????????64a100000000508b442410896c24108d6c24102be05356578b45f88965e8508b45fcc745fcffffffff8945f88d45f064a300000000c3";
const MSVC_SEH_EPILOG_HEX: &str = "8b4df064890d00000000595f5e5bc951c3";
const MSVC_CHKSTK_HEX: &str = "3d00100000730ef7d803c483c0048500948b0050c3518d4c240881e9001000002d0010000085013d0010000073ec2bc88bc485018be18b088b400450c3";
const MSVC_EH3_FRAME_HEX: &str = "558bec6aff6800000000680000000064a1000000005064892500000000";
pub(crate) const MSVC_SEH_PROLOG_LEN: u32 = 59;
pub(crate) const MSVC_EH3_FRAME_LEN: u32 = 29;
const MSVC_EH3_HANDLER_HEX: &str = "558bec83ec0853565755fc8b5d0c8b4508f74004060000000f85";

const EXPLICIT_DESTINATION: [&str; 22] = [
    "MOV", "LEA", "ADD", "SUB", "AND", "OR", "XOR", "INC", "DEC", "NEG", "NOT", "SHL", "SHR",
    "SAR", "MOVZX", "MOVSX", "POP", "PUSH", "CMP", "TEST", "JMP", "NOP",
];

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn seh_pattern(hex: &str, wildcards: &[usize]) -> Vec<Option<u8>> {
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    let mut at = 0;
    while i + 1 < bytes.len() {
        let hi = hex_nibble(bytes[i]).unwrap_or(0);
        let lo = hex_nibble(bytes[i + 1]).unwrap_or(0);
        out.push(if wildcards.contains(&at) {
            None
        } else {
            Some((hi << 4) | lo)
        });
        i += 2;
        at += 1;
    }
    out
}

pub fn code_matches(image: &rk_core::program::Image, va: Addr, pattern: &[Option<u8>]) -> bool {
    if !image.is_exec(va) || pattern.is_empty() || !image.is_exec(va + pattern.len() as u32 - 1) {
        return false;
    }
    let Some(code) = image.bytes(va, pattern.len()) else {
        return false;
    };
    code.iter()
        .zip(pattern)
        .all(|(b, p)| p.is_none_or(|p| p == *b))
}

pub fn msvc_seh_prolog(image: &rk_core::program::Image, va: Addr) -> Option<Addr> {
    code_matches(image, va, &seh_pattern(MSVC_SEH_PROLOG_HEX, &[1, 2, 3, 4]))
        .then(|| image.rd32(va + 1))
        .flatten()
        .filter(|handler| image.is_exec(*handler))
}

pub fn msvc_seh_epilog(image: &rk_core::program::Image, va: Addr) -> bool {
    code_matches(image, va, &seh_pattern(MSVC_SEH_EPILOG_HEX, &[]))
}

pub fn msvc_chkstk(image: &rk_core::program::Image, va: Addr) -> bool {
    code_matches(image, va, &seh_pattern(MSVC_CHKSTK_HEX, &[]))
}

pub(crate) fn msvc_unwind_only(image: &rk_core::program::Image, handler: Option<Addr>) -> bool {
    let Some(handler) = handler else {
        return false;
    };
    let mut pattern = vec![
        Some(0xb8),
        None,
        None,
        None,
        None,
        Some(0xe9),
        None,
        None,
        None,
        None,
    ];
    if !code_matches(image, handler, &pattern) {
        return false;
    }
    pattern.clear();
    let Some(info) = image.rd32(handler + 1) else {
        return false;
    };
    matches!(image.rd32(info), Some(0x19930520..=0x19930522)) && image.rd32(info + 12) == Some(0)
}

pub(crate) fn msvc_eh3_finally_only(
    image: &rk_core::program::Image,
    va: Addr,
    levels: Option<&std::collections::BTreeSet<u32>>,
) -> bool {
    let Some(levels) = levels else {
        return false;
    };
    if !code_matches(
        image,
        va,
        &seh_pattern(MSVC_EH3_FRAME_HEX, &[6, 7, 8, 9, 11, 12, 13, 14]),
    ) {
        return false;
    }
    let Some(handler) = image.rd32(va + 11) else {
        return false;
    };
    let handler_pattern = seh_pattern(MSVC_EH3_HANDLER_HEX, &[]);
    if !code_matches(image, handler, &handler_pattern) {
        return false;
    }
    let Some(scope) = image.rd32(va + 6) else {
        return false;
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut todo: Vec<u32> = levels.iter().copied().collect();
    while let Some(level) = todo.pop() {
        if level == 0xffff_ffff || !seen.insert(level) {
            continue;
        }
        if level >= 0x100 || image.rd32(scope + 12 * level + 4) != Some(0) {
            return false;
        }
        if let Some(parent) = image.rd32(scope + 12 * level) {
            todo.push(parent);
        }
    }
    true
}

pub fn seh_constructor_helper(image: &rk_core::program::Image, va: Addr) -> Option<Addr> {
    let prefix = "52515384d27c03ff50f431d28d4c2410648b1a8919896908c74104";
    let suffix = "89410c64890a5b595ac3";
    let prefix = seh_pattern(prefix, &[]);
    let suffix = seh_pattern(suffix, &[]);
    let size = prefix.len() + 4 + suffix.len();
    if !image.is_exec(va) || !image.is_exec(va + size as u32 - 1) {
        return None;
    }
    let code = image.bytes(va, size)?;
    if !code[..prefix.len()]
        .iter()
        .zip(&prefix)
        .all(|(b, p)| p.is_none_or(|p| p == *b))
        || !code[prefix.len() + 4..]
            .iter()
            .zip(&suffix)
            .all(|(b, p)| p.is_none_or(|p| p == *b))
    {
        return None;
    }
    let candidate = image.rd32(va + prefix.len() as u32)?;
    image.is_exec(candidate).then_some(candidate)
}

impl RawFunction {
    fn calls_seh_epilog(&self, program: &Program, i: usize) -> bool {
        self.decoded.get(i).is_some_and(|ins| {
            ins.mnemonic == "CALL"
                && self
                    .branch_target(i)
                    .is_some_and(|target| msvc_seh_epilog(&program.image, target))
        })
    }

    pub(crate) fn seh_escaping_returns(
        &self,
        program: &Program,
        helpers: &BTreeSet<Addr>,
    ) -> BTreeSet<u32> {
        if self.seh_sites.is_empty()
            && !self.decoded.iter().any(|ins| {
                ins.mnemonic == "CALL"
                    && self
                        .branch_target(self.index_of(ins.addr))
                        .is_some_and(|target| helpers.contains(&target))
            })
        {
            return BTreeSet::new();
        }
        let returns = self.popped_return_jumps(&[]);
        let mut work = vec![(self.idx(self.addr).unwrap_or(0), false)];
        let mut seen = BTreeSet::new();
        let mut escapes = BTreeSet::new();
        while let Some((i, mut active)) = work.pop() {
            if !seen.insert((i, active)) {
                continue;
            }
            let index = i as usize;
            let ins = &self.decoded[index];
            if self.seh_sites.contains_key(&i)
                || (ins.mnemonic == "CALL"
                    && self
                        .branch_target(index)
                        .is_some_and(|t| helpers.contains(&t)))
            {
                active = true;
            } else if self.seh_restores.contains(&i) || self.calls_seh_epilog(program, index) {
                active = false;
            }
            if returns.contains(&i)
                || (ins.mnemonic == "RET" && self.push_ret_target(index).is_none())
            {
                if active {
                    escapes.insert(i);
                }
                if returns.contains(&i) {
                    continue;
                }
            }
            work.extend(
                self.successors(program, index)
                    .into_iter()
                    .map(|j| (j, active)),
            );
        }
        escapes
    }

    pub(crate) fn seh_effects(
        &self,
        program: &Program,
        helpers: &BTreeSet<Addr>,
    ) -> Result<BTreeMap<u32, Vec<SehEffect>>> {
        for &i in self.seh_sites.keys() {
            if self.decoded[i as usize].mnemonic != "MOV" {
                return Err(Diagnostic::error(
                    self.decoded[i as usize].addr,
                    rk_core::Reason::Unsupported {
                        detail: "SEH frame site is not a MOV".into(),
                    },
                ));
            }
        }
        let mut effects = BTreeMap::new();
        let unlinks = !self.seh_sites.is_empty() || msvc_seh_epilog(&program.image, self.addr);
        for (i, ins) in self.decoded.iter().enumerate() {
            let i = i as u32;
            if seh_except_entry(ins) {
                effects.insert(i, vec![SehEffect::Landing]);
            } else if ins.mnemonic == "MOV" && unlinks && ins.operands.len() == 2 {
                let dst = &ins.operands[0];
                let src = &ins.operands[1];
                if (seh_chain_operand(dst, 0) || self.seh_sites.contains_key(&i))
                    && gpr32(src).is_some()
                {
                    effects.insert(
                        i,
                        vec![if self.seh_sites.contains_key(&i) {
                            SehEffect::Enter
                        } else {
                            SehEffect::Leave
                        }],
                    );
                }
            } else if ins.mnemonic == "POP" && self.seh_restores.contains(&i) {
                effects.insert(i, vec![SehEffect::Leave]);
            } else if ins.mnemonic == "CALL"
                && self
                    .branch_target(i as usize)
                    .is_some_and(|t| helpers.contains(&t))
            {
                effects.insert(i, vec![SehEffect::Adopt]);
            } else if ins.mnemonic == "RET" && self.seh_escapes.contains(&i) {
                effects.insert(i, vec![SehEffect::Orphan]);
            }
        }
        Ok(effects)
    }
}

pub(crate) fn seh_helpers(
    program: &Program,
    raw: &[(Addr, Result<RawFunction>)],
) -> BTreeSet<Addr> {
    let mut helpers: BTreeSet<Addr> = raw
        .iter()
        .filter_map(|(addr, raw)| {
            let raw = raw.as_ref().ok()?;
            (!raw
                .seh_escaping_returns(program, &BTreeSet::new())
                .is_empty())
            .then_some(*addr)
        })
        .collect();
    loop {
        let pending: Vec<Addr> = raw
            .iter()
            .filter_map(|(addr, raw)| {
                let raw = raw.as_ref().ok()?;
                if helpers.contains(addr) {
                    return None;
                }
                let calls = raw.decoded.iter().enumerate().filter_map(|(i, ins)| {
                    (ins.mnemonic == "CALL")
                        .then(|| raw.branch_target(i))
                        .flatten()
                });
                calls
                    .into_iter()
                    .any(|target| helpers.contains(&target))
                    .then_some(*addr)
            })
            .collect();
        if pending.is_empty() {
            return helpers;
        }
        let mut found = BTreeSet::new();
        for addr in pending {
            if let Some(Ok(raw)) = raw.iter().find(|(a, _)| *a == addr).map(|(_, r)| r) {
                if !raw.seh_escaping_returns(program, &helpers).is_empty() {
                    found.insert(addr);
                }
            }
        }
        if found.is_empty() {
            return helpers;
        }
        helpers.extend(found);
    }
}

fn seh_chain_operand(op: &Operand, zero_base: u8) -> bool {
    mem(op).is_some_and(|(base, index, _scale, disp, seg)| {
        op.size == Some(32)
            && seg == Some("FS")
            && (base.is_none() || base == Some(zero_base))
            && index.is_none()
            && disp == 0
    })
}

fn writes_reg32(ins: &DecodedInsn, reg: u8) -> bool {
    if ins.operands.is_empty() {
        return false;
    }
    if ins.mnemonic.starts_with("LOOP") {
        return reg == R_ECX;
    }
    if matches!(ins.mnemonic.as_str(), "CMP" | "TEST" | "PUSH" | "JMP")
        || JCC.contains(&ins.mnemonic.as_str())
    {
        return false;
    }
    match ins.operands[0].kind {
        OperandKind::Reg { gpr, .. } => gpr == reg,
        _ => true,
    }
}

fn seh_zero_base(raw: &RawFunction, i: usize, reg: Option<u8>) -> bool {
    let Some(reg) = reg else {
        return true;
    };
    if reg == R_ESP {
        return false;
    }
    let targets: BTreeSet<Addr> = raw
        .decoded
        .iter()
        .enumerate()
        .filter(|(_, ins)| JCC.contains(&ins.mnemonic.as_str()) || ins.mnemonic == "JMP")
        .filter_map(|(i, _)| raw.branch_target(i))
        .collect();
    for j in (i.saturating_sub(15)..i).rev() {
        if !raw.contiguous[j] || targets.contains(&raw.decoded[j + 1].addr) {
            return false;
        }
        let ins = &raw.decoded[j];
        if ins.mnemonic == "XOR" && ins.operands.len() == 2 {
            if let (Some(a), Some(b)) = (gpr32(&ins.operands[0]), gpr32(&ins.operands[1])) {
                if a == b && a == reg {
                    return true;
                }
            }
        }
        if !matches!(
            ins.mnemonic.as_str(),
            "MOV"
                | "LEA"
                | "POP"
                | "PUSH"
                | "ADD"
                | "SUB"
                | "AND"
                | "OR"
                | "XOR"
                | "TEST"
                | "CMP"
                | "NOP"
        ) || writes_reg32(ins, reg)
        {
            return false;
        }
    }
    false
}

pub(crate) fn seh_restore_sites(raw: &RawFunction) -> BTreeSet<u32> {
    let mut result = BTreeSet::new();
    for (i, ins) in raw.decoded.iter().enumerate() {
        if !matches!(ins.mnemonic.as_str(), "POP" | "MOV") || ins.operands.is_empty() {
            continue;
        }
        let dst = &ins.operands[0];
        let base = mem(dst).and_then(|(base, _, _, _, _)| base);
        if !seh_chain_operand(dst, base.unwrap_or(0)) || !seh_zero_base(raw, i, base) {
            continue;
        }
        if ins.mnemonic == "POP" {
            result.insert(i as u32);
        } else if ins.operands.len() == 2 {
            if let Some(src) = gpr32(&ins.operands[1]) {
                if src != R_ESP {
                    result.insert(i as u32);
                }
            }
        }
    }
    result
}

pub(crate) fn seh_frame_sites(raw: &RawFunction, image: &Image) -> BTreeMap<u32, Addr> {
    let mut sites = BTreeMap::new();
    if let Some(handler) = msvc_seh_prolog(image, raw.addr) {
        let publish = raw.addr + MSVC_SEH_PROLOG_LEN - 7;
        for (i, ins) in raw.decoded.iter().enumerate() {
            if ins.addr == publish {
                sites.insert(i as u32, handler);
            }
        }
    }
    if let Some(stub) = seh_constructor_helper(image, raw.addr) {
        for (i, ins) in raw.decoded.iter().enumerate() {
            if ins.mnemonic == "MOV" && ins.operands.len() == 2 {
                let dst = &ins.operands[0];
                let src = &ins.operands[1];
                if seh_chain_operand(dst, 2)
                    && mem(dst).and_then(|(base, _, _, _, _)| base) == Some(R_EDX)
                    && gpr32(src) == Some(R_ECX)
                {
                    sites.insert(i as u32, stub);
                }
            }
        }
        for (i, ins) in raw.decoded.iter().enumerate() {
            if i == 0 || ins.mnemonic != "CALL" || !raw.contiguous[i - 1] {
                continue;
            }
            let Some(target) = raw.branch_target(i) else {
                continue;
            };
            let reserve = &raw.decoded[i - 1];
            if !matches!(reserve.mnemonic.as_str(), "ADD" | "SUB") || reserve.operands.len() != 2 {
                continue;
            }
            let dst = &reserve.operands[0];
            let amount = &reserve.operands[1];
            if gpr32(dst) != Some(R_ESP)
                || imm(amount).is_none_or(|v| {
                    v as u32
                        != if reserve.mnemonic == "ADD" {
                            0xffff_fff0
                        } else {
                            16
                        }
                })
            {
                continue;
            }
            if let Some(stub) = seh_constructor_helper(image, target) {
                sites.insert(i as u32, stub);
            }
        }
    }
    for i in 2..raw.decoded.len() {
        let mov = &raw.decoded[i];
        let push = &raw.decoded[i - 1];
        if mov.mnemonic != "MOV" || push.mnemonic != "PUSH" || mov.operands.len() != 2 {
            continue;
        }
        let dst = &mov.operands[0];
        let src = &mov.operands[1];
        let dst_base = mem(dst).and_then(|(base, _, _, _, _)| base);
        let zero_base = if seh_zero_base(raw, i, dst_base) {
            dst_base.unwrap_or(0)
        } else {
            0
        };
        if !seh_chain_operand(dst, zero_base) || gpr32(src) != Some(R_ESP) || !raw.contiguous[i - 1]
        {
            continue;
        }
        let Some(pushed) = push.operands.first() else {
            continue;
        };
        let mut handler_index = i as isize - 2;
        if !seh_chain_operand(pushed, zero_base) {
            let load = &raw.decoded[i - 2];
            let Some(pushed_reg) = gpr32(pushed) else {
                continue;
            };
            if pushed_reg == R_ESP
                || load.mnemonic != "MOV"
                || load.operands.len() != 2
                || !raw.contiguous[i - 2]
            {
                continue;
            }
            let loaded = &load.operands[0];
            let head = &load.operands[1];
            let Some(loaded_reg) = gpr32(loaded) else {
                continue;
            };
            if loaded_reg != pushed_reg || loaded_reg == R_ESP {
                continue;
            }
            let mut head = head.clone();
            if head.size.is_none() && matches!(head.kind, OperandKind::Mem { .. }) {
                head.size = loaded.size;
            }
            if !seh_chain_operand(&head, zero_base) {
                continue;
            }
            handler_index -= 1;
        }
        for j in [handler_index, handler_index - 1] {
            if j < 0 {
                continue;
            }
            let prev = &raw.decoded[j as usize];
            if !raw.contiguous[j as usize] {
                break;
            }
            if prev.mnemonic == "PUSH" {
                if let Some(value) = prev.operands.first().and_then(imm) {
                    sites.insert(i as u32, value as Addr);
                }
                break;
            }
        }
    }
    sites
}

fn seh_except_entry(ins: &DecodedInsn) -> bool {
    if ins.mnemonic != "MOV" || ins.operands.len() != 2 || ins.operands[0].display != "ESP" {
        return false;
    }
    mem(&ins.operands[1]).is_some_and(|(base, index, _scale, disp, seg)| {
        base == Some(R_EBP) && index.is_none() && seg.is_none() && disp as u32 == 0xffff_ffe8
    })
}

fn register_constants(raw: &RawFunction, program: &Program) -> BTreeMap<u32, BTreeMap<u8, u32>> {
    let mut before: BTreeMap<u32, BTreeMap<u8, u32>> = BTreeMap::new();
    let Some(root) = raw.idx(raw.addr) else {
        return before;
    };
    before.insert(root, BTreeMap::new());
    let mut work = vec![root];
    while let Some(i) = work.pop() {
        let ins = &raw.decoded[i as usize];
        let mut state = before.get(&i).cloned().unwrap_or_default();
        let mnem = ins.mnemonic.as_str();
        if mnem == "CALL" {
            for reg in [R_EAX, R_ECX, R_EDX] {
                state.remove(&reg);
            }
        } else if EXPLICIT_DESTINATION.contains(&mnem)
            || (JCC.contains(&mnem) && !mnem.starts_with("LOOP"))
            || mnem.starts_with('F')
        {
            let first = ins.operands.first();
            let second = ins.operands.get(1);
            if first.is_none() || matches!(first.map(|o| &o.kind), Some(OperandKind::Other(_))) {
                state.clear();
            } else if let Some(reg) = first.and_then(gpr32) {
                if !matches!(mnem, "PUSH" | "CMP" | "TEST") {
                    state.remove(&reg);
                    if first.is_some_and(|o| o.size == Some(32)) {
                        if mnem == "XOR" {
                            if second.and_then(gpr32) == Some(reg) {
                                state.insert(reg, 0);
                            }
                        } else if mnem == "MOV" {
                            if let Some(value) = second.and_then(imm) {
                                state.insert(reg, value as u32);
                            }
                        }
                    }
                }
            }
        } else {
            state.clear();
        }
        for j in raw.successors(program, i as usize) {
            let joined = if let Some(old) = before.get(&j) {
                state
                    .iter()
                    .filter(|(reg, value)| old.get(reg) == Some(value))
                    .map(|(reg, value)| (*reg, *value))
                    .collect()
            } else {
                state.clone()
            };
            if before.get(&j) != Some(&joined) {
                before.insert(j, joined);
                work.push(j);
            }
        }
    }
    before
}

pub(crate) fn eh3_trylevels(raw: &RawFunction, program: &Program) -> Option<BTreeSet<u32>> {
    let mut levels = BTreeSet::new();
    let mut constants: Option<BTreeMap<u32, BTreeMap<u8, u32>>> = None;
    for (i, ins) in raw.decoded.iter().enumerate() {
        if ins.operands.is_empty() || matches!(ins.mnemonic.as_str(), "CMP" | "TEST" | "PUSH") {
            continue;
        }
        let dst = &ins.operands[0];
        let src = ins.operands.get(1);
        let Some((base, index, _scale, disp, seg)) = mem(dst) else {
            continue;
        };
        if base != Some(R_EBP) || index.is_some() || seg.is_some() || disp as u32 != 0xffff_fffc {
            continue;
        }
        let value = src.and_then(imm).map(|v| v as u32);
        let source_reg = src.and_then(gpr32);
        if ins.mnemonic == "MOV" && source_reg.is_some() {
            let constants = constants.get_or_insert_with(|| register_constants(raw, program));
            let value = constants
                .get(&(i as u32))
                .and_then(|s| s.get(&source_reg.unwrap()))?;
            levels.insert(*value);
        } else if ins.mnemonic == "MOV" && value.is_some() {
            levels.insert(value.unwrap());
        } else if ins.mnemonic == "AND" && value == Some(0) {
            levels.insert(0);
        } else if ins.mnemonic == "OR" && value == Some(0xffff_ffff) {
            levels.insert(value.unwrap());
        } else {
            return None;
        }
    }
    Some(levels)
}
