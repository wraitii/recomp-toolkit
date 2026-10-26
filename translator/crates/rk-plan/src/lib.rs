//! Per-function planning between SSA and C. For each boundary it decides which guest state lanes to
//! publish (skipping those already resident), which values stay live and where lazy flag
//! descriptors settle. It also chooses what hook-guarded calls republish and which output fields
//! each direct private-contract call needs. It then attaches the x87 plan. `PlannedFunction` holds
//! every decision that `rk-emit` prints.

use std::collections::{BTreeMap, BTreeSet};

use rk_core::contract::{Field, Fields};
use rk_core::ir::Policy;
use rk_core::{Addr, Diagnostic, LookupSet, Result};
use rk_ir::{CgFunction, StateKey};
use rk_ssa::{
    live_values, prepare_with_publications, publications, CallFacts, FlagPublication,
    PrepareOptions, Ssa, ValueId,
};

#[derive(Clone, Debug, Default)]
pub struct ContractRequests {
    pub by_caller: BTreeMap<Addr, std::collections::BTreeSet<(Addr, Fields)>>,
}

pub struct PlanOptions<'a> {
    pub call_facts: &'a dyn CallFacts,
    pub policy: Policy,
    pub x87_modes: &'a rk_x87::ModeInput,
    pub x87_options: rk_x87::Options,
    pub fault_state: rk_core::settings::FaultState,
    pub private_targets: &'a BTreeSet<Addr>,
    pub descriptors: bool,
    pub coalesce_roots: bool,
    pub assume_private_stack: bool,
    pub lazy_stack_writes: bool,
    pub retire_reused_stack: bool,
    pub assume_stack_pointer_bases: bool,
    pub call_hooks: bool,
}

pub struct PlannedFunction {
    pub ssa: Ssa,
    pub publish: BTreeMap<ValueId, Vec<StateKey>>,
    pub guard: BTreeMap<ValueId, Vec<StateKey>>,
    pub live: LookupSet<ValueId>,
    pub return_addr: BTreeMap<ValueId, Addr>,
    pub block_addrs: Vec<Addr>,
    pub settle_sites: BTreeSet<ValueId>,
    pub flag_publish: BTreeMap<ValueId, FlagPublication>,
    pub entry_settle: bool,
    pub call_masks: BTreeMap<ValueId, (Addr, Fields)>,
    pub requests: ContractRequests,
    pub x87: Option<rk_x87::Plan>,
}

pub fn plan_function(cg: &CgFunction, opts: &PlanOptions<'_>) -> Result<PlannedFunction> {
    let mut prepare_options = PrepareOptions::new(opts.call_facts);
    prepare_options.policy = opts.policy;
    prepare_options.fault_state = opts.fault_state;
    prepare_options.coalesce_roots = opts.coalesce_roots;
    prepare_options.assume_private_stack = opts.assume_private_stack;
    prepare_options.lazy_stack_writes = opts.lazy_stack_writes;
    prepare_options.retire_reused_stack = opts.retire_reused_stack;
    prepare_options.assume_stack_pointer_bases = opts.assume_stack_pointer_bases;
    let (ssa, base) = prepare_with_publications(cg, &prepare_options)?;
    let base = base.unwrap_or_else(|| publications(&ssa, opts.call_facts));
    let (publish, guard) = reduce_publications(&ssa, &base, opts.call_facts, opts.call_hooks);
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
    let live = live_values(&ssa, Some(&live_publish), &descriptor_roots, &[]);
    let entry_settle = (0..ssa.values.len()).any(|index| {
        let id = index as ValueId;
        live.contains(&id)
            && matches!(ssa.value(id).op, rk_ir::Op::Input(key) if ssa.field(key).is_some_and(|field| Fields::FLAGS.contains(field)))
    });
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
    let mut reload_masks: BTreeMap<ValueId, Fields> = BTreeMap::new();
    for (index, value) in ssa.values.iter().enumerate() {
        let id = index as ValueId;
        let rk_ir::Op::StateReload(key) = value.op else {
            continue;
        };
        let Some(field) = ssa.field(key) else {
            continue;
        };
        let site = ssa.args(id)[0];
        *reload_masks.entry(site).or_default() |= Fields(field.bit());
    }
    for (value, keys) in &base {
        let Some(state) = ssa.boundary(*value) else {
            continue;
        };
        for &(key, state_value) in state {
            if !keys.contains(&key) {
                continue;
            }
            let rk_ir::Op::StateReload(loaded) = ssa.value(state_value).op else {
                continue;
            };
            let Some(field) = ssa.field(loaded) else {
                continue;
            };
            let site = ssa.args(state_value)[0];
            *reload_masks.entry(site).or_default() |= Fields(field.bit());
        }
    }
    let mut call_masks = BTreeMap::new();
    let mut requested: BTreeMap<(Addr, Fields), ()> = BTreeMap::new();
    for (index, value) in ssa.values.iter().enumerate() {
        let id = index as ValueId;
        let rk_ir::Op::Call { target } = value.op else {
            continue;
        };
        if !opts.private_targets.contains(&target) {
            continue;
        }
        let mask = reload_masks.get(&id).copied().unwrap_or_default();
        call_masks.insert(id, (target, mask));
        requested.insert((target, mask), ());
    }
    let mut requests = ContractRequests::default();
    if !requested.is_empty() {
        requests
            .by_caller
            .insert(ssa.addr, requested.keys().copied().collect());
    }
    let x87 = rk_x87::prepare(&ssa, opts.x87_modes, &opts.x87_options)
        .map_err(|reason| Diagnostic::error(ssa.addr, reason))?;
    let x87 = x87.used.then_some(x87);
    Ok(PlannedFunction {
        ssa,
        publish,
        guard,
        live,
        return_addr,
        block_addrs,
        settle_sites,
        flag_publish,
        entry_settle,
        call_masks,
        requests,
        x87,
    })
}

fn reduce_publications(
    s: &Ssa,
    base: &BTreeMap<ValueId, Vec<StateKey>>,
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
    let n = s.keys.len();
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
    let mut out = vec![vec![None; n]; block_count];
    let mut pending: std::collections::VecDeque<usize> = (0..block_count).collect();
    let mut queued = vec![true; block_count];
    while let Some(block) = pending.pop_front() {
        queued[block] = false;
        let mut resident = resident_in(s, block, &out, &dispatch, &phi_lanes[block]);
        walk_block(
            s,
            block,
            base,
            call_facts,
            call_hooks,
            &key_index,
            &reloads,
            &mut resident,
            None,
            None,
        );
        if resident != out[block] {
            out[block] = resident;
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
    out: &[Vec<Option<ValueId>>],
    dispatch: &[Option<ValueId>],
    lanes: &[(usize, ValueId, Vec<ValueId>)],
) -> Vec<Option<ValueId>> {
    let preds = &s.blocks[block].preds;
    let mut resident: Option<Vec<Option<ValueId>>> = None;
    for pred in preds {
        let source: &[Option<ValueId>] = if *pred == rk_ssa::DISPATCH_BLOCK {
            dispatch
        } else {
            &out[*pred as usize]
        };
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
        let carries = preds.iter().enumerate().all(|(position, pred)| {
            let source: &[Option<ValueId>] = if *pred == rk_ssa::DISPATCH_BLOCK {
                dispatch
            } else {
                &out[*pred as usize]
            };
            source[*index] == Some(args[position])
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
        let escape_only = s.value(value).op == rk_ir::Op::BranchInd;
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
        rk_ir::Op::Slow(_) | rk_ir::Op::Div { .. } | rk_ir::Op::Idiv { .. } => Fields::ALL,
        rk_ir::Op::P(rk_core::ir::OpCode::Call)
        | rk_ir::Op::P(rk_core::ir::OpCode::CallInd)
        | rk_ir::Op::P(rk_core::ir::OpCode::CallOther)
        | rk_ir::Op::P(rk_core::ir::OpCode::Unknown) => Fields::ALL,
        _ => Fields::NONE,
    }
}
