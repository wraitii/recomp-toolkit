use std::collections::BTreeSet;

use rk_core::contract::{Field, Fields};
use rk_core::ir::OpCode;
use rk_ir::{field_lane, Op, Registers, StateKey};
use rk_ssa::ValueId;

use crate::PrivateEmit;

pub(crate) fn const_expr(data: u64, size: u8) -> String {
    if size > 4 {
        format!("0x{data:x}ull")
    } else {
        format!("0x{:x}u", data & ((1u64 << (size as u32 * 8)) - 1))
    }
}

pub(crate) fn keep_expr(loaded: &str, old: &str) -> String {
    if loaded == old {
        old.to_string()
    } else {
        format!("(RECOMP_UNLIKELY(recomp_hooks_ever) ? ({loaded}) : ({old}))")
    }
}

pub(crate) fn reads_c_field(
    s: &rk_ssa::Ssa,
    value: ValueId,
    materialized: &[bool],
    private: Option<&PrivateEmit>,
) -> bool {
    if materialized[value as usize] {
        return false;
    }
    match s.value(value).op {
        Op::Input(StateKey::Memory)
        | Op::StateReload(StateKey::Memory)
        | Op::StateKeep(StateKey::Memory) => false,
        Op::Input(key) | Op::StateReload(key) | Op::StateKeep(key) => private_loads(private, key),
        Op::Byte(_) => s
            .args(value)
            .first()
            .is_some_and(|arg| reads_c_field(s, *arg, materialized, private)),
        Op::Pack => s
            .args(value)
            .iter()
            .any(|arg| reads_c_field(s, *arg, materialized, private)),
        Op::P(OpCode::Copy | OpCode::IntZExt) => s
            .args(value)
            .first()
            .is_some_and(|arg| reads_c_field(s, *arg, materialized, private)),
        _ => false,
    }
}

pub(crate) fn identity_assignment(line: &str) -> bool {
    let text = line.trim().trim_end_matches(';');
    text.split_once('=')
        .is_some_and(|(target, value)| target.trim() == value.trim())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Location {
    Frame(StateKey),
    Gpr(usize),
    Flag(Field),
    Xmm(usize, usize),
    Eip,
    Fs,
    Df,
}

pub(crate) fn x87_reads_flags(line: &str) -> bool {
    [
        "c->eflags_cf",
        "c->eflags_pf",
        "c->eflags_af",
        "c->eflags_zf",
        "c->eflags_sf",
        "c->eflags_of",
    ]
    .iter()
    .any(|field| line.contains(field))
}

pub(crate) fn cc_mask(fields: Fields) -> u8 {
    let mut mask = 0u8;
    for (field, bit) in [
        (Field::Cf, 0),
        (Field::Pf, 1),
        (Field::Af, 2),
        (Field::Zf, 3),
        (Field::Sf, 4),
        (Field::Of, 5),
    ] {
        if fields.contains(field) {
            mask |= 1u8 << bit;
        }
    }
    mask
}

pub(crate) fn location(key: StateKey) -> Option<Location> {
    if let Some((field, _)) = field_lane(key) {
        if (field as usize) < 8 {
            return Some(Location::Gpr(field as usize));
        }
        return Some(Location::Flag(field));
    }
    if let Some((reg, offset)) = rk_ir::xmm_offset(key) {
        return Some(Location::Xmm(reg, (offset / 4) as usize));
    }
    let offset = match key {
        StateKey::Register(offset) => offset,
        StateKey::Frame { .. } => return Some(Location::Frame(key)),
        StateKey::Memory => return None,
    };
    let (name, _) = Registers::get().covering(offset as u64)?;
    match name {
        "EIP" | "IP" => Some(Location::Eip),
        "FS_OFFSET" => Some(Location::Fs),
        "DF" => Some(Location::Df),
        _ => None,
    }
}

pub(crate) fn private_loads(private: Option<&PrivateEmit>, key: StateKey) -> bool {
    let Some(private) = private else { return true };
    match field_lane(key) {
        Some((field, _)) => private.inputs.contains(field) || private.input_arguments.registers.contains(field),
        None => true,
    }
}

pub(crate) fn select_mask(masks: &BTreeSet<Fields>, wanted: Fields) -> Option<Fields> {
    masks
        .iter()
        .copied()
        .filter(|mask| wanted & !*mask == Fields::NONE)
        .min_by_key(|mask| (mask.0.count_ones(), *mask))
}

pub(crate) fn ends_block(op: Op) -> bool {
    matches!(
        op,
        Op::Branch { .. }
            | Op::CBranch { .. }
            | Op::BranchInd
            | Op::Return
            | Op::Tail { .. }
            | Op::ImportTail { .. }
            | Op::Trap(_)
            | Op::Breakpoint
            | Op::CallOther(_)
    )
}

pub(crate) fn ctype(size: u8) -> &'static str {
    match size {
        1 => "uint8_t",
        2 => "uint16_t",
        4 => "uint32_t",
        _ => "uint64_t",
    }
}

pub(crate) fn c_mask(size: u8) -> String {
    if size >= 8 {
        "0xffffffffffffffffull".to_string()
    } else {
        format!("0x{:x}u", rk_core::mask(size as u32))
    }
}

pub(crate) fn signed_type(size: u8) -> &'static str {
    match size {
        1 => "int8_t",
        2 => "int16_t",
        4 => "int32_t",
        _ => "int64_t",
    }
}

pub(crate) fn cc_kind_name(kind: rk_ir::CcKind) -> &'static str {
    use rk_ir::CcKind;
    match kind {
        CcKind::Add => "X86_CC_ADD",
        CcKind::Sub => "X86_CC_SUB",
        CcKind::Logic => "X86_CC_LOGIC",
        CcKind::Inc => "X86_CC_INC",
        CcKind::Dec => "X86_CC_DEC",
        CcKind::Adc => "X86_CC_ADC",
        CcKind::Sbb => "X86_CC_SBB",
    }
}
