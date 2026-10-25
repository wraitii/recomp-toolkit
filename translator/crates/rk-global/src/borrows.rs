use std::collections::{BTreeMap, BTreeSet};

use rayon::prelude::*;
use rk_core::contract::Field;
use rk_core::ir::{FunctionIr, OpCode, Policy};
use rk_core::memory::{Address, Base, Range};
use rk_core::{Addr, LookupSet};
use rk_ir::Op;
use rk_ssa::{BorrowedStack, BuildOptions, CallFacts};

use crate::callgraph::CallGraph;
use crate::{stack, GlobalInput};

#[derive(Clone, Debug, Default)]
pub struct Analysis {
    pub functions: BTreeMap<Addr, BorrowedStack>,
    pub sites: BTreeMap<Addr, BorrowedStack>,
    pub checkpoints: BTreeMap<Addr, u32>,
}

struct Call {
    targets: Vec<Addr>,
    bases: BTreeMap<Base, Address>,
}

impl Call {
    fn address(&self, address: Address) -> Address {
        match address {
            Address::Known {
                base: Base::Absolute,
                ..
            } => address,
            Address::Known { base, offset, span } => self
                .bases
                .get(&base)
                .copied()
                .unwrap_or(Address::Unknown)
                .offset(offset)
                .bounded(0, span),
            _ => Address::Unknown,
        }
    }
}

struct Prepared {
    local: BorrowedStack,
    calls: Vec<Call>,
}

fn capture_sources(
    s: &rk_ssa::Ssa,
    addresses: &mut rk_ssa::address::Addresses<'_>,
    derived: &LookupSet<u32>,
    value: u32,
) -> Option<BTreeSet<Base>> {
    let mut result = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut pending = vec![value];
    while let Some(id) = pending.pop() {
        if !seen.insert(id) || !derived.contains(&id) {
            continue;
        }
        match s.value(id).op {
            Op::Input(key) => {
                let field = s.field(key)?;
                result.insert(if field == Field::Esp {
                    Base::Frame
                } else {
                    Base::Parameter(field as u8)
                });
            }
            Op::P(OpCode::Load) => {
                let Address::Known {
                    base: Base::Frame,
                    offset,
                    span: 0,
                } = addresses.get(s.args(id)[1])
                else {
                    return None;
                };
                if (offset as i32) < 4 {
                    return None;
                }
                result.insert(Base::Argument(offset as i32));
            }
            op if rk_ir::properties(op).pure => pending.extend(s.args(id)),
            _ => return None,
        }
    }
    Some(result)
}

fn prepare(
    input: &GlobalInput<'_>,
    addr: Addr,
    facts: &dyn CallFacts,
    returns: &stack::Analysis,
) -> Option<Prepared> {
    let fir = input.analysis.ir.get(&addr)?.as_ref().ok()?;
    if !fir.entries.is_empty()
        || !fir.dynamic.is_empty()
        || !fir.seh.is_empty()
        || !fir.seh_handlers.is_empty()
        || !fir.seh_unwind_only.is_empty()
        || !fir.import_tails.is_empty()
        || !crate::cfg::closed(fir, &Default::default())
    {
        return None;
    }
    let cg = rk_ir::lower(fir, &input.program.image, &rk_x87::Recipes).ok()?;
    let mut s = rk_ssa::build(
        &cg,
        &BuildOptions {
            policy: Policy::Optimizing,
            call_facts: facts,
            ..BuildOptions::default()
        },
    )
    .ok()?;
    rk_ssa::canonicalize(&mut s, true);
    let accesses = rk_ssa::memory::accesses(&s, None);
    let mut addresses = rk_ssa::address::Addresses::with_parameters(&s);
    let mut derived: LookupSet<_> = s
        .values
        .iter()
        .enumerate()
        .filter_map(|(id, value)| {
            (matches!(value.op, Op::Input(key) if s.field(key)
            .is_some_and(|field| (field as usize) < 8))
                || value.op == Op::P(OpCode::Load)
                    && matches!(addresses.get(s.args(id as u32)[1]),
                Address::Known { base: Base::Frame, offset, span: 0 } if (offset as i32) >= 4))
            .then_some(id as u32)
        })
        .collect();
    for (id, value) in s.values.iter().enumerate() {
        let Op::StateReload(key) = value.op else {
            continue;
        };
        if !s.field(key).is_some_and(|field| (field as usize) < 8) {
            continue;
        }
        let call = s.args(id as u32)[0];
        let target = match s.value(call).op {
            Op::Call { target } => Some(target),
            _ => None,
        };
        let site = s.value_addr[call as usize];
        let effect = facts.call(site, target);
        if effect
            .reads
            .iter()
            .any(|field| (field as usize) < 8 && field != Field::Esp)
            || target
                .and_then(|target| returns.functions.get(&target))
                .is_none_or(|summary| summary.reads.iter().any(|&(offset, _)| offset >= 4))
        {
            derived.insert(id as u32);
        }
    }
    loop {
        let old = derived.len();
        for (id, value) in s.values.iter().enumerate() {
            if value.size != 0
                && rk_ir::properties(value.op).pure
                && s.args(id as u32).iter().any(|arg| derived.contains(arg))
            {
                derived.insert(id as u32);
            }
        }
        if old == derived.len() {
            break;
        }
    }
    let frame_slots: BTreeSet<_> = accesses
        .iter()
        .filter_map(|(&id, access)| {
            access.location?;
            match addresses.get(s.args(id)[1]) {
                Address::Known {
                    base: Base::Frame,
                    offset,
                    span: 0,
                } => Some(offset),
                _ => None,
            }
        })
        .collect();
    let mut local = BorrowedStack::default();
    let mut calls = Vec::new();
    for &id in s.blocks.iter().flat_map(|block| &block.ops) {
        let op = s.value(id).op;
        if matches!(op, Op::Call { .. } | Op::CallInd) {
            let site = s.value_addr[id as usize];
            let targets = match op {
                Op::Call { target } => vec![target],
                _ => input.indirect.get(&site)?.clone(),
            };
            if targets.is_empty() {
                return None;
            }
            let frame = addresses.call_frame(id);
            let mut bases = BTreeMap::from([(Base::Frame, frame)]);
            for field in Field::ALL
                .into_iter()
                .take(8)
                .filter(|&field| field != Field::Esp)
            {
                bases.insert(Base::Parameter(field as u8), addresses.state(id, field));
            }
            if let Address::Known {
                base: Base::Frame,
                offset,
                span: 0,
            } = frame
            {
                for &slot in &frame_slots {
                    let argument = slot.wrapping_sub(offset) as i32;
                    if argument >= 4 {
                        bases.insert(Base::Argument(argument), addresses.argument(id, argument));
                    }
                }
            }
            calls.push(Call { targets, bases });
            continue;
        }
        if rk_ir::properties(op).opaque
            || matches!(
                op,
                Op::Tail { .. }
                    | Op::ImportTail { .. }
                    | Op::BranchInd
                    | Op::Seh(_)
                    | Op::Breakpoint
            )
        {
            return None;
        }
        if op == Op::Return {
            for field in Field::ALL
                .into_iter()
                .take(8)
                .filter(|&field| field != Field::Esp)
            {
                match addresses.state(id, field) {
                    Address::Known {
                        base: base @ (Base::Frame | Base::Parameter(_) | Base::Argument(_)),
                        offset,
                        span: 0,
                    } => {
                        local.capture(base, offset as i32);
                    }
                    _ if s.boundary(id).is_some_and(|state| {
                        state.iter().any(|&(key, value)| {
                            s.field(key) == Some(field) && derived.contains(&value)
                        })
                    }) =>
                    {
                        for &(_, value) in s
                            .boundary(id)?
                            .iter()
                            .filter(|&&(key, _)| s.field(key) == Some(field))
                        {
                            let sources = capture_sources(&s, &mut addresses, &derived, value)?;
                            if sources.contains(&Base::Frame) {
                                return None;
                            }
                            local.unknown_captures.extend(sources);
                        }
                    }
                    _ => {}
                }
            }
        }
        let Some(access) = accesses.get(&id) else {
            continue;
        };
        let location = access.location?;
        let address = addresses.get(s.args(id)[1]);
        let range = Range::new(address, location.size);
        match address {
            Address::Known {
                base: Base::Frame | Base::Absolute | Base::Parameter(_) | Base::Argument(_),
                ..
            } => {
                if !access.reads.ranges.is_empty() || access.reads.opaque {
                    local.effects.reads.insert(range);
                }
                if !access.writes.ranges.is_empty() || access.writes.opaque {
                    local.effects.writes.insert(range);
                }
            }
            _ if derived.contains(&s.args(id)[1]) => return None,
            _ => {}
        }
        if op == Op::P(OpCode::Store) && derived.contains(&s.args(id)[2]) {
            match addresses.get(s.args(id)[2]) {
                Address::Known {
                    base: base @ (Base::Frame | Base::Parameter(_) | Base::Argument(_)),
                    offset,
                    span: 0,
                } => {
                    local.capture(base, offset as i32);
                }
                Address::Known {
                    base: Base::Absolute,
                    ..
                } => {}
                _ => return None,
            }
        }
    }
    Some(Prepared { local, calls })
}

fn compose(
    body: &Prepared,
    known: &BTreeMap<Addr, BorrowedStack>,
    current: &BTreeMap<Addr, BorrowedStack>,
) -> Option<BorrowedStack> {
    let mut result = body.local.clone();
    for call in &body.calls {
        for target in &call.targets {
            let callee = current.get(target).or_else(|| known.get(target))?;
            for &base in &callee.unknown_captures {
                match call.address(Address::exact(base, 0)) {
                    Address::Known {
                        base: base @ (Base::Parameter(_) | Base::Argument(_)),
                        ..
                    } => {
                        result.unknown_captures.insert(base);
                    }
                    Address::Known {
                        base: Base::Absolute,
                        ..
                    } => {}
                    _ => return None,
                }
            }
            for (source, output) in [
                (&callee.effects.reads, &mut result.effects.reads),
                (&callee.effects.writes, &mut result.effects.writes),
            ] {
                output.opaque |= source.opaque;
                for range in &source.ranges {
                    output.insert(Range::new(call.address(range.address), range.size));
                }
            }
            for (&base, &offset) in &callee.captures {
                match call.address(Address::exact(base, offset as u32)) {
                    Address::Known {
                        base: base @ (Base::Frame | Base::Parameter(_) | Base::Argument(_)),
                        offset,
                        span: 0,
                    } => {
                        result.capture(base, offset as i32);
                    }
                    Address::Known {
                        base: Base::Absolute,
                        ..
                    } => {}
                    _ => return None,
                }
            }
        }
    }
    Some(result)
}

fn checkpoints(
    fir: &FunctionIr,
    indirect: &BTreeMap<Addr, Vec<Addr>>,
    known: &BTreeMap<Addr, u32>,
) -> Option<u32> {
    let flow = CallGraph::from_succ((0..fir.insns.len() as u32).collect(), fir.succ.clone());
    let mut costs = vec![0u32; fir.insns.len()];
    for (index, insn) in fir.insns.iter().enumerate() {
        for op in &insn.ops {
            let targets = match op.opcode {
                OpCode::Call => vec![op.inputs.first()?.offset as Addr],
                OpCode::CallInd => indirect.get(&insn.addr)?.clone(),
                _ => continue,
            };
            let cost = targets
                .iter()
                .map(|target| known.get(target)?.checked_add(1))
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .max()?;
            costs[index] = costs[index].checked_add(cost)?;
        }
    }
    let mut path = vec![0u32; costs.len()];
    for members in &flow.components {
        let cyclic = members.len() > 1 || flow.succ[members[0] as usize].contains(&members[0]);
        if cyclic && members.iter().any(|&node| costs[node as usize] != 0) {
            return None;
        }
        let exits = members
            .iter()
            .flat_map(|&node| &flow.succ[node as usize])
            .filter(|node| members.binary_search(node).is_err())
            .map(|&node| path[node as usize])
            .max()
            .unwrap_or(0);
        let cost = members.iter().try_fold(exits, |total, &node| {
            total.checked_add(costs[node as usize])
        })?;
        for &node in members {
            path[node as usize] = cost;
        }
    }
    path.get(fir.index_of(fir.addr)? as usize).copied()
}

fn checkpoint_bounds(
    input: &GlobalInput<'_>,
    excluded: &BTreeSet<Addr>,
    returns: &stack::Analysis,
) -> BTreeMap<Addr, u32> {
    let graph = CallGraph::build(
        input.analysis,
        input.indirect,
        input.indirect_jumps,
        |addr| {
            !excluded.contains(&addr)
                && returns
                    .functions
                    .get(&addr)
                    .is_some_and(|summary| summary.returns == stack::ReturnEffect::Exact)
        },
    );
    let mut known = BTreeMap::new();
    for members in &graph.components {
        if members.len() != 1 || graph.succ[members[0] as usize].contains(&members[0]) {
            continue;
        }
        let addr = graph.nodes[members[0] as usize];
        let Some(Ok(fir)) = input.analysis.ir.get(&addr) else {
            continue;
        };
        if !fir.entries.is_empty()
            || !fir.dynamic.is_empty()
            || !fir.seh.is_empty()
            || !fir.seh_handlers.is_empty()
            || !fir.seh_unwind_only.is_empty()
            || !fir.import_tails.is_empty()
            || !crate::cfg::closed(fir, &Default::default())
            || fir.insns.iter().any(|insn| {
                insn.ops
                    .iter()
                    .any(|op| matches!(op.opcode, OpCode::CallOther | OpCode::Unknown))
            })
        {
            continue;
        }
        if let Some(cost) = checkpoints(fir, input.indirect, &known) {
            known.insert(addr, cost);
        }
    }
    known
}

pub fn analyze(
    input: &GlobalInput<'_>,
    excluded: &BTreeSet<Addr>,
    facts: &dyn CallFacts,
    returns: &stack::Analysis,
) -> Analysis {
    let prepared: BTreeMap<_, _> = input
        .analysis
        .ir
        .par_iter()
        .filter(|(addr, _)| !excluded.contains(addr))
        .filter_map(|(&addr, _)| prepare(input, addr, facts, returns).map(|body| (addr, body)))
        .collect();
    let graph = CallGraph::build(
        input.analysis,
        input.indirect,
        input.indirect_jumps,
        |addr| prepared.contains_key(&addr),
    );
    let mut functions = BTreeMap::new();
    for members in &graph.components {
        let mut current: BTreeMap<_, _> = members
            .iter()
            .map(|&node| {
                let addr = graph.nodes[node as usize];
                (addr, prepared[&addr].local.clone())
            })
            .collect();
        let mut converged = false;
        for _ in 0..128 {
            let next = current
                .keys()
                .filter_map(|&addr| {
                    compose(&prepared[&addr], &functions, &current).map(|summary| (addr, summary))
                })
                .collect();
            converged = current == next;
            current = next;
            if converged {
                break;
            }
        }
        if converged {
            functions.extend(current);
        }
    }
    let checkpoints = checkpoint_bounds(input, excluded, returns);
    for (&addr, summary) in &mut functions {
        summary.checkpoints = checkpoints.get(&addr).copied();
    }
    let sites = input
        .indirect
        .iter()
        .filter_map(|(&site, targets)| {
            if targets.is_empty() {
                return None;
            }
            let mut result = BorrowedStack {
                checkpoints: Some(0),
                ..BorrowedStack::default()
            };
            for target in targets {
                let summary = functions.get(target)?;
                result.effects.join(&summary.effects);
                result.unknown_captures.extend(&summary.unknown_captures);
                result.checkpoints = result
                    .checkpoints
                    .zip(summary.checkpoints)
                    .map(|(a, b)| a.max(b));
                for (&base, &offset) in &summary.captures {
                    result.capture(base, offset);
                }
            }
            Some((site, result))
        })
        .collect();
    Analysis {
        functions,
        sites,
        checkpoints,
    }
}
