//! Offline producer of `recomp-analysis-facts-v2`, run by `tools/build.py
//! --produce-analysis-facts`. It reruns discovery, lifting and contract analysis (`guest`) and
//! refines indirect call and jump bounds to a fixed point from tables (`guest::tables`), SSA site
//! shapes (`guest::summary`) and stack cleanup proofs (`guest::stack`). It then joins them with the
//! `[analysis]` method and effect catalogs and runtime observation TSVs. The facts record their
//! input digests and assumptions and are validated before being written; the translator only
//! consumes them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::Parser;
use rk_core::contract::{Contract, Fields};
use rk_core::facts::{AnalysisFacts, FORMAT_V2};
use rk_core::settings::{CallEffect, RegsEffect, Settings, X87Effect, X87Mask};
use rk_core::{Addr, Error, Hex, Result};
use rk_decode::{DecodedInsn, Decoder, OperandKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod guest;

#[derive(Parser)]
#[command(about = "Produce validated analysis facts from code, observations and method metadata")]
pub struct Args {
    #[arg(long, help = "resolved translation input written by tools/build.py")]
    pub input: PathBuf,
    #[arg(
        long,
        help = "explicit path for fresh facts; existing content is replaced"
    )]
    pub out: PathBuf,
    #[arg(long = "observation")]
    pub observations: Vec<PathBuf>,
    #[arg(long)]
    pub report: Option<PathBuf>,
    #[arg(long, help = "old facts used only for a separate comparison report")]
    pub compare: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AnalysisConfig {
    #[serde(default)]
    method_catalog: String,
    #[serde(default)]
    effect_catalog: String,
    #[serde(default)]
    observations: Vec<String>,
    #[serde(default)]
    families: Vec<String>,
    #[serde(default)]
    overrides: Vec<AnalysisOverride>,
    #[serde(default)]
    assert_effect_coverage: bool,
    #[serde(default)]
    guest_bounds: bool,
    #[serde(default)]
    assume_constant_vptr_stores: bool,
    #[serde(default)]
    assume_compatible_cleanup: bool,
    #[serde(default)]
    assume_balanced_returns: bool,
    #[serde(default)]
    reuse_discovery_seeds: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AnalysisOverride {
    #[serde(default)]
    effect: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    interface: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    site: Option<String>,
    #[serde(default)]
    evidence: Vec<String>,
}

#[derive(Clone, Debug)]
struct PolicyOverride {
    effect: Option<EffectDef>,
    enabled: Option<bool>,
    family: Option<String>,
    interface: Option<String>,
    method: Option<String>,
    site: Option<Addr>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MethodCatalog {
    format: String,
    reference_sha256: String,
    methods: Vec<MethodMeta>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MethodMeta {
    dll: String,
    interface: String,
    method: String,
    slot: u32,
    argc: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectCatalog {
    format: String,
    effects: BTreeMap<String, CatalogEffect>,
    #[serde(default)]
    families: Vec<CatalogFamily>,
    #[serde(default)]
    methods: Vec<CatalogMethod>,
    #[serde(default)]
    sites: Vec<CatalogSite>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogEffect {
    #[serde(default)]
    regs: Option<RegsEffect>,
    #[serde(default)]
    x87: Option<X87Effect>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogFamily {
    pattern: String,
    effect: String,
    evidence: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogMethod {
    identity: String,
    effect: String,
    evidence: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogSite {
    site: String,
    effect: String,
    evidence: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct Observation {
    identities: BTreeSet<String>,
    targets: BTreeSet<Addr>,
    kinds: BTreeSet<String>,
    unclassified_shim: bool,
    sources: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct Selected {
    site: Addr,
    identity: String,
    meta: MethodMeta,
    effect: CallEffect,
    method_evidence: Vec<String>,
    site_evidence: Vec<String>,
}

#[derive(Clone, Debug)]
struct EffectDef {
    effect: CallEffect,
    evidence: Vec<String>,
}

pub fn run(args: Args) -> Result<()> {
    let input_bytes = std::fs::read(&args.input).map_err(|error| Error::io(&args.input, error))?;
    let input_value: serde_json::Value = serde_json::from_slice(&input_bytes)
        .map_err(|error| Error::input(format!("{}: {error}", args.input.display())))?;
    let analysis: AnalysisConfig = match input_value.get("analysis") {
        Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
            Error::input(format!("{}: analysis: {error}", args.input.display()))
        })?,
        None => AnalysisConfig::default(),
    };
    if analysis.method_catalog.is_empty() != analysis.effect_catalog.is_empty() {
        return Err(Error::input(
            "[analysis] needs method_catalog and effect_catalog together",
        ));
    }
    for family in &analysis.families {
        if family.is_empty() || !family.contains('!') {
            return Err(Error::input(format!(
                "invalid observation family {family:?}"
            )));
        }
    }
    let settings = Settings::load(&args.input, None)?;
    let program = rk_image::load(&settings)?;
    let decoder = Decoder::new()?;
    let com = !analysis.method_catalog.is_empty();
    let (method_bytes, effect_bytes, methods, effects, policy) = if com {
        let method_path = settings.game_dir.join(&analysis.method_catalog);
        let effect_path = settings.game_dir.join(&analysis.effect_catalog);
        let method_bytes =
            std::fs::read(&method_path).map_err(|error| Error::io(&method_path, error))?;
        let method_catalog: MethodCatalog = serde_json::from_slice(&method_bytes)
            .map_err(|error| Error::input(format!("{}: {error}", method_path.display())))?;
        if method_catalog.format != "recomp-com-methods-v1" {
            return Err(Error::input(format!(
                "{}: expected recomp-com-methods-v1",
                method_path.display()
            )));
        }
        if method_catalog.reference_sha256.len() != 64
            || !method_catalog
                .reference_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        {
            return Err(Error::input("method catalog has invalid reference_sha256"));
        }
        let methods = method_index(method_catalog)?;
        let effect_bytes =
            std::fs::read(&effect_path).map_err(|error| Error::io(&effect_path, error))?;
        let effect_catalog: EffectCatalog = serde_json::from_slice(&effect_bytes)
            .map_err(|error| Error::input(format!("{}: {error}", effect_path.display())))?;
        if effect_catalog.format != "recomp-method-effects-v1" {
            return Err(Error::input(format!(
                "{}: expected recomp-method-effects-v1",
                effect_path.display()
            )));
        }
        let effects = effect_index(effect_catalog)?;
        let policy = policy_index(&effects, analysis.overrides.clone())?;
        (method_bytes, effect_bytes, methods, effects, policy)
    } else {
        (
            Vec::new(),
            Vec::new(),
            BTreeMap::new(),
            EffectIndex::default(),
            Vec::new(),
        )
    };
    let mut observations = BTreeMap::new();
    let mut observation_inputs = BTreeMap::new();
    let observation_paths: Vec<PathBuf> = if args.observations.is_empty() {
        analysis
            .observations
            .iter()
            .map(|path| settings.game_dir.join(path))
            .collect()
    } else {
        args.observations.clone()
    };
    for path in &observation_paths {
        let bytes = std::fs::read(path).map_err(|error| Error::io(path, error))?;
        let hash = rk_image::sha256_hex(&bytes);
        if observation_inputs.insert(hash.clone(), hash).is_none() {
            parse_observations(path, &bytes, &mut observations)?;
        }
    }
    let guest_preferences: BTreeSet<Addr> = observations
        .iter()
        .filter(|(_, row)| {
            !row.kinds.is_empty()
                && row
                    .kinds
                    .iter()
                    .all(|kind| kind == "known" || kind == "module")
        })
        .filter_map(|(&site, _)| {
            let function = program.functions.get(&program.containing(site)?)?;
            let span = function
                .spans
                .iter()
                .find(|span| span.start <= site && site < span.end())?;
            let insn = decode_mapped(&program, &decoder, span, site).ok()?;
            (insn.mnemonic.eq_ignore_ascii_case("CALL")
                && insn.operands.first().is_some_and(|operand| {
                    matches!(
                        operand.kind,
                        OperandKind::Reg { .. } | OperandKind::Mem { .. }
                    )
                }))
            .then_some(site)
        })
        .collect();
    let mut generated = Vec::new();
    let mut rejected = Vec::new();
    let mut unresolved: BTreeMap<String, usize> = BTreeMap::new();
    let mut com_analysis = None;
    let mut com_slots = BTreeMap::new();
    for (&site, observed) in &observations {
        if observed.identities.is_empty() {
            continue;
        }
        if observed.unclassified_shim {
            rejected.push(json_reject(
                site,
                None,
                "mixed classified and unclassified shim observations",
            ));
            continue;
        }
        if observed.identities.len() != 1 {
            rejected.push(json_reject(site, None, "conflicting observed identities"));
            continue;
        }
        let identity = observed.identities.iter().next().unwrap();
        let Some((dll, interface, method)) = com_identity(identity) else {
            continue;
        };
        let Some(meta) = methods.get(&normalize_identity(identity)) else {
            *unresolved
                .entry("method missing from COM catalog".to_string())
                .or_default() += 1;
            continue;
        };
        let override_ = policy.iter().find(|row| row.matches(site, identity, meta));
        let selected = analysis
            .families
            .iter()
            .any(|family| wildcard(family, identity))
            || override_.is_some();
        if !selected {
            *unresolved
                .entry("outside configured families".to_string())
                .or_default() += 1;
            continue;
        }
        if !override_
            .and_then(|row| row.enabled)
            .unwrap_or(analysis.assert_effect_coverage)
        {
            *unresolved
                .entry("observation effect coverage disabled".to_string())
                .or_default() += 1;
            continue;
        }
        if observed.kinds.len() != 1 || !observed.kinds.contains("shim") {
            rejected.push(json_reject(
                site,
                Some(identity),
                "mixed shim and guest observations",
            ));
            continue;
        }
        if !meta.dll.eq_ignore_ascii_case(dll)
            || !meta.interface.eq_ignore_ascii_case(interface)
            || !meta.method.eq_ignore_ascii_case(method)
        {
            rejected.push(json_reject(
                site,
                Some(identity),
                "COM catalog identity mismatch",
            ));
            continue;
        }
        if let Err(reason) = validate_site(
            &program,
            &decoder,
            site,
            meta,
            &mut com_analysis,
            &mut com_slots,
        ) {
            rejected.push(json_reject(site, Some(identity), &reason));
            continue;
        }
        let Some(effect) = override_
            .and_then(|row| row.effect.clone())
            .or_else(|| select_effect(&effects, site, identity))
        else {
            *unresolved
                .entry("no audited effect declaration".to_string())
                .or_default() += 1;
            continue;
        };
        let mut site_evidence = effect.evidence.clone();
        site_evidence.extend(
            observed
                .sources
                .iter()
                .map(|source| format!("observation {source}: site {site:08x} -> {identity}")),
        );
        site_evidence.push(format!(
            "validated COM call at {site:08x}: slot {:#x}, argc {} from pinned headers",
            meta.slot, meta.argc
        ));
        let method_evidence = vec![format!(
            "COM method {identity}: slot {:#x}, argc {} from pinned interface catalog",
            meta.slot, meta.argc
        )];
        generated.push(Selected {
            site,
            identity: identity.clone(),
            meta: meta.clone(),
            effect: effect.effect,
            method_evidence,
            site_evidence,
        });
    }

    let selected_sites: BTreeSet<Addr> = generated.iter().map(|row| row.site).collect();
    let effect_map: BTreeMap<Addr, CallEffect> = generated
        .iter()
        .map(|row| {
            let mut effect = row.effect.clone();
            effect.site = Some(row.site);
            (row.site, effect)
        })
        .collect();
    let mut discovery = serde_json::json!({
        "reuse_seeds": analysis.reuse_discovery_seeds,
        "mode": if analysis.reuse_discovery_seeds { "configured-seeds" } else { "clean" },
    });
    let guest = if analysis.guest_bounds {
        discovery["functions_before"] = serde_json::json!(program.functions.len());
        let mut discovery_settings = settings.clone();
        discovery_settings.indirect_targets = None;
        discovery_settings.discovery_evidence = None;
        if !analysis.reuse_discovery_seeds {
            discovery_settings.analysis_facts = None;
        }
        let seeds = seed_provenance(&settings, analysis.reuse_discovery_seeds)?;
        discovery["seeds"] = seeds;
        eprintln!("analysis guest: discovery");
        let (program, discovery_converged) =
            guest::discover_program(&discovery_settings, &program)?;
        discovery["converged"] = serde_json::json!(discovery_converged);
        if !discovery_converged {
            return Err(Error::input(
                "guest analysis discovery did not converge; refusing to emit bounds",
            ));
        }
        discovery["functions_after"] = serde_json::json!(program.functions.len());
        discovery["function_universe_sha256"] =
            serde_json::json!(function_universe_digest(&program));
        eprintln!("analysis guest: front end");
        let frontend = rk_frontend::analyze(&program);
        let mut observed_sites = BTreeMap::new();
        for (&site, row) in &observations {
            let blocked = row.kinds.iter().any(|kind| kind != "known");
            if blocked || !row.targets.is_empty() {
                observed_sites.insert(
                    site,
                    guest::ObservedSite {
                        targets: row.targets.clone(),
                        blocked,
                    },
                );
            }
        }
        let mut output = guest::produce(
            &settings,
            &program,
            &frontend,
            &effect_map,
            &observed_sites,
            guest::Options::from_config(&analysis),
        )?;
        output
            .sites
            .retain(|site| !selected_sites.contains(&site.site));
        output.report["discovery"] = discovery.clone();
        output
    } else {
        guest::Output::default()
    };
    let artifacts = build_artifacts(
        &analysis,
        &settings,
        &method_bytes,
        &effect_bytes,
        &observation_inputs,
        &guest_preferences,
        generated,
        guest,
        &discovery,
        rejected,
        unresolved,
    )?;
    let candidate = AnalysisFacts::parse(&artifacts.text, &args.out, &settings.sha256)?;
    let mut report = artifacts.report;
    if let Some(path) = &args.compare {
        report["compare"] = compare(&candidate, path, &settings.sha256)?;
    }
    write_atomic(&args.out, artifacts.text.as_bytes())?;
    if let Some(path) = &args.report {
        write_atomic(path, json_text(&report, true)?.as_bytes())?;
    }
    println!(
        "analysis facts: generated {} sites, {} methods, {} effects; rejected {}; unresolved {}",
        report["generated"]["sites"],
        report["generated"]["methods"],
        report["generated"]["effects"],
        report["rejected"].as_array().map_or(0, Vec::len),
        report["unresolved"].as_object().map_or(0, |rows| rows
            .values()
            .filter_map(|n| n.as_u64())
            .sum::<u64>()),
    );
    Ok(())
}

fn method_index(catalog: MethodCatalog) -> Result<BTreeMap<String, MethodMeta>> {
    let mut out = BTreeMap::new();
    for mut row in catalog.methods {
        row.dll = row.dll.to_ascii_lowercase();
        let identity = format!("{}!{}::{}", row.dll, row.interface, row.method);
        if out.insert(normalize_identity(&identity), row).is_some() {
            return Err(Error::input(format!("COM catalog repeats {identity}")));
        }
    }
    Ok(out)
}

#[derive(Clone, Debug, Default)]
struct EffectIndex {
    base: BTreeMap<String, CallEffect>,
    selectors: BTreeMap<String, EffectDef>,
}

fn effect_index(catalog: EffectCatalog) -> Result<EffectIndex> {
    let mut base = BTreeMap::new();
    for (name, row) in catalog.effects {
        let effect = CallEffect {
            site: None,
            import: None,
            regs: row.regs,
            x87: row.x87,
        };
        if effect.regs.is_none() && effect.x87.is_none() {
            return Err(Error::input(format!(
                "effect catalog entry {name} sets neither regs nor x87"
            )));
        }
        if let Some(X87Effect::Masked(X87Mask {
            preserved,
            known,
            value,
        })) = effect.x87
        {
            if value & !known != 0 || known & preserved != 0 {
                return Err(Error::input(format!(
                    "effect catalog entry {name} has invalid x87 masks"
                )));
            }
        }
        base.insert(name, effect);
    }
    let lookup = |name: &str| {
        base.get(name)
            .cloned()
            .ok_or_else(|| Error::input(format!("effect catalog references missing effect {name}")))
    };
    let mut out = BTreeMap::new();
    for row in catalog.families {
        out.insert(
            format!("family:{}", row.pattern),
            EffectDef {
                effect: lookup(&row.effect)?,
                evidence: row.evidence,
            },
        );
    }
    for row in catalog.methods {
        out.insert(
            format!("method:{}", normalize_identity(&row.identity)),
            EffectDef {
                effect: lookup(&row.effect)?,
                evidence: row.evidence,
            },
        );
    }
    for row in catalog.sites {
        let site = parse_addr(&row.site).ok_or_else(|| {
            Error::input(format!(
                "effect catalog site {:?} is not an address",
                row.site
            ))
        })?;
        out.insert(
            format!("site:{site:08x}"),
            EffectDef {
                effect: lookup(&row.effect)?,
                evidence: row.evidence,
            },
        );
    }
    for (key, row) in &out {
        if row.evidence.is_empty() {
            return Err(Error::input(format!(
                "effect catalog selector {key} has no evidence"
            )));
        }
    }
    Ok(EffectIndex {
        base,
        selectors: out,
    })
}

fn select_effect(effects: &EffectIndex, site: Addr, identity: &str) -> Option<EffectDef> {
    effects
        .selectors
        .get(&format!("site:{site:08x}"))
        .or_else(|| {
            effects
                .selectors
                .get(&format!("method:{}", normalize_identity(identity)))
        })
        .or_else(|| {
            effects
                .selectors
                .iter()
                .filter_map(|(key, effect)| {
                    key.strip_prefix("family:").map(|pattern| (pattern, effect))
                })
                .filter(|(pattern, _)| wildcard(pattern, identity))
                .max_by_key(|(pattern, _)| {
                    pattern.bytes().filter(|b| *b != b'*' && *b != b'?').count()
                })
                .map(|(_, effect)| effect)
        })
        .cloned()
}

impl PolicyOverride {
    fn matches(&self, site: Addr, identity: &str, meta: &MethodMeta) -> bool {
        self.site.is_none_or(|wanted| wanted == site)
            && self
                .family
                .as_ref()
                .is_none_or(|pattern| wildcard(pattern, identity))
            && self
                .interface
                .as_ref()
                .is_none_or(|name| *name == meta.interface)
            && self.method.as_ref().is_none_or(|name| *name == meta.method)
    }

    fn rank(&self) -> (bool, bool, bool, usize) {
        (
            self.site.is_some(),
            self.method.is_some(),
            self.interface.is_some(),
            self.family.as_ref().map_or(0, |p| {
                p.bytes().filter(|b| *b != b'*' && *b != b'?').count()
            }),
        )
    }
}

fn policy_index(
    effects: &EffectIndex,
    overrides: Vec<AnalysisOverride>,
) -> Result<Vec<PolicyOverride>> {
    let mut out = Vec::new();
    for row in overrides {
        if row.family.is_none()
            && row.interface.is_none()
            && row.method.is_none()
            && row.site.is_none()
        {
            return Err(Error::input(
                "analysis override needs a family, interface, method or site",
            ));
        }
        if row.effect.is_none() && row.enabled.is_none() {
            return Err(Error::input("analysis override needs enabled or effect"));
        }
        let effect = row
            .effect
            .map(|name| {
                let effect = effects
                    .base
                    .get(&name)
                    .ok_or_else(|| Error::input(format!("unknown override effect {name}")))?;
                if row.evidence.is_empty() || row.evidence.iter().any(|text| text.is_empty()) {
                    return Err(Error::input("effect overrides require evidence"));
                }
                Ok(EffectDef {
                    effect: effect.clone(),
                    evidence: row.evidence,
                })
            })
            .transpose()?;
        let site = row
            .site
            .as_deref()
            .map(|s| {
                parse_addr(s).ok_or_else(|| Error::input(format!("invalid override site {s:?}")))
            })
            .transpose()?;
        out.push(PolicyOverride {
            effect,
            enabled: row.enabled,
            family: row.family,
            interface: row.interface,
            method: row.method,
            site,
        });
    }
    out.reverse();
    out.sort_by_key(|row| std::cmp::Reverse(row.rank()));
    Ok(out)
}

fn validate_site(
    program: &rk_core::program::Program,
    decoder: &Decoder,
    site: Addr,
    meta: &MethodMeta,
    analysis: &mut Option<rk_frontend::Analysis>,
    slots: &mut BTreeMap<Addr, std::result::Result<BTreeMap<Addr, i32>, String>>,
) -> std::result::Result<(), String> {
    let owner = program
        .containing(site)
        .ok_or_else(|| "site is outside every mapped function".to_string())?;
    let function = program
        .functions
        .get(&owner)
        .ok_or_else(|| "site owner is missing from the code map".to_string())?;
    let span = function
        .spans
        .iter()
        .find(|span| span.start <= site && site < span.end())
        .ok_or_else(|| "site is outside every mapped span".to_string())?;
    let insn = decode_mapped(program, decoder, span, site)?;
    if !insn.mnemonic.eq_ignore_ascii_case("CALL") {
        return Err("site is not a mapped call instruction".to_string());
    }
    let Some(operand) = insn.operands.first() else {
        return Err("call has no operand".to_string());
    };
    if operand.size != Some(32) || insn.address_size != 4 {
        return Err("COM call is not a 32-bit transfer".to_string());
    }
    let slot = match &operand.kind {
        OperandKind::Mem {
            base: Some(_),
            index: None,
            seg: None,
            disp,
            ..
        } => *disp,
        OperandKind::Reg { part: None, .. } => {
            let found = slots.entry(owner).or_insert_with(|| {
                let analysis = analysis.get_or_insert_with(|| rk_frontend::analyze(program));
                guest::com_slots(program, analysis, owner)
            });
            i64::from(
                *found
                    .as_ref()
                    .map_err(Clone::clone)?
                    .get(&site)
                    .ok_or("COM register target has no unambiguous vtable slot")?,
            )
        }
        _ => return Err("COM call has an unsupported target operand".to_string()),
    };
    if slot < 0 || slot as u64 != u64::from(meta.slot) {
        return Err(format!(
            "COM slot {:#x} does not match catalog slot {:#x}",
            slot, meta.slot
        ));
    }
    Ok(())
}

fn decode_mapped(
    program: &rk_core::program::Program,
    decoder: &Decoder,
    span: &rk_core::program::Span,
    site: Addr,
) -> std::result::Result<DecodedInsn, String> {
    if let Some(lengths) = &span.lengths {
        let mut at = span.start;
        for &length in lengths {
            if at == site {
                let raw = program
                    .image
                    .bytes(site, length as usize)
                    .ok_or_else(|| "mapped instruction bytes are unavailable".to_string())?;
                let insn = decoder
                    .decode_span(site, raw)
                    .ok_or_else(|| "mapped instruction does not decode exactly".to_string())?;
                return Ok(insn);
            }
            at += length as u32;
        }
        return Err("site is not an instruction boundary in the mapped span".to_string());
    }
    let mut at = span.start;
    while at <= site {
        let available = (span.end() - at).min(15) as usize;
        let raw = program
            .image
            .bytes(at, available)
            .ok_or_else(|| "span bytes are unavailable".to_string())?;
        let mut found = None;
        for length in 1..=available {
            if let Some(insn) = decoder.decode_span(at, &raw[..length]) {
                found = Some(insn);
                break;
            }
        }
        let insn = found.ok_or_else(|| "span does not decode at mapped boundary".to_string())?;
        if at == site {
            return Ok(insn);
        }
        at += insn.length as u32;
    }
    Err("site is not a decoded instruction boundary".to_string())
}

fn parse_observations(
    path: &Path,
    bytes: &[u8],
    out: &mut BTreeMap<Addr, Observation>,
) -> Result<()> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| Error::input(format!("{}: {error}", path.display())))?;
    let source = rk_image::sha256_hex(bytes);
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            return Err(Error::input(format!(
                "{}:{}: expected site target kind count and optional description",
                path.display(),
                number + 1
            )));
        }
        let site = parse_addr(fields[0]).ok_or_else(|| {
            Error::input(format!(
                "{}:{}: invalid site {:?}",
                path.display(),
                number + 1,
                fields[0]
            ))
        })?;
        let target = parse_addr(fields[1]).ok_or_else(|| {
            Error::input(format!(
                "{}:{}: invalid target {:?}",
                path.display(),
                number + 1,
                fields[1]
            ))
        })?;
        let count = fields[3].parse::<u64>().map_err(|_| {
            Error::input(format!(
                "{}:{}: invalid observation count {:?}",
                path.display(),
                number + 1,
                fields[3]
            ))
        })?;
        if count == 0 {
            return Err(Error::input(format!(
                "{}:{}: observation count must be positive",
                path.display(),
                number + 1
            )));
        }
        let identity = fields.get(4).copied().unwrap_or_default();
        let row = out.entry(site).or_default();
        row.targets.insert(target);
        row.kinds.insert(fields[2].to_string());
        if fields[2] == "shim" && (fields.len() != 5 || com_identity(identity).is_none()) {
            row.unclassified_shim = true;
        }
        if fields[2] == "shim" && fields.len() == 5 && !identity.is_empty() {
            let identity = normalize_identity(identity);
            if com_identity(&identity).is_some() {
                row.identities.insert(identity);
            }
        }
        row.sources.insert(source.clone());
    }
    Ok(())
}

fn seed_provenance(settings: &Settings, reuse: bool) -> Result<serde_json::Value> {
    if !reuse {
        return Ok(serde_json::json!({
            "mode": "clean",
            "sites": 0,
            "target_sets": 0,
            "rows": 0,
        }));
    }
    let Some(path) = &settings.analysis_facts else {
        return Ok(serde_json::json!({
            "mode": "configured",
            "sites": 0,
            "target_sets": 0,
            "rows": 0,
        }));
    };
    let bytes = std::fs::read(path).map_err(|error| Error::io(path, error))?;
    let facts = rk_core::facts::AnalysisFacts::load(path, &settings.sha256)?;
    let mut rows = BTreeSet::new();
    for (&site, row) in &facts.sites {
        if let Some(targets) = &row.targets {
            let kind = match row.kind {
                rk_core::facts::TransferKind::Call => "call",
                rk_core::facts::TransferKind::Jump => "jump",
            };
            for &target in targets {
                rows.insert(format!("{site:08x}\t{kind}\t{target:08x}"));
            }
        }
    }
    let normalized = rows.iter().cloned().collect::<Vec<_>>().join("\n");
    Ok(serde_json::json!({
        "mode": "configured",
        "file_sha256": rk_image::sha256_hex(&bytes),
        "normalized_sha256": rk_image::sha256_hex(normalized.as_bytes()),
        "sites": facts
            .sites
            .values()
            .filter(|row| row.targets.is_some())
            .count(),
        "target_sets": facts
            .sites
            .values()
            .filter(|row| row.targets.is_some())
            .count(),
        "target_entries": rows.len(),
        "rows": rows.len(),
    }))
}

fn function_universe_digest(program: &rk_core::program::Program) -> String {
    let rows = program
        .functions
        .keys()
        .map(|addr| format!("{addr:08x}"))
        .collect::<Vec<_>>()
        .join("\n");
    rk_image::sha256_hex(rows.as_bytes())
}

fn parse_addr(text: &str) -> Option<Addr> {
    (text.len() == 8 && text.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| Addr::from_str_radix(text, 16).ok())
        .flatten()
}

fn normalize_identity(identity: &str) -> String {
    match identity.split_once('!') {
        Some((dll, rest)) => format!("{}!{rest}", dll.to_ascii_lowercase()),
        None => identity.to_string(),
    }
}

fn com_identity(identity: &str) -> Option<(&str, &str, &str)> {
    let (dll, rest) = identity.split_once('!')?;
    let (interface, method) = rest.split_once("::")?;
    (!dll.is_empty()
        && !interface.is_empty()
        && !method.is_empty()
        && !method.contains("::")
        && !interface.contains('!'))
    .then_some((dll, interface, method))
}

fn wildcard(pattern: &str, text: &str) -> bool {
    fn at(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.first() {
            None => text.is_empty(),
            Some(b'*') => at(&pattern[1..], text) || (!text.is_empty() && at(pattern, &text[1..])),
            Some(b'?') => !text.is_empty() && at(&pattern[1..], &text[1..]),
            Some(byte) => text.first() == Some(byte) && at(&pattern[1..], &text[1..]),
        }
    }
    at(pattern.as_bytes(), text.as_bytes())
}

fn fields_json(fields: Fields) -> serde_json::Value {
    serde_json::Value::Array(
        fields
            .iter()
            .map(|field| serde_json::Value::String(field.name().to_string()))
            .collect(),
    )
}

fn contract_json(contract: Contract) -> serde_json::Value {
    serde_json::json!({
        "reads": fields_json(contract.reads),
        "kills": fields_json(contract.kills),
        "writes": fields_json(contract.writes),
        "preserves": fields_json(contract.preserves),
    })
}

fn effect_json(effect: &CallEffect) -> serde_json::Value {
    let mut row = serde_json::Map::new();
    if let Some(regs) = effect.regs {
        row.insert(
            "regs".to_string(),
            match regs {
                RegsEffect::Abi => serde_json::Value::String("abi".to_string()),
                RegsEffect::Unknown => serde_json::Value::String("unknown".to_string()),
                RegsEffect::Contract(contract) => contract_json(contract),
            },
        );
    }
    if let Some(x87) = effect.x87 {
        row.insert(
            "x87".to_string(),
            match x87 {
                X87Effect::Named(rk_core::settings::X87Named::Preserve) => {
                    serde_json::Value::String("preserve".to_string())
                }
                X87Effect::Named(rk_core::settings::X87Named::Unknown) => {
                    serde_json::Value::String("unknown".to_string())
                }
                X87Effect::Masked(X87Mask {
                    preserved,
                    known,
                    value,
                }) => serde_json::json!({
                    "preserved": preserved,
                    "known": known,
                    "value": value,
                }),
            },
        );
    }
    serde_json::Value::Object(row)
}

struct Artifacts {
    text: String,
    report: serde_json::Value,
}

fn build_artifacts(
    analysis: &AnalysisConfig,
    settings: &Settings,
    method_bytes: &[u8],
    effect_bytes: &[u8],
    observation_inputs: &BTreeMap<String, String>,
    guest_preferences: &BTreeSet<Addr>,
    selected: Vec<Selected>,
    guest: guest::Output,
    discovery: &serde_json::Value,
    rejected: Vec<serde_json::Value>,
    unresolved: BTreeMap<String, usize>,
) -> Result<Artifacts> {
    let mut effects: BTreeMap<String, (CallEffect, u32)> = BTreeMap::new();
    let mut methods: BTreeMap<String, (MethodMeta, u32)> = BTreeMap::new();
    let mut sites = Vec::new();
    for row in selected {
        let key = json_text(&effect_json(&row.effect), false)?;
        let effect_id = if let Some((_, id)) = effects.get(&key) {
            *id
        } else {
            let id = effects.len() as u32;
            effects.insert(key, (row.effect.clone(), id));
            id
        };
        let method_key = normalize_identity(&row.identity);
        methods
            .entry(method_key)
            .or_insert_with(|| (row.meta.clone(), 0));
        sites.push((row, effect_id));
    }
    let method_ids: BTreeMap<String, u32> = methods
        .keys()
        .enumerate()
        .map(|(index, identity)| (identity.clone(), index as u32))
        .collect();
    let mut evidence = BTreeSet::new();
    for (row, _) in &sites {
        evidence.extend(row.method_evidence.iter().cloned());
        evidence.extend(row.site_evidence.iter().cloned());
    }
    let guest_evidence = (!guest.sites.is_empty()).then(|| format!(
        "Structural guest vtable bounds from constant table installations; observed targets constrain compatible tables and are not exhaustive. Compatible-cleanup assumption: {}; balanced-returns assumption: {}.",
        analysis.assume_compatible_cleanup,
        analysis.assume_compatible_cleanup && analysis.assume_balanced_returns
    ));
    if let Some(text) = &guest_evidence {
        evidence.insert(text.clone());
    }
    let evidence_ids: BTreeMap<String, u32> = evidence
        .iter()
        .enumerate()
        .map(|(index, text)| (text.clone(), index as u32))
        .collect();
    let mut methods_json = Vec::new();
    for (identity, (meta, _)) in &methods {
        let Some((row, _)) = sites
            .iter()
            .find(|(row, _)| normalize_identity(&row.identity) == *identity)
        else {
            continue;
        };
        methods_json.push(serde_json::json!({
            "id": method_ids[identity],
            "identity": identity,
            "interface": meta.interface,
            "method": meta.method,
            "slot": meta.slot,
            "argc": meta.argc,
            "evidence": row.method_evidence.iter().map(|text| evidence_ids[text]).collect::<Vec<_>>(),
        }));
    }
    let mut target_sets: BTreeMap<Vec<Addr>, u32> = BTreeMap::new();
    for site in &guest.sites {
        if !target_sets.contains_key(&site.targets) {
            let id = target_sets.len() as u32;
            target_sets.insert(site.targets.clone(), id);
        }
    }
    let mut sites_json: Vec<serde_json::Value> = sites
        .iter()
        .map(|(row, effect_id)| {
            serde_json::json!({
                "s": Hex(row.site).to_string(),
                "k": "call",
                "e": effect_id,
                "m": method_ids[&normalize_identity(&row.identity)],
                "ev": row.site_evidence.iter().map(|text| evidence_ids[text]).collect::<Vec<_>>(),
            })
        })
        .collect();
    let guest_evidence_id = guest_evidence.as_ref().map(|text| evidence_ids[text]);
    sites_json.extend(guest.sites.iter().map(|site| {
        serde_json::json!({
            "s": Hex(site.site).to_string(),
            "k": site.kind,
            "t": target_sets[&site.targets],
        })
    }));
    let indices: BTreeMap<String, usize> = sites_json
        .iter()
        .enumerate()
        .map(|(index, row)| (row["s"].as_str().unwrap().to_string(), index))
        .collect();
    for &site in guest_preferences {
        let site = Hex(site).to_string();
        if let Some(&index) = indices.get(&site) {
            sites_json[index]["prefer_guest"] = serde_json::json!(true);
        } else {
            sites_json.push(serde_json::json!({"s": site, "prefer_guest": true}));
        }
    }
    sites_json.sort_by(|a, b| a["s"].as_str().cmp(&b["s"].as_str()));
    let targets_json: Vec<serde_json::Value> = {
        let mut rows: Vec<_> = target_sets.iter().collect();
        rows.sort_by_key(|(_, id)| **id);
        rows.into_iter()
            .map(|(targets, id)| {
                serde_json::json!({
                    "id": id,
                    "targets": targets.iter().map(|target| Hex(*target).to_string()).collect::<Vec<_>>(),
                    "evidence": guest_evidence_id.map_or_else(Vec::new, |id| vec![id]),
                })
            })
            .collect()
    };
    let mut effect_rows: Vec<_> = effects.values().collect();
    effect_rows.sort_by_key(|(_, id)| *id);
    let effects_json: Vec<serde_json::Value> = effect_rows
        .into_iter()
        .map(|(effect, id)| {
            let mut row = serde_json::Map::new();
            row.insert("id".to_string(), serde_json::json!(id));
            if let serde_json::Value::Object(fields) = effect_json(effect) {
                row.extend(fields);
            }
            serde_json::Value::Object(row)
        })
        .collect();
    let code_map_sha256 = hash_tree(&settings.code_map)?;
    let mut config = analysis.clone();
    config.observations.clear();
    config.method_catalog.clear();
    config.effect_catalog.clear();
    let mut discovery_inputs = discovery.clone();
    if let Some(seeds) = discovery_inputs
        .get_mut("seeds")
        .and_then(serde_json::Value::as_object_mut)
    {
        seeds.remove("file_sha256");
    }
    let inference = serde_json::json!({
        "analysis": config,
        "discovery": discovery_inputs,
        "settings": {
            "discover": settings.discover,
            "fault_state": format!("{:?}", settings.fault_state),
            "call_contracts": settings.call_contracts,
            "private_leaf_contracts": settings.private_leaf_contracts,
            "private_input_arguments": settings.private_input_arguments,
            "private_input_registers": settings.private_input_registers,
            "alternate_entries": settings.alternate_entries,
            "configured_entries": settings.configured_entries,
            "call_effect_sites": settings.call_effects.sites.keys().collect::<Vec<_>>(),
            "call_effect_imports": settings.call_effects.imports.keys().collect::<Vec<_>>(),
            "msvc_x87_convention": settings.msvc_x87_convention,
            "x87_cw_clone": settings.x87_cw_clone,
        },
    });
    let config_sha256 = rk_image::sha256_hex(
        &serde_json::to_vec(&inference).map_err(|error| Error::input(error.to_string()))?,
    );
    let executable = std::env::current_exe().map_err(|error| Error::input(error.to_string()))?;
    let producer_sha256 = rk_image::sha256_hex(
        &std::fs::read(&executable).map_err(|error| Error::io(&executable, error))?,
    );
    let mut assumption_rows = Vec::new();
    if !sites.is_empty() {
        assumption_rows.push(serde_json::json!({
            "id": "observed-host-effects",
            "text": "At each emitted site, all executions are assumed to have the audited effects of its observed host method, subject to the configured family and site policies. Observations are not exhaustive target bounds.",
            "requires": []
        }));
    }
    for row in &guest.assumptions {
        assumption_rows.push(serde_json::json!({
            "id": row.id,
            "text": row.text,
            "requires": row.requires,
        }));
    }
    let assumptions = serde_json::Value::Array(assumption_rows);
    let site_count = sites_json.len();
    let method_count = methods_json.len();
    let effect_count = effects_json.len();
    let evidence_count = evidence_ids.len();
    let target_count = target_sets.len();
    let target_entries = target_sets.keys().map(Vec::len).sum::<usize>();
    let value = serde_json::json!({
        "format": FORMAT_V2,
        "executable_sha256": settings.sha256,
        "inputs": {
            "code_map_sha256": code_map_sha256,
            "config_sha256": config_sha256,
            "producer_sha256": producer_sha256,
            "method_catalog_sha256": rk_image::sha256_hex(method_bytes),
            "catalog_sha256": rk_image::sha256_hex(effect_bytes),
            "observations": observation_inputs,
        },
        "assumptions": assumptions,
        "evidence": evidence_ids.iter().map(|(text, id)| serde_json::json!({"id": id, "text": text})).collect::<Vec<_>>(),
        "targets": targets_json,
        "methods": methods_json,
        "effects": effects_json,
        "sites": sites_json,
    });
    let report = serde_json::json!({
        "format": "recomp-analysis-report-v1",
        "generated": {
            "sites": site_count,
            "methods": method_count,
            "effects": effect_count,
            "evidence": evidence_count,
            "target_sets": target_count,
            "target_entries": target_entries,
            "guest_preferences": guest_preferences.len(),
        },
        "guest": guest.report,
        "discovery": discovery,
        "rejected": rejected,
        "unresolved": unresolved,
        "inputs": {
            "code_map_sha256": hash_tree(&settings.code_map)?,
            "config_sha256": config_sha256,
            "producer_sha256": producer_sha256,
            "method_catalog_sha256": rk_image::sha256_hex(method_bytes),
            "catalog_sha256": rk_image::sha256_hex(effect_bytes),
        }
    });
    Ok(Artifacts {
        text: json_text(&value, true)? + "\n",
        report,
    })
}

fn json_reject(site: Addr, identity: Option<&str>, reason: &str) -> serde_json::Value {
    serde_json::json!({
        "site": Hex(site).to_string(),
        "identity": identity,
        "reason": reason,
    })
}

fn compare(new: &AnalysisFacts, old: &Path, sha256: &str) -> Result<serde_json::Value> {
    let old = AnalysisFacts::load(old, sha256)?;
    let old_effects: BTreeMap<Addr, CallEffect> = old
        .sites
        .iter()
        .filter_map(|(&site, row)| row.effect.clone().map(|effect| (site, effect)))
        .collect();
    let new_effects: BTreeMap<Addr, CallEffect> = new
        .sites
        .iter()
        .filter_map(|(&site, row)| row.effect.clone().map(|effect| (site, effect)))
        .collect();
    let same = old_effects
        .iter()
        .filter(|(site, effect)| new_effects.get(site) == Some(*effect))
        .count();
    let changed: Vec<_> = old_effects
        .iter()
        .filter(|(site, effect)| new_effects.get(site).is_some_and(|other| other != *effect))
        .map(|(site, _)| Hex(*site).to_string())
        .collect();
    let missing: Vec<_> = old_effects
        .keys()
        .filter(|site| !new_effects.contains_key(site))
        .map(|site| Hex(*site).to_string())
        .collect();
    let added: Vec<_> = new_effects
        .keys()
        .filter(|site| !old_effects.contains_key(site))
        .map(|site| Hex(*site).to_string())
        .collect();
    let old_targets: BTreeMap<Addr, Vec<Addr>> = old
        .sites
        .iter()
        .filter_map(|(&site, row)| row.targets.clone().map(|targets| (site, targets)))
        .collect();
    let new_targets: BTreeMap<Addr, Vec<Addr>> = new
        .sites
        .iter()
        .filter_map(|(&site, row)| row.targets.clone().map(|targets| (site, targets)))
        .collect();
    let mut target_same = 0usize;
    let mut target_narrower = Vec::new();
    let mut target_wider = Vec::new();
    let mut target_incomparable = Vec::new();
    for (site, old) in &old_targets {
        match new_targets.get(site) {
            Some(new) if new == old => target_same += 1,
            Some(new) if new.iter().all(|target| old.contains(target)) => {
                target_narrower.push(Hex(*site).to_string())
            }
            Some(new) if old.iter().all(|target| new.contains(target)) => {
                target_wider.push(Hex(*site).to_string())
            }
            Some(_) => target_incomparable.push(Hex(*site).to_string()),
            None => {}
        }
    }
    let target_missing: Vec<_> = old_targets
        .keys()
        .filter(|site| !new_targets.contains_key(site))
        .map(|site| Hex(*site).to_string())
        .collect();
    let target_added: Vec<_> = new_targets
        .keys()
        .filter(|site| !old_targets.contains_key(site))
        .map(|site| Hex(*site).to_string())
        .collect();
    Ok(serde_json::json!({
        "effects": {
            "old_sites": old_effects.len(),
            "new_sites": new_effects.len(),
            "same": same,
            "changed": changed,
            "missing_from_candidate": missing,
            "added_in_candidate": added,
        },
        "targets": {
            "old_sites": old_targets.len(),
            "new_sites": new_targets.len(),
            "same": target_same,
            "narrower": target_narrower,
            "wider": target_wider,
            "incomparable": target_incomparable,
            "missing_from_candidate": target_missing,
            "added_in_candidate": target_added,
        },
    }))
}

fn hash_tree(path: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    for name in [
        "entries.tsv",
        "metadata.txt",
        "noreturn.tsv",
        "spans.tsv",
        "tables.tsv",
    ] {
        let file = path.join(name);
        digest.update(name.as_bytes());
        digest.update([0]);
        digest.update(std::fs::read(&file).map_err(|error| Error::io(&file, error))?);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    }
    let temporary = path.with_extension(format!(
        "{}.new",
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or("tmp")
    ));
    std::fs::write(&temporary, bytes).map_err(|error| Error::io(&temporary, error))?;
    std::fs::rename(&temporary, path).map_err(|error| Error::io(path, error))
}

fn json_text(value: &serde_json::Value, pretty: bool) -> Result<String> {
    let text = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    text.map_err(|error| Error::input(format!("cannot serialize JSON: {error}")))
}
