//! Driver that `tools/build.py` runs once per image. It reads the `recomp-translate-input-v1` JSON,
//! loads the image and code map, discovers uncovered code, lifts every function, validates analysis
//! facts against the lifted code (`facts`), runs whole-program analysis, then plans every function
//! and emits them in a second parallel pass, once return-slot proofs are known. A function that fails planning or emission
//! becomes a named trap body. See `docs/translator.md`.

mod reloc;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use recomp_translate::{
    facts, prepare_return_entries, return_slot_json, sites_json, summary_line, EmitOptions,
    EmitStats, IrSummary, Job, Switches, X87Mode,
};
use rk_core::contract::Field;
use rk_core::program::{EntryKind, Program};
use rk_core::settings::Settings;
use rk_core::timing::Phases;
use rk_core::{parse_hex, Addr, Hex};
use rk_emit::Body;
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
    #[arg(
        long,
        help = "JSON selection containing functions with hexadecimal addr fields"
    )]
    selection: Option<PathBuf>,
    #[arg(long, value_enum)]
    dump: Vec<Stage>,
    #[arg(long, value_enum, default_value_t = X87Mode::Auto)]
    x87_mode: X87Mode,
    #[arg(
        long,
        help = "write the discovered code map, instruction lengths and discovery report as JSON"
    )]
    discovery_report: Option<PathBuf>,
    #[arg(
        long,
        help = "write a read-only report of stack publications at returns"
    )]
    stack_return_report: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Stage {
    Program,
    Fir,
    Ssa,
    Prepared,
    Stack,
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
    global: &rk_global::GlobalFacts,
    only: &[Addr],
    stage: Stage,
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
        if matches!(stage, Stage::Prepared | Stage::Stack) {
            let stack = stage == Stage::Stack;
            let cg = rk_ir::lower(ir, &rk_x87::Recipes)?;
            let facts = global.call_facts();
            let options = rk_ssa::PrepareOptions {
                call_facts: &facts,
                fault_state: settings.fault_state,
                coalesce_roots: switches.coalesce_roots,
                lazy_stack_writes: settings.lazy_stack_writes,
                assume_stack_pointer_bases: settings.assume_stack_pointer_bases,
                assume_private_stack: settings.assume_private_stack
                    && !settings.exposed_stack_functions.contains(&owner),
                ..Default::default()
            };
            let result = if stack {
                rk_ssa::prepare_with_stack_dump(&cg, &options, out)
            } else {
                rk_ssa::prepare(&cg, &options)
            };
            match result {
                Ok(ssa) if !stack => rk_ssa::dump(&ssa, out)?,
                Ok(_) => {}
                Err(error) => writeln!(out, "== prepared {owner:08x}: {error}")?,
            }
            continue;
        }
        let cg = rk_ir::lower(ir, &rk_ir::NoX87)?;
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
        let rejection =
            line.starts_with("return-rejected ") || line.starts_with("cleanup-rejected ");
        let selected = if rejection {
            line.split_whitespace()
                .nth(1)
                .is_some_and(|owner| owners.contains(owner))
        } else {
            owners.iter().any(|o| line.contains(o.as_str()))
        };
        if owners.is_empty() || selected {
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
            "direct_targets": global.direct_targets,
            "direct_variants": global.direct_variants,
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
        "direct_arguments": emitted.direct_arguments.iter().map(|(addr, arguments)| {
            let mut names: Vec<_> = arguments.registers.iter().map(Field::name).collect();
            if arguments.st0 { names.push("ST0"); }
            (format!("{addr:08x}"), names)
        }).collect::<BTreeMap<_, _>>(),
        "fallbacks": sites_json(emitted.fallback_functions, &emitted.fallbacks),
        "slow_ops": sites_json(emitted.slow_op_functions, &emitted.slow_ops),
        "stack_promotion_failures": emitted
            .promotion_failures
            .iter()
            .map(|(addr, failure)| (format!("{addr:08x}"), failure))
            .collect::<BTreeMap<_, _>>(),
        "return_slot": return_slot_json(&emitted.return_slots),
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
    let mut args = Args::parse();
    if args.stack_return_report.is_some()
        && (args.out.is_some() || args.reloc_only || !args.dump.is_empty())
    {
        bail!("--stack-return-report is an isolated read-only mode");
    }
    if let Some(path) = &args.selection {
        let selection: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
        for row in selection["functions"]
            .as_array()
            .context("selection needs a functions array")?
        {
            let text = row["addr"]
                .as_str()
                .context("selection function needs a hexadecimal addr")?;
            args.only
                .push(parse_addr(text).map_err(anyhow::Error::msg)?);
        }
        if args.only.is_empty() {
            bail!("selection contains no functions");
        }
    }
    let unset = |name| std::env::var_os(name).is_none();
    let switches = Switches {
        direct: unset("RK_NO_DIRECT"),
        descriptors: unset("RK_NO_DESCRIPTORS"),
        coalesce_roots: unset("RK_NO_ROOTS"),
        x87_dynamic: !unset("RK_X87_DYNAMIC"),
    };
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
    let rk_image::LoadedProgram { mut program, pe } = rk_image::load(&settings)?;
    phases.mark("load");
    let discovered = rk_discover::discover(&settings, &program, &pe)?;
    if let Some(path) = &args.discovery_report {
        write_discovery_report(path, &settings, &program, &discovered)?;
    }
    rk_discover::merge(&mut program, &discovered)?;
    phases.mark("discover");
    let whole = args.out.is_some() || args.dump.is_empty();
    let wants_global = whole
        || [Stage::Global, Stage::Prepared, Stage::Stack, Stage::Bodies]
            .into_iter()
            .any(|stage| args.wants(stage));
    let mut analysis = Job {
        program: &program,
        settings: &settings,
        native: &BTreeSet::new(),
    }
    .lift();
    phases.mark("lift");
    let summary = IrSummary::new(&analysis);
    let facts = facts::load(&settings, &program, &analysis)?;
    phases.mark("facts");
    let native = if wants_global {
        settings.native_replaced()?
    } else {
        BTreeSet::new()
    };
    let job = Job {
        program: &program,
        settings: &settings,
        native: &native,
    };
    let global = if wants_global {
        job.global(&analysis, &facts)?
    } else {
        rk_global::GlobalFacts::default()
    };
    phases.mark("global");
    let extra_entries = if wants_global {
        prepare_return_entries(&mut analysis, &global.returns)
    } else {
        BTreeMap::new()
    };
    let only = if whole && args.stack_return_report.is_none() {
        &[][..]
    } else {
        &args.only[..]
    };
    let wants_bodies = whole || args.wants(Stage::Bodies) || args.stack_return_report.is_some();
    let owned = (wants_bodies && args.dump.is_empty()).then(|| std::mem::take(&mut analysis.ir));
    let (bodies, emit_stats, direct_variants) = if wants_bodies {
        let mode = if switches.x87_dynamic {
            X87Mode::Dynamic
        } else {
            args.x87_mode
        };
        let stack_inputs = recomp_translate::offline_stack_inputs(
            &settings,
            switches,
            mode,
            args.stack_return_report.is_some(),
        )?;
        let options = EmitOptions {
            x87_mode: mode,
            switches,
            only,
            reloc: None,
            return_stack_inputs: Some(&stack_inputs),
        };
        if let Some(path) = &args.stack_return_report {
            job.stack_return_report(&analysis, owned, &global, &facts, options, path)?;
            (Vec::new(), EmitStats::default(), BTreeSet::new())
        } else {
            job.emit(&analysis, owned, &global, &facts, options)?
        }
    } else {
        (Vec::new(), EmitStats::default(), BTreeSet::new())
    };
    phases.mark("emit");
    if args.stack_return_report.is_some() {
        return Ok(());
    }
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
        host_methods: facts.host_calls.methods.clone(),
        direct_variants,
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
        for stage in [Stage::Ssa, Stage::Prepared, Stage::Stack] {
            if args.wants(stage) {
                dump_ssa(
                    &program, &analysis, &settings, &global, &args.only, stage, switches, &mut out,
                )?;
            }
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
        global_report.direct_variants = translation.direct_variants.len();
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
