use std::collections::{BTreeMap, BTreeSet};

use rk_core::ir::OpCode;
use rk_core::memory::{Address, Base, Range};
use rk_core::{Lookup, LookupSet};
use rk_ir::x87::X87Op;
use rk_ir::Op;

use crate::canonical::{canonicalize, rewrite_aliases};
use crate::memory::{Clobber, Frame, Link};
use crate::{CallFacts, Ssa, ValueId};

fn located(frame: &Frame, s: &Ssa, load: ValueId) -> Option<Range> {
    let width = match s.value(load).op {
        Op::P(OpCode::Load) => u32::from(s.value(load).size),
        Op::X87(X87Op::PushInteger {
            bits: bits @ (16 | 32 | 64),
        }) => u32::from(bits / 8),
        _ => return None,
    };
    let address = frame.accesses.get(&load)?.location?.address;
    address.is_exact().then(|| Range::new(address, width))
}

pub(crate) fn dump_dependencies(
    s: &Ssa,
    frame: &Frame,
    out: &mut (impl std::io::Write + ?Sized),
) -> std::io::Result<()> {
    for &load in frame.accesses.keys() {
        let Some(range) = located(frame, s, load).filter(|range| {
            matches!(
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
        let mut blockers = BTreeSet::new();
        while let Some(token) = pending.pop() {
            if !seen.insert(token) {
                continue;
            }
            if seen.len() > 4096 {
                blockers.insert(token);
                break;
            }
            match frame.link(s, token, range) {
                Link::Phi(args) | Link::Merge(args) => pending.extend_from_slice(args),
                Link::Effect(_, Clobber::Value(..), _) => {}
                Link::Effect(_, Clobber::Transparent, prior) => pending.push(prior),
                Link::Effect(effect, Clobber::May, _) => {
                    blockers.insert(effect);
                }
                Link::End => {
                    blockers.insert(token);
                }
            }
        }
        writeln!(out, "load-dependencies {} v{load} range={range:?} guard-address-supported={} blockers={blockers:?}",
            rk_core::Hex(s.value_addr[load as usize]),
            matches!(frame.relations.bounds(range.address), Address::Known { base: Base::Frame, .. }))?;
        for blocker in blockers {
            writeln!(
                out,
                "load-blocker v{load} {} v{blocker} {:?} writes={:?}",
                rk_core::Hex(s.value_addr[blocker as usize]),
                s.value(blocker).op,
                frame.accesses.get(&blocker).map(|access| &access.writes)
            )?;
        }
    }
    Ok(())
}

pub fn forward(s: &mut Ssa, call_facts: Option<&dyn CallFacts>) -> bool {
    let frame = Frame::new(s, call_facts);
    let loads: Vec<(ValueId, Range)> = frame
        .accesses
        .keys()
        .filter_map(|&load| Some((load, located(&frame, s, load)?)))
        .collect();
    crate::memory::partition(s, &frame);
    let owners: Lookup<ValueId, usize> = s
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(block, item)| item.phis.iter().map(move |&phi| (phi, block)))
        .collect();
    let mut alias: Lookup<ValueId, ValueId> = Lookup::default();
    let mut integers = Vec::new();
    let mut proven = Lookup::default();
    for (load, target) in loads {
        let old_len = s.values.len();
        let old_args = s.args.len();
        let token = s.args(load)[0];
        let mut projection = Values {
            s,
            frame: &frame,
            owners: &owners,
            target,
            load,
            proven: &proven,
            memo: BTreeMap::new(),
            phis: Vec::new(),
            anchored: false,
            parts: BTreeMap::new(),
        };
        let saved = projection
            .get(token, 0)
            .map(|saved| projection.collapse(saved));
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
                integers.push((load, saved, target.size));
            } else {
                alias.insert(load, saved);
            }
            for (token, value) in projection.memo {
                proven.insert((target, token), value);
            }
        } else {
            projection.s.values.truncate(old_len);
            projection.s.value_addr.truncate(old_len);
            projection.s.args.truncate(old_args);
        }
    }
    if alias.is_empty() && integers.is_empty() {
        return false;
    }
    let forwarded: LookupSet<ValueId> = integers.iter().map(|&(load, _, _)| load).collect();
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
    frame: &'a Frame,
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
    fn collapse(&mut self, result: ValueId) -> ValueId {
        let created: BTreeSet<ValueId> = self.phis.iter().map(|&(phi, _)| phi).collect();
        let mut alias = BTreeMap::new();
        for &phi in &created {
            let mut seen = BTreeSet::from([phi]);
            let mut pending = vec![phi];
            let mut external = None;
            while let Some(next) = pending.pop() {
                for &arg in self.s.args(next) {
                    if created.contains(&arg) {
                        if seen.insert(arg) {
                            pending.push(arg);
                        }
                    } else if external.is_some_and(|value| value != arg) {
                        external = None;
                        pending.clear();
                        break;
                    } else {
                        external = Some(arg);
                    }
                }
            }
            if let Some(value) = external {
                alias.insert(phi, value);
            }
        }
        if alias.is_empty() {
            return result;
        }
        self.phis.retain(|(phi, _)| !alias.contains_key(phi));
        for &(phi, _) in &self.phis {
            let value = self.s.values[phi as usize];
            for arg in &mut self.s.args
                [value.args_start as usize..(value.args_start + value.args_len) as usize]
            {
                *arg = alias.get(arg).copied().unwrap_or(*arg);
            }
        }
        for value in self.memo.values_mut() {
            *value = alias.get(value).copied().unwrap_or(*value);
        }
        alias.get(&result).copied().unwrap_or(result)
    }

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
        let result = match self.frame.link(self.s, token, self.target) {
            Link::Phi(args) => {
                let args = args.to_vec();
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
            Link::Merge(args) => {
                let args = args.to_vec();
                let mut values = args.into_iter().map(|arg| self.get(arg, depth + 1));
                let first = values.next()??;
                for next in values {
                    if next? != first {
                        return None;
                    }
                }
                first
            }
            Link::Effect(effect, Clobber::Value(candidate, offset), _) => {
                self.anchored |= candidate != self.load;
                if offset == 0 && u32::from(self.s.value(candidate).size) == self.target.size {
                    candidate
                } else {
                    let addr = self.s.value_addr[self.load as usize];
                    let offset = self.s.new_value(Op::Const(u64::from(offset)), 4, &[], addr);
                    let part = self.s.new_value(
                        Op::P(OpCode::SubPiece),
                        self.target.size as u8,
                        &[candidate, offset],
                        addr,
                    );
                    self.parts.entry(effect).or_default().push(part);
                    part
                }
            }
            Link::Effect(_, Clobber::Transparent, prior) => self.get(prior, depth + 1)?,
            Link::Effect(_, Clobber::May, _) | Link::End => return None,
        };
        self.memo.insert(token, result);
        Some(result)
    }
}
