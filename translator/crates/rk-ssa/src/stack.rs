use std::collections::{BTreeMap, BTreeSet};

use rk_core::contract::Field;
use rk_core::ir::OpCode;
use rk_core::memory::{Address, Base, Range};
use rk_ir::{Op, StateKey};

use crate::address::Addresses;
use crate::memory::{accesses, PrivateFrame};
use crate::{Ssa, ValueId, DISPATCH_BLOCK};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Keep,
    Write,
    Settle,
    Separate,
}

const OVERWRITE: u8 = 1;
const SEPARATED_OVERWRITE: u8 = 2;
const CLEAN: u8 = 1;
const PENDING: u8 = 2;

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
        Some(Action::Separate) if state & OVERWRITE != 0 => state | SEPARATED_OVERWRITE,
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

pub(crate) fn promote(s: &mut Ssa) {
    for block in &mut s.blocks {
        block.ops.retain(|&id| {
            let value = &s.values[id as usize];
            value.op != Op::StackBoundary
                || s.args[value.args_start as usize] != s.args[value.args_start as usize + 1]
        });
    }
    place_writes(s);
    for block in &mut s.blocks {
        block
            .ops
            .retain(|&id| s.values[id as usize].op != Op::StackBoundary);
    }
}

fn place_writes(s: &mut Ssa) {
    let accesses = accesses(s, None);
    let private = PrivateFrame::new(s, &accesses, true);
    if !private.eligible {
        return;
    }
    let mut addresses = Addresses::new(s);
    let mut slots: BTreeMap<Range, BTreeSet<ValueId>> = BTreeMap::new();
    for (&id, access) in &accesses {
        if s.value(id).op != Op::P(OpCode::Store) {
            continue;
        }
        let Some(range) = access.location else {
            continue;
        };
        let Address::Known {
            base: Base::Frame,
            offset,
            span: 0,
        } = range.address
        else {
            continue;
        };
        let start = i64::from(offset as i32);
        if !matches!(range.size, 1 | 2 | 4 | 8) || start >= 0 || start + i64::from(range.size) > 0 {
            continue;
        }
        let Address::Known {
            base: Base::Frame,
            offset: esp,
            span: 0,
        } = addresses.state(id, Field::Esp)
        else {
            continue;
        };
        if i64::from(esp as i32) <= start {
            slots.entry(range).or_default().insert(id);
        }
    }
    let ranges: Vec<_> = slots.keys().copied().collect();
    slots.retain(|range, _| {
        ranges
            .iter()
            .all(|other| range == other || range.disjoint(*other))
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
    let mut changed = false;
    let mut constants = BTreeMap::new();
    let esp: Vec<_> = rk_ir::lanes(Field::Esp)
        .iter()
        .map(|&key| s.input(key).unwrap())
        .collect();
    let frame = s.new_value(Op::Pack, 4, &esp, 0);
    s.blocks[s.entries[0].target as usize].ops.insert(0, frame);
    for (range, writes) in slots {
        let mut actions = BTreeMap::new();
        for &id in s.blocks.iter().flat_map(|block| &block.ops) {
            let op = s.value(id).op;
            let action = if writes.contains(&id) {
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
        let transfer = |id: ValueId, state: u8| match actions.get(&id) {
            Some(Action::Write) => {
                if stores.contains(&id) {
                    PENDING
                } else {
                    CLEAN
                }
            }
            Some(Action::Settle) => CLEAN,
            _ => state,
        };
        let mut outputs = vec![0; s.blocks.len()];
        loop {
            let mut changed = false;
            for (block, item) in s.blocks.iter().enumerate() {
                let mut state = incoming(s, block, &outputs);
                for &id in &item.ops {
                    state = transfer(id, state);
                }
                if state != outputs[block] {
                    outputs[block] = state;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mixed = s.blocks.iter().enumerate().any(|(block, item)| {
            let mut state = incoming(s, block, &outputs);
            for &id in &item.ops {
                if actions.get(&id) == Some(&Action::Settle) && state == (CLEAN | PENDING) {
                    return true;
                }
                state = transfer(id, state);
            }
            false
        });
        if mixed {
            continue;
        }
        changed = true;
        let zero = s.constant(&mut constants, range.size as u8, 0);
        let Address::Known { offset, .. } = range.address else {
            unreachable!()
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
                if actions.get(&id) == Some(&Action::Settle) && state == PENDING {
                    let addr = s.value_addr[id as usize];
                    let address = s.new_value(Op::P(OpCode::IntAdd), 4, &[frame, offset], addr);
                    let flush = s.new_value(
                        Op::P(OpCode::Store),
                        0,
                        &[s.memory().unwrap(), address, value],
                        addr,
                    );
                    ops.extend([address, flush]);
                }
                state = transfer(id, state);
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
