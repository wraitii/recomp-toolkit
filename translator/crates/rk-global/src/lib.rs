//! Whole-program analysis between lifting and planning. `contracts` builds SSA for every function
//! and solves register and flag call contracts over call-graph SCCs, using indirect call and jump
//! bounds and configured site effects. Functions it cannot prove stay conservative, with a reason.
//! `modes` propagates x87 control-word facts. `private` selects private-contract targets and
//! checked returns, and `specialize` merges the planner's call-site requests into variant layouts.
//! `GlobalFacts::call_facts` feeds the contracts back into SSA.

mod borrows;
mod callgraph;
mod cfg;
mod contracts;
pub mod memory;
mod modes;
mod private;
pub mod stack;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use rk_core::contract::{Contract, Field};
use rk_core::facts::Summary as FactSummary;
use rk_core::program::Program;
use rk_core::settings::CallEffect as ConfiguredEffect;
use rk_core::settings::Settings;
use rk_core::{Addr, Result};
use rk_frontend::Analysis;
use rk_ssa::{CallEffect, CallFacts, CallSummary};

pub use modes::{ModeCondition, ReturnSummary, X87ModeFacts, CW_MASK, CW_PC_RC};
pub use private::{specialize, PrivateAnalysis, PrivateLayout};

pub struct ContractCallFacts<'a> {
    contracts: &'a BTreeMap<Addr, Contract>,
    indirect: &'a BTreeMap<Addr, Contract>,
    effects: &'a BTreeMap<Addr, ConfiguredEffect>,
    borrows: Option<&'a borrows::Analysis>,
    returns: Option<&'a stack::Analysis>,
}

pub(crate) fn effect_contract(effect: &ConfiguredEffect) -> Option<Contract> {
    effect.regs.map(|regs| regs.contract())
}

pub(crate) fn call_effect(contract: Contract) -> CallEffect {
    CallEffect {
        reads: contract.reads,
        kills: contract.kills,
        writes: contract.writes,
        preserves: contract.preserves,
    }
}

impl CallFacts for ContractCallFacts<'_> {
    fn summary(&self, site: Addr, target: Option<Addr>) -> CallSummary<'_> {
        let returns = self.returns.and_then(|returns| match target {
            Some(target) => returns.functions.get(&target),
            None => returns.sites.get(&site),
        });
        let borrows = self.borrows;
        CallSummary {
            cleanup: returns
                .map(|summary| summary.effect.cleanup)
                .or_else(|| target.and_then(|target| self.returns?.cleanups.get(&target).copied())),
            effects: None,
            borrowed: borrows.and_then(|borrows| match target {
                Some(target) => borrows.functions.get(&target),
                None => borrows.sites.get(&site),
            }),
            checkpoints: borrows.and_then(|borrows| match target {
                Some(target) => borrows.checkpoints.get(&target).copied(),
                None => borrows.sites.get(&site)?.checkpoints,
            }),
            readonly: returns.is_some_and(|summary| {
                !summary.writes_other
                    && summary.effect.writes.is_empty()
                    && !summary.mapped_noreturn
                    && summary.returns == stack::ReturnEffect::Exact
            }),
            argument_writes: target
                .and_then(|target| self.returns?.argument_writes.get(&target))
                .map(Vec::as_slice),
        }
    }
    fn call(&self, site: Addr, target: Option<Addr>) -> CallEffect {
        if let Some(effect) = self.effects.get(&site) {
            match effect_contract(effect) {
                Some(contract) => return call_effect(contract),
                None => {}
            }
        }
        let contract = match target {
            Some(target) => self.contracts.get(&target).copied(),
            None => self.indirect.get(&site).copied(),
        };
        contract.map_or(CallEffect::CONSERVATIVE, call_effect)
    }
}

impl From<&X87ModeFacts> for rk_x87::ModeInput {
    fn from(facts: &X87ModeFacts) -> Self {
        Self {
            known_modes: facts.known_modes.clone(),
        }
    }
}

pub struct GlobalInput<'a> {
    pub settings: &'a Settings,
    pub program: &'a Program,
    pub analysis: &'a Analysis,
    pub indirect: &'a BTreeMap<Addr, Vec<Addr>>,
    pub indirect_jumps: &'a BTreeMap<Addr, Vec<Addr>>,
    pub site_effects: &'a BTreeMap<Addr, ConfiguredEffect>,
    pub fact_summary: &'a FactSummary,
}

#[derive(Clone, Debug, Default)]
pub struct GlobalReport {
    pub stack_functions: usize,
    pub stack_sites: usize,
    pub stack_readonly_sites: usize,
    pub stack_mapped_noreturn_sites: usize,
    pub stack_rejected: BTreeMap<String, usize>,
    pub stack_rejected_sites: BTreeMap<String, usize>,
    pub functions: usize,
    pub contract_excluded: BTreeMap<String, usize>,
    pub contracts: usize,
    pub indirect_contracts: usize,
    pub call_bounds: usize,
    pub jump_bounds: usize,
    pub jump_bounds_consumed: usize,
    pub jump_bounds_unsupported: usize,
    pub site_effects: usize,
    pub import_effects: usize,
    pub fact_evidence: usize,
    pub sccs: usize,
    pub private_targets: usize,
    pub private_variants: usize,
    pub checked_returns: usize,
    pub selected_callers: usize,
    pub known_modes: usize,
    pub instruction_checks: usize,
    pub instruction_mismatches: BTreeMap<Addr, (usize, usize)>,
}

#[derive(Clone, Debug, Default)]
pub struct GlobalFacts {
    borrows: borrows::Analysis,
    pub stack: stack::Analysis,
    pub returns: stack::Analysis,
    pub memory: memory::Analysis,
    memory_inference: bool,
    pub contracts: BTreeMap<Addr, Contract>,
    pub indirect_contracts: BTreeMap<Addr, Contract>,
    pub site_effects: BTreeMap<Addr, ConfiguredEffect>,
    pub private: PrivateAnalysis,
    pub x87: X87ModeFacts,
    pub report: GlobalReport,
}

impl GlobalFacts {
    pub fn analyze(input: &GlobalInput<'_>) -> Result<Self> {
        let opaque_helpers: BTreeSet<Addr> = input.analysis.seh_helpers.clone();
        let native = input.settings.native_replaced()?;
        let mut mode_opaque = opaque_helpers.clone();
        mode_opaque.extend(native.iter().copied());
        let stack = if input.settings.stack_memory {
            stack::analyze(
                input.program,
                input.analysis,
                &mode_opaque,
                input.indirect,
                false,
                input.settings,
            )
        } else {
            stack::Analysis::default()
        };
        let returns = stack::analyze(
            input.program,
            input.analysis,
            &mode_opaque,
            input.indirect,
            true,
            input.settings,
        );
        let modes = modes::analyze(
            input.analysis,
            input.indirect,
            input.indirect_jumps,
            input.site_effects,
            &input
                .program
                .functions
                .iter()
                .filter_map(|(&addr, function)| function.noreturn.then_some(addr))
                .collect(),
            &mode_opaque,
        );
        let contracts = contracts::analyze(
            input.program,
            input.analysis,
            input.indirect,
            input.indirect_jumps,
            input.site_effects,
            &native,
        )?;
        let calls = stack::CallFacts {
            registers: ContractCallFacts {
                contracts: &contracts.contracts,
                indirect: &contracts.indirect,
                effects: input.site_effects,
                borrows: None,
                returns: Some(&returns),
            },
            stack: &stack,
            memory: None,
        };
        let memory = memory::analyze(input, &mode_opaque, &calls);
        let borrows = borrows::analyze(input, &mode_opaque, &calls, &returns);
        let mut opaque = opaque_helpers;
        opaque.extend(contracts.rejected.keys().copied());
        let private = private::analyze(&private::PrivateInput {
            settings: input.settings,
            analysis: input.analysis,
            contracts: &contracts,
            modes: &modes,
            opaque: &opaque,
        });
        let mut report = GlobalReport {
            stack_functions: stack.functions.len(),
            stack_sites: stack.sites.len(),
            stack_readonly_sites: stack
                .sites
                .values()
                .filter(|summary| summary.effect.writes.is_empty())
                .count(),
            stack_mapped_noreturn_sites: stack
                .sites
                .values()
                .filter(|summary| summary.mapped_noreturn)
                .count(),
            stack_rejected: stack.rejected.clone(),
            stack_rejected_sites: stack.rejected_sites.clone(),
            functions: input.program.functions.len(),
            contracts: contracts.contracts.len(),
            indirect_contracts: contracts.indirect.len(),
            call_bounds: input.fact_summary.call_bounds,
            jump_bounds: input.fact_summary.jump_bounds,
            jump_bounds_consumed: modes.jump_bounds_consumed,
            jump_bounds_unsupported: input
                .indirect_jumps
                .len()
                .saturating_sub(modes.jump_bounds_consumed),
            site_effects: input.fact_summary.site_effects,
            import_effects: input.fact_summary.import_effects,
            fact_evidence: input.fact_summary.evidence,
            private_targets: private.targets.len(),
            private_variants: private.masks.values().map(BTreeSet::len).sum(),
            checked_returns: private.checked_returns.len(),
            selected_callers: private.callers.len(),
            known_modes: private.known_modes.len(),
            sccs: modes.sccs,
            instruction_checks: contracts.instruction_checks,
            instruction_mismatches: contracts.instruction_mismatches,
            ..GlobalReport::default()
        };
        for reason in contracts.rejected.values() {
            let name = match reason {
                rk_core::Reason::Conservative { detail } => detail.clone(),
                other => format!("{other:?}"),
            };
            *report.contract_excluded.entry(name).or_default() += 1;
        }
        Ok(Self {
            borrows,
            stack,
            returns,
            memory,
            memory_inference: input.settings.stack_memory,
            contracts: contracts.contracts,
            indirect_contracts: contracts.indirect,
            site_effects: input.site_effects.clone(),
            private,
            x87: modes.facts,
            report,
        })
    }

    pub fn call_facts(&self) -> ContractCallFacts<'_> {
        ContractCallFacts {
            contracts: &self.contracts,
            indirect: &self.indirect_contracts,
            effects: &self.site_effects,
            borrows: Some(&self.borrows),
            returns: Some(&self.returns),
        }
    }

    pub fn inference_call_facts(&self) -> stack::CallFacts<'_> {
        stack::CallFacts {
            registers: self.call_facts(),
            stack: &self.stack,
            memory: self.memory_inference.then_some(&self.memory),
        }
    }

    pub fn summary(&self) -> String {
        format_report(&self.report)
    }

    pub fn dump(&self, out: &mut impl Write) -> std::io::Result<()> {
        writeln!(out, "global report {}", format_report(&self.report))?;
        for (&addr, reason) in &self.returns.rejected_functions {
            writeln!(out, "return-rejected {addr:08x} {reason}")?;
        }
        for (&addr, source) in &self.returns.stack_helpers {
            writeln!(out, "stack-helper {addr:08x} {source}")?;
        }
        for (&addr, source) in &self.returns.stack_probes {
            writeln!(out, "stack-probe {addr:08x} {source}")?;
        }
        for (&addr, cleanup) in &self.returns.cleanups {
            writeln!(out, "cleanup-only {addr:08x} pop={cleanup}")?;
        }
        for (&addr, reason) in &self.returns.rejected_cleanups {
            writeln!(out, "cleanup-rejected {addr:08x} {reason}")?;
        }
        for (&addr, writes) in &self.returns.argument_writes {
            writeln!(out, "argument-writes {addr:08x} {writes:?}")?;
        }
        writeln!(
            out,
            "jump-bounds consumed={} unsupported={}",
            self.report.jump_bounds_consumed, self.report.jump_bounds_unsupported
        )?;
        for (&addr, &(expected, visited)) in self.report.instruction_mismatches.iter().take(8) {
            writeln!(
                out,
                "instruction-mismatch {:08x} expected={expected} visited={visited}",
                addr
            )?;
        }
        writeln!(out, "memory-effects")?;
        for (&addr, effects) in &self.memory.functions {
            writeln!(
                out,
                "memory {addr:08x} reads={:?} writes={:?}",
                effects.reads, effects.writes
            )?;
        }
        for (&addr, summary) in &self.borrows.functions {
            writeln!(out, "borrowed-stack {addr:08x} checkpoints={:?} reads={:?} writes={:?} captures={:?} unknown-captures={:?}",
                summary.checkpoints, summary.effects.reads, summary.effects.writes, summary.captures, summary.unknown_captures)?;
        }
        for (&addr, cost) in &self.borrows.checkpoints {
            writeln!(out, "checkpoint-bound {addr:08x} calls={cost}")?;
        }
        writeln!(out, "contracts")?;
        for (&addr, contract) in &self.contracts {
            writeln!(
                out,
                "contract {:08x} reads={} kills={} writes={} preserves={}",
                addr,
                fields(contract.reads),
                fields(contract.kills),
                fields(contract.writes),
                fields(contract.preserves),
            )?;
        }
        writeln!(out, "indirect-contracts")?;
        for (&site, contract) in &self.indirect_contracts {
            writeln!(
                out,
                "indirect {:08x} reads={} kills={} writes={} preserves={}",
                site,
                fields(contract.reads),
                fields(contract.kills),
                fields(contract.writes),
                fields(contract.preserves),
            )?;
        }
        writeln!(out, "site-effects")?;
        for (&site, effect) in &self.site_effects {
            let contract = effect_contract(effect);
            let x87 = effect.x87.map(|value| {
                let (preserved, known, value) = value.control_word();
                format!("{preserved:04x}:{known:04x}:{value:04x}")
            });
            writeln!(
                out,
                "site-effect {:08x} regs={} contract={} x87={}",
                site,
                effect.regs.map_or("-", |value| match value {
                    rk_core::settings::RegsEffect::Abi => "abi",
                    rk_core::settings::RegsEffect::Unknown => "unknown",
                    rk_core::settings::RegsEffect::Contract(_) => "explicit",
                }),
                contract.map_or("-".to_string(), |contract| format!(
                    "reads={} kills={} writes={} preserves={}",
                    fields(contract.reads),
                    fields(contract.kills),
                    fields(contract.writes),
                    fields(contract.preserves),
                )),
                x87.unwrap_or_else(|| "-".to_string()),
            )?;
        }
        writeln!(out, "private")?;
        for (&addr, contract) in &self.private.targets {
            writeln!(
                out,
                "private {:08x} reads={} kills={} writes={} preserves={}",
                addr,
                fields(contract.reads),
                fields(contract.kills),
                fields(contract.writes),
                fields(contract.preserves),
            )?;
        }
        for addr in &self.private.checked_returns {
            writeln!(out, "checked {:08x}", addr)?;
        }
        for addr in &self.private.callers {
            writeln!(out, "caller {:08x}", addr)?;
        }
        for (&addr, layouts) in &self.private.layouts {
            for layout in layouts {
                writeln!(
                    out,
                    "layout {:08x} mask={:04x} inputs={} argument={}",
                    addr,
                    layout.outputs.0,
                    fields(layout.inputs),
                    layout.input_argument.map_or("-", Field::name),
                )?;
            }
        }
        for (&addr, field) in &self.private.input_arguments {
            writeln!(out, "input-argument {:08x} {}", addr, field.name())?;
        }
        for addr in &self.private.known_modes {
            writeln!(out, "known-mode {:08x}", addr)?;
        }
        writeln!(out, "x87")?;
        for (&addr, summary) in &self.x87.returns {
            writeln!(
                out,
                "x87-return {:08x} preserved={:04x} known={:04x} value={:04x}",
                addr, summary.preserved, summary.known, summary.value
            )?;
        }
        Ok(())
    }
}

fn fields(fields: rk_core::contract::Fields) -> String {
    let names: Vec<&str> = fields.iter().map(Field::name).collect();
    if names.is_empty() {
        "-".to_string()
    } else {
        names.join(",")
    }
}

fn format_report(report: &GlobalReport) -> String {
    let excluded: Vec<String> = report
        .contract_excluded
        .iter()
        .map(|(reason, count)| format!("{reason}={count}"))
        .collect();
    format!(
        "functions={} contracts={} indirect={} bounds={}/{} jumps_consumed={} jumps_unsupported={} effects={}/{} evidence={} excluded=[{}] private={} variants={} checked={} callers={} known_modes={} instruction_checks={} instruction_mismatches={}",
        report.functions,
        report.contracts,
        report.indirect_contracts,
        report.call_bounds,
        report.jump_bounds,
        report.jump_bounds_consumed,
        report.jump_bounds_unsupported,
        report.site_effects,
        report.import_effects,
        report.fact_evidence,
        excluded.join(","),
        report.private_targets,
        report.private_variants,
        report.checked_returns,
        report.selected_callers,
        report.known_modes,
        report.instruction_checks,
        report.instruction_mismatches.len(),
    )
}
