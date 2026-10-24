use std::collections::BTreeMap;

use rk_core::ir::OpCode;
use rk_core::memory::{Address, Base, Range};
use rk_core::{Lookup, LookupSet};
use rk_ir::x87::{MemoryEffect, X87Op};
use rk_ir::Op;

use crate::address::Addresses;
use crate::canonical::{canonicalize, rewrite_aliases};
use crate::{CallFacts, Ssa, ValueId};

pub fn forward(s: &mut Ssa, call_facts: Option<&dyn CallFacts>) -> bool {
    crate::memory::partition(s, call_facts);
    let mut addresses = Addresses::new(s);
    let locations: Lookup<ValueId, Address> = s
        .blocks
        .iter()
        .flat_map(|block| &block.ops)
        .filter(|&&id| matches!(s.value(id).op, Op::P(OpCode::Load | OpCode::Store)))
        .map(|&id| (id, addresses.get(s.args(id)[1])))
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
            if failed
                && matches!(
                    target,
                    Address::Known {
                        base: Base::Frame | Base::Absolute,
                        ..
                    }
                )
            {
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
            owners: &owners,
            target: Range::new(target, u32::from(width)),
            load,
            proven: &proven,
            memo: BTreeMap::new(),
            phis: Vec::new(),
            anchored: false,
        };
        let saved = projection.get(token, 0);
        if let Some(saved) = saved.filter(|&saved| saved != load && projection.anchored) {
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
    if alias.is_empty() && integers.is_empty() {
        return false;
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
    owners: &'a Lookup<ValueId, usize>,
    target: Range,
    load: ValueId,
    proven: &'a Lookup<(Range, ValueId), ValueId>,
    memo: BTreeMap<ValueId, ValueId>,
    phis: Vec<(ValueId, usize)>,
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
                        if self.s.value(candidate).size as u32 == self.target.size
                            && self.locations.get(&effect) == Some(&self.target.address)
                        {
                            self.anchored |= candidate != self.load;
                            candidate
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
