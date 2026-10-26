use std::collections::{BTreeMap, BTreeSet};

use rk_core::ir::{FunctionIr, OpCode, Space};
use rk_core::settings::Settings;
use rk_core::Addr;
use rk_global::stack::ReturnEffect;
use rk_global::GlobalFacts;
use rk_ssa::CallFacts;

fn reaches_reuse(
    fir: &FunctionIr,
    index: u32,
    active: &mut BTreeSet<u32>,
    budget: &mut usize,
) -> bool {
    if *budget == 0 || !active.insert(index) {
        return false;
    }
    *budget -= 1;
    let insn = &fir.insns[index as usize];
    let reads = insn.ops.iter().any(|op| {
        op.opcode == OpCode::Load
            || !op.opcode.is_branch() && op.inputs.iter().any(|input| input.space == Space::Ram)
    });
    let reuse = matches!(insn.mnemonic.as_str(), "PUSH" | "CALL") && !reads;
    let result = reuse
        || !reads
            && !insn.ops.iter().any(|op| {
                matches!(
                    op.opcode,
                    OpCode::Store | OpCode::CallOther | OpCode::Return | OpCode::CallInd
                ) || op.output.is_some_and(|output| {
                    let esp = rk_ir::Registers::get().require("ESP");
                    output.space == esp.space
                        && output.offset < esp.offset + u64::from(esp.size)
                        && esp.offset < output.offset + u64::from(output.size)
                })
            })
            && !fir.exits.contains_key(&index)
            && !fir.dynamic.contains(&index)
            && !fir.succ[index as usize].is_empty()
            && fir.succ[index as usize]
                .iter()
                .all(|&next| reaches_reuse(fir, next, active, budget));
    active.remove(&index);
    result
}

pub fn calls(
    irs: &BTreeMap<Addr, rk_core::Result<FunctionIr>>,
    global: &GlobalFacts,
    settings: &Settings,
) -> BTreeMap<Addr, (Addr, u32)> {
    let facts = global.call_facts();
    let targets: BTreeMap<_, _> = irs
        .iter()
        .filter_map(|(&addr, result)| {
            let fir = result.as_ref().ok()?;
            let summary = global.returns.functions.get(&addr)?;
            let checkpoints = facts.summary(0, Some(addr)).checkpoints?;
            (summary.returns == ReturnEffect::Exact
                && summary.effect.cleanup >= 0
                && summary
                    .effect
                    .writes
                    .iter()
                    .any(|&(offset, size)| offset < 0 && size != 0)
                && fir.entries.is_empty()
                && fir.exits.is_empty()
                && fir.dynamic.is_empty()
                && fir.seh.is_empty()
                && fir.seh_handlers.is_empty()
                && fir.seh_unwind_only.is_empty()
                && fir.import_tails.is_empty()
                && !settings.exposed_stack_functions.contains(&addr))
            .then_some((addr, checkpoints))
        })
        .collect();
    let mut calls = BTreeMap::new();
    for result in irs.values() {
        let Ok(fir) = result else { continue };
        for (index, insn) in fir.insns.iter().enumerate() {
            if insn.mnemonic != "CALL" || fir.noreturn.contains_key(&(index as u32)) {
                continue;
            }
            let Some(target) = insn
                .ops
                .iter()
                .find(|op| op.opcode == OpCode::Call)
                .and_then(|op| op.inputs.first())
                .map(|input| input.offset as Addr)
                .filter(|target| targets.contains_key(target))
            else {
                continue;
            };
            let mut budget = 64;
            if !fir.succ[index].is_empty()
                && fir.succ[index]
                    .iter()
                    .all(|&next| reaches_reuse(fir, next, &mut BTreeSet::new(), &mut budget))
            {
                calls.insert(insn.addr, (target, targets[&target]));
            }
        }
    }
    calls
}
