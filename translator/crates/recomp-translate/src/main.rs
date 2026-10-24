//! Driver that `tools/build.py` runs once per image. It reads the `recomp-translate-input-v1` JSON,
//! loads the image and code map, discovers uncovered code, lifts every function, validates analysis
//! facts against the lifted code (`facts`), runs whole-program analysis, then plans and emits every
//! function in two parallel passes: the second re-emits callers of private-contract targets once
//! `rk_global::specialize` has merged their requests. A function that fails planning or emission
//! becomes a named trap body. See `docs/translator.md`.

mod facts;
mod reloc;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use rayon::prelude::*;
use rk_core::contract::Fields;
use rk_core::ir::{FunctionIr, OpCode, Space};
use rk_core::program::{EntryKind, Program};
use rk_core::settings::Settings;
use rk_core::timing::Phases;
use rk_core::{parse_hex, Addr, Diagnostic, Hex};
use rk_emit::{Body, Requires};
use rk_output::Translation;

#[derive(Parser)]
#[command(about = "Translate a game image from its code map to C")]
struct Args {
    #[arg(long, help = "resolved translation input written by tools/build.py")]
    input: PathBuf,
    #[arg(long, help = "directory receiving the generated sources")]
    out: Option<PathBuf>,
    #[arg(long, help = "translate one [modules.aux] image")]
    module: Option<String>,
    #[arg(long)]
    report: Option<PathBuf>,
    #[arg(long, help = "translate only the [reloc] manifest into out/reloc")]
    reloc_only: bool,
    #[arg(long)]
    quiet: bool,
    #[arg(long)]
    jobs: Option<usize>,
    #[arg(long, value_parser = parse_addr, help = "restrict dumps to these functions or entries")]
    only: Vec<Addr>,
    #[arg(long, value_enum)]
    dump: Vec<Stage>,
    #[arg(long, value_enum, default_value_t = X87Mode::Auto)]
    x87_mode: X87Mode,
    #[arg(
        long,
        help = "write the discovered code map, instruction lengths and discovery report as JSON"
    )]
    discovery_report: Option<PathBuf>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
enum X87Mode {
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

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Stage {
    Program,
    Fir,
    Ssa,
    Prepared,
    Global,
    Bodies,
    All,
}

impl Args {
    fn wants(&self, stage: Stage) -> bool {
        self.dump.iter().any(|s| *s == stage || *s == Stage::All)
    }
}

fn parse_addr(text: &str) -> std::result::Result<Addr, String> {
    parse_hex(text).ok_or_else(|| format!("{text:?} is not a hexadecimal address"))
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
struct Sites {
    functions: usize,
    sites: Vec<Addr>,
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

fn sites_json(functions: usize, by_reason: &BTreeMap<String, Sites>) -> serde_json::Value {
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

fn summary_line(by_reason: &BTreeMap<String, Sites>) -> String {
    by_reason
        .iter()
        .map(|(reason, found)| format!("{reason}={}", found.functions))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Default)]
struct EmitStats {
    emitted: usize,
    trapping: usize,
    rejections: BTreeMap<String, usize>,
    details: BTreeMap<String, usize>,
    detail_addrs: BTreeMap<String, Vec<Addr>>,
    fallback_functions: usize,
    fallbacks: BTreeMap<String, Sites>,
    slow_op_functions: usize,
    slow_ops: BTreeMap<String, Sites>,
    c_field_residency_violations: usize,
    c_field_residency_violation_addrs: Vec<Addr>,
}

struct IrSummary {
    diagnostics: Vec<Diagnostic>,
    lifted: usize,
    instructions: usize,
    call_returns: Vec<Addr>,
}

impl IrSummary {
    fn new(analysis: &rk_frontend::Analysis) -> Self {
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
struct Switches {
    private: bool,
    descriptors: bool,
    coalesce_roots: bool,
    x87_dynamic: bool,
}

impl Switches {
    fn from_env() -> Self {
        let unset = |name| std::env::var_os(name).is_none();
        Self {
            private: unset("RK_NO_PRIVATE"),
            descriptors: unset("RK_NO_DESCRIPTORS"),
            coalesce_roots: unset("RK_NO_ROOTS"),
            x87_dynamic: !unset("RK_X87_DYNAMIC"),
        }
    }
}

struct BodyContext<'a> {
    program: &'a Program,
    facts: rk_global::ContractCallFacts<'a>,
    checked: &'a BTreeSet<Addr>,
    effect_returns: &'a BTreeSet<Addr>,
    x87_modes: rk_x87::ModeInput,
    x87_options: rk_x87::Options,
    fault_state: rk_core::settings::FaultState,
    assume_private_stack: bool,
    lazy_stack_writes: bool,
    exposed_stack_functions: &'a BTreeSet<Addr>,
    known_entries: &'a BTreeSet<Addr>,
    setjmp_targets: &'a BTreeSet<Addr>,
    guest_calls: &'a BTreeSet<Addr>,
    host_calls: &'a BTreeSet<Addr>,
    private_targets: &'a BTreeSet<Addr>,
    private_masks: BTreeMap<Addr, BTreeSet<Fields>>,
    private_input_arguments: BTreeMap<Addr, String>,
    switches: Switches,
    reloc: Option<rk_emit::RelocEmit>,
}

impl<'a> BodyContext<'a> {
    fn emit_context<'b>(
        &'b self,
        ir: &'b FunctionIr,
        private: Option<&'b rk_emit::PrivateEmit>,
    ) -> rk_emit::EmitContext<'b> {
        rk_emit::EmitContext {
            ir,
            checked_returns: self.checked,
            effect_returns: self.effect_returns,
            private_masks: &self.private_masks,
            private_input_arguments: &self.private_input_arguments,
            known_entries: self.known_entries,
            region_entries: &self.program.region_entries,
            setjmp_targets: self.setjmp_targets,
            guest_calls: self.guest_calls,
            host_calls: self.host_calls,
            private,
            reloc: self.reloc,
        }
    }
}

fn plan_one(ctx: &BodyContext<'_>, ir: &FunctionIr) -> rk_core::Result<rk_plan::PlannedFunction> {
    let cg = rk_ir::lower_with(
        ir,
        &ctx.program.image,
        &rk_x87::Recipes,
        ctx.reloc.is_some(),
    )?;
    rk_plan::plan_function(
        &cg,
        &rk_plan::PlanOptions {
            call_facts: &ctx.facts,
            policy: rk_core::ir::Policy::Optimizing,
            x87_modes: &ctx.x87_modes,
            x87_options: ctx.x87_options.clone(),
            fault_state: ctx.fault_state,
            private_targets: ctx.private_targets,
            descriptors: ctx.switches.descriptors,
            coalesce_roots: ctx.switches.coalesce_roots,
            lazy_stack_writes: ctx.lazy_stack_writes,
            assume_private_stack: ctx.assume_private_stack
                && !ctx.exposed_stack_functions.contains(&ir.addr),
        },
    )
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
                        "\nvoid fn_{entry:08x}(X86 *c) {{ recomp_unmodelled(c, 0x{entry:08x}u); }}"
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
    },
}

fn account(stats: &mut EmitStats, (addr, body, reason, report): BodyRow, bodies: &mut Vec<Body>) {
    if let Some(report) = report {
        if report.c_field_residency_violations != 0 {
            stats.c_field_residency_violations += report.c_field_residency_violations;
            stats.c_field_residency_violation_addrs.push(addr);
        }
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

fn prepare_return_entries(
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
    host_calls: &BTreeSet<Addr>,
    settings: &Settings,
    x87_mode: X87Mode,
    switches: Switches,
    only: &[Addr],
    reloc: Option<rk_emit::RelocEmit>,
) -> Result<(
    Vec<Body>,
    EmitStats,
    BTreeMap<Addr, BTreeSet<Fields>>,
    BTreeMap<Addr, String>,
    BTreeSet<Addr>,
)> {
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
    let private_targets: BTreeSet<Addr> = if switches.private {
        global.private.targets.keys().copied().collect()
    } else {
        BTreeSet::new()
    };
    let effect_returns = effect_returns(&global.returns);
    let make_ctx = |private_masks, private_input_arguments| BodyContext {
        program,
        facts: global.call_facts(),
        checked: &global.private.checked_returns,
        effect_returns: &effect_returns,
        x87_modes: rk_x87::ModeInput::from(&global.x87),
        x87_options: rk_x87::Options {
            strict: settings.fault_state == rk_core::settings::FaultState::Exact,
            convention: settings.msvc_x87_convention,
            cw_clone: settings.x87_cw_clone,
            force: x87_mode.into(),
        },
        fault_state: settings.fault_state,
        assume_private_stack: settings.assume_private_stack,
        lazy_stack_writes: settings.lazy_stack_writes,
        exposed_stack_functions: &settings.exposed_stack_functions,
        known_entries: &known_entries,
        setjmp_targets: &setjmp_targets,
        guest_calls,
        host_calls,
        private_targets: &private_targets,
        private_masks,
        private_input_arguments,
        switches,
        reloc,
    };
    let ctx = make_ctx(BTreeMap::new(), BTreeMap::new());
    let selected: BTreeSet<Addr> = only.iter().copied().collect();
    let wanted = |addr: Addr| selected.is_empty() || selected.contains(&addr);
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
    let prepared: Vec<Prepared> = sources
        .into_par_iter()
        .map(|(addr, ir)| {
            let Some(ir) = ir else {
                return Prepared::Emitted((addr, trap_body(program, addr, None), None, None));
            };
            match plan_one(&ctx, &ir) {
                Ok(planned) => {
                    let retain =
                        !planned.requests.by_caller.is_empty() || private_targets.contains(&addr);
                    if retain {
                        Prepared::Retained { addr, ir, planned }
                    } else {
                        Prepared::Emitted(emit_row(&ctx, addr, &ir, &planned))
                    }
                }
                Err(error) => Prepared::Emitted(error_row(&ctx, addr, &ir, &error)),
            }
        })
        .collect();
    let mut requests = rk_plan::ContractRequests::default();
    let mut retained = Vec::new();
    for item in prepared {
        match item {
            Prepared::Emitted(row) => retained.push(Err(row)),
            Prepared::Retained { addr, ir, planned } => {
                for (caller, wanted) in &planned.requests.by_caller {
                    requests
                        .by_caller
                        .entry(*caller)
                        .or_default()
                        .extend(wanted.iter().copied());
                }
                retained.push(Ok((addr, ir, planned)));
            }
        }
    }
    let specialized = rk_global::specialize(
        &requests.by_caller,
        &global.contracts,
        &global.private.input_arguments,
    );
    let private_arguments: BTreeMap<Addr, String> = specialized
        .input_arguments
        .iter()
        .map(|(&addr, &field)| (addr, field.name().to_string()))
        .collect();
    let ctx2 = make_ctx(specialized.masks.clone(), private_arguments);
    let phase2: Vec<(Vec<BodyRow>, Vec<Body>)> = retained
        .into_par_iter()
        .map(|item| -> Result<(Vec<BodyRow>, Vec<Body>)> {
            match item {
                Err(row) => Ok((vec![row], Vec::new())),
                Ok((addr, ir, mut planned)) => {
                    let rows = vec![emit_row(&ctx2, addr, &ir, &planned)];
                    let mut private_bodies = Vec::new();
                    if let Some(layouts) = specialized.layouts.get(&addr) {
                        if global.private.known_modes.contains(&addr)
                            && ctx2.x87_options.cw_clone
                            && ctx2.x87_options.force == rk_x87::ModeForce::Auto
                        {
                            let mut options = ctx2.x87_options.clone();
                            options.force = rk_x87::ModeForce::Fast;
                            planned.x87 = Some(
                                rk_x87::prepare(&planned.ssa, &ctx2.x87_modes, &options)
                                    .map_err(|reason| Diagnostic::error(addr, reason))?,
                            );
                        }
                        for layout in layouts {
                            let private = rk_emit::PrivateEmit {
                                mask: layout.outputs,
                                inputs: layout.inputs,
                                input_argument: layout.input_argument,
                            };
                            let emit_ctx = ctx2.emit_context(&ir, Some(&private));
                            match rk_emit::emit_body(&planned, &emit_ctx) {
                                Ok((body, _)) => private_bodies.push(body),
                                Err(error) => {
                                    bail!(
                                        "private body {addr:08x}/{:04x} emission failed: {error}",
                                        layout.outputs.0
                                    );
                                }
                            }
                        }
                    }
                    Ok((rows, private_bodies))
                }
            }
        })
        .collect::<Result<_>>()?;
    let mut stats = EmitStats::default();
    let mut bodies = Vec::new();
    let mut private_bodies = Vec::new();
    for (rows, private) in phase2 {
        for row in rows {
            account(&mut stats, row, &mut bodies);
        }
        private_bodies.extend(private);
    }
    bodies.sort_by_key(|body| body.addr);
    private_bodies.sort_by_key(|body| (body.addr, body.private_mask.unwrap_or_default()));
    bodies.extend(private_bodies);
    Ok((
        bodies,
        stats,
        specialized.masks,
        specialized
            .input_arguments
            .iter()
            .map(|(&addr, &field)| (addr, field.name().to_string()))
            .collect(),
        global.private.known_modes.clone(),
    ))
}

fn trap_body(program: &Program, addr: Addr, ir: Option<&FunctionIr>) -> Body {
    let mut text = format!("void fn_{addr:08x}(X86 *c) {{ recomp_unmodelled(c, 0x{addr:08x}u); }}");
    for entry in program
        .interior_by_function
        .get(&addr)
        .into_iter()
        .flatten()
    {
        text.push_str(&format!(
            "\nvoid fn_{entry:08x}(X86 *c) {{ recomp_unmodelled(c, 0x{entry:08x}u); }}"
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
        private_mask: None,
    }
}

fn dump_program(program: &Program, only: &[Addr], out: &mut impl std::io::Write) -> Result<()> {
    for &addr in only {
        let owner = program
            .entry_owner(addr)
            .with_context(|| format!("{} is not inside a mapped function", Hex(addr)))?;
        let function = &program.functions[&owner];
        writeln!(out, "== program {}", Hex(owner))?;
        writeln!(
            out,
            "noreturn {}  provisional {}",
            function.noreturn, function.provisional
        )?;
        for span in &function.spans {
            let lengths = span
                .lengths
                .as_ref()
                .map(|l| format!("  explicit lengths {}", l.len()))
                .unwrap_or_default();
            writeln!(
                out,
                "span {}..{}{lengths}",
                Hex(span.start),
                Hex(span.end())
            )?;
        }
        for entry in program
            .interior_by_function
            .get(&owner)
            .into_iter()
            .flatten()
        {
            writeln!(
                out,
                "entry {} {}",
                Hex(*entry),
                program.entries[entry].name()
            )?;
        }
        for ((function, site), targets) in program.tables.range((owner, 0)..=(owner, Addr::MAX)) {
            debug_assert_eq!(*function, owner);
            let targets: Vec<String> = targets.iter().map(|t| Hex(*t).to_string()).collect();
            writeln!(out, "table {} -> {}", Hex(*site), targets.join(","))?;
        }
    }
    Ok(())
}

fn dump_fir(
    program: &Program,
    analysis: &rk_frontend::Analysis,
    only: &[Addr],
    out: &mut impl std::io::Write,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for &addr in only {
        let owner = program
            .entry_owner(addr)
            .with_context(|| format!("{} is not inside a mapped function", Hex(addr)))?;
        let Some(Ok(ir)) = analysis.ir.get(&owner) else {
            writeln!(out, "== function {owner:08x}: no function IR")?;
            continue;
        };
        if seen.insert(ir.addr) {
            rk_frontend::dump(ir, out)?;
        }
    }
    Ok(())
}

fn dump_ssa(
    program: &Program,
    analysis: &rk_frontend::Analysis,
    settings: &Settings,
    only: &[Addr],
    prepared: bool,
    switches: Switches,
    out: &mut impl std::io::Write,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for &addr in only {
        let owner = program
            .entry_owner(addr)
            .with_context(|| format!("{} is not inside a mapped function", Hex(addr)))?;
        let Some(Ok(ir)) = analysis.ir.get(&owner) else {
            writeln!(out, "== ssa {owner:08x}: no function IR")?;
            continue;
        };
        if !seen.insert(owner) {
            continue;
        }
        let cg = rk_ir::lower(ir, &program.image, &rk_ir::NoX87)?;
        if prepared {
            let options = rk_ssa::PrepareOptions {
                coalesce_roots: switches.coalesce_roots,
                lazy_stack_writes: settings.lazy_stack_writes,
                assume_private_stack: settings.assume_private_stack
                    && !settings.exposed_stack_functions.contains(&owner),
                ..Default::default()
            };
            match rk_ssa::prepare(&cg, &options) {
                Ok(ssa) => rk_ssa::dump(&ssa, out)?,
                Err(error) => writeln!(out, "== prepared {owner:08x}: {error}")?,
            }
            continue;
        }
        let options = rk_ssa::BuildOptions {
            policy: rk_core::ir::Policy::Conservative,
            call_facts: &rk_ssa::ConservativeCallFacts,
            allow_slow: true,
            value_budget: rk_ssa::DEFAULT_VALUE_BUDGET,
        };
        match rk_ssa::build(&cg, &options) {
            Ok(ssa) => rk_ssa::dump(&ssa, out)?,
            Err(diagnostic) => writeln!(out, "== ssa {owner:08x}: {diagnostic:?}")?,
        }
    }
    Ok(())
}

fn dump_global(
    program: &Program,
    global: &rk_global::GlobalFacts,
    only: &[Addr],
    out: &mut impl std::io::Write,
) -> Result<()> {
    let mut text = Vec::new();
    global.dump(&mut text)?;
    let text = String::from_utf8_lossy(&text);
    let owners: BTreeSet<String> = only
        .iter()
        .filter_map(|&a| program.entry_owner(a))
        .map(|a| format!("{a:08x}"))
        .collect();
    for line in text.lines() {
        if owners.is_empty() || owners.iter().any(|o| line.contains(o.as_str())) {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

fn dump_bodies(
    program: &Program,
    bodies: &[Body],
    only: &[Addr],
    out: &mut impl std::io::Write,
) -> Result<()> {
    let selected: BTreeSet<Addr> = only
        .iter()
        .map(|&addr| {
            program
                .entry_owner(addr)
                .with_context(|| format!("{} is not inside a mapped function", Hex(addr)))
        })
        .collect::<Result<_>>()?;
    for body in bodies.iter().filter(|body| selected.contains(&body.addr)) {
        writeln!(out, "== body {}", Hex(body.addr))?;
        writeln!(out, "{}", body.text)?;
    }
    Ok(())
}

struct ReportInputs<'a> {
    settings: &'a Settings,
    program: &'a Program,
    analysis: &'a rk_frontend::Analysis,
    summary: &'a IrSummary,
    global: &'a rk_global::GlobalReport,
    emitted: &'a EmitStats,
    traps: &'a BTreeMap<String, String>,
    written: &'a rk_output::Written,
    phases: &'a Phases,
}

fn report(input: &ReportInputs<'_>) -> Result<String> {
    let ReportInputs {
        settings,
        program,
        analysis,
        summary,
        global,
        emitted,
        traps,
        written,
        phases,
    } = input;
    let diagnostics = &summary.diagnostics;
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for row in &written.symbols {
        *kinds.entry(row.kind).or_default() += 1;
    }
    let mut histogram: BTreeMap<String, usize> = BTreeMap::new();
    for diagnostic in diagnostics {
        let reason = serde_json::to_value(&diagnostic.reason)?;
        let name = reason["reason"].as_str().unwrap_or("unknown").to_string();
        *histogram.entry(name).or_default() += 1;
    }
    let lifted = summary.lifted;
    let instructions = summary.instructions;
    let call_returns = summary.call_returns.len();
    let json = serde_json::json!({
        "engine": "rust",
        "module": settings.module.as_ref().map(|m| m.key.as_str()),
        "functions": program.functions.len(),
        "entry_points": written.symbols.len(),
        "region_entries": program.region_entries.len(),
        "entry_kinds": kinds,
        "body_chunks": written.chunks,
        "entry_chunks": written.entry_chunks,
        "lifted": lifted,
        "instructions": instructions,
        "call_returns": call_returns,
        "seh_helpers": analysis.seh_helpers.len(),
        "global": {
            "stack_functions": global.stack_functions,
            "stack_sites": global.stack_sites,
            "stack_readonly_sites": global.stack_readonly_sites,
            "stack_mapped_noreturn_sites": global.stack_mapped_noreturn_sites,
            "stack_rejected": global.stack_rejected,
            "stack_rejected_sites": global.stack_rejected_sites,
            "contracts": global.contracts,
            "contract_excluded": global.contract_excluded,
            "indirect_contracts": global.indirect_contracts,
            "call_bounds": global.call_bounds,
            "jump_bounds": global.jump_bounds,
            "jump_bounds_consumed": global.jump_bounds_consumed,
            "jump_bounds_unsupported": global.jump_bounds_unsupported,
            "site_effects": global.site_effects,
            "import_effects": global.import_effects,
            "fact_evidence": global.fact_evidence,
            "sccs": global.sccs,
            "private_targets": global.private_targets,
            "private_variants": global.private_variants,
            "checked_returns": global.checked_returns,
            "selected_callers": global.selected_callers,
            "known_modes": global.known_modes,
            "instruction_checks": global.instruction_checks,
            "instruction_mismatches": global.instruction_mismatches.len(),
        },
        "bodies": {
            "trapping": emitted.trapping,
            "emitted": emitted.emitted,
            "rejections": emitted.rejections,
            "details": emitted.details,
            "detail_addrs": emitted.detail_addrs,
            "c_field_residency_violations": emitted.c_field_residency_violations,
            "c_field_residency_violation_addrs": emitted.c_field_residency_violation_addrs,
        },
        "fallbacks": sites_json(emitted.fallback_functions, &emitted.fallbacks),
        "slow_ops": sites_json(emitted.slow_op_functions, &emitted.slow_ops),
        "discovery_traps": traps,
        "diagnostics": diagnostics,
        "diagnostics_histogram": histogram,
        "phases": phases.seconds,
        "seconds": phases.total(),
    });
    Ok(serde_json::to_string_pretty(&json)? + "\n")
}

fn write_discovery_report(
    path: &std::path::Path,
    settings: &Settings,
    program: &Program,
    discovered: &rk_discover::Discovered,
) -> Result<()> {
    let functions = discovered
        .functions
        .iter()
        .map(|(&addr, spans)| {
            let spans = spans
                .iter()
                .map(|span| {
                    serde_json::json!({
                        "start": Hex(span.start).to_string(),
                        "end": Hex(span.end()).to_string(),
                        "bytes": span.size,
                        "lengths": span.lengths,
                    })
                })
                .collect::<Vec<_>>();
            (Hex(addr).to_string(), spans)
        })
        .collect::<BTreeMap<_, _>>();
    let entries = discovered
        .entries
        .iter()
        .map(|(&addr, &(owner, kind))| {
            (
                Hex(addr).to_string(),
                serde_json::json!({ "owner": Hex(owner).to_string(), "kind": kind }),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let instructions = discovered
        .instruction_lengths
        .iter()
        .map(|(&addr, &length)| (Hex(addr).to_string(), length))
        .collect::<BTreeMap<_, _>>();
    let mapped_functions = program
        .functions
        .iter()
        .flat_map(|(&owner, function)| {
            function.spans.iter().map(move |span| {
                serde_json::json!({
                    "owner": Hex(owner).to_string(),
                    "start": Hex(span.start).to_string(),
                    "end": Hex(span.end()).to_string(),
                })
            })
        })
        .collect::<Vec<_>>();
    let mapped_entries = program
        .entries
        .iter()
        .map(|(&addr, &kind)| (Hex(addr).to_string(), kind))
        .collect::<BTreeMap<_, _>>();
    let mapped_tables = program
        .tables
        .iter()
        .map(|(&(owner, site), targets)| {
            serde_json::json!({
                "owner": Hex(owner).to_string(),
                "site": Hex(site).to_string(),
                "targets": targets.iter().map(|&target| Hex(target).to_string()).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    let exec_ranges = program
        .image
        .exec_ranges
        .iter()
        .map(|&(lo, hi)| [Hex(lo).to_string(), Hex(hi).to_string()])
        .collect::<Vec<_>>();
    let json = serde_json::json!({
        "format": "recomp-discovery-v1",
        "executable": settings.exe.to_string_lossy(),
        "module": settings.module.as_ref().map(|module| module.key.as_str()),
        "image": {
            "base": Hex(program.image.base).to_string(),
            "size": program.image.size,
            "exec_ranges": exec_ranges,
        },
        "functions": functions,
        "entries": entries,
        "instructions": instructions,
        "mapped": {
            "functions": mapped_functions,
            "entries": mapped_entries,
            "tables": mapped_tables,
        },
        "report": discovered.report,
    });
    std::fs::write(path, serde_json::to_string_pretty(&json)? + "\n")
        .with_context(|| path.display().to_string())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let switches = Switches::from_env();
    if let Some(jobs) = args.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(jobs.max(1))
            .build_global()?;
    }
    let mut phases = Phases::new(args.quiet);
    let settings = Settings::load(&args.input, args.module.as_deref())?;
    phases.mark("settings");
    if args.reloc_only {
        let Some(out) = args.out.as_ref() else {
            bail!("--out is required with --reloc-only");
        };
        let Some(manifest) = &settings.reloc else {
            bail!("--reloc-only needs a [reloc] manifest in the translation input");
        };
        let report = args
            .report
            .clone()
            .unwrap_or_else(|| out.join("translate-reloc-report.json"));
        reloc::translate(&settings, manifest, out, &report, switches)?;
        return Ok(());
    }
    let mut program = rk_image::load(&settings)?;
    phases.mark("load");
    let discovered = rk_discover::discover(&settings, &program)?;
    if let Some(path) = &args.discovery_report {
        write_discovery_report(path, &settings, &program, &discovered)?;
    }
    rk_discover::merge(&mut program, &discovered)?;
    phases.mark("discover");
    let mut analysis = rk_frontend::analyze(&program);
    phases.mark("lift");
    let summary = IrSummary::new(&analysis);
    let facts = facts::load(&settings, &program, &analysis)?;
    phases.mark("facts");
    let whole = args.out.is_some() || args.dump.is_empty();
    let wants_global = whole || args.wants(Stage::Global);
    let mut global = if wants_global {
        rk_global::GlobalFacts::analyze(&rk_global::GlobalInput {
            settings: &settings,
            program: &program,
            analysis: &analysis,
            indirect: &facts.call_targets,
            indirect_jumps: &facts.jump_targets,
            site_effects: &facts.site_effects,
            fact_summary: &facts.summary,
        })?
    } else {
        rk_global::GlobalFacts::default()
    };
    phases.mark("global");
    let extra_entries = if wants_global {
        prepare_return_entries(&mut analysis, &global.returns)
    } else {
        BTreeMap::new()
    };
    let effect_returns = effect_returns(&global.returns);
    global
        .private
        .checked_returns
        .extend(effect_returns.iter().copied());
    let mut pending: Vec<_> = global.private.checked_returns.iter().copied().collect();
    while let Some(addr) = pending.pop() {
        let Some(Ok(fir)) = analysis.ir.get(&addr) else {
            continue;
        };
        for (&index, targets) in &fir.exits {
            if fir.insns[index as usize].mnemonic != "JMP" {
                continue;
            }
            for &target in targets {
                if matches!(analysis.ir.get(&target), Some(Ok(_)))
                    && global.private.checked_returns.insert(target)
                {
                    pending.push(target);
                }
            }
        }
    }
    let only = if whole { &[][..] } else { &args.only[..] };
    let wants_bodies = whole || args.wants(Stage::Bodies);
    let owned = (wants_bodies && args.dump.is_empty()).then(|| std::mem::take(&mut analysis.ir));
    let (bodies, emit_stats, private_masks, private_input_arguments, known_modes) = if wants_bodies
    {
        build_bodies(
            &program,
            &analysis,
            owned,
            &global,
            &facts.guest_calls,
            &facts.host_calls,
            &settings,
            if switches.x87_dynamic {
                X87Mode::Dynamic
            } else {
                args.x87_mode
            },
            switches,
            only,
            None,
        )?
    } else {
        (
            Vec::new(),
            EmitStats::default(),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeSet::new(),
        )
    };
    phases.mark("emit");
    for (addr, owner) in extra_entries {
        program.entries.entry(addr).or_insert(EntryKind::Discovered);
        let entries = program.interior_by_function.entry(owner).or_default();
        if !entries.contains(&addr) {
            entries.push(addr);
            entries.sort_unstable();
        }
    }
    let call_returns = summary.call_returns.clone();
    let translation = Translation {
        settings: &settings,
        program: &program,
        bodies,
        preludes: rk_emit::preludes(),
        call_returns,
        call_facts: facts.runtime_call_facts,
        cw_effects: facts.cw_effects,
        checked_returns: global.private.checked_returns.clone(),
        private_masks,
        private_input_arguments,
        known_modes,
    };

    if !args.dump.is_empty() {
        if args.only.is_empty() && args.dump.iter().any(|s| !matches!(s, Stage::Global)) {
            bail!("--dump needs at least one --only address, except --dump global");
        }
        let mut out = std::io::stdout().lock();
        if args.wants(Stage::Program) {
            dump_program(&program, &args.only, &mut out)?;
        }
        if args.wants(Stage::Fir) {
            dump_fir(&program, &analysis, &args.only, &mut out)?;
        }
        if args.wants(Stage::Ssa) {
            dump_ssa(
                &program, &analysis, &settings, &args.only, false, switches, &mut out,
            )?;
        }
        if args.wants(Stage::Prepared) {
            dump_ssa(
                &program, &analysis, &settings, &args.only, true, switches, &mut out,
            )?;
        }
        if args.wants(Stage::Global) {
            dump_global(&program, &global, &args.only, &mut out)?;
        }
        if args.wants(Stage::Bodies) {
            dump_bodies(&program, &translation.bodies, &args.only, &mut out)?;
        }
        out.flush()?;
        if args.out.is_none() {
            return Ok(());
        }
    }

    let Some(out) = args.out.as_ref() else {
        bail!("--out is required unless dumping");
    };
    let written = rk_output::write(out, &translation)?;
    phases.mark("write");

    if !args.quiet {
        let kinds: Vec<String> = {
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            for row in &written.symbols {
                *counts.entry(row.kind).or_default() += 1;
            }
            counts
                .into_iter()
                .map(|(k, n)| format!("{n} {k}"))
                .collect()
        };
        eprintln!(
            "  {} functions, {} entries ({}), {} body chunks, {} entry chunks; {} bodies emitted, {} trapped",
            program.functions.len(),
            written.symbols.len(),
            kinds.join(", "),
            written.chunks,
            written.entry_chunks,
            emit_stats.emitted,
            emit_stats.trapping
        );
        if !emit_stats.rejections.is_empty() {
            let rows: Vec<String> = emit_stats
                .rejections
                .iter()
                .map(|(reason, count)| format!("{reason}={count}"))
                .collect();
            eprintln!("  body rejections: {}", rows.join(", "));
        }
        if emit_stats.fallback_functions != 0 {
            eprintln!(
                "  {} functions with fallbacks: {}",
                emit_stats.fallback_functions,
                summary_line(&emit_stats.fallbacks)
            );
        }
        if emit_stats.slow_op_functions != 0 {
            eprintln!(
                "  {} functions with slow ops: {}",
                emit_stats.slow_op_functions,
                summary_line(&emit_stats.slow_ops)
            );
        }
        if !discovered.report.traps.is_empty() {
            eprintln!("  {} discovery traps", discovered.report.traps.len());
        }
    }
    if let Some(path) = &args.report {
        let mut global_report = global.report.clone();
        global_report.private_variants =
            translation.private_masks.values().map(BTreeSet::len).sum();
        std::fs::write(
            path,
            report(&ReportInputs {
                settings: &settings,
                program: &program,
                analysis: &analysis,
                summary: &summary,
                global: &global_report,
                emitted: &emit_stats,
                traps: &discovered.report.traps,
                written: &written,
                phases: &phases,
            })?,
        )
        .with_context(|| path.display().to_string())?;
        if args.module.is_none() {
            if let Some(manifest) = &settings.reloc {
                let reloc_report = path.with_file_name("translate-reloc-report.json");
                reloc::translate(&settings, manifest, out, &reloc_report, switches)?;
            }
        }
    }
    Ok(())
}
