use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;

use crate::settings::{CallEffect, RegsEffect, X87Effect};
use crate::{parse_hex, Addr, Error, Result};

pub const FORMAT_V1: &str = "recomp-analysis-facts-v1";
pub const FORMAT_V2: &str = "recomp-analysis-facts-v2";
pub const FORMAT: &str = FORMAT_V2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferKind {
    #[default]
    Call,
    Jump,
}

#[derive(Clone, Debug)]
pub struct SiteFacts {
    pub kind: TransferKind,
    pub prefer_guest: bool,
    pub targets: Option<Vec<Addr>>,
    pub effect: Option<CallEffect>,
    pub method: Option<u32>,
    pub effect_evidence: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct MethodFacts {
    pub identity: String,
    pub interface: String,
    pub method: String,
    pub slot: Option<u32>,
    pub argc: Option<u32>,
    pub effect: Option<CallEffect>,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Assumption {
    pub id: String,
    pub text: String,
    pub requires: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct InputDigests {
    pub producer_sha256: Option<String>,
    pub code_map_sha256: Option<String>,
    pub config_sha256: Option<String>,
    pub catalog_sha256: Option<String>,
    pub method_catalog_sha256: Option<String>,
    pub observations: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct ImportFacts {
    pub effect: CallEffect,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub target_sets: usize,
    pub target_entries: usize,
    pub call_bounds: usize,
    pub jump_bounds: usize,
    pub site_effects: usize,
    pub import_effects: usize,
    pub methods: usize,
    pub assumptions: usize,
    pub evidence: usize,
}

#[derive(Clone, Debug, Default)]
pub struct AnalysisFacts {
    pub sites: BTreeMap<Addr, SiteFacts>,
    pub imports: BTreeMap<String, ImportFacts>,
    pub methods: BTreeMap<u32, MethodFacts>,
    pub assumptions: Vec<Assumption>,
    pub inputs: InputDigests,
    pub summary: Summary,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFacts {
    format: String,
    executable_sha256: String,
    #[serde(default)]
    evidence: Vec<RawEvidence>,
    #[serde(default, rename = "targets")]
    target_sets: Vec<RawTargetSet>,
    #[serde(default)]
    effects: Vec<RawEffect>,
    #[serde(default)]
    sites: Vec<RawSite>,
    #[serde(default)]
    imports: Vec<RawImport>,
    #[serde(default)]
    methods: Option<Vec<RawMethod>>,
    #[serde(default)]
    assumptions: Option<Vec<RawAssumption>>,
    #[serde(default)]
    inputs: Option<RawInputs>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMethod {
    id: u32,
    identity: String,
    #[serde(default)]
    interface: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    slot: Option<u32>,
    #[serde(default)]
    argc: Option<u32>,
    #[serde(default)]
    effect: Option<u32>,
    #[serde(default)]
    evidence: Vec<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAssumption {
    id: String,
    text: String,
    #[serde(default)]
    requires: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInputs {
    #[serde(default)]
    producer_sha256: Option<String>,
    #[serde(default)]
    code_map_sha256: Option<String>,
    #[serde(default)]
    config_sha256: Option<String>,
    #[serde(default)]
    catalog_sha256: Option<String>,
    #[serde(default)]
    method_catalog_sha256: Option<String>,
    #[serde(default)]
    observations: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEvidence {
    id: u32,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTargetSet {
    id: u32,
    targets: Vec<RawAddr>,
    evidence: Vec<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEffect {
    id: u32,
    #[serde(default)]
    regs: Option<RegsEffect>,
    #[serde(default)]
    x87: Option<X87Effect>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSite {
    #[serde(rename = "s")]
    site: RawAddr,
    #[serde(default, rename = "k")]
    kind: TransferKind,
    #[serde(default)]
    prefer_guest: bool,
    #[serde(default, rename = "t")]
    targets: Option<u32>,
    #[serde(default, rename = "e")]
    effect: Option<u32>,
    #[serde(default, rename = "m")]
    method: Option<u32>,
    #[serde(default, rename = "ev")]
    effect_evidence: Vec<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImport {
    name: String,
    effect: u32,
    #[serde(default)]
    evidence: Vec<u32>,
}

#[derive(Clone, Copy)]
struct RawAddr(Addr);

impl<'de> Deserialize<'de> for RawAddr {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        let valid = text.len() == 8 && text.bytes().all(|b| b.is_ascii_hexdigit());
        parse_hex(text.as_str())
            .filter(|_| valid)
            .map(RawAddr)
            .ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "expected an 8-digit hexadecimal address, not {text:?}"
                ))
            })
    }
}

impl AnalysisFacts {
    pub fn load(path: &Path, sha256: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        Self::parse(&text, path, sha256)
    }

    pub fn parse(text: &str, path: &Path, sha256: &str) -> Result<Self> {
        let raw: RawFacts = serde_json::from_str(text)
            .map_err(|e| Error::input(format!("{}: {e}", path.display())))?;
        if raw.format != FORMAT_V1 && raw.format != FORMAT_V2 {
            return Err(Error::input(format!(
                "{}: expected format {FORMAT_V1:?} or {FORMAT_V2:?}, not {:?}",
                path.display(),
                raw.format
            )));
        }
        let v2 = raw.format == FORMAT_V2;
        if !v2 && (raw.methods.is_some() || raw.assumptions.is_some() || raw.inputs.is_some()) {
            return Err(Error::input(format!(
                "{}: v1 facts cannot carry methods, assumptions or inputs",
                path.display()
            )));
        }
        if v2 && raw.inputs.is_none() {
            return Err(Error::input(format!(
                "{}: v2 facts require input digests",
                path.display()
            )));
        }
        if !raw.executable_sha256.eq_ignore_ascii_case(sha256) {
            return Err(Error::input(format!(
                "{}: executable_sha256 does not match {}",
                path.display(),
                sha256
            )));
        }

        let mut evidence = BTreeMap::new();
        for row in raw.evidence {
            if row.text.is_empty() {
                return Err(Error::input(format!(
                    "{}: evidence {} has empty text",
                    path.display(),
                    row.id
                )));
            }
            if evidence.insert(row.id, row.text).is_some() {
                return Err(Error::input(format!(
                    "{}: evidence {} repeats",
                    path.display(),
                    row.id
                )));
            }
        }

        let mut used_evidence = BTreeSet::new();
        let mut target_sets = BTreeMap::new();
        let mut target_entries = 0;
        for row in raw.target_sets {
            let mut seen_evidence = BTreeSet::new();
            if row.evidence.is_empty() {
                return Err(Error::input(format!(
                    "{}: target set {} has no evidence",
                    path.display(),
                    row.id
                )));
            }
            for note in row.evidence {
                used_evidence.insert(note);
                if !seen_evidence.insert(note) || !evidence.contains_key(&note) {
                    return Err(Error::input(format!(
                        "{}: target set {} references invalid evidence {note}",
                        path.display(),
                        row.id
                    )));
                }
            }
            if row.targets.is_empty() {
                return Err(Error::input(format!(
                    "{}: target set {} is empty",
                    path.display(),
                    row.id
                )));
            }
            let mut seen = BTreeSet::new();
            let targets: Vec<Addr> = row.targets.into_iter().map(|a| a.0).collect();
            for &target in &targets {
                if !seen.insert(target) {
                    return Err(Error::input(format!(
                        "{}: target set {} repeats {target:08x}",
                        path.display(),
                        row.id
                    )));
                }
            }
            target_entries += targets.len();
            if target_sets.insert(row.id, targets).is_some() {
                return Err(Error::input(format!(
                    "{}: target set {} repeats",
                    path.display(),
                    row.id
                )));
            }
        }

        let mut effects = BTreeMap::new();
        for row in raw.effects {
            let Some(effect) = effect(row.regs, row.x87, path, row.id)? else {
                return Err(Error::input(format!(
                    "{}: effect {} must set regs or x87",
                    path.display(),
                    row.id
                )));
            };
            if effects.insert(row.id, effect).is_some() {
                return Err(Error::input(format!(
                    "{}: effect {} repeats",
                    path.display(),
                    row.id
                )));
            }
        }

        let check_refs = |ids: &[u32], what: &str, id: u32| -> Result<Vec<String>> {
            let mut out = Vec::with_capacity(ids.len());
            let mut seen = BTreeSet::new();
            for &note in ids {
                if !seen.insert(note) {
                    return Err(Error::input(format!(
                        "{}: {what} {id} repeats evidence {note}",
                        path.display()
                    )));
                }
                let Some(text) = evidence.get(&note) else {
                    return Err(Error::input(format!(
                        "{}: {what} {id} references missing evidence {note}",
                        path.display()
                    )));
                };
                out.push(text.clone());
            }
            Ok(out)
        };
        let mut methods = BTreeMap::new();
        for row in raw.methods.unwrap_or_default() {
            let (dll, interface, method) = method_identity(&row.identity).ok_or_else(|| {
                Error::input(format!(
                    "{}: method {} identity {:?} must be dll!Interface::Method",
                    path.display(),
                    row.id,
                    row.identity
                ))
            })?;
            if row
                .interface
                .as_deref()
                .is_some_and(|value| value != interface)
            {
                return Err(Error::input(format!(
                    "{}: method {} interface does not match identity",
                    path.display(),
                    row.id
                )));
            }
            if row.method.as_deref().is_some_and(|value| value != method) {
                return Err(Error::input(format!(
                    "{}: method {} name does not match identity",
                    path.display(),
                    row.id
                )));
            }
            let effect = row
                .effect
                .map(|id| {
                    effects.get(&id).cloned().ok_or_else(|| {
                        Error::input(format!(
                            "{}: method {} references missing effect {id}",
                            path.display(),
                            row.id
                        ))
                    })
                })
                .transpose()?;
            used_evidence.extend(row.evidence.iter().copied());
            let evidence = check_refs(&row.evidence, "method", row.id)?;
            if effect.is_some() && evidence.is_empty() {
                return Err(Error::input(format!(
                    "{}: method {} has an effect without evidence",
                    path.display(),
                    row.id
                )));
            }
            let id = row.id;
            let row = MethodFacts {
                identity: format!("{}!{interface}::{method}", dll.to_lowercase()),
                interface: interface.to_string(),
                method: method.to_string(),
                slot: row.slot,
                argc: row.argc,
                effect,
                evidence,
            };
            if methods.insert(id, row).is_some() {
                return Err(Error::input(format!(
                    "{}: method {} repeats",
                    path.display(),
                    id
                )));
            }
        }

        let mut assumptions = Vec::new();
        let mut assumption_ids = BTreeSet::new();
        for row in raw.assumptions.unwrap_or_default() {
            if row.id.is_empty() || row.text.is_empty() {
                return Err(Error::input(format!(
                    "{}: assumptions require non-empty id and text",
                    path.display()
                )));
            }
            let mut requires = BTreeSet::new();
            for required in &row.requires {
                if !requires.insert(required.clone()) {
                    return Err(Error::input(format!(
                        "{}: assumption {} repeats dependency {:?}",
                        path.display(),
                        row.id,
                        required
                    )));
                }
            }
            if !assumption_ids.insert(row.id.clone()) {
                return Err(Error::input(format!(
                    "{}: assumption {} repeats",
                    path.display(),
                    row.id
                )));
            }
            assumptions.push(Assumption {
                id: row.id,
                text: row.text,
                requires: requires.into_iter().collect(),
            });
        }
        let missing: BTreeSet<String> = assumptions
            .iter()
            .flat_map(|row| row.requires.iter())
            .filter(|required| !assumption_ids.contains(*required))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(Error::input(format!(
                "{}: assumptions have missing dependencies {:?}",
                path.display(),
                missing
            )));
        }
        let mut known: BTreeSet<String> = BTreeSet::new();
        loop {
            let resolved: BTreeSet<String> = assumptions
                .iter()
                .filter(|row| row.requires.iter().all(|required| known.contains(required)))
                .map(|row| row.id.clone())
                .filter(|id| !known.contains(id))
                .collect();
            if resolved.is_empty() {
                break;
            }
            known.extend(resolved);
        }
        if known != assumption_ids {
            let cyclic: Vec<&str> = assumptions
                .iter()
                .filter(|row| !known.contains(&row.id))
                .map(|row| row.id.as_str())
                .collect();
            return Err(Error::input(format!(
                "{}: assumptions have cyclic dependencies {:?}",
                path.display(),
                cyclic
            )));
        }

        let inputs = match raw.inputs {
            Some(raw) => {
                let mut observations = BTreeMap::new();
                for (label, hash) in raw.observations {
                    if label.is_empty() {
                        return Err(Error::input(format!(
                            "{}: observation labels must be non-empty",
                            path.display()
                        )));
                    }
                    let name = format!("observation {label:?}");
                    observations.insert(label, digest_hex(&hash, path, &name)?);
                }
                InputDigests {
                    producer_sha256: digest(raw.producer_sha256, path, "producer_sha256")?,
                    code_map_sha256: digest(raw.code_map_sha256, path, "code_map_sha256")?,
                    config_sha256: digest(raw.config_sha256, path, "config_sha256")?,
                    catalog_sha256: digest(raw.catalog_sha256, path, "catalog_sha256")?,
                    method_catalog_sha256: digest(
                        raw.method_catalog_sha256,
                        path,
                        "method_catalog_sha256",
                    )?,
                    observations,
                }
            }
            None => InputDigests::default(),
        };

        let mut sites = BTreeMap::new();
        let mut call_bounds = 0;
        let mut jump_bounds = 0;
        let mut site_effects = 0;
        for row in raw.sites {
            let site = row.site.0;
            if row.targets.is_none() && row.effect.is_none() && !row.prefer_guest {
                return Err(Error::input(format!(
                    "{}: site {site:08x} has neither bound, effect nor dispatch preference",
                    path.display()
                )));
            }
            if row.prefer_guest && (!v2 || row.kind != TransferKind::Call) {
                return Err(Error::input(format!(
                    "{}: site {site:08x} guest dispatch preference requires a v2 call site",
                    path.display()
                )));
            }
            if row.effect.is_none() && !row.effect_evidence.is_empty() {
                return Err(Error::input(format!(
                    "{}: site {site:08x} has evidence without an effect",
                    path.display()
                )));
            }
            if !v2 && row.method.is_some() {
                return Err(Error::input(format!(
                    "{}: v1 facts cannot carry method references",
                    path.display()
                )));
            }
            let method = row
                .method
                .map(|id| {
                    methods.contains_key(&id).then_some(id).ok_or_else(|| {
                        Error::input(format!(
                            "{}: site {site:08x} references missing method {id}",
                            path.display()
                        ))
                    })
                })
                .transpose()?;
            let targets = row
                .targets
                .map(|id| {
                    target_sets.get(&id).cloned().ok_or_else(|| {
                        Error::input(format!(
                            "{}: site {site:08x} references missing target set {id}",
                            path.display()
                        ))
                    })
                })
                .transpose()?;
            match row.kind {
                TransferKind::Call => {
                    if targets.is_some() {
                        call_bounds += 1;
                    }
                }
                TransferKind::Jump => {
                    if targets.is_some() {
                        jump_bounds += 1;
                    }
                }
            }
            let effect = row
                .effect
                .map(|id| {
                    effects.get(&id).cloned().ok_or_else(|| {
                        Error::input(format!(
                            "{}: site {site:08x} references missing effect {id}",
                            path.display()
                        ))
                    })
                })
                .transpose()?;
            if effect.is_some() {
                site_effects += 1;
            }
            used_evidence.extend(row.effect_evidence.iter().copied());
            let effect_evidence = check_refs(&row.effect_evidence, "site", site)?;
            if effect.is_some() && effect_evidence.is_empty() {
                return Err(Error::input(format!(
                    "{}: site {site:08x} has an effect without evidence",
                    path.display()
                )));
            }
            let row = SiteFacts {
                kind: row.kind,
                prefer_guest: row.prefer_guest,
                targets,
                effect,
                method,
                effect_evidence,
            };
            if sites.insert(site, row).is_some() {
                return Err(Error::input(format!(
                    "{}: site {site:08x} repeats",
                    path.display()
                )));
            }
        }

        let mut imports = BTreeMap::new();
        for row in raw.imports {
            let name = import_name(&row.name).ok_or_else(|| {
                Error::input(format!(
                    "{}: import {:?} must be dll!name",
                    path.display(),
                    row.name
                ))
            })?;
            let Some(effect) = effects.get(&row.effect).cloned() else {
                return Err(Error::input(format!(
                    "{}: import {name} references missing effect {}",
                    path.display(),
                    row.effect
                )));
            };
            used_evidence.extend(row.evidence.iter().copied());
            let evidence = check_refs(&row.evidence, "import", row.effect)?;
            if evidence.is_empty() {
                return Err(Error::input(format!(
                    "{}: import {name} has an effect without evidence",
                    path.display()
                )));
            }
            if imports
                .insert(name.clone(), ImportFacts { effect, evidence })
                .is_some()
            {
                return Err(Error::input(format!(
                    "{}: import {name} repeats",
                    path.display()
                )));
            }
        }

        if evidence.keys().copied().collect::<BTreeSet<_>>() != used_evidence {
            let unused: Vec<u32> = evidence
                .keys()
                .copied()
                .filter(|id| !used_evidence.contains(id))
                .collect();
            return Err(Error::input(format!(
                "{}: unused evidence ids {unused:?}",
                path.display()
            )));
        }
        let summary = Summary {
            target_sets: target_sets.len(),
            target_entries,
            call_bounds,
            jump_bounds,
            site_effects,
            import_effects: imports.len(),
            methods: methods.len(),
            assumptions: assumptions.len(),
            evidence: evidence.len(),
        };
        Ok(Self {
            sites,
            imports,
            methods,
            assumptions,
            inputs,
            summary,
        })
    }
}

fn effect(
    regs: Option<RegsEffect>,
    x87: Option<X87Effect>,
    path: &Path,
    id: u32,
) -> Result<Option<CallEffect>> {
    if let Some(X87Effect::Masked(crate::settings::X87Mask {
        preserved,
        known,
        value,
    })) = x87
    {
        if value & !known != 0 || known & preserved != 0 {
            return Err(Error::input(format!(
                "{}: effect {id} has invalid x87 preserved/known/value masks",
                path.display()
            )));
        }
    }
    if regs.is_none() && x87.is_none() {
        return Ok(None);
    }
    Ok(Some(CallEffect {
        site: None,
        import: None,
        regs,
        x87,
    }))
}

pub fn import_name(text: &str) -> Option<String> {
    let (dll, name) = text.split_once('!')?;
    if dll.is_empty() || name.is_empty() || name.contains('!') || dll.contains('!') {
        return None;
    }
    Some(format!("{}!{name}", dll.to_lowercase()))
}

fn method_identity(text: &str) -> Option<(&str, &str, &str)> {
    let (dll, rest) = text.split_once('!')?;
    let (interface, method) = rest.split_once("::")?;
    if dll.is_empty()
        || interface.is_empty()
        || method.is_empty()
        || method.contains("::")
        || interface.contains('!')
    {
        return None;
    }
    Some((dll, interface, method))
}

fn digest(value: Option<String>, path: &Path, name: &str) -> Result<Option<String>> {
    value
        .as_deref()
        .map(|value| digest_hex(value, path, name))
        .transpose()
}

fn digest_hex(value: &str, path: &Path, name: &str) -> Result<String> {
    let valid = value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit());
    if !valid {
        return Err(Error::input(format!(
            "{}: input {name} must be a SHA-256 digest",
            path.display()
        )));
    }
    Ok(value.to_ascii_lowercase())
}
