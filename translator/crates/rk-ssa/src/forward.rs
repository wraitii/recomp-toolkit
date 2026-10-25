use std::collections::BTreeMap;

use rk_core::ir::OpCode;
use rk_core::memory::{Address, Base, Range};
use rk_core::{Lookup, LookupSet};
use rk_ir::x87::{MemoryEffect, X87Op};
use rk_ir::Op;

use crate::address::{Addresses, Relations};
use crate::canonical::{canonicalize, rewrite_aliases};
use crate::{CallFacts, Ssa, ValueId};

fn crosses_call(
    s: &Ssa,
    load: ValueId,
    locations: &BTreeMap<ValueId, crate::memory::Access>,
) -> bool {
    let Some(target) = locations.get(&load).and_then(|access| access.location) else {
        return false;
    };
    let mut pending = vec![s.args(load)[0]];
    let mut visited = LookupSet::default();
    while let Some(token) = pending.pop() {
        if !visited.insert(token) {
            continue;
        }
        if visited.len() > 4096 {
            return true;
        }
        match s.value(token).op {
            Op::Phi => pending.extend_from_slice(s.args(token)),
            Op::Memory => {
                let args = s.args(token);
                let Some(&effect) = args.first() else {
                    continue;
                };
                if matches!(s.value(effect).op, Op::Call { .. } | Op::CallInd) {
                    return true;
                }
                if effect != load
                    && matches!(s.value(effect).op, Op::P(OpCode::Load | OpCode::Store))
                    && locations.get(&effect).and_then(|access| access.location) == Some(target)
                {
                    continue;
                }
                if rk_ir::properties(s.value(effect).op).memory {
                    pending.push(s.args(effect)[0]);
                } else {
                    pending.extend_from_slice(args);
                }
            }
            _ => {}
        }
    }
    false
}

pub(crate) fn dump_dependencies(
    s: &Ssa,
    facts: &dyn CallFacts,
    out: &mut (impl std::io::Write + ?Sized),
) -> std::io::Result<()> {
    let locations = crate::memory::local_accesses(s, Some(facts));
    let private = crate::memory::PrivateFrame::new(s, &locations, Some(facts));
    let mut addresses = Addresses::local(s);
    for &id in locations.keys() {
        if locations[&id].location.is_some() {
            addresses.get(s.args(id)[1]);
        }
    }
    for (&load, access) in &locations {
        if !matches!(
            s.value(load).op,
            Op::P(OpCode::Load) | Op::X87(X87Op::PushInteger { .. })
        ) {
            continue;
        }
        let Some(range) = access.location.filter(|range| {
            range.address.is_exact()
                && matches!(
                    range.address,
                    Address::Known {
                        base: Base::Frame | Base::Stack(_),
                        ..
                    }
                )
        }) else {
            continue;
        };
        let mut pending = vec![s.args(load)[0]];
        let mut seen = LookupSet::default();
        let mut blockers = std::collections::BTreeSet::new();
        while let Some(token) = pending.pop() {
            if !seen.insert(token) {
                continue;
            }
            if seen.len() > 4096 {
                blockers.insert(token);
                break;
            }
            match s.value(token).op {
                Op::Phi => pending.extend_from_slice(s.args(token)),
                Op::Memory => {
                    let args = s.args(token);
                    let Some(&effect) = args.first() else {
                        blockers.insert(token);
                        continue;
                    };
                    if let Some(access) = locations.get(&effect) {
                        if s.value(effect).op == Op::P(OpCode::Store)
                            && access.location == Some(range)
                        {
                            continue;
                        }
                        if !addresses.relations.excludes(&access.writes, range)
                            && !private.disjoint(effect, range)
                        {
                            blockers.insert(effect);
                            continue;
                        }
                        pending.push(args.get(1).copied().unwrap_or(s.args(effect)[0]));
                    } else {
                        pending.extend_from_slice(args);
                    }
                }
                _ => {
                    blockers.insert(token);
                }
            }
        }
        writeln!(out, "load-dependencies {} v{load} range={range:?} crosses-call={} guard-address-supported={} blockers={blockers:?}",
            rk_core::Hex(s.value_addr[load as usize]), crosses_call(s, load, &locations),
            matches!(addresses.relations.bounds(range.address), Address::Known { base: Base::Frame, .. }))?;
        for blocker in blockers {
            writeln!(
                out,
                "load-blocker v{load} {} v{blocker} {:?} writes={:?}",
                rk_core::Hex(s.value_addr[blocker as usize]),
                s.value(blocker).op,
                locations.get(&blocker).map(|access| &access.writes)
            )?;
        }
    }
    Ok(())
}

pub fn forward(s: &mut Ssa, call_facts: Option<&dyn CallFacts>) -> bool {
    let calls = s.assume_private_stack
        && s.blocks
            .iter()
            .flat_map(|block| &block.ops)
            .any(|&id| matches!(s.value(id).op, Op::Call { .. } | Op::CallInd));
    let guards: LookupSet<_> = if calls {
        let locations = crate::memory::local_accesses(s, call_facts);
        s.blocks
            .iter()
            .flat_map(|block| &block.ops)
            .filter(|&&id| {
                matches!(
                    s.value(id).op,
                    Op::P(OpCode::Load) | Op::X87(X87Op::PushInteger { .. })
                )
            })
            .filter(|&&id| crosses_call(s, id, &locations))
            .copied()
            .collect()
    } else {
        LookupSet::default()
    };
    crate::memory::partition(s, call_facts);
    let mut addresses = Addresses::local(s);
    let locations: Lookup<ValueId, Address> = s
        .blocks
        .iter()
        .flat_map(|block| &block.ops)
        .filter(|&&id| matches!(s.value(id).op, Op::P(OpCode::Load | OpCode::Store)))
        .map(|&id| (id, addresses.get(s.args(id)[1])))
        .collect();
    let guardable: LookupSet<_> = s
        .blocks
        .iter()
        .flat_map(|block| &block.ops)
        .filter(|&&id| {
            locations.get(&id).is_some_and(|&address| {
                matches!(
                    addresses.relations.bounds(address),
                    Address::Known {
                        base: Base::Frame,
                        ..
                    }
                )
            })
        })
        .copied()
        .collect();
    let mut candidates = Vec::new();
    let mut alias: Lookup<ValueId, ValueId> = Lookup::default();
    let mut integers = Vec::new();
    for block in &s.blocks {
        for &load in &block.ops {
            let width = match s.value(load).op {
                Op::P(OpCode::Load) => s.value(load).size,
                Op::X87(X87Op::PushInteger {
                    bits: bits @ (16 | 32 | 64),
                }) => (bits / 8) as u8,
                _ => continue,
            };
            let target = addresses.get(s.args(load)[1]);
            if !target.is_exact() {
                continue;
            }
            let mut pending = vec![s.args(load)[0]];
            let mut visited = LookupSet::default();
            let mut saved = None;
            let mut failed = false;
            while let Some(token) = pending.pop() {
                if !visited.insert(token) {
                    continue;
                }
                if visited.len() > 4096 {
                    failed = true;
                    break;
                }
                match s.value(token).op {
                    Op::Phi => pending.extend_from_slice(s.args(token)),
                    Op::Memory => {
                        let args = s.args(token);
                        let Some(&effect) = args.first() else {
                            failed = true;
                            break;
                        };
                        let candidate = match s.value(effect).op {
                            Op::P(OpCode::Store) => {
                                let stored = s.args(effect)[2];
                                (s.value(stored).size == width
                                    && addresses.get(s.args(effect)[1]) == target)
                                    .then_some(stored)
                            }
                            Op::P(OpCode::Load) => {
                                if s.value(effect).size == width
                                    && addresses.get(s.args(effect)[1]) == target
                                {
                                    Some(effect)
                                } else {
                                    pending.push(args.get(1).copied().unwrap_or(s.args(effect)[0]));
                                    continue;
                                }
                            }
                            Op::X87(op) if op.memory() == Some(MemoryEffect::Read) => {
                                pending.push(args.get(1).copied().unwrap_or(s.args(effect)[0]));
                                continue;
                            }
                            Op::Memory | Op::Phi | Op::Input(_) => {
                                pending.extend_from_slice(args);
                                continue;
                            }
                            _ => None,
                        };
                        let Some(candidate) = candidate else {
                            failed = true;
                            break;
                        };
                        if candidate == load || saved.is_some_and(|prior| prior != candidate) {
                            failed = true;
                            break;
                        }
                        saved = Some(candidate);
                    }
                    _ => {
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                candidates.push((load, target, width));
            }
            if !failed {
                if let Some(saved) = saved {
                    if matches!(s.value(load).op, Op::X87(_)) {
                        integers.push((load, saved, width));
                    } else {
                        alias.insert(load, saved);
                    }
                }
            }
        }
    }
    let relations = addresses.relations;
    let owners: Lookup<ValueId, usize> = s
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(block, item)| item.phis.iter().map(move |&phi| (phi, block)))
        .collect();
    let mut proven = Lookup::default();
    for (load, target, width) in candidates {
        let old_len = s.values.len();
        let old_args = s.args.len();
        let token = s.args(load)[0];
        let mut projection = Values {
            s,
            locations: &locations,
            relations: &relations,
            owners: &owners,
            target: Range::new(target, u32::from(width)),
            load,
            proven: &proven,
            memo: BTreeMap::new(),
            phis: Vec::new(),
            anchored: false,
            parts: BTreeMap::new(),
        };
        let saved = projection.get(token, 0);
        if let Some(saved) = saved.filter(|&saved| saved != load && projection.anchored) {
            if !projection.parts.is_empty() {
                for block in &mut projection.s.blocks {
                    block.ops = block
                        .ops
                        .iter()
                        .flat_map(|&id| {
                            std::iter::once(id)
                                .chain(projection.parts.get(&id).into_iter().flatten().copied())
                        })
                        .collect();
                }
            }
            for (phi, block) in projection.phis {
                projection.s.blocks[block].phis.push(phi);
            }
            if matches!(projection.s.value(load).op, Op::X87(_)) {
                integers.push((load, saved, width));
            } else {
                alias.insert(load, saved);
            }
            let range = projection.target;
            for (token, value) in projection.memo {
                proven.insert((range, token), value);
            }
        } else {
            projection.s.values.truncate(old_len);
            projection.s.value_addr.truncate(old_len);
            projection.s.args.truncate(old_args);
        }
    }
    integers.retain(|&(load, _, _)| !guards.contains(&load));
    for &load in s.blocks.iter().flat_map(|block| &block.ops) {
        if guards.contains(&load) && !guardable.contains(&load) {
            alias.remove(&load);
        }
    }
    if alias.is_empty() && integers.is_empty() {
        return false;
    }
    if calls {
        let guarded: Vec<_> = s
            .blocks
            .iter()
            .flat_map(|block| &block.ops)
            .filter_map(|&load| {
                let &saved = alias.get(&load)?;
                if !guards.contains(&load) {
                    return None;
                }
                guardable.contains(&load).then_some((load, saved))
            })
            .collect();
        for (load, saved) in guarded {
            let mut args = s.args(load).to_vec();
            args.push(saved);
            let start = s.args.len() as u32;
            s.args.extend(args);
            s.values[load as usize].op = Op::StackLoad;
            s.values[load as usize].args_start = start;
            s.values[load as usize].args_len = 3;
            alias.remove(&load);
        }
    }
    let mut forwarded: LookupSet<ValueId> = LookupSet::default();
    for &(load, _, _) in &integers {
        forwarded.insert(load);
    }
    for index in 0..s.values.len() {
        if s.values[index].op != Op::Memory {
            continue;
        }
        let args = s.args(index as ValueId);
        let Some(&effect) = args.first() else {
            continue;
        };
        if alias.contains_key(&effect) || forwarded.contains(&effect) {
            let memory = args.get(1).copied().unwrap_or(s.args(effect)[0]);
            alias.insert(index as ValueId, memory);
            let start = s.values[index].args_start as usize;
            s.args[start] = memory;
            s.values[index].args_len = 1;
        }
    }
    for (load, saved, width) in integers {
        let start = s.args.len() as u32;
        s.args.push(saved);
        let value = &mut s.values[load as usize];
        value.op = Op::X87(X87Op::PushIntegerValue {
            bits: width as u16 * 8,
        });
        value.args_start = start;
        value.args_len = 1;
    }
    rewrite_aliases(s, &alias);
    canonicalize(s, true);
    true
}

struct Values<'a> {
    s: &'a mut Ssa,
    locations: &'a Lookup<ValueId, Address>,
    relations: &'a Relations,
    owners: &'a Lookup<ValueId, usize>,
    target: Range,
    load: ValueId,
    proven: &'a Lookup<(Range, ValueId), ValueId>,
    memo: BTreeMap<ValueId, ValueId>,
    phis: Vec<(ValueId, usize)>,
    parts: BTreeMap<ValueId, Vec<ValueId>>,
    anchored: bool,
}

impl Values<'_> {
    fn get(&mut self, token: ValueId, depth: usize) -> Option<ValueId> {
        if let Some(&value) = self.proven.get(&(self.target, token)) {
            self.anchored = true;
            return Some(value);
        }
        if let Some(&value) = self.memo.get(&token) {
            return Some(value);
        }
        if depth >= 256 || self.memo.len() >= 4096 {
            return None;
        }
        let args = self.s.args(token).to_vec();
        let result = match self.s.value(token).op {
            Op::Phi => {
                let block = *self.owners.get(&token)?;
                if args.is_empty() {
                    return None;
                }
                let phi = self.s.new_value(
                    Op::Phi,
                    self.target.size as u8,
                    &[],
                    self.s.value_addr[token as usize],
                );
                self.memo.insert(token, phi);
                self.phis.push((phi, block));
                let inputs: Option<Vec<_>> = args
                    .into_iter()
                    .map(|arg| self.get(arg, depth + 1))
                    .collect();
                let inputs = inputs?;
                let start = self.s.args.len() as u32;
                self.s.args.extend_from_slice(&inputs);
                self.s.values[phi as usize].args_start = start;
                self.s.values[phi as usize].args_len = inputs.len() as u32;
                phi
            }
            Op::Memory => {
                let &effect = args.first()?;
                match self.s.value(effect).op {
                    Op::P(OpCode::Store | OpCode::Load) => {
                        let store = self.s.value(effect).op == Op::P(OpCode::Store);
                        let candidate = if store {
                            self.s.args(effect)[2]
                        } else {
                            effect
                        };
                        let part = self.locations.get(&effect).and_then(|&address| {
                            self.relations.offset_of(
                                Range::new(address, u32::from(self.s.value(candidate).size)),
                                self.target,
                            )
                        });
                        if let Some(offset) = part {
                            self.anchored |= candidate != self.load;
                            if offset == 0
                                && u32::from(self.s.value(candidate).size) == self.target.size
                            {
                                candidate
                            } else {
                                let addr = self.s.value_addr[self.load as usize];
                                let offset =
                                    self.s.new_value(Op::Const(u64::from(offset)), 4, &[], addr);
                                let part = self.s.new_value(
                                    Op::P(OpCode::SubPiece),
                                    self.target.size as u8,
                                    &[candidate, offset],
                                    addr,
                                );
                                self.parts.entry(effect).or_default().push(part);
                                part
                            }
                        } else if !store {
                            self.get(
                                args.get(1).copied().unwrap_or(self.s.args(effect)[0]),
                                depth + 1,
                            )?
                        } else {
                            return None;
                        }
                    }
                    Op::X87(op) if op.memory() == Some(MemoryEffect::Read) => self.get(
                        args.get(1).copied().unwrap_or(self.s.args(effect)[0]),
                        depth + 1,
                    )?,
                    Op::Memory | Op::Phi | Op::Input(_) => {
                        let mut values = args.into_iter().map(|arg| self.get(arg, depth + 1));
                        let first = values.next()??;
                        for next in values {
                            if next? != first {
                                return None;
                            }
                        }
                        first
                    }
                    _ => return None,
                }
            }
            _ => return None,
        };
        self.memo.insert(token, result);
        Some(result)
    }
}
