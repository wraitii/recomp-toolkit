use std::collections::{BTreeMap, BTreeSet};

use rk_core::contract::Field;
use rk_core::ir::OpCode;
use rk_core::memory::{Address, Base, Range};
use rk_ir::{x87::X87Op, Op, StateKey};

use crate::address::Addresses;
use crate::memory::{local_accesses, PrivateFrame};
use crate::{Ssa, ValueId, DISPATCH_BLOCK};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Keep,
    Write,
    Settle,
    Separate,
    Retire,
    Hook,
}

const OVERWRITE: u8 = 1;
const SEPARATED_OVERWRITE: u8 = 2;
const CLEAN: u8 = 1;
const PENDING: u8 = 2;

pub(crate) fn split_x87(s: &mut Ssa, facts: &dyn crate::CallFacts) {
    let accesses = local_accesses(s, None);
    let private = PrivateFrame::new(s, &accesses, Some(facts));
    if !private.eligible {
        return;
    }
    let mut addresses = Addresses::local(s);
    let mut candidates = BTreeMap::new();
    for (&id, access) in &accesses {
        let Op::X87(op) = s.value(id).op else {
            continue;
        };
        if !matches!(
            op,
            X87Op::Push {
                src: None,
                bits: 32 | 64
            } | X87Op::Store {
                dst: None,
                bits: 32 | 64,
                ..
            } | X87Op::Store {
                dst: None,
                bits: 16,
                integer: true,
                ..
            }
        ) {
            continue;
        }
        let Some(range) = access.location else {
            continue;
        };
        if !range.address.is_exact() || !private.unexposed(range) {
            continue;
        }
        addresses.get(s.args(id)[1]);
        let Address::Known {
            base: Base::Frame,
            offset,
            span,
        } = addresses.relations.bounds(range.address)
        else {
            continue;
        };
        let start = i64::from(offset as i32);
        if start < 0 && start + i64::from(span) + i64::from(range.size) <= 0 {
            candidates.insert(id, op);
        }
    }
    let mut loads = BTreeMap::new();
    for block in 0..s.blocks.len() {
        let mut ops = Vec::new();
        for id in s.blocks[block].ops.clone() {
            let Some(&op) = candidates.get(&id) else {
                ops.push(id);
                continue;
            };
            let args = s.args(id).to_vec();
            let addr = s.value_addr[id as usize];
            match op {
                X87Op::Store {
                    bits, pop, integer, ..
                } => {
                    let value = s.new_value(
                        Op::X87(X87Op::StoreValue { bits, pop, integer }),
                        (bits / 8) as u8,
                        &[],
                        addr,
                    );
                    s.values[id as usize].op = Op::P(OpCode::Store);
                    s.values[id as usize].args_start = s.args.len() as u32;
                    s.values[id as usize].args_len = 3;
                    s.args.extend([args[0], args[1], value]);
                    ops.extend([value, id]);
                }
                X87Op::Push { bits, .. } => {
                    let value = s.new_value(Op::P(OpCode::Load), (bits / 8) as u8, &args, addr);
                    s.values[id as usize].op = Op::X87(X87Op::PushValue { bits });
                    s.values[id as usize].args_start = s.args.len() as u32;
                    s.values[id as usize].args_len = 1;
                    s.args.push(value);
                    loads.insert(id, value);
                    ops.extend([value, id]);
                }
                _ => unreachable!(),
            }
        }
        s.blocks[block].ops = ops;
    }
    for index in 0..s.values.len() {
        if s.values[index].op != Op::Memory {
            continue;
        }
        let start = s.values[index].args_start as usize;
        if let Some(&load) = loads.get(&s.args[start]) {
            s.args[start] = load;
        }
    }
}

fn successors(s: &Ssa) -> Vec<Vec<usize>> {
    let mut result = vec![Vec::new(); s.blocks.len()];
    for (block, item) in s.blocks.iter().enumerate() {
        for &pred in &item.preds {
            if pred != DISPATCH_BLOCK {
                result[pred as usize].push(block);
            }
        }
    }
    result
}

fn delayed_stores(
    s: &Ssa,
    next: &[Vec<usize>],
    actions: &BTreeMap<ValueId, Action>,
) -> BTreeSet<ValueId> {
    let mut future = vec![0u8; s.blocks.len()];
    let transfer = |id: ValueId, state: u8| match actions.get(&id) {
        Some(Action::Write) => OVERWRITE,
        Some(Action::Settle) => 0,
        Some(Action::Retire) => OVERWRITE | SEPARATED_OVERWRITE,
        Some(Action::Separate | Action::Hook) if state & OVERWRITE != 0 => {
            state | SEPARATED_OVERWRITE
        }
        _ => state,
    };
    loop {
        let mut changed = false;
        for (block, item) in s.blocks.iter().enumerate().rev() {
            let mut state = next[block]
                .iter()
                .fold(0, |state, &next| state | future[next]);
            for &id in item.ops.iter().rev() {
                state = transfer(id, state);
            }
            if state != future[block] {
                future[block] = state;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut stores = BTreeSet::new();
    for (block, item) in s.blocks.iter().enumerate() {
        let mut state = next[block]
            .iter()
            .fold(0, |state, &next| state | future[next]);
        for &id in item.ops.iter().rev() {
            if actions.get(&id) == Some(&Action::Write) && state & SEPARATED_OVERWRITE != 0 {
                stores.insert(id);
            }
            state = transfer(id, state);
        }
    }
    stores
}

fn incoming(s: &Ssa, block: usize, outputs: &[u8]) -> u8 {
    s.blocks[block].preds.iter().fold(0, |state, &pred| {
        state
            | if pred == DISPATCH_BLOCK {
                CLEAN
            } else {
                outputs[pred as usize]
            }
    })
}

fn transfer(
    actions: &BTreeMap<ValueId, Action>,
    stores: &BTreeSet<ValueId>,
    id: ValueId,
    state: u8,
) -> u8 {
    match actions.get(&id) {
        Some(Action::Write) => {
            if stores.contains(&id) {
                PENDING
            } else {
                CLEAN
            }
        }
        Some(Action::Settle | Action::Retire) => CLEAN,
        _ => state,
    }
}

fn settle_joins(
    s: &mut Ssa,
    frame: ValueId,
    actions: &mut BTreeMap<ValueId, Action>,
    stores: &BTreeSet<ValueId>,
    settlements: &mut BTreeSet<ValueId>,
) -> Option<Vec<u8>> {
    loop {
        let mut outputs = vec![0; s.blocks.len()];
        loop {
            let mut changed = false;
            for (block, item) in s.blocks.iter().enumerate() {
                let state = item
                    .ops
                    .iter()
                    .fold(incoming(s, block, &outputs), |state, &id| {
                        transfer(actions, stores, id, state)
                    });
                if state != outputs[block] {
                    outputs[block] = state;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut mixed: Vec<_> = s
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(block, item)| {
                let mut state = incoming(s, block, &outputs);
                for &id in &item.ops {
                    if matches!(actions.get(&id), Some(Action::Settle | Action::Hook))
                        && state == (CLEAN | PENDING)
                    {
                        return Some(block);
                    }
                    state = transfer(actions, stores, id, state);
                }
                None
            })
            .collect();
        if mixed.is_empty() {
            return Some(outputs);
        }
        let mut visited = BTreeSet::new();
        let mut predecessors = BTreeSet::new();
        while let Some(block) = mixed.pop() {
            if !visited.insert(block) {
                continue;
            }
            for &pred in &s.blocks[block].preds {
                if pred == DISPATCH_BLOCK {
                    continue;
                }
                match outputs[pred as usize] {
                    PENDING => {
                        predecessors.insert(pred);
                    }
                    state if state == (CLEAN | PENDING) => mixed.push(pred as usize),
                    _ => {}
                }
            }
        }
        if predecessors.is_empty() {
            return None;
        }
        for pred in predecessors {
            let block = &s.blocks[pred as usize];
            let last = block.ops.last().copied();
            let position = block.ops.len()
                - usize::from(last.is_some_and(|id| rk_ir::properties(s.value(id).op).terminator));
            let addr = last.map_or(s.addr, |id| s.value_addr[id as usize]);
            let marker = s.new_value(Op::StackBoundary, 0, &[frame, frame], addr);
            s.blocks[pred as usize].ops.insert(position, marker);
            settlements.insert(marker);
            actions.insert(marker, Action::Settle);
        }
    }
}

pub(crate) fn promote(s: &mut Ssa, retire_reused_stack: bool, facts: &dyn crate::CallFacts) {
    for block in &mut s.blocks {
        block.ops.retain(|&id| {
            let value = &s.values[id as usize];
            value.op != Op::StackBoundary
                || s.args[value.args_start as usize] != s.args[value.args_start as usize + 1]
        });
    }
    place_writes(s, retire_reused_stack, facts);
    if !s
        .blocks
        .iter()
        .flat_map(|block| &block.ops)
        .any(|&id| s.value(id).op == Op::StackFlush)
    {
        s.stack_guards.clear();
    }
    for block in &mut s.blocks {
        block
            .ops
            .retain(|&id| s.values[id as usize].op != Op::StackBoundary);
    }
}

fn place_writes(s: &mut Ssa, retire_reused_stack: bool, facts: &dyn crate::CallFacts) {
    let accesses = local_accesses(s, None);
    let private = PrivateFrame::new(s, &accesses, Some(facts));
    if !private.eligible {
        return;
    }
    let mut addresses = Addresses::local(s);
    let mut slots: BTreeMap<Range, BTreeSet<ValueId>> = BTreeMap::new();
    for (&id, access) in &accesses {
        if s.value(id).op != Op::P(OpCode::Store) {
            continue;
        }
        let Some(range) = access.location else {
            continue;
        };
        if !private.unexposed(range) {
            continue;
        }
        addresses.get(s.args(id)[1]);
        let Address::Known {
            base,
            offset,
            span: 0,
        } = range.address
        else {
            continue;
        };
        if !matches!(base, Base::Frame)
            && !matches!(base, Base::Stack(root)
            if s.value(root).size == 4 && matches!(s.value(root).op, Op::P(OpCode::IntAnd)))
        {
            continue;
        }
        let Address::Known {
            base: Base::Frame,
            offset: floor,
            span,
        } = addresses.relations.bounds(range.address)
        else {
            continue;
        };
        if i64::from(floor as i32) + i64::from(span) + i64::from(range.size) > 0 {
            continue;
        }
        let start = i64::from(offset as i32);
        if !matches!(range.size, 1 | 2 | 4 | 8) {
            continue;
        }
        let Address::Known {
            base: esp_base,
            offset: esp,
            span: 0,
        } = addresses.state(id, Field::Esp)
        else {
            continue;
        };
        if esp_base == base && i64::from(esp as i32) <= start {
            slots.entry(range).or_default().insert(id);
        }
    }
    let ranges: Vec<_> = slots.keys().copied().collect();
    let ordinary: BTreeSet<_> = slots
        .iter()
        .filter_map(|(&range, writes)| {
            writes
                .iter()
                .any(|&id| s.boundary(id).is_some())
                .then_some(range)
        })
        .collect();
    slots.retain(|range, _| {
        ranges.iter().all(|other| {
            range == other
                || ordinary.contains(range) && !ordinary.contains(other)
                || addresses.relations.disjoint(*range, *other)
        })
    });
    if slots.len() > 64 || slots.len().saturating_mul(s.blocks.len()) > 100_000 {
        return;
    }
    let next = successors(s);
    if next.iter().enumerate().any(|(block, next)| {
        next.is_empty()
            && s.blocks[block]
                .ops
                .last()
                .is_none_or(|&id| s.value(id).op != Op::Return)
    }) {
        return;
    }
    let return_stacks: BTreeMap<_, _> = s
        .blocks
        .iter()
        .flat_map(|block| &block.ops)
        .filter(|&&id| s.value(id).op == Op::Return)
        .map(|&id| (id, addresses.state(id, Field::Esp)))
        .collect();
    let stack_moves: BTreeMap<_, _> = s
        .blocks
        .iter()
        .flat_map(|block| &block.ops)
        .filter(|&&id| s.value(id).op == Op::StackBoundary)
        .map(|&id| (id, addresses.get(s.args(id)[1])))
        .collect();
    let mut changed = false;
    let mut settlements = BTreeSet::new();
    let mut constants = BTreeMap::new();
    let esp: Vec<_> = rk_ir::lanes(Field::Esp)
        .iter()
        .map(|&key| s.input(key).unwrap())
        .collect();
    let frame = s.new_value(Op::Pack, 4, &esp, 0);
    s.blocks[s.entries[0].target as usize].ops.insert(0, frame);
    let calls = s
        .blocks
        .iter()
        .flat_map(|block| &block.ops)
        .any(|&id| matches!(s.value(id).op, Op::Call { .. } | Op::CallInd));
    if !slots.is_empty() {
        for &id in s.blocks.iter().flat_map(|block| &block.ops) {
            if let (Op::Call { .. }, Some(checkpoints)) =
                (s.value(id).op, private.call_checkpoints(id))
            {
                s.stack_guards.insert(id, checkpoints);
            }
        }
    }
    for (range, writes) in slots {
        let Address::Known { base, .. } = range.address else {
            unreachable!()
        };
        let mut actions = BTreeMap::new();
        for &id in s.blocks.iter().flat_map(|block| &block.ops) {
            let op = s.value(id).op;
            let action = if settlements.contains(&id) {
                Action::Keep
            } else if retire_reused_stack
                && base == Base::Frame
                && op == Op::Return
                && matches!(return_stacks.get(&id).copied(), Some(Address::Known {
                    base: Base::Frame, offset, span: 0
                }) if i64::from(offset as i32) >= i64::from(match range.address {
                    Address::Known { offset, .. } => offset as i32,
                    _ => unreachable!(),
                }) + i64::from(range.size))
            {
                Action::Retire
            } else if retire_reused_stack && op == Op::StackBoundary {
                Action::Keep
            } else if op == Op::StackBoundary
                && matches!(stack_moves.get(&id),
                Some(Address::Known { base: moved_base, offset, span: 0 })
                    if *moved_base == base && (*offset as i32) <= match range.address {
                        Address::Known { offset, .. } => offset as i32,
                        _ => unreachable!(),
                    })
            {
                Action::Keep
            } else if s.stack_guards.contains_key(&id) && private.call_disjoint(id, range) {
                Action::Hook
            } else if writes.contains(&id) {
                Action::Write
            } else if matches!(
                op,
                Op::StackBoundary | Op::Return | Op::Call { .. } | Op::CallInd
            ) || rk_ir::properties(op).opaque
                || rk_ir::properties(op).terminator && s.boundary(id).is_some()
            {
                Action::Settle
            } else if let Some(access) = accesses.get(&id) {
                if access.reads.disjoint(range) && access.writes.disjoint(range) {
                    Action::Keep
                } else if private.disjoint(id, range) {
                    Action::Separate
                } else {
                    Action::Settle
                }
            } else {
                Action::Keep
            };
            actions.insert(id, action);
        }
        let stores = delayed_stores(s, &next, &actions);
        if stores.is_empty() {
            continue;
        }
        let Some(outputs) = settle_joins(s, frame, &mut actions, &stores, &mut settlements) else {
            continue;
        };
        changed = true;
        let zero = s.constant(&mut constants, range.size as u8, 0);
        let Address::Known { offset, .. } = range.address else {
            unreachable!()
        };
        let slot_base = match base {
            Base::Frame => frame,
            Base::Stack(root) => root,
            _ => unreachable!(),
        };
        let offset = s.constant(&mut constants, 4, u64::from(offset));
        let mut phis = Vec::new();
        for block in 0..s.blocks.len() {
            let phi = s.new_value(Op::Phi, range.size as u8, &[], 0);
            s.blocks[block].phis.push(phi);
            phis.push(phi);
        }
        let mut values = Vec::new();
        for block in 0..s.blocks.len() {
            let mut value = phis[block];
            let mut state = incoming(s, block, &outputs);
            let mut ops = Vec::new();
            for id in s.blocks[block].ops.clone() {
                if stores.contains(&id) {
                    value = s.args(id)[2];
                    state = PENDING;
                    let prior = s.args(id)[0];
                    s.values[id as usize].op = Op::Memory;
                    s.values[id as usize].args_start = s.args.len() as u32;
                    s.values[id as usize].args_len = 1;
                    s.args.push(prior);
                    continue;
                }
                if matches!(actions.get(&id), Some(Action::Settle | Action::Hook))
                    && state == PENDING
                {
                    let addr = s.value_addr[id as usize];
                    let address = s.new_value(Op::P(OpCode::IntAdd), 4, &[slot_base, offset], addr);
                    let op = if actions.get(&id) == Some(&Action::Hook) {
                        Op::StackFlush
                    } else {
                        Op::P(OpCode::Store)
                    };
                    let flush = s.new_value(op, 0, &[s.memory().unwrap(), address, value], addr);
                    ops.extend([address, flush]);
                }
                state = transfer(&actions, &stores, id, state);
                if state == CLEAN {
                    value = zero;
                }
                ops.push(id);
            }
            s.blocks[block].ops = ops;
            values.push(value);
        }
        for (block, &phi) in phis.iter().enumerate() {
            let args: Vec<_> = s.blocks[block]
                .preds
                .iter()
                .map(|&pred| {
                    if pred == DISPATCH_BLOCK {
                        zero
                    } else {
                        values[pred as usize]
                    }
                })
                .collect();
            s.values[phi as usize].args_start = s.args.len() as u32;
            s.values[phi as usize].args_len = args.len() as u32;
            s.args.extend(args);
        }
        s.boundaries
            .retain(|boundary| !stores.contains(&boundary.at));
    }
    s.boundary_index = s
        .boundaries
        .iter()
        .enumerate()
        .map(|(index, boundary)| (boundary.at, index as u32))
        .collect();
    if changed {
        s.cleanup_used |= calls;
        rebuild_memory(s);
    }
}

fn rebuild_memory(s: &mut Ssa) {
    let memory_key = s
        .keys
        .iter()
        .position(|&key| key == StateKey::Memory)
        .unwrap();
    let mut entries = Vec::new();
    for block in 0..s.blocks.len() {
        s.blocks[block]
            .phis
            .retain(|&id| s.values[id as usize].size != 0);
        let phi = s.new_value(Op::Phi, 0, &[], 0);
        s.blocks[block].phis.push(phi);
        s.phi_key.insert(phi, memory_key);
        entries.push(phi);
    }
    let mut exits = Vec::new();
    for block in 0..s.blocks.len() {
        let mut memory = entries[block];
        for id in s.blocks[block].ops.clone() {
            if let Some(&index) = s.boundary_index.get(&id) {
                let boundary = s.boundaries[index as usize];
                for (key, value) in &mut s.state[boundary.state_start as usize
                    ..(boundary.state_start + boundary.state_len) as usize]
                {
                    if *key == StateKey::Memory {
                        *value = memory;
                    }
                }
            }
            if rk_ir::properties(s.value(id).op).memory {
                let args = s.values[id as usize].args_start as usize;
                s.args[args] = memory;
                memory = s.new_value(Op::Memory, 0, &[id, memory], s.value_addr[id as usize]);
            }
        }
        s.exit_state[block][memory_key] = Some(memory);
        for (key, value) in &mut s.entry_reads[block] {
            if *key == memory_key {
                *value = entries[block];
            }
        }
        exits.push(memory);
    }
    for (block, &phi) in entries.iter().enumerate() {
        let args: Vec<_> = s.blocks[block]
            .preds
            .iter()
            .map(|&pred| {
                if pred == DISPATCH_BLOCK {
                    s.memory().unwrap()
                } else {
                    exits[pred as usize]
                }
            })
            .collect();
        s.values[phi as usize].args_start = s.args.len() as u32;
        s.values[phi as usize].args_len = args.len() as u32;
        s.args.extend(args);
    }
    s.memory_partitioned = false;
}
