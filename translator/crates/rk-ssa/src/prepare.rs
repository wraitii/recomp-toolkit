use std::collections::BTreeMap;

use rk_core::ir::Policy;
use rk_core::settings::FaultState;
use rk_core::LookupSet;
use rk_ir::{xmm_offset, Op, StateKey};

use crate::{
    canonicalize, forward, simplify, BuildOptions, CallFacts, ConservativeCallFacts, Ssa, ValueId,
    DEFAULT_VALUE_BUDGET,
};

#[derive(Clone, Copy)]
pub struct PrepareOptions<'a> {
    pub policy: Policy,
    pub call_facts: &'a dyn CallFacts,
    pub allow_slow: bool,
    pub value_budget: usize,
    pub signed_sub: bool,
    pub lazy_publications: bool,
    pub fault_state: FaultState,
    pub coalesce_roots: bool,
    pub assume_private_stack: bool,
    pub lazy_stack_writes: bool,
    pub retire_reused_stack: bool,
    pub assume_stack_pointer_bases: bool,
}

impl Default for PrepareOptions<'static> {
    fn default() -> Self {
        Self::new(&ConservativeCallFacts)
    }
}

impl<'a> PrepareOptions<'a> {
    pub fn new(call_facts: &'a dyn CallFacts) -> Self {
        Self {
            policy: Policy::Optimizing,
            call_facts,
            allow_slow: true,
            value_budget: DEFAULT_VALUE_BUDGET,
            signed_sub: true,
            lazy_publications: true,
            fault_state: FaultState::Relaxed,
            coalesce_roots: true,
            assume_private_stack: false,
            lazy_stack_writes: false,
            retire_reused_stack: false,
            assume_stack_pointer_bases: false,
        }
    }
}

pub fn prepare(cg: &rk_ir::CgFunction, opts: &PrepareOptions) -> rk_core::Result<Ssa> {
    prepare_with_publications(cg, opts).map(|(ssa, _)| ssa)
}

pub fn prepare_with_publications(
    cg: &rk_ir::CgFunction,
    opts: &PrepareOptions,
) -> rk_core::Result<(Ssa, Option<BTreeMap<ValueId, Vec<StateKey>>>)> {
    prepare_using(cg, opts, true, None)
}

pub fn prepare_with_stack_dump(
    cg: &rk_ir::CgFunction,
    opts: &PrepareOptions,
    out: &mut impl std::io::Write,
) -> rk_core::Result<Ssa> {
    prepare_using(cg, opts, true, Some(out)).map(|(ssa, _)| ssa)
}

struct NoCleanup<'a>(&'a dyn CallFacts);

impl CallFacts for NoCleanup<'_> {
    fn readonly_memory(&self, site: rk_core::Addr, target: Option<rk_core::Addr>) -> bool {
        self.0.readonly_memory(site, target)
    }
    fn checkpoints(&self, site: rk_core::Addr, target: Option<rk_core::Addr>) -> Option<u32> {
        self.0.checkpoints(site, target)
    }
    fn call(&self, site: rk_core::Addr, target: Option<rk_core::Addr>) -> crate::CallEffect {
        self.0.call(site, target)
    }
    fn cleanup(&self, _: rk_core::Addr, _: Option<rk_core::Addr>) -> Option<i32> {
        None
    }
    fn stack(
        &self,
        site: rk_core::Addr,
        target: Option<rk_core::Addr>,
    ) -> Option<&crate::StackEffect> {
        self.0.stack(site, target)
    }
    fn memory(
        &self,
        site: rk_core::Addr,
        target: Option<rk_core::Addr>,
    ) -> Option<&rk_core::memory::Effects> {
        self.0.memory(site, target)
    }
    fn borrowed_stack(
        &self,
        site: rk_core::Addr,
        target: Option<rk_core::Addr>,
    ) -> Option<&crate::BorrowedStack> {
        self.0.borrowed_stack(site, target)
    }
}

fn prepare_using(
    cg: &rk_ir::CgFunction,
    opts: &PrepareOptions,
    stack_cleanups: bool,
    mut trace: Option<&mut dyn std::io::Write>,
) -> rk_core::Result<(Ssa, Option<BTreeMap<ValueId, Vec<StateKey>>>)> {
    let build_options = BuildOptions {
        policy: opts.policy,
        call_facts: opts.call_facts,
        allow_slow: opts.allow_slow,
        value_budget: opts.value_budget,
    };
    let lazy_stack_writes = opts.lazy_stack_writes
        && opts.assume_private_stack
        && opts.policy == Policy::Optimizing
        && opts.fault_state == FaultState::Relaxed;
    let mut ssa =
        crate::build::build_with_faults(cg, &build_options, opts.fault_state, lazy_stack_writes)
            .map_err(rk_core::Error::Diagnostic)?;
    crate::validate(&ssa)?;
    ssa.assume_private_stack = opts.assume_private_stack && opts.policy == Policy::Optimizing;
    ssa.assume_stack_pointer_bases = opts.assume_stack_pointer_bases;
    canonicalize(&mut ssa, opts.signed_sub);
    if let Some(out) = trace.as_mut() {
        writeln!(
            out,
            "before memory passes cleanup-inference={stack_cleanups} retirement={}",
            opts.retire_reused_stack
        )
        .and_then(|_| crate::memory::dump_stack(&ssa, opts.call_facts, *out))
        .map_err(|error| rk_core::Error::io("stack dump", error))?;
    }
    if ssa.assume_private_stack && opts.fault_state == FaultState::Relaxed {
        crate::stack::split_x87(&mut ssa, opts.call_facts);
    }
    forward(&mut ssa, Some(opts.call_facts));
    if opts.policy == Policy::Optimizing && opts.fault_state == FaultState::Relaxed {
        crate::memory::eliminate_dead_stores(&mut ssa, Some(opts.call_facts));
    }
    if lazy_stack_writes {
        crate::stack::promote(&mut ssa, opts.retire_reused_stack, opts.call_facts);
    }
    if let Some(out) = trace.as_mut() {
        writeln!(
            out,
            "after memory passes cleanup-inference={stack_cleanups} retirement={}",
            opts.retire_reused_stack
        )
        .and_then(|_| crate::memory::dump_stack(&ssa, opts.call_facts, *out))
        .map_err(|error| rk_core::Error::io("stack dump", error))?;
    }
    if stack_cleanups
        && opts.policy != Policy::Conservative
        && ssa
            .blocks
            .iter()
            .flat_map(|block| &block.ops)
            .any(|&id| matches!(ssa.value(id).op, Op::Call { .. } | Op::CallInd))
        && !ssa
            .blocks
            .iter()
            .flat_map(|block| &block.ops)
            .any(|&id| matches!(ssa.value(id).op, Op::StackLoad | Op::StackStore))
    {
        let facts = NoCleanup(opts.call_facts);
        return prepare_using(
            cg,
            &PrepareOptions {
                call_facts: &facts,
                ..*opts
            },
            false,
            trace,
        );
    }
    crate::flags::build_joins(&mut ssa);
    crate::coalesce::coalesce(&mut ssa, opts.coalesce_roots);
    canonicalize(&mut ssa, opts.signed_sub);
    if opts.fault_state == FaultState::Relaxed {
        crate::stack::refresh_loads(&mut ssa);
    }
    let plans = opts.lazy_publications.then(|| {
        let base = publications(&ssa, opts.call_facts);
        let simplify = ssa
            .boundaries
            .iter()
            .map(|boundary| (boundary.at, ssa.keys.to_vec()))
            .collect();
        (base, simplify)
    });
    let flag_roots: Vec<ValueId> = ssa
        .flags
        .iter()
        .flat_map(|record| match record.kind {
            crate::FlagKind::Logic => vec![record.result],
            crate::FlagKind::Inc | crate::FlagKind::Dec => vec![record.a, record.result],
            crate::FlagKind::Adc | crate::FlagKind::Sbb => {
                vec![record.a, record.b, record.carry, record.result]
            }
            crate::FlagKind::Join => vec![
                record.kind_value,
                record.a,
                record.b,
                record.carry,
                record.result,
            ],
            _ => vec![record.a, record.b, record.result],
        })
        .flatten()
        .collect();
    simplify(
        &mut ssa,
        plans.as_ref().map(|(_, simplify)| simplify),
        &flag_roots,
        &[],
    );
    crate::validate(&ssa)?;
    Ok((ssa, plans.map(|(base, _)| base)))
}

pub fn publications(s: &Ssa, call_facts: &dyn CallFacts) -> BTreeMap<ValueId, Vec<StateKey>> {
    let all: Vec<StateKey> = s.keys.to_vec();
    let mut plan = BTreeMap::new();
    for boundary in &s.boundaries {
        let value = &s.values[boundary.at as usize];
        let fields = match value.op {
            Op::Call { target } => Some(
                call_facts
                    .call(s.value_addr[boundary.at as usize], Some(target))
                    .reads,
            ),
            Op::CallInd => Some(
                call_facts
                    .call(s.value_addr[boundary.at as usize], None)
                    .reads,
            ),
            _ => None,
        };
        let keys: Vec<StateKey> = match fields {
            Some(fields) => {
                let wanted: LookupSet<rk_core::contract::Field> = fields.iter().collect();
                s.keys
                    .iter()
                    .enumerate()
                    .filter(|(index, key)| {
                        **key == StateKey::Memory
                            || xmm_offset(**key).is_some()
                            || s.field_of[*index].is_some_and(|field| wanted.contains(&field))
                    })
                    .map(|(_, key)| *key)
                    .collect()
            }
            None => all.clone(),
        };
        plan.insert(boundary.at, keys);
    }
    plan
}
