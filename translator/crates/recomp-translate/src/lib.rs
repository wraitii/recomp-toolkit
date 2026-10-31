use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};
use clap::ValueEnum;
use rayon::prelude::*;
use rk_core::contract::{DirectArguments, Field, Fields};
use rk_core::ir::{FunctionIr, OpCode, Space};
use rk_core::program::Program;
use rk_core::settings::Settings;
use rk_core::{Addr, Diagnostic, Hex};
use rk_emit::{Body, Requires};

pub mod facts;
mod stack_policy;
mod stack_returns;

#[derive(Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum X87Mode {
    #[default]
    Auto,
    Dynamic,
    Fast,
}

impl From<X87Mode> for rk_x87::ModeForce {
    fn from(mode: X87Mode) -> Self {
        match mode {
            X87Mode::Auto => Self::Auto,
            X87Mode::Dynamic => Self::Dynamic,
            X87Mode::Fast => Self::Fast,
        }
    }
}

fn parse_addr8(text: &str) -> Option<Addr> {
    (text.len() == 8 && text.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| Addr::from_str_radix(text, 16).ok())
        .flatten()
}

type BodyRow = (
    Addr,
    Body,
    Option<(String, Option<String>)>,
    Option<rk_emit::EmitReport>,
);

#[derive(Default)]
pub struct Sites {
    pub functions: usize,
    pub sites: Vec<Addr>,
}

fn tally(
    functions: &mut usize,
    by_reason: &mut BTreeMap<String, Sites>,
    found: BTreeSet<(Addr, String)>,
) {
    if found.is_empty() {
        return;
    }
    *functions += 1;
    let mut reasons = BTreeSet::new();
    for (site, reason) in found {
        let entry = by_reason.entry(reason.clone()).or_default();
        if reasons.insert(reason) {
            entry.functions += 1;
        }
        entry.sites.push(site);
    }
}

pub fn sites_json(functions: usize, by_reason: &BTreeMap<String, Sites>) -> serde_json::Value {
    serde_json::json!({
        "functions": functions,
        "reasons": by_reason.iter().map(|(reason, found)| {
            let mut sites = found.sites.clone();
            sites.sort_unstable();
            (reason.clone(), serde_json::json!({
                "functions": found.functions,
                "count": sites.len(),
                "sites": sites.iter().map(|&site| Hex(site).to_string()).collect::<Vec<_>>(),
            }))
        }).collect::<BTreeMap<_, _>>(),
    })
}

pub fn return_slot_json(
    slots: &BTreeMap<Addr, std::result::Result<(), &'static str>>,
) -> serde_json::Value {
    let mut reasons: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (addr, slot) in slots {
        if let Err(reason) = slot {
            reasons
                .entry(reason)
                .or_default()
                .push(Hex(*addr).to_string());
        }
    }
    serde_json::json!({
        "functions": slots.len(),
        "proven": slots.values().filter(|slot| slot.is_ok()).count(),
        "unproven": reasons.iter().map(|(reason, addrs)| {
            (*reason, serde_json::json!({ "count": addrs.len(), "functions": addrs }))
        }).collect::<BTreeMap<_, _>>(),
    })
}

pub fn summary_line(by_reason: &BTreeMap<String, Sites>) -> String {
    by_reason
        .iter()
        .map(|(reason, found)| format!("{reason}={}", found.functions))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Default)]
pub struct EmitStats {
    pub emitted: usize,
    pub trapping: usize,
    pub rejections: BTreeMap<String, usize>,
    pub details: BTreeMap<String, usize>,
    pub detail_addrs: BTreeMap<String, Vec<Addr>>,
    pub fallback_functions: usize,
    pub fallbacks: BTreeMap<String, Sites>,
    pub slow_op_functions: usize,
    pub slow_ops: BTreeMap<String, Sites>,
    pub c_field_residency_violations: usize,
    pub c_field_residency_violation_addrs: Vec<Addr>,
    pub promotion_failures: BTreeMap<Addr, String>,
    pub return_slots: BTreeMap<Addr, std::result::Result<(), &'static str>>,
    pub direct_arguments: BTreeMap<Addr, DirectArguments>,
}

pub struct IrSummary {
    pub diagnostics: Vec<Diagnostic>,
    pub lifted: usize,
    pub instructions: usize,
    pub call_returns: Vec<Addr>,
}

impl IrSummary {
    pub fn new(analysis: &rk_frontend::Analysis) -> Self {
        let diagnostics: Vec<Diagnostic> = analysis
            .ir
            .values()
            .filter_map(|r| r.as_ref().err())
            .filter_map(|e| match e {
                rk_core::Error::Diagnostic(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        let lifted = analysis.ir.values().filter(|r| r.is_ok()).count();
        let instructions: usize = analysis
            .ir
            .values()
            .filter_map(|r| r.as_ref().ok())
            .map(|ir| ir.insns.len())
            .sum();
        let call_returns: Vec<Addr> = analysis
            .ir
            .values()
            .filter_map(|r| r.as_ref().ok())
            .flat_map(|ir| ir.call_returns.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Self {
            diagnostics,
            lifted,
            instructions,
            call_returns,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Switches {
    pub direct: bool,
    pub descriptors: bool,
    pub coalesce_roots: bool,
    pub x87_dynamic: bool,
}

struct BodyContext<'a> {
    program: &'a Program,
    executable_sha256: &'a str,
    facts: rk_global::ContractCallFacts<'a>,
    effect_returns: &'a BTreeSet<Addr>,
    proven_returns: BTreeSet<Addr>,
    known_modes: &'a BTreeSet<Addr>,
    x87_modes: rk_x87::ModeInput,
    x87_options: rk_x87::Options,
    fault_state: rk_core::settings::FaultState,
    assume_private_stack: bool,
    lazy_stack_writes: bool,
    return_stack_inputs: &'a serde_json::Value,
    return_stack_policy: rk_core::settings::ReturnStackPolicy,
    return_stack_functions: &'a BTreeSet<Addr>,
    return_stack_opt_out: &'a BTreeSet<Addr>,
    assume_stack_pointer_bases: bool,
    call_hooks: bool,
    exposed_stack_functions: &'a BTreeSet<Addr>,
    known_entries: &'a BTreeSet<Addr>,
    setjmp_targets: &'a BTreeSet<Addr>,
    guest_calls: &'a BTreeSet<Addr>,
    host_calls: &'a rk_emit::HostCalls,
    direct_targets: &'a BTreeSet<Addr>,
    direct_arguments: BTreeMap<Addr, DirectArguments>,
    switches: Switches,
    reloc: Option<rk_emit::RelocEmit>,
}

impl<'a> BodyContext<'a> {
    fn plan_options(&self, addr: Addr) -> rk_plan::PlanOptions<'_> {
        rk_plan::PlanOptions {
            call_facts: &self.facts,
            policy: rk_core::ir::Policy::Optimizing,
            x87_modes: &self.x87_modes,
            x87_options: self.x87_options.clone(),
            fault_state: self.fault_state,
            direct_targets: self.direct_targets,
            direct_arguments: &self.direct_arguments,
            descriptors: self.switches.descriptors,
            coalesce_roots: self.switches.coalesce_roots,
            lazy_stack_writes: self.lazy_stack_writes,
            assume_stack_pointer_bases: self.assume_stack_pointer_bases,
            call_hooks: self.call_hooks,
            assume_private_stack: self.assume_private_stack
                && !self.exposed_stack_functions.contains(&addr),
        }
    }

    fn emit_context<'b>(
        &'b self,
        ir: &'b FunctionIr,
        direct: Option<&'b rk_emit::DirectEmit>,
    ) -> rk_emit::EmitContext<'b> {
        rk_emit::EmitContext {
            ir,
            effect_returns: self.effect_returns,
            proven_returns: &self.proven_returns,
            known_modes: self.known_modes,
            direct_targets: self.direct_targets,
            direct_arguments: &self.direct_arguments,
            known_entries: self.known_entries,
            region_entries: &self.program.region_entries,
            setjmp_targets: self.setjmp_targets,
            guest_calls: self.guest_calls,
            host_calls: self.host_calls,
            direct,
            reloc: self.reloc,
            call_hooks: self.call_hooks,
        }
    }
}

fn plan_one(ctx: &BodyContext<'_>, ir: &FunctionIr) -> rk_core::Result<rk_plan::PlannedFunction> {
    let cg = rk_ir::lower_with(ir, &rk_x87::Recipes, ctx.reloc.is_some())?;
    let options = ctx.plan_options(ir.addr);
    let enabled = match ctx.return_stack_policy {
        rk_core::settings::ReturnStackPolicy::Off => false,
        rk_core::settings::ReturnStackPolicy::NormalCLifetime => {
            ctx.return_stack_functions.contains(&ir.addr)
        }
        rk_core::settings::ReturnStackPolicy::Yolo => true,
    } && !ctx.return_stack_opt_out.contains(&ir.addr)
        && options.assume_private_stack;
    if !enabled {
        return rk_plan::plan_function_with_view(&cg, &options, None);
    }
    let mut view = None;
    let planned = rk_plan::plan_function_with_view(
        &cg,
        &options,
        Some(&mut |ssa| view = Some(rk_ssa::memory::ReturnStackSummary::new(ssa, &ctx.facts))),
    )?;
    let Some(view) = view else { return Ok(planned) };
    let omissions = rk_plan::return_stack_omissions(&planned, &view);
    rk_plan::omit_return_stack(planned, omissions, &options)
}

fn error_reason(error: &rk_core::Error) -> (String, Option<String>) {
    let reason = match error {
        rk_core::Error::Diagnostic(diagnostic) => {
            let value = serde_json::to_value(&diagnostic.reason).ok();
            value
                .as_ref()
                .and_then(|value| {
                    value
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "unknown".to_string())
        }
        other => other.to_string(),
    };
    let detail = match error {
        rk_core::Error::Diagnostic(diagnostic) => serde_json::to_value(&diagnostic.reason)
            .ok()
            .and_then(|value| {
                value
                    .get("detail")
                    .and_then(|d| d.as_str())
                    .map(str::to_string)
            }),
        _ => None,
    };
    (reason, detail)
}

fn error_row(
    ctx: &BodyContext<'_>,
    addr: Addr,
    ir: &FunctionIr,
    error: &rk_core::Error,
) -> BodyRow {
    (
        addr,
        trap_body(ctx.program, addr, Some(ir)),
        Some(error_reason(error)),
        None,
    )
}

fn emit_row(
    ctx: &BodyContext<'_>,
    addr: Addr,
    ir: &FunctionIr,
    planned: &rk_plan::PlannedFunction,
) -> BodyRow {
    let emit_ctx = ctx.emit_context(ir, None);
    match rk_emit::emit_body(planned, &emit_ctx) {
        Ok((mut body, mut emit_report)) => {
            let covered: BTreeSet<Addr> =
                planned.ssa.entries.iter().map(|entry| entry.addr).collect();
            for entry in ctx
                .program
                .interior_by_function
                .get(&addr)
                .into_iter()
                .flatten()
            {
                if !covered.contains(entry) {
                    emit_report
                        .fallbacks
                        .insert((*entry, "uncovered entry".to_string()));
                    body.text.push_str(&format!(
                        "\nvoid raw_{entry:08x}(X86 *c) {{ recomp_unmodelled(c, 0x{entry:08x}u); }}\nvoid fn_{entry:08x}(X86 *c) {{ raw_{entry:08x}(c); }}"
                    ));
                }
            }
            (addr, body, None, Some(emit_report))
        }
        Err(error) => error_row(ctx, addr, ir, &error),
    }
}

enum Prepared {
    Emitted(BodyRow),
    Retained {
        addr: Addr,
        ir: FunctionIr,
        planned: rk_plan::PlannedFunction,
        view: Option<anyhow::Result<stack_returns::Body>>,
    },
}

fn account(stats: &mut EmitStats, (addr, body, reason, report): BodyRow, bodies: &mut Vec<Body>) {
    if let Some(report) = report {
        if report.c_field_residency_violations != 0 {
            stats.c_field_residency_violations += report.c_field_residency_violations;
            stats.c_field_residency_violation_addrs.push(addr);
        }
        if let Some(failure) = report.promotion_failure {
            stats.promotion_failures.insert(addr, failure);
        }
        stats.return_slots.insert(addr, report.return_slot);
        tally(
            &mut stats.fallback_functions,
            &mut stats.fallbacks,
            report.fallbacks,
        );
        tally(
            &mut stats.slow_op_functions,
            &mut stats.slow_ops,
            report.slow_ops,
        );
    }
    if let Some((reason, detail)) = reason {
        stats.trapping += 1;
        *stats.rejections.entry(reason).or_default() += 1;
        if let Some(detail) = detail {
            *stats.details.entry(detail.clone()).or_default() += 1;
            stats.detail_addrs.entry(detail).or_default().push(addr);
        }
    } else {
        stats.emitted += 1;
    }
    bodies.push(body);
}

fn effect_returns(stack: &rk_global::stack::Analysis) -> BTreeSet<Addr> {
    stack
        .functions
        .iter()
        .filter_map(|(&addr, summary)| {
            (summary.returns != rk_global::stack::ReturnEffect::Exact).then_some(addr)
        })
        .chain(stack.unbalanced_returns.iter().copied())
        .collect()
}

pub fn prepare_return_entries(
    analysis: &mut rk_frontend::Analysis,
    stack: &rk_global::stack::Analysis,
) -> BTreeMap<Addr, Addr> {
    let mut entries = BTreeMap::new();
    for (&caller, result) in &mut analysis.ir {
        let Ok(fir) = result else { continue };
        let mut invalid = None;
        let mut added = false;
        for insn in &fir.insns {
            if insn.mnemonic != "CALL" {
                continue;
            }
            let target = insn.ops.iter().find_map(|op| {
                matches!(op.opcode, OpCode::Call | OpCode::Branch)
                    .then_some(op.inputs.first())
                    .flatten()
                    .filter(|input| matches!(input.space, Space::Ram | Space::Const))
                    .map(|input| input.offset as Addr)
            });
            let Some(summary) = target.and_then(|target| stack.functions.get(&target)) else {
                continue;
            };
            let rk_global::stack::ReturnEffect::Adjust(delta) = summary.returns else {
                continue;
            };
            let continuation = insn.addr + insn.length as Addr;
            let adjusted = continuation.wrapping_add(delta as u32);
            if fir.index_of(adjusted).is_none() {
                invalid = Some(Diagnostic::new(
                    insn.addr,
                    rk_core::Reason::UnsupportedControlFlow {
                        detail: "return adjustment leaves translated block".into(),
                    },
                ));
                break;
            }
            if !fir.entries.contains(&adjusted) {
                fir.entries.push(adjusted);
                added = true;
            }
            entries.insert(adjusted, caller);
        }
        if let Some(diagnostic) = invalid {
            *result = Err(rk_core::Error::Diagnostic(diagnostic));
        } else if added {
            fir.entries.sort_unstable();
            fir.entries.dedup();
        }
    }
    entries
}

fn build_bodies(
    program: &Program,
    analysis: &rk_frontend::Analysis,
    owned: Option<BTreeMap<Addr, std::result::Result<FunctionIr, rk_core::Error>>>,
    global: &rk_global::GlobalFacts,
    guest_calls: &BTreeSet<Addr>,
    host_calls: &rk_emit::HostCalls,
    settings: &Settings,
    x87_mode: X87Mode,
    switches: Switches,
    only: &[Addr],
    reloc: Option<rk_emit::RelocEmit>,
    stack_return_report: Option<&std::path::Path>,
    return_stack_inputs: &serde_json::Value,
    native: &BTreeSet<Addr>,
) -> Result<(Vec<Body>, EmitStats, BTreeSet<Addr>)> {
    let known_entries: BTreeSet<Addr> = program
        .functions
        .keys()
        .chain(program.entries.keys())
        .filter(|addr| !program.region_entries.contains_key(addr))
        .copied()
        .collect();
    let setjmp_targets: BTreeSet<Addr> = program
        .functions
        .keys()
        .copied()
        .filter(|&addr| program.is_setjmp_thunk(addr))
        .collect();
    let direct_targets: BTreeSet<Addr> = if switches.direct {
        global.direct.targets.keys().copied().collect()
    } else {
        BTreeSet::new()
    };
    let effect_returns = effect_returns(&global.returns);
    let direct_arguments: BTreeMap<Addr, DirectArguments> = settings
        .direct_arguments
        .keys()
        .filter(|addr| direct_targets.contains(addr))
        .map(|&addr| {
            (
                addr,
                global
                    .direct
                    .input_arguments
                    .get(&addr)
                    .copied()
                    .unwrap_or_default(),
            )
        })
        .collect();
    let st0_targets: BTreeSet<Addr> = direct_arguments
        .iter()
        .filter(|(_, arguments)| arguments.st0)
        .map(|(&addr, _)| addr)
        .collect();
    let make_ctx = |proven_returns, direct_arguments| BodyContext {
        program,
        executable_sha256: &settings.sha256,
        facts: global.call_facts(),
        effect_returns: &effect_returns,
        proven_returns,
        known_modes: &global.direct.known_modes,
        x87_modes: rk_x87::ModeInput::from(&global.x87),
        x87_options: rk_x87::Options {
            strict: settings.fault_state == rk_core::settings::FaultState::Exact,
            convention: settings.msvc_x87_convention,
            cw_clone: settings.x87_cw_clone,
            force: x87_mode.into(),
            st0_targets: st0_targets.clone(),
            st0_argument: false,
        },
        fault_state: settings.fault_state,
        assume_private_stack: settings.assume_private_stack,
        lazy_stack_writes: settings.lazy_stack_writes,
        return_stack_inputs,
        return_stack_policy: settings.return_stack_policy,
        return_stack_functions: &settings.return_stack_functions,
        return_stack_opt_out: &settings.return_stack_opt_out,
        assume_stack_pointer_bases: settings.assume_stack_pointer_bases,
        call_hooks: settings.call_hooks,
        exposed_stack_functions: &settings.exposed_stack_functions,
        known_entries: &known_entries,
        setjmp_targets: &setjmp_targets,
        guest_calls,
        host_calls,
        direct_targets: &direct_targets,
        direct_arguments,
        switches,
        reloc,
    };
    let ctx = make_ctx(BTreeSet::new(), direct_arguments.clone());
    let selected: BTreeSet<Addr> = only.iter().copied().collect();
    let wanted = |addr: Addr| {
        stack_return_report.is_some() || selected.is_empty() || selected.contains(&addr)
    };
    let sources: Vec<(Addr, Option<FunctionIr>)> = match owned {
        Some(irs) => irs
            .into_iter()
            .filter(|(addr, _)| wanted(*addr))
            .map(|(addr, result)| (addr, result.ok()))
            .collect(),
        None => program
            .functions
            .keys()
            .copied()
            .filter(|addr| wanted(*addr))
            .map(|addr| {
                let ir = analysis
                    .ir
                    .get(&addr)
                    .and_then(|result| result.as_ref().ok().cloned());
                (addr, ir)
            })
            .collect(),
    };
    let planning_started = std::time::Instant::now();
    let prepared: Vec<Prepared> = sources
        .into_par_iter()
        .map(|(addr, ir)| {
            let Some(ir) = ir else {
                return Prepared::Emitted((addr, trap_body(program, addr, None), None, None));
            };
            let mut view = None;
            let planned = if stack_return_report.is_some() {
                rk_ir::lower_with(&ir, &rk_x87::Recipes, ctx.reloc.is_some()).and_then(|cg| {
                    rk_plan::plan_function_with_view(
                        &cg,
                        &ctx.plan_options(ir.addr),
                        Some(&mut |ssa| view = Some(stack_returns::capture(ssa, &ctx.facts))),
                    )
                })
            } else {
                plan_one(&ctx, &ir)
            };
            match planned {
                Ok(planned) => Prepared::Retained {
                    addr,
                    ir,
                    planned,
                    view,
                },
                Err(error) => Prepared::Emitted(error_row(&ctx, addr, &ir, &error)),
            }
        })
        .collect();
    if let Some(path) = stack_return_report {
        eprintln!(
            "stack-return planning/capture: {:.3}s",
            planning_started.elapsed().as_secs_f64()
        );
        stack_returns::write(&ctx, prepared, &selected, native, path)?;
        return Ok((Vec::new(), EmitStats::default(), BTreeSet::new()));
    }
    let mut proven_returns = BTreeSet::new();
    let mut retained = Vec::new();
    for item in prepared {
        match item {
            Prepared::Emitted(row) => retained.push(Err(row)),
            Prepared::Retained {
                addr, ir, planned, ..
            } => {
                if planned.ssa.return_slot.is_ok() {
                    proven_returns.insert(addr);
                }
                retained.push(Ok((addr, ir, planned)));
            }
        }
    }
    let direct_arguments = if settings.direct_variants {
        let functions = retained
            .iter()
            .filter_map(|item| {
                item.as_ref()
                    .ok()
                    .map(|(addr, _, planned)| (*addr, planned))
            })
            .collect();
        rk_plan::select_arguments(&functions, &direct_targets, &direct_arguments)
    } else {
        direct_arguments
    };
    let ctx2 = make_ctx(proven_returns, direct_arguments.clone());
    let phase2: Vec<(Vec<BodyRow>, Vec<Body>)> = retained
        .into_par_iter()
        .map(|item| -> Result<(Vec<BodyRow>, Vec<Body>)> {
            match item {
                Err(row) => Ok((vec![row], Vec::new())),
                Ok((addr, ir, mut planned)) => {
                    let changed = planned.ssa.values.iter().any(|value| {
                        matches!(value.op, rk_ir::Op::Call { target } if ctx.direct_arguments.get(&target) != direct_arguments.get(&target))
                    });
                    if changed {
                        planned = rk_plan::replan_function(planned, &ctx2.plan_options(addr))?;
                    }
                    let rows = vec![emit_row(&ctx2, addr, &ir, &planned)];
                    let mut direct_bodies = Vec::new();
                    if direct_targets.contains(&addr) {
                        planned = rk_plan::defer_return_eip(planned, &ctx2.plan_options(addr))?;
                        let fast = global.direct.known_modes.contains(&addr)
                            && ctx2.x87_options.cw_clone
                            && ctx2.x87_options.force == rk_x87::ModeForce::Auto;
                        let st0 = direct_arguments
                            .get(&addr)
                            .is_some_and(|arguments| arguments.st0);
                        if fast || st0 {
                            let mut options = ctx2.x87_options.clone();
                            if fast {
                                options.force = rk_x87::ModeForce::Fast;
                            }
                            options.st0_argument = st0;
                            planned.x87 = Some(
                                rk_x87::prepare(&planned.ssa, &ctx2.x87_modes, &options)
                                    .map_err(|reason| Diagnostic::error(addr, reason))?,
                            );
                        }
                        let arguments = direct_arguments.get(&addr).copied().unwrap_or_default();
                        let direct = rk_emit::DirectEmit {
                            inputs: (global.contracts[&addr].reads
                                | Fields(Field::Esp.bit())
                                | Fields(Field::Ebp.bit()))
                            .difference(arguments.registers),
                            input_arguments: arguments,
                        };
                        let emit_ctx = ctx2.emit_context(&ir, Some(&direct));
                        match rk_emit::emit_body(&planned, &emit_ctx) {
                            Ok((body, _)) => direct_bodies.push(body),
                            Err(error) => bail!("direct body {addr:08x} emission failed: {error}"),
                        }
                    }
                    Ok((rows, direct_bodies))
                }
            }
        })
        .collect::<Result<_>>()?;
    let mut stats = EmitStats {
        direct_arguments,
        ..EmitStats::default()
    };
    let mut bodies = Vec::new();
    let mut direct_bodies = Vec::new();
    for (rows, direct) in phase2 {
        for row in rows {
            account(&mut stats, row, &mut bodies);
        }
        direct_bodies.extend(direct);
    }
    bodies.sort_by_key(|body| body.addr);
    direct_bodies.sort_by_key(|body| body.addr);
    bodies.extend(direct_bodies);
    Ok((bodies, stats, direct_targets))
}

fn trap_body(program: &Program, addr: Addr, ir: Option<&FunctionIr>) -> Body {
    let mut text = String::new();
    for entry in std::iter::once(&addr).chain(
        program
            .interior_by_function
            .get(&addr)
            .into_iter()
            .flatten(),
    ) {
        text.push_str(&format!(
            "void raw_{entry:08x}(X86 *c) {{ recomp_unmodelled(c, 0x{entry:08x}u); }}\nvoid fn_{entry:08x}(X86 *c) {{ raw_{entry:08x}(c); }}\n"
        ));
    }
    if program.region_entries.values().any(|&owner| owner == addr) {
        text.push_str(&format!(
            "\nvoid region_{addr:08x}(X86 *c, uint32_t target) {{ recomp_unmodelled(c, target); }}"
        ));
    }
    let (insns, first, last) = ir
        .map(|ir| {
            let first = ir.insns.first().map_or(addr, |i| i.addr);
            let last = ir.insns.last().map_or(addr, |i| i.addr);
            (ir.insns.len(), first, last)
        })
        .unwrap_or((0, addr, addr));
    Body {
        addr,
        insns,
        first,
        last,
        text,
        requires: Requires::default(),
        declarations: Default::default(),
    }
}

#[derive(Clone, Copy)]
pub struct EmitOptions<'a> {
    pub x87_mode: X87Mode,
    pub switches: Switches,
    pub only: &'a [Addr],
    pub reloc: Option<rk_emit::RelocEmit>,
    pub return_stack_inputs: Option<&'a serde_json::Value>,
}

impl Default for Switches {
    fn default() -> Self {
        Self {
            direct: true,
            descriptors: true,
            coalesce_roots: true,
            x87_dynamic: false,
        }
    }
}

impl Default for EmitOptions<'_> {
    fn default() -> Self {
        Self {
            x87_mode: X87Mode::Auto,
            switches: Switches::default(),
            only: &[],
            reloc: None,
            return_stack_inputs: None,
        }
    }
}

pub struct Job<'a> {
    pub program: &'a Program,
    pub settings: &'a Settings,
    pub native: &'a BTreeSet<Addr>,
}

pub struct JobOutput {
    pub analysis: rk_frontend::Analysis,
    pub global: rk_global::GlobalFacts,
    pub extra_entries: BTreeMap<Addr, Addr>,
    pub bodies: Vec<Body>,
    pub stats: EmitStats,
    pub direct_variants: BTreeSet<Addr>,
}

impl Job<'_> {
    pub fn lift(&self) -> rk_frontend::Analysis {
        rk_frontend::analyze(self.program)
    }

    pub fn global(
        &self,
        analysis: &rk_frontend::Analysis,
        facts: &facts::Facts,
    ) -> Result<rk_global::GlobalFacts> {
        Ok(rk_global::GlobalFacts::analyze_with_native(
            &rk_global::GlobalInput {
                settings: self.settings,
                program: self.program,
                analysis,
                indirect: &facts.call_targets,
                indirect_jumps: &facts.jump_targets,
                site_effects: &facts.site_effects,
                fact_summary: &facts.summary,
            },
            self.native,
        )?)
    }

    pub fn emit(
        &self,
        analysis: &rk_frontend::Analysis,
        owned: Option<BTreeMap<Addr, std::result::Result<FunctionIr, rk_core::Error>>>,
        global: &rk_global::GlobalFacts,
        facts: &facts::Facts,
        options: EmitOptions<'_>,
    ) -> Result<(Vec<Body>, EmitStats, BTreeSet<Addr>)> {
        build_bodies(
            self.program,
            analysis,
            owned,
            global,
            &facts.guest_calls,
            &facts.host_calls,
            self.settings,
            options.x87_mode,
            options.switches,
            options.only,
            options.reloc,
            None,
            options
                .return_stack_inputs
                .unwrap_or(&serde_json::Value::Null),
            self.native,
        )
    }

    pub fn stack_return_report(
        &self,
        analysis: &rk_frontend::Analysis,
        owned: Option<BTreeMap<Addr, std::result::Result<FunctionIr, rk_core::Error>>>,
        global: &rk_global::GlobalFacts,
        facts: &facts::Facts,
        options: EmitOptions<'_>,
        path: &std::path::Path,
    ) -> Result<()> {
        build_bodies(
            self.program,
            analysis,
            owned,
            global,
            &facts.guest_calls,
            &facts.host_calls,
            self.settings,
            options.x87_mode,
            options.switches,
            options.only,
            options.reloc,
            Some(path),
            options
                .return_stack_inputs
                .unwrap_or(&serde_json::Value::Null),
            self.native,
        )?;
        Ok(())
    }

    pub fn translate(&self, facts: &facts::Facts, options: EmitOptions<'_>) -> Result<JobOutput> {
        let mut analysis = self.lift();
        let global = self.global(&analysis, facts)?;
        let extra_entries = prepare_return_entries(&mut analysis, &global.returns);
        let (bodies, stats, direct_variants) =
            self.emit(&analysis, None, &global, facts, options)?;
        Ok(JobOutput {
            analysis,
            global,
            extra_entries,
            bodies,
            stats,
            direct_variants,
        })
    }
}

pub fn offline_stack_inputs(
    settings: &Settings,
    switches: Switches,
    x87_mode: X87Mode,
    report: bool,
) -> Result<serde_json::Value> {
    let mut inputs = if report
        || settings.return_stack_policy == rk_core::settings::ReturnStackPolicy::NormalCLifetime
    {
        stack_policy::fingerprint(settings)?
    } else {
        serde_json::Value::Null
    };
    if !inputs.is_null() {
        inputs["planning"] = serde_json::json!({"direct": switches.direct, "descriptors": switches.descriptors,
            "coalesce_roots": switches.coalesce_roots, "x87_dynamic": switches.x87_dynamic, "x87_mode": x87_mode as u8});
    }
    if !report {
        stack_policy::validate(settings, &inputs)?;
    }
    Ok(inputs)
}
