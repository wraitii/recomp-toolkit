use std::collections::{BTreeMap, BTreeSet};

use rk_core::flow::{Effect, Flow, Value};
use rk_core::ir::{FunctionIr, Insn, OpCode, SehEffect, Space, Varnode};
use rk_core::program::Program;
use rk_core::{Addr, Diagnostic, Result};
use rk_ir::Registers;
use rk_sleigh::Lifter;

use crate::seh::{
    eh3_trylevels, msvc_eh3_finally_only, msvc_unwind_only, seh_frame_sites, seh_restore_sites,
    MSVC_EH3_FRAME_LEN,
};
use crate::tables::{recover_tables, table_jumps};
use crate::R_ESP;

#[derive(Clone)]
pub(crate) struct RawFunction {
    pub(crate) addr: Addr,
    pub(crate) insns: Vec<Insn>,
    pub(crate) flows: Vec<Flow>,
    effects: Vec<Effect>,
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

    fn flow(&self, i: usize) -> Flow {
        self.flows.get(i).copied().unwrap_or(Flow::Fall)
    }

    fn jump_register(&self, i: usize) -> Option<u8> {
        let Flow::Jump(rk_core::flow::Target::Indirect(target)) = self.flow(i) else {
            return None;
        };
        general_index(target)
    }

    fn pushed_constant(&self, i: usize) -> Option<Addr> {
        if self.flow(i) != Flow::Fall {
            return None;
        }
        self.effects.get(i)?.pushed_constant(stack_pointer())
    }

    pub(crate) fn push_ret_target(&self, i: usize) -> Option<Addr> {
        if i + 1 >= self.insns.len() || !self.contiguous[i] {
            return None;
        }
        if self.flow(i + 1) != Flow::Return
            || self.effects[i + 1].value(stack_pointer())
                != Some(Value::Stack(i64::from(stack_pointer().size)))
        {
            return None;
        }
        self.pushed_constant(i)
    }

    pub(crate) fn branch_target(&self, i: usize) -> Option<Addr> {
        self.flow(i).target()
    }

    pub(crate) fn direct_call(&self, i: usize) -> Option<Addr> {
        let flow = self.flow(i);
        flow.is_call().then(|| flow.target()).flatten()
    }

    fn import_tail_slot(&self, program: &Program, i: usize) -> Option<Addr> {
        let Flow::Jump(rk_core::flow::Target::Indirect(slot)) = self.flow(i) else {
            return None;
        };
        (slot.space == Space::Ram && slot.size == stack_pointer().size)
            .then_some(slot.offset)
            .filter(|slot| program.image.imports.contains_key(slot))
    }

    pub(crate) fn successors(&self, program: &Program, i: usize) -> Vec<u32> {
        let Some(ins) = self.insns.get(i) else {
            return Vec::new();
        };
        let flow = self.flow(i);
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
        if flow == Flow::Trap {
            return Vec::new();
        }
        if flow == Flow::Return {
            return self
                .pushed_continuations
                .iter()
                .chain(self.intra_call_returns.iter())
                .filter_map(|target| self.idx(*target))
                .collect();
        }
        if let Flow::Branch(target) = flow {
            let mut out: Vec<u32> = next.into_iter().collect();
            if let Some(index) = self.idx(target) {
                out.push(index);
            }
            return out;
        }
        if flow.is_jump() {
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
        if !self
            .effects
            .iter()
            .any(|effect| effect.writes.values().any(|v| matches!(v, Value::Slot(_))))
        {
            return BTreeSet::new();
        }
        let candidates: Vec<usize> = (0..self.insns.len())
            .filter(|&i| self.jump_register(i).is_some())
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
            if index >= self.insns.len() {
                continue;
            }
            let Some(state) = states.get(&i).cloned() else {
                continue;
            };
            if candidates.contains(&index)
                && self
                    .jump_register(index)
                    .is_some_and(|r| state.saved.contains(&r))
            {
                continue;
            }
            let out = self.transfer(index, state);
            if matches!(
                self.flow(index),
                Flow::Jump(rk_core::flow::Target::Indirect(_))
            ) {
                if !unknown_indirect_seen {
                    for j in 0..self.insns.len() as u32 {
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
                    self.jump_register(i)
                        .is_some_and(|r| state.saved.contains(&r))
                })
            })
            .map(|i| i as u32)
            .collect()
    }

    fn successors_during_proof(&self, i: usize) -> Vec<u32> {
        if i >= self.insns.len() {
            return Vec::new();
        }
        let flow = self.flow(i);
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
        } else if flow == Flow::Trap {
            Vec::new()
        } else if flow == Flow::Return {
            self.pushed_continuations
                .iter()
                .chain(self.intra_call_returns.iter())
                .filter_map(|t| self.idx(*t))
                .collect()
        } else if let Flow::Branch(target) = flow {
            next.into_iter().chain(self.idx(target)).collect()
        } else if flow.is_jump() {
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
        if self.flow(index).is_call() {
            return ReturnState::unknown();
        }
        let Some(writes) = &self.register_writes[index] else {
            return ReturnState::unknown();
        };
        let effect = &self.effects[index];
        let stack = stack_pointer();
        let resolve = |value: Option<Value>, unchanged: Option<i32>| match value {
            None => unchanged,
            Some(Value::Stack(at)) => state.delta.map(|delta| delta + at as i32),
            Some(Value::Frame(at)) => state.frame.map(|frame| frame + at as i32),
            Some(_) => None,
        };
        let new_delta = resolve(effect.value(stack), state.delta);
        let mut saved = state.saved.clone();
        for reg in writes {
            saved.remove(reg);
        }
        if let Some(Value::Stack(top)) = effect.value(stack) {
            for (&register, &value) in &effect.writes {
                if let (Value::Slot(at), Some(reg)) = (value, general_index(register)) {
                    if reg != R_ESP && state.delta.map(|d| d + at as i32) == Some(0) && top > at {
                        saved.insert(reg);
                    }
                }
            }
        }
        let new_frame = resolve(effect.value(frame_pointer()), state.frame);
        ReturnState {
            delta: new_delta,
            frame: new_frame,
            saved,
        }
    }

    fn reached(&self, program: &Program, roots: &BTreeSet<Addr>) -> BTreeSet<u32> {
        if self.insns.iter().enumerate().any(|(i, ins)| {
            matches!(self.flow(i), Flow::Jump(rk_core::flow::Target::Indirect(_)))
                && !program.tables.contains_key(&(self.addr, ins.addr))
                && !self.recovered_tables.contains_key(&ins.addr)
                && !popped_jump(self, i)
        }) {
            return (0..self.insns.len() as u32).collect();
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
            if never_returns(program, self, index) {
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
            if !self.flow(index).ends_block() && !self.contiguous[index] {
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
        if !(0..self.insns.len()).any(|i| never_returns(program, self, i)) {
            return BTreeSet::new();
        }
        let roots = std::iter::once(self.addr)
            .chain(entries.iter().copied())
            .collect();
        let live = self.reached(program, &roots);
        let mut dead = BTreeSet::new();
        let mut after_noreturn = false;
        for (i, ins) in self.insns.iter().enumerate() {
            if live.contains(&(i as u32)) {
                after_noreturn = never_returns(program, self, i);
            } else if after_noreturn {
                dead.insert(ins.addr);
            }
        }
        dead
    }

    fn prepare(&mut self, program: &Program, relocated: &BTreeMap<Addr, Addr>) {
        self.index = self
            .insns
            .iter()
            .enumerate()
            .map(|(i, ins)| (ins.addr, i as u32))
            .collect();
        self.fallthrough = self
            .insns
            .iter()
            .map(|ins| ins.addr + u64::from(ins.length))
            .collect();
        for i in 0..self.insns.len() {
            let next = self
                .insns
                .get(i + 1)
                .map(|ins| ins.addr)
                .unwrap_or(self.end);
            self.contiguous[i] = self.fallthrough[i] == next;
        }
        self.recovered_tables = recover_tables(program, relocated, self);
        self.intra_calls = (0..self.insns.len())
            .filter_map(|i| {
                let target = self.direct_call(i)?;
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
        (0..self.insns.len())
            .filter_map(|i| self.pushed_constant(i))
            .filter(|target| self.index.contains_key(target))
            .collect()
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
    relocated: &BTreeMap<Addr, Addr>,
) -> Result<RawFunction> {
    let function = program
        .functions
        .get(&addr)
        .ok_or_else(|| Diagnostic::error(addr, rk_core::Reason::EntryOutsideFunctions))?;
    let machine = rk_core::isa::machine();
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
            let ins = machine.decoder.decode(at, raw).ok_or_else(|| {
                Diagnostic::error(at, rk_core::Reason::DecoderCannotTile { span: span.start })
            })?;
            decoded.push((at, raw, ins));
            at += u64::from(length);
        }
    }
    let starts: BTreeSet<Addr> = decoded.iter().map(|&(at, _, _)| at).collect();
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
            let bytes = program
                .image
                .bytes(at, usize::from(machine.max_length))
                .ok_or_else(|| {
                    Diagnostic::error(at, rk_core::Reason::DecoderCannotTile { span: entry })
                })?;
            let length = lifter.length(at, bytes)?;
            lengths.push(length);
            at += u64::from(length);
            let instruction = program
                .image
                .bytes(at.wrapping_sub(u64::from(length)), length as usize)
                .ok_or_else(|| {
                    Diagnostic::error(at, rk_core::Reason::DecoderCannotTile { span: entry })
                })?;
            if lifter.ends_flow(at.wrapping_sub(u64::from(length)), instruction)? {
                break;
            }
        }
        let mut at = entry;
        for length in lengths {
            let raw = program.image.bytes(at, length as usize).ok_or_else(|| {
                Diagnostic::error(at, rk_core::Reason::DecoderCannotTile { span: entry })
            })?;
            let ins = machine.decoder.decode(at, raw).ok_or_else(|| {
                Diagnostic::error(at, rk_core::Reason::DecoderCannotTile { span: entry })
            })?;
            decoded.push((at, raw, ins));
            at += u64::from(length);
        }
    }
    decoded.sort_by_key(|&(at, _, _)| at);
    if !decoded.iter().any(|&(at, _, _)| at == addr) {
        return Err(Diagnostic::error(
            addr,
            rk_core::Reason::EntryNotInstructionBoundary,
        ));
    }
    let gprs: Vec<(u8, u64, u32)> = Registers::get()
        .arch()
        .general()
        .iter()
        .enumerate()
        .map(|(index, v)| (index as u8, v.offset, v.size))
        .collect();
    let mut register_writes = Vec::with_capacity(decoded.len());
    let mut insns = Vec::with_capacity(decoded.len());
    for (at, bytes, ins) in decoded {
        let lifted = lifter.lift(at, bytes)?;
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
        insns.push(Insn::new(at, bytes, ins, lifted.ops, lifted.x87));
    }
    let flows = insns
        .iter()
        .map(|ins| Flow::of(&ins.ops, ins.addr, ins.end()))
        .collect();
    let effects = insns
        .iter()
        .map(|ins| Effect::of(&ins.ops, stack_pointer(), frame_pointer()))
        .collect();
    let end = (addr + u64::from(function.spans.iter().map(|s| s.size).sum::<u32>()))
        .max(insns.last().map(|ins| ins.addr + 1).unwrap_or(addr + 1));
    let contiguous = vec![true; insns.len()];
    let mut raw = RawFunction {
        addr,
        insns,
        flows,
        effects,
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
    let mut succ = Vec::with_capacity(raw.insns.len());
    for i in 0..raw.insns.len() {
        succ.push(raw.successors(program, i));
    }
    let mut noreturn = BTreeMap::new();
    for (i, ins) in raw.insns.iter().enumerate() {
        if never_returns(program, &raw, i) {
            noreturn.insert(i as u32, ins.addr + u64::from(ins.length));
            succ[i] = Vec::new();
        }
    }
    let exits = external_exits(program, &raw);
    let tables = table_jumps(&raw);
    let dynamic: BTreeSet<u32> = raw
        .insns
        .iter()
        .enumerate()
        .filter(|(i, ins)| {
            matches!(raw.flow(*i), Flow::Jump(rk_core::flow::Target::Indirect(_)))
                && !program.tables.contains_key(&(raw.addr, ins.addr))
                && !raw.recovered_tables.contains_key(&ins.addr)
                && !raw.return_jumps.contains(&(*i as u32))
        })
        .map(|(i, _)| i as u32)
        .collect();
    let call_returns = (0..raw.insns.len())
        .filter(|&i| raw.flow(i).is_call())
        .map(|i| raw.fallthrough[i])
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
        .then_some(addr + u64::from(MSVC_EH3_FRAME_LEN) - 7);
    let seh_unwind_only: BTreeSet<u32> = raw
        .seh_sites
        .iter()
        .filter_map(|(i, handler)| {
            (msvc_unwind_only(&program.image, Some(*handler))
                || raw.insns[*i as usize].addr == eh3.unwrap_or(0))
            .then_some(*i)
        })
        .collect();
    let import_tails = raw
        .insns
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
    for (i, ins) in raw.insns.iter().enumerate() {
        let flow = raw.flow(i);
        if flow == Flow::Trap
            || raw.push_ret_target(i).is_some()
            || never_returns(program, raw, i)
            || raw.dead_addrs.contains(&ins.addr)
        {
            continue;
        }
        let mut out = Vec::new();
        if flow.is_jump() {
            if let Some(target) = flow.target() {
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
        } else if let Flow::Branch(target) = flow {
            out.push(target);
        }
        if !flow.ends_block() && !(i + 1 < raw.insns.len() && raw.contiguous[i]) {
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

fn never_returns(program: &Program, raw: &RawFunction, i: usize) -> bool {
    if !raw.flow(i).is_call() {
        return false;
    }
    if raw
        .insns
        .get(i)
        .is_some_and(|ins| program.noreturn_calls.contains(&ins.addr))
    {
        return true;
    }
    raw.direct_call(i)
        .is_some_and(|target| program.is_noreturn(target))
}

fn popped_jump(raw: &RawFunction, i: usize) -> bool {
    let Some(register) = raw.jump_register(i) else {
        return false;
    };
    i > 0
        && raw.contiguous[i - 1]
        && raw.effects[i - 1].writes.iter().any(|(&written, &value)| {
            matches!(value, Value::Slot(0)) && general_index(written) == Some(register)
        })
        && matches!(raw.effects[i - 1].value(stack_pointer()), Some(Value::Stack(top)) if top > 0)
}

fn general_index(register: Varnode) -> Option<u8> {
    Registers::get()
        .arch()
        .general()
        .iter()
        .position(|&general| general == register)
        .map(|index| index as u8)
}

fn stack_pointer() -> Varnode {
    Registers::get().arch().stack_pointer
}

fn frame_pointer() -> Varnode {
    Registers::get().arch().frame_pointer
}
