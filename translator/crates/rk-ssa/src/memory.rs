use std::collections::{BTreeMap, BTreeSet};

use rk_core::ir::OpCode;
use rk_core::memory::{Address, Base, Effects, Footprint, Range};
use rk_core::{Lookup, LookupSet};
use rk_ir::x87::{MemoryEffect, X87Op};
use rk_ir::Op;

use crate::address::Addresses;
use crate::{CallFacts, Ssa, ValueId};

#[derive(Clone, Debug)]
pub struct Access {
    pub reads: Footprint,
    pub writes: Footprint,
    pub location: Option<Range>,
}

fn footprint(range: Range) -> Footprint {
    let mut result = Footprint::default();
    result.insert(range);
    result
}

pub fn accesses(s: &Ssa, facts: Option<&dyn CallFacts>) -> BTreeMap<ValueId, Access> {
    let mut addresses = Addresses::new(s);
    let mut result = BTreeMap::new();
    for block in &s.blocks {
        for &id in &block.ops {
            let args = s.args(id);
            let op = s.value(id).op;
            let empty = Footprint::default();
            let access = match op {
                Op::P(OpCode::Load | OpCode::Store) => {
                    let size = if op == Op::P(OpCode::Load) {
                        s.value(id).size
                    } else {
                        s.value(args[2]).size
                    };
                    let range = Range::new(addresses.get(args[1]), size as u32);
                    let effect = footprint(range);
                    if op == Op::P(OpCode::Load) {
                        Access {
                            reads: effect,
                            writes: empty,
                            location: Some(range),
                        }
                    } else {
                        Access {
                            reads: empty,
                            writes: effect,
                            location: Some(range),
                        }
                    }
                }
                Op::X87(
                    op @ (X87Op::PushInteger { bits: 16 | 32 | 64 }
                    | X87Op::Store {
                        dst: None,
                        bits: 16 | 32 | 64 | 80,
                        ..
                    }),
                ) => {
                    let bits = match op {
                        X87Op::PushInteger { bits } | X87Op::Store { bits, .. } => bits,
                        _ => unreachable!(),
                    };
                    let range = Range::new(addresses.get(args[1]), u32::from(bits / 8));
                    let effect = footprint(range);
                    if op.memory() == Some(MemoryEffect::Read) {
                        Access {
                            reads: effect,
                            writes: empty,
                            location: Some(range),
                        }
                    } else {
                        Access {
                            reads: empty,
                            writes: effect,
                            location: Some(range),
                        }
                    }
                }
                Op::Call { .. } | Op::CallInd => {
                    let target = match op {
                        Op::Call { target } => Some(target),
                        _ => None,
                    };
                    let site = s.value_addr[id as usize];
                    let frame = addresses.call_frame(id);
                    let effects = facts
                        .and_then(|facts| facts.memory(site, target))
                        .map(|effects| effects.in_frame(frame))
                        .or_else(|| {
                            facts
                                .and_then(|facts| facts.stack(site, target))
                                .map(|stack| {
                                    let mut writes = Footprint::default();
                                    for &(offset, size) in &stack.writes {
                                        writes
                                            .insert(Range::new(frame.offset(offset as u32), size));
                                    }
                                    Effects {
                                        reads: Footprint::unknown(),
                                        writes,
                                    }
                                })
                        })
                        .unwrap_or_else(Effects::unknown);
                    Access {
                        reads: effects.reads,
                        writes: effects.writes,
                        location: None,
                    }
                }
                _ if rk_ir::properties(op).memory => Access {
                    reads: Footprint::unknown(),
                    writes: Footprint::unknown(),
                    location: None,
                },
                _ => continue,
            };
            result.insert(id, access);
        }
    }
    result
}

pub(crate) fn eliminate_dead_stores(s: &mut Ssa, facts: Option<&dyn CallFacts>) {
    let accesses = accesses(s, facts);
    let mut aliases = Lookup::default();
    for block in &s.blocks {
        let mut overwritten = BTreeSet::new();
        for &id in block.ops.iter().rev() {
            let op = s.value(id).op;
            if s.boundary(id).is_some() && op != Op::P(OpCode::Store) {
                overwritten.clear();
                continue;
            }
            let Some(access) = accesses.get(&id) else {
                if rk_ir::properties(op).observes && !matches!(op, Op::X87(_)) {
                    overwritten.clear();
                }
                continue;
            };
            if op == Op::P(OpCode::Store)
                && access
                    .location
                    .is_some_and(|range| overwritten.contains(&range))
            {
                aliases.insert(id, s.args(id)[0]);
                continue;
            }
            overwritten.retain(|range| access.reads.disjoint(*range));
            if let Some(range) = access.location.filter(|range| range.address.is_exact()) {
                if !access.writes.ranges.is_empty() && !access.writes.opaque {
                    overwritten.insert(range);
                }
            }
        }
    }
    if aliases.is_empty() {
        return;
    }
    for index in 0..s.values.len() {
        if s.values[index].op != Op::Memory {
            continue;
        }
        let args = s.args(index as ValueId);
        if args
            .first()
            .is_some_and(|effect| aliases.contains_key(effect))
        {
            aliases.insert(index as ValueId, s.args(args[0])[0]);
        }
    }
    crate::canonical::rewrite_aliases(s, &aliases);
}

fn identified(range: Range) -> bool {
    matches!(
        range.address,
        Address::Known {
            base: Base::Frame | Base::Absolute,
            span: 0,
            ..
        }
    )
}

pub fn partition(s: &mut Ssa, facts: Option<&dyn CallFacts>) -> bool {
    if s.memory_partitioned {
        return true;
    }
    let accesses = accesses(s, facts);
    let mut ranges: BTreeSet<Range> = accesses
        .values()
        .filter_map(|access| access.location)
        .filter(|range| identified(*range))
        .collect();
    if ranges.len() > 512 {
        return false;
    }
    let regions: Vec<Option<Range>> = std::iter::once(None)
        .chain(std::mem::take(&mut ranges).into_iter().map(Some))
        .collect();
    let old_len = s.values.len();
    let old_args = s.args.len();
    let mut projection = Projection {
        s,
        accesses: &accesses,
        regions: &regions,
        old_len,
        cache: Lookup::default(),
        aliases: Lookup::default(),
        pending_phis: Vec::new(),
        owners: Lookup::default(),
        budget: old_len.saturating_add(1_000_000),
    };
    for (block, item) in projection.s.blocks.iter().enumerate() {
        for &phi in &item.phis {
            projection.owners.insert(phi, block);
        }
    }
    let mut rewrites = Vec::new();
    let mut failed = false;
    for (&effect, access) in &accesses {
        let Some(&before) = projection.s.args(effect).first() else {
            continue;
        };
        let exact = access.location.filter(|range| identified(*range));
        let affected: Vec<usize> = regions
            .iter()
            .enumerate()
            .filter_map(|(index, region)| {
                if let Some(range) = exact {
                    return (*region == Some(range)).then_some(index);
                }
                let touched = match region {
                    Some(region) => {
                        !access.reads.disjoint(*region) || !access.writes.disjoint(*region)
                    }
                    None => {
                        access.reads.opaque
                            || access.writes.opaque
                            || access.location.is_some_and(|range| !identified(range))
                    }
                };
                touched.then_some(index)
            })
            .collect();
        let mut dependencies = Vec::new();
        for region in affected {
            match projection.get(before, region) {
                Some(version) => dependencies.push(version),
                None => {
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            break;
        }
        dependencies.sort_unstable();
        dependencies.dedup();
        let version = match dependencies.as_slice() {
            [] => projection.s.memory().unwrap_or(before),
            [only] => *only,
            _ => projection.s.new_value(Op::Memory, 0, &dependencies, 0),
        };
        rewrites.push((effect, version));
    }
    if failed {
        projection.s.values.truncate(old_len);
        projection.s.value_addr.truncate(old_len);
        projection.s.args.truncate(old_args);
        return false;
    }
    for (phi, block) in projection.pending_phis {
        projection.s.blocks[block].phis.push(phi);
    }
    for index in 0..old_len {
        if projection.s.values[index].op != Op::Memory {
            continue;
        }
        let args = projection.s.args(index as ValueId);
        if args.len() != 1 {
            continue;
        }
        let effect = args[0];
        if !accesses.contains_key(&effect) {
            continue;
        }
        let Some(&prior) = projection.s.args(effect).first() else {
            continue;
        };
        let start = projection.s.args.len() as u32;
        projection.s.args.extend([effect, prior]);
        projection.s.values[index].args_start = start;
        projection.s.values[index].args_len = 2;
    }
    for (effect, version) in rewrites {
        let start = projection.s.values[effect as usize].args_start as usize;
        projection.s.args[start] = version;
    }
    crate::canonical::rewrite_aliases(projection.s, &projection.aliases);
    projection.s.memory_partitioned = true;
    true
}

struct Projection<'a> {
    s: &'a mut Ssa,
    accesses: &'a BTreeMap<ValueId, Access>,
    regions: &'a [Option<Range>],
    old_len: usize,
    cache: Lookup<(ValueId, usize), ValueId>,
    aliases: Lookup<ValueId, ValueId>,
    owners: Lookup<ValueId, usize>,
    pending_phis: Vec<(ValueId, usize)>,
    budget: usize,
}

impl Projection<'_> {
    fn get(&mut self, token: ValueId, region: usize) -> Option<ValueId> {
        let mut pending = vec![(token, false)];
        while let Some((token, finish)) = pending.pop() {
            if finish {
                let output = *self.cache.get(&(token, region))?;
                let (op, sources, effect) = self.sources(token, region);
                let mut args: Vec<ValueId> = effect.into_iter().collect();
                args.extend(sources.iter().map(|source| {
                    *self
                        .cache
                        .get(&(*source, region))
                        .expect("projected memory predecessor")
                }));
                if effect.is_none() && op == Op::Memory && args.len() == 1 {
                    self.aliases.insert(output, args[0]);
                }
                let start = self.s.args.len() as u32;
                self.s.args.extend(args.iter().copied());
                let value = &mut self.s.values[output as usize];
                value.op = op;
                value.args_start = start;
                value.args_len = args.len() as u32;
                continue;
            }
            if self.cache.contains_key(&(token, region)) {
                continue;
            }
            if self.s.values.len() >= self.budget {
                return None;
            }
            if token as usize >= self.old_len {
                return None;
            }
            match self.s.value(token).op {
                Op::Input(_) => {
                    self.cache.insert((token, region), token);
                }
                Op::Memory | Op::Phi => {
                    let (op, sources, _) = self.sources(token, region);
                    let output = self
                        .s
                        .new_value(op, 0, &[], self.s.value_addr[token as usize]);
                    self.cache.insert((token, region), output);
                    if op == Op::Phi {
                        self.pending_phis.push((output, *self.owners.get(&token)?));
                    }
                    pending.push((token, true));
                    pending.extend(sources.into_iter().map(|source| (source, false)));
                }
                _ => {
                    self.cache.insert((token, region), token);
                }
            }
        }
        self.cache.get(&(token, region)).copied()
    }

    fn sources(&self, token: ValueId, region: usize) -> (Op, Vec<ValueId>, Option<ValueId>) {
        let args = self.s.args(token);
        if self.s.value(token).op == Op::Phi {
            return (Op::Phi, args.to_vec(), None);
        }
        let effect = args[0];
        let Some(access) = self.accesses.get(&effect) else {
            return (Op::Memory, args.to_vec(), None);
        };
        let prior = self.s.args(effect)[0];
        let touched = match self.regions[region] {
            Some(range) => !access.writes.disjoint(range),
            None => {
                access.writes.opaque
                    || (!access.writes.ranges.is_empty() && access.location.is_none())
                    || access
                        .location
                        .is_some_and(|range| !identified(range) && !access.writes.ranges.is_empty())
            }
        };
        let observed = self.regions[region].is_some_and(|range| access.location == Some(range))
            && self.s.value(effect).op == Op::P(OpCode::Load);
        if !touched && !observed {
            return (Op::Memory, vec![prior], None);
        }
        let definite = self.regions[region].is_some_and(|range| access.location == Some(range))
            && self.s.value(effect).op == Op::P(OpCode::Store);
        (
            Op::Memory,
            if definite { Vec::new() } else { vec![prior] },
            Some(effect),
        )
    }
}

pub fn dependencies(s: &Ssa, effect: ValueId) -> Vec<ValueId> {
    let mut pending = s
        .args(effect)
        .first()
        .copied()
        .into_iter()
        .collect::<Vec<_>>();
    let mut visited = LookupSet::default();
    let mut result = Vec::new();
    while let Some(token) = pending.pop() {
        if !visited.insert(token) {
            continue;
        }
        match s.value(token).op {
            Op::Memory | Op::Phi => {
                for &arg in s.args(token) {
                    if s.value(arg).size == 0
                        && matches!(s.value(arg).op, Op::Memory | Op::Phi | Op::Input(_))
                    {
                        pending.push(arg);
                    } else {
                        result.push(arg);
                    }
                }
            }
            _ => result.push(token),
        }
    }
    result.sort_unstable();
    result.dedup();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BuildOptions, CallEffect, ConservativeCallFacts};
    use rk_core::contract::Fields;
    use rk_core::ir::{Policy, Varnode};
    use rk_ir::{CgFunction, CgInsn, CgOp, Registers};

    fn op(code: Op, output: Option<Varnode>, inputs: &[Varnode]) -> CgOp {
        CgOp {
            op: code,
            output,
            inputs: inputs.to_vec(),
        }
    }

    fn fixture(opaque_load: bool, opaque_store: bool, facts: &dyn CallFacts, call: bool) -> Ssa {
        let esp = Registers::get().require("ESP");
        let ecx = Registers::get().require("ECX");
        let eax = Registers::get().require("EAX");
        let address = Varnode::new(rk_core::ir::Space::Unique, 0x100, 4);
        let mut ops = vec![
            op(
                Op::P(OpCode::Store),
                None,
                &[esp, Varnode::constant(0x11223344, 4)],
            ),
            op(
                Op::P(OpCode::IntAdd),
                Some(address),
                &[esp, Varnode::constant(8, 4)],
            ),
            op(
                Op::P(OpCode::Store),
                None,
                &[address, Varnode::constant(0x55667788, 4)],
            ),
        ];
        let saved_esp = Varnode::new(rk_core::ir::Space::Unique, 0x200, 4);
        if call {
            ops.push(op(Op::P(OpCode::Copy), Some(saved_esp), &[esp]));
            ops.push(op(Op::Call { target: 0x5000 }, None, &[]));
        }
        if opaque_store {
            ops.push(op(
                Op::P(OpCode::Store),
                None,
                &[ecx, Varnode::constant(0xaa, 1)],
            ));
        }
        ops.push(op(
            Op::P(OpCode::Load),
            Some(eax),
            &[if opaque_load {
                ecx
            } else if call {
                saved_esp
            } else {
                esp
            }],
        ));
        ops.push(op(Op::Return, None, &[]));
        let n = ops.len();
        let cg = CgFunction {
            addr: 0x1000,
            insns: ops
                .into_iter()
                .enumerate()
                .map(|(i, op)| CgInsn {
                    addr: 0x1000 + i as u32,
                    insn: i as u32,
                    length: 1,
                    ops: vec![op],
                    x87: false,
                    flag: None,
                    flag_operands: None,
                })
                .collect(),
            succ: (0..n)
                .map(|i| {
                    if i + 1 < n {
                        vec![(i + 1) as u32]
                    } else {
                        Vec::new()
                    }
                })
                .collect(),
            entries: Vec::new(),
            tables: BTreeSet::new(),
            dynamic: BTreeSet::new(),
            exits: BTreeMap::new(),
            noreturn: BTreeMap::new(),
            import_tails: BTreeMap::new(),
            seh: BTreeMap::new(),
            slow: Vec::new(),
            diagnostics: Vec::new(),
        };
        let mut s = crate::build(
            &cg,
            &BuildOptions {
                policy: Policy::Optimizing,
                call_facts: facts,
                ..BuildOptions::default()
            },
        )
        .unwrap();
        crate::canonicalize(&mut s, true);
        s
    }

    fn loads(s: &Ssa) -> Vec<ValueId> {
        s.blocks
            .iter()
            .flat_map(|block| block.ops.iter().copied())
            .filter(|&id| s.value(id).op == Op::P(OpCode::Load))
            .collect()
    }

    #[test]
    fn disjoint_store_retains_the_slot_definition() {
        let mut s = fixture(false, false, &ConservativeCallFacts, false);
        assert!(partition(&mut s, None));
        let effects = dependencies(&s, loads(&s)[0]);
        assert_eq!(effects.len(), 1);
        assert_eq!(s.value_addr[effects[0] as usize], 0x1000);
        crate::validate(&s).unwrap();
        let count = s.values.len();
        assert!(partition(&mut s, None));
        assert_eq!(count, s.values.len());
        assert!(crate::forward(&mut s, None));
        assert!(loads(&s).is_empty());
        let mut s = fixture(false, false, &ConservativeCallFacts, false);
        assert!(crate::forward(&mut s, None));
        assert!(loads(&s).is_empty());
        crate::validate(&s).unwrap();
    }

    #[test]
    fn opaque_reads_depend_on_tracked_slots_and_residual_memory() {
        let mut s = fixture(true, false, &ConservativeCallFacts, false);
        assert!(partition(&mut s, None));
        let effects = dependencies(&s, loads(&s)[0]);
        for addr in [0x1000, 0x1002] {
            assert!(effects.iter().any(|&id| s.value_addr[id as usize] == addr));
        }
        assert!(effects
            .iter()
            .any(|&id| matches!(s.value(id).op, Op::Input(_))));
        crate::validate(&s).unwrap();
    }

    #[test]
    fn opaque_writes_are_may_definitions() {
        let mut s = fixture(false, true, &ConservativeCallFacts, false);
        assert!(partition(&mut s, None));
        let effects = dependencies(&s, loads(&s)[0]);
        for addr in [0x1000, 0x1003] {
            assert!(effects.iter().any(|&id| s.value_addr[id as usize] == addr));
        }
        crate::validate(&s).unwrap();
        let mut s = fixture(false, true, &ConservativeCallFacts, false);
        assert!(!crate::forward(&mut s, None));
        assert_eq!(loads(&s).len(), 1);
    }

    #[test]
    fn region_budget_keeps_the_original_conservative_graph() {
        let mut s = fixture(false, false, &ConservativeCallFacts, false);
        let load = loads(&s)[0];
        let mut before = s.args(load)[0];
        let stored = s.new_value(Op::Const(7), 4, &[], 0);
        let mut stores = Vec::new();
        for index in 0..513 {
            let address = s.new_value(Op::Const(0x500000 + index * 4), 4, &[], 0);
            let store = s.new_value(Op::P(OpCode::Store), 0, &[before, address, stored], 0x1003);
            before = s.new_value(Op::Memory, 0, &[store], 0x1003);
            stores.push(store);
        }
        let position = s.blocks[0].ops.iter().position(|&id| id == load).unwrap();
        s.blocks[0].ops.splice(position..position, stores);
        let start = s.values[load as usize].args_start as usize;
        s.args[start] = before;
        let values = s.values.clone();
        let args = s.args.clone();
        assert!(!partition(&mut s, None));
        assert_eq!(values, s.values);
        assert_eq!(args, s.args);
        assert!(!crate::forward(&mut s, None));
        crate::validate(&s).unwrap();
    }

    struct Facts {
        effects: Effects,
    }
    impl CallFacts for Facts {
        fn call(&self, _: u32, _: Option<u32>) -> CallEffect {
            CallEffect {
                reads: Fields::NONE,
                writes: Fields::NONE,
                kills: Fields::NONE,
                preserves: Fields::ALL,
            }
        }
        fn memory(&self, _: u32, _: Option<u32>) -> Option<&Effects> {
            Some(&self.effects)
        }
    }

    #[test]
    fn conditional_register_keeps_are_not_frame_proofs() {
        let facts = Facts {
            effects: Effects::default(),
        };
        let mut s = fixture(false, false, &facts, true);
        let load = loads(&s)[0];
        let ret = *s.blocks[0].ops.last().unwrap();
        let state = s.boundary(ret).unwrap();
        let lanes: Vec<_> = rk_ir::lanes(rk_core::contract::Field::Esp)
            .iter()
            .map(|key| {
                state
                    .iter()
                    .find(|(candidate, _)| candidate == key)
                    .unwrap()
                    .1
            })
            .collect();
        let address = s.new_value(Op::Pack, 4, &lanes, 0);
        let start = s.values[load as usize].args_start as usize;
        s.args[start + 1] = address;
        assert!(!matches!(
            Addresses::new(&s).get(address),
            Address::Known {
                base: Base::Frame,
                ..
            }
        ));
        assert!(!crate::forward(&mut s, Some(&facts)));
        crate::validate(&s).unwrap();
    }

    #[test]
    fn calls_only_preserve_ranges_proved_disjoint_from_their_writes() {
        let mut effects = Effects::default();
        effects
            .writes
            .insert(Range::new(Address::exact(Base::Frame, 8), 4));
        let facts = Facts { effects };
        let mut s = fixture(false, false, &facts, true);
        assert!(crate::forward(&mut s, Some(&facts)));
        assert!(loads(&s).is_empty());
        let facts = Facts {
            effects: Effects::unknown(),
        };
        let mut s = fixture(false, false, &facts, true);
        assert!(!crate::forward(&mut s, Some(&facts)));
        assert_eq!(loads(&s).len(), 1);
    }
}
