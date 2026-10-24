//! Recovers code the code map does not cover. Seeds come from the PE entry point, exports, TLS
//! callbacks, relocated pointers, mapped flow, analysis-fact targets and `discovery_evidence`.
//! Discovery traces direct flow, pointer runs and switch tables, then sweeps unclaimed executable
//! bytes; higher-ranked evidence retracts lower-ranked evidence. Fragment filters discard padding,
//! stubs and low-entropy bodies, and passes repeat until nothing changes. iced-x86 decodes,
//! with every length cross-checked against SLEIGH. The map stays authoritative: `merge`
//! adds only new, provisional functions, and every rejection is reported with its pass and reason.

pub mod report;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};
use rayon::prelude::*;
use rk_core::facts::{AnalysisFacts, TransferKind};
use rk_core::program::{Function, Image, InteriorKind, PeDetails, Program, Span};
use rk_core::settings::Settings;
use rk_core::{parse_hex, Addr, Error, Hex, Result};

use report::{
    Conflict, DiscardedSeed, DiscoveryReport, EntryReject, EntryRejection, FunctionReject,
    FunctionRejection, Gap, PointerRun, RejectionCounts,
};

const RANK_TRUSTED: u16 = 100;
const RANK_MAPPED_FLOW: u16 = 90;
const RANK_OBSERVED: u16 = 80;
const RANK_RELOCATED_FLOW: u16 = 70;
const RANK_RELOCATION: u16 = 60;
const RANK_RELOCATED_IMMEDIATE: u16 = 55;
const RANK_IMMEDIATE: u16 = 45;
const RANK_POINTER: u16 = 40;
const RANK_SWEEP: u16 = 30;
const RANK_INTERIOR_POINTER: u16 = 20;
const RANK_SINGLE_POINTER: u16 = 15;
const BODY_BYTES: Addr = 4096;
const SHORT_FRAGMENT_MIN: Addr = 4;
const SHORT_FRAGMENT_MAX: Addr = 8;
const SHORT_FRAGMENT_REPEAT_LIMIT: usize = 10;
const DATA_ENTROPY_MIN_BYTES: Addr = 512;
const DATA_ENTROPY_MAX: f64 = 3.5;
const DATA_ZERO_RATIO_MIN: f64 = 0.4;
const MAX_DISCOVERY_PASSES: usize = 8;
const SWITCH_LOOKBACK: usize = 6;
const SWITCH_SLOTS: u32 = 4096;
const SWITCH_REACH: u32 = 0x10000;
const NORETURN_IMPORTS: &[&str] = &[
    "ExitProcess",
    "ExitThread",
    "FreeLibraryAndExitThread",
    "_exit",
    "exit",
    "abort",
    "_amsg_exit",
    "_CxxThrowException",
    "longjmp",
    "_endthread",
    "_endthreadex",
];

type Functions = BTreeMap<Addr, Vec<Span>>;
type Entries = BTreeMap<Addr, (Addr, InteriorKind)>;

#[derive(Default)]
struct SleighLengths {
    cache: OnceLock<Box<[AtomicU8]>>,
}

thread_local! {
    static LIFTER: std::cell::RefCell<Option<rk_sleigh::Lifter>> = const { std::cell::RefCell::new(None) };
}

impl SleighLengths {
    fn length(&self, image: &Image, at: Addr) -> Option<u8> {
        let cache = self
            .cache
            .get_or_init(|| (0..image.size).map(|_| AtomicU8::new(0)).collect());
        let slot = cache.get(at.saturating_sub(image.base) as usize)?;
        let cached = slot.load(Ordering::Relaxed);
        if cached != 0 {
            return (cached != u8::MAX).then_some(cached);
        }
        let available = image.end().saturating_sub(at).min(15) as usize;
        let bytes = image.bytes(at, available)?;
        let out = LIFTER.with(|lifter| {
            let mut lifter = lifter.borrow_mut();
            let lifter = lifter
                .get_or_insert_with(|| rk_sleigh::Lifter::new().expect("SLEIGH initialization"));
            lifter.length(at, bytes).ok()
        });
        slot.store(out.unwrap_or(u8::MAX), Ordering::Relaxed);
        out
    }
}

#[derive(Clone, Copy, Debug)]
enum SimpleOp {
    None,
    Reg(Register),
    Mem(Mem),
    Imm(u32),
}

#[derive(Clone, Copy, Debug)]
struct Mem {
    base: Register,
    index: Register,
    scale: u32,
    displacement: u32,
    size: u32,
}

#[derive(Clone, Debug)]
struct Insn {
    addr: Addr,
    size: u8,
    flow: FlowControl,
    mnemonic: Mnemonic,
    ops: [SimpleOp; 4],
    op_count: u8,
    direct: Option<(Addr, bool)>,
    imm4: Option<(u8, u32)>,
    disp4: Option<(u8, u32)>,
    padding: bool,
}

impl Insn {
    fn is_call(&self) -> bool {
        matches!(self.flow, FlowControl::Call | FlowControl::IndirectCall)
    }

    fn is_jump(&self) -> bool {
        matches!(
            self.flow,
            FlowControl::ConditionalBranch
                | FlowControl::UnconditionalBranch
                | FlowControl::IndirectBranch
        )
    }

    fn is_ret(&self) -> bool {
        self.flow == FlowControl::Return
    }

    fn end(&self) -> Addr {
        self.addr + self.size as u32
    }

    fn signature(&self) -> String {
        format!(
            "{}:{:?}:{:?}:{:?}:{}:{:?}:{:?}:{:?}:{}",
            self.size,
            self.mnemonic,
            self.flow,
            self.ops,
            self.op_count,
            self.direct,
            self.imm4,
            self.disp4,
            self.padding
        )
    }

    fn seeds_flow(&self) -> bool {
        self.is_call()
            || (self.is_jump()
                && !matches!(
                    self.mnemonic,
                    Mnemonic::Loop | Mnemonic::Loope | Mnemonic::Loopne
                ))
    }

    fn ends_flow(&self) -> bool {
        self.is_ret()
            || matches!(
                self.mnemonic,
                Mnemonic::Jmp | Mnemonic::Int3 | Mnemonic::Ud0 | Mnemonic::Ud1 | Mnemonic::Ud2
            )
    }

    fn flow_target(&self) -> Option<(Addr, bool)> {
        self.direct.filter(|_| self.seeds_flow())
    }

    fn absolute_slot(&self) -> Option<Addr> {
        match self.ops[0] {
            SimpleOp::Mem(m) if m.base == Register::None && m.index == Register::None => {
                Some(m.displacement)
            }
            _ => None,
        }
    }

    fn pointer_immediate(&self) -> Option<Addr> {
        matches!(self.mnemonic, Mnemonic::Push | Mnemonic::Mov)
            .then_some(self.imm4)
            .flatten()
            .map(|(_, value)| value)
    }

    fn switch_jump(&self) -> Option<(Register, Addr)> {
        if self.flow != FlowControl::IndirectBranch || self.op_count != 1 {
            return None;
        }
        match self.ops[0] {
            SimpleOp::Mem(m)
                if m.base == Register::None
                    && m.index != Register::None
                    && m.scale == 4
                    && m.size == 4 =>
            {
                Some((m.index.full_register(), m.displacement))
            }
            _ => None,
        }
    }

    fn writes(&self, register: Register) -> bool {
        !matches!(
            self.mnemonic,
            Mnemonic::Cmp | Mnemonic::Test | Mnemonic::Push
        ) && matches!(self.ops[0], SimpleOp::Reg(r) if r.full_register() == register)
    }
}

struct Switch {
    table: Addr,
    slots: u32,
    index_table: Option<(Addr, u32)>,
}

#[derive(Clone, Debug)]
pub struct Discovered {
    pub functions: Functions,
    pub entries: Entries,
    pub instruction_lengths: BTreeMap<Addr, u8>,
    pub report: DiscoveryReport,
}

pub fn discover(settings: &Settings, program: &Program) -> Result<Discovered> {
    if !settings.discover {
        return Ok(Discovered {
            functions: BTreeMap::new(),
            entries: BTreeMap::new(),
            instruction_lengths: BTreeMap::new(),
            report: DiscoveryReport::empty(),
        });
    }
    let check = SleighLengths::default();
    let mut suppressions = Vec::new();
    let mut rejections = Vec::new();
    let mut functions = BTreeMap::new();
    let mut entries = BTreeMap::new();
    let mut recovery = None;
    let mut converged = false;
    for pass in 1..=MAX_DISCOVERY_PASSES {
        let mut current = Recovery::new(program, program.pe.clone(), &check, suppressions.clone());
        current.pass = pass;
        current.function_rejections = rejections;
        run_discovery(&mut current, settings)?;
        let (next_functions, next_entries) = current.code_map_rows()?;
        let (next_functions, next_entries, next_suppressions) =
            current.discard_fragments(next_functions, next_entries);
        let done = next_suppressions.is_empty();
        suppressions.extend(next_suppressions);
        rejections = std::mem::take(&mut current.function_rejections);
        functions = next_functions;
        entries = next_entries;
        recovery = Some(current);
        if done {
            converged = true;
            break;
        }
    }
    let mut recovery = recovery.expect("discovery runs at least once");
    recovery.function_rejections = rejections;
    let (functions, entries) = recovery.filter(functions, entries)?;
    let mut report = recovery.report();
    report.converged = converged;
    let instruction_lengths = recovery
        .instructions
        .iter()
        .map(|(&at, ins)| (at, ins.size))
        .collect();
    Ok(Discovered {
        functions,
        entries,
        instruction_lengths,
        report,
    })
}

fn run_discovery(recovery: &mut Recovery<'_>, settings: &Settings) -> Result<()> {
    recovery.map_lengths()?;
    recovery.initial_seeds();
    recovery.observed_seeds(settings)?;
    recovery.mapped_flow_seeds();
    recovery.pointer_tables();
    recovery.sweep();
    recovery.finish();
    Ok(())
}

pub fn merge(program: &mut Program, discovered: &Discovered) -> Result<()> {
    for (&addr, spans) in &discovered.functions {
        program.functions.entry(addr).or_insert_with(|| Function {
            addr,
            spans: spans.clone(),
            noreturn: false,
            provisional: true,
        });
    }
    for (&addr, &(owner, kind)) in &discovered.entries {
        if !program.entries.contains_key(&addr)
            && !program.functions.contains_key(&addr)
            && !program.configured_entries.contains(&addr)
            && program.functions.get(&owner).is_some_and(|f| f.provisional)
        {
            program.region_entries.insert(addr, owner);
        }
        program.interior.entry(addr).or_insert((owner, kind));
    }
    program.spans = program
        .functions
        .values()
        .flat_map(|f| f.spans.iter().map(move |s| (s.start, s.end(), f.addr)))
        .collect();
    program.spans.sort_unstable();
    rk_image::index_entries(program)
}

struct Recovery<'a> {
    image: &'a Image,
    details: PeDetails,
    check: &'a SleighLengths,
    delta: u32,
    instructions: BTreeMap<Addr, Insn>,
    covered: Vec<Addr>,
    speculative: BTreeSet<Addr>,
    trusted_seeds: BTreeSet<Addr>,
    seed_rank: BTreeMap<Addr, u16>,
    instruction_rank: BTreeMap<Addr, u16>,
    discarded_seeds: Vec<(Addr, String, Vec<String>)>,
    ghidra_spans: Vec<(Addr, Addr, Addr)>,
    ghidra_lengths: BTreeMap<Addr, Vec<u8>>,
    ghidra_entries: BTreeMap<Addr, Addr>,
    mapped_starts: BTreeSet<Addr>,
    mapped_ends: BTreeSet<Addr>,
    mapped_tables: BTreeMap<Addr, BTreeSet<Addr>>,
    noreturn_functions: BTreeSet<Addr>,
    noreturn_calls: BTreeSet<Addr>,
    seeds: BTreeMap<Addr, BTreeSet<String>>,
    pending: VecDeque<Addr>,
    roots: BTreeSet<Addr>,
    tables: Vec<(Addr, Addr)>,
    switches: BTreeMap<Addr, (Addr, Addr, Vec<Addr>)>,
    gaps: Vec<(Addr, Addr, &'static str)>,
    traps: BTreeMap<Addr, String>,
    data_ranges: Vec<(Addr, Addr, &'static str)>,
    known_mask: Vec<u8>,
    data_owner: Vec<i32>,
    conflicts: BTreeSet<(Addr, Addr, String)>,
    reloc_slots: BTreeSet<Addr>,
    known_entries: BTreeSet<Addr>,
    suppressed: Vec<(Addr, Addr)>,
    excluded_bytes: u64,
    kept_functions: usize,
    kept_entries: usize,
    pass: usize,
    function_rejections: Vec<(Addr, FunctionReject, usize)>,
    entry_rejections: Vec<(Addr, EntryReject)>,
}

impl<'a> Recovery<'a> {
    fn new(
        program: &'a Program,
        details: PeDetails,
        check: &'a SleighLengths,
        mut suppressed: Vec<(Addr, Addr)>,
    ) -> Self {
        let image = &program.image;
        suppressed.sort_unstable();
        let mut merged: Vec<(Addr, Addr)> = Vec::new();
        for (start, end) in suppressed {
            if start >= end {
                continue;
            }
            if let Some(last) = merged.last_mut() {
                if start <= last.1 {
                    last.1 = last.1.max(end);
                    continue;
                }
            }
            merged.push((start, end));
        }
        let mut ghidra_spans = Vec::new();
        let mut ghidra_lengths = BTreeMap::new();
        let mut known_mask = vec![0u8; image.size as usize];
        let mut excluded_bytes = 0u64;
        for function in program.functions.values() {
            for span in &function.spans {
                ghidra_spans.push((span.start, span.end(), function.addr));
                if let Some(lengths) = &span.lengths {
                    ghidra_lengths.insert(span.start, lengths.clone());
                }
                let lo = span.start.saturating_sub(image.base) as usize;
                let hi = span.end().saturating_sub(image.base) as usize;
                if lo < known_mask.len() {
                    let hi = hi.min(known_mask.len());
                    known_mask[lo..hi].fill(1);
                }
                excluded_bytes += span.size as u64;
            }
        }
        ghidra_spans.sort_unstable();
        let mapped_ends = ghidra_spans.iter().map(|&(_, end, _)| end).collect();
        let mut mapped_tables: BTreeMap<Addr, BTreeSet<Addr>> = BTreeMap::new();
        for (&(_, site), targets) in &program.tables {
            mapped_tables.entry(site).or_default().extend(targets);
        }
        let mut recovery = Self {
            image,
            delta: details.delta,
            data_ranges: Vec::new(),
            details,
            check,
            instructions: BTreeMap::new(),
            covered: vec![0; image.size as usize],
            speculative: BTreeSet::new(),
            trusted_seeds: BTreeSet::new(),
            seed_rank: BTreeMap::new(),
            instruction_rank: BTreeMap::new(),
            discarded_seeds: Vec::new(),
            ghidra_spans,
            ghidra_lengths,
            ghidra_entries: BTreeMap::new(),
            mapped_starts: BTreeSet::new(),
            mapped_ends,
            mapped_tables,
            noreturn_functions: program
                .functions
                .values()
                .filter(|f| f.noreturn)
                .map(|f| f.addr)
                .collect(),
            noreturn_calls: program.noreturn_calls.clone(),
            seeds: BTreeMap::new(),
            pending: VecDeque::new(),
            roots: BTreeSet::new(),
            tables: Vec::new(),
            switches: BTreeMap::new(),
            gaps: Vec::new(),
            traps: BTreeMap::new(),
            known_mask,
            data_owner: vec![-1; image.size as usize],
            conflicts: BTreeSet::new(),
            reloc_slots: image.relocations.iter().map(|r| r.site).collect(),
            known_entries: program.entries.keys().copied().collect(),
            suppressed: merged,
            excluded_bytes,
            kept_functions: 0,
            kept_entries: 0,
            pass: 1,
            function_rejections: Vec::new(),
            entry_rejections: Vec::new(),
        };
        for (lo, hi, kind) in recovery.details.data_ranges.clone() {
            recovery.add_data(lo, hi, kind);
        }
        recovery
    }

    fn offset(&self, at: Addr) -> Option<usize> {
        at.checked_sub(self.image.base)
            .map(|off| off as usize)
            .filter(|&off| off < self.covered.len())
    }

    fn owner_at(&self, at: Addr) -> Option<Addr> {
        self.offset(at)
            .map(|off| self.covered[off])
            .filter(|&owner| owner != 0)
    }

    fn rank_of(&self, at: Addr) -> u16 {
        self.instruction_rank.get(&at).copied().unwrap_or(0)
    }

    fn decode_at(&self, at: Addr) -> Option<Insn> {
        decode_at(self.image, self.check, at)
    }

    fn at_known_data(&self, at: Addr) -> bool {
        self.offset(at).is_some_and(|off| self.known_mask[off] != 0)
    }

    fn suppressed_at(&self, at: Addr) -> Option<(Addr, Addr)> {
        let index = self
            .suppressed
            .partition_point(|&(start, _)| start <= at)
            .checked_sub(1)?;
        let &(start, end) = self.suppressed.get(index)?;
        (at < end).then_some((start, end))
    }

    fn in_data(&self, at: Addr) -> bool {
        self.offset(at).is_some_and(|off| self.data_owner[off] >= 0)
    }

    fn add_data(&mut self, lo: Addr, hi: Addr, kind: &'static str) {
        let index = self.data_ranges.len() as i32;
        self.data_ranges.push((lo, hi, kind));
        let size = self.data_owner.len();
        let start = (lo.saturating_sub(self.image.base) as usize).min(size);
        let end = (hi.saturating_sub(self.image.base) as usize).min(size);
        for owner in &mut self.data_owner[start..end] {
            if *owner < 0 {
                *owner = index;
            }
        }
    }

    fn ghidra_owner(&self, target: Addr) -> Option<Addr> {
        let (_, _, owner) = self.mapped_span_at(target)?;
        self.mapped_starts.contains(&target).then_some(owner)
    }

    fn noreturn_import(&self, slot: Addr) -> bool {
        self.image
            .imports
            .get(&slot)
            .is_some_and(|import| NORETURN_IMPORTS.contains(&import.name.as_str()))
    }

    fn calls_noreturn(&self, ins: &Insn) -> bool {
        if !ins.is_call() {
            return false;
        }
        if self.noreturn_calls.contains(&ins.addr) {
            return true;
        }
        match ins.direct {
            Some((target, _)) => {
                self.noreturn_functions.contains(&target)
                    || self
                        .image
                        .bytes(target, 6)
                        .filter(|b| b[0] == 0xff && b[1] == 0x25)
                        .and_then(|_| self.image.rd32(target + 2))
                        .is_some_and(|slot| self.noreturn_import(slot))
            }
            None => ins
                .absolute_slot()
                .is_some_and(|slot| self.noreturn_import(slot)),
        }
    }

    fn stops(&self, ins: &Insn) -> bool {
        ins.ends_flow() || self.calls_noreturn(ins)
    }

    fn clean_start(&self, target: Addr) -> bool {
        self.clean_pointer_start(target) || (target % 16 == 0 && self.image.is_exec(target))
    }

    fn clean_pointer_start(&self, target: Addr) -> bool {
        self.image.is_exec(target)
            && (matches!(self.image.rd8(target.wrapping_sub(1)), Some(0xcc | 0x90))
                || self.mapped_ends.contains(&target)
                || self
                    .owner_at(target.wrapping_sub(1))
                    .and_then(|owner| self.instructions.get(&owner))
                    .is_some_and(|ins| ins.end() == target && self.stops(ins)))
    }

    fn seed_from(&mut self, source: Addr, target: Addr, kind: &str, rank: u16, root: bool) {
        if !self.image.is_exec(target) {
            return;
        }
        if self.at_known_data(target) {
            if let Some(owner) = self.ghidra_owner(target) {
                if owner != target {
                    self.ghidra_entries.insert(target, owner);
                }
            }
            return;
        }
        self.seed(target, &format!("{kind}:{source:08x}"), rank, root);
    }

    fn seed(&mut self, target: Addr, evidence: &str, rank: u16, root: bool) {
        if !self.image.is_exec(target) || self.suppressed_at(target).is_some() {
            return;
        }
        if root {
            self.roots.insert(target);
        }
        let old_rank = self.seed_rank.get(&target).copied();
        let effective = old_rank.map_or(rank, |old| old.max(rank));
        let mut first = old_rank.is_none() || effective > old_rank.unwrap_or(0);
        self.seeds
            .entry(target)
            .or_default()
            .insert(evidence.to_string());
        self.seed_rank.insert(target, effective);
        if effective >= RANK_TRUSTED {
            self.trusted_seeds.insert(target);
        }
        let mut owner = self.owner_at(target);
        while let Some(current) = owner {
            if current == target || self.rank_of(current) >= effective {
                break;
            }
            self.retract(current);
            owner = self.owner_at(target);
            first = true;
        }
        if owner == Some(target) {
            if self.rank_of(target) < effective {
                self.retract(target);
                first = true;
            }
        } else if let Some(current) = owner {
            if effective >= RANK_TRUSTED {
                self.conflicts
                    .insert((target, current, evidence.to_string()));
            } else if effective < self.rank_of(current) {
                self.discard_seed(
                    target,
                    &format!("covered by {current:08x} rank {}", self.rank_of(current)),
                );
            }
            return;
        }
        if first {
            self.pending.push_back(target);
        }
    }

    fn discard_seed(&mut self, target: Addr, reason: &str) {
        let Some(evidence) = self.seeds.remove(&target) else {
            return;
        };
        self.seed_rank.remove(&target);
        self.trusted_seeds.remove(&target);
        self.roots.remove(&target);
        self.discarded_seeds
            .push((target, reason.to_string(), evidence.into_iter().collect()));
    }

    fn retract(&mut self, address: Addr) {
        self.speculative.remove(&address);
        self.instruction_rank.remove(&address);
        let Some(ins) = self.instructions.remove(&address) else {
            return;
        };
        for at in address..ins.end() {
            if let Some(off) = self.offset(at) {
                if self.covered[off] == address {
                    self.covered[off] = 0;
                }
            }
        }
    }

    fn pointer_run_at(&self, at: Addr) -> bool {
        (0..2).all(|step| {
            let slot = at + step * 4;
            self.reloc_slots.contains(&slot)
                && self
                    .image
                    .rd32(slot)
                    .is_some_and(|target| self.image.is_exec(target))
        })
    }

    fn record(&mut self, ins: &Insn, rank: u16, flow: bool) -> bool {
        if self.pointer_run_at(ins.addr) {
            return false;
        }
        let speculative = rank < RANK_TRUSTED;
        let end = ins.end();
        if (ins.addr..end).any(|at| self.suppressed_at(at).is_some()) {
            return false;
        }
        if (ins.addr..end).any(|at| self.at_known_data(at) || self.in_data(at)) {
            return false;
        }
        let mut retract = BTreeSet::new();
        for at in ins.addr..end {
            if let Some(owner) = self.owner_at(at) {
                if owner != ins.addr {
                    let owner_rank = self.rank_of(owner);
                    if owner_rank > rank
                        || (owner_rank == rank && !(flow && self.speculative.contains(&owner)))
                    {
                        if rank >= RANK_TRUSTED {
                            self.conflicts
                                .insert((ins.addr, owner, "overlapping decode".into()));
                        }
                        return false;
                    }
                    retract.insert(owner);
                }
            }
            if at != ins.addr {
                if let Some(seed_rank) = self.seed_rank.get(&at).copied() {
                    if seed_rank >= rank {
                        if rank >= RANK_TRUSTED {
                            let evidence = self
                                .seeds
                                .get(&at)
                                .map(|v| v.iter().cloned().collect::<Vec<_>>().join("; "))
                                .unwrap_or_default();
                            self.conflicts.insert((at, ins.addr, evidence));
                        }
                        return false;
                    }
                    self.discard_seed(at, &format!("covered by {:08x} rank {rank}", ins.addr));
                }
            }
        }
        for owner in retract {
            let keep = self.instructions.contains_key(&owner)
                && (self.rank_of(owner) < rank || (flow && self.speculative.contains(&owner)));
            if keep {
                self.retract(owner);
            }
        }
        self.instructions.insert(ins.addr, ins.clone());
        for at in ins.addr..end {
            if let Some(off) = self.offset(at) {
                self.covered[off] = ins.addr;
            }
        }
        self.instruction_rank.insert(ins.addr, rank);
        if speculative {
            self.speculative.insert(ins.addr);
        }
        if let Some((target, call)) = ins.flow_target() {
            self.seed_from(
                ins.addr,
                target,
                "direct",
                rank,
                call && rank >= RANK_MAPPED_FLOW,
            );
        }
        if let Some(target) = ins.pointer_immediate().filter(|&t| self.clean_start(t)) {
            self.seed_from(
                ins.addr,
                target,
                "immediate",
                rank.min(RANK_IMMEDIATE),
                true,
            );
        }
        if let Some((index, table)) = ins.switch_jump() {
            let previous = self.previous_instructions(ins.addr);
            if let Some(switch) = self.switch_extent(&previous, index, table, None) {
                self.register_switch(ins.addr, switch, rank);
            }
        }
        true
    }

    fn previous_instructions(&self, at: Addr) -> Vec<Insn> {
        let mut out = Vec::new();
        let mut next = at;
        for (&addr, ins) in self.instructions.range(..at).rev().take(SWITCH_LOOKBACK) {
            if ins.end() != next {
                break;
            }
            out.push(ins.clone());
            next = addr;
        }
        out
    }

    fn switch_extent(
        &self,
        previous: &[Insn],
        index: Register,
        table: Addr,
        known: Option<&BTreeSet<Addr>>,
    ) -> Option<Switch> {
        if !self.image.is_exec(table) {
            return None;
        }
        let mut index = index;
        let mut index_table = None;
        let mut bound = None;
        let mut inclusive = true;
        for ins in previous {
            match (ins.mnemonic, ins.ops[0], ins.ops[1]) {
                (Mnemonic::Movzx | Mnemonic::Mov, SimpleOp::Reg(dest), SimpleOp::Mem(m))
                    if index_table.is_none()
                        && dest.full_register() == index
                        && m.size == 1
                        && m.scale <= 1 =>
                {
                    let register = match (m.base, m.index) {
                        (base, Register::None) if base != Register::None => base,
                        (Register::None, index) if index != Register::None => index,
                        _ => return None,
                    };
                    index_table = Some(m.displacement);
                    index = register.full_register();
                }
                (Mnemonic::Cmp, SimpleOp::Reg(left), SimpleOp::Imm(right)) => {
                    if left.full_register() == index {
                        bound = Some(right);
                    }
                    break;
                }
                (Mnemonic::Jae | Mnemonic::Jb, _, _) => inclusive = false,
                (Mnemonic::Ja | Mnemonic::Jbe, _, _) => inclusive = true,
                _ if ins.flow != FlowControl::Next || ins.writes(index) => break,
                _ => {}
            }
        }
        let cases = bound.map(|bound| {
            if inclusive {
                bound.saturating_add(1)
            } else {
                bound
            }
        });
        let slots = match (cases, index_table) {
            (Some(cases), Some(bytes)) => {
                let values = self.image.bytes(bytes, cases.min(SWITCH_SLOTS) as usize)?;
                values.iter().map(|&v| v as u32 + 1).max()?
            }
            (Some(cases), None) => cases,
            (None, Some(_)) => return None,
            (None, None) => {
                let fits = |value: Addr| match known {
                    Some(targets) => targets.contains(&value),
                    None => {
                        self.image.is_exec(value) && value < table && table - value < SWITCH_REACH
                    }
                };
                let mut count = 0;
                while count < SWITCH_SLOTS && self.image.rd32(table + count * 4).is_some_and(fits) {
                    count += 1;
                }
                if count < 2 && known.is_none() {
                    return None;
                }
                count
            }
        };
        if slots == 0 || slots > SWITCH_SLOTS {
            return None;
        }
        if !(0..slots).all(|i| {
            self.image
                .rd32(table + i * 4)
                .is_some_and(|t| self.image.is_exec(t))
        }) {
            return None;
        }
        if let Some(targets) = known {
            if !(0..slots).all(|i| {
                self.image
                    .rd32(table + i * 4)
                    .is_some_and(|t| targets.contains(&t))
            }) {
                return self.switch_extent(&[], index, table, known);
            }
        }
        Some(Switch {
            table,
            slots,
            index_table: index_table.zip(cases),
        })
    }

    fn claim_data(&mut self, lo: Addr, hi: Addr, kind: &'static str, rank: u16) -> bool {
        let mut owners = BTreeSet::new();
        for at in lo..hi {
            if self.at_known_data(at) || !self.image.is_exec(at) {
                return false;
            }
            if let Some(owner) = self.owner_at(at) {
                if self.rank_of(owner) >= rank {
                    self.conflicts.insert((lo, owner, kind.into()));
                    return false;
                }
                owners.insert(owner);
            }
        }
        for owner in owners {
            self.retract(owner);
        }
        self.add_data(lo, hi, kind);
        true
    }

    fn register_switch(&mut self, site: Addr, switch: Switch, rank: u16) {
        if self.switches.contains_key(&site) {
            return;
        }
        let end = switch.table + switch.slots * 4;
        if !self.claim_data(switch.table, end, "switch table", rank) {
            return;
        }
        if let Some((bytes, count)) = switch.index_table {
            self.claim_data(bytes, bytes + count, "switch index table", rank);
        }
        let targets: BTreeSet<Addr> = (0..switch.slots)
            .filter_map(|i| self.image.rd32(switch.table + i * 4))
            .collect();
        self.switches
            .insert(site, (switch.table, end, targets.iter().copied().collect()));
        for target in targets {
            self.seed_from(site, target, "switch", rank.min(RANK_MAPPED_FLOW), false);
        }
    }

    fn trace(&mut self) {
        while let Some(at) = self.pending.pop_front() {
            if !self.seeds.contains_key(&at) {
                continue;
            }
            let rank = self.seed_rank.get(&at).copied().unwrap_or(RANK_SWEEP);
            let speculative = rank < RANK_TRUSTED;
            let mut at = at;
            while self.image.is_exec(at) {
                if self.suppressed_at(at).is_some() {
                    break;
                }
                if self.at_known_data(at) {
                    if let Some(owner) = self.ghidra_owner(at) {
                        if owner != at {
                            self.ghidra_entries.insert(at, owner);
                        }
                    }
                    break;
                }
                if let Some(owner) = self.owner_at(at) {
                    let owner_rank = self.rank_of(owner);
                    if rank > owner_rank
                        || (rank == owner_rank && owner != at && self.speculative.contains(&owner))
                    {
                        self.retract(owner);
                        continue;
                    }
                    if owner != at && !speculative {
                        self.conflicts.insert((at, owner, "entry alignment".into()));
                    }
                    break;
                }
                let Some(ins) = self.decode_at(at) else {
                    break;
                };
                if !self.record(&ins, rank, true) || self.stops(&ins) {
                    break;
                }
                at = ins.end();
            }
        }
    }

    fn map_lengths(&mut self) -> Result<()> {
        let missing: Vec<(Addr, Addr)> = self
            .ghidra_spans
            .iter()
            .filter(|(start, _, _)| !self.ghidra_lengths.contains_key(start))
            .map(|&(start, end, _)| (start, end))
            .collect();
        let image = self.image;
        let check = self.check;
        let computed: Vec<(Addr, Result<Vec<u8>>)> = missing
            .par_iter()
            .map(|&(start, end)| (start, span_lengths(image, check, start, end)))
            .collect();
        for (start, lengths) in computed {
            self.ghidra_lengths.insert(start, lengths?);
        }
        for &(start, _, _) in &self.ghidra_spans {
            let mut at = start;
            for &length in &self.ghidra_lengths[&start] {
                self.mapped_starts.insert(at);
                at += length as u32;
            }
        }
        Ok(())
    }

    fn initial_seeds(&mut self) {
        self.seed(self.details.entry_point, "PE entry", RANK_TRUSTED, true);
        let exports = self.details.exports.clone();
        for addr in exports {
            self.seed(addr, "export", RANK_TRUSTED, true);
        }
        let relocations = self.image.relocations.clone();
        for r in &relocations {
            if !self.image.is_exec(r.site) && !self.at_known_data(r.target) {
                self.seed(
                    r.target,
                    &format!("relocation:{:08x}", r.site),
                    RANK_RELOCATION,
                    false,
                );
            }
        }
        if let Some(mut at) = self.details.tls_callbacks {
            while let Some(target) = self.image.rd32(at) {
                if target == 0 {
                    break;
                }
                self.seed(target, &format!("TLS:{at:08x}"), RANK_TRUSTED, true);
                at = at.wrapping_add(4);
            }
        }
        self.trace();
        for r in &relocations {
            let Some(owner) = self.owner_at(r.site) else {
                continue;
            };
            let Some(ins) = self.instructions.get(&owner) else {
                continue;
            };
            let direct = ins.imm4.is_some_and(|(_, value)| value == r.target);
            if direct && !self.at_known_data(r.target) {
                let rank = if ins.seeds_flow() {
                    RANK_RELOCATED_FLOW
                } else {
                    RANK_RELOCATED_IMMEDIATE
                };
                self.seed(
                    r.target,
                    &format!("relocated-immediate:{owner:08x}"),
                    rank,
                    false,
                );
            }
        }
        self.trace();
    }

    fn observed_seeds(&mut self, settings: &Settings) -> Result<()> {
        let mut observed = Vec::new();
        if let Some(path) = &settings.analysis_facts {
            let facts = AnalysisFacts::load(path, &settings.sha256)?;
            for (&site, row) in &facts.sites {
                if let Some(targets) = &row.targets {
                    for &target in targets {
                        observed.push((site, target, row.kind == TransferKind::Call));
                    }
                }
            }
        }
        if let Some(path) = &settings.indirect_targets {
            for fields in rows(path)?.iter().skip(2) {
                let [site, kind, targets] = &fields[..] else {
                    return Err(Error::input(format!(
                        "{}: invalid indirect target row",
                        path.display()
                    )));
                };
                for target in targets.split(',') {
                    observed.push((address(path, site)?, address(path, target)?, kind == "call"));
                }
            }
        }
        if let Some(path) = &settings.discovery_evidence {
            for fields in rows(path)?.iter().filter(|f| !f[0].starts_with('#')) {
                let [target, kind, site, _] = &fields[..] else {
                    return Err(Error::input(format!(
                        "{}: invalid discovery row",
                        path.display()
                    )));
                };
                observed.push((address(path, site)?, address(path, target)?, kind == "call"));
            }
        }
        for (site, target, call) in observed {
            self.seed_from(site, target, "observed", RANK_OBSERVED, call);
        }
        self.trace();
        Ok(())
    }

    fn mapped_flow_seeds(&mut self) {
        let mut instructions = Vec::new();
        for &(start, _, _) in &self.ghidra_spans {
            let mut at = start;
            for &size in &self.ghidra_lengths[&start] {
                instructions.push((at, size));
                at += size as u32;
            }
        }
        instructions.sort_unstable();
        let image = self.image;
        let check = self.check;
        let decoded: Vec<Option<Insn>> = instructions
            .par_iter()
            .map(|&(at, size)| decode_at(image, check, at).filter(|ins| ins.size == size))
            .collect();
        for (position, ins) in decoded.iter().enumerate() {
            let Some(ins) = ins else {
                continue;
            };
            if let Some((target, call)) = ins.flow_target() {
                if !self.at_known_data(target) {
                    self.seed_from(ins.addr, target, "mapped-flow", RANK_MAPPED_FLOW, call);
                }
            }
            if let Some(target) = ins.pointer_immediate() {
                if !self.at_known_data(target) && self.clean_start(target) {
                    self.seed_from(ins.addr, target, "immediate", RANK_IMMEDIATE, true);
                }
            }
            if let Some((index, table)) = ins.switch_jump() {
                let mut previous = Vec::new();
                let mut next = ins.addr;
                for earlier in decoded[..position].iter().rev().take(SWITCH_LOOKBACK) {
                    match earlier {
                        Some(earlier) if earlier.end() == next => {
                            next = earlier.addr;
                            previous.push(earlier.clone());
                        }
                        _ => break,
                    }
                }
                let known = self.mapped_tables.get(&ins.addr).cloned();
                if let Some(switch) = self.switch_extent(&previous, index, table, known.as_ref()) {
                    self.register_switch(ins.addr, switch, RANK_MAPPED_FLOW);
                }
            }
        }
        self.trace();
    }

    fn pointer_tables(&mut self) {
        let base = self.image.base;
        let size = self.image.size as usize;
        let mut executable = vec![0u8; size];
        for &(low, high) in &self.image.exec_ranges {
            let lo = low.saturating_sub(base) as usize;
            let hi = high.saturating_sub(base) as usize;
            if lo < size {
                executable[lo..hi.min(size)].fill(1);
            }
        }
        let sections = self.details.sections.clone();
        for section in &sections {
            if section.characteristics & 0x4000_0000 == 0 {
                continue;
            }
            let executable_section = section.characteristics & 0x2000_0000 != 0;
            let start = base + section.virtual_address;
            let end = start + section.raw_size;
            let lo = start.saturating_sub(base) as usize;
            let hi = (end.saturating_sub(base) as usize).min(size);
            if lo >= hi {
                continue;
            }
            let data = &self.image.data[lo..hi];
            let mut candidates = BTreeMap::new();
            for alignment in 0..4 {
                let stop = alignment + (data.len().saturating_sub(alignment)) / 4 * 4;
                let mut offset = alignment;
                while offset + 4 <= stop {
                    let target = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                    let image_offset = lo + offset;
                    let absolute = base + image_offset as u32;
                    offset += 4;
                    let Some(relative) = target.checked_sub(base) else {
                        continue;
                    };
                    if relative as usize >= size || executable[relative as usize] == 0 {
                        continue;
                    }
                    if executable_section && !self.reloc_slots.contains(&absolute) {
                        continue;
                    }
                    if self.covered[image_offset..image_offset + 4]
                        .iter()
                        .any(|&b| b != 0)
                    {
                        continue;
                    }
                    let original = self
                        .details
                        .original
                        .get(image_offset..image_offset + 4)
                        .map(|b| {
                            u32::from_le_bytes(b.try_into().unwrap()).wrapping_add(self.delta)
                        });
                    if original == Some(target) {
                        candidates.insert(absolute, target);
                    }
                }
            }
            let mut next_at = start;
            for &site in candidates.keys() {
                if site < next_at {
                    continue;
                }
                let mut at = site;
                let mut slots = Vec::new();
                while let Some(&target) = candidates.get(&at) {
                    slots.push((at, target));
                    at += 4;
                }
                if slots.len() >= 2 && at <= end {
                    self.tables.push((slots[0].0, at));
                    for (slot, target) in slots {
                        if !self.at_known_data(target) && self.pointer_source(slot, target) {
                            let (rank, root) = if self.clean_pointer_start(target) {
                                (RANK_POINTER, true)
                            } else {
                                (RANK_INTERIOR_POINTER, false)
                            };
                            self.seed(target, &format!("pointer-run:{slot:08x}"), rank, root);
                        }
                    }
                    next_at = at;
                } else {
                    let (slot, target) = slots[0];
                    let single = !executable_section
                        && slot % 4 == 0
                        && !self.at_known_data(target)
                        && self.clean_start(target)
                        && self.pointer_source(slot, target);
                    if single {
                        self.seed(
                            target,
                            &format!("pointer:{slot:08x}"),
                            RANK_SINGLE_POINTER,
                            true,
                        );
                    }
                    next_at = slot + 1;
                }
            }
        }
        self.trace();
    }

    fn pointer_source(&self, slot: Addr, target: Addr) -> bool {
        let index = self
            .ghidra_spans
            .partition_point(|&(start, _, _)| start <= slot + 3)
            .saturating_sub(1);
        let Some(&(start, end, _)) = self.ghidra_spans.get(index) else {
            return true;
        };
        if end <= slot {
            return true;
        }
        if slot < start || slot + 4 > end {
            return false;
        }
        let mut at = start;
        for &length in &self.ghidra_lengths[&start] {
            let size = length as u32;
            if at + size > slot {
                let Some(ins) = self.decode_at(at) else {
                    return false;
                };
                if slot + 4 > at + size {
                    return false;
                }
                let field = slot - at;
                let immediate = ins.imm4 == Some((field as u8, target));
                let displacement = ins.disp4 == Some((field as u8, target));
                return immediate || displacement;
            }
            at += size;
        }
        false
    }

    fn data_containing(&self, at: Addr) -> Option<(Addr, Addr, &'static str)> {
        let index = self
            .offset(at)
            .map(|off| self.data_owner[off])
            .filter(|&i| i >= 0)?;
        Some(self.data_ranges[index as usize])
    }

    fn first_data_start(&self, at: Addr, size: u8) -> Option<Addr> {
        (1..size as u32)
            .map(|step| at + step)
            .find(|&inner| self.in_data(inner) || self.at_known_data(inner))
    }

    fn sweep(&mut self) {
        let tables: BTreeMap<Addr, Addr> = self.tables.iter().copied().collect();
        let table_starts: Vec<Addr> = tables.keys().copied().collect();
        let ranges = self.image.exec_ranges.clone();
        for (start, end) in ranges {
            let mut at = start;
            while at < end {
                if let Some((_, skip)) = self.suppressed_at(at) {
                    at = skip.min(end);
                    continue;
                }
                if let Some(owner) = self.owner_at(at) {
                    let size = self
                        .instructions
                        .get(&owner)
                        .map_or(1, |ins| ins.size as u32);
                    at = owner + size;
                    continue;
                }
                if let Some((_, hi, _)) = self.mapped_span_at(at) {
                    at = hi.min(end);
                    continue;
                }
                if let Some((_, hi, kind)) = self.data_containing(at) {
                    self.gaps.push((at, hi.min(end), kind));
                    at = hi.min(end);
                    continue;
                }
                if let Some(&table_end) = tables.get(&at) {
                    self.gaps.push((at, table_end, "pointer-table"));
                    at = table_end;
                    continue;
                }
                let Some(ins) = self.decode_at(at) else {
                    self.gaps.push((at, at + 1, "undecodable"));
                    at += 1;
                    continue;
                };
                let size = ins.size as u32;
                let next_table = table_starts.partition_point(|&table| table <= at);
                let table_start = table_starts
                    .get(next_table)
                    .copied()
                    .filter(|&table| table < at + size);
                let data_start = table_start.or_else(|| self.first_data_start(at, ins.size));
                if let Some(data_start) = data_start {
                    self.gaps.push((at, data_start, "data-boundary"));
                    at = data_start;
                    continue;
                }
                let rank = RANK_SWEEP.max(self.seed_rank.get(&at).copied().unwrap_or(0));
                let blocked = (at + 1..at + size).any(|inner| {
                    self.owner_at(inner)
                        .is_some_and(|owner| self.rank_of(owner) >= rank)
                        || self
                            .seed_rank
                            .get(&inner)
                            .is_some_and(|&rank| rank >= RANK_SWEEP)
                });
                if blocked {
                    self.gaps.push((at, at + 1, "sweep-padding"));
                    at += 1;
                    continue;
                }
                if !self.record(&ins, rank, false) {
                    at += 1;
                    continue;
                }
                self.trace();
                at += size;
            }
        }
    }

    fn finish(&mut self) {
        for (&at, ins) in &self.instructions {
            if !self.speculative.contains(&at) {
                if let Some((target, _)) = ins.flow_target() {
                    if !self.instructions.contains_key(&target) {
                        self.traps.insert(
                            at,
                            format!("direct transfer to unmapped instruction {target:08x}"),
                        );
                    }
                }
            }
        }
        for (&target, evidence) in &self.seeds {
            if self.trusted_seeds.contains(&target)
                && !self.instructions.contains_key(&target)
                && !self.at_known_data(target)
            {
                let owner = self.owner_at(target).unwrap_or(target);
                let joined = evidence.iter().cloned().collect::<Vec<_>>().join("; ");
                self.conflicts.insert((target, owner, joined));
            }
        }
    }

    fn code_map_rows(&self) -> Result<(Functions, Entries)> {
        let mut roots = self.roots.clone();
        for ins in self.instructions.values() {
            if let Some((target, true)) = ins.flow_target() {
                if self.instructions.contains_key(&target) {
                    roots.insert(target);
                }
            }
        }
        let mut groups: BTreeMap<Addr, Vec<(Addr, u8)>> = BTreeMap::new();
        let mut owner: Option<Addr> = None;
        let mut previous: Option<(Addr, bool)> = None;
        let mut targets = BTreeSet::new();
        for (&at, ins) in &self.instructions {
            let falls = previous.is_some_and(|(end, stops)| end == at && !stops);
            if !falls && !self.seeds.contains_key(&at) && ins.padding {
                previous = None;
                continue;
            }
            let joins = owner.is_some_and(|current| {
                falls
                    || (!roots.contains(&at) && targets.contains(&at) && at < current + BODY_BYTES)
            });
            if !joins {
                owner = Some(at);
                targets.clear();
            }
            let current = owner.unwrap();
            if let Some((target, false)) = ins.flow_target() {
                targets.insert(target);
            }
            if let Some((_, _, cases)) = self.switches.get(&at) {
                targets.extend(cases);
            }
            groups.entry(current).or_default().push((at, ins.size));
            previous = Some((ins.end(), self.stops(ins)));
        }
        let mut functions = BTreeMap::new();
        let mut entries = BTreeMap::new();
        for (owner, group) in groups {
            if !self.instructions.contains_key(&owner) {
                return Err(Error::input(format!(
                    "function {} has no instruction at its entry",
                    Hex(owner)
                )));
            }
            let mut runs: Vec<(Addr, Vec<u8>)> = Vec::new();
            for (at, size) in group {
                let extend = runs.last().is_some_and(|(start, lengths)| {
                    at == *start + lengths.iter().map(|&n| n as u32).sum::<u32>()
                });
                if !extend {
                    runs.push((at, Vec::new()));
                }
                runs.last_mut().unwrap().1.push(size);
                if at != owner {
                    entries.insert(at, (owner, InteriorKind::Branch));
                }
            }
            functions.insert(
                owner,
                runs.into_iter()
                    .map(|(start, lengths)| Span {
                        start,
                        size: lengths.iter().map(|&n| n as u32).sum(),
                        lengths: Some(lengths),
                    })
                    .collect(),
            );
        }
        for (&target, &owner) in &self.ghidra_entries {
            entries
                .entry(target)
                .or_insert((owner, InteriorKind::Branch));
        }
        Ok((functions, entries))
    }

    fn mapped_span_at(&self, at: Addr) -> Option<(Addr, Addr, Addr)> {
        let index = self
            .ghidra_spans
            .partition_point(|&(start, _, _)| start <= at)
            .checked_sub(1)?;
        let &(start, end, owner) = self.ghidra_spans.get(index)?;
        (at < end).then_some((start, end, owner))
    }

    fn spans_are_padding(&self, owner: Addr, spans: &[Span]) -> bool {
        if self.mapped_span_at(owner).is_some()
            || self
                .seed_rank
                .get(&owner)
                .is_some_and(|&rank| rank > RANK_POINTER)
            || self.known_entries.contains(&owner)
        {
            return false;
        }
        let mut any = false;
        for span in spans {
            let mut at = span.start;
            for &length in span.lengths.iter().flatten() {
                any = true;
                let Some(ins) = self.instructions.get(&at) else {
                    return false;
                };
                if !ins.padding {
                    return false;
                }
                at += length as u32;
            }
        }
        any
    }

    fn entropy_data(&self, owner: Addr, spans: &[Span]) -> bool {
        if self.mapped_span_at(owner).is_some()
            || self.known_entries.contains(&owner)
            || self.trusted_seeds.contains(&owner)
        {
            return false;
        }
        let size: Addr = spans.iter().map(|span| span.size).sum();
        if size < DATA_ENTROPY_MIN_BYTES {
            return false;
        }
        let mut counts = [0usize; 256];
        let mut total = 0usize;
        let mut zeros = 0usize;
        for span in spans {
            let Some(bytes) = self.image.bytes(span.start, span.size as usize) else {
                return false;
            };
            for &byte in bytes {
                counts[byte as usize] += 1;
                total += 1;
                zeros += usize::from(byte == 0);
            }
        }
        if total == 0 {
            return false;
        }
        let total = total as f64;
        let entropy = counts
            .iter()
            .filter(|&&count| count > 0)
            .map(|&count| {
                let probability = count as f64 / total;
                -probability * probability.log2()
            })
            .sum::<f64>();
        entropy < DATA_ENTROPY_MAX && zeros as f64 / total > DATA_ZERO_RATIO_MIN
    }

    fn alignment_call_stub(&self, owner: Addr, spans: &[Span]) -> bool {
        if self.mapped_span_at(owner).is_some()
            || self.seeds.contains_key(&owner)
            || self.known_entries.contains(&owner)
        {
            return false;
        }
        let [span] = spans else {
            return false;
        };
        let [length] = span.lengths.as_deref().unwrap_or_default() else {
            return false;
        };
        if !(SHORT_FRAGMENT_MIN..=SHORT_FRAGMENT_MAX).contains(&span.size) {
            return false;
        }
        let Some(ins) = self.instructions.get(&span.start) else {
            return false;
        };
        if !ins.is_call() || ins.size != *length {
            return false;
        }
        let end = span.end();
        if end % 16 != 0 {
            return false;
        }
        let next = self
            .ghidra_spans
            .partition_point(|&(start, _, _)| start < end);
        let Some(&(next_start, _, _)) = self.ghidra_spans.get(next) else {
            return false;
        };
        if next_start != end {
            return false;
        }
        let previous = self
            .ghidra_spans
            .partition_point(|&(start, _, _)| start < span.start);
        let Some(&(_, previous_end, _)) = previous
            .checked_sub(1)
            .and_then(|index| self.ghidra_spans.get(index))
        else {
            return false;
        };
        previous_end == span.start
    }

    fn short_signature(&self, spans: &[Span]) -> Option<String> {
        let mut key = String::new();
        for span in spans {
            let mut at = span.start;
            for &length in span.lengths.as_ref()? {
                let ins = self.instructions.get(&at)?;
                key.push_str(&ins.signature());
                key.push(';');
                at += length as u32;
            }
        }
        Some(key)
    }

    fn discard_fragments(
        &mut self,
        functions: Functions,
        entries: Entries,
    ) -> (Functions, Entries, Vec<(Addr, Addr)>) {
        let mut signatures = BTreeMap::new();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for (&owner, spans) in &functions {
            let size: Addr = spans.iter().map(|span| span.size).sum();
            if !(SHORT_FRAGMENT_MIN..=SHORT_FRAGMENT_MAX).contains(&size) {
                continue;
            }
            if self.mapped_span_at(owner).is_some()
                || self.seeds.contains_key(&owner)
                || self.known_entries.contains(&owner)
            {
                continue;
            }
            let Some(signature) = self.short_signature(spans) else {
                continue;
            };
            *counts.entry(signature.clone()).or_default() += 1;
            signatures.insert(owner, signature);
        }
        let mut kept = BTreeMap::new();
        let mut discarded = Vec::new();
        for (&owner, spans) in &functions {
            let reason = if self.spans_are_padding(owner, spans) {
                Some(FunctionReject::PaddingOnly)
            } else if signatures.get(&owner).is_some_and(|signature| {
                counts.get(signature).copied().unwrap_or(0) > SHORT_FRAGMENT_REPEAT_LIMIT
            }) {
                Some(FunctionReject::RepeatedShortFragment)
            } else if self.alignment_call_stub(owner, spans) {
                Some(FunctionReject::AlignmentCallStub)
            } else if self.entropy_data(owner, spans) {
                Some(FunctionReject::DataTable)
            } else {
                None
            };
            match reason {
                Some(reason) => {
                    self.function_rejections.push((owner, reason, self.pass));
                    discarded.push((owner, spans.clone()));
                }
                None => {
                    kept.insert(owner, spans.clone());
                }
            }
        }
        let mut suppressions: Vec<(Addr, Addr)> = discarded
            .iter()
            .flat_map(|(_, spans)| spans.iter().map(|span| (span.start, span.end())))
            .collect();
        let mut discarded_sources = BTreeSet::new();
        for (_, spans) in &discarded {
            for span in spans {
                let mut at = span.start;
                for &length in span.lengths.iter().flatten() {
                    discarded_sources.insert(at);
                    at += length as u32;
                }
            }
        }
        if !discarded_sources.is_empty() {
            let orphans: BTreeSet<Addr> = self
                .seeds
                .iter()
                .filter(|(&target, evidence)| {
                    !evidence.is_empty()
                        && !self.roots.contains(&target)
                        && !self.trusted_seeds.contains(&target)
                        && self.mapped_span_at(target).is_none()
                        && evidence.iter().all(|item| {
                            evidence_source(item)
                                .is_some_and(|source| discarded_sources.contains(&source))
                        })
                })
                .map(|(&target, _)| target)
                .collect();
            for target in orphans {
                let Some(spans) = functions.get(&target) else {
                    continue;
                };
                if discarded.iter().any(|(owner, _)| *owner == target) {
                    continue;
                }
                self.function_rejections.push((
                    target,
                    FunctionReject::OnlySeededByDiscardedCode,
                    self.pass,
                ));
                suppressions.extend(spans.iter().map(|span| (span.start, span.end())));
            }
        }
        (kept, entries, suppressions)
    }

    fn filter(&mut self, functions: Functions, entries: Entries) -> Result<(Functions, Entries)> {
        let mut discovered_spans: Vec<(Addr, Addr, Addr)> = functions
            .iter()
            .flat_map(|(&owner, spans)| {
                spans
                    .iter()
                    .map(move |span| (span.start, span.end(), owner))
            })
            .collect();
        discovered_spans.sort_unstable();
        let discovered_at = |at: Addr| -> Option<Addr> {
            let index = discovered_spans
                .partition_point(|&(start, _, _)| start <= at)
                .checked_sub(1)?;
            let &(_, end, owner) = discovered_spans.get(index)?;
            (at < end).then_some(owner)
        };
        let mut valid_entries = self.known_entries.clone();
        valid_entries.extend(functions.keys().copied());
        for (&addr, &(owner, _)) in &entries {
            let valid = if let Some((_, _, mapped_owner)) = self.mapped_span_at(addr) {
                self.mapped_starts.contains(&addr) && owner == mapped_owner
            } else {
                self.instructions.contains_key(&addr) && discovered_at(addr) == Some(owner)
            };
            if valid {
                valid_entries.insert(addr);
            }
        }
        let mut kept = BTreeMap::new();
        let mut kept_instructions = BTreeSet::new();
        for (&owner, spans) in &functions {
            let mut reason = None;
            if self.mapped_span_at(owner).is_some() {
                if !self.mapped_starts.contains(&owner) {
                    reason = Some(FunctionReject::StartsInsideMappedInstruction);
                } else if !self.known_entries.contains(&owner) {
                    reason = Some(FunctionReject::StartsInMappedFunction);
                }
            }
            if reason.is_none() {
                'spans: for span in spans {
                    let Some(lengths) = &span.lengths else {
                        return Err(Error::input(format!(
                            "{}: discovered span has no lengths",
                            Hex(span.start)
                        )));
                    };
                    let mut at = span.start;
                    for &length in lengths {
                        for byte in at..at + length as u32 {
                            if self.mapped_span_at(byte).is_some() {
                                reason = Some(FunctionReject::OverlapsMappedCode);
                                break 'spans;
                            }
                        }
                        let falls = self
                            .instructions
                            .get(&at)
                            .filter(|ins| !self.stops(ins))
                            .map(Insn::end);
                        if let Some(next) =
                            falls.filter(|&next| self.mapped_span_at(next).is_some())
                        {
                            if !self.mapped_starts.contains(&next) {
                                reason = Some(FunctionReject::FlowsIntoMappedInstruction);
                                break 'spans;
                            }
                            if !valid_entries.contains(&next) {
                                reason = Some(FunctionReject::FlowsToMappedInterior);
                                break 'spans;
                            }
                        }
                        if let Some((target, call)) =
                            self.instructions.get(&at).and_then(Insn::flow_target)
                        {
                            if self.mapped_span_at(target).is_some() {
                                if !self.mapped_starts.contains(&target) {
                                    reason = Some(FunctionReject::FlowsIntoMappedInstruction);
                                    break 'spans;
                                }
                                if !valid_entries.contains(&target) {
                                    reason = Some(FunctionReject::FlowsToMappedInterior);
                                    break 'spans;
                                }
                            } else if call {
                                let entry = valid_entries.contains(&target);
                                if !entry {
                                    if discovered_at(target).is_some() {
                                        reason = Some(FunctionReject::CallsToDiscoveredInterior);
                                    } else {
                                        reason = Some(FunctionReject::CallsToUngroundedTarget);
                                    }
                                    break 'spans;
                                }
                            } else if let Some(other) = discovered_at(target) {
                                if other != owner && !valid_entries.contains(&target) {
                                    reason = Some(FunctionReject::JumpsToDiscoveredInterior);
                                    break 'spans;
                                }
                            }
                        }
                        at += length as u32;
                    }
                }
            }
            if let Some(reason) = reason {
                self.function_rejections.push((owner, reason, self.pass));
            } else {
                self.kept_functions += 1;
                for span in spans {
                    let mut at = span.start;
                    for &length in span.lengths.iter().flatten() {
                        kept_instructions.insert(at);
                        at += length as u32;
                    }
                }
                kept.insert(owner, spans.clone());
            }
        }
        let mut remaining = BTreeMap::new();
        for (addr, (owner, kind)) in entries {
            let reason = if self.mapped_span_at(addr).is_some() {
                if !self.mapped_starts.contains(&addr) {
                    Some(EntryReject::EntryInsideMappedInstruction)
                } else if !valid_entries.contains(&addr) {
                    Some(EntryReject::EntryInMappedFunction)
                } else {
                    None
                }
            } else if !kept.contains_key(&owner) {
                Some(EntryReject::EntryInRejectedFunction)
            } else if !kept_instructions.contains(&addr) {
                Some(EntryReject::EntryNotInFunction)
            } else {
                None
            };
            match reason {
                Some(reason) => self.entry_rejections.push((addr, reason)),
                None => {
                    self.kept_entries += 1;
                    remaining.insert(addr, (owner, kind));
                }
            }
        }
        Ok((kept, remaining))
    }

    fn report(&self) -> DiscoveryReport {
        let run = |&(start, end): &(Addr, Addr)| PointerRun {
            start: Hex(start).to_string(),
            end: Hex(end).to_string(),
        };
        let pointer_runs = self.tables.iter().map(run).collect();
        let switch_tables = self
            .switches
            .values()
            .map(|&(start, end, _)| run(&(start, end)))
            .collect();
        let gaps = self
            .gaps
            .iter()
            .map(|&(start, end, kind)| Gap {
                start: Hex(start).to_string(),
                end: Hex(end).to_string(),
                kind,
            })
            .collect();
        let traps = self
            .traps
            .iter()
            .map(|(&at, reason)| (Hex(at).to_string(), reason.clone()))
            .collect();
        let conflicts = self
            .conflicts
            .iter()
            .map(|&(target, instruction, ref evidence)| Conflict {
                target: Hex(target).to_string(),
                instruction: Hex(instruction).to_string(),
                evidence: evidence.clone(),
                kind: if self.instructions.contains_key(&target) {
                    "shared"
                } else {
                    "rejected"
                },
            })
            .collect();
        let seeds = self
            .seeds
            .iter()
            .map(|(&at, evidence)| (Hex(at).to_string(), evidence.iter().cloned().collect()))
            .collect();
        let seed_ranks = self
            .seed_rank
            .iter()
            .map(|(&at, &rank)| (Hex(at).to_string(), rank))
            .collect();
        let discarded_seeds = self
            .discarded_seeds
            .iter()
            .map(|(at, reason, evidence)| DiscardedSeed {
                address: Hex(*at).to_string(),
                reason: reason.clone(),
                evidence: evidence.clone(),
            })
            .collect();
        let mut rejection_counts = RejectionCounts::default();
        for (_, reason, _) in &self.function_rejections {
            match reason {
                FunctionReject::StartsInsideMappedInstruction => {
                    rejection_counts.functions_starts_inside_mapped_instruction += 1
                }
                FunctionReject::StartsInMappedFunction => {
                    rejection_counts.functions_starts_in_mapped_function += 1
                }
                FunctionReject::OverlapsMappedCode => {
                    rejection_counts.functions_overlaps_mapped_code += 1
                }
                FunctionReject::FlowsIntoMappedInstruction => {
                    rejection_counts.functions_flows_into_mapped_instruction += 1
                }
                FunctionReject::FlowsToMappedInterior => {
                    rejection_counts.functions_flows_to_mapped_interior += 1
                }
                FunctionReject::CallsToDiscoveredInterior => {
                    rejection_counts.functions_calls_to_discovered_interior += 1
                }
                FunctionReject::CallsToUngroundedTarget => {
                    rejection_counts.functions_calls_to_ungrounded_target += 1
                }
                FunctionReject::JumpsToDiscoveredInterior => {
                    rejection_counts.functions_jumps_to_discovered_interior += 1
                }
                FunctionReject::PaddingOnly => rejection_counts.functions_padding_only += 1,
                FunctionReject::RepeatedShortFragment => {
                    rejection_counts.functions_repeated_short_fragment += 1
                }
                FunctionReject::OnlySeededByDiscardedCode => {
                    rejection_counts.functions_only_seeded_by_discarded_code += 1
                }
                FunctionReject::AlignmentCallStub => {
                    rejection_counts.functions_alignment_call_stub += 1
                }
                FunctionReject::DataTable => rejection_counts.functions_data_table += 1,
            }
        }
        for (_, reason) in &self.entry_rejections {
            match reason {
                EntryReject::EntryInsideMappedInstruction => {
                    rejection_counts.entries_inside_mapped_instruction += 1
                }
                EntryReject::EntryInMappedFunction => {
                    rejection_counts.entries_in_mapped_function += 1
                }
                EntryReject::EntryInRejectedFunction => {
                    rejection_counts.entries_in_rejected_function += 1
                }
                EntryReject::EntryNotInFunction => rejection_counts.entries_not_in_function += 1,
            }
        }
        let function_rejections = self
            .function_rejections
            .iter()
            .map(|&(addr, reason, pass)| FunctionRejection {
                address: Hex(addr).to_string(),
                reason,
                pass,
            })
            .collect();
        let entry_rejections = self
            .entry_rejections
            .iter()
            .map(|&(addr, reason)| EntryRejection {
                address: Hex(addr).to_string(),
                reason,
            })
            .collect();
        DiscoveryReport {
            instructions: self.instructions.len(),
            passes: self.pass,
            converged: false,
            entry_seeds: self.seeds.len(),
            excluded_bytes: self.excluded_bytes,
            discovered_functions: self.kept_functions + self.function_rejections.len(),
            kept_functions: self.kept_functions,
            rejected_functions: function_rejections,
            discovered_entries: self.kept_entries + self.entry_rejections.len(),
            kept_entries: self.kept_entries,
            rejected_entries: entry_rejections,
            rejection_counts,
            pointer_runs,
            switch_tables,
            gaps,
            traps,
            conflicts,
            seeds,
            seed_ranks,
            discarded_seeds,
        }
    }
}

fn rows(path: &Path) -> Result<Vec<Vec<String>>> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            line.split(['\t', ' '])
                .filter(|f| !f.is_empty())
                .map(str::to_string)
                .collect()
        })
        .collect())
}

fn address(path: &Path, text: &str) -> Result<Addr> {
    parse_hex(text)
        .ok_or_else(|| Error::input(format!("{}: invalid address {text:?}", path.display())))
}

fn span_lengths(image: &Image, check: &SleighLengths, start: Addr, end: Addr) -> Result<Vec<u8>> {
    let mut lengths = Vec::new();
    let mut at = start;
    while at < end {
        let ins = decode_between(image, check, at, end).ok_or_else(|| {
            Error::input(format!(
                "{}: cannot recover mapped instruction boundaries",
                Hex(at)
            ))
        })?;
        if at + ins.size as u32 > end {
            return Err(Error::input(format!(
                "{}: cannot recover mapped instruction boundaries",
                Hex(at)
            )));
        }
        lengths.push(ins.size);
        at += ins.size as u32;
    }
    Ok(lengths)
}

fn decode_at(image: &Image, check: &SleighLengths, at: Addr) -> Option<Insn> {
    let end = image
        .exec_ranges
        .iter()
        .find(|&&(lo, hi)| lo <= at && at < hi)
        .map(|&(_, hi)| hi)?;
    decode_between(image, check, at, end)
}

fn decode_between(image: &Image, check: &SleighLengths, at: Addr, end: Addr) -> Option<Insn> {
    let len = 15usize.min(end.saturating_sub(at) as usize);
    let bytes = image.bytes(at, len)?;
    let found = if let Some(mut next) = (decode_one(bytes, at, None)?)
        .mnemonic
        .eq(&Mnemonic::Wait)
        .then(|| decode_one(&bytes[1..], at + 1, Some(1)))
        .flatten()
        .filter(|next| mnemonic_name(next.mnemonic).starts_with("fn"))
    {
        next.addr = at;
        next
    } else {
        decode_one(bytes, at, None)?
    };
    (check.length(image, at)? == found.size).then_some(found)
}

fn decode_one(bytes: &[u8], at: Addr, folded: Option<u8>) -> Option<Insn> {
    let mut decoder = Decoder::with_ip(32, bytes, at as u64, DecoderOptions::NONE);
    let mut insn = Instruction::default();
    decoder.decode_out(&mut insn);
    if insn.is_invalid() || insn.len() == 0 || insn.len() > bytes.len() {
        return None;
    }
    let base = folded.unwrap_or(0);
    let prefix = if folded.is_some() { 1u8 } else { 0u8 };
    let size = insn.len() as u8 + prefix;
    let mut ops = [SimpleOp::None; 4];
    let op_count = insn.op_count().min(4) as u8;
    for (index, op) in ops.iter_mut().enumerate().take(op_count as usize) {
        *op = match insn.op_kind(index as u32) {
            OpKind::Register => SimpleOp::Reg(insn.op_register(index as u32)),
            OpKind::Memory => SimpleOp::Mem(Mem {
                base: insn.memory_base(),
                index: insn.memory_index(),
                scale: insn.memory_index_scale(),
                displacement: insn.memory_displacement64() as u32,
                size: insn.memory_size().size() as u32,
            }),
            OpKind::Immediate8
            | OpKind::Immediate8_2nd
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate64
            | OpKind::Immediate8to16
            | OpKind::Immediate8to32
            | OpKind::Immediate8to64
            | OpKind::Immediate32to64 => SimpleOp::Imm(insn.immediate(index as u32) as u32),
            _ => SimpleOp::None,
        };
    }
    let constant = decoder.get_constant_offsets(&insn);
    let imm4 = (constant.immediate_size() == 4).then(|| {
        (
            constant.immediate_offset().wrapping_add(base as usize) as u8,
            insn.immediate(0) as u32,
        )
    });
    let disp4 = (constant.displacement_size() == 4).then(|| {
        (
            constant.displacement_offset().wrapping_add(base as usize) as u8,
            insn.memory_displacement64() as u32,
        )
    });
    let flow = insn.flow_control();
    let direct = if matches!(
        flow,
        FlowControl::Call | FlowControl::ConditionalBranch | FlowControl::UnconditionalBranch
    ) && matches!(
        insn.op0_kind(),
        OpKind::NearBranch16 | OpKind::NearBranch32 | OpKind::NearBranch64
    ) {
        Some((insn.near_branch_target() as u32, flow == FlowControl::Call))
    } else {
        None
    };
    Some(Insn {
        addr: at,
        size,
        flow,
        mnemonic: insn.mnemonic(),
        ops,
        op_count,
        direct,
        imm4,
        disp4,
        padding: bytes[..insn.len()].iter().all(|&b| b == 0x90 || b == 0xcc),
    })
}

fn mnemonic_name(mnemonic: Mnemonic) -> String {
    format!("{mnemonic:?}").to_lowercase()
}

fn evidence_source(evidence: &str) -> Option<Addr> {
    parse_hex(evidence.rsplit_once(':')?.1)
}
