use std::collections::{BTreeMap, BTreeSet};

use rayon::prelude::*;
use rk_core::facts::Summary as FactSummary;
use rk_core::program::Program;
use rk_core::settings::{CallEffect as ConfiguredEffect, Settings};
use rk_core::{Addr, Result};
use rk_ir::Op;
use rk_ssa::{prepare, PrepareOptions, Ssa, ValueId};

use crate::AnalysisConfig;

mod stack;
mod summary;
mod tables;

pub use summary::{Base, Ref, Shape, ShapeKind};

pub(crate) fn com_slots(
    program: &Program,
    analysis: &rk_frontend::Analysis,
    owner: Addr,
) -> std::result::Result<BTreeMap<Addr, i32>, String> {
    let result = analysis
        .ir
        .get(&owner)
        .ok_or("COM owner was not analyzed")?;
    let summary = summary::summarize(program, owner, result, &rk_ssa::ConservativeCallFacts);
    if let Some(reason) = summary.blocked() {
        return Err(format!("COM slot analysis blocked: {reason}"));
    }
    Ok(summary
        .sites
        .into_iter()
        .filter_map(|shape| {
            (shape.kind == ShapeKind::Call && matches!(shape.form, summary::ShapeForm::Vptr { .. }))
                .then_some((shape.site, shape.slot))
        })
        .collect())
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub bounds: bool,
    pub assume_constant_vptr_stores: bool,
    pub assume_compatible_cleanup: bool,
    pub assume_balanced_returns: bool,
    pub reuse_discovery_seeds: bool,
}

#[derive(Clone, Debug)]
pub struct Site {
    pub site: Addr,
    pub kind: &'static str,
    pub targets: Vec<Addr>,
}

#[derive(Clone, Debug)]
pub struct Assumption {
    pub id: String,
    pub text: String,
    pub requires: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Output {
    pub sites: Vec<Site>,
    pub assumptions: Vec<Assumption>,
    pub report: serde_json::Value,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ObservedSite {
    pub targets: BTreeSet<Addr>,
    pub blocked: bool,
}

impl Options {
    pub fn from_config(config: &AnalysisConfig) -> Self {
        Self {
            bounds: config.guest_bounds,
            assume_constant_vptr_stores: config.assume_constant_vptr_stores,
            assume_compatible_cleanup: config.assume_compatible_cleanup,
            assume_balanced_returns: config.assume_balanced_returns,
            reuse_discovery_seeds: config.reuse_discovery_seeds,
        }
    }
}

pub fn discover_program(settings: &Settings, program: &Program) -> Result<(Program, bool)> {
    let discovered = rk_discover::discover(settings, program)?;
    let converged = discovered.report.converged;
    let mut program = program.clone();
    rk_discover::merge(&mut program, &discovered)?;
    Ok((program, converged))
}

pub fn produce(
    settings: &Settings,
    program: &Program,
    analysis: &rk_frontend::Analysis,
    effects: &BTreeMap<Addr, ConfiguredEffect>,
    observed: &BTreeMap<Addr, ObservedSite>,
    options: Options,
) -> Result<Output> {
    if !options.bounds {
        return Ok(Output::default());
    }
    let started = std::time::Instant::now();
    eprintln!("analysis guest: raw table census");
    let raw = tables::raw_census(program, options.assume_constant_vptr_stores);
    eprintln!("analysis guest: stack cleanup summaries");
    let mut cleanups = stack::analyze(program, analysis);
    let verified_cleanups = cleanups.len();
    if options.assume_compatible_cleanup && options.assume_balanced_returns {
        for (addr, cleanup) in stack::return_immediates(program, analysis) {
            cleanups.entry(addr).or_insert(cleanup);
        }
    }
    let mut indirect = BTreeMap::new();
    let mut indirect_jumps = BTreeMap::new();
    let mut pass = 0usize;
    let mut all_sites: BTreeMap<(Addr, ShapeKind), BTreeSet<Addr>> = BTreeMap::new();
    let mut converged = false;
    let mut output_sites: Vec<Site>;
    let mut report_parts = Vec::new();
    loop {
        pass += 1;
        eprintln!(
            "analysis guest: pass {pass} global contracts ({} call bounds, {} jump bounds)",
            indirect.values().map(Vec::len).sum::<usize>(),
            indirect_jumps.values().map(Vec::len).sum::<usize>(),
        );
        let facts = FactSummary {
            call_bounds: indirect.len(),
            jump_bounds: indirect_jumps.len(),
            ..FactSummary::default()
        };
        let global = rk_global::GlobalFacts::analyze(&rk_global::GlobalInput {
            settings,
            program,
            analysis,
            indirect: &indirect,
            indirect_jumps: &indirect_jumps,
            site_effects: effects,
            fact_summary: &facts,
        })?;
        let contracts = global.inference_call_facts();
        for (&addr, summary) in &global.stack.functions {
            cleanups.insert(addr, summary.effect.cleanup);
        }
        eprintln!(
            "analysis guest: pass {pass} summaries ({} functions, {} contracts)",
            analysis.ir.len(),
            global.report.contracts,
        );
        let summaries: BTreeMap<Addr, summary::FunctionSummary> = analysis
            .ir
            .par_iter()
            .map(|(&addr, result)| {
                let mut summary = summary::summarize(program, addr, result, &contracts);
                summary.cleanup = cleanups.get(&addr).copied();
                (addr, summary)
            })
            .collect();
        eprintln!("analysis guest: pass {pass} table census");
        let census = tables::census(program, &summaries, &raw);
        eprintln!("analysis guest: pass {pass} structural bounds");
        let bounds = tables::bounds(program, &summaries, &census, observed, options);
        let current: BTreeMap<(Addr, ShapeKind), Vec<Addr>> = bounds
            .sites
            .iter()
            .map(|site| ((site.site, site_kind(site.kind)), site.targets.clone()))
            .collect();
        let previous = all_sites.clone();
        let mut rejected_refinements = 0usize;
        for site in &bounds.sites {
            let targets: BTreeSet<Addr> = site.targets.iter().copied().collect();
            match all_sites.entry((site.site, site_kind(site.kind))) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(targets);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let refined: BTreeSet<Addr> =
                        entry.get().intersection(&targets).copied().collect();
                    if refined.is_empty()
                        || observed
                            .get(&site.site)
                            .is_some_and(|row| !row.targets.is_subset(&refined))
                    {
                        rejected_refinements += 1;
                    } else {
                        entry.insert(refined);
                    }
                }
            }
        }
        let retained_sites: Vec<Site> = all_sites
            .iter()
            .map(|(&(site, kind), targets)| Site {
                site,
                kind: kind.name(),
                targets: targets.iter().copied().collect(),
            })
            .collect();
        report_parts.push(serde_json::json!({
            "pass": pass,
            "target_sites": current.len(),
            "target_entries": current.values().map(Vec::len).sum::<usize>(),
            "retained_target_entries": all_sites.values().map(BTreeSet::len).sum::<usize>(),
            "rejected_refinements": rejected_refinements,
            "contracts": global.report.contracts,
            "frame_checked_cleanups": global.report.stack_functions,
            "stack_sites": global.report.stack_sites,
            "stack_readonly_sites": global.report.stack_readonly_sites,
            "stack_mapped_noreturn_sites": global.report.stack_mapped_noreturn_sites,
            "stack_rejected": global.report.stack_rejected,
            "stack_rejected_sites": global.report.stack_rejected_sites,
            "indirect_contracts": global.report.indirect_contracts,
            "preserved_register_fields": global.contracts.values().map(|contract|
                ((contract.preserves | !contract.writes) & rk_core::contract::Fields::GPRS).iter().count()
            ).sum::<usize>(),
            "preserved_flag_fields": global.contracts.values().map(|contract|
                ((contract.preserves | !contract.writes) & rk_core::contract::Fields::FLAGS).iter().count()
            ).sum::<usize>(),
            "cw_return_summaries": global.x87.returns.len(),
            "cw_preserving_functions": global.x87.returns.values().filter(|summary| summary.preserved == 0xffff).count(),
            "cw_known_sites": global.x87.sites.len(),
            "contract_excluded": global.report.contract_excluded,
            "cw_sccs": global.report.sccs,
            "jump_bounds_consumed": global.report.jump_bounds_consumed,
            "summaries": bounds.report,
        }));
        if all_sites == previous {
            converged = true;
            output_sites = retained_sites;
            break;
        }
        indirect = site_map(&retained_sites, ShapeKind::Call);
        indirect_jumps = site_map(&retained_sites, ShapeKind::Jump);
        if pass >= 8 {
            output_sites = all_sites
                .iter()
                .map(|(&(site, kind), targets)| Site {
                    site,
                    kind: kind.name(),
                    targets: targets.iter().copied().collect(),
                })
                .collect();
            break;
        }
    }
    output_sites.sort_by_key(|site| (site.site, site.kind));
    let mut assumptions = Vec::new();
    if !output_sites.is_empty() {
        assumptions.push(Assumption {
            id: "guest-vtable-structure".into(),
            text: "Bounded double-load sites are assumed to dispatch through immutable guest C++ vtables named by decoded image operands, with a nonnegative pointer-sized slot. Dominating earlier calls with identical receiver and vptr SSA values contribute requirements. All compatible table targets are retained; host observations block guest bounds. Observations are not exhaustive target sets.".into(),
            requires: Vec::new(),
        });
        if settings.stack_memory {
            assumptions.push(Assumption {
                id: "original-code-stack-memory".into(),
                text: "Stack and continuation-memory summaries model original guest code without concurrent guest memory interference. Mapped noreturn functions contribute no normal-continuation effects but remain dispatch targets. Calls, their return-address stores, tail calls, saved registers and stack slots are tracked explicitly; unknown writes, opaque/native bodies and unresolved callees block a summary. The game asserts single-threaded execution for this inference. Hooks must respect original contracts or use runtime fallback; no threads are suppressed.".into(),
                requires: vec!["guest-vtable-structure".into()],
            });
        }
        if options.assume_constant_vptr_stores {
            assumptions.push(Assumption {
                id: "constant-vptr-stores".into(),
                text: "Every runtime vtable used at a bounded site is assumed to be installed somewhere in the mapped/discovered code by MOV mem32, immediate32. The table census includes every such byte-decoded store, even when function lifting fails, and is frozen before contract refinement.".into(),
                requires: vec!["guest-vtable-structure".into()],
            });
        }
        if options.assume_compatible_cleanup {
            assumptions.push(Assumption {
                id: "guest-cleanup-compatibility".into(),
                text: "Narrowing by observed targets additionally assumes that known RET cleanup immediates agree across a table prefix. Unknown cleanup imposes no incompatibility and never rejects a candidate. Cleanup is candidate evidence, not a frame-balance proof.".into(),
                requires: vec!["guest-vtable-structure".into()],
            });
            if options.assume_balanced_returns {
                assumptions.push(Assumption {
                    id: "guest-balanced-returns".into(),
                    text: "Additional cleanup compatibility evidence assumes that guest functions restore ESP to their incoming return-address slot before every normal RET. Only closed single-entry functions without exception flow, unknown exits or tail exits and with one byte-checked RET immediate across all reachable returns contribute. This is not a frame-balance or stack-alias proof; unknown cleanup remains compatible.".into(),
                    requires: vec!["guest-cleanup-compatibility".into()],
                });
            }
        }
    }
    let report = serde_json::json!({
        "enabled": true,
        "converged": converged,
        "passes": pass,
        "pass_reports": report_parts,
        "functions": analysis.ir.len(),
        "sites_emitted": output_sites.len(),
        "target_entries": output_sites.iter().map(|site| site.targets.len()).sum::<usize>(),
        "compatible_cleanup_assumption": options.assume_compatible_cleanup,
        "reuse_discovery_seeds": options.reuse_discovery_seeds,
        "raw_table_candidates": raw.len(),
        "verified_stack_cleanups": verified_cleanups,
        "assumed_balanced_cleanups": cleanups.len() - verified_cleanups,
        "balanced_returns_assumption": options.assume_compatible_cleanup && options.assume_balanced_returns,
        "seconds": started.elapsed().as_secs_f64(),
    });
    Ok(Output {
        sites: output_sites,
        assumptions,
        report,
    })
}

fn site_kind(kind: &str) -> ShapeKind {
    if kind == "jump" {
        ShapeKind::Jump
    } else {
        ShapeKind::Call
    }
}

fn site_map(sites: &[Site], kind: ShapeKind) -> BTreeMap<Addr, Vec<Addr>> {
    sites
        .iter()
        .filter(|site| site.kind == kind.name())
        .map(|site| (site.site, site.targets.clone()))
        .collect()
}

pub(crate) fn prepare_ssa<'a>(
    program: &Program,
    result: &std::result::Result<rk_core::ir::FunctionIr, rk_core::Error>,
    contracts: &'a dyn rk_ssa::CallFacts,
) -> std::result::Result<(rk_core::ir::FunctionIr, Ssa), String> {
    let mut fir = result.as_ref().map_err(|error| error.to_string())?.clone();
    if fir.entries.is_empty() && fir.seh.is_empty() && (2..=8).contains(&fir.insns.len()) {
        let last = fir.insns.len() - 1;
        let tail = &fir.insns[last];
        if tail.mnemonic.eq_ignore_ascii_case("JMP")
            && !program.tables.contains_key(&(fir.addr, tail.addr))
            && tail
                .ops
                .iter()
                .any(|op| op.opcode == rk_core::ir::OpCode::BranchInd)
            && fir.insns[..last].iter().enumerate().all(|(i, insn)| {
                matches!(
                    insn.mnemonic.to_ascii_uppercase().as_str(),
                    "MOV" | "LEA" | "ADD" | "SUB" | "NOP"
                ) && fir.succ[i] == [i as u32 + 1]
                    && !fir.exits.contains_key(&(i as u32))
            })
        {
            fir.succ[last].clear();
            fir.exits.insert(last as u32, Vec::new());
            fir.dynamic.remove(&(last as u32));
            fir.tables.remove(&(last as u32));
        }
    }
    let cg =
        rk_ir::lower(&fir, &program.image, &rk_x87::Recipes).map_err(|error| error.to_string())?;
    let options = PrepareOptions::new(contracts);
    let ssa = prepare(&cg, &options).map_err(|error| error.to_string())?;
    Ok((fir, ssa))
}

pub(crate) fn whole<'a>(ssa: &'a Ssa, mut id: ValueId) -> (ValueId, &'a rk_ssa::Value) {
    loop {
        let value = ssa.value(id);
        match value.op {
            Op::P(rk_core::ir::OpCode::Copy) => {
                let args = ssa.args(id);
                if let Some(&next) = args.first() {
                    id = next;
                    continue;
                }
            }
            Op::StateKeep(_) => {
                let args = ssa.args(id);
                if let Some(&next) = args.get(1) {
                    id = next;
                    continue;
                }
            }
            Op::Pack => {
                let args = ssa.args(id);
                if !args.is_empty() && args.len() == value.size as usize {
                    let mut source = None;
                    let mut consistent = true;
                    for (lane, &arg) in args.iter().enumerate() {
                        let item = ssa.value(arg);
                        if item.op != Op::Byte(lane as u8) || item.size != 1 {
                            consistent = false;
                            break;
                        }
                        let inner = ssa.args(arg);
                        let Some(&inner) = inner.first() else {
                            consistent = false;
                            break;
                        };
                        if source.is_some_and(|previous| previous != inner) {
                            consistent = false;
                            break;
                        }
                        source = Some(inner);
                    }
                    if consistent {
                        if let Some(source) = source {
                            if ssa.value(source).size == value.size {
                                id = source;
                                continue;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        return (id, ssa.value(id));
    }
}

pub(crate) fn address_form(ssa: &Ssa, id: ValueId) -> Option<Ref> {
    let (id, value) = whole(ssa, id);
    match value.op {
        Op::P(rk_core::ir::OpCode::IntAdd) | Op::P(rk_core::ir::OpCode::IntSub)
            if value.size == 4 =>
        {
            let args = ssa.args(id);
            if args.len() != 2 {
                return None;
            }
            let (left, right) = (args[0], args[1]);
            let (_, right_value) = whole(ssa, right);
            if let Op::Const(data) = right_value.op {
                let mut form = address_form(ssa, left)?;
                let delta = data as u32 as i32;
                form.offset = if value.op == Op::P(rk_core::ir::OpCode::IntAdd) {
                    form.offset.wrapping_add(delta)
                } else {
                    form.offset.wrapping_sub(delta)
                };
                return Some(form);
            }
            if value.op == Op::P(rk_core::ir::OpCode::IntAdd) {
                let (left_id, left_value) = whole(ssa, left);
                if let Op::Const(data) = left_value.op {
                    let mut form = address_form(ssa, right)?;
                    form.offset = form.offset.wrapping_add(data as u32 as i32);
                    return Some(form);
                }
                let _ = left_id;
            }
            None
        }
        Op::Const(data) => Some(Ref {
            base: Base::Const(data as u32 as Addr),
            offset: 0,
        }),
        _ => Some(Ref {
            base: Base::Value(id),
            offset: 0,
        }),
    }
}
