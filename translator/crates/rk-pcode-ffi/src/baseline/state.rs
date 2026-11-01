use std::sync::OnceLock;

use rk_core::ir::{Space, Varnode};
use rk_ir::{Registers, EFLAGS_MISC};

use super::abi::CpuLayout;

const SPLIT_FLAGS: usize = 7;
pub const SPLIT_FLAG_BITS: [u32; SPLIT_FLAGS] = [0, 2, 4, 6, 7, 10, 11];
const SPLIT_FLAG_NAMES: [&str; SPLIT_FLAGS] = ["CF", "PF", "AF", "ZF", "SF", "DF", "OF"];
const OTHER_FLAGS: [(&str, u32); 13] = [
    ("F1", 1),
    ("F3", 3),
    ("F5", 5),
    ("TF", 8),
    ("IF", 9),
    ("NT", 14),
    ("F15", 15),
    ("RF", 16),
    ("VM", 17),
    ("AC", 18),
    ("VIF", 19),
    ("VIP", 20),
    ("ID", 21),
];
pub const EFLAGS_RESERVED_ONE: u32 = 1 << 1;
pub const EFLAGS_SPLIT_MASK: u32 = {
    let mut mask = 0;
    let mut i = 0;
    while i < SPLIT_FLAGS {
        mask |= 1 << SPLIT_FLAG_BITS[i];
        i += 1;
    }
    mask
};
pub const FPU_STATUS_CLEARED_BY_FNCLEX: u16 = 0x80ff;
pub const FPU_TAGS_EMPTY: u16 = 0xffff;

const GPR_COUNT: u32 = 8;
const MM_COUNT: u32 = 8;
const MM_BYTES: u32 = 8;
const XMM_COUNT: u32 = 8;
const XMM_BYTES: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Gpr,
    Eip,
    EflagsMisc,
    FsBase,
    FpuControl,
    Mm(u32),
    Xmm(u32),
}

impl Field {
    pub fn offset(self, cpu: &CpuLayout) -> u32 {
        match self {
            Self::Gpr => cpu.gpr,
            Self::Eip => cpu.eip,
            Self::EflagsMisc => cpu.eflags_misc,
            Self::FsBase => cpu.fs_base,
            Self::FpuControl => cpu.fpu_cw,
            Self::Mm(index) => cpu.mm + index * MM_BYTES,
            Self::Xmm(index) => cpu.xmm + index * XMM_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Register {
    Bytes { field: Field, byte: u32 },
    SplitFlag(usize),
    EflagsBit(u32),
    Eflags,
}

struct Map {
    fields: Vec<(Varnode, Field)>,
    split_flags: [u64; SPLIT_FLAGS],
    other_flags: Vec<(u64, u32)>,
    eflags: u64,
}

fn map() -> &'static Map {
    static MAP: OnceLock<Map> = OnceLock::new();
    MAP.get_or_init(|| {
        let registers = Registers::get();
        let arch = registers.arch();
        let mut gpr = registers.require("EAX");
        gpr.size *= GPR_COUNT;
        let mut fields = vec![
            (gpr, Field::Gpr),
            (arch.program_counter, Field::Eip),
            (EFLAGS_MISC, Field::EflagsMisc),
            (registers.require("FS_OFFSET"), Field::FsBase),
            (arch.x86.fpu_control, Field::FpuControl),
        ];
        fields.extend((0..MM_COUNT).map(|i| (registers.require(&format!("MM{i}")), Field::Mm(i))));
        fields
            .extend((0..XMM_COUNT).map(|i| (registers.require(&format!("XMM{i}")), Field::Xmm(i))));
        Map {
            fields,
            split_flags: SPLIT_FLAG_NAMES.map(|name| registers.require(name).offset),
            other_flags: OTHER_FLAGS
                .iter()
                .map(|&(name, bit)| (registers.require(name).offset, bit))
                .collect(),
            eflags: registers.require("eflags").offset,
        }
    })
}

pub fn classify(v: Varnode) -> Option<Register> {
    if v.space != Space::Register || !(1..=8).contains(&v.size) {
        return None;
    }
    let map = map();
    let end = v.offset + u64::from(v.size);
    if let Some((base, field)) = map
        .fields
        .iter()
        .find(|(base, _)| base.offset <= v.offset && end <= base.offset + u64::from(base.size))
    {
        return Some(Register::Bytes {
            field: *field,
            byte: (v.offset - base.offset) as u32,
        });
    }
    if v.size == 1 {
        if let Some(index) = map.split_flags.iter().position(|&o| o == v.offset) {
            return Some(Register::SplitFlag(index));
        }
        if let Some(&(_, bit)) = map.other_flags.iter().find(|&&(o, _)| o == v.offset) {
            return Some(Register::EflagsBit(bit));
        }
    }
    (v.offset == map.eflags && matches!(v.size, 2 | 4)).then_some(Register::Eflags)
}
