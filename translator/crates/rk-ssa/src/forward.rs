use rk_core::ir::OpCode;
use rk_core::{Lookup, LookupSet};
use rk_ir::x87::{MemoryEffect, X87Op};
use rk_ir::Op;

use crate::address::Addresses;
use crate::canonical::{canonicalize, rewrite_aliases};
use crate::{CallFacts, Ssa, ValueId};

pub fn forward(s: &mut Ssa, call_facts: Option<&dyn CallFacts>) -> bool {
    crate::memory::partition(s, call_facts);
    let mut addresses = Addresses::new(s);
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
