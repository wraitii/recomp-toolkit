use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rk_core::program::{InteriorKind, Span};
use rk_core::{parse_hex, Addr, Error, Hex, Result};

pub const FORMAT: &str = "recomp-code-map-v3";
const SPANS_HEADER: &str = "function\tstart\tbytes\tlengths";
const TABLES_HEADER: &str = "function\tsite\ttargets";
const ENTRIES_HEADER: &str = "address\towner\tkind";
const NORETURN_HEADER: &str = "address\tkind";

pub struct CodeMap {
    pub metadata: BTreeMap<String, String>,
    pub functions: BTreeMap<Addr, Vec<Span>>,
    pub tables: BTreeMap<(Addr, Addr), Vec<Addr>>,
    pub interior: BTreeMap<Addr, (Addr, InteriorKind)>,
    pub noreturn_functions: BTreeSet<Addr>,
    pub noreturn_calls: BTreeSet<Addr>,
}

fn read(root: &Path, name: &str) -> Result<String> {
    let path = root.join(name);
    std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))
}

fn rows<'a>(text: &'a str, name: &str, header: &str) -> Result<impl Iterator<Item = Vec<&'a str>>> {
    let mut lines = text.lines();
    if lines.next() != Some(header) {
        return Err(Error::input(format!("invalid code-map header in {name}")));
    }
    Ok(lines.map(|line| line.split('\t').collect()))
}

fn addr(text: &str, what: &str) -> Result<Addr> {
    parse_hex(text).ok_or_else(|| Error::input(format!("invalid code-map {what}: {text:?}")))
}

fn columns<'a>(row: &[&'a str], count: usize, name: &str) -> Result<Vec<&'a str>> {
    if row.len() != count {
        return Err(Error::input(format!(
            "invalid code-map row in {name}: {}",
            row.join("\t")
        )));
    }
    Ok(row.to_vec())
}

pub fn read_map(root: &Path) -> Result<CodeMap> {
    let metadata: BTreeMap<String, String> = read(root, "metadata.txt")?
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    if metadata.get("format").map(String::as_str) != Some(FORMAT)
        || metadata.get("status").map(String::as_str) != Some("complete")
    {
        return Err(Error::input("unsupported or incomplete code map"));
    }

    let spans_text = read(root, "spans.tsv")?;
    let mut functions: BTreeMap<Addr, Vec<Span>> = BTreeMap::new();
    let mut ends: BTreeMap<Addr, u64> = BTreeMap::new();
    for row in rows(&spans_text, "spans.tsv", SPANS_HEADER)? {
        let row = columns(&row, 4, "spans.tsv")?;
        let start = addr(row[1], "span start")?;
        let size: u32 = row[2]
            .parse()
            .map_err(|_| Error::input(format!("invalid code-map span size at {}", Hex(start))))?;
        let owner = if row[0].is_empty() {
            start
        } else {
            addr(row[0], "span owner")?
        };
        if size == 0 || start as u64 + size as u64 > 1 << 32 {
            return Err(Error::input(format!(
                "invalid code-map span at {}",
                Hex(start)
            )));
        }
        let lengths = if row[3].is_empty() {
            None
        } else {
            let lengths: Option<Vec<u8>> = row[3]
                .chars()
                .map(|c| c.to_digit(16).filter(|&d| d != 0).map(|d| d as u8))
                .collect();
            match lengths {
                Some(l) if l.iter().map(|&n| n as u32).sum::<u32>() == size => Some(l),
                _ => {
                    return Err(Error::input(format!(
                        "invalid code-map lengths at {}",
                        Hex(start)
                    )))
                }
            }
        };
        if (start as u64) < ends.get(&owner).copied().unwrap_or(0) {
            return Err(Error::input("overlapping or unordered code-map spans"));
        }
        ends.insert(owner, start as u64 + size as u64);
        functions.entry(owner).or_default().push(Span {
            start,
            size,
            lengths,
        });
    }
    let expected: usize = metadata
        .get("functions")
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| Error::input("code-map metadata has no function count"))?;
    if expected != functions.len() {
        return Err(Error::input("code-map function count mismatch"));
    }

    let mut tables = BTreeMap::new();
    let text = read(root, "tables.tsv")?;
    for row in rows(&text, "tables.tsv", TABLES_HEADER)? {
        let row = columns(&row, 3, "tables.tsv")?;
        let targets = row[2]
            .split(',')
            .map(|t| addr(t, "table target"))
            .collect::<Result<Vec<_>>>()?;
        tables.insert(
            (addr(row[0], "table function")?, addr(row[1], "table site")?),
            targets,
        );
    }

    let mut interior = BTreeMap::new();
    let text = read(root, "entries.tsv")?;
    for row in rows(&text, "entries.tsv", ENTRIES_HEADER)? {
        let row = columns(&row, 3, "entries.tsv")?;
        let kind = match row[2] {
            "branch" => InteriorKind::Branch,
            "data" => InteriorKind::Data,
            "table" => InteriorKind::Table,
            other => {
                return Err(Error::input(format!(
                    "invalid code-map entry kind: {other}"
                )))
            }
        };
        interior.insert(addr(row[0], "entry")?, (addr(row[1], "entry owner")?, kind));
    }

    let mut noreturn_functions = BTreeSet::new();
    let mut noreturn_calls = BTreeSet::new();
    let text = read(root, "noreturn.tsv")?;
    for row in rows(&text, "noreturn.tsv", NORETURN_HEADER)? {
        let row = columns(&row, 2, "noreturn.tsv")?;
        let at = addr(row[0], "noreturn address")?;
        match row[1] {
            "function" => noreturn_functions.insert(at),
            "call" => noreturn_calls.insert(at),
            other => {
                return Err(Error::input(format!(
                    "invalid code-map noreturn kind: {other}"
                )))
            }
        };
    }

    Ok(CodeMap {
        metadata,
        functions,
        tables,
        interior,
        noreturn_functions,
        noreturn_calls,
    })
}
