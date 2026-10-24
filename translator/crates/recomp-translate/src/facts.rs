use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rk_core::facts::{self, AnalysisFacts, SiteFacts, TransferKind};
use rk_core::ir::{OpCode, Space};
use rk_core::program::Program;
use rk_core::settings::{CallEffect, CallEffects, Settings};
use rk_core::{Addr, Diagnostic, Error, Reason, Result};
use rk_frontend::Analysis;

use crate::parse_addr8;

const INDIRECT_FORMAT: &str = "recomp-indirect-targets-v1";
const INDIRECT_HEADER: &str = "site\tkind\ttargets";

#[derive(Default)]
pub struct Facts {
    pub call_targets: BTreeMap<Addr, Vec<Addr>>,
    pub jump_targets: BTreeMap<Addr, Vec<Addr>>,
    pub host_calls: BTreeSet<Addr>,
    pub site_effects: BTreeMap<Addr, CallEffect>,
    pub imports: BTreeMap<String, CallEffect>,
    pub runtime_call_facts: BTreeMap<Addr, Vec<Addr>>,
    pub cw_effects: BTreeMap<Addr, (u16, u16, u16)>,
    pub summary: facts::Summary,
}

pub fn load(settings: &Settings, program: &Program, analysis: &Analysis) -> Result<Facts> {
    let mut facts = match (&settings.analysis_facts, &settings.indirect_targets) {
        (Some(path), None) => read_analysis_facts(path, settings, program, analysis)?,
        (None, Some(path)) => read_legacy_targets(path, settings, program, analysis)?,
        (None, None) => Facts::default(),
        (Some(_), Some(_)) => unreachable!("settings rejects mixed fact sources"),
    };
    if settings.analysis_facts.is_none() {
        facts.site_effects = legacy_effects(settings, program, analysis)?;
    }
    resolve_import_effects(&mut facts, program, analysis, &settings.call_effects)?;
    facts.summary.import_effects = facts.imports.len();
    facts.runtime_call_facts = runtime_call_facts(program, analysis, &facts.call_targets)?;
    facts.cw_effects = cw_effects(program, analysis, &facts.site_effects)?;
    facts.summary.site_effects = facts.site_effects.len();
    Ok(facts)
}

fn read_analysis_facts(
    path: &Path,
    settings: &Settings,
    program: &Program,
    analysis: &Analysis,
) -> Result<Facts> {
    let loaded = AnalysisFacts::load(path, &settings.sha256)?;
    let mut call_targets = BTreeMap::new();
    let mut jump_targets = BTreeMap::new();
    let mut host_calls = BTreeSet::new();
    let mut site_effects = BTreeMap::new();
    for (&site, row) in &loaded.sites {
        validate_site(program, analysis, path, site, row)?;
        if let Some(targets) = &row.targets {
            match row.kind {
                TransferKind::Call => {
                    call_targets.insert(site, targets.clone());
                }
                TransferKind::Jump => {
                    jump_targets.insert(site, targets.clone());
                }
            }
        }
        if let Some(effect) = &row.effect {
            if row.kind != TransferKind::Call {
                return Err(Error::input(format!(
                    "{}: jump site {site:08x} cannot carry a call effect",
                    path.display()
                )));
            }
            site_effects.insert(site, effect.clone());
        }
        if row.method.is_some() {
            host_calls.insert(site);
        }
    }
    let imports: BTreeMap<String, CallEffect> = loaded
        .imports
        .iter()
        .map(|(name, row)| (name.clone(), row.effect.clone()))
        .collect();
    Ok(Facts {
        call_targets,
        jump_targets,
        host_calls,
        site_effects,
        imports,
        runtime_call_facts: BTreeMap::new(),
        cw_effects: BTreeMap::new(),
        summary: loaded.summary,
    })
}

fn validate_site(
    program: &Program,
    analysis: &Analysis,
    path: &Path,
    site: Addr,
    row: &SiteFacts,
) -> Result<()> {
    let (insn, _, indirect) = instruction_at(program, analysis, site)?;
    let call = insn
        .ops
        .iter()
        .any(|op| matches!(op.opcode, OpCode::Call | OpCode::CallInd));
    let jump = insn.mnemonic.eq_ignore_ascii_case("JMP")
        && insn.ops.iter().any(|op| op.opcode == OpCode::BranchInd);
    match row.kind {
        TransferKind::Call => {
            if !call {
                return Err(Error::input(format!(
                    "{}: site {site:08x} is not a call",
                    path.display()
                )));
            }
            if row.targets.is_some() && !indirect {
                return Err(Error::input(format!(
                    "{}: site {site:08x} has a target bound but is not an indirect call",
                    path.display()
                )));
            }
        }
        TransferKind::Jump => {
            if !jump {
                return Err(Error::input(format!(
                    "{}: site {site:08x} is not an indirect jump",
                    path.display()
                )));
            }
        }
    }
    if let Some(targets) = &row.targets {
        for &target in targets {
            if !program.functions.contains_key(&target) {
                return Err(Error::input(format!(
                    "{}: site {site:08x} target {target:08x} is not a function entry",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn read_legacy_targets(
    path: &Path,
    settings: &Settings,
    program: &Program,
    analysis: &Analysis,
) -> Result<Facts> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    let mut fields = first.split('\t');
    if fields.next() != Some(INDIRECT_FORMAT)
        || fields.next() != Some(settings.sha256.to_lowercase().as_str())
    {
        return Err(Error::input(format!(
            "{}: first line must be '{INDIRECT_FORMAT}' and the executable SHA-256",
            path.display()
        )));
    }
    if lines.next() != Some(INDIRECT_HEADER) {
        return Err(Error::input(format!(
            "{}: second line must be '{INDIRECT_HEADER}'",
            path.display()
        )));
    }
    let mut seen = BTreeSet::new();
    let mut call_targets = BTreeMap::new();
    let mut jump_targets = BTreeMap::new();
    for (number, line) in lines.enumerate() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 3 {
            return Err(Error::input(format!(
                "{}: line {} must have three tab-separated fields",
                path.display(),
                number + 3
            )));
        }
        let site = parse_addr8(fields[0]).ok_or_else(|| {
            Error::input(format!(
                "{}: invalid site address {:?}",
                path.display(),
                fields[0]
            ))
        })?;
        let kind = fields[1];
        if kind != "call" && kind != "jump" {
            return Err(Error::input(format!(
                "{}: site {site:08x} has unknown kind {kind:?}",
                path.display()
            )));
        }
        if !seen.insert(site) {
            return Err(Error::input(format!(
                "{}: site {site:08x} repeats",
                path.display()
            )));
        }
        let mut targets = BTreeSet::new();
        for field in fields[2].split(',') {
            let target = parse_addr8(field).ok_or_else(|| {
                Error::input(format!(
                    "{}: site {site:08x} has invalid target {field:?}",
                    path.display()
                ))
            })?;
            if !program.functions.contains_key(&target) {
                return Err(Error::input(format!(
                    "{}: site {site:08x} target {target:08x} is not a function entry",
                    path.display()
                )));
            }
            targets.insert(target);
        }
        let (_, _, indirect) = instruction_at(program, analysis, site)?;
        if kind == "jump" {
            let (insn, _, _) = instruction_at(program, analysis, site)?;
            if !(insn.mnemonic.eq_ignore_ascii_case("JMP")
                && insn.ops.iter().any(|op| op.opcode == OpCode::BranchInd))
            {
                return Err(Error::input(format!(
                    "{}: site {site:08x} is not an indirect jump",
                    path.display()
                )));
            }
            jump_targets.insert(site, targets.into_iter().collect());
            continue;
        }
        if !indirect {
            return Err(Error::input(format!(
                "{}: site {site:08x} is not an indirect call",
                path.display()
            )));
        }
        call_targets.insert(site, targets.into_iter().collect());
    }
    let mut summary = facts::Summary::default();
    summary.call_bounds = call_targets.len();
    summary.jump_bounds = jump_targets.len();
    summary.target_sets = call_targets
        .values()
        .chain(jump_targets.values())
        .collect::<BTreeSet<_>>()
        .len();
    summary.target_entries = call_targets
        .values()
        .chain(jump_targets.values())
        .map(Vec::len)
        .sum();
    Ok(Facts {
        call_targets,
        jump_targets,
        host_calls: BTreeSet::new(),
        site_effects: BTreeMap::new(),
        imports: BTreeMap::new(),
        runtime_call_facts: BTreeMap::new(),
        cw_effects: BTreeMap::new(),
        summary,
    })
}

fn legacy_effects(
    settings: &Settings,
    program: &Program,
    analysis: &Analysis,
) -> Result<BTreeMap<Addr, CallEffect>> {
    let mut out = BTreeMap::new();
    for (&site, effect) in &settings.call_effects.sites {
        validate_call_effect(program, analysis, site)?;
        out.insert(site, effect.clone());
    }
    Ok(out)
}

fn validate_call_effect(program: &Program, analysis: &Analysis, site: Addr) -> Result<()> {
    let (insn, _, _) = instruction_at(program, analysis, site)?;
    if !insn
        .ops
        .iter()
        .any(|op| matches!(op.opcode, OpCode::Call | OpCode::CallInd))
    {
        return Err(Diagnostic::error(
            site,
            Reason::Unsupported {
                detail: "call effect site is not a call".to_string(),
            },
        )
        .into());
    }
    Ok(())
}

fn resolve_import_effects(
    facts: &mut Facts,
    program: &Program,
    analysis: &Analysis,
    configured: &CallEffects,
) -> Result<()> {
    for (name, effect) in &configured.imports {
        let name = facts::import_name(name)
            .ok_or_else(|| Error::input(format!("import {name:?} must be dll!name")))?;
        facts.imports.insert(name, effect.clone());
    }
    for fir in analysis
        .ir
        .values()
        .filter_map(|result| result.as_ref().ok())
    {
        for insn in &fir.insns {
            let Some(name) = called_import(program, insn) else {
                continue;
            };
            let Some(effect) = facts.imports.get(&name) else {
                continue;
            };
            let merged = facts
                .site_effects
                .get(&insn.addr)
                .map_or_else(CallEffect::default, Clone::clone);
            facts
                .site_effects
                .insert(insn.addr, merge_effect(merged, effect.clone()));
        }
    }
    Ok(())
}

fn merge_effect(mut site: CallEffect, import: CallEffect) -> CallEffect {
    if site.regs.is_none() {
        site.regs = import.regs;
    }
    if site.x87.is_none() {
        site.x87 = import.x87;
    }
    site
}

fn called_import(program: &Program, insn: &rk_core::ir::Insn) -> Option<String> {
    let indirect = insn.ops.iter().any(|op| op.opcode == OpCode::CallInd);
    if indirect {
        return absolute_iat_call(program, insn);
    }
    let direct = insn.ops.iter().find(|op| op.opcode == OpCode::Call)?;
    let target = direct.inputs.first()?;
    let value = match target.space {
        Space::Const | Space::Ram => target.offset as Addr,
        _ => return None,
    };
    import_thunk(program, value)
}

fn absolute_iat_call(program: &Program, insn: &rk_core::ir::Insn) -> Option<String> {
    let raw = program.image.bytes(insn.addr, insn.length as usize)?;
    let mut at = 0;
    while at < raw.len() && matches!(raw[at], 0x26 | 0x2e | 0x36 | 0x3e) {
        at += 1;
    }
    if at + 6 != raw.len() || raw[at] != 0xff || raw[at + 1] != 0x15 {
        return None;
    }
    let slot = u32::from_le_bytes([raw[at + 2], raw[at + 3], raw[at + 4], raw[at + 5]]);
    program.image.imports.get(&slot).map(|import| import.key())
}

fn import_thunk(program: &Program, value: Addr) -> Option<String> {
    if !program.image.is_exec(value) {
        return None;
    }
    let raw = program.image.bytes(value, 6)?;
    if raw[..2] != [0xff, 0x25] {
        return None;
    }
    let slot = u32::from_le_bytes([raw[2], raw[3], raw[4], raw[5]]);
    program.image.imports.get(&slot).map(|import| import.key())
}

fn runtime_call_facts(
    program: &Program,
    analysis: &Analysis,
    targets: &BTreeMap<Addr, Vec<Addr>>,
) -> Result<BTreeMap<Addr, Vec<Addr>>> {
    let mut out = BTreeMap::new();
    for (&site, found) in targets {
        let (end, indirect) = call_end(program, analysis, site)?;
        if !indirect {
            return Err(Error::input(format!(
                "site {site:08x} is not an indirect call"
            )));
        }
        out.insert(end, found.clone());
    }
    Ok(out)
}

fn cw_effects(
    program: &Program,
    analysis: &Analysis,
    effects: &BTreeMap<Addr, CallEffect>,
) -> Result<BTreeMap<Addr, (u16, u16, u16)>> {
    let mut out = BTreeMap::new();
    for (&site, effect) in effects {
        if let Some(x87) = effect.x87 {
            let (end, _) = call_end(program, analysis, site)?;
            out.insert(end, x87.control_word());
        }
    }
    Ok(out)
}

fn instruction_at<'a>(
    program: &Program,
    analysis: &'a Analysis,
    site: Addr,
) -> Result<(&'a rk_core::ir::Insn, u32, bool)> {
    let owner = program.entry_owner(site).ok_or_else(|| {
        Diagnostic::error(
            site,
            Reason::Unsupported {
                detail: "site lies outside every mapped function".to_string(),
            },
        )
    })?;
    let ir = match analysis.ir.get(&owner) {
        Some(Ok(ir)) => ir,
        Some(Err(error)) => return Err(Error::input(format!("site {site:08x}: {error}"))),
        None => {
            return Err(Diagnostic::error(
                site,
                Reason::Unsupported {
                    detail: "site has no function IR".to_string(),
                },
            )
            .into())
        }
    };
    let index = ir.index_of(site).ok_or_else(|| {
        Diagnostic::error(
            site,
            Reason::Unsupported {
                detail: "site is not an instruction boundary".to_string(),
            },
        )
    })?;
    let insn = &ir.insns[index as usize];
    let indirect = insn.ops.iter().any(|op| op.opcode == OpCode::CallInd);
    Ok((insn, index, indirect))
}

fn call_end(program: &Program, analysis: &Analysis, site: Addr) -> Result<(Addr, bool)> {
    let (insn, _, indirect) = instruction_at(program, analysis, site)?;
    let call = insn
        .ops
        .iter()
        .any(|op| matches!(op.opcode, OpCode::Call | OpCode::CallInd));
    if !call {
        return Err(Diagnostic::error(
            site,
            Reason::Unsupported {
                detail: "instruction is not a call".to_string(),
            },
        )
        .into());
    }
    Ok((insn.end(), indirect))
}
