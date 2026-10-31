use std::collections::{BTreeMap, BTreeSet};

use rk_core::flow::{Flow, Target};
use rk_core::ir::{OpCode, PcodeOp, Space, Varnode};
use rk_core::program::Program;
use rk_core::Addr;

use crate::cfg::RawFunction;

const MAX_JUMP_TABLE: usize = 4096;

pub(crate) fn recover_tables(
    program: &Program,
    relocated: &BTreeMap<Addr, Addr>,
    raw: &RawFunction,
) -> BTreeMap<Addr, Vec<Addr>> {
    let mut tables = BTreeMap::new();
    for (i, ins) in raw.insns.iter().enumerate() {
        let Flow::Jump(Target::Indirect(target)) = raw.flows[i] else {
            continue;
        };
        if program.tables.contains_key(&(raw.addr, ins.addr)) {
            continue;
        }
        let Some(table) = indexed_load(&ins.ops, target) else {
            continue;
        };
        if table == 0 || program.image.bytes(table, 4).is_none() {
            continue;
        }
        let relocated_run = relocated.contains_key(&table);
        let mut seen = BTreeSet::new();
        let mut targets = Vec::new();
        for slot in 0..MAX_JUMP_TABLE {
            let at = table.wrapping_add(slot as u64 * 4);
            let Some(target) = program.image.rd32(at) else {
                break;
            };
            if relocated_run && relocated.get(&at) != Some(&target) {
                break;
            }
            if !raw.index.contains_key(&target)
                && !program.functions.contains_key(&target)
                && !program.entries.contains_key(&target)
            {
                break;
            }
            if seen.insert(target) {
                targets.push(target);
            }
        }
        if !targets.is_empty() {
            tables.insert(ins.addr, targets);
        }
    }
    tables
}

pub(crate) fn table_jumps(raw: &RawFunction) -> BTreeSet<u32> {
    (0..raw.flows.len())
        .filter(|&i| {
            matches!(raw.flows[i], Flow::Jump(Target::Indirect(_)))
                && !raw.return_jumps.contains(&(i as u32))
        })
        .map(|i| i as u32)
        .collect()
}

fn indexed_load(ops: &[PcodeOp], target: Varnode) -> Option<Addr> {
    let defining = |v: Varnode| ops.iter().rev().find(|op| op.output == Some(v));
    let pointer = u32::from(rk_core::isa::target().address_bytes);
    let load = defining(target).filter(|op| op.opcode == OpCode::Load && target.size == pointer)?;
    let sum = defining(*load.inputs.first()?).filter(|op| op.opcode == OpCode::IntAdd)?;
    let [a, b] = sum.inputs.as_slice() else {
        return None;
    };
    let (base, scaled) = match (a.space, b.space) {
        (Space::Const, _) => (a, b),
        (_, Space::Const) => (b, a),
        _ => return None,
    };
    let scale = defining(*scaled).filter(|op| op.opcode == OpCode::IntMult)?;
    let [index, factor] = scale.inputs.as_slice() else {
        return None;
    };
    (index.space == Space::Register
        && factor.space == Space::Const
        && factor.offset == u64::from(pointer))
    .then_some(base.offset)
}
