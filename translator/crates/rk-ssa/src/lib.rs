//! Byte-lane SSA over `CgFunction`, with range-scoped memory definitions and conservative residual
//! memory. Observable effects retain ordered boundaries carrying the guest state. Calls use
//! `CallFacts` under the optimizing policy and conservative effects otherwise. `prepare`
//! canonicalizes, forwards stores to loads, coalesces whole-register phis and removes dead values;
//! `liveness` and `flags` derive state publications and lazy flag plans.

use std::collections::BTreeMap;

use rk_core::contract::{Field, Fields};
use rk_core::{Addr, Lookup};
use rk_ir::{Op, SlowInsn, StateKey};

pub mod address;
mod build;
mod canonical;
mod coalesce;
mod dump;
mod flags;
mod forward;
mod liveness;
pub mod memory;
mod prepare;
mod stack;
mod validate;
pub use validate::validate;

pub use build::{
    build, BorrowedStack, BuildOptions, CallEffect, CallFacts, CallSummary, ConservativeCallFacts,
    DEFAULT_VALUE_BUDGET,
};
pub use canonical::canonicalize;
pub use dump::dump;
pub use flags::{cc_kind, FlagDescriptor, FlagPlan, FlagPublication, FlagSource};
pub use forward::forward;
pub use liveness::{live_values, simplify};
pub use prepare::{
    prepare, prepare_with_publications, prepare_with_stack_dump, publications, PrepareOptions,
};

pub type ValueId = u32;
pub type BlockId = u32;
pub type FlagId = u32;

pub const DISPATCH_BLOCK: BlockId = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Value {
    pub op: Op,
    pub size: u8,
    pub args_start: u32,
    pub args_len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InsnLoc {
    pub block: BlockId,
    pub position: u32,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub start: u32,
    pub end: u32,
    pub preds: Vec<BlockId>,
    pub phis: Vec<ValueId>,
    pub ops: Vec<ValueId>,
}

#[derive(Clone, Debug)]
pub struct Dispatch {
    pub loads: Vec<(StateKey, ValueId)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub addr: Addr,
    pub target: BlockId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boundary {
    pub at: ValueId,
    pub state_start: u32,
    pub state_len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FlagKind {
    Incoming,
    Add,
    Sub,
    Adc,
    Sbb,
    Cmp,
    Logic,
    Inc,
    Dec,
    Join,
}

#[derive(Clone, Debug)]
pub struct FlagRecord {
    pub kind: FlagKind,
    pub size: u8,
    pub op: Option<ValueId>,
    pub kind_value: Option<ValueId>,
    pub a: Option<ValueId>,
    pub b: Option<ValueId>,
    pub carry: Option<ValueId>,
    pub result: Option<ValueId>,
    pub flags: BTreeMap<Field, ValueId>,
    pub defined: Fields,
    pub guarded: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct LaneGroup {
    pub phi: ValueId,
    pub block: BlockId,
    pub lanes: Vec<LaneGroupLane>,
}

#[derive(Clone, Debug)]
pub(crate) struct LaneGroupLane {
    pub key: usize,
    pub value: ValueId,
    pub args: Vec<ValueId>,
}

pub struct Ssa {
    pub addr: Addr,
    pub values: Vec<Value>,
    pub args: Vec<ValueId>,
    pub blocks: Vec<Block>,
    pub dispatch: Dispatch,
    pub entries: Vec<Entry>,
    pub boundaries: Vec<Boundary>,
    pub boundary_index: Lookup<ValueId, u32>,
    pub state: Vec<(StateKey, ValueId)>,
    pub keys: Vec<StateKey>,
    pub field_of: Vec<Option<Field>>,
    pub slow: Vec<SlowInsn>,
    pub flags: Vec<FlagRecord>,
    pub loc: Vec<Option<InsnLoc>>,
    pub value_addr: Vec<Addr>,
    pub(crate) stack_states: BTreeMap<ValueId, [ValueId; 4]>,
    pub stack_guards: BTreeMap<ValueId, u32>,
    pub(crate) stack_calls: bool,
    pub(crate) memory_partitioned: bool,
    pub(crate) assume_private_stack: bool,
    pub(crate) assume_stack_pointer_bases: bool,
    pub(crate) phi_key: Lookup<ValueId, usize>,
    pub(crate) entry_reads: Vec<Vec<(usize, ValueId)>>,
    pub(crate) exit_state: Vec<Vec<Option<ValueId>>>,
    pub(crate) lane_groups: Vec<LaneGroup>,
}

impl Ssa {
    pub(crate) fn new_value(&mut self, op: Op, size: u8, args: &[ValueId], addr: Addr) -> ValueId {
        let id = self.values.len() as ValueId;
        let start = self.args.len() as u32;
        self.args.extend_from_slice(args);
        self.values.push(Value {
            op,
            size,
            args_start: start,
            args_len: args.len() as u32,
        });
        self.value_addr.push(addr);
        id
    }

    pub(crate) fn constant(
        &mut self,
        cache: &mut BTreeMap<(u8, u64), ValueId>,
        size: u8,
        data: u64,
    ) -> ValueId {
        let data = data & rk_core::mask(size as u32);
        if let Some(&value) = cache.get(&(size, data)) {
            return value;
        }
        let id = self.new_value(Op::Const(data), size, &[], 0);
        cache.insert((size, data), id);
        id
    }

    pub fn value(&self, value: ValueId) -> &Value {
        &self.values[value as usize]
    }

    pub fn args(&self, value: ValueId) -> &[ValueId] {
        let value = &self.values[value as usize];
        &self.args[value.args_start as usize..(value.args_start + value.args_len) as usize]
    }

    pub fn block(&self, block: BlockId) -> Option<&Block> {
        self.blocks.get(block as usize)
    }

    pub fn block_of_insn(&self, insn: u32) -> Option<InsnLoc> {
        self.loc.get(insn as usize).copied().flatten()
    }

    pub fn boundary(&self, at: ValueId) -> Option<&[(StateKey, ValueId)]> {
        let index = *self.boundary_index.get(&at)? as usize;
        let boundary = &self.boundaries[index];
        Some(
            &self.state[boundary.state_start as usize
                ..(boundary.state_start + boundary.state_len) as usize],
        )
    }

    pub fn input(&self, key: StateKey) -> Option<ValueId> {
        self.dispatch
            .loads
            .iter()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, value)| *value)
    }

    pub fn memory(&self) -> Option<ValueId> {
        self.input(StateKey::Memory)
    }

    pub fn field(&self, key: StateKey) -> Option<Field> {
        let index = self.keys.iter().position(|candidate| *candidate == key)?;
        self.field_of[index]
    }

    pub fn phi_lanes(&self) -> Vec<Vec<(usize, ValueId, Vec<ValueId>)>> {
        let mut blocks = vec![Vec::new(); self.blocks.len()];
        for (block, item) in self.blocks.iter().enumerate() {
            for &phi in &item.phis {
                let Some(&key) = self.phi_key.get(&phi) else {
                    continue;
                };
                blocks[block].push((key, phi, self.args(phi).to_vec()));
            }
        }
        for group in &self.lane_groups {
            for lane in &group.lanes {
                blocks[group.block as usize].push((lane.key, lane.value, lane.args.clone()));
            }
        }
        blocks
    }

    pub(crate) fn successors(&self) -> Vec<Vec<usize>> {
        let mut result = vec![Vec::new(); self.blocks.len()];
        for (block, item) in self.blocks.iter().enumerate() {
            for &pred in &item.preds {
                if pred != DISPATCH_BLOCK {
                    result[pred as usize].push(block);
                }
            }
        }
        result
    }

    pub fn flushed_call(&self, block: BlockId, position: usize) -> Option<ValueId> {
        self.blocks[block as usize].ops[position + 1..]
            .iter()
            .copied()
            .find(|&id| {
                self.value(id).op != Op::StackFlush && !rk_ir::properties(self.value(id).op).pure
            })
            .filter(|call| self.stack_guards.contains_key(call))
    }

    pub fn slow(&self, index: u32) -> Option<&SlowInsn> {
        self.slow.get(index as usize)
    }

    pub fn flag(&self, index: FlagId) -> Option<&FlagRecord> {
        self.flags.get(index as usize)
    }
}
