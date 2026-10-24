use std::collections::{BTreeMap, BTreeSet};

use rk_core::program::Program;
use rk_core::Addr;
use rk_decode::{branch_target, mem, OperandKind};

use crate::cfg::RawFunction;

const MAX_JUMP_TABLE: usize = 4096;

pub(crate) fn recover_tables(
    program: &Program,
    relocated: &BTreeMap<Addr, Addr>,
    raw: &RawFunction,
) -> BTreeMap<Addr, Vec<Addr>> {
    let mut tables = BTreeMap::new();
    for ins in &raw.decoded {
        if ins.mnemonic != "JMP"
            || branch_target(ins).is_some()
            || program.tables.contains_key(&(raw.addr, ins.addr))
        {
            continue;
        }
        let Some((None, Some(_), 4, displacement, None)) = ins.operands.first().and_then(mem)
        else {
            continue;
        };
        let Ok(table) = u32::try_from(displacement) else {
            continue;
        };
        if table == 0 || program.image.bytes(table, 4).is_none() {
            continue;
        }
        let relocated_run = relocated.contains_key(&table);
        let mut seen = BTreeSet::new();
        let mut targets = Vec::new();
        for slot in 0..MAX_JUMP_TABLE {
            let at = table.wrapping_add((slot as u32) * 4);
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
    raw.decoded
        .iter()
        .enumerate()
        .filter(|(i, ins)| {
            ins.mnemonic == "JMP"
                && !ins.operands.is_empty()
                && !matches!(ins.operands[0].kind, OperandKind::Imm(_))
                && !raw.return_jumps.contains(&(*i as u32))
        })
        .map(|(i, _)| i as u32)
        .collect()
}
