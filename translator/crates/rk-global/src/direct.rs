use std::collections::{BTreeMap, BTreeSet};

use rk_core::contract::{Contract, DirectArguments};
use rk_core::ir::{OpCode, Space};
use rk_core::settings::Settings;
use rk_core::{Addr, Reason};
use rk_frontend::Analysis;

use crate::contracts::ContractAnalysis;
use crate::modes::ModeAnalysis;

#[derive(Clone, Debug, Default)]
pub struct DirectAnalysis {
    pub targets: BTreeMap<Addr, Contract>,
    pub input_arguments: BTreeMap<Addr, DirectArguments>,
    pub rejections: BTreeMap<Addr, Reason>,
    pub known_modes: BTreeSet<Addr>,
}

pub(crate) struct DirectInput<'a> {
    pub settings: &'a Settings,
    pub analysis: &'a Analysis,
    pub contracts: &'a ContractAnalysis,
    pub modes: &'a ModeAnalysis,
    pub opaque: &'a BTreeSet<Addr>,
}

pub(crate) fn analyze(input: &DirectInput<'_>) -> DirectAnalysis {
    let enabled = input.settings.call_contracts && input.settings.module.is_none();
    let mut result = DirectAnalysis {
        input_arguments: input.settings.direct_arguments.clone(),
        ..DirectAnalysis::default()
    };
    if !enabled {
        return result;
    }
    let uses = call_uses(input.analysis, input.modes, input.contracts, input.opaque);
    let mut candidates: BTreeSet<Addr> = input.settings.direct_arguments.keys().copied().collect();
    if input.settings.direct_variants {
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
                    detail: "direct state entry requires a leaf without opaque observers".into(),
                },
            );
            continue;
        }
        let Some(contract) = input.contracts.contracts.get(&addr).copied() else {
            result.rejections.insert(
                addr,
                Reason::Conservative {
                    detail: "direct state entry has no register contract".into(),
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
                DirectArguments {
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
    result
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
