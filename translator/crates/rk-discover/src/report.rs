use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Clone, Debug, Default, Serialize)]
pub struct PointerRun {
    pub start: String,
    pub end: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Gap {
    pub start: String,
    pub end: String,
    pub kind: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct Conflict {
    pub target: String,
    pub instruction: String,
    pub evidence: String,
    pub kind: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiscardedSeed {
    pub address: String,
    pub reason: String,
    pub evidence: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionReject {
    StartsInsideMappedInstruction,
    StartsInMappedFunction,
    OverlapsMappedCode,
    FlowsIntoMappedInstruction,
    FlowsToMappedInterior,
    CallsToDiscoveredInterior,
    CallsToUngroundedTarget,
    JumpsToDiscoveredInterior,
    PaddingOnly,
    RepeatedShortFragment,
    OnlySeededByDiscardedCode,
    AlignmentCallStub,
    DataTable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryReject {
    EntryInsideMappedInstruction,
    EntryInMappedFunction,
    EntryInRejectedFunction,
    EntryNotInFunction,
}

#[derive(Clone, Debug, Serialize)]
pub struct FunctionRejection {
    pub address: String,
    pub reason: FunctionReject,
    pub pass: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct EntryRejection {
    pub address: String,
    pub reason: EntryReject,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RejectionCounts {
    pub functions_starts_inside_mapped_instruction: usize,
    pub functions_starts_in_mapped_function: usize,
    pub functions_overlaps_mapped_code: usize,
    pub functions_flows_into_mapped_instruction: usize,
    pub functions_flows_to_mapped_interior: usize,
    pub functions_calls_to_discovered_interior: usize,
    pub functions_calls_to_ungrounded_target: usize,
    pub functions_jumps_to_discovered_interior: usize,
    pub functions_padding_only: usize,
    pub functions_repeated_short_fragment: usize,
    pub functions_only_seeded_by_discarded_code: usize,
    pub functions_alignment_call_stub: usize,
    pub functions_data_table: usize,
    pub entries_inside_mapped_instruction: usize,
    pub entries_in_mapped_function: usize,
    pub entries_in_rejected_function: usize,
    pub entries_not_in_function: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DiscoveryReport {
    pub instructions: usize,
    pub passes: usize,
    pub converged: bool,
    pub entry_seeds: usize,
    pub excluded_bytes: u64,
    pub discovered_functions: usize,
    pub kept_functions: usize,
    pub rejected_functions: Vec<FunctionRejection>,
    pub discovered_entries: usize,
    pub kept_entries: usize,
    pub rejected_entries: Vec<EntryRejection>,
    pub rejection_counts: RejectionCounts,
    pub pointer_runs: Vec<PointerRun>,
    pub switch_tables: Vec<PointerRun>,
    pub gaps: Vec<Gap>,
    pub traps: BTreeMap<String, String>,
    pub conflicts: Vec<Conflict>,
    pub seeds: BTreeMap<String, Vec<String>>,
    pub seed_ranks: BTreeMap<String, u16>,
    pub discarded_seeds: Vec<DiscardedSeed>,
}

impl DiscoveryReport {
    pub fn empty() -> Self {
        Self {
            converged: true,
            ..Self::default()
        }
    }
}
