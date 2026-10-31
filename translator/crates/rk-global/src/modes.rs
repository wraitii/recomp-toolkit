use std::collections::{BTreeMap, BTreeSet};

use rk_core::ir::{FunctionIr, OpCode, Space, Varnode};
use rk_core::settings::{CallEffect as ConfiguredEffect, X87Effect};
use rk_core::{Addr, Lookup};
use rk_frontend::Analysis;

use crate::callgraph::CallGraph;
use crate::cfg::{closed, is_return, mnemonic};

pub const CW_MASK: u16 = 0xffff;
pub const CW_PC_RC: u16 = 0x0f00;
pub const CW_EXCEPTION_MASK: u16 = 0x003f;
pub const CW_RESET: u16 = 0x037f;

const CW_NEUTRAL: &[&str] = &[
    "FADD", "FADDP", "FIADD", "FSUB", "FSUBP", "FISUB", "FSUBR", "FSUBRP", "FISUBR", "FMUL",
    "FMULP", "FIMUL", "FDIV", "FDIVP", "FIDIV", "FDIVR", "FDIVRP", "FIDIVR", "FABS", "FCHS",
    "FSQRT", "FRNDINT", "FSIN", "FCOS", "F2XM1", "FSCALE", "FPATAN", "FYL2X", "FYL2XP1", "FPTAN",
    "FSINCOS", "FPREM", "FPREM1", "FXCH", "FFREE", "FDECSTP", "FINCSTP", "FNOP", "FLD", "FILD",
    "FST", "FSTP", "FIST", "FISTP", "FLD1", "FLDZ", "FLDPI", "FLDLN2", "FLDL2E", "FLDLG2",
    "FLDL2T", "FCOM", "FCOMP", "FCOMPP", "FUCOM", "FUCOMP", "FUCOMPP", "FICOM", "FICOMP", "FCOMI",
    "FCOMIP", "FUCOMI", "FUCOMIP", "FTST", "FXAM", "FNCLEX", "FCLEX", "FNSTSW", "FSTSW", "FNSTCW",
    "FSTCW", "WAIT", "FEMMS",
];

pub use rk_x87::ModeCondition;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReturnSummary {
    pub preserved: u16,
    pub known: u16,
    pub value: u16,
}

#[derive(Clone, Debug, Default)]
pub struct X87ModeFacts {
    pub entries: BTreeMap<Addr, ModeCondition>,
    pub returns: BTreeMap<Addr, ReturnSummary>,
    pub sites: BTreeMap<Addr, ModeCondition>,
    pub known_modes: BTreeSet<Addr>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Fact {
    known_mask: u16,
    known_value: u16,
    entry_mask: u16,
}

const ENTRY: Fact = Fact {
    known_mask: 0,
    known_value: 0,
    entry_mask: CW_MASK,
};
const UNKNOWN: Fact = Fact {
    known_mask: 0,
    known_value: 0,
    entry_mask: 0,
};
const PRESERVE: ReturnSummary = ReturnSummary {
    preserved: CW_MASK,
    known: 0,
    value: 0,
};
const UNKNOWN_SUMMARY: ReturnSummary = ReturnSummary {
    preserved: 0,
    known: 0,
    value: 0,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Effect {
    Keep,
    Set(u16, u16),
    Restore(u32),
    Unknown,
}

#[derive(Clone, Debug)]
struct ModeCall {
    index: u32,
    targets: Vec<Addr>,
}

#[derive(Clone, Debug)]
struct ModePrepared {
    sites: Vec<Addr>,
    roots: Vec<u32>,
    order: Vec<u32>,
    succ: Vec<Vec<u32>>,
    preds: Vec<Vec<u32>>,
    effects: Vec<Effect>,
    calls: Vec<ModeCall>,
    returns: Vec<u32>,
    x87_count: u32,
    jumps_used: usize,
}

pub(crate) struct ModeAnalysis {
    pub facts: X87ModeFacts,
    pub leaves: BTreeSet<Addr>,
    pub prepared: BTreeSet<Addr>,
    pub calls: BTreeSet<Addr>,
    pub sccs: usize,
    pub jump_bounds_consumed: usize,
}

pub(crate) fn analyze(
    analysis: &Analysis,
    indirect: &BTreeMap<Addr, Vec<Addr>>,
    indirect_jumps: &BTreeMap<Addr, Vec<Addr>>,
    effects: &BTreeMap<Addr, ConfiguredEffect>,
    noreturn: &BTreeSet<Addr>,
    opaque: &BTreeSet<Addr>,
) -> ModeAnalysis {
    let prepared: Vec<(Addr, ModePrepared)> = analysis
        .ir
        .iter()
        .filter_map(|(&addr, result)| {
            let fir = result.as_ref().ok()?;
            if opaque.contains(&addr) {
                return None;
            }
            prepare(fir, indirect, indirect_jumps).map(|p| (addr, p))
        })
        .collect();
    let jump_bounds_consumed = prepared.iter().map(|(_, body)| body.jumps_used).sum();
    let prepared: BTreeMap<Addr, ModePrepared> = prepared.into_iter().collect();
    let graph = CallGraph::build(analysis, indirect, indirect_jumps, |addr| {
        prepared.contains_key(&addr)
    });
    let mut summaries: BTreeMap<Addr, ReturnSummary> = BTreeMap::new();
    let mut site_facts: BTreeMap<Addr, BTreeMap<u32, Fact>> = BTreeMap::new();
    for &addr in noreturn {
        if !prepared.contains_key(&addr) {
            summaries.insert(addr, PRESERVE);
        }
    }
    for component in &graph.components {
        let recursive = component.len() > 1
            || component
                .iter()
                .any(|&n| graph.succ[n as usize].contains(&n));
        let recursive_targets: BTreeSet<Addr> = component
            .iter()
            .map(|&node| graph.nodes[node as usize])
            .collect();
        let mut entries = Vec::new();
        for &node in component {
            let addr = graph.nodes[node as usize];
            let prep = &prepared[&addr];
            for &root in &prep.roots {
                entries.push((addr, root, prep.sites[root as usize]));
            }
        }
        entries.sort_by_key(|&(addr, root, _)| (addr, root));
        if recursive {
            for &(_, _, entry_addr) in &entries {
                summaries.insert(entry_addr, PRESERVE);
            }
        }
        for &(addr, root, entry_addr) in &entries {
            let prep = &prepared[&addr];
            let (summary, facts) = run(
                prep,
                &summaries,
                effects,
                root,
                recursive.then_some(&recursive_targets),
            );
            summaries.insert(entry_addr, summary);
            let row = site_facts.entry(addr).or_default();
            for (index, fact) in facts {
                let fact = row
                    .get(&index)
                    .copied()
                    .map_or(fact, |previous| join([previous, fact].into_iter()));
                row.insert(index, fact);
            }
        }
    }
    let leaves: BTreeSet<Addr> = prepared
        .iter()
        .filter_map(|(&addr, prep)| mode_leaf(prep).then_some(addr))
        .collect();
    let sites = site_facts
        .iter()
        .flat_map(|(&addr, row)| {
            let prep = &prepared[&addr];
            row.iter().filter_map(move |(&index, fact)| {
                let mask = fact.known_mask & CW_PC_RC;
                (mask != 0).then_some((
                    prep.sites[index as usize],
                    ModeCondition {
                        mask,
                        value: fact.known_value & mask,
                    },
                ))
            })
        })
        .collect();
    let facts = X87ModeFacts {
        entries: summaries
            .iter()
            .filter_map(|(&addr, summary)| {
                (summary.known != 0).then_some((
                    addr,
                    ModeCondition {
                        mask: summary.known,
                        value: summary.value,
                    },
                ))
            })
            .collect(),
        returns: summaries,
        sites,
        known_modes: leaves.clone(),
    };
    let sccs = graph.components.len();
    let prepared_addresses: BTreeSet<Addr> = prepared.keys().copied().collect();
    let calls = prepared
        .iter()
        .filter(|(_, body)| !body.calls.is_empty())
        .map(|(&addr, _)| addr)
        .collect();
    ModeAnalysis {
        facts,
        leaves,
        prepared: prepared_addresses,
        calls,
        sccs,
        jump_bounds_consumed,
    }
}

fn defs_of(insn: &rk_core::ir::Insn) -> BTreeMap<Varnode, (OpCode, Vec<Varnode>)> {
    let mut defs = BTreeMap::new();
    for op in &insn.ops {
        if let Some(output) = op.output {
            defs.insert(output, (op.opcode, op.inputs.clone()));
        }
    }
    defs
}

#[derive(Clone, PartialEq, Eq)]
enum AddressKey {
    Leaf(Varnode),
    Op(OpCode, Vec<AddressKey>),
}

fn key_of(
    varnode: Varnode,
    defs: &BTreeMap<Varnode, (OpCode, Vec<Varnode>)>,
    depth: u32,
) -> AddressKey {
    match defs.get(&varnode).filter(|_| depth > 0) {
        Some((opcode, inputs)) => AddressKey::Op(
            *opcode,
            inputs
                .iter()
                .map(|input| key_of(*input, defs, depth - 1))
                .collect(),
        ),
        None => AddressKey::Leaf(varnode),
    }
}

fn key_registers(
    varnode: Varnode,
    defs: &BTreeMap<Varnode, (OpCode, Vec<Varnode>)>,
    regs: &mut BTreeSet<Varnode>,
    depth: u32,
) {
    if varnode.space == Space::Register {
        regs.insert(varnode);
    }
    if depth == 0 {
        return;
    }
    if let Some((_, inputs)) = defs.get(&varnode) {
        for input in inputs {
            key_registers(*input, defs, regs, depth - 1);
        }
    }
}

fn control_word() -> Varnode {
    rk_ir::Registers::get().arch().x86.fpu_control
}

fn traces_to(
    varnode: Varnode,
    target: Varnode,
    defs: &BTreeMap<Varnode, (OpCode, Vec<Varnode>)>,
) -> bool {
    let mut value = varnode;
    for _ in 0..8 {
        if value == target {
            return true;
        }
        match defs.get(&value) {
            Some((OpCode::Copy, inputs)) if inputs.len() == 1 => value = inputs[0],
            _ => return false,
        }
    }
    false
}

type StoreKey = (AddressKey, BTreeSet<Varnode>, bool);
type LoadKey = (AddressKey, BTreeSet<Varnode>);

fn store_keys(insn: &rk_core::ir::Insn) -> Vec<StoreKey> {
    if !insn.ops.iter().any(|op| op.opcode == OpCode::Store) {
        return Vec::new();
    }
    let defs = defs_of(insn);
    let mut stores = Vec::new();
    for op in &insn.ops {
        if op.opcode == OpCode::Store && op.inputs.len() >= 2 {
            let address = op.inputs[0];
            let value = op.inputs[1];
            let mut regs = BTreeSet::new();
            key_registers(address, &defs, &mut regs, 8);
            stores.push((
                key_of(address, &defs, 8),
                regs,
                traces_to(value, control_word(), &defs),
            ));
        }
    }
    stores
}

fn load_keys(insn: &rk_core::ir::Insn) -> Vec<LoadKey> {
    if !insn.ops.iter().any(|op| op.opcode == OpCode::Load) {
        return Vec::new();
    }
    let defs = defs_of(insn);
    let mut loads = Vec::new();
    for op in &insn.ops {
        if op.opcode != OpCode::Load {
            continue;
        }
        let Some(loaded) = op.output else { continue };
        let address = op
            .inputs
            .first()
            .copied()
            .unwrap_or(Varnode::constant(0, 0));
        let cw = control_word();
        if !insn.ops.iter().any(|candidate| {
            candidate.output == Some(cw)
                && traces_to(
                    candidate.inputs.first().copied().unwrap_or(loaded),
                    loaded,
                    &defs,
                )
        }) {
            continue;
        }
        let mut regs = BTreeSet::new();
        key_registers(address, &defs, &mut regs, 8);
        loads.push((key_of(address, &defs, 8), regs));
    }
    loads
}

fn saved_restores(
    fir: &FunctionIr,
    order: &[u32],
    preds: &[Vec<u32>],
    roots: &[u32],
) -> BTreeMap<u32, u32> {
    let mut stores: Vec<Option<Vec<StoreKey>>> = vec![None; fir.insns.len()];
    let mut restores = BTreeMap::new();
    for &index in order {
        if roots.contains(&index) {
            continue;
        }
        for (key, key_regs) in load_keys(&fir.insns[index as usize]) {
            let mut cur = index;
            let mut seen = BTreeSet::new();
            let mut found = None;
            while seen.insert(cur) {
                let previous = &preds[cur as usize];
                if previous.len() != 1 {
                    break;
                }
                let prev = previous[0];
                let some_stores = stores[prev as usize]
                    .get_or_insert_with(|| store_keys(&fir.insns[prev as usize]));
                let writes_key = fir.insns[prev as usize].ops.iter().any(|op| {
                    op.output.is_some_and(|output| {
                        output.space == Space::Register && key_regs.contains(&output)
                    })
                });
                if writes_key {
                    break;
                }
                if let Some((_, _, true)) = some_stores
                    .iter()
                    .find(|(candidate, _, _)| *candidate == key)
                {
                    found = Some(prev);
                    break;
                }
                if some_stores
                    .iter()
                    .any(|(candidate, _, _)| *candidate == key)
                {
                    break;
                }
                if fir.insns[prev as usize].ops.iter().any(|op| {
                    matches!(
                        op.opcode,
                        OpCode::Call | OpCode::CallInd | OpCode::CallOther
                    )
                }) || roots.contains(&prev)
                {
                    break;
                }
                cur = prev;
            }
            if let Some(found) = found {
                restores.insert(index, found);
            }
        }
    }
    restores
}

fn prepare(
    fir: &FunctionIr,
    indirect: &BTreeMap<Addr, Vec<Addr>>,
    indirect_jumps: &BTreeMap<Addr, Vec<Addr>>,
) -> Option<ModePrepared> {
    let refined = crate::cfg::bounded_tails(fir, indirect_jumps);
    let fir = refined.as_ref();
    if fir.insns.is_empty() {
        return None;
    }
    let mut tails = BTreeSet::new();
    let mut rows: BTreeMap<u32, ModeCall> = BTreeMap::new();
    let mut jumps_used = 0;
    for (&index, targets) in &fir.exits {
        let insn = &fir.insns[index as usize];
        let flow = insn.flow();
        let indirect_jump = flow.is_jump() && flow.indirect().is_some();
        if !flow.is_jump() || !fir.succ[index as usize].is_empty() {
            continue;
        }
        if indirect_jump {
            tails.insert(index);
            let targets = indirect_jumps.get(&insn.addr).cloned().unwrap_or_default();
            if !targets.is_empty() {
                jumps_used += 1;
            }
            rows.insert(index, ModeCall { index, targets });
        } else if targets.len() == 1 {
            tails.insert(index);
            rows.insert(
                index,
                ModeCall {
                    index,
                    targets: vec![targets[0]],
                },
            );
        }
    }
    if fir.exits.keys().any(|index| !tails.contains(index)) {
        return None;
    }
    if fir.seh.iter().any(|(index, kinds)| {
        kinds.iter().any(|kind| {
            !matches!(
                kind,
                rk_core::ir::SehEffect::Enter | rk_core::ir::SehEffect::Leave
            )
        }) || !fir.seh_unwind_only.contains(index)
    }) {
        return None;
    }
    if !closed(fir, &tails) {
        return None;
    }
    let entry = fir.index_of(fir.addr)?;
    let mut roots = vec![entry];
    for &addr in &fir.entries {
        if let Some(index) = fir.index_of(addr) {
            roots.push(index);
        }
    }
    roots.sort_unstable();
    roots.dedup();
    let successors = fir.succ.clone();
    let mut seen = vec![false; fir.insns.len()];
    let mut pending = roots.clone();
    while let Some(index) = pending.pop() {
        if seen[index as usize] {
            continue;
        }
        seen[index as usize] = true;
        pending.extend(successors[index as usize].iter().copied());
    }
    let order: Vec<u32> = (0..fir.insns.len() as u32)
        .filter(|&index| seen[index as usize])
        .collect();
    let mut preds = vec![Vec::new(); fir.insns.len()];
    for &index in &order {
        for &target in &successors[index as usize] {
            preds[target as usize].push(index);
        }
    }
    let restores = saved_restores(fir, &order, &preds, &roots);
    let mut effects = vec![Effect::Keep; fir.insns.len()];
    let mut returns = Vec::new();
    let mut x87_count = 0;
    for &index in &order {
        let insn = &fir.insns[index as usize];
        let mnemonic = mnemonic(insn);
        let effect = match mnemonic.as_ref() {
            "FINIT" | "FNINIT" | "FSAVE" | "FNSAVE" => Effect::Set(CW_MASK, CW_RESET),
            "FNSTENV" | "FSTENV" => Effect::Set(CW_EXCEPTION_MASK, CW_EXCEPTION_MASK),
            "FLDCW" => restores
                .get(&index)
                .map_or(Effect::Unknown, |&save| Effect::Restore(save)),
            "FLDENV" | "FRSTOR" | "FXRSTOR" | "XRSTOR" | "XRSTORS" => Effect::Unknown,
            name if name.starts_with('F') && !CW_NEUTRAL.contains(&name) => Effect::Unknown,
            _ => Effect::Keep,
        };
        effects[index as usize] = effect;
        if mnemonic.starts_with('F') && mnemonic != "FEMMS" {
            x87_count += 1;
        }
        let is_call = tails.contains(&index)
            || insn
                .ops
                .iter()
                .any(|op| matches!(op.opcode, OpCode::Call | OpCode::CallInd));
        if is_call {
            let targets = if let Some(row) = rows.get(&index) {
                row.targets.clone()
            } else {
                direct_targets(insn, indirect)
            };
            rows.entry(index).or_insert(ModeCall { index, targets });
        }
        if tails.contains(&index) || is_return(insn) {
            returns.push(index);
        }
    }
    let mut calls: Vec<ModeCall> = rows.into_values().collect();
    calls.sort_by_key(|call| call.index);
    Some(ModePrepared {
        sites: fir.insns.iter().map(|insn| insn.addr).collect(),
        roots,
        order,
        succ: successors,
        preds,
        effects,
        calls,
        returns,
        x87_count,
        jumps_used,
    })
}

fn direct_targets(insn: &rk_core::ir::Insn, indirect: &BTreeMap<Addr, Vec<Addr>>) -> Vec<Addr> {
    for op in &insn.ops {
        if op.opcode == OpCode::Call {
            if let Some(v) = op.inputs.first() {
                if matches!(v.space, Space::Const | Space::Ram) {
                    return vec![v.offset as Addr];
                }
            }
        }
        if op.opcode == OpCode::CallInd {
            return indirect.get(&insn.addr).cloned().unwrap_or_default();
        }
    }
    Vec::new()
}

fn run(
    prep: &ModePrepared,
    summaries: &BTreeMap<Addr, ReturnSummary>,
    effects: &BTreeMap<Addr, ConfiguredEffect>,
    root: u32,
    recursive: Option<&BTreeSet<Addr>>,
) -> (ReturnSummary, BTreeMap<u32, Fact>) {
    let call_of: Lookup<u32, &ModeCall> =
        prep.calls.iter().map(|call| (call.index, call)).collect();
    let mut incoming: BTreeMap<u32, Fact> = BTreeMap::new();
    let mut outgoing: BTreeMap<u32, Fact> = BTreeMap::new();
    let mut work = vec![root];
    let mut queued: BTreeSet<u32> = work.iter().copied().collect();
    while let Some(index) = work.pop() {
        queued.remove(&index);
        let mut facts = Vec::new();
        for &pred in &prep.preds[index as usize] {
            if let Some(&fact) = outgoing.get(&pred) {
                facts.push(fact);
            }
        }
        if index == root {
            facts.push(ENTRY);
        } else if prep.roots.contains(&index) {
            facts.push(UNKNOWN);
        }
        let mut state = join(facts.iter().copied());
        if incoming.get(&index) == Some(&state) && outgoing.contains_key(&index) {
            continue;
        }
        incoming.insert(index, state);
        if let Some(call) = call_of.get(&index) {
            let summary = call_summary(prep, call, summaries, effects, recursive);
            state = apply_summary(state, summary);
        }
        match prep.effects[index as usize] {
            Effect::Keep => {}
            Effect::Set(mask, value) => state = overwrite(state, mask, value),
            Effect::Restore(save) => state = outgoing.get(&save).copied().unwrap_or(UNKNOWN),
            Effect::Unknown => state = UNKNOWN,
        }
        if outgoing.get(&index) == Some(&state) {
            continue;
        }
        outgoing.insert(index, state);
        for &target in &prep.succ[index as usize] {
            if queued.insert(target) {
                work.push(target);
            }
        }
    }
    let facts: Vec<Fact> = prep
        .returns
        .iter()
        .filter_map(|index| outgoing.get(index).copied())
        .collect();
    let summary = if facts.is_empty() {
        PRESERVE
    } else {
        summary(join(facts.into_iter()))
    };
    (summary, incoming)
}

fn call_summary(
    prep: &ModePrepared,
    call: &ModeCall,
    summaries: &BTreeMap<Addr, ReturnSummary>,
    effects: &BTreeMap<Addr, ConfiguredEffect>,
    recursive: Option<&BTreeSet<Addr>>,
) -> Option<ReturnSummary> {
    if let Some(effect) = effects.get(&prep.sites[call.index as usize]) {
        if let Some(x87) = effect.x87 {
            return match x87 {
                X87Effect::Named(rk_core::settings::X87Named::Preserve) => Some(PRESERVE),
                X87Effect::Named(rk_core::settings::X87Named::Unknown) => None,
                X87Effect::Masked(mask) => Some(ReturnSummary {
                    preserved: mask.preserved,
                    known: mask.known,
                    value: mask.value,
                }),
            };
        }
    }
    if call.targets.is_empty() {
        return None;
    }
    let mut found = Vec::new();
    for &target in &call.targets {
        if recursive.is_some_and(|set| set.contains(&target)) {
            return None;
        }
        found.push(*summaries.get(&target)?);
    }
    Some(join_summaries(&found))
}

fn join(facts: impl Iterator<Item = Fact>) -> Fact {
    let mut iter = facts;
    let Some(first) = iter.next() else {
        return UNKNOWN;
    };
    let mut known = first.known_mask;
    let value = first.known_value;
    let mut entry = first.entry_mask;
    for fact in iter {
        let equal = !(value ^ fact.known_value) & CW_MASK;
        known &= fact.known_mask & equal;
        entry &= fact.entry_mask;
    }
    Fact {
        known_mask: known,
        known_value: value & known,
        entry_mask: entry & !known & CW_MASK,
    }
}

fn join_summaries(summaries: &[ReturnSummary]) -> ReturnSummary {
    let Some(first) = summaries.first() else {
        return UNKNOWN_SUMMARY;
    };
    let mut preserved = CW_MASK;
    let mut known = CW_MASK;
    for summary in summaries {
        preserved &= summary.preserved;
        known &= summary.known & !(first.value ^ summary.value);
    }
    ReturnSummary {
        preserved: preserved & !known,
        known,
        value: first.value & known,
    }
}

fn overwrite(fact: Fact, mask: u16, value: u16) -> Fact {
    let mask = mask & CW_MASK;
    let known = (fact.known_mask & !mask) | mask;
    let known_value = (fact.known_value & !mask) | (value & mask);
    Fact {
        known_mask: known & CW_MASK,
        known_value: known_value & known & CW_MASK,
        entry_mask: fact.entry_mask & !mask & CW_MASK,
    }
}

fn apply_summary(fact: Fact, summary: Option<ReturnSummary>) -> Fact {
    let Some(summary) = summary else {
        return UNKNOWN;
    };
    let preserved = summary.preserved;
    let known = (fact.known_mask & preserved) | summary.known;
    let value = (fact.known_value & preserved) | summary.value;
    Fact {
        known_mask: known & CW_MASK,
        known_value: value & known & CW_MASK,
        entry_mask: fact.entry_mask & preserved & !known & CW_MASK,
    }
}

fn summary(fact: Fact) -> ReturnSummary {
    ReturnSummary {
        preserved: fact.entry_mask,
        known: fact.known_mask,
        value: fact.known_value,
    }
}

fn mode_leaf(prep: &ModePrepared) -> bool {
    prep.x87_count > 0
        && !prep.returns.is_empty()
        && prep.calls.is_empty()
        && prep.order.iter().all(|&index| {
            matches!(
                prep.effects[index as usize],
                Effect::Keep | Effect::Restore(_)
            )
        })
}
