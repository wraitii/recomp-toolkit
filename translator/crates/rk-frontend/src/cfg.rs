use std::collections::{BTreeMap, BTreeSet};

use rk_core::ir::{FunctionIr, Insn, OpCode, SehEffect, Space};
use rk_core::program::Program;
use rk_core::{Addr, Diagnostic, Result};
use rk_decode::{branch_target, gpr32, imm, DecodedInsn, Decoder, OperandKind, JCC};
use rk_sleigh::Lifter;

use crate::seh::{
    eh3_trylevels, msvc_eh3_finally_only, msvc_unwind_only, seh_frame_sites, seh_restore_sites,
    MSVC_EH3_FRAME_LEN,
};
use crate::tables::{recover_tables, table_jumps};
use crate::{R_EBP, R_ESP};

const TERMINATORS: [&str; 2] = ["RET", "JMP"];
#[derive(Clone)]
pub(crate) struct RawFunction {
    pub(crate) addr: Addr,
    pub(crate) decoded: Vec<DecodedInsn>,
    pub(crate) insns: Vec<Insn>,
    register_writes: Vec<Option<BTreeSet<u8>>>,
    pub(crate) index: BTreeMap<Addr, u32>,
    pub(crate) contiguous: Vec<bool>,
    pub(crate) fallthrough: Vec<Addr>,
    pub(crate) end: Addr,
    pub(crate) pushed_continuations: BTreeSet<Addr>,
    pub(crate) intra_calls: BTreeMap<u32, Addr>,
    pub(crate) intra_call_returns: BTreeSet<Addr>,
    pub(crate) seh_sites: BTreeMap<u32, Addr>,
    pub(crate) seh_restores: BTreeSet<u32>,
    pub(crate) return_jumps: BTreeSet<u32>,
    pub(crate) seh_escapes: BTreeSet<u32>,
    pub(crate) dead_addrs: BTreeSet<Addr>,
    pub(crate) recovered_tables: BTreeMap<Addr, Vec<Addr>>,
}

#[derive(Clone, PartialEq, Eq)]
struct ReturnState {
    delta: Option<i32>,
    frame: Option<i32>,
    saved: BTreeSet<u8>,
}

impl ReturnState {
    fn unknown() -> Self {
        Self {
            delta: None,
            frame: None,
            saved: BTreeSet::new(),
        }
    }
}

impl RawFunction {
    pub(crate) fn idx(&self, addr: Addr) -> Option<u32> {
        self.index.get(&addr).copied()
    }

    fn first(&self, i: usize) -> Option<&rk_decode::Operand> {
        self.decoded.get(i)?.operands.first()
    }

    pub(crate) fn push_ret_target(&self, i: usize) -> Option<Addr> {
        let ins = self.decoded.get(i)?;
        if ins.mnemonic != "PUSH" || i + 1 >= self.decoded.len() || !self.contiguous[i] {
            return None;
        }
        if self.decoded[i + 1].mnemonic != "RET" || !self.decoded[i + 1].operands.is_empty() {
            return None;
        }
        let target = imm(ins.operands.first()?)?;
        (operand_size(&ins.operands, Some(32))? == 32).then_some(target as Addr)
    }

    pub(crate) fn branch_target(&self, i: usize) -> Option<Addr> {
        branch_target(self.decoded.get(i)?)
    }

    fn import_tail_slot(&self, program: &Program, i: usize) -> Option<Addr> {
        let ins = self.decoded.get(i)?;
        if ins.mnemonic != "JMP" || ins.length != 6 || ins.raw[..2] != [0xff, 0x25] {
            return None;
        }
        let slot = u32::from_le_bytes([ins.raw[2], ins.raw[3], ins.raw[4], ins.raw[5]]);
        program.image.imports.contains_key(&slot).then_some(slot)
    }

    pub(crate) fn successors(&self, program: &Program, i: usize) -> Vec<u32> {
        let Some(ins) = self.decoded.get(i) else {
            return Vec::new();
        };
        let m = ins.mnemonic.as_str();
        if let Some(target) = self.push_ret_target(i) {
            if !self.pushed_continuations.contains(&target) {
                return self.idx(target).into_iter().collect();
            }
        }
        let next = self
            .fallthrough
            .get(i)
            .copied()
            .and_then(|addr| self.idx(addr));
        if let Some(&target) = self.intra_calls.get(&(i as u32)) {
            return std::iter::once(target)
                .chain(self.fallthrough.get(i).copied())
                .filter_map(|target| self.idx(target))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
        }
        if m == "INT3" {
            return Vec::new();
        }
        if m == "RET" {
            return self
                .pushed_continuations
                .iter()
                .chain(self.intra_call_returns.iter())
                .filter_map(|target| self.idx(*target))
                .collect();
        }
        if JCC.contains(&m) {
            let mut out: Vec<u32> = next.into_iter().collect();
            if let Some(target) = self.branch_target(i) {
                if let Some(index) = self.idx(target) {
                    out.push(index);
                }
            }
            return out;
        }
        if m == "JMP" {
            if self.import_tail_slot(program, i).is_some() {
                return Vec::new();
            }
            if let Some(target) = self.branch_target(i) {
                return self.idx(target).into_iter().collect();
            }
            let targets: Vec<Addr> = program
                .tables
                .get(&(self.addr, ins.addr))
                .or_else(|| self.recovered_tables.get(&ins.addr))
                .cloned()
                .unwrap_or_else(|| self.index.keys().copied().collect());
            return targets
                .into_iter()
                .filter_map(|target| self.idx(target))
                .collect();
        }
        next.into_iter().collect()
    }

    pub(crate) fn popped_return_jumps(&self, entries: &[Addr]) -> BTreeSet<u32> {
        if !self.decoded.iter().any(|ins| ins.mnemonic == "POP") {
            return BTreeSet::new();
        }
        let candidates: Vec<usize> = self
            .decoded
            .iter()
            .enumerate()
            .filter(|(_, ins)| {
                ins.mnemonic == "JMP" && ins.operands.first().and_then(gpr32).is_some()
            })
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() {
            return BTreeSet::new();
        }
        let mut states: BTreeMap<u32, ReturnState> = BTreeMap::new();
        let mut pending: Vec<u32> = Vec::new();
        let mut unknown_indirect_seen = false;

        fn merge(
            states: &mut BTreeMap<u32, ReturnState>,
            pending: &mut Vec<u32>,
            i: u32,
            state: ReturnState,
        ) {
            let merged = if let Some(old) = states.get(&i) {
                ReturnState {
                    delta: (old.delta == state.delta).then_some(state.delta).flatten(),
                    frame: (old.frame == state.frame).then_some(state.frame).flatten(),
                    saved: old.saved.intersection(&state.saved).copied().collect(),
                }
            } else {
                state
            };
            if states.get(&i) != Some(&merged) {
                states.insert(i, merged);
                pending.push(i);
            }
        }

        let roots: Vec<Addr> = std::iter::once(self.addr)
            .chain(entries.iter().copied())
            .collect();
        for addr in roots {
            if let Some(i) = self.idx(addr) {
                merge(
                    &mut states,
                    &mut pending,
                    i,
                    ReturnState {
                        delta: Some(0),
                        frame: None,
                        saved: BTreeSet::new(),
                    },
                );
            }
        }
        while let Some(i) = pending.pop() {
            let index = i as usize;
            let Some(ins) = self.decoded.get(index) else {
                continue;
            };
            let Some(state) = states.get(&i).cloned() else {
                continue;
            };
            if candidates.contains(&index)
                && self
                    .first(index)
                    .and_then(gpr32)
                    .is_some_and(|r| state.saved.contains(&r))
            {
                continue;
            }
            let out = self.transfer(index, state);
            if ins.mnemonic == "JMP" && self.branch_target(index).is_none() {
                if !unknown_indirect_seen {
                    for j in 0..self.decoded.len() as u32 {
                        merge(&mut states, &mut pending, j, ReturnState::unknown());
                    }
                    unknown_indirect_seen = true;
                }
                continue;
            }
            for successor in self.successors_during_proof(index) {
                merge(&mut states, &mut pending, successor, out.clone());
            }
        }
        candidates
            .into_iter()
            .filter(|&i| {
                states.get(&(i as u32)).is_some_and(|state| {
                    self.first(i)
                        .and_then(gpr32)
                        .is_some_and(|r| state.saved.contains(&r))
                })
            })
            .map(|i| i as u32)
            .collect()
    }

    fn successors_during_proof(&self, i: usize) -> Vec<u32> {
        let Some(ins) = self.decoded.get(i) else {
            return Vec::new();
        };
        let m = ins.mnemonic.as_str();
        let next = self
            .fallthrough
            .get(i)
            .copied()
            .and_then(|addr| self.idx(addr));
        if let Some(&target) = self.intra_calls.get(&(i as u32)) {
            std::iter::once(target)
                .chain(self.fallthrough.get(i).copied())
                .filter_map(|target| self.idx(target))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else if m == "INT3" {
            Vec::new()
        } else if m == "RET" {
            self.pushed_continuations
                .iter()
                .chain(self.intra_call_returns.iter())
                .filter_map(|t| self.idx(*t))
                .collect()
        } else if JCC.contains(&m) {
            let mut out: Vec<u32> = next.into_iter().collect();
            if let Some(target) = self.branch_target(i) {
                if let Some(index) = self.idx(target) {
                    out.push(index);
                }
            }
            out
        } else if m == "JMP" {
            if let Some(target) = self.branch_target(i) {
                self.idx(target).into_iter().collect()
            } else if self.push_ret_target(i).is_some() {
                self.push_ret_target(i)
                    .and_then(|t| self.idx(t))
                    .into_iter()
                    .collect()
            } else {
                self.index
                    .keys()
                    .copied()
                    .filter_map(|t| self.idx(t))
                    .collect()
            }
        } else {
            next.into_iter().collect()
        }
    }

    fn transfer(&self, index: usize, state: ReturnState) -> ReturnState {
        let ins = &self.decoded[index];
        let m = ins.mnemonic.as_str();
        if m == "CALL" {
            return ReturnState::unknown();
        }
        let Some(writes) = &self.register_writes[index] else {
            return ReturnState::unknown();
        };
        let mut saved = state.saved.clone();
        let ops = &ins.operands;
        for reg in writes {
            saved.remove(reg);
        }
        if m == "POP" && state.delta == Some(0) {
            if let Some(reg) = ops.first().and_then(gpr32) {
                if reg != R_ESP {
                    saved.insert(reg);
                }
            }
        }
        let adjustment = if matches!(m, "PUSH" | "POP") {
            operand_size(ops, Some(32))
                .map(|size| size as i32 / 8 * if m == "PUSH" { -1 } else { 1 })
        } else if matches!(m, "PUSHAD" | "PUSHFD" | "PUSHF" | "POPFD" | "POPF") {
            Some(match m {
                "PUSHAD" => -32,
                "PUSHFD" => -4,
                "PUSHF" => -2,
                "POPFD" => 4,
                _ => 2,
            })
        } else if m == "RET" {
            Some(4 + ops.first().and_then(imm).unwrap_or(0) as i32)
        } else if matches!(m, "ADD" | "SUB") && ops.first().and_then(gpr32) == Some(R_ESP) {
            ops.get(1).and_then(imm).map(|value| {
                let signed = value as u32 as i32;
                signed * if m == "SUB" { -1 } else { 1 }
            })
        } else {
            None
        };
        let new_delta = if m == "LEAVE" {
            state.frame.map(|frame| frame + 4)
        } else if m == "POP" && ops.first().and_then(gpr32) == Some(R_ESP) {
            None
        } else if let Some(adjustment) = adjustment {
            state.delta.map(|delta| delta + adjustment)
        } else if writes.contains(&R_ESP) {
            None
        } else {
            state.delta
        };
        let mut new_frame = if writes.contains(&R_EBP) {
            None
        } else {
            state.frame
        };
        if m == "MOV" && ops.len() == 2 && ops[0].display == "EBP" && ops[1].display == "ESP" {
            new_frame = state.delta;
        }
        ReturnState {
            delta: new_delta,
            frame: new_frame,
            saved,
        }
    }

    fn reached(&self, program: &Program, roots: &BTreeSet<Addr>) -> BTreeSet<u32> {
        if self.decoded.iter().enumerate().any(|(i, ins)| {
            ins.mnemonic == "JMP"
                && self.branch_target(i).is_none()
                && !program.tables.contains_key(&(self.addr, ins.addr))
                && !self.recovered_tables.contains_key(&ins.addr)
                && !popped_jump(self, i)
        }) {
            return (0..self.decoded.len() as u32).collect();
        }
        let mut work: Vec<u32> = roots
            .iter()
            .chain(self.pushed_continuations.iter())
            .filter_map(|addr| self.idx(*addr))
            .collect();
        let mut live = BTreeSet::new();
        while let Some(i) = work.pop() {
            if !live.insert(i) {
                continue;
            }
            let index = i as usize;
            let ins = &self.decoded[index];
            if never_returns(program, ins) {
                continue;
            }
            if popped_jump(self, index) {
                work.extend(
                    self.pushed_continuations
                        .iter()
                        .filter_map(|t| self.idx(*t)),
                );
                continue;
            }
            if !TERMINATORS.contains(&ins.mnemonic.as_str()) && !self.contiguous[index] {
                if let Some(target) = self.idx(self.fallthrough[index]) {
                    work.push(target);
                }
                continue;
            }
            work.extend(self.successors(program, index));
        }
        live
    }

    fn dead_after_noreturn(&self, program: &Program, entries: &[Addr]) -> BTreeSet<Addr> {
        if !self.decoded.iter().any(|ins| never_returns(program, ins)) {
            return BTreeSet::new();
        }
        let roots = std::iter::once(self.addr)
            .chain(entries.iter().copied())
            .collect();
        let live = self.reached(program, &roots);
        let mut dead = BTreeSet::new();
        let mut after_noreturn = false;
        for (i, ins) in self.decoded.iter().enumerate() {
            if live.contains(&(i as u32)) {
                after_noreturn = never_returns(program, ins);
            } else if after_noreturn {
                dead.insert(ins.addr);
            }
        }
        dead
    }

    pub(crate) fn index_of(&self, addr: Addr) -> usize {
        self.index.get(&addr).copied().unwrap_or(0) as usize
    }

    fn prepare(&mut self, program: &Program, relocated: &BTreeMap<Addr, Addr>) {
        self.index = self
            .decoded
            .iter()
            .enumerate()
            .map(|(i, ins)| (ins.addr, i as u32))
            .collect();
        self.fallthrough = self
            .decoded
            .iter()
            .map(|ins| ins.addr + ins.length as u32)
            .collect();
        for i in 0..self.decoded.len() {
            let next = self
                .decoded
                .get(i + 1)
                .map(|ins| ins.addr)
                .unwrap_or(self.end);
            self.contiguous[i] = self.fallthrough[i] == next;
        }
        self.recovered_tables = recover_tables(program, relocated, self);
        self.intra_calls = self
            .decoded
            .iter()
            .enumerate()
            .filter_map(|(i, ins)| {
                if ins.mnemonic != "CALL" {
                    return None;
                }
                let target = self.branch_target(i)?;
                (self.index.contains_key(&target) && !program.entries.contains_key(&target))
                    .then_some((i as u32, target))
            })
            .collect();
        self.intra_call_returns = self
            .intra_calls
            .keys()
            .filter_map(|&i| self.fallthrough.get(i as usize).copied())
            .collect();
        self.pushed_continuations = self.pushed_continuations();
        self.seh_sites = seh_frame_sites(self, &program.image);
        self.seh_restores = seh_restore_sites(self);
    }

    fn pushed_continuations(&self) -> BTreeSet<Addr> {
        let mut targets = BTreeSet::new();
        for ins in &self.decoded {
            if ins.mnemonic != "PUSH" {
                continue;
            }
            let Some(op) = ins.operands.first() else {
                continue;
            };
            let Some(target) = imm(op) else {
                continue;
            };
            let target = target as Addr;
            if operand_size(&ins.operands, Some(32)) == Some(32) && self.index.contains_key(&target)
            {
                targets.insert(target);
            }
        }
        targets
    }

    fn analyze(&mut self, program: &Program, entries: &[Addr], helpers: &BTreeSet<Addr>) {
        self.return_jumps = self.popped_return_jumps(entries);
        self.seh_escapes = self.seh_escaping_returns(program, helpers);
        self.dead_addrs = self.dead_after_noreturn(program, entries);
    }
}
pub(crate) fn build_raw(
    program: &Program,
    addr: Addr,
    lifter: &mut Lifter,
    decoder: &Decoder,
    relocated: &BTreeMap<Addr, Addr>,
) -> Result<RawFunction> {
    let function = program
        .functions
        .get(&addr)
        .ok_or_else(|| Diagnostic::error(addr, rk_core::Reason::EntryOutsideFunctions))?;
    let mut decoded = Vec::new();
    for span in &function.spans {
        let raw = program.image.bytes_at(span.start, span.size as usize)?;
        let lengths;
        let lengths: &[u8] = match &span.lengths {
            Some(lengths) => lengths,
            None => {
                lengths = lifter.lengths(span.start, raw)?;
                &lengths
            }
        };
        let mut at = span.start;
        for &length in lengths {
            let raw = program.image.bytes(at, length as usize).ok_or_else(|| {
                Diagnostic::error(at, rk_core::Reason::DecoderCannotTile { span: span.start })
            })?;
            let ins = decoder.decode_span(at, raw).ok_or_else(|| {
                Diagnostic::error(at, rk_core::Reason::DecoderCannotTile { span: span.start })
            })?;
            decoded.push(ins);
            at += length as u32;
        }
    }
    let starts: BTreeSet<Addr> = decoded.iter().map(|ins| ins.addr).collect();
    for entry in program.configured_entries.iter().copied() {
        if starts.contains(&entry) || program.containing(entry) != Some(addr) {
            continue;
        }
        let mut at = entry;
        let mut lengths = Vec::new();
        while !starts.contains(&at) {
            if program.containing(at) != Some(addr) || lengths.len() == 16 {
                return Err(Diagnostic::error(
                    addr,
                    rk_core::Reason::OverlappingEntryDoesNotRejoin { entry },
                ));
            }
            let bytes = program.image.bytes(at, 15).ok_or_else(|| {
                Diagnostic::error(at, rk_core::Reason::DecoderCannotTile { span: entry })
            })?;
            let length = lifter.length(at, bytes)?;
            lengths.push(length);
            at += length as u32;
            let instruction = program
                .image
                .bytes(at - length as u32, length as usize)
                .ok_or_else(|| {
                    Diagnostic::error(at, rk_core::Reason::DecoderCannotTile { span: entry })
                })?;
            if lifter.ends_flow(at - length as u32, instruction)? {
                break;
            }
        }
        let mut at = entry;
        for length in lengths {
            let raw = program.image.bytes(at, length as usize).ok_or_else(|| {
                Diagnostic::error(at, rk_core::Reason::DecoderCannotTile { span: entry })
            })?;
            let ins = decoder.decode_span(at, raw).ok_or_else(|| {
                Diagnostic::error(at, rk_core::Reason::DecoderCannotTile { span: entry })
            })?;
            decoded.push(ins);
            at += length as u32;
        }
    }
    decoded.sort_by_key(|ins| ins.addr);
    if !decoded.iter().any(|ins| ins.addr == addr) {
        return Err(Diagnostic::error(
            addr,
            rk_core::Reason::EntryNotInstructionBoundary,
        ));
    }
    let gprs: Vec<(u8, u64, u32)> = lifter
        .registers()
        .filter_map(|(name, offset, size)| {
            rk_decode::REG32
                .iter()
                .position(|&reg| reg == name)
                .map(|index| (index as u8, offset, size))
        })
        .collect();
    let mut register_writes = Vec::with_capacity(decoded.len());
    let mut insns = Vec::with_capacity(decoded.len());
    for ins in &decoded {
        let lifted = lifter.lift(ins.addr, &ins.raw[..ins.length as usize])?;
        register_writes.push(
            if lifted
                .ops
                .iter()
                .any(|op| matches!(op.opcode, OpCode::CallOther | OpCode::Unknown))
            {
                None
            } else {
                Some(
                    lifted
                        .ops
                        .iter()
                        .filter_map(|op| op.output)
                        .filter(|v| v.space == Space::Register)
                        .flat_map(|v| {
                            gprs.iter().filter_map(move |&(reg, offset, size)| {
                                (v.offset < offset + size as u64
                                    && offset < v.offset + v.size as u64)
                                    .then_some(reg)
                            })
                        })
                        .collect(),
                )
            },
        );
        insns.push(Insn {
            addr: ins.addr,
            length: lifted.length,
            address_size: ins.address_size,
            mnemonic: ins.mnemonic.clone(),
            operands: ins
                .operands
                .iter()
                .map(|o| o.display.clone())
                .collect::<Vec<_>>()
                .join(","),
            typed_operands: ins.operands.clone(),
            repeat: ins.rep,
            text: ins.text.clone(),
            ops: lifted.ops,
            x87: lifted.x87,
        });
    }
    let end = (addr + function.spans.iter().map(|s| s.size).sum::<u32>())
        .max(decoded.last().map(|ins| ins.addr + 1).unwrap_or(addr + 1));
    let contiguous = vec![true; decoded.len()];
    let mut raw = RawFunction {
        addr,
        decoded,
        insns,
        register_writes,
        index: BTreeMap::new(),
        contiguous,
        fallthrough: Vec::new(),
        end,
        pushed_continuations: BTreeSet::new(),
        intra_calls: BTreeMap::new(),
        intra_call_returns: BTreeSet::new(),
        seh_sites: BTreeMap::new(),
        seh_restores: BTreeSet::new(),
        return_jumps: BTreeSet::new(),
        seh_escapes: BTreeSet::new(),
        dead_addrs: BTreeSet::new(),
        recovered_tables: BTreeMap::new(),
    };
    raw.prepare(program, relocated);
    Ok(raw)
}

pub(crate) fn finalize(
    program: &Program,
    addr: Addr,
    mut raw: RawFunction,
    helpers: &BTreeSet<Addr>,
) -> Result<FunctionIr> {
    let mut entries = program
        .interior_by_function
        .get(&addr)
        .cloned()
        .unwrap_or_default();
    raw.analyze(program, &entries, helpers);
    let mut succ = Vec::with_capacity(raw.decoded.len());
    for i in 0..raw.decoded.len() {
        succ.push(raw.successors(program, i));
    }
    let mut noreturn = BTreeMap::new();
    for (i, ins) in raw.decoded.iter().enumerate() {
        if never_returns(program, ins) {
            noreturn.insert(i as u32, ins.addr + ins.length as u32);
            succ[i] = Vec::new();
        }
    }
    let exits = external_exits(program, &raw);
    let tables = table_jumps(&raw);
    let dynamic: BTreeSet<u32> = raw
        .decoded
        .iter()
        .enumerate()
        .filter(|(i, ins)| {
            ins.mnemonic == "JMP"
                && raw.branch_target(*i).is_none()
                && !program.tables.contains_key(&(raw.addr, ins.addr))
                && !raw.recovered_tables.contains_key(&ins.addr)
                && !raw.return_jumps.contains(&(*i as u32))
        })
        .map(|(i, _)| i as u32)
        .collect();
    let call_returns = raw
        .decoded
        .iter()
        .filter(|ins| ins.mnemonic == "CALL")
        .map(|ins| raw.fallthrough[raw.index[&ins.addr] as usize])
        .collect();
    let seh = raw.seh_effects(program, helpers)?;
    entries.extend(seh.iter().filter_map(|(index, kinds)| {
        let next = raw.fallthrough[*index as usize];
        (kinds
            .iter()
            .any(|kind| matches!(kind, SehEffect::Enter | SehEffect::Adopt))
            && raw.idx(next).is_some())
        .then_some(next)
    }));
    entries.sort_unstable();
    entries.dedup();
    let eh3 = msvc_eh3_finally_only(&program.image, addr, eh3_trylevels(&raw, program).as_ref())
        .then_some(addr + MSVC_EH3_FRAME_LEN - 7);
    let seh_unwind_only: BTreeSet<u32> = raw
        .seh_sites
        .iter()
        .filter_map(|(i, handler)| {
            (msvc_unwind_only(&program.image, Some(*handler))
                || raw.decoded[*i as usize].addr == eh3.unwrap_or(0))
            .then_some(*i)
        })
        .collect();
    let import_tails = raw
        .decoded
        .iter()
        .enumerate()
        .filter_map(|(i, _)| {
            raw.import_tail_slot(program, i)
                .map(|slot| (i as u32, slot))
        })
        .collect();
    let dead = raw.dead_addrs.iter().filter_map(|a| raw.idx(*a)).collect();
    Ok(FunctionIr {
        addr,
        insns: raw.insns,
        succ,
        exits,
        tables,
        dynamic,
        noreturn,
        entries,
        dead,
        call_returns,
        intra_calls: raw.intra_calls,
        intra_call_returns: raw.intra_call_returns,
        seh,
        seh_handlers: raw.seh_sites,
        seh_unwind_only,
        import_tails,
    })
}

fn external_exits(program: &Program, raw: &RawFunction) -> BTreeMap<u32, Vec<Addr>> {
    let mut exits = BTreeMap::new();
    for (i, ins) in raw.decoded.iter().enumerate() {
        let m = ins.mnemonic.as_str();
        if m == "INT3"
            || raw.push_ret_target(i).is_some()
            || never_returns(program, ins)
            || raw.dead_addrs.contains(&ins.addr)
        {
            continue;
        }
        let mut out = Vec::new();
        if m == "JMP" {
            if let Some(target) = raw.branch_target(i) {
                out.push(target);
            } else if !raw.return_jumps.contains(&(i as u32)) {
                let table = program
                    .tables
                    .get(&(raw.addr, ins.addr))
                    .or_else(|| raw.recovered_tables.get(&ins.addr));
                match table {
                    Some(targets) => out.extend(targets.iter().copied()),
                    None => {
                        exits.insert(i as u32, Vec::new());
                        continue;
                    }
                }
            }
        } else if JCC.contains(&m) {
            if let Some(target) = raw.branch_target(i) {
                out.push(target);
            }
        }
        if m != "RET" && m != "JMP" && !(i + 1 < raw.decoded.len() && raw.contiguous[i]) {
            out.push(raw.fallthrough[i]);
        }
        let out: BTreeSet<Addr> = out
            .into_iter()
            .filter(|target| !raw.index.contains_key(target))
            .collect();
        if !out.is_empty() {
            exits.insert(i as u32, out.into_iter().collect());
        }
    }
    exits
}

fn never_returns(program: &Program, ins: &DecodedInsn) -> bool {
    if ins.mnemonic != "CALL" {
        return false;
    }
    if program.noreturn_calls.contains(&ins.addr) {
        return true;
    }
    let Some(target) = branch_target(ins) else {
        return false;
    };
    program.is_noreturn(target)
}

fn popped_jump(raw: &RawFunction, i: usize) -> bool {
    let Some(ins) = raw.decoded.get(i) else {
        return false;
    };
    ins.mnemonic == "JMP"
        && ins.operands.first().and_then(gpr32).is_some()
        && i > 0
        && raw.contiguous[i - 1]
        && raw.decoded[i - 1].mnemonic == "POP"
        && raw.decoded[i - 1].operands.first().map(|o| &o.display)
            == ins.operands.first().map(|o| &o.display)
}

fn operand_size(ops: &[rk_decode::Operand], hint: Option<u16>) -> Option<u16> {
    ops.iter()
        .find_map(|o| match o.kind {
            OperandKind::Reg { .. } => o.size,
            _ => None,
        })
        .or_else(|| {
            ops.iter().find_map(|o| match o.kind {
                OperandKind::Mem { .. } => o.size,
                _ => None,
            })
        })
        .or(hint)
}
