use std::collections::{BTreeMap, BTreeSet};

use rk_core::contract::{Contract, Field, Fields, PrivateArguments};
use rk_core::ir::{OpCode, Space};
use rk_core::settings::Settings;
use rk_core::{Addr, Reason};
use rk_frontend::Analysis;

use crate::contracts::ContractAnalysis;
use crate::modes::ModeAnalysis;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrivateLayout {
    pub inputs: Fields,
    pub outputs: Fields,
    pub input_arguments: PrivateArguments,
}

#[derive(Clone, Debug, Default)]
pub struct PrivateAnalysis {
    pub targets: BTreeMap<Addr, Contract>,
    pub layouts: BTreeMap<Addr, Vec<PrivateLayout>>,
    pub masks: BTreeMap<Addr, BTreeSet<Fields>>,
    pub input_arguments: BTreeMap<Addr, PrivateArguments>,
    pub callers: BTreeSet<Addr>,
    pub rejections: BTreeMap<Addr, Reason>,
    pub known_modes: BTreeSet<Addr>,
}

pub(crate) struct PrivateInput<'a> {
    pub settings: &'a Settings,
    pub analysis: &'a Analysis,
    pub contracts: &'a ContractAnalysis,
    pub modes: &'a ModeAnalysis,
    pub opaque: &'a BTreeSet<Addr>,
}

pub(crate) fn analyze(input: &PrivateInput<'_>) -> PrivateAnalysis {
    let enabled = input.settings.call_contracts && input.settings.module.is_none();
    let mut result = PrivateAnalysis {
        input_arguments: input.settings.private_input_registers.clone(),
        ..PrivateAnalysis::default()
    };
    if !enabled {
        return result;
    }
    let uses = call_uses(input.analysis, input.modes, input.contracts, input.opaque);
    let mut candidates: BTreeSet<Addr> = input
        .settings
        .private_input_registers
        .keys()
        .copied()
        .collect();
    if input.settings.private_leaf_contracts {
        for &addr in input.contracts.contracts.keys() {
            if input.opaque.contains(&addr)
                || input.contracts.rejected.contains_key(&addr)
                || !input.modes.prepared.contains(&addr)
                || !input.modes.facts.returns.contains_key(&addr)
                || input.modes.calls.contains(&addr)
                || !single_entry(input.analysis, addr)
            {
                continue;
            }
            let count = uses.get(&addr).copied().unwrap_or(0);
            let instructions = input
                .analysis
                .ir
                .get(&addr)
                .and_then(|result| result.as_ref().ok())
                .map_or(0, |fir| fir.insns.len());
            if count >= 2 && instructions <= 64 * count {
                candidates.insert(addr);
            }
        }
    }
    for addr in candidates {
        if input.opaque.contains(&addr) || input.contracts.rejected.contains_key(&addr) {
            result.rejections.insert(
                addr,
                Reason::Conservative {
                    detail: "private state entry requires a leaf without opaque observers".into(),
                },
            );
            continue;
        }
        let Some(contract) = input.contracts.contracts.get(&addr).copied() else {
            result.rejections.insert(
                addr,
                Reason::Conservative {
                    detail: "private state entry has no register contract".into(),
                },
            );
            continue;
        };
        result.targets.insert(addr, contract);
    }
    result.input_arguments = result
        .input_arguments
        .iter()
        .filter_map(|(addr, arguments)| {
            let reads = result.targets.get(addr)?.reads;
            Some((
                *addr,
                PrivateArguments {
                    registers: arguments.registers.intersection(reads),
                    st0: arguments.st0,
                },
            ))
        })
        .filter(|(_, arguments)| !arguments.is_empty())
        .collect();
    result.known_modes = result
        .targets
        .keys()
        .copied()
        .filter(|addr| {
            input.modes.leaves.contains(addr)
                && !input.opaque.contains(addr)
                && x87_count(input.analysis, *addr) > 0
        })
        .collect();
    result.callers = callers(input.analysis, &result.targets, input.settings, input);
    result
}

pub fn specialize(
    requests: &BTreeMap<Addr, BTreeSet<(Addr, Fields)>>,
    contracts: &BTreeMap<Addr, Contract>,
    input_arguments: &BTreeMap<Addr, PrivateArguments>,
) -> PrivateAnalysis {
    let mut masks: BTreeMap<Addr, Fields> = BTreeMap::new();
    let mut flagless: BTreeMap<Addr, Fields> = BTreeMap::new();
    let mut flagless_users: BTreeMap<Addr, usize> = BTreeMap::new();
    let flags = Fields::FLAGS;
    for wanted in requests.values() {
        let mut local_flagless = BTreeSet::new();
        for &(target, mask) in wanted {
            *masks.entry(target).or_default() |= mask;
            if mask & flags == Fields::NONE {
                *flagless.entry(target).or_default() |= mask;
                local_flagless.insert(target);
            }
        }
        for target in local_flagless {
            *flagless_users.entry(target).or_default() += 1;
        }
    }
    let mut result = PrivateAnalysis {
        input_arguments: input_arguments.clone(),
        ..PrivateAnalysis::default()
    };
    for (&target, &mask) in &masks {
        let inputs = contracts.get(&target).map_or(Fields::ALL, |contract| {
            contract.reads | Fields(Field::Esp.bit()) | Fields(Field::Ebp.bit())
        });
        push_layout(&mut result, target, mask, inputs, input_arguments);
    }
    for (&target, &mask) in &flagless {
        if flagless_users.get(&target).copied().unwrap_or(0) >= 16
            && masks.get(&target).copied().unwrap_or_default() & flags != Fields::NONE
        {
            let inputs = result
                .layouts
                .get(&target)
                .and_then(|layouts| layouts.first())
                .map_or(Fields::ALL, |layout| layout.inputs);
            push_layout(&mut result, target, mask, inputs, input_arguments);
        }
    }
    for layouts in result.layouts.values_mut() {
        layouts.sort_by_key(|layout| (layout.outputs, layout.input_arguments));
        layouts.dedup();
    }
    result
}

fn push_layout(
    result: &mut PrivateAnalysis,
    target: Addr,
    mask: Fields,
    input_contract: Fields,
    input_arguments: &BTreeMap<Addr, PrivateArguments>,
) {
    let arguments = input_arguments.get(&target).copied().unwrap_or_default();
    let layout = PrivateLayout {
        inputs: input_contract.difference(arguments.registers),
        outputs: mask,
        input_arguments: arguments,
    };
    result.layouts.entry(target).or_default().push(layout);
    result.masks.entry(target).or_default().insert(mask);
}

fn callers(
    analysis: &Analysis,
    targets: &BTreeMap<Addr, Contract>,
    settings: &Settings,
    input: &PrivateInput<'_>,
) -> BTreeSet<Addr> {
    let mut callers = settings.state_contract_callers.clone();
    for (caller, result) in &analysis.ir {
        if !input.modes.prepared.contains(caller)
            || !input.contracts.contracts.contains_key(caller)
            || input.opaque.contains(caller)
            || !single_entry(analysis, *caller)
        {
            continue;
        }
        let Ok(fir) = result else { continue };
        if settings.private_leaf_contracts
            && fir.insns.iter().any(|insn| {
                insn.ops.iter().any(|op| {
                    op.opcode == OpCode::Call
                        && op.inputs.first().is_some_and(|v| {
                            matches!(v.space, Space::Const | Space::Ram)
                                && targets.contains_key(&(v.offset as Addr))
                        })
                })
            })
        {
            callers.insert(*caller);
        }
        if !settings.private_input_registers.is_empty()
            && fir.insns.iter().any(|insn| {
                insn.ops.iter().any(|op| {
                    op.opcode == OpCode::Call
                        && op.inputs.first().is_some_and(|v| {
                            matches!(v.space, Space::Const | Space::Ram)
                                && settings
                                    .private_input_registers
                                    .contains_key(&(v.offset as Addr))
                        })
                })
            })
        {
            callers.insert(*caller);
        }
    }
    callers
}

fn call_uses(
    analysis: &Analysis,
    modes: &ModeAnalysis,
    contracts: &ContractAnalysis,
    opaque: &BTreeSet<Addr>,
) -> BTreeMap<Addr, usize> {
    let mut uses = BTreeMap::new();
    for (&caller, result) in &analysis.ir {
        if !modes.prepared.contains(&caller)
            || !contracts.contracts.contains_key(&caller)
            || opaque.contains(&caller)
            || !single_entry(analysis, caller)
        {
            continue;
        }
        let Ok(fir) = result else { continue };
        for insn in &fir.insns {
            for op in &insn.ops {
                if op.opcode == OpCode::Call {
                    if let Some(v) = op.inputs.first() {
                        if matches!(v.space, Space::Const | Space::Ram) {
                            *uses.entry(v.offset as Addr).or_default() += 1;
                        }
                    }
                }
            }
        }
    }
    uses
}

fn single_entry(analysis: &Analysis, addr: Addr) -> bool {
    analysis
        .ir
        .get(&addr)
        .and_then(|result| result.as_ref().ok())
        .is_some_and(|fir| fir.entries.is_empty())
}

fn x87_count(analysis: &Analysis, addr: Addr) -> usize {
    analysis
        .ir
        .get(&addr)
        .and_then(|result| result.as_ref().ok())
        .map_or(0, |fir| {
            fir.insns
                .iter()
                .filter(|insn| {
                    let name = insn.mnemonic.to_ascii_uppercase();
                    name.starts_with('F') && name != "FEMMS"
                })
                .count()
        })
}
