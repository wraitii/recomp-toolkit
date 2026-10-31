//! Loads and cross-checks the translation inputs: the PE32 image (`pe`, rebased through its HIGHLOW
//! relocations when needed) and the Ghidra `recomp-code-map-v3` directory (`code_map`), whose
//! executable hash and image base must match the settings. `rebuild_index` classifies every entry
//! as function, table target, interior, thunk target, relocated pointer or configured entry;
//! `details` derives unrelocated PE metadata from the same parse.

pub mod code_map;
pub mod details;
pub mod pe;
pub mod reloc;

use std::collections::BTreeMap;

use rk_core::program::{EntryKind, Function, Image, Program};
use rk_core::settings::Settings;
use rk_core::{Addr, Diagnostic, Error, Reason, Result};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct Section {
    pub virtual_address: u32,
    pub virtual_size: u32,
    pub raw_size: u32,
    pub raw_pointer: u32,
    pub characteristics: u32,
}

#[derive(Clone, Debug)]
pub struct PeDetails {
    pub preferred_base: Addr,
    pub entry_point: Addr,
    pub size: u32,
    pub delta: u32,
    pub original: Vec<u8>,
    pub sections: Vec<Section>,
    pub data_ranges: Vec<(Addr, Addr, &'static str)>,
    pub exports: Vec<Addr>,
    pub tls_callbacks: Option<Addr>,
}

pub struct LoadedProgram {
    pub program: Program,
    pub pe: PeDetails,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn load(settings: &Settings) -> Result<LoadedProgram> {
    let exe = std::fs::read(&settings.exe).map_err(|e| Error::io(&settings.exe, e))?;
    let actual = sha256_hex(&exe);
    let invalid = |message: &str| Error::input(format!("{}: {message}", settings.exe.display()));
    let headers = pe::Headers::parse(&exe).ok_or_else(|| invalid("malformed PE image"))?;
    rk_core::isa::install(headers.machine().map_err(|m| invalid(&m))?)?;
    let pe = details::parse_headers(&headers, settings.base);
    let image = pe::parse_headers(headers, Some(settings.base))
        .ok_or_else(|| invalid("malformed PE image"))?
        .map_err(|message| invalid(&message))?;
    let map = code_map::read_map(&settings.code_map, image.target)?;
    if actual != settings.sha256 || map.metadata.get("executable_sha256") != Some(&actual) {
        return Err(Error::input(
            "code-map, game.toml and executable SHA-256 do not match",
        ));
    }
    if image.base != settings.base
        || map.metadata.get("image_base") != Some(&image.target.hex(image.base))
    {
        return Err(Error::input(
            "code-map, game.toml and executable image bases do not match",
        ));
    }
    let pe = pe
        .ok_or_else(|| invalid("malformed PE image"))?
        .map_err(|message| invalid(&message))?;
    let provisional =
        map.metadata.get("provenance").map(String::as_str) == Some("provisional-sweep");
    let functions: BTreeMap<Addr, Function> = map
        .functions
        .into_iter()
        .map(|(addr, spans)| {
            let function = Function {
                addr,
                spans,
                noreturn: map.noreturn_functions.contains(&addr),
                provisional,
            };
            (addr, function)
        })
        .collect();
    let mut program = Program::new(image);
    program.functions = functions;
    program.tables = map.tables;
    program.interior = map.interior;
    program.noreturn_calls = map.noreturn_calls;
    program.configured_entries = settings.configured_entries.clone();
    rebuild_index(&mut program)?;
    Ok(LoadedProgram { program, pe })
}

pub fn rebuild_index(program: &mut Program) -> Result<()> {
    program.spans = program
        .functions
        .values()
        .flat_map(|f| f.spans.iter().map(move |s| (s.start, s.end(), f.addr)))
        .collect();
    program.spans.sort_unstable();
    index_entries(program)
}

fn index_entries(program: &mut Program) -> Result<()> {
    let mut found: BTreeMap<Addr, EntryKind> = program
        .functions
        .keys()
        .map(|&a| (a, EntryKind::Function))
        .collect();
    for targets in program.tables.values() {
        for &target in targets {
            found.entry(target).or_insert(EntryKind::Table);
        }
    }
    for (&addr, &(_, kind)) in &program.interior {
        found.entry(addr).or_insert(kind.into());
    }
    for &addr in program.functions.keys() {
        if let Some(target) = entry_thunk_target(&program.image, addr) {
            if target != addr && program.containing(target).is_some() {
                found.entry(target).or_insert(EntryKind::Thunk);
            }
        }
    }
    for &target in program.image.relocated_data_pointers().keys() {
        if program.containing(target).is_some() {
            found.entry(target).or_insert(EntryKind::Reloc);
        }
    }
    for &addr in &program.configured_entries {
        found.entry(addr).or_insert(EntryKind::Config);
    }
    let mut interior: BTreeMap<Addr, Vec<Addr>> = BTreeMap::new();
    for &addr in found.keys() {
        if program.functions.contains_key(&addr) {
            continue;
        }
        let owner = program
            .containing(addr)
            .ok_or_else(|| Diagnostic::error(addr, Reason::EntryOutsideFunctions))?;
        interior.entry(owner).or_default().push(addr);
    }
    program.entries = found;
    program.interior_by_function = interior;
    Ok(())
}

pub fn entry_thunk_target(image: &Image, addr: Addr) -> Option<Addr> {
    let b = image.bytes(addr, 16)?;
    let size = if b[0] == 0x83 && matches!(b[1], 0xe9 | 0xc1) && b[3] == 0xe9 {
        8
    } else if b[0] == 0x81 && matches!(b[1], 0xe9 | 0xc1) && b[6] == 0xe9 {
        11
    } else if matches!(b[0], 0xb8 | 0xb9) && b[5] == 0xe9 {
        10
    } else if b[0] == 0xc7 && b[1] == 0x44 && b[2] == 0x24 && b[8] == 0xe9 {
        13
    } else if b[0] == 0xe9 {
        5
    } else {
        return None;
    };
    let rel = i32::from_le_bytes(b[size - 4..size].try_into().ok()?);
    Some(
        image.target.wrap(
            addr.wrapping_add(size as u64)
                .wrapping_add(rel as i64 as u64),
        ),
    )
}
