use std::collections::{BTreeMap, BTreeSet, VecDeque};

use rk_core::contract::Field;
use rk_core::ir::OpCode;
use rk_core::memory::{Address, Base, Effects, Footprint, Range};
use rk_core::{Lookup, LookupSet};
use rk_ir::x87::MemoryEffect;
use rk_ir::Op;

use crate::address::{Addresses, Relations};
use crate::{CallFacts, Ssa, ValueId};

#[derive(Clone, Debug)]
pub struct Access {
    pub reads: Footprint,
    pub writes: Footprint,
    pub location: Option<Range>,
}

fn footprint(range: Range, local: bool) -> Footprint {
    let mut result = Footprint::default();
    if local && range.address.is_exact() && range.size != 0 {
        result.ranges.insert(range);
    } else {
        result.insert(range);
    }
    result
}

pub fn accesses(s: &Ssa, facts: Option<&dyn CallFacts>) -> BTreeMap<ValueId, Access> {
    accesses_using(s, facts, false).0
}

pub(crate) fn local_accesses(s: &Ssa, facts: Option<&dyn CallFacts>) -> BTreeMap<ValueId, Access> {
    accesses_using(s, facts, true).0
}

fn accesses_using(
    s: &Ssa,
    facts: Option<&dyn CallFacts>,
    local: bool,
) -> (BTreeMap<ValueId, Access>, Relations) {
    let mut addresses = if local {
        Addresses::local(s)
    } else {
        Addresses::new(s)
    };
    let mut result = BTreeMap::new();
    for block in &s.blocks {
        for &id in &block.ops {
            let args = s.args(id);
            let op = s.value(id).op;
            let empty = Footprint::default();
            let access = match op {
                Op::P(OpCode::Load | OpCode::Store) | Op::StackFlush => {
                    let size = if op == Op::P(OpCode::Load) {
                        s.value(id).size
                    } else {
                        s.value(args[2]).size
                    };
                    let range = Range::new(addresses.get(args[1]), size as u32);
                    let effect = footprint(range, local);
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
                Op::X87(op) if op.memory_size().is_some() => {
                    let range = Range::new(addresses.get(args[1]), op.memory_size().unwrap());
                    let effect = footprint(range, local);
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
    (result, addresses.relations)
}

pub(crate) fn eliminate_dead_stores(s: &mut Ssa, facts: Option<&dyn CallFacts>) {
    let (accesses, relations) = accesses_using(s, facts, true);
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
                && access.location.is_some_and(|range| {
                    overwritten
                        .iter()
                        .any(|&later| relations.offset_of(later, range).is_some())
                })
            {
                aliases.insert(id, s.args(id)[0]);
                continue;
            }
            overwritten.retain(|range| relations.excludes(&access.reads, *range));
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
            base: Base::Frame | Base::Absolute | Base::Value(_) | Base::Stack(_),
            span: 0,
            ..
        }
    )
}

#[derive(Default)]
pub(crate) struct PrivateFrame {
    pub(crate) eligible: bool,
    relations: Relations,
    independent: Lookup<ValueId, i32>,
    exposed: Footprint,
    calls: Lookup<ValueId, Effects>,
    checkpoints: Lookup<ValueId, u32>,
}

impl PrivateFrame {
    fn rejection(
        s: &Ssa,
        accesses: &BTreeMap<ValueId, Access>,
    ) -> Option<(&'static str, Option<ValueId>)> {
        if !s.assume_private_stack {
            return Some(("private-stack-disabled", None));
        }
        if s.entries.len() != 1 || s.entries[0].addr != s.addr {
            return Some(("multiple-or-interior-entries", None));
        }
        if let Some((&id, _)) = accesses.iter().find(|&(id, access)| {
            access.location.is_none() && !matches!(s.value(*id).op, Op::Call { .. } | Op::CallInd)
        }) {
            return Some(("unmodelled-memory-effect", Some(id)));
        }
        for block in &s.blocks {
            for &id in &block.ops {
                let op = s.value(id).op;
                if rk_ir::properties(op).opaque && !matches!(op, Op::Call { .. } | Op::CallInd)
                    || matches!(
                        op,
                        Op::Seh(_)
                            | Op::Tail { .. }
                            | Op::ImportTail { .. }
                            | Op::BranchInd
                            | Op::Breakpoint
                    )
                    || matches!(op, Op::Branch { .. } | Op::CBranch { .. })
                        && s.boundary(id).is_some()
                {
                    return Some(("opaque-or-external-control-flow", Some(id)));
                }
            }
        }
        None
    }

    pub(crate) fn new(
        s: &Ssa,
        accesses: &BTreeMap<ValueId, Access>,
        facts: Option<&dyn CallFacts>,
    ) -> Self {
        if Self::rejection(s, accesses).is_some() {
            return Self::default();
        }
        let mut derived = vec![false; s.values.len()];
        let mut users = vec![Vec::new(); s.values.len()];
        for (index, value) in s.values.iter().enumerate() {
            if value.size != 0 && rk_ir::properties(value.op).pure {
                for &arg in s.args(index as ValueId) {
                    users[arg as usize].push(index);
                }
            }
        }
        let mut pending = VecDeque::new();
        for &key in rk_ir::lanes(Field::Esp) {
            if let Some(id) = s.input(key) {
                derived[id as usize] = true;
                pending.push_back(id as usize);
            }
        }
        while let Some(id) = pending.pop_front() {
            for &user in &users[id] {
                if !derived[user] {
                    derived[user] = true;
                    pending.push_back(user);
                }
            }
        }
        let mut addresses = Addresses::new(s);
        let mut exposed = Footprint::default();
        let expose = |exposed: &mut Footprint, address| {
            if s.assume_stack_pointer_bases {
                if let Address::Known {
                    base: Base::Frame,
                    offset,
                    span: 0,
                } = address
                {
                    if (offset as i32) < 0 {
                        exposed.insert(Range::new(address, offset.wrapping_neg()));
                    }
                    return;
                }
            }
            *exposed = Footprint::unknown();
        };
        let mut stored_frame = false;
        let mut stored_frame_ranges = Vec::new();
        for &id in accesses.keys() {
            if accesses[&id].location.is_some() {
                addresses.get(s.args(id)[1]);
            }
            if s.value(id).op == Op::P(OpCode::Store) && derived[s.args(id)[2] as usize] {
                stored_frame = true;
                stored_frame_ranges.push(accesses[&id].location);
                expose(&mut exposed, addresses.get(s.args(id)[2]));
            }
        }
        {
            for &id in s.blocks.iter().flat_map(|block| &block.ops) {
                if !matches!(s.value(id).op, Op::Call { .. } | Op::CallInd) {
                    continue;
                }
                let target = match s.value(id).op {
                    Op::Call { target } => Some(target),
                    _ => None,
                };
                let summary =
                    facts.and_then(|facts| facts.borrowed_stack(s.value_addr[id as usize], target));
                if let Some(summary) =
                    summary.filter(|summary| !summary.unknown_captures.is_empty())
                {
                    let frame = addresses.state(id, Field::Esp);
                    if stored_frame_ranges.iter().any(|stored| {
                        stored.is_none_or(|stored| {
                            let stored =
                                Range::new(addresses.relations.bounds(stored.address), stored.size);
                            summary.unknown_captures.iter().any(|base| match *base {
                                Base::Argument(offset) => {
                                    let read = Range::new(frame.offset(offset as u32), 4);
                                    !stored.disjoint(Range::new(
                                        addresses.relations.bounds(read.address),
                                        read.size,
                                    ))
                                }
                                _ => false,
                            })
                        })
                    }) {
                        exposed = Footprint::unknown();
                    }
                }
                if let Some(&offset) =
                    summary.and_then(|summary| summary.captures.get(&Base::Frame))
                {
                    expose(
                        &mut exposed,
                        addresses.state(id, Field::Esp).offset(offset as u32),
                    );
                }
                if stored_frame && summary.is_some_and(|summary| {
                    summary.captures.iter().any(|(base, &offset)| matches!(base, Base::Argument(_)) && offset < 0)
                        || summary.effects.reads.ranges.iter().chain(&summary.effects.writes.ranges).any(|range|
                            matches!(range.address, Address::Known { base: Base::Argument(_), offset, .. } if (offset as i32) < 0))
                }) {
                    exposed = Footprint::unknown();
                }
                for field in Field::ALL {
                    if field == Field::Esp || rk_core::contract::Fields::FLAGS.contains(field) {
                        continue;
                    }
                    if s.boundary(id).is_some_and(|state| {
                        state.iter().any(|&(key, value)| {
                            s.field(key) == Some(field) && derived[value as usize]
                        })
                    }) {
                        let address = addresses.state(id, field);
                        let parameter = Base::Parameter(field as u8);
                        if summary
                            .is_some_and(|summary| summary.unknown_captures.contains(&parameter))
                        {
                            exposed = Footprint::unknown();
                        }
                        if let Some(summary) = summary {
                            for range in summary
                                .effects
                                .reads
                                .ranges
                                .iter()
                                .chain(&summary.effects.writes.ranges)
                            {
                                if let Address::Known { base, offset, span } = range.address {
                                    if base == parameter {
                                        exposed.insert(Range::new(
                                            address.offset(offset).bounded(0, span),
                                            range.size,
                                        ));
                                    }
                                }
                            }
                            if !summary.unknown_captures.contains(&parameter)
                                && !summary.captures.contains_key(&parameter)
                                && !summary.effects.reads.opaque
                                && !summary.effects.writes.opaque
                            {
                                continue;
                            }
                        }
                        let floor = summary
                            .and_then(|summary| summary.captures.get(&parameter))
                            .copied()
                            .unwrap_or(0)
                            .min(0);
                        expose(&mut exposed, address.offset(floor as u32));
                    }
                }
            }
        }
        let mut independent = Lookup::default();
        let mut calls = Lookup::default();
        let mut checkpoints = Lookup::default();
        for (&id, access) in accesses {
            if let Op::Call { target } = s.value(id).op {
                let site = s.value_addr[id as usize];
                if s.stack_calls
                    && facts.is_some_and(|facts| {
                        facts.readonly_memory(site, Some(target))
                            && facts.cleanup(site, Some(target)).is_some()
                    })
                {
                    if let Some(cost) =
                        facts.and_then(|facts| facts.checkpoints(site, Some(target)))
                    {
                        calls.insert(
                            id,
                            Effects {
                                reads: Footprint::unknown(),
                                writes: Footprint::default(),
                            },
                        );
                        checkpoints.insert(id, cost);
                    }
                }
                if let Some(summary) = s
                    .stack_calls
                    .then(|| {
                        facts.and_then(|facts| {
                            facts.borrowed_stack(s.value_addr[id as usize], Some(target))
                        })
                    })
                    .flatten()
                {
                    let Some(cost) = summary.checkpoints else {
                        continue;
                    };
                    if facts
                        .and_then(|facts| facts.cleanup(s.value_addr[id as usize], Some(target)))
                        .is_none()
                        || summary.effects.writes.ranges.iter().any(|range| {
                            matches!(
                                range.address,
                                Address::Known {
                                    base: Base::Frame,
                                    ..
                                }
                            ) && !range.disjoint(Range::new(Address::exact(Base::Frame, 0), 4))
                        })
                    {
                        continue;
                    }
                    let mut effects = Effects::default();
                    let frame = addresses.state(id, Field::Esp);
                    for (source, output) in [
                        (&summary.effects.reads, &mut effects.reads),
                        (&summary.effects.writes, &mut effects.writes),
                    ] {
                        output.opaque = source.opaque;
                        for &range in &source.ranges {
                            if matches!(
                                range.address,
                                Address::Known {
                                    base: Base::Frame,
                                    ..
                                }
                            ) {
                                output.insert(range.in_frame(frame));
                            }
                        }
                    }
                    calls.insert(id, effects);
                    checkpoints.insert(id, cost);
                }
            }
            if access.location.is_none() || derived[s.args(id)[1] as usize] {
                continue;
            }
            if let Address::Known {
                base: Base::Frame,
                offset,
                span,
            } = addresses.state(id, Field::Esp)
            {
                let top = i64::from(offset as i32) + i64::from(span);
                if (offset as i32) < 0 && top < 0 {
                    independent.insert(id, top as i32);
                }
            }
        }
        Self {
            eligible: true,
            relations: addresses.relations,
            independent,
            exposed,
            calls,
            checkpoints,
        }
    }

    pub(crate) fn unexposed(&self, range: Range) -> bool {
        self.eligible
            && self
                .exposed
                .disjoint(Range::new(self.relations.bounds(range.address), range.size))
    }

    pub(crate) fn call_disjoint(&self, effect: ValueId, range: Range) -> bool {
        let range = Range::new(self.relations.bounds(range.address), range.size);
        self.unexposed(range)
            && self.calls.get(&effect).is_some_and(|effects| {
                effects.reads.disjoint(range) && effects.writes.disjoint(range)
            })
    }

    pub(crate) fn call_checkpoints(&self, effect: ValueId) -> Option<u32> {
        self.checkpoints.get(&effect).copied()
    }

    pub(crate) fn disjoint(&self, effect: ValueId, range: Range) -> bool {
        let range = Range::new(self.relations.bounds(range.address), range.size);
        let Address::Known {
            base: Base::Frame,
            offset,
            span,
        } = range.address
        else {
            return false;
        };
        let start = i64::from(offset as i32);
        self.unexposed(range)
            && range.size != 0
            && start < 0
            && start + i64::from(span) + i64::from(range.size) <= 0
            && (self
                .calls
                .get(&effect)
                .is_some_and(|effects| effects.writes.disjoint(range))
                || self
                    .independent
                    .get(&effect)
                    .is_some_and(|&esp| i64::from(esp) <= start))
    }
}

pub(crate) fn dump_stack(
    s: &Ssa,
    facts: &dyn CallFacts,
    out: &mut (impl std::io::Write + ?Sized),
) -> std::io::Result<()> {
    let accesses = accesses(s, Some(facts));
    let private = PrivateFrame::new(s, &accesses, Some(facts));
    let mut addresses = Addresses::local(s);
    writeln!(
        out,
        "== stack {} private={} pointer-bases={} guarded-calls={}",
        rk_core::Hex(s.addr),
        s.assume_private_stack,
        s.assume_stack_pointer_bases,
        s.stack_calls
    )?;
    match PrivateFrame::rejection(s, &accesses) {
        Some((reason, Some(id))) => writeln!(
            out,
            "frame rejected: {reason} at {} v{id} {:?}",
            rk_core::Hex(s.value_addr[id as usize]),
            s.value(id).op
        )?,
        Some((reason, None)) => writeln!(out, "frame rejected: {reason}")?,
        None => writeln!(out, "frame eligible; exposed={:?}", private.exposed)?,
    }
    let mut counts = BTreeMap::<&str, usize>::new();
    let mut origins = BTreeSet::new();
    for block in &s.blocks {
        for &id in &block.ops {
            let value = s.value(id);
            let site = s.value_addr[id as usize];
            if matches!(value.op, Op::Call { .. } | Op::CallInd) {
                let target = match value.op {
                    Op::Call { target } => Some(target),
                    _ => None,
                };
                if let Address::Known {
                    base: Base::Value(root),
                    ..
                } = addresses.state(id, Field::Esp)
                {
                    origins.insert(root);
                }
                writeln!(out, "call {} v{id} {:?} esp={:?} cleanup={:?} checkpoints={:?} borrowed={} readonly={} guard={}",
                    rk_core::Hex(site), value.op, addresses.state(id, Field::Esp),
                    facts.cleanup(site, target), facts.checkpoints(site, target),
                    facts.borrowed_stack(site, target).is_some(), facts.readonly_memory(site, target), s.stack_guards.contains_key(&id))?;
            }
            if value.op == Op::P(OpCode::IntAnd) && value.size == 4 {
                let local = addresses.get(id);
                if matches!(
                    local,
                    Address::Known {
                        base: Base::Stack(_),
                        ..
                    }
                ) {
                    writeln!(
                        out,
                        "aligned {} v{id} local={local:?} frame={:?}",
                        rk_core::Hex(site),
                        addresses.relations.bounds(local)
                    )?;
                }
            }
            let Some(access) = accesses.get(&id) else {
                continue;
            };
            let Some(range) = access.location else {
                *counts.entry("unlocated-effects").or_default() += 1;
                continue;
            };
            let local = addresses.get(s.args(id)[1]);
            let kind = match range.address {
                Address::Known {
                    base: Base::Frame,
                    span: 0,
                    ..
                } => "exact-frame",
                Address::Known {
                    base: Base::Frame, ..
                } => "bounded-frame",
                _ if matches!(
                    local,
                    Address::Known {
                        base: Base::Stack(_),
                        ..
                    }
                ) =>
                {
                    "stack-base"
                }
                _ => "other-memory",
            };
            *counts.entry(kind).or_default() += 1;
            if kind == "other-memory" {
                continue;
            }
            writeln!(
                out,
                "access {} v{id} {:?} size={} local={local:?} frame={:?} esp={:?} unexposed={}",
                rk_core::Hex(site),
                value.op,
                range.size,
                range.address,
                addresses.state(id, Field::Esp),
                private.unexposed(range)
            )?;
        }
    }
    let mut seen = BTreeSet::new();
    while let Some(id) = origins.pop_first() {
        if seen.len() >= 256 || !seen.insert(id) {
            continue;
        }
        let op = s.value(id).op;
        writeln!(
            out,
            "stack-origin v{id} {} {op:?} args={:?}",
            rk_core::Hex(s.value_addr[id as usize]),
            s.args(id)
        )?;
        if matches!(
            op,
            Op::Phi
                | Op::Pack
                | Op::Byte(_)
                | Op::StateKeep(_)
                | Op::P(OpCode::Copy | OpCode::IntAdd | OpCode::IntSub)
        ) {
            origins.extend(s.args(id));
        }
    }
    writeln!(out, "access-counts {counts:?}")?;
    crate::forward::dump_dependencies(s, facts, out)
}

pub fn partition(s: &mut Ssa, facts: Option<&dyn CallFacts>) -> bool {
    if s.memory_partitioned {
        return true;
    }
    let (accesses, relations) = accesses_using(s, facts, true);
    let private_frame = PrivateFrame::new(s, &accesses, facts);
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
        relations: &relations,
        regions: &regions,
        private_frame: &private_frame,
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
                        !projection.private_frame.disjoint(effect, *region)
                            && (!projection.relations.excludes(&access.reads, *region)
                                || !projection.relations.excludes(&access.writes, *region))
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
    relations: &'a Relations,
    regions: &'a [Option<Range>],
    private_frame: &'a PrivateFrame,
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
            Some(range) => {
                !self.private_frame.disjoint(effect, range)
                    && !self.relations.excludes(&access.writes, range)
            }
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
