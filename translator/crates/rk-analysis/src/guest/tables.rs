use std::collections::{BTreeMap, BTreeSet};

use rk_core::program::Program;
use rk_core::Addr;
use rk_x86::OperandKind;

use super::summary::{FunctionSummary, ShapeForm};
use super::{ObservedSite, Options, Site};

const MAX_TABLE_SLOTS: usize = 16384;

pub struct Census {
    pub slots: BTreeMap<Addr, Vec<Addr>>,
    pub installs: BTreeMap<Addr, Vec<(Addr, Option<super::Ref>)>>,
    pub unknown_installs: BTreeSet<Addr>,
}

pub struct Bounds {
    pub sites: Vec<Site>,
    pub report: serde_json::Value,
}

pub fn raw_census(program: &Program, constant_stores: bool) -> BTreeMap<Addr, Vec<Addr>> {
    let decoder = rk_core::isa::machine();
    let mut candidates = BTreeSet::new();
    for function in program.functions.values() {
        for span in &function.spans {
            if let Some(lengths) = &span.lengths {
                let mut at = span.start;
                for &length in lengths {
                    let length = length as usize;
                    if let Some(raw) = program.image.bytes(at, length) {
                        if let Some(insn) = decoder.decode(at, raw) {
                            scan_operands(program, &insn, constant_stores, &mut candidates);
                        }
                    }
                    at += length as Addr;
                }
            } else {
                let mut at = span.start;
                while at < span.end() {
                    let available = (span.end() - at).min(u64::from(decoder.max_length())) as usize;
                    let Some(bytes) = program.image.bytes(at, available) else {
                        break;
                    };
                    let mut next = None;
                    for length in 1..=available {
                        if let Some(insn) = decoder.decode(at, &bytes[..length]) {
                            next = Some(insn);
                            break;
                        }
                    }
                    let Some(insn) = next else {
                        at += 1;
                        continue;
                    };
                    scan_operands(program, &insn, constant_stores, &mut candidates);
                    at += insn.length as Addr;
                }
            }
        }
    }
    let mut slots = BTreeMap::new();
    for table in candidates {
        if let Some(words) = table_slots(program, table) {
            slots.insert(table, words);
        }
    }
    slots
}

pub fn census(
    _program: &Program,
    summaries: &BTreeMap<Addr, FunctionSummary>,
    slots: &BTreeMap<Addr, Vec<Addr>>,
) -> Census {
    let slots = slots.clone();
    let mut installs: BTreeMap<Addr, Vec<(Addr, Option<super::Ref>)>> = BTreeMap::new();
    let mut unknown_installs = BTreeSet::new();
    for summary in summaries.values() {
        for store in &summary.constants {
            if slots.contains_key(&store.value) {
                installs
                    .entry(store.value)
                    .or_default()
                    .push((store.site, store.base));
                if store.base.is_none() {
                    unknown_installs.insert(store.value);
                }
            }
        }
    }
    Census {
        slots,
        installs,
        unknown_installs,
    }
}

fn scan_operands(
    program: &Program,
    insn: &rk_core::isa::Decoded,
    constant_stores: bool,
    candidates: &mut BTreeSet<Addr>,
) {
    if constant_stores {
        if insn.mnemonic == "MOV"
            && insn.operands.len() == 2
            && insn.operands[0].size == Some(32)
            && matches!(insn.operands[0].kind, OperandKind::Mem { .. })
        {
            if let OperandKind::Imm(value) = insn.operands[1].kind {
                candidates.insert(value as Addr);
            }
        }
        return;
    }
    for operand in &insn.operands {
        let address = match operand.kind {
            OperandKind::Imm(value) => value as u64 as u32 as Addr,
            OperandKind::Mem {
                base: None,
                index: None,
                disp,
                ..
            } => disp as u64 as u32 as Addr,
            _ => continue,
        };
        if address >= program.image.base && address < program.image.end() {
            candidates.insert(address);
        }
    }
}

fn table_slots(program: &Program, table: Addr) -> Option<Vec<Addr>> {
    if program.image.is_exec(table) {
        return None;
    }
    let mut slots = Vec::new();
    for index in 0..MAX_TABLE_SLOTS {
        let Some(word) = program.image.rd32(table.wrapping_add((index * 4) as Addr)) else {
            break;
        };
        if !program.image.is_exec(word) {
            break;
        }
        slots.push(word);
    }
    (!slots.is_empty()).then_some(slots)
}

pub fn bounds(
    _program: &Program,
    summaries: &BTreeMap<Addr, FunctionSummary>,
    census: &Census,
    observed: &BTreeMap<Addr, ObservedSite>,
    options: Options,
) -> Bounds {
    let mut shape_rejections: BTreeMap<String, usize> = BTreeMap::new();
    let mut blocked_functions: BTreeMap<String, usize> = BTreeMap::new();
    let mut open: BTreeMap<String, usize> = BTreeMap::new();
    let mut open_examples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut invalid_targets = BTreeMap::new();
    let mut cleanup_refinements = BTreeMap::new();
    for summary in summaries.values() {
        for (reason, count) in &summary.shape_rejections {
            *shape_rejections.entry(reason.clone()).or_default() += count;
        }
        if let Some(reason) = summary.blocked() {
            *blocked_functions.entry(reason.into()).or_default() += 1;
        }
    }
    macro_rules! open {
        ($reason:expr, $site:expr) => {{
            let reason: String = $reason.into();
            *open.entry(reason.clone()).or_default() += 1;
            let list = open_examples.entry(reason).or_default();
            if list.len() < 8 {
                list.push(format!("{:08x}", $site));
            }
        }};
    }
    let cleanups: BTreeMap<Addr, Option<i32>> = summaries
        .iter()
        .map(|(&addr, summary)| (addr, summary.cleanup))
        .collect();
    let mut sites = Vec::new();
    for summary in summaries.values() {
        if summary.blocked().is_some() {
            continue;
        }
        for shape in &summary.sites {
            let mut requirements = vec![shape];
            if let Some(earlier) = summary.requirements.get(&shape.site) {
                requirements.extend(
                    summary
                        .sites
                        .iter()
                        .filter(|site| earlier.contains(&site.site)),
                );
            }
            let required = requirements.iter().map(|site| site.slot).collect();
            let Some(mut candidates) = candidate_tables(shape, census, &required) else {
                open!("no installed table covers the called slot", shape.site);
                continue;
            };
            let site_observed = observed.get(&shape.site);
            if site_observed.is_some_and(|row| row.blocked) {
                open!("shim or unsupported observation", shape.site);
                continue;
            }
            let guest_observed: BTreeSet<Addr> = site_observed
                .map(|row| row.targets.clone())
                .unwrap_or_default();
            let mut blocked_requirement = false;
            let initial_candidates = candidates.len();
            for requirement in &requirements {
                let Some(row) = observed.get(&requirement.site) else {
                    continue;
                };
                if row.blocked {
                    blocked_requirement = true;
                    break;
                }
                if options.assume_compatible_cleanup
                    && !row.targets.is_empty()
                    && matches!(requirement.form, ShapeForm::Vptr { .. })
                {
                    narrow_cleanup(
                        requirement,
                        &mut candidates,
                        census,
                        &cleanups,
                        &row.targets,
                    );
                }
            }
            if blocked_requirement {
                open!("shim or unsupported receiver requirement", shape.site);
                continue;
            }
            if candidates.len() < initial_candidates {
                cleanup_refinements.insert(
                    format!("{:08x}", shape.site),
                    serde_json::json!({
                        "tables_before": initial_candidates,
                        "tables_after": candidates.len(),
                    }),
                );
            }
            let mut targets = BTreeSet::new();
            let mut invalid = None;
            if let ShapeForm::Target { target } = shape.form {
                if summaries.get(&target).is_some_and(|summary| summary.lifted) {
                    targets.insert(target);
                } else {
                    invalid = Some("constant target is not a lifted function");
                }
            }
            for table in &candidates {
                let Some(words) = census.slots.get(table) else {
                    invalid = Some("candidate table has no byte-checked extent");
                    break;
                };
                let index = (shape.slot / 4) as usize;
                let Some(&target) = words.get(index) else {
                    invalid = Some("candidate table does not cover the called slot");
                    break;
                };
                if !summaries.get(&target).is_some_and(|summary| summary.lifted) {
                    invalid_targets.insert(
                        format!("{table:08x}+{:x}", shape.slot),
                        serde_json::json!({
                            "target": format!("{target:08x}"),
                            "reason": summaries.get(&target).and_then(FunctionSummary::blocked),
                        }),
                    );
                    invalid = Some("candidate table target is not a lifted function");
                    break;
                }
                targets.insert(target);
            }
            if let Some(reason) = invalid {
                open!(reason, shape.site);
                continue;
            }
            if targets.is_empty() {
                open!("no candidate target at the called slot", shape.site);
                continue;
            }
            if !guest_observed.is_subset(&targets) {
                open!(
                    "structural bound excludes observed guest targets",
                    shape.site
                );
                continue;
            }
            sites.push(Site {
                site: shape.site,
                kind: shape.kind.name(),
                targets: targets.into_iter().collect(),
            });
        }
    }
    sites.sort_by_key(|site| (site.site, site.kind));
    sites.dedup_by_key(|site| (site.site, site.kind));
    let mut sizes: BTreeMap<usize, usize> = BTreeMap::new();
    for site in &sites {
        *sizes.entry(site.targets.len()).or_default() += 1;
    }
    let report = serde_json::json!({
        "functions": summaries.len(),
        "summarized_functions": summaries.values().filter(|summary| summary.blocked().is_none()).count(),
        "blocked_functions": blocked_functions,
        "shape_rejections": shape_rejections,
        "installed_tables": census.slots.len(),
        "install_sites": census.installs.values().map(Vec::len).sum::<usize>(),
        "unknown_install_tables": census.unknown_installs.len(),
        "indirect_sites_seen": summaries.values().map(|summary| summary.sites.len() + summary.shape_rejections.values().sum::<usize>()).sum::<usize>(),
        "shared_receiver_sites": summaries.values().flat_map(|summary| summary.requirements.values()).filter(|sites| !sites.is_empty()).count(),
        "sites_emitted": sites.len(),
        "target_entries": sites.iter().map(|site| site.targets.len()).sum::<usize>(),
        "target_set_sizes": sizes,
        "open": open,
        "open_examples": open_examples,
        "invalid_targets": invalid_targets,
        "cleanup_refinements": cleanup_refinements,
    });
    Bounds { sites, report }
}

fn candidate_tables(
    shape: &super::Shape,
    census: &Census,
    required: &BTreeSet<i32>,
) -> Option<BTreeSet<Addr>> {
    match shape.form {
        ShapeForm::Target { .. } => Some(BTreeSet::new()),
        ShapeForm::Table { table } => {
            let words = census.slots.get(&table)?;
            let index = (shape.slot / 4) as usize;
            words.get(index)?;
            Some(BTreeSet::from([table]))
        }
        ShapeForm::Vptr { .. } => {
            let index = (shape.slot / 4) as usize;
            let candidates: BTreeSet<Addr> = census
                .slots
                .iter()
                .filter(|(_, words)| {
                    words.get(index).is_some()
                        && required.iter().all(|slot| {
                            *slot >= 0 && slot % 4 == 0 && words.get((*slot / 4) as usize).is_some()
                        })
                })
                .map(|(&table, _)| table)
                .collect();
            (!candidates.is_empty()).then_some(candidates)
        }
    }
}

fn narrow_cleanup(
    shape: &super::Shape,
    candidates: &mut BTreeSet<Addr>,
    census: &Census,
    cleanups: &BTreeMap<Addr, Option<i32>>,
    observed: &BTreeSet<Addr>,
) {
    let index = (shape.slot / 4) as usize;
    let anchors: Vec<Vec<Vec<Option<i32>>>> = observed
        .iter()
        .map(|target| {
            census
                .slots
                .iter()
                .filter(|(_, words)| words.get(index) == Some(target))
                .map(|(&table, words)| profile(table, words, index, cleanups))
                .collect()
        })
        .collect();
    candidates.retain(|table| {
        let Some(words) = census.slots.get(table) else {
            return false;
        };
        let own = profile(*table, words, index, cleanups);
        anchors
            .iter()
            .all(|group| group.is_empty() || group.iter().any(|anchor| compatible(&own, anchor)))
    });
}

fn profile(
    _table: Addr,
    words: &[Addr],
    index: usize,
    cleanups: &BTreeMap<Addr, Option<i32>>,
) -> Vec<Option<i32>> {
    (0..=index)
        .map(|slot| cleanups.get(&words[slot]).copied().flatten())
        .collect()
}

fn compatible(left: &[Option<i32>], right: &[Option<i32>]) -> bool {
    left.iter()
        .zip(right)
        .all(|(a, b)| a.is_none() || b.is_none() || a == b)
}
