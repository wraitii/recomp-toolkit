use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use rk_core::ir::OpCode;
use rk_core::program::{Function, Program};
use rk_core::settings::{Curated, Settings};
use rk_core::Addr;
use rk_emit::RelocEmit;

use crate::facts::Facts;
use crate::{build_bodies, Switches, X87Mode};

#[derive(Default)]
struct Refusal {
    reason: String,
    detail: String,
}

fn imported_targets(template: &rk_image::reloc::Template) -> Result<BTreeSet<Addr>> {
    template
        .imports
        .iter()
        .map(|import| {
            Addr::from_str_radix(import.target.trim_start_matches("0x"), 16)
                .map_err(|_| anyhow::anyhow!("{}: invalid import target", template.id))
        })
        .collect()
}

fn refuse(
    program: &Program,
    analysis: &rk_frontend::Analysis,
    template: &rk_image::reloc::Template,
    target_imports: &BTreeSet<Addr>,
) -> Option<Refusal> {
    let base = program.image.base;
    let end = program.image.end();
    let masked: BTreeSet<u32> = template.masked.iter().copied().collect();
    let import_slots: BTreeSet<u32> = template.imports.iter().map(|import| import.slot).collect();
    for (&addr, result) in &analysis.ir {
        let Ok(ir) = result else {
            return Some(Refusal {
                reason: "lift-failed".into(),
                detail: format!("{addr:08x}"),
            });
        };
        for insn in &ir.insns {
            if insn.addr < base || insn.addr + insn.length as Addr > end {
                return Some(Refusal {
                    reason: "instruction-outside-body".into(),
                    detail: format!("{:08x}", insn.addr),
                });
            }
            let start = insn.addr - base;
            let imported_call = insn.mnemonic.eq_ignore_ascii_case("CALL")
                && import_slots.contains(&(start + insn.length as u32 - 4));
            for offset in start..start + insn.length as u32 {
                if masked.contains(&offset) {
                    let import_field = imported_call
                        && offset >= start + insn.length as u32 - 4
                        && offset < start + insn.length as u32;
                    if !import_field {
                        return Some(Refusal {
                            reason: "masked-slot-overlap".into(),
                            detail: format!("{:08x}+{offset:x}", insn.addr),
                        });
                    }
                }
            }
            for op in &insn.ops {
                if matches!(op.opcode, OpCode::BranchInd) {
                    return Some(Refusal {
                        reason: "indirect-jump".into(),
                        detail: format!("{:08x}", insn.addr),
                    });
                }
                if matches!(op.opcode, OpCode::CallInd) {
                    return Some(Refusal {
                        reason: "indirect-call".into(),
                        detail: format!("{:08x}", insn.addr),
                    });
                }
                if matches!(op.opcode, OpCode::Call | OpCode::Branch) {
                    let Some(target) = op
                        .inputs
                        .first()
                        .filter(|input| {
                            matches!(
                                input.space,
                                rk_core::ir::Space::Ram | rk_core::ir::Space::Const
                            )
                        })
                        .map(|input| input.offset as Addr)
                    else {
                        continue;
                    };
                    if !(base..end).contains(&target) && !target_imports.contains(&target) {
                        return Some(Refusal {
                            reason: "transfer-outside-body".into(),
                            detail: format!("{:08x} -> {target:08x}", insn.addr),
                        });
                    }
                }
            }
        }
    }
    None
}

fn merge_trusted(program: &mut Program, discovered: &rk_discover::Discovered) -> Result<()> {
    for (&addr, spans) in &discovered.functions {
        program.functions.insert(
            addr,
            Function {
                addr,
                spans: spans.clone(),
                noreturn: false,
                provisional: false,
            },
        );
    }
    program.spans = program
        .functions
        .values()
        .flat_map(|function| {
            function
                .spans
                .iter()
                .map(move |span| (span.start, span.end(), function.addr))
        })
        .collect();
    program.spans.sort_unstable();
    Ok(rk_image::index_entries(program)?)
}

fn reloc_settings(settings: &Settings) -> Settings {
    let mut reloc = settings.clone();
    reloc.analysis_facts = None;
    reloc.indirect_targets = None;
    reloc.discovery_evidence = None;
    reloc.native_header = None;
    reloc.globals.clear();
    reloc.curated = Curated::default();
    reloc.alternate_entries.clear();
    reloc.configured_entries.clear();
    reloc.checked_returns.clear();
    reloc.state_contract_callers.clear();
    reloc.private_input_registers.clear();
    reloc.call_effects.sites.clear();
    reloc.private_input_arguments = false;
    reloc.private_leaf_contracts = false;
    reloc.call_contracts = false;
    reloc
}

pub fn translate(
    settings: &Settings,
    manifest_path: &Path,
    out: &Path,
    report_path: &Path,
    switches: Switches,
) -> Result<()> {
    let manifest = rk_image::reloc::load(manifest_path)?;
    let reloc_settings = reloc_settings(settings);
    let mut published = Vec::new();
    let mut rows = Vec::new();
    let mut refusals: BTreeMap<String, (usize, u64, Vec<String>)> = BTreeMap::new();
    let mut published_bytes = 0u64;
    let mut refused_bytes = 0u64;

    for (index, template) in manifest.templates.iter().enumerate() {
        let mut program =
            rk_image::reloc::program(template, index).with_context(|| template.id.clone())?;
        let code_len = template.code.len() / 2;
        let targets = imported_targets(template)?;
        let entries: Vec<Addr> = template
            .entries
            .iter()
            .map(|&offset| program.image.base + offset)
            .collect();
        let discovered = rk_discover::discover_from_entries(&program, &entries)?;
        merge_trusted(&mut program, &discovered)?;
        let analysis = rk_frontend::analyze(&program);
        let facts = Facts::default();
        let global = rk_global::GlobalFacts::analyze(&rk_global::GlobalInput {
            settings: &reloc_settings,
            program: &program,
            analysis: &analysis,
            indirect: &facts.call_targets,
            indirect_jumps: &facts.jump_targets,
            site_effects: &facts.site_effects,
            fact_summary: &facts.summary,
        })?;
        let mut reloc_switches = switches;
        reloc_switches.private = false;
        let built = build_bodies(
            &program,
            &analysis,
            None,
            &global,
            &facts.call_targets,
            &facts.host_calls,
            &reloc_settings,
            X87Mode::Auto,
            reloc_switches,
            &[],
            Some(RelocEmit {
                image_base: program.image.base,
                local_size: program.image.size,
            }),
        )?;
        let (bodies, stats, _, _, _) = built;

        let mut refusal = refuse(&program, &analysis, template, &targets);
        if refusal.is_none() && stats.trapping != 0 {
            let reason = stats
                .rejections
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| "trap".into());
            refusal = Some(Refusal {
                reason,
                detail: format!("{} trapped bodies", stats.trapping),
            });
        }
        if refusal.is_none() && stats.fallback_functions != 0 {
            refusal = Some(Refusal {
                reason: "fallback".into(),
                detail: stats
                    .fallbacks
                    .keys()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "emission fallback".into()),
            });
        }
        if refusal.is_none()
            && bodies
                .iter()
                .any(|body| body.text.contains("recomp_unmodelled"))
        {
            refusal = Some(Refusal {
                reason: "trap".into(),
                detail: "emitted unmodelled trap".into(),
            });
        }

        match refusal {
            Some(refusal) => {
                refused_bytes += code_len as u64;
                let sources: Vec<String> = template
                    .sources
                    .iter()
                    .map(|source| format!("{}:{:#x}", source.path, source.body_offset))
                    .collect();
                let entry = refusals
                    .entry(refusal.reason.clone())
                    .or_insert_with(|| (0, 0, Vec::new()));
                entry.0 += 1;
                entry.1 += code_len as u64;
                entry.2.extend(sources.iter().cloned());
                rows.push(serde_json::json!({
                    "id": template.id,
                    "bytes": code_len,
                    "sources": sources,
                    "status": "refused",
                    "reason": refusal.reason,
                    "detail": refusal.detail,
                }));
            }
            None => {
                published_bytes += code_len as u64;
                published.push(rk_output::reloc_output::Template {
                    id: template.id.clone(),
                    base: program.image.base,
                    code: rk_image::reloc::code_bytes(template)?,
                    masked: template.masked.clone(),
                    imports: template
                        .imports
                        .iter()
                        .map(|import| {
                            Ok((
                                import.slot,
                                Addr::from_str_radix(import.target.trim_start_matches("0x"), 16)?,
                            ))
                        })
                        .collect::<std::result::Result<_, std::num::ParseIntError>>()?,
                    entries: template.entries.clone(),
                    bodies,
                });
                rows.push(serde_json::json!({
                    "id": template.id,
                    "bytes": code_len,
                    "sources": template.sources.iter().map(|source| {
                        format!("{}:{:#x}", source.path, source.body_offset)
                    }).collect::<Vec<_>>(),
                    "status": "published",
                }));
            }
        }
    }

    let translation = rk_output::reloc_output::Translation {
        templates: published,
        preludes: rk_emit::preludes(),
    };
    rk_output::reloc_output::write(out, &translation)?;

    let refusal_rows: BTreeMap<String, serde_json::Value> = refusals
        .into_iter()
        .map(|(reason, (templates, bytes, sources))| {
            (
                reason,
                serde_json::json!({
                    "templates": templates,
                    "bytes": bytes,
                    "sources": sources,
                }),
            )
        })
        .collect();
    let report = serde_json::json!({
        "format": "recomp-reloc-report-v1",
        "manifest": manifest_path.to_string_lossy(),
        "templates": manifest.templates.len(),
        "published": translation.templates.len(),
        "refused": manifest.templates.len() - translation.templates.len(),
        "published_bytes": published_bytes,
        "refused_bytes": refused_bytes,
        "refusals": refusal_rows,
        "rows": rows,
    });
    std::fs::write(report_path, serde_json::to_string_pretty(&report)? + "\n")
        .with_context(|| report_path.display().to_string())?;
    Ok(())
}
