use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;
use rk_core::program::{EntryKind, Program};
use rk_core::settings::Settings;
use rk_core::{Addr, Error, Hex, Result};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct SymbolRow {
    #[serde(serialize_with = "hex")]
    pub addr: Addr,
    pub index: usize,
    pub name: String,
    pub kind: &'static str,
    pub provenance: &'static str,
    pub evidence: Vec<&'static str>,
    pub hookable: bool,
    pub aliases: Vec<String>,
}

fn hex<S: serde::Serializer>(addr: &Addr, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.collect_str(&Hex(*addr))
}

pub fn symbol_rows(
    settings: &Settings,
    program: &Program,
    entry_names: &[Addr],
) -> Result<Vec<SymbolRow>> {
    let curated = &settings.curated;
    let relocated = program.image.relocated_pointers();
    let mut evidence: BTreeMap<Addr, BTreeSet<&'static str>> = BTreeMap::new();
    for &addr in curated.alternates.values() {
        evidence.entry(addr).or_default().insert("curated");
    }
    for &addr in curated.entries.values() {
        evidence.entry(addr).or_default().insert("curated_entry");
    }
    for &addr in &settings.alternate_entries {
        evidence.entry(addr).or_default().insert("curated");
    }
    let mut alt_owner = BTreeMap::new();
    for (&owner, addrs) in &program.interior_by_function {
        for &addr in addrs {
            alt_owner.insert(addr, owner);
            if relocated.contains_key(&addr) {
                evidence.entry(addr).or_default().insert("reloc");
            }
        }
    }
    let empty = BTreeSet::new();
    let mut rows: Vec<SymbolRow> = entry_names
        .iter()
        .enumerate()
        .map(|(index, &addr)| {
            let found = evidence.get(&addr).unwrap_or(&empty);
            let listed = program.functions.contains_key(&addr);
            let provenance = program
                .entries
                .get(&addr)
                .copied()
                .unwrap_or(EntryKind::Function);
            let (kind, hookable) = if listed || found.contains("curated_entry") {
                ("entry", true)
            } else if !alt_owner.contains_key(&addr) || provenance == EntryKind::Table {
                ("block", false)
            } else if found.iter().any(|e| matches!(*e, "reloc" | "curated")) {
                ("alternate", true)
            } else {
                ("continuation", false)
            };
            let name = if listed {
                format!("FUN_{addr:08x}")
            } else {
                let owner = alt_owner.get(&addr).copied().unwrap_or(addr);
                let suffix = if kind == "alternate" { "alt" } else { "blk" };
                format!("FUN_{owner:08x}.{suffix}_{addr:08x}")
            };
            SymbolRow {
                addr,
                index,
                name,
                kind,
                provenance: if listed { "listed" } else { provenance.name() },
                evidence: found.iter().copied().collect(),
                hookable,
                aliases: Vec::new(),
            }
        })
        .collect();
    let positions: BTreeMap<Addr, usize> =
        rows.iter().enumerate().map(|(i, r)| (r.addr, i)).collect();
    let mut aliases: BTreeMap<&String, Addr> = BTreeMap::new();
    for (name, &addr) in curated.entries.iter().chain(&curated.aliases) {
        aliases.insert(name, addr);
    }
    for (alias, addr) in aliases {
        if let Some(&i) = positions.get(&addr) {
            rows[i].aliases.push(alias.clone());
        }
    }
    Ok(rows)
}

#[derive(Serialize)]
struct GlobalRow<'a> {
    name: &'a str,
    #[serde(serialize_with = "hex")]
    addr: Addr,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stride: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<u64>,
}

#[derive(Serialize)]
struct Symbols<'a> {
    exe_sha256: String,
    image_base: String,
    functions: &'a [SymbolRow],
    globals: Vec<GlobalRow<'a>>,
    events: IndexMap<&'a str, String>,
}

pub fn symbols_json(settings: &Settings, program: &Program, rows: &[SymbolRow]) -> Result<String> {
    let by_addr: BTreeMap<Addr, &SymbolRow> = rows.iter().map(|r| (r.addr, r)).collect();
    let mut events = IndexMap::new();
    for (name, &addr) in &settings.curated.events {
        if !by_addr.get(&addr).is_some_and(|r| r.hookable) {
            return Err(Error::input(format!(
                "event {name} ({}) is not a hookable entry symbol",
                Hex(addr)
            )));
        }
        events.insert(name.as_str(), Hex(addr).to_string());
    }
    let symbols = Symbols {
        exe_sha256: settings.sha256.clone(),
        image_base: Hex(program.image.base).to_string(),
        functions: rows,
        globals: settings
            .globals
            .iter()
            .map(|(name, g)| GlobalRow {
                name,
                addr: g.addr,
                size: g.size,
                stride: g.stride,
                count: g.count,
            })
            .collect(),
        events,
    };
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    symbols
        .serialize(&mut serializer)
        .map_err(|e| Error::input(format!("symbols.json: {e}")))?;
    Ok(String::from_utf8(out).unwrap_or_default())
}
