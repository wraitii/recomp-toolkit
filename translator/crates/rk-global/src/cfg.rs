use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use rk_core::flow::Flow;
use rk_core::ir::{FunctionIr, Insn, OpCode};
use rk_core::Addr;

pub(crate) fn bounded_tails<'a>(
    fir: &'a FunctionIr,
    bounds: &BTreeMap<Addr, Vec<Addr>>,
) -> Cow<'a, FunctionIr> {
    let mut refined = Cow::Borrowed(fir);
    for (&index, original) in &fir.exits {
        let insn = &fir.insns[index as usize];
        let Some(targets) = bounds.get(&insn.addr) else {
            continue;
        };
        if !original.is_empty()
            || targets.is_empty()
            || !insn.mnemonic.eq_ignore_ascii_case("JMP")
            || !insn.ops.iter().any(|op| op.opcode == OpCode::BranchInd)
            || targets.iter().any(|&target| fir.index_of(target).is_some())
        {
            continue;
        }
        let body = refined.to_mut();
        body.succ[index as usize].clear();
        body.exits.insert(index, targets.clone());
    }
    refined
}

pub(crate) fn closed(fir: &FunctionIr, tails: &BTreeSet<u32>) -> bool {
    for (index, insn) in fir.insns.iter().enumerate() {
        let index = index as u32;
        if tails.contains(&index) {
            continue;
        }
        if fir.exits.contains_key(&index) {
            return false;
        }
        let flow = insn.flow();
        let succ = &fir.succ[index as usize];
        if flow == Flow::Return {
            if !succ.is_empty() {
                return false;
            }
        } else if flow.is_jump() {
            if succ.is_empty() {
                return false;
            }
        } else if flow.is_call() {
            if succ.is_empty() && !fir.noreturn.contains_key(&index) {
                return false;
            }
        } else if succ.is_empty() && !is_return(insn) && !fir.noreturn.contains_key(&index) {
            return false;
        }
    }
    true
}

pub(crate) fn is_return(insn: &Insn) -> bool {
    insn.mnemonic.eq_ignore_ascii_case("RET")
        || insn.ops.iter().any(|op| op.opcode == OpCode::Return)
}

pub(crate) fn mnemonic(insn: &Insn) -> Cow<'_, str> {
    let name = insn.mnemonic.as_str();
    let name = name
        .get(..5)
        .filter(|prefix| prefix.eq_ignore_ascii_case("WAIT "))
        .map_or(name, |_| &name[5..]);
    if name.bytes().any(|byte| byte.is_ascii_lowercase()) {
        Cow::Owned(name.to_ascii_uppercase())
    } else {
        Cow::Borrowed(name)
    }
}
