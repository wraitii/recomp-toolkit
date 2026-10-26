use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::{Addr, Error, Hex, Result};

#[derive(Clone, Debug)]
pub struct Image {
    pub base: Addr,
    pub size: u32,
    pub data: Vec<u8>,
    pub exec_ranges: Vec<(Addr, Addr)>,
    pub imports: BTreeMap<Addr, Import>,
    pub relocations: Vec<Relocation>,
}

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    pub dll: String,
    pub name: String,
}

impl Import {
    pub fn key(&self) -> String {
        format!("{}!{}", self.dll, self.name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Relocation {
    pub target: Addr,
    pub site: Addr,
}

impl Image {
    pub fn end(&self) -> Addr {
        self.base + self.size
    }

    pub fn is_exec(&self, va: Addr) -> bool {
        self.exec_ranges.iter().any(|&(lo, hi)| lo <= va && va < hi)
    }

    pub fn bytes(&self, va: Addr, len: usize) -> Option<&[u8]> {
        let lo = va.checked_sub(self.base)? as usize;
        self.data.get(lo..lo.checked_add(len)?)
    }

    pub fn bytes_at(&self, va: Addr, len: usize) -> Result<&[u8]> {
        self.bytes(va, len)
            .ok_or_else(|| Error::input(format!("{}: outside the image", Hex(va))))
    }

    pub fn rd8(&self, va: Addr) -> Option<u8> {
        self.bytes(va, 1).map(|b| b[0])
    }

    pub fn rd32(&self, va: Addr) -> Option<u32> {
        self.bytes(va, 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn relocated_pointers(&self) -> BTreeMap<Addr, Addr> {
        let mut out = BTreeMap::new();
        for r in &self.relocations {
            out.entry(r.target).or_insert(r.site);
        }
        out
    }

    pub fn relocated_data_pointers(&self) -> BTreeMap<Addr, Addr> {
        let mut out = BTreeMap::new();
        for r in self.relocations.iter().filter(|r| !self.is_exec(r.site)) {
            out.entry(r.target).or_insert(r.site);
        }
        out
    }
}

#[derive(Clone, Debug)]
pub struct Span {
    pub start: Addr,
    pub size: u32,
    pub lengths: Option<Vec<u8>>,
}

impl Span {
    pub fn end(&self) -> Addr {
        self.start + self.size
    }
}

#[derive(Clone, Debug)]
pub struct Function {
    pub addr: Addr,
    pub spans: Vec<Span>,
    pub noreturn: bool,
    pub provisional: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InteriorKind {
    Branch,
    Data,
    Table,
    Discovered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Function,
    Table,
    Branch,
    Data,
    Discovered,
    Thunk,
    Reloc,
    Config,
}

impl EntryKind {
    pub fn name(self) -> &'static str {
        match self {
            EntryKind::Function => "function",
            EntryKind::Table => "table",
            EntryKind::Branch => "branch",
            EntryKind::Data => "data",
            EntryKind::Discovered => "discovered",
            EntryKind::Thunk => "thunk",
            EntryKind::Reloc => "reloc",
            EntryKind::Config => "config",
        }
    }
}

impl From<InteriorKind> for EntryKind {
    fn from(kind: InteriorKind) -> Self {
        match kind {
            InteriorKind::Branch => EntryKind::Branch,
            InteriorKind::Data => EntryKind::Data,
            InteriorKind::Table => EntryKind::Table,
            InteriorKind::Discovered => EntryKind::Discovered,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Program {
    pub image: Image,
    pub sha256: String,
    pub pe: PeDetails,
    pub metadata: BTreeMap<String, String>,
    pub functions: BTreeMap<Addr, Function>,
    pub tables: BTreeMap<(Addr, Addr), Vec<Addr>>,
    pub interior: BTreeMap<Addr, (Addr, InteriorKind)>,
    pub noreturn_calls: BTreeSet<Addr>,
    pub configured_entries: BTreeSet<Addr>,
    pub entries: BTreeMap<Addr, EntryKind>,
    pub interior_by_function: BTreeMap<Addr, Vec<Addr>>,
    pub region_entries: BTreeMap<Addr, Addr>,
    pub spans: Vec<(Addr, Addr, Addr)>,
}

impl Program {
    pub fn import_thunk(&self, va: Addr) -> Option<&Import> {
        if !self.image.is_exec(va) || va + 6 > self.image.end() {
            return None;
        }
        let raw = self.image.bytes(va, 6)?;
        if raw[..2] != [0xff, 0x25] {
            return None;
        }
        let slot = u32::from_le_bytes([raw[2], raw[3], raw[4], raw[5]]);
        self.image.imports.get(&slot)
    }

    pub fn import_thunk_name(&self, va: Addr) -> Option<&str> {
        self.import_thunk(va).map(|import| import.name.as_str())
    }

    pub fn iat_call(&self, insn: &crate::ir::Insn) -> Option<&Import> {
        let raw = self.image.bytes(insn.addr, insn.length as usize)?;
        let at = raw
            .iter()
            .take_while(|byte| matches!(byte, 0x26 | 0x2e | 0x36 | 0x3e))
            .count();
        if at + 6 != raw.len() || raw[at] != 0xff || raw[at + 1] != 0x15 {
            return None;
        }
        let slot = u32::from_le_bytes([raw[at + 2], raw[at + 3], raw[at + 4], raw[at + 5]]);
        self.image.imports.get(&slot)
    }

    pub fn is_setjmp_thunk(&self, va: Addr) -> bool {
        self.import_thunk_name(va)
            .is_some_and(|name| name.trim_start_matches('_') == "setjmp3")
    }

    pub fn containing(&self, addr: Addr) -> Option<Addr> {
        let k = self.spans.partition_point(|&(start, _, _)| start <= addr);
        let &(_, end, owner) = self.spans.get(k.checked_sub(1)?)?;
        (addr < end).then_some(owner)
    }

    pub fn is_noreturn(&self, addr: Addr) -> bool {
        self.functions.get(&addr).is_some_and(|f| f.noreturn)
    }

    pub fn entry_owner(&self, addr: Addr) -> Option<Addr> {
        if self.functions.contains_key(&addr) {
            Some(addr)
        } else {
            self.containing(addr)
        }
    }
}
