use std::collections::BTreeMap;

use rk_core::ir::OpCode;
use rk_core::memory::{Address, Base, Range};
use rk_core::{Lookup, LookupSet};
use rk_ir::{x87::X87Op, Op};

use crate::memory::{Clobber, Frame};
use crate::{Ssa, ValueId};

const SLOT_BUDGET: usize = 64;

#[derive(Default)]
pub(crate) struct FramePlan {
    pub(crate) slots: Vec<(i32, u8)>,
    pub(crate) accesses: BTreeMap<(u32, u16), (usize, u8)>,
    pub(crate) analyzed: LookupSet<(u32, u16)>,
    pub(crate) sites: BTreeMap<(u32, u16), Site>,
    pub(crate) eager: LookupSet<(u32, u16)>,
}

#[derive(Default)]
pub(crate) struct Site {
    pub(crate) publish: Vec<usize>,
    pub(crate) guard: Vec<usize>,
    pub(crate) clobber: Vec<usize>,
    pub(crate) checkpoints: Option<u32>,
}

fn promotable(op: Op) -> bool {
    matches!(
        op,
        Op::P(OpCode::Load | OpCode::Store)
            | Op::X87(
                X87Op::Push {
                    src: None,
                    bits: 32 | 64
                } | X87Op::PushInteger { bits: 16 | 32 | 64 }
                    | X87Op::Store {
                        dst: None,
                        bits: 32 | 64,
                        ..
                    }
                    | X87Op::Store {
                        dst: None,
                        bits: 16,
                        integer: true,
                        ..
                    }
            )
    )
}

fn writes(op: Op) -> bool {
    matches!(op, Op::P(OpCode::Store) | Op::X87(X87Op::Store { .. }))
}

pub(crate) fn plan(s: &Ssa, facts: &dyn crate::CallFacts, retire: bool) -> Option<FramePlan> {
    let frame = Frame::new(s, Some(facts));
    if !frame.eligible() {
        return None;
    }
    if s.successors().iter().enumerate().any(|(block, next)| {
        next.is_empty()
            && s.blocks[block]
                .ops
                .last()
                .is_none_or(|&id| s.value(id).op != Op::Return)
    }) {
        return None;
    }
    let local = |range: Range| match frame.relations.bounds(range.address) {
        Address::Known {
            base: Base::Frame,
            offset,
            span: 0,
        } if i64::from(offset as i32) + i64::from(range.size) <= 0 && frame.unexposed(range) => {
            Some(offset as i32)
        }
        _ => None,
    };
    let top = |id: ValueId| match frame.esp.get(&id).map(|&esp| frame.relations.bounds(esp)) {
        Some(Address::Known {
            base: Base::Frame,
            offset,
            span: 0,
        }) => Some(offset as i32),
        _ => None,
    };
    let mut candidates: BTreeMap<(i32, u8), usize> = BTreeMap::new();
    for (&id, access) in &frame.accesses {
        let op = s.value(id).op;
        let Some(range) = access
            .location
            .filter(|range| promotable(op) && writes(op) && matches!(range.size, 1 | 2 | 4 | 8))
        else {
            continue;
        };
        let Some(start) = local(range) else { continue };
        if top(id).is_some_and(|top| top <= start) {
            *candidates.entry((start, range.size as u8)).or_default() += 1;
        }
    }
    let overlaps =
        |(a, n): (i32, u8), (b, m): (i32, u8)| a < b + i32::from(m) && b < a + i32::from(n);
    let mut slots: Vec<(i32, u8)> = candidates
        .keys()
        .copied()
        .filter(|&slot| {
            candidates
                .keys()
                .all(|&other| other == slot || !overlaps(slot, other))
        })
        .collect();
    let range = |(start, size): (i32, u8)| {
        Range::new(Address::exact(Base::Frame, start as u32), u32::from(size))
    };
    let mut rejected = vec![false; slots.len()];
    let mut contained = BTreeMap::new();
    let mut overlaps = Vec::new();
    let mut analyzed = LookupSet::default();
    for (&id, access) in &frame.accesses {
        let op = s.value(id).op;
        let Some(&origin) = s.origins.get(&id) else {
            return None;
        };
        analyzed.insert(origin);
        if matches!(op, Op::Call { .. } | Op::CallInd) {
            continue;
        }
        let location = access
            .location
            .filter(|_| promotable(op))
            .and_then(|location| Some((local(location)?, location.size as i32)));
        for (index, &slot) in slots.iter().enumerate() {
            let (start, size) = (slot.0, i32::from(slot.1));
            if let Some((offset, _)) = location
                .filter(|&(offset, width)| offset >= start && offset + width <= start + size)
            {
                if writes(op) && top(id).is_none_or(|top| top > offset) {
                    rejected[index] = true;
                }
                contained.insert(origin, (index, (offset - start) as u8));
            } else if !frame.disjoint(id, range(slot)) {
                let reads = !frame.relations.excludes(&access.reads, range(slot));
                let written = !frame.relations.excludes(&access.writes, range(slot));
                if reads || written {
                    overlaps.push((origin, index, written));
                }
            }
        }
    }
    let mut uses = vec![0usize; slots.len()];
    for &(index, _) in contained.values() {
        uses[index] += 1;
    }
    let mut order: Vec<usize> = (0..slots.len()).filter(|&index| !rejected[index]).collect();
    order.sort_by_key(|&index| std::cmp::Reverse(uses[index]));
    order.truncate(SLOT_BUDGET);
    order.sort_unstable();
    if order.is_empty() || order.len().saturating_mul(s.blocks.len()) > 100_000 {
        return None;
    }
    let renumber: BTreeMap<usize, usize> = order
        .iter()
        .enumerate()
        .map(|(new, &old)| (old, new))
        .collect();
    slots = order.iter().map(|&index| slots[index]).collect();
    let accesses = contained
        .into_iter()
        .filter_map(|(origin, (index, offset))| Some((origin, (*renumber.get(&index)?, offset))))
        .collect();
    let mut sites: BTreeMap<(u32, u16), Site> = BTreeMap::new();
    for (origin, index, written) in overlaps {
        let Some(&index) = renumber.get(&index) else {
            continue;
        };
        let site = sites.entry(origin).or_default();
        site.publish.push(index);
        if written {
            site.clobber.push(index);
        }
    }
    for &id in s.blocks.iter().flat_map(|block| &block.ops) {
        let op = s.value(id).op;
        if !matches!(op, Op::Call { .. } | Op::CallInd | Op::Return) {
            continue;
        }
        let Some(&origin) = s.origins.get(&id) else {
            return None;
        };
        let top = top(id);
        let mut site = Site::default();
        for (index, &slot) in slots.iter().enumerate() {
            let below = top.is_none_or(|top| slot.0 < top);
            if op == Op::Return {
                if !retire || !below {
                    site.publish.push(index);
                }
                continue;
            }
            if below || frame.clobber(s, id, range(slot)) != Clobber::Transparent {
                site.clobber.push(index);
            }
            if !below
                && frame.call_checkpoints(id).is_some()
                && frame.call_disjoint(id, range(slot))
            {
                site.guard.push(index);
            } else {
                site.publish.push(index);
            }
        }
        if !site.guard.is_empty() {
            site.checkpoints = frame.call_checkpoints(id);
        }
        sites.insert(origin, site);
    }
    let mut plan = FramePlan {
        slots,
        accesses,
        analyzed,
        sites,
        eager: LookupSet::default(),
    };
    let next = s.successors();
    let table = events(s, &frame, &plan);
    let mut crossing = 0;
    solve(
        s,
        &next,
        &table,
        false,
        |state, [uses, defs, demands, clobbers]| (state & !defs & !clobbers) | uses | demands,
        |id, after| crossing |= table.get(&id).map_or(0, |events| events[3]) & after,
    );
    if crossing != 0 {
        plan.retain(|index| crossing & 1 << index == 0);
        if plan.slots.is_empty() {
            return None;
        }
    }
    let table = events(s, &frame, &plan);
    let mut eager = LookupSet::default();
    solve(
        s,
        &next,
        &table,
        true,
        |state, [_, defs, demands, _]| (state & !defs) | demands,
        |id, after| {
            let Some(&origin) = s.origins.get(&id) else {
                return;
            };
            if let Some(&(index, _)) = plan.accesses.get(&origin) {
                if writes(s.value(id).op) && after & 1 << index != 0 {
                    eager.insert(origin);
                }
            }
        },
    );
    plan.eager = eager;
    Some(plan)
}

impl FramePlan {
    fn retain(&mut self, keep: impl Fn(usize) -> bool) {
        let renumber: BTreeMap<usize, usize> = (0..self.slots.len())
            .filter(|&index| keep(index))
            .enumerate()
            .map(|(new, old)| (old, new))
            .collect();
        self.slots = renumber.keys().map(|&old| self.slots[old]).collect();
        self.accesses
            .retain(|_, (slot, _)| renumber.get(slot).map(|&new| *slot = new).is_some());
        let remap = |list: &mut Vec<usize>| {
            *list = list
                .iter()
                .filter_map(|slot| renumber.get(slot).copied())
                .collect()
        };
        for site in self.sites.values_mut() {
            remap(&mut site.publish);
            remap(&mut site.guard);
            remap(&mut site.clobber);
            if site.guard.is_empty() {
                site.checkpoints = None;
            }
        }
    }
}

fn events(s: &Ssa, frame: &Frame, plan: &FramePlan) -> Lookup<ValueId, [u64; 4]> {
    let mask = |list: &[usize]| list.iter().fold(0u64, |mask, &slot| mask | 1 << slot);
    let mut table = Lookup::default();
    for &id in s.blocks.iter().flat_map(|block| &block.ops) {
        let Some(origin) = s.origins.get(&id) else {
            continue;
        };
        let mut events = [0u64; 4];
        if let Some(&(slot, offset)) = plan.accesses.get(origin) {
            let width = frame
                .accesses
                .get(&id)
                .and_then(|access| access.location)
                .map_or(0, |location| location.size);
            let full =
                writes(s.value(id).op) && offset == 0 && width == u32::from(plan.slots[slot].1);
            events[usize::from(full)] |= 1 << slot;
        }
        if let Some(site) = plan.sites.get(origin) {
            events[2] = mask(&site.publish);
            events[3] = mask(&site.clobber);
        }
        if events != [0; 4] {
            table.insert(id, events);
        }
    }
    table
}

fn solve(
    s: &Ssa,
    next: &[Vec<usize>],
    table: &Lookup<ValueId, [u64; 4]>,
    must: bool,
    transfer: impl Fn(u64, [u64; 4]) -> u64,
    mut visit: impl FnMut(ValueId, u64),
) {
    let exit = |entry: &[u64], block: usize| {
        if must {
            if next[block].is_empty() {
                0
            } else {
                next[block]
                    .iter()
                    .fold(u64::MAX, |state, &next| state & entry[next])
            }
        } else {
            next[block]
                .iter()
                .fold(0, |state, &next| state | entry[next])
        }
    };
    let step = |state, id| transfer(state, table.get(&id).copied().unwrap_or_default());
    let mut entry = vec![if must { u64::MAX } else { 0 }; s.blocks.len()];
    loop {
        let mut changed = false;
        for block in (0..s.blocks.len()).rev() {
            let state = s.blocks[block]
                .ops
                .iter()
                .rev()
                .fold(exit(&entry, block), |state, &id| step(state, id));
            if state != entry[block] {
                entry[block] = state;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for block in 0..s.blocks.len() {
        let mut state = exit(&entry, block);
        for &id in s.blocks[block].ops.iter().rev() {
            visit(id, state);
            state = step(state, id);
        }
    }
}

pub(crate) fn split_x87(s: &mut Ssa, facts: &dyn crate::CallFacts) {
    let private = Frame::new(s, Some(facts));
    if !private.eligible() {
        return;
    }
    let mut candidates = BTreeMap::new();
    for (&id, access) in &private.accesses {
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
        let Address::Known {
            base: Base::Frame,
            offset,
            span,
        } = private.relations.bounds(range.address)
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
