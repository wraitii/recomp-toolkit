//! Relocatable-code manifest types and the synthetic image each body is translated from.

use std::collections::BTreeMap;

use rk_core::program::{Image, Program};
use rk_core::{wrap_addr, Addr, Error, Result};
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
    u32::from_str_radix(text.trim_start_matches("0x"), 16)
        .map(Addr::from)
        .map_err(|_| Error::input(format!("{text:?} is not an address")))
}

pub fn synthetic_base(index: usize) -> Addr {
    SYNTHETIC_BASE + index as u64 * u64::from(SYNTHETIC_STRIDE)
}

pub fn program(template: &Template, index: usize) -> Result<Program> {
    rk_x86::install()?;
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
        let relative = wrap_addr(
            target
                .wrapping_sub(base + Addr::from(import.slot))
                .wrapping_sub(4),
        );
        data[slot..slot + 4].copy_from_slice(&(relative as u32).to_le_bytes());
    }
    let size = data.len() as u32;
    let image = Image {
        target: rk_core::isa::target(),
        base,
        size,
        data,
        exec_ranges: vec![(base, base + Addr::from(size))],
        imports: BTreeMap::new(),
        relocations: Vec::new(),
    };
    let mut program = Program::new(image);
    program.configured_entries = template
        .entries
        .iter()
        .map(|&offset| base + Addr::from(offset))
        .collect();
    Ok(program)
}
