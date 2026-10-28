//! Relocatable-code manifest types and the synthetic image each body is translated from.

use std::collections::BTreeMap;

use rk_core::program::{Image, PeDetails, Program, Section};
use rk_core::{Addr, Error, Result};
use serde::Deserialize;

pub const FORMAT: &str = "recomp-reloc-manifest-v1";
pub const SYNTHETIC_BASE: Addr = 0x3000_0000;
pub const SYNTHETIC_STRIDE: u32 = 0x1_0000;

#[derive(Clone, Debug, Deserialize)]
pub struct Source {
    pub path: String,
    pub body_offset: u32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Import {
    pub slot: u32,
    pub target: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Template {
    pub id: String,
    pub code: String,
    pub masked: Vec<u32>,
    pub imports: Vec<Import>,
    pub entries: Vec<u32>,
    pub sources: Vec<Source>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub source: serde_json::Value,
    pub templates: Vec<Template>,
}

pub fn load(path: &std::path::Path) -> Result<Manifest> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    let manifest: Manifest = serde_json::from_str(&text)
        .map_err(|e| Error::input(format!("{}: {e}", path.display())))?;
    if manifest.format != FORMAT {
        return Err(Error::input(format!(
            "{}: expected {FORMAT}",
            path.display()
        )));
    }
    Ok(manifest)
}

pub fn code_bytes(template: &Template) -> Result<Vec<u8>> {
    hex_bytes(&template.code)
}

fn hex_bytes(text: &str) -> Result<Vec<u8>> {
    if text.len() % 2 != 0 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::input("reloc template code is not hexadecimal"));
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&text[index..index + 2], 16).map_err(|e| Error::input(e.to_string()))
        })
        .collect()
}

fn hex_addr(text: &str) -> Result<Addr> {
    Addr::from_str_radix(text.trim_start_matches("0x"), 16)
        .map_err(|_| Error::input(format!("{text:?} is not an address")))
}

pub fn synthetic_base(index: usize) -> Addr {
    SYNTHETIC_BASE + index as u32 * SYNTHETIC_STRIDE
}

pub fn program(template: &Template, index: usize) -> Result<Program> {
    let base = synthetic_base(index);
    let mut data = code_bytes(template)?;
    if data.is_empty() || data.len() > SYNTHETIC_STRIDE as usize {
        return Err(Error::input(format!(
            "{}: body length {} exceeds the synthetic stride",
            template.id,
            data.len()
        )));
    }
    for import in &template.imports {
        let slot = import.slot as usize;
        if slot + 4 > data.len() {
            return Err(Error::input(format!(
                "{}: import slot {slot:#x} is outside the body",
                template.id
            )));
        }
        let target = hex_addr(&import.target)?;
        let relative = target.wrapping_sub(base + import.slot).wrapping_sub(4);
        data[slot..slot + 4].copy_from_slice(&relative.to_le_bytes());
    }
    let size = data.len() as u32;
    let image = Image {
        base,
        size,
        data: data.clone(),
        exec_ranges: vec![(base, base + size)],
        imports: BTreeMap::new(),
        relocations: Vec::new(),
    };
    let pe = PeDetails {
        preferred_base: base,
        entry_point: base,
        size,
        delta: 0,
        original: data,
        sections: vec![Section {
            virtual_address: 0,
            virtual_size: size,
            raw_size: size,
            raw_pointer: 0,
            characteristics: 0x6000_0020,
        }],
        data_ranges: Vec::new(),
        exports: Vec::new(),
        tls_callbacks: None,
    };
    let configured_entries = template
        .entries
        .iter()
        .map(|&offset| base + offset)
        .collect();
    let mut metadata = BTreeMap::new();
    metadata.insert("reloc_template".to_string(), template.id.clone());
    Ok(Program {
        image,
        sha256: template.id.clone(),
        pe,
        metadata,
        functions: BTreeMap::new(),
        tables: BTreeMap::new(),
        interior: BTreeMap::new(),
        noreturn_calls: Default::default(),
        configured_entries,
        entries: BTreeMap::new(),
        interior_by_function: BTreeMap::new(),
        region_entries: BTreeMap::new(),
        spans: Vec::new(),
    })
}
