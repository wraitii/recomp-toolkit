use std::collections::BTreeMap;

use rk_core::ir::OpCode;
use rk_core::memory::{Address, Base, Range};
use rk_core::{Lookup, LookupSet};
use rk_ir::{x87::X87Op, Op, StateKey};

use crate::memory::{Clobber, Frame};
use crate::{Ssa, ValueId};

const SLOT_BUDGET: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Slot {
    pub(crate) aligned: bool,
    pub(crate) offset: i32,
    pub(crate) size: u8,
}

impl Slot {
    pub(crate) fn key(self) -> StateKey {
        StateKey::Frame {
            offset: self.offset,
            aligned: self.aligned,
        }
    }
}

#[derive(Default)]
pub(crate) struct FramePlan {
    pub(crate) slots: Vec<Slot>,
    pub(crate) aligned: Option<(u32, u32)>,
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
    let parent = |root: ValueId| match frame.relations.parent(Base::Stack(root)) {
        Some(Address::Known {
            base: Base::Frame,
            offset,
            span,
        }) if s.value(root).op == Op::P(OpCode::IntAnd)
            && span.wrapping_add(1).is_power_of_two() =>
        {
            Some((offset as i32, span))
        }
        _ => None,
    };
    let mut roots: BTreeMap<ValueId, usize> = BTreeMap::new();
    for access in frame.accesses.values() {
        if let Some(Range {
            address:
                Address::Known {
                    base: Base::Stack(root),
                    span: 0,
                    ..
                },
            ..
        }) = access.location
        {
            if parent(root).is_some() {
                *roots.entry(root).or_default() += 1;
            }
        }
    }
    let root = roots
        .iter()
        .max_by_key(|&(_, &count)| count)
        .map(|(&root, _)| root);
    let (low, slack) = root
        .and_then(parent)
        .map_or((0, 0), |(low, span)| (i64::from(low), i64::from(span)));
    let position = |address: Address| match address {
        Address::Known {
            base: Base::Stack(base),
            offset,
            span: 0,
        } if Some(base) == root => Some((true, offset as i32)),
        _ => match frame.relations.bounds(address) {
            Address::Known {
                base: Base::Frame,
                offset,
                span: 0,
            } => Some((false, offset as i32)),
            _ => None,
        },
    };
    let interval = |(aligned, offset): (bool, i32)| {
        if aligned {
            (low + i64::from(offset), low + i64::from(offset) + slack)
        } else {
            (i64::from(offset), i64::from(offset))
        }
    };
    let le = |a: (bool, i32), b: (bool, i32)| {
        if a.0 == b.0 {
            a.1 <= b.1
        } else {
            interval(a).1 <= interval(b).0
        }
    };
    let lt = |a: (bool, i32), b: (bool, i32)| {
        if a.0 == b.0 {
            a.1 < b.1
        } else {
            interval(a).1 < interval(b).0
        }
    };
    let local = |range: Range| {
        position(range.address)
            .filter(|&at| interval(at).1 + i64::from(range.size) <= 0 && frame.unexposed(range))
    };
    let top = |id: ValueId| frame.esp.get(&id).and_then(|&esp| position(esp));
    let range = |slot: Slot| {
        Range::new(
            Address::exact(
                if slot.aligned {
                    Base::Stack(root.unwrap())
                } else {
                    Base::Frame
                },
                slot.offset as u32,
            ),
            u32::from(slot.size),
        )
    };
    let mut candidates: BTreeMap<Slot, usize> = BTreeMap::new();
    for (&id, access) in &frame.accesses {
        let op = s.value(id).op;
        let Some(range) = access
            .location
            .filter(|range| promotable(op) && writes(op) && matches!(range.size, 1 | 2 | 4 | 8))
        else {
            continue;
        };
        let Some(start) = local(range) else { continue };
        if top(id).is_some_and(|top| le(top, start)) {
            *candidates
                .entry(Slot {
                    aligned: start.0,
                    offset: start.1,
                    size: range.size as u8,
                })
                .or_default() += 1;
        }
    }
    let mut slots: Vec<Slot> = candidates
        .keys()
        .copied()
        .filter(|&slot| {
            candidates
                .keys()
                .all(|&other| other == slot || frame.relations.disjoint(range(slot), range(other)))
        })
        .collect();
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
            let (start, size) = (slot.offset, i32::from(slot.size));
            if let Some(((_, offset), _)) = location.filter(|&((aligned, offset), width)| {
                aligned == slot.aligned && offset >= start && offset + width <= start + size
            }) {
                if writes(op) && top(id).is_none_or(|top| !le(top, (slot.aligned, offset))) {
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
            let start = (slot.aligned, slot.offset);
            if op == Op::Return {
                if !retire || !top.is_some_and(|top| lt(start, top)) {
                    site.publish.push(index);
                }
                continue;
            }
            let below = !top.is_some_and(|top| le(top, start));
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
    let aligned = root
        .filter(|_| slots.iter().any(|slot| slot.aligned))
        .map(|_| ((low + slack) as u32, !(slack as u32)));
    let mut plan = FramePlan {
        slots,
        aligned,
        accesses,
        analyzed,
        sites,
        eager: LookupSet::default(),
    };
    let table = events(s, &frame, &plan);
    let mut eager = LookupSet::default();
    anticipate(s, &table, |id, after| {
        let Some(&origin) = s.origins.get(&id) else {
            return;
        };
        if let Some(&(index, _)) = plan.accesses.get(&origin) {
            if writes(s.value(id).op) && after & 1 << index != 0 {
                eager.insert(origin);
            }
        }
    });
    plan.eager = eager;
    Some(plan)
}

fn events(s: &Ssa, frame: &Frame, plan: &FramePlan) -> Lookup<ValueId, (u64, u64)> {
    let mut table = Lookup::default();
    for &id in s.blocks.iter().flat_map(|block| &block.ops) {
        let Some(origin) = s.origins.get(&id) else {
            continue;
        };
        let mut defs = 0;
        if let Some(&(slot, offset)) = plan.accesses.get(origin) {
            let width = frame
                .accesses
                .get(&id)
                .and_then(|access| access.location)
                .map_or(0, |location| location.size);
            if writes(s.value(id).op) && offset == 0 && width == u32::from(plan.slots[slot].size) {
                defs = 1 << slot;
            }
        }
        let demands = plan.sites.get(origin).map_or(0, |site| {
            site.publish
                .iter()
                .fold(0u64, |mask, &slot| mask | 1 << slot)
        });
        if defs | demands != 0 {
            table.insert(id, (defs, demands));
        }
    }
    table
}

fn anticipate(s: &Ssa, table: &Lookup<ValueId, (u64, u64)>, mut visit: impl FnMut(ValueId, u64)) {
    let next = s.successors();
    let exit = |entry: &[u64], block: usize| {
        if next[block].is_empty() {
            0
        } else {
            next[block]
                .iter()
                .fold(u64::MAX, |state, &next| state & entry[next])
        }
    };
    let step = |state: u64, id| {
        table
            .get(&id)
            .map_or(state, |&(defs, demands)| (state & !defs) | demands)
    };
    let mut entry = vec![u64::MAX; s.blocks.len()];
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
