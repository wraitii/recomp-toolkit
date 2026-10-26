use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::de::{self, Deserializer};
use serde::Deserialize;

use crate::contract::{Contract, Field, Fields};
use crate::{parse_hex, Addr, Error, Result};

pub const INPUT_FORMAT: &str = "recomp-translate-input-v1";

#[derive(Debug, Deserialize)]
struct Input {
    format: String,
    dir: PathBuf,
    exe: PathBuf,
    code_map: Option<PathBuf>,
    image_base: Addr,
    sha256: String,
    #[serde(default)]
    reloc: Option<PathBuf>,
    translate: Translate,
    #[serde(default)]
    store_hooks: Option<bool>,
    #[serde(default)]
    call_hooks: Option<bool>,
    #[serde(default)]
    globals: BTreeMap<String, Global>,
    #[serde(default)]
    curated: Curated,
    #[serde(default)]
    aux_modules: Vec<Module>,
    #[serde(default)]
    import_pops: BTreeMap<String, i32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallPop {
    pub site: Option<Addr>,
    pub function: Option<Addr>,
    pub pop: i32,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Translate {
    pub dispatch: Option<Dispatch>,
    pub fault_state: FaultState,
    pub msvc_x87_convention: bool,
    pub call_contracts: bool,
    pub stack_memory: bool,
    pub assume_private_stack: Option<bool>,
    pub lazy_stack_writes: Option<bool>,
    pub retire_reused_stack: bool,
    pub assume_stack_pointer_bases: bool,
    pub exposed_stack_functions: Vec<Addr>,
    pub stack_helpers: Vec<Addr>,
    pub stack_probes: Vec<Addr>,
    pub call_pops: Vec<CallPop>,
    pub x87_cw_clone: bool,
    pub private_leaf_contracts: bool,
    pub private_input_arguments: bool,
    pub private_input_registers: BTreeMap<String, String>,
    pub x87_imports_preserve_pc_rc: bool,
    pub x87_facts: Option<String>,
    pub x87_mode_metadata: Option<String>,
    pub analysis_facts: Option<String>,
    pub indirect_targets: Option<String>,
    pub discovery_evidence: Option<String>,
    pub call_effects: Vec<CallEffect>,
    pub x87_entry_contracts: Vec<de::IgnoredAny>,
    pub allocators: Vec<de::IgnoredAny>,
    pub checked_returns: Vec<Addr>,
    pub state_contract_callers: Vec<Addr>,
    pub alternate_entries: Vec<Addr>,
    pub entry_points: Vec<Addr>,
    pub discover: Option<bool>,
    pub native: Native,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dispatch {
    pub shift: u8,
    pub max_probe: u32,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Native {
    pub header: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultState {
    #[default]
    Relaxed,
    Exact,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct CallEffect {
    pub site: Option<Addr>,
    pub import: Option<String>,
    pub regs: Option<RegsEffect>,
    pub x87: Option<X87Effect>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegsEffect {
    Abi,
    Unknown,
    Contract(Contract),
}

impl RegsEffect {
    pub fn contract(self) -> Contract {
        match self {
            Self::Abi => Contract::ABI,
            Self::Unknown => Contract::CONSERVATIVE,
            Self::Contract(contract) => contract,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawRegsEffect {
    Named(RawRegsName),
    Fields(RawRegsFields),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawRegsName {
    Abi,
    Unknown,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRegsFields {
    reads: Vec<String>,
    kills: Vec<String>,
    writes: Vec<String>,
    preserves: Vec<String>,
}

fn fields(values: Vec<String>, what: &str) -> std::result::Result<Fields, String> {
    let mut out = Fields::NONE;
    let mut seen = BTreeSet::new();
    for name in values {
        let field = Field::from_name(&name)
            .ok_or_else(|| format!("unknown register or flag {name:?} in {what}"))?;
        if !seen.insert(field) {
            return Err(format!("field {name:?} repeats in {what}"));
        }
        out.insert(field);
    }
    Ok(out)
}

impl<'de> Deserialize<'de> for RegsEffect {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match RawRegsEffect::deserialize(deserializer)? {
            RawRegsEffect::Named(RawRegsName::Abi) => Ok(Self::Abi),
            RawRegsEffect::Named(RawRegsName::Unknown) => Ok(Self::Unknown),
            RawRegsEffect::Fields(raw) => {
                let reads = fields(raw.reads, "reads").map_err(de::Error::custom)?;
                let kills = fields(raw.kills, "kills").map_err(de::Error::custom)?;
                let writes = fields(raw.writes, "writes").map_err(de::Error::custom)?;
                let preserves = fields(raw.preserves, "preserves").map_err(de::Error::custom)?;
                if !kills.intersection(preserves).is_empty() {
                    return Err(de::Error::custom("kills and preserves overlap"));
                }
                if !kills.difference(writes).is_empty() {
                    return Err(de::Error::custom("kills must be a subset of writes"));
                }
                if !(writes | preserves).contains_all() {
                    return Err(de::Error::custom(
                        "every register and flag must be covered by writes or preserves",
                    ));
                }
                Ok(Self::Contract(Contract {
                    reads,
                    kills,
                    writes,
                    preserves,
                }))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum X87Effect {
    Named(X87Named),
    Masked(X87Mask),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct X87Mask {
    pub preserved: u16,
    pub known: u16,
    pub value: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum X87Named {
    Preserve,
    Unknown,
}

impl X87Effect {
    pub fn control_word(self) -> (u16, u16, u16) {
        match self {
            X87Effect::Named(X87Named::Preserve) => (0xffff, 0, 0),
            X87Effect::Named(X87Named::Unknown) => (0, 0, 0),
            X87Effect::Masked(X87Mask {
                preserved,
                known,
                value,
            }) => (preserved, known, value),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Global {
    pub addr: Addr,
    pub size: Option<u64>,
    pub stride: Option<u64>,
    pub count: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Curated {
    pub alternates: BTreeMap<String, Addr>,
    pub entries: BTreeMap<String, Addr>,
    pub aliases: BTreeMap<String, Addr>,
    pub events: BTreeMap<String, Addr>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Module {
    pub key: String,
    pub name: String,
    pub base: Addr,
    pub size: u32,
    pub path: PathBuf,
    pub code_map_path: PathBuf,
    pub sha256: String,
    #[serde(default)]
    pub entry_points: Vec<Addr>,
}

#[derive(Clone, Debug)]
pub struct CallEffects {
    pub sites: BTreeMap<Addr, CallEffect>,
    pub imports: BTreeMap<String, CallEffect>,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub game_dir: PathBuf,
    pub exe: PathBuf,
    pub code_map: PathBuf,
    pub base: Addr,
    pub sha256: String,
    pub module: Option<Module>,
    pub globals: BTreeMap<String, Global>,
    pub curated: Curated,
    pub alternate_entries: BTreeSet<Addr>,
    pub configured_entries: BTreeSet<Addr>,
    pub analysis_facts: Option<PathBuf>,
    pub indirect_targets: Option<PathBuf>,
    pub discovery_evidence: Option<PathBuf>,
    pub native_header: Option<PathBuf>,
    pub call_effects: CallEffects,
    pub import_pops: BTreeMap<String, i32>,
    pub call_contracts: bool,
    pub stack_memory: bool,
    pub assume_private_stack: bool,
    pub lazy_stack_writes: bool,
    pub retire_reused_stack: bool,
    pub assume_stack_pointer_bases: bool,
    pub exposed_stack_functions: BTreeSet<Addr>,
    pub stack_helpers: BTreeSet<Addr>,
    pub stack_probes: BTreeSet<Addr>,
    pub call_pops: BTreeMap<Addr, i32>,
    pub function_pops: BTreeMap<Addr, i32>,
    pub call_hooks: bool,
    pub private_leaf_contracts: bool,
    pub private_input_arguments: bool,
    pub private_input_registers: BTreeMap<Addr, String>,
    pub checked_returns: BTreeSet<Addr>,
    pub state_contract_callers: BTreeSet<Addr>,
    pub discover: bool,
    pub dispatch: Option<Dispatch>,
    pub fault_state: FaultState,
    pub msvc_x87_convention: bool,
    pub x87_cw_clone: bool,
    pub reloc: Option<PathBuf>,
}

impl Settings {
    pub fn load(path: &Path, module: Option<&str>) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let input: Input = serde_json::from_str(&text)
            .map_err(|e| Error::input(format!("{}: {e}", path.display())))?;
        if input.format != INPUT_FORMAT {
            return Err(Error::input(format!(
                "{}: expected {INPUT_FORMAT}",
                path.display()
            )));
        }
        let t = &input.translate;
        let unsupported = [
            ("x87_facts", module.is_none() && t.x87_facts.is_some()),
            (
                "x87_mode_metadata",
                module.is_none() && t.x87_mode_metadata.is_some(),
            ),
            ("x87_entry_contracts", !t.x87_entry_contracts.is_empty()),
            ("x87_imports_preserve_pc_rc", t.x87_imports_preserve_pc_rc),
            ("allocators", !t.allocators.is_empty()),
        ];
        if let Some((name, _)) = unsupported.iter().find(|(_, set)| *set) {
            return Err(Error::input(format!(
                "{}: the Rust translator does not support translate.{name}",
                path.display()
            )));
        }
        Self::resolve(input, module)
    }

    fn resolve(input: Input, module: Option<&str>) -> Result<Self> {
        let t = input.translate;
        let code_map = input
            .code_map
            .ok_or_else(|| Error::input("game.toml [translate] code_map is required"))?;
        let module = match module {
            None => None,
            Some(key) => Some(
                input
                    .aux_modules
                    .iter()
                    .find(|m| m.key == key)
                    .cloned()
                    .ok_or_else(|| Error::input(format!("game.toml has no [modules.aux.{key}]")))?,
            ),
        };
        let main = module.is_none();
        let resolve = |name: &Option<String>| {
            name.as_ref()
                .filter(|_| main)
                .map(|name| input.dir.join(name))
        };
        if main
            && t.analysis_facts.is_some()
            && (t.indirect_targets.is_some() || !t.call_effects.is_empty())
        {
            return Err(Error::input(
                "translate.analysis_facts cannot be combined with translate.indirect_targets or translate.call_effects",
            ));
        }
        let mut sites = BTreeMap::new();
        let mut imports = BTreeMap::new();
        for effect in &t.call_effects {
            if let Some(X87Effect::Masked(X87Mask {
                preserved,
                known,
                value,
            })) = effect.x87
            {
                if value & !known != 0 || known & preserved != 0 {
                    return Err(Error::input(
                        "call_effects x87 masks require value within known and known disjoint from preserved",
                    ));
                }
            }
            match (effect.site, &effect.import) {
                (Some(site), None) => {
                    if main {
                        sites.insert(site, effect.clone());
                    }
                }
                (None, Some(import)) => {
                    imports.insert(import.clone(), effect.clone());
                }
                _ => {
                    return Err(Error::input(
                        "call_effects entry must set exactly one of import or site",
                    ))
                }
            }
        }
        let mut private_input_registers = BTreeMap::new();
        if main {
            for (addr, register) in &t.private_input_registers {
                let addr = parse_hex(addr).ok_or_else(|| {
                    Error::input(format!(
                        "private_input_registers key {addr} is not an address"
                    ))
                })?;
                private_input_registers.insert(addr, register.clone());
            }
        }
        let alternate_entries: BTreeSet<Addr> = t.alternate_entries.iter().copied().collect();
        let configured_entries = match &module {
            Some(m) => m.entry_points.iter().copied().collect(),
            None => alternate_entries
                .iter()
                .chain(&t.entry_points)
                .copied()
                .collect(),
        };
        let (exe, code_map, base, sha256) = match &module {
            Some(m) => (
                m.path.clone(),
                m.code_map_path.clone(),
                m.base,
                m.sha256.clone(),
            ),
            None => (input.exe, code_map, input.image_base, input.sha256),
        };
        let only_main = |set: &[Addr]| -> BTreeSet<Addr> {
            if main {
                set.iter().copied().collect()
            } else {
                BTreeSet::new()
            }
        };
        Ok(Settings {
            exe,
            code_map,
            base,
            sha256,
            globals: if main { input.globals } else { BTreeMap::new() },
            curated: if main {
                input.curated
            } else {
                Curated::default()
            },
            alternate_entries: if main {
                alternate_entries
            } else {
                BTreeSet::new()
            },
            configured_entries,
            analysis_facts: resolve(&t.analysis_facts),
            indirect_targets: resolve(&t.indirect_targets),
            discovery_evidence: resolve(&t.discovery_evidence),
            native_header: resolve(&t.native.header),
            call_effects: CallEffects { sites, imports },
            import_pops: input.import_pops,
            dispatch: t.dispatch.clone(),
            call_contracts: t.call_contracts,
            stack_memory: t.stack_memory,
            assume_private_stack: t.assume_private_stack.unwrap_or(true),
            lazy_stack_writes: t.lazy_stack_writes.unwrap_or(true)
                && !input.store_hooks.unwrap_or(true),
            retire_reused_stack: t.retire_reused_stack && !input.store_hooks.unwrap_or(true),
            assume_stack_pointer_bases: t.assume_stack_pointer_bases,
            exposed_stack_functions: only_main(&t.exposed_stack_functions),
            stack_helpers: only_main(&t.stack_helpers),
            stack_probes: only_main(&t.stack_probes),
            call_pops: if main {
                t.call_pops
                    .iter()
                    .filter_map(|row| Some((row.site?, row.pop)))
                    .collect()
            } else {
                BTreeMap::new()
            },
            function_pops: if main {
                t.call_pops
                    .iter()
                    .filter_map(|row| Some((row.function?, row.pop)))
                    .collect()
            } else {
                BTreeMap::new()
            },
            call_hooks: input.call_hooks.unwrap_or(true),
            private_leaf_contracts: t.private_leaf_contracts && main,
            private_input_arguments: t.private_input_arguments && main,
            private_input_registers,
            checked_returns: only_main(&t.checked_returns),
            state_contract_callers: only_main(&t.state_contract_callers),
            discover: t.discover.unwrap_or(true),
            fault_state: t.fault_state,
            msvc_x87_convention: t.msvc_x87_convention,
            x87_cw_clone: t.x87_cw_clone,
            reloc: if main { input.reloc } else { None },
            game_dir: input.dir,
            module,
        })
    }
}

impl Settings {
    pub fn native_replaced(&self) -> Result<BTreeSet<Addr>> {
        let mut named = BTreeSet::new();
        let Some(header) = &self.native_header else {
            return Ok(named);
        };
        let mut seen = BTreeSet::new();
        let mut stack = vec![header.clone()];
        while let Some(path) = stack.pop() {
            if !seen.insert(path.clone()) {
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) if path == *header => return Err(Error::io(&path, e)),
                Err(_) => continue,
            };
            for (i, _) in text.match_indices("FN_") {
                if let Some(addr) = text.get(i + 3..i + 11).and_then(|hex| {
                    hex.bytes()
                        .all(|b| b.is_ascii_hexdigit())
                        .then(|| Addr::from_str_radix(hex, 16).ok())
                        .flatten()
                }) {
                    named.insert(addr);
                }
            }
            let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
            for line in text.lines() {
                let line = line.trim_start();
                let Some(rest) = line.strip_prefix('#') else {
                    continue;
                };
                let Some(rest) = rest.trim_start().strip_prefix("include") else {
                    continue;
                };
                if let Some(name) = rest
                    .trim()
                    .strip_prefix('"')
                    .and_then(|r| r.split('"').next())
                {
                    stack.push(dir.join(name));
                }
            }
        }
        Ok(named)
    }
}
