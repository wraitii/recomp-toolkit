//! Per-function planning between SSA and C. For each boundary it decides which guest state lanes to
//! publish (skipping those already resident), which values stay live and where lazy flag
//! descriptors settle. It also chooses what guarded calls republish and their C arguments,
//! then attaches the x87 plan. `PlannedFunction` holds
//! every decision that `rk-emit` prints.

use std::collections::{BTreeMap, BTreeSet};

use rk_core::contract::{DirectArguments, Field, Fields};
use rk_core::ir::Policy;
use rk_core::{Addr, Diagnostic, LookupSet, Result};
use rk_ir::{CgFunction, StateKey};
use rk_ssa::{
    live_values, prepare_with_publications, publications, CallFacts, FlagPublication,
    PrepareOptions, Ssa, ValueId,
};

mod arguments;
pub use arguments::select_arguments;

pub struct PlanOptions<'a> {
    pub call_facts: &'a dyn CallFacts,
    pub policy: Policy,
    pub x87_modes: &'a rk_x87::ModeInput,
    pub x87_options: rk_x87::Options,
    pub fault_state: rk_core::settings::FaultState,
    pub direct_targets: &'a BTreeSet<Addr>,
    pub direct_arguments: &'a BTreeMap<Addr, DirectArguments>,
    pub descriptors: bool,
    pub coalesce_roots: bool,
    pub assume_private_stack: bool,
    pub lazy_stack_writes: bool,
    pub assume_stack_pointer_bases: bool,
    pub call_hooks: bool,
}

pub struct PlannedFunction {
    pub ssa: Ssa,
    base: BTreeMap<ValueId, Vec<StateKey>>,
    pub publish: BTreeMap<ValueId, Vec<StateKey>>,
    pub guard: BTreeMap<ValueId, Vec<StateKey>>,
    pub live: LookupSet<ValueId>,
    pub return_addr: BTreeMap<ValueId, Addr>,
    pub block_addrs: Vec<Addr>,
    pub settle_sites: BTreeSet<ValueId>,
    pub flag_publish: BTreeMap<ValueId, FlagPublication>,
    pub entry_settle: bool,
    pub arguments: BTreeMap<ValueId, Vec<StateKey>>,
    pub x87: Option<rk_x87::Plan>,
}

pub fn plan_function(cg: &CgFunction, opts: &PlanOptions<'_>) -> Result<PlannedFunction> {
    let mut prepare_options = PrepareOptions::new(opts.call_facts);
    prepare_options.policy = opts.policy;
    prepare_options.fault_state = opts.fault_state;
    prepare_options.coalesce_roots = opts.coalesce_roots;
    prepare_options.assume_private_stack = opts.assume_private_stack;
    prepare_options.lazy_stack_writes = opts.lazy_stack_writes;
    prepare_options.assume_stack_pointer_bases = opts.assume_stack_pointer_bases;
    let (ssa, base) = prepare_with_publications(cg, &prepare_options)?;
    let base = base.unwrap_or_else(|| publications(&ssa, opts.call_facts));
    let mut return_addr = BTreeMap::new();
    for (index, value) in ssa.values.iter().enumerate() {
        if !matches!(value.op, rk_ir::Op::Call { .. }) {
            continue;
        }
        let addr = ssa.value_addr[index];
        if let Some(insn) = cg.index_of(addr).and_then(|n| cg.insns.get(n as usize)) {
            return_addr.insert(index as ValueId, addr + insn.length as Addr);
        }
    }
    let block_addrs = ssa
        .blocks
        .iter()
        .map(|block| cg.insns[block.start as usize].addr)
        .collect();
    plan_prepared(ssa, base, return_addr, block_addrs, opts)
}

pub fn replan_function(
    planned: PlannedFunction,
    opts: &PlanOptions<'_>,
) -> Result<PlannedFunction> {
    plan_prepared(
        planned.ssa,
        planned.base,
        planned.return_addr,
        planned.block_addrs,
        opts,
    )
}

pub fn defer_return_eip(
    mut planned: PlannedFunction,
    opts: &PlanOptions<'_>,
) -> Result<PlannedFunction> {
    if planned.ssa.return_slot.is_err() {
        return Ok(planned);
    }
    let eip = rk_ir::Registers::get().require("EIP").offset as u32;
    let is_eip = |key: StateKey| matches!(key, StateKey::Register(offset) if (eip..eip + 4).contains(&offset));
    let mut loads = BTreeSet::new();
    for boundary in &planned.ssa.boundaries {
        if planned.ssa.value(boundary.at).op != rk_ir::Op::Return {
            continue;
        }
        for &(key, value) in planned.ssa.boundary(boundary.at).unwrap_or_default() {
            if is_eip(key) {
                loads.insert(planned.ssa.args(value)[0]);
            }
        }
        if let Some(keys) = planned.base.get_mut(&boundary.at) {
            keys.retain(|key| !is_eip(*key) && *key != StateKey::Memory);
        }
    }
    let original: Vec<_> = planned
        .ssa
        .blocks
        .iter_mut()
        .map(|block| {
            let ops = block.ops.clone();
            block.ops.retain(|value| !loads.contains(value));
            ops
        })
        .collect();
    loop {
        planned = replan_function(planned, opts)?;
        let removed = loads.len();
        loads.retain(|value| !planned.live.contains(value));
        if loads.len() == removed {
            return Ok(planned);
        }
        for (block, ops) in planned.ssa.blocks.iter_mut().zip(&original) {
            block.ops = ops
                .iter()
                .copied()
                .filter(|value| !loads.contains(value))
                .collect();
        }
    }
}

fn plan_prepared(
    ssa: Ssa,
    base: BTreeMap<ValueId, Vec<StateKey>>,
    return_addr: BTreeMap<ValueId, Addr>,
    block_addrs: Vec<Addr>,
    opts: &PlanOptions<'_>,
) -> Result<PlannedFunction> {
    let mut passed = base.clone();
    let arguments = direct_arguments(&ssa, opts);
    for (value, keys) in &arguments {
        if let Some(published) = passed.get_mut(value) {
            published.retain(|key| !keys.contains(key));
        }
    }
    let (publish, guard) =
        reduce_publications(&ssa, &passed, &arguments, opts.call_facts, opts.call_hooks);
    let flag_plan = ssa.flag_plan();
    let mut flag_publish = BTreeMap::new();
    let mut descriptor_roots = Vec::new();
    let mut guard_only = BTreeMap::new();
    for (&value, keys) in &guard {
        let published = publish.get(&value);
        let keys: Vec<StateKey> = keys
            .iter()
            .copied()
            .filter(|key| published.is_none_or(|published| !published.contains(key)))
            .collect();
        if !keys.is_empty() {
            guard_only.insert(value, keys);
        }
    }
    let mut boundaries: BTreeMap<ValueId, Vec<StateKey>> = publish.clone();
    for (&value, keys) in &guard {
        boundaries
            .entry(value)
            .or_default()
            .extend(keys.iter().copied());
    }
    let mut live_publish = BTreeMap::new();
    for (value, keys) in &boundaries {
        let forwarded = guard_only.get(value).map_or(&[][..], Vec::as_slice);
        let mut publication = flag_plan.publication(&ssa, *value, keys, forwarded);
        if !opts.descriptors {
            publication.descriptor = None;
        }
        let removed = publication
            .descriptor
            .map_or(Fields::NONE, |descriptor| descriptor.mask)
            | publication.forwarded;
        let mut live_keys = keys.clone();
        live_keys.retain(|key| !ssa.field(*key).is_some_and(|field| removed.contains(field)));
        if let Some(descriptor) = &publication.descriptor {
            descriptor_roots.extend(descriptor.values());
        }
        flag_publish.insert(*value, publication);
        live_publish.insert(*value, live_keys);
    }
    for boundary in &ssa.boundaries {
        live_publish.entry(boundary.at).or_default();
    }
    for (value, keys) in &arguments {
        live_publish
            .entry(*value)
            .or_default()
            .extend(keys.iter().copied());
    }
    descriptor_roots.extend(ssa.frame);
    let live = live_values(&ssa, Some(&live_publish), &descriptor_roots, &[]);
    let entry_settle = (0..ssa.values.len()).any(|index| {
        let id = index as ValueId;
        live.contains(&id)
            && matches!(ssa.value(id).op, rk_ir::Op::Input(key) if ssa.field(key).is_some_and(|field| Fields::FLAGS.contains(field)))
    });
    let mut settle_sites = BTreeSet::new();
    for (index, value) in ssa.values.iter().enumerate() {
        if !live.contains(&(index as ValueId)) {
            continue;
        }
        let (rk_ir::Op::StateKeep(key) | rk_ir::Op::StateReload(key)) = value.op else {
            continue;
        };
        if !ssa
            .field(key)
            .is_some_and(|field| Fields::FLAGS.contains(field))
        {
            continue;
        }
        settle_sites.insert(ssa.args(index as ValueId)[0]);
    }
    let x87 = rk_x87::prepare(&ssa, opts.x87_modes, &opts.x87_options)
        .map_err(|reason| Diagnostic::error(ssa.addr, reason))?;
    let x87 = x87.used.then_some(x87);
    Ok(PlannedFunction {
        ssa,
        base,
        publish,
        guard,
        live,
        return_addr,
        block_addrs,
        settle_sites,
        flag_publish,
        entry_settle,
        arguments,
        x87,
    })
}

fn direct_arguments(ssa: &Ssa, opts: &PlanOptions<'_>) -> BTreeMap<ValueId, Vec<StateKey>> {
    let mut out = BTreeMap::new();
    for (index, value) in ssa.values.iter().enumerate() {
        let rk_ir::Op::Call { target } = value.op else {
            continue;
        };
        if !opts.direct_targets.contains(&target) {
            continue;
        }
        let Some(arguments) = opts.direct_arguments.get(&target) else {
            continue;
        };
        if arguments.registers.is_empty() {
            continue;
        }
        let keys: Vec<StateKey> = arguments
            .registers
            .iter()
            .flat_map(|field| rk_ir::lanes(field).iter().copied())
            .collect();
        out.insert(index as ValueId, keys);
    }
    out
}

fn reduce_publications(
    s: &Ssa,
    base: &BTreeMap<ValueId, Vec<StateKey>>,
    arguments: &BTreeMap<ValueId, Vec<StateKey>>,
    call_facts: &dyn CallFacts,
    call_hooks: bool,
) -> (
    BTreeMap<ValueId, Vec<StateKey>>,
    BTreeMap<ValueId, Vec<StateKey>>,
) {
    let block_count = s.blocks.len();
    if block_count == 0 || s.keys.is_empty() {
        return (BTreeMap::new(), BTreeMap::new());
    }
    let key_index: rk_core::Lookup<StateKey, usize> = s
        .keys
        .iter()
        .enumerate()
        .map(|(index, key)| (*key, index))
        .collect();
    let dispatch: Vec<Option<ValueId>> = s
        .keys
        .iter()
        .map(|key| {
            s.dispatch
                .loads
                .iter()
                .find(|(loaded, _)| loaded == key)
                .map(|(_, value)| *value)
        })
        .collect();
    let mut reloads: BTreeMap<ValueId, Vec<(usize, ValueId, bool)>> = BTreeMap::new();
    for (index, item) in s.values.iter().enumerate() {
        let value = index as ValueId;
        let (source, lane) = match item.op {
            rk_ir::Op::StateReload(_) | rk_ir::Op::StateKeep(_) => (value, None),
            rk_ir::Op::Byte(lane) => (s.args(value)[0], Some(lane)),
            _ => continue,
        };
        let (loaded, kept) = match s.value(source).op {
            rk_ir::Op::StateReload(loaded) => (loaded, false),
            rk_ir::Op::StateKeep(loaded)
                if rk_ir::field_lane(loaded).is_some_and(|(field, _)| field == Field::Esp) =>
            {
                (loaded, true)
            }
            _ => continue,
        };
        for (index, key) in s.keys.iter().enumerate() {
            if lane.map_or(loaded == *key, |lane| same_word_lane(loaded, *key, lane)) {
                reloads
                    .entry(s.args(source)[0])
                    .or_default()
                    .push((index, value, kept));
            }
        }
    }
    let phi_lanes = s.phi_lanes();
    let successors = successors(s);
    let mut out: Vec<Option<Vec<Option<ValueId>>>> = vec![None; block_count];
    let mut pending: std::collections::VecDeque<usize> = (0..block_count).collect();
    let mut queued = vec![true; block_count];
    let mut visits = vec![0u32; block_count];
    while let Some(block) = pending.pop_front() {
        queued[block] = false;
        if s.blocks[block]
            .preds
            .iter()
            .all(|&pred| pred != rk_ssa::DISPATCH_BLOCK && out[pred as usize].is_none())
        {
            continue;
        }
        visits[block] += 1;
        let mut resident = resident_in(s, block, &out, &dispatch, &phi_lanes[block]);
        walk_block(
            s,
            block,
            base,
            arguments,
            call_facts,
            call_hooks,
            &key_index,
            &reloads,
            &mut resident,
            None,
            None,
        );
        if let Some(previous) = out[block].as_ref().filter(|_| visits[block] > 4) {
            for (current, previous) in resident.iter_mut().zip(previous) {
                if current != previous {
                    *current = None;
                }
            }
        }
        if out[block].as_ref() != Some(&resident) {
            out[block] = Some(resident);
            for &next in &successors[block] {
                if !queued[next] {
                    queued[next] = true;
                    pending.push_back(next);
                }
            }
        }
    }
    let mut publish = BTreeMap::new();
    let mut guard = BTreeMap::new();
    for block in 0..block_count {
        let mut resident = resident_in(s, block, &out, &dispatch, &phi_lanes[block]);
        walk_block(
            s,
            block,
            base,
            arguments,
            call_facts,
            call_hooks,
            &key_index,
            &reloads,
            &mut resident,
            Some(&mut publish),
            Some(&mut guard),
        );
    }
    (publish, guard)
}

fn successors(s: &Ssa) -> Vec<Vec<usize>> {
    let mut successors = vec![Vec::new(); s.blocks.len()];
    for (index, block) in s.blocks.iter().enumerate() {
        for pred in &block.preds {
            if *pred != rk_ssa::DISPATCH_BLOCK {
                successors[*pred as usize].push(index);
            }
        }
    }
    successors
}

fn resident_in(
    s: &Ssa,
    block: usize,
    out: &[Option<Vec<Option<ValueId>>>],
    dispatch: &[Option<ValueId>],
    lanes: &[(usize, ValueId, Vec<ValueId>)],
) -> Vec<Option<ValueId>> {
    let preds = &s.blocks[block].preds;
    let source = |pred: rk_ssa::BlockId| {
        if pred == rk_ssa::DISPATCH_BLOCK {
            Some(dispatch)
        } else {
            out[pred as usize].as_deref()
        }
    };
    let mut resident: Option<Vec<Option<ValueId>>> = None;
    for source in preds.iter().filter_map(|&pred| source(pred)) {
        match &mut resident {
            None => resident = Some(source.to_vec()),
            Some(current) => {
                for (left, right) in current.iter_mut().zip(source) {
                    if left != right {
                        *left = None;
                    }
                }
            }
        }
    }
    let mut resident = resident.unwrap_or_else(|| vec![None; dispatch.len()]);
    for (index, value, args) in lanes {
        let carries = preds.iter().enumerate().all(|(position, &pred)| {
            source(pred).is_none_or(|source| source[*index] == Some(args[position]))
        });
        if carries {
            resident[*index] = Some(*value);
        }
    }
    resident
}

fn walk_block(
    s: &Ssa,
    block: usize,
    base: &BTreeMap<ValueId, Vec<StateKey>>,
    arguments: &BTreeMap<ValueId, Vec<StateKey>>,
    call_facts: &dyn CallFacts,
    call_hooks: bool,
    key_index: &rk_core::Lookup<StateKey, usize>,
    reloads: &BTreeMap<ValueId, Vec<(usize, ValueId, bool)>>,
    resident: &mut [Option<ValueId>],
    mut publish: Option<&mut BTreeMap<ValueId, Vec<StateKey>>>,
    mut guard: Option<&mut BTreeMap<ValueId, Vec<StateKey>>>,
) {
    let item = &s.blocks[block];
    for value in item.phis.iter().chain(item.ops.iter()).copied() {
        let Some(state) = s.boundary(value) else {
            continue;
        };
        for &(key, state_value) in state {
            let Some(&index) = key_index.get(&key) else {
                continue;
            };
            match s.value(state_value).op {
                rk_ir::Op::StateKeep(loaded) if loaded == key => {
                    let old = s.args(state_value)[1];
                    if resident[index] == Some(old) {
                        resident[index] = Some(state_value);
                    }
                }
                rk_ir::Op::Byte(lane) => {
                    let source = s.args(state_value)[0];
                    match s.value(source).op {
                        rk_ir::Op::StateKeep(loaded) if same_word_lane(loaded, key, lane) => {
                            let old = s.args(source)[1];
                            let old_lane = match s.value(old).op {
                                rk_ir::Op::Pack => s.args(old).get(lane as usize).copied(),
                                _ if lane == 0 => Some(old),
                                _ => None,
                            };
                            if old_lane.is_some_and(|old| resident[index] == Some(old)) {
                                resident[index] = Some(state_value);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        let Some(keys) = base.get(&value) else {
            continue;
        };
        let current: BTreeMap<StateKey, ValueId> = state.iter().copied().collect();
        let escape_only = matches!(
            s.value(value).op,
            rk_ir::Op::BranchInd | rk_ir::Op::Div { .. } | rk_ir::Op::Idiv { .. }
        );
        for key in keys {
            let Some(&state_value) = current.get(key) else {
                continue;
            };
            let Some(&index) = key_index.get(key) else {
                continue;
            };
            if resident[index] != Some(state_value) {
                if !escape_only {
                    resident[index] = Some(state_value);
                }
                if let Some(out) = publish.as_deref_mut() {
                    out.entry(value).or_default().push(*key);
                }
            }
        }
        for key in arguments.get(&value).into_iter().flatten() {
            if let Some(&index) = key_index.get(key) {
                resident[index] = None;
            }
        }
        if call_hooks
            && matches!(
                s.value(value).op,
                rk_ir::Op::Call { .. } | rk_ir::Op::CallInd
            )
        {
            for (index, key) in s.keys.iter().enumerate() {
                if *key == StateKey::Memory || s.field(*key).is_none() || keys.contains(key) {
                    continue;
                }
                let Some(&state_value) = current.get(key) else {
                    continue;
                };
                if resident[index] == Some(state_value) {
                    continue;
                }
                resident[index] = None;
                if let Some(out) = guard.as_deref_mut() {
                    out.entry(value).or_default().push(*key);
                }
            }
        }
        for key in s.frame_guard.get(&value).into_iter().flatten() {
            let (Some(&index), Some(&state_value)) = (key_index.get(key), current.get(key)) else {
                continue;
            };
            if resident[index] == Some(state_value) {
                continue;
            }
            resident[index] = None;
            if let Some(out) = guard.as_deref_mut() {
                out.entry(value).or_default().push(*key);
            }
        }
        let homes: Vec<bool> = reloads
            .get(&value)
            .into_iter()
            .flatten()
            .map(|&(index, _, kept)| {
                !kept
                    || resident[index].is_some()
                        && resident[index] == current.get(&s.keys[index]).copied()
            })
            .collect();
        let writes = boundary_writes(s, call_facts, value);
        if writes != Fields::NONE {
            for (index, key) in s.keys.iter().enumerate() {
                if s.field(*key).is_some_and(|field| writes.contains(field)) {
                    resident[index] = None;
                }
            }
        }
        for kept in [false, true] {
            for (&(index, loaded, keep), &home) in
                reloads.get(&value).into_iter().flatten().zip(&homes)
            {
                if keep == kept && home {
                    resident[index] = Some(loaded);
                }
            }
        }
    }
}

fn same_word_lane(loaded: StateKey, key: StateKey, lane: u8) -> bool {
    if let (Some((loaded_field, loaded_lane)), Some((field, key_lane))) =
        (rk_ir::field_lane(loaded), rk_ir::field_lane(key))
    {
        return loaded_field == field && loaded_lane == 0 && key_lane == lane;
    }
    if let (Some((loaded_reg, loaded_offset)), Some((reg, offset))) =
        (rk_ir::xmm_offset(loaded), rk_ir::xmm_offset(key))
    {
        return loaded_reg == reg
            && loaded_offset / 4 == offset / 4
            && loaded_offset % 4 == 0
            && offset % 4 == lane;
    }
    false
}

fn boundary_writes(s: &Ssa, call_facts: &dyn CallFacts, value: ValueId) -> Fields {
    match s.value(value).op {
        rk_ir::Op::Call { target } => {
            call_facts
                .call(s.value_addr[value as usize], Some(target))
                .writes
        }
        rk_ir::Op::CallInd => call_facts.call(s.value_addr[value as usize], None).writes,
        rk_ir::Op::String(name) => rk_ir::string::effect(name.0).1,
        rk_ir::Op::Slow(_) => Fields::ALL,
        rk_ir::Op::P(rk_core::ir::OpCode::Call)
        | rk_ir::Op::P(rk_core::ir::OpCode::CallInd)
        | rk_ir::Op::P(rk_core::ir::OpCode::CallOther)
        | rk_ir::Op::P(rk_core::ir::OpCode::Unknown) => Fields::ALL,
        _ => Fields::NONE,
    }
}
