use std::collections::BTreeMap;
use std::sync::OnceLock;

use rk_core::contract::{Field, Fields};
use rk_core::ir::Policy;
use rk_core::ir::{OpCode, Space, Varnode};
use rk_core::settings::FaultState;
use rk_core::{Addr, Diagnostic, Lookup, Reason};
use rk_ir::{CgFunction, CgOp, FlagShape, Op, Registers, SlowInsn, StateKey};

use crate::canonical::{resolve_alias, rewrite_aliases};
use crate::{
    BlockId, Boundary, Dispatch, Entry, FlagRecord, InsnLoc, Ssa, Value, ValueId, DISPATCH_BLOCK,
};

pub const DEFAULT_VALUE_BUDGET: usize = 40_000_000;

#[derive(Clone, Copy, Debug)]
pub struct CallEffect {
    pub reads: Fields,
    pub kills: Fields,
    pub writes: Fields,
    pub preserves: Fields,
}

impl CallEffect {
    pub const CONSERVATIVE: Self = Self {
        reads: Fields::ALL,
        kills: Fields::ALL,
        writes: Fields::ALL,
        preserves: Fields::NONE,
    };
}

pub trait CallFacts: Sync {
    fn memory(&self, _site: Addr, _target: Option<Addr>) -> Option<&rk_core::memory::Effects> {
        None
    }

    fn call(&self, site: Addr, target: Option<Addr>) -> CallEffect;

    fn stack(&self, _site: Addr, _target: Option<Addr>) -> Option<&StackEffect> {
        None
    }
}

#[derive(Clone, Debug)]
pub struct StackEffect {
    pub cleanup: i32,
    pub writes: Vec<(i32, u32)>,
}

pub struct ConservativeCallFacts;
impl CallFacts for ConservativeCallFacts {
    fn call(&self, _site: Addr, _target: Option<Addr>) -> CallEffect {
        CallEffect::CONSERVATIVE
    }
}

pub struct BuildOptions<'a> {
    pub policy: Policy,
    pub call_facts: &'a dyn CallFacts,
    pub allow_slow: bool,
    pub value_budget: usize,
}

impl Default for BuildOptions<'static> {
    fn default() -> Self {
        Self {
            policy: Policy::Conservative,
            call_facts: &ConservativeCallFacts,
            allow_slow: true,
            value_budget: DEFAULT_VALUE_BUDGET,
        }
    }
}

pub fn build(cg: &CgFunction, opts: &BuildOptions) -> Result<Ssa, Diagnostic> {
    build_with_faults(cg, opts, FaultState::Exact)
}

pub(crate) fn build_with_faults(
    cg: &CgFunction,
    opts: &BuildOptions,
    fault_state: FaultState,
) -> Result<Ssa, Diagnostic> {
    Builder::new(cg, opts, fault_state)?.run(cg)
}

struct Builder<'a> {
    opts: &'a BuildOptions<'a>,
    keys: Vec<StateKey>,
    key_index: Lookup<StateKey, usize>,
    field_of: Vec<Option<Field>>,
    values: Vec<Value>,
    args: Vec<ValueId>,
    state_arena: Vec<(StateKey, ValueId)>,
    boundaries: Vec<Boundary>,
    boundary_index: Lookup<ValueId, u32>,
    loc: Vec<Option<InsnLoc>>,
    slow: Vec<SlowInsn>,
    flags: Vec<FlagRecord>,
    preds: Vec<Vec<BlockId>>,
    dispatch_loads: Vec<ValueId>,
    current_def: Vec<Lookup<BlockId, ValueId>>,
    block_phis: Vec<Vec<ValueId>>,
    phi_key: Lookup<ValueId, usize>,
    entry_reads: Vec<Vec<(usize, ValueId)>>,
    exit_state: Vec<Vec<Option<ValueId>>>,
    processed: Vec<bool>,
    pending_phis: Vec<PendingPhi>,
    value_addr: Vec<Addr>,
    block_addrs: Vec<Addr>,
    current_addr: Addr,
    pending: Pending,
    access_indices: Vec<usize>,
    observable_faults: bool,
}

#[derive(Default)]
struct Pending {
    flag: Option<(ValueId, Vec<ValueId>, u8)>,
    ab: Option<Vec<ValueId>>,
    shape: Option<FlagShape>,
    carry: Option<ValueId>,
}

struct PendingPhi {
    value: ValueId,
    index: usize,
    block: BlockId,
    positions: Vec<usize>,
}

fn base_layout() -> &'static (Vec<StateKey>, Vec<Option<Field>>) {
    static LAYOUT: OnceLock<(Vec<StateKey>, Vec<Option<Field>>)> = OnceLock::new();
    LAYOUT.get_or_init(|| {
        let regs = Registers::get();
        let mut keys = Vec::new();
        let mut fields = Vec::new();
        for name in [
            "EAX",
            "ECX",
            "EDX",
            "EBX",
            "ESP",
            "EBP",
            "ESI",
            "EDI",
            "EIP",
            "FS_OFFSET",
            "CF",
            "PF",
            "AF",
            "ZF",
            "SF",
            "OF",
            "DF",
        ] {
            let Some(v) = regs.register(name) else {
                continue;
            };
            if v.space != Space::Register {
                continue;
            }
            for n in 0..v.size {
                keys.push(StateKey::Register((v.offset + n as u64) as u32));
                let field = Field::ALL.iter().find(|field| {
                    regs.register(field.name()).is_some_and(|r| {
                        r.space == Space::Register
                            && r.offset <= v.offset + n as u64
                            && v.offset + (n as u64) < r.offset + r.size as u64
                    })
                });
                fields.push(field.copied());
            }
        }
        (keys, fields)
    })
}

fn cf_register() -> Varnode {
    static CF: OnceLock<Varnode> = OnceLock::new();
    *CF.get_or_init(|| Registers::get().require("CF"))
}

impl<'a> Builder<'a> {
    fn new(
        cg: &CgFunction,
        opts: &'a BuildOptions<'a>,
        fault_state: FaultState,
    ) -> Result<Self, Diagnostic> {
        let regs = Registers::get();
        let (base_keys, base_fields) = base_layout();
        let mut keys = base_keys.clone();
        let mut fields = base_fields.clone();
        let mut extra = std::collections::BTreeSet::new();
        for insn in &cg.insns {
            for op in &insn.ops {
                for v in std::iter::once(op.output.as_ref())
                    .chain(op.inputs.iter().map(Some))
                    .flatten()
                {
                    if v.space == Space::Register {
                        for n in 0..v.size as u64 {
                            let offset = (v.offset + n) as u32;
                            if !keys.contains(&StateKey::Register(offset)) {
                                extra.insert(offset);
                            }
                        }
                    }
                }
            }
        }
        for offset in extra {
            keys.push(StateKey::Register(offset));
            fields.push(None);
        }
        keys.push(StateKey::Memory);
        fields.push(None);
        let mut seen = Lookup::default();
        for key in &keys {
            if seen.insert(*key, ()).is_some() {
                return Err(Diagnostic::new(
                    cg.addr,
                    Reason::UnsupportedVarnode {
                        detail: format!("duplicate state key {key:?}"),
                    },
                ));
            }
        }
        let key_index: Lookup<StateKey, usize> =
            keys.iter().enumerate().map(|(i, key)| (*key, i)).collect();
        let mut access_indices = Vec::new();
        for name in ["ESP", "EBP", "EIP"] {
            let Some(register) = regs.register(name) else {
                continue;
            };
            if register.space != Space::Register {
                continue;
            }
            for n in 0..register.size {
                let key = StateKey::Register((register.offset + n as u64) as u32);
                if let Some(&index) = key_index.get(&key) {
                    access_indices.push(index);
                }
            }
        }
        let keys_len = keys.len();
        Ok(Self {
            opts,
            keys,
            key_index,
            field_of: fields,
            values: Vec::new(),
            args: Vec::new(),
            state_arena: Vec::new(),
            boundaries: Vec::new(),
            boundary_index: Lookup::default(),
            loc: vec![None; cg.insns.len()],
            slow: cg.slow.clone(),
            flags: Vec::new(),
            preds: Vec::new(),
            dispatch_loads: Vec::new(),
            current_def: (0..keys_len).map(|_| Lookup::default()).collect(),
            block_phis: Vec::new(),
            phi_key: Lookup::default(),
            entry_reads: Vec::new(),
            exit_state: Vec::new(),
            processed: Vec::new(),
            pending_phis: Vec::new(),
            value_addr: Vec::new(),
            block_addrs: Vec::new(),
            current_addr: 0,
            pending: Pending::default(),
            access_indices,
            // DIVERGENCE(original): relaxed fault state does not preserve exact register state at a fault inside __try.
            observable_faults: fault_state == FaultState::Exact && !cg.seh.is_empty(),
        })
    }

    fn run(mut self, cg: &CgFunction) -> Result<Ssa, Diagnostic> {
        let n = cg.insns.len();
        if cg.succ.len() != n {
            return Err(Diagnostic::new(
                cg.addr,
                Reason::UnsupportedControlFlow {
                    detail: "successor table length mismatch".into(),
                },
            ));
        }
        let mut roots = Vec::new();
        roots.push(cg.index_of(cg.addr).ok_or_else(|| {
            Diagnostic::new(
                cg.addr,
                Reason::UnsupportedControlFlow {
                    detail: "function address is not an instruction".into(),
                },
            )
        })?);
        for &entry in &cg.entries {
            roots.push(cg.index_of(entry).ok_or_else(|| {
                Diagnostic::new(
                    entry,
                    Reason::UnsupportedControlFlow {
                        detail: "entry is not an instruction boundary".into(),
                    },
                )
            })?);
        }
        roots.sort_unstable();
        roots.dedup();

        let mut reachable = vec![false; n];
        let mut pending = roots.clone();
        while let Some(i) = pending.pop() {
            if i as usize >= n || reachable[i as usize] {
                continue;
            }
            reachable[i as usize] = true;
            for &s in &cg.succ[i as usize] {
                if s as usize >= n {
                    return Err(Diagnostic::new(
                        cg.insns[i as usize].addr,
                        Reason::UnsupportedControlFlow {
                            detail: format!("successor {s} outside the instruction table"),
                        },
                    ));
                }
                pending.push(s);
            }
        }

        let mut leader = vec![false; n];
        for &root in &roots {
            leader[root as usize] = true;
        }
        for (i, is_reachable) in reachable.iter().copied().enumerate().take(n) {
            if !is_reachable {
                continue;
            }
            let succ = &cg.succ[i];
            let fallthrough = succ.len() == 1
                && succ[0] == i as u32 + 1
                && cg.insns.get(i + 1).is_some_and(|next| {
                    cg.insns[i].addr + u32::from(cg.insns[i].length) == next.addr
                });
            if !fallthrough
                || cg.exits.contains_key(&(i as u32))
                || cg.import_tails.contains_key(&(i as u32))
            {
                for &s in succ {
                    leader[s as usize] = true;
                }
            }
        }

        let mut blocks = Vec::new();
        let mut start = 0usize;
        while start < n {
            if !reachable[start] {
                start += 1;
                continue;
            }
            if !leader[start] {
                start += 1;
                continue;
            }
            let mut end = start + 1;
            while end < n && reachable[end] && !leader[end] {
                let succ = &cg.succ[end - 1];
                if !(succ.len() == 1 && succ[0] == end as u32) {
                    break;
                }
                end += 1;
            }
            let id = blocks.len() as BlockId;
            for i in start..end {
                self.loc[i] = Some(InsnLoc {
                    block: id,
                    position: (i - start) as u32,
                });
            }
            blocks.push(crate::Block {
                start: start as u32,
                end: end as u32,
                preds: Vec::new(),
                phis: Vec::new(),
                ops: Vec::new(),
            });
            start = end;
        }

        for id in 0..blocks.len() {
            let last = (blocks[id].end - 1) as usize;
            for &successor in &cg.succ[last] {
                let target = self.loc[successor as usize]
                    .expect("reachable successor block")
                    .block;
                if !blocks[target as usize].preds.contains(&(id as BlockId)) {
                    blocks[target as usize].preds.push(id as BlockId);
                }
            }
        }
        for &root in &roots {
            let target = self.loc[root as usize].expect("root block").block;
            if !blocks[target as usize].preds.contains(&DISPATCH_BLOCK) {
                blocks[target as usize].preds.insert(0, DISPATCH_BLOCK);
            }
        }

        let mut block_succ = vec![Vec::new(); blocks.len()];
        for (id, block) in blocks.iter().enumerate() {
            for &pred in &block.preds {
                if pred != DISPATCH_BLOCK && !block_succ[pred as usize].contains(&(id as BlockId)) {
                    block_succ[pred as usize].push(id as BlockId);
                }
            }
        }
        let mut rpo_roots = Vec::new();
        for &root in &roots {
            let target = self.loc[root as usize].expect("root block").block;
            if !rpo_roots.contains(&target) {
                rpo_roots.push(target);
            }
        }
        let order = rpo(&rpo_roots, &block_succ);

        let keys = self.keys.clone();
        self.dispatch_loads = keys
            .iter()
            .map(|&key| self.value(Op::Input(key), key_size(key), &[]))
            .collect();
        let memory_index = self.memory_index();
        self.preds = blocks.iter().map(|block| block.preds.clone()).collect();
        self.block_phis = vec![Vec::new(); blocks.len()];
        self.entry_reads = vec![Vec::new(); blocks.len()];
        self.exit_state = vec![Vec::new(); blocks.len()];
        self.processed = vec![false; blocks.len()];
        self.block_addrs = blocks
            .iter()
            .map(|block| cg.insns[block.start as usize].addr)
            .collect();

        for &block_id in &order {
            let (start, end) = {
                let block = &blocks[block_id as usize];
                (block.start, block.end)
            };
            let mut state = vec![None; keys.len()];
            let mut ops = Vec::new();
            let mut temps = Lookup::default();
            for index in start..end {
                let insn = &cg.insns[index as usize];
                self.pending = Pending {
                    shape: insn.flag,
                    ..Pending::default()
                };
                let cf = cf_register();
                let touches_cf =
                    |v: Varnode| v.space == Space::Register && v.offset == cf.offset && v.size == 1;
                let reads_cf = insn
                    .ops
                    .iter()
                    .any(|op| op.inputs.iter().copied().any(touches_cf));
                let writes_cf = insn.ops.iter().any(|op| op.output.is_some_and(touches_cf));
                if reads_cf && writes_cf {
                    let cf_key = StateKey::Register(cf.offset as u32);
                    if let Some(&cf_index) = self.key_index.get(&cf_key) {
                        self.pending.carry = Some(self.state_slot(&mut state, cf_index, block_id)?);
                    }
                }
                for cg_op in &insn.ops {
                    if self.values.len() >= self.opts.value_budget {
                        return Err(Diagnostic::new(
                            insn.addr,
                            Reason::SsaBudget {
                                detail: format!("more than {} values", self.opts.value_budget),
                            },
                        ));
                    }
                    self.current_addr = insn.addr;
                    if let Err(mut diagnostic) = self.apply_op(
                        cg_op,
                        insn.addr,
                        &mut state,
                        &mut temps,
                        &mut ops,
                        memory_index,
                        block_id,
                    ) {
                        if diagnostic.addr == 0 {
                            diagnostic.addr = insn.addr;
                        }
                        return Err(diagnostic);
                    }
                }
                if let Some((a, b)) = insn.flag_operands {
                    let a = self.read(&a, &mut state, &mut temps, block_id)?;
                    let b = self.read(&b, &mut state, &mut temps, block_id)?;
                    self.pending.ab = Some(vec![a, b]);
                }
                self.record_flags(&mut state, block_id)?;
            }
            self.exit_state[block_id as usize] = state.clone();
            for (index, value) in state.into_iter().enumerate() {
                if let Some(value) = value {
                    self.current_def[index].insert(block_id, value);
                }
            }
            self.processed[block_id as usize] = true;
            self.complete_pending(block_id)?;
            blocks[block_id as usize].ops = ops;
        }
        for (block, phis) in blocks.iter_mut().zip(self.block_phis.iter()) {
            block.phis.clone_from(phis);
        }

        let alias = self.trivial_phi_aliases();

        let dispatch = Dispatch {
            loads: keys
                .iter()
                .copied()
                .zip(self.dispatch_loads.iter().copied())
                .collect(),
        };
        let mut ssa = Ssa {
            addr: cg.addr,
            values: self.values,
            args: self.args,
            blocks,
            dispatch,
            entries: roots
                .iter()
                .map(|&root| Entry {
                    addr: cg.insns[root as usize].addr,
                    target: self.loc[root as usize].expect("root block").block,
                })
                .collect(),
            boundaries: self.boundaries,
            boundary_index: self.boundary_index,
            state: self.state_arena,
            keys: self.keys,
            field_of: self.field_of,
            slow: self.slow,
            flags: self.flags,
            loc: self.loc,
            value_addr: self.value_addr,
            memory_partitioned: false,
            phi_key: self.phi_key,
            entry_reads: self.entry_reads,
            exit_state: self.exit_state,
            lane_groups: Vec::new(),
        };
        rewrite_aliases(&mut ssa, &alias);
        Ok(ssa)
    }

    fn state_slot(
        &mut self,
        state: &mut [Option<ValueId>],
        index: usize,
        block: BlockId,
    ) -> Result<ValueId, Diagnostic> {
        if let Some(value) = state[index] {
            return Ok(value);
        }
        let value = self.defined(index, block)?;
        state[index] = Some(value);
        if block != DISPATCH_BLOCK
            && !self.entry_reads[block as usize]
                .iter()
                .any(|(key, _)| *key == index)
        {
            self.entry_reads[block as usize].push((index, value));
        }
        Ok(value)
    }

    fn sealed(&self, block: BlockId) -> bool {
        self.preds[block as usize]
            .iter()
            .all(|&pred| pred == DISPATCH_BLOCK || self.processed[pred as usize])
    }

    fn defined(&mut self, index: usize, block: BlockId) -> Result<ValueId, Diagnostic> {
        if block == DISPATCH_BLOCK {
            return Ok(self.dispatch_loads[index]);
        }
        if let Some(&value) = self.current_def[index].get(&block) {
            return Ok(value);
        }
        let preds = self.preds[block as usize].clone();
        if preds.is_empty() {
            return Err(Diagnostic::new(
                0,
                Reason::UnsupportedControlFlow {
                    detail: format!(
                        "block {block} has no predecessor for {:?}",
                        self.keys[index]
                    ),
                },
            ));
        }
        if preds.len() == 1 && self.sealed(block) {
            return self.defined(index, preds[0]);
        }
        let phi = self.phi(key_size(self.keys[index]), preds.len());
        self.value_addr[phi as usize] = self.block_addrs[block as usize];
        self.block_phis[block as usize].push(phi);
        self.phi_key.insert(phi, index);
        self.current_def[index].insert(block, phi);
        let mut missing = Vec::new();
        for (position, pred) in preds.iter().copied().enumerate() {
            if pred == DISPATCH_BLOCK {
                let value = self.dispatch_loads[index];
                self.set_arg(phi, position, value);
            } else if self.processed[pred as usize] {
                let value = self.defined(index, pred)?;
                self.set_arg(phi, position, value);
            } else {
                missing.push(position);
            }
        }
        if !missing.is_empty() {
            self.pending_phis.push(PendingPhi {
                value: phi,
                index,
                block,
                positions: missing,
            });
        }
        Ok(phi)
    }

    fn complete_pending(&mut self, block: BlockId) -> Result<(), Diagnostic> {
        let mut position = 0usize;
        while position < self.pending_phis.len() {
            let pending = &self.pending_phis[position];
            let key = pending.index;
            let value = pending.value;
            let owner = pending.block;
            let ready: Vec<usize> = pending
                .positions
                .iter()
                .copied()
                .filter(|&slot| self.preds[owner as usize][slot] == block)
                .collect();
            if ready.is_empty() {
                position += 1;
                continue;
            }
            let resolved = self.defined(key, block)?;
            for slot in &ready {
                self.set_arg(value, *slot, resolved);
            }
            let pending = &mut self.pending_phis[position];
            pending.positions.retain(|slot| !ready.contains(slot));
            if pending.positions.is_empty() {
                self.pending_phis.swap_remove(position);
            } else {
                position += 1;
            }
        }
        Ok(())
    }

    fn trivial_phi_aliases(&self) -> Lookup<ValueId, ValueId> {
        let mut alias: Lookup<ValueId, ValueId> = Lookup::default();
        loop {
            let mut changed = false;
            for (index, value) in self.values.iter().enumerate() {
                if value.op != Op::Phi {
                    continue;
                }
                let id = index as ValueId;
                if alias.contains_key(&id) {
                    continue;
                }
                let args = &self.args
                    [value.args_start as usize..(value.args_start + value.args_len) as usize];
                let mut replacement = None;
                let mut consistent = true;
                for &arg in args {
                    let arg = resolve_alias(&alias, arg);
                    if arg == id {
                        continue;
                    }
                    if let Some(previous) = replacement {
                        if previous != arg {
                            consistent = false;
                            break;
                        }
                    } else {
                        replacement = Some(arg);
                    }
                }
                if let Some(replacement) = replacement.filter(|_| consistent) {
                    alias.insert(id, replacement);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        alias
    }

    fn is_flag(&self, varnode: Varnode) -> bool {
        if varnode.space != Space::Register {
            return false;
        }
        let cf = cf_register();
        varnode.offset >= cf.offset && varnode.offset < cf.offset + 6
    }

    fn record_flags(
        &mut self,
        state: &mut [Option<ValueId>],
        block: BlockId,
    ) -> Result<(), Diagnostic> {
        let carry = self.pending.carry.take();
        let Some((op, args, size)) = self.pending.flag.take() else {
            return Ok(());
        };
        let kind = match self.pending.shape {
            Some(FlagShape::Add) => crate::FlagKind::Add,
            Some(FlagShape::Sub) => crate::FlagKind::Sub,
            Some(FlagShape::Cmp) => crate::FlagKind::Cmp,
            Some(FlagShape::Inc) => crate::FlagKind::Inc,
            Some(FlagShape::Dec) => crate::FlagKind::Dec,
            Some(FlagShape::Adc) => crate::FlagKind::Adc,
            Some(FlagShape::Sbb) => crate::FlagKind::Sbb,
            Some(FlagShape::Logic) => crate::FlagKind::Logic,
            None => return Ok(()),
        };
        let operands = self
            .pending
            .ab
            .take()
            .filter(|operands| operands.len() >= 2)
            .unwrap_or(args);
        let mut flags = BTreeMap::new();
        for field in [
            Field::Cf,
            Field::Pf,
            Field::Af,
            Field::Zf,
            Field::Sf,
            Field::Of,
        ] {
            if let Some(index) = self
                .field_of
                .iter()
                .position(|candidate| *candidate == Some(field))
            {
                flags.insert(field, self.state_slot(state, index, block)?);
            }
        }
        let writes_cf = !matches!(kind, crate::FlagKind::Inc | crate::FlagKind::Dec);
        let defined: Fields = [
            Field::Cf,
            Field::Pf,
            Field::Af,
            Field::Zf,
            Field::Sf,
            Field::Of,
        ]
        .into_iter()
        .filter(|field| writes_cf || *field != Field::Cf)
        .collect();
        self.flags.push(FlagRecord {
            kind,
            size,
            op: Some(op),
            kind_value: None,
            a: operands.first().copied(),
            b: operands.get(1).copied(),
            carry,
            result: Some(op),
            flags,
            defined,
            guarded: false,
        });
        Ok(())
    }

    fn memory_index(&self) -> usize {
        *self.key_index.get(&StateKey::Memory).expect("memory key")
    }

    fn value(&mut self, op: Op, size: u8, args: &[ValueId]) -> ValueId {
        let id = self.values.len() as ValueId;
        let start = self.args.len() as u32;
        self.args.extend_from_slice(args);
        self.values.push(Value {
            op,
            size,
            args_start: start,
            args_len: args.len() as u32,
        });
        self.value_addr.push(self.current_addr);
        id
    }

    fn phi(&mut self, size: u8, preds: usize) -> ValueId {
        let id = self.values.len() as ValueId;
        let start = self.args.len() as u32;
        self.args.extend(std::iter::repeat_n(0, preds));
        self.values.push(Value {
            op: Op::Phi,
            size,
            args_start: start,
            args_len: preds as u32,
        });
        self.value_addr.push(self.current_addr);
        id
    }

    fn set_arg(&mut self, value: ValueId, position: usize, arg: ValueId) {
        let start = self.values[value as usize].args_start as usize;
        self.args[start + position] = arg;
    }

    fn boundary(
        &mut self,
        at: ValueId,
        state: &mut [Option<ValueId>],
        block: BlockId,
        access: bool,
    ) -> Result<(), Diagnostic> {
        let count = if access {
            self.access_indices.len()
        } else {
            self.keys.len()
        };
        let mut resolved = Vec::with_capacity(count);
        for position in 0..count {
            let index = if access {
                self.access_indices[position]
            } else {
                position
            };
            let key = self.keys[index];
            resolved.push((key, self.state_slot(state, index, block)?));
        }
        let state_start = self.state_arena.len() as u32;
        let state_len = resolved.len() as u32;
        self.state_arena.extend(resolved);
        let index = self.boundaries.len() as u32;
        self.boundaries.push(Boundary {
            at,
            state_start,
            state_len,
        });
        self.boundary_index.insert(at, index);
        Ok(())
    }

    fn relaxed_memory(&self) -> bool {
        self.opts.policy == Policy::Optimizing && !self.observable_faults
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_op(
        &mut self,
        cg_op: &CgOp,
        addr: Addr,
        state: &mut [Option<ValueId>],
        temps: &mut Lookup<(u64, u32), ValueId>,
        ops: &mut Vec<ValueId>,
        memory_index: usize,
        block: BlockId,
    ) -> Result<(), Diagnostic> {
        match cg_op.op {
            Op::Slow(index) => {
                let memory = self.state_slot(state, memory_index, block)?;
                let value = self.value(Op::Slow(index), 0, &[memory]);
                self.boundary(value, state, block, false)?;
                let token = self.value(Op::Memory, 0, &[value]);
                state[memory_index] = Some(token);
                self.reload_all(value, state);
                ops.push(value);
            }
            Op::CallOther(userop) => {
                let memory = self.state_slot(state, memory_index, block)?;
                let value = self.value(Op::CallOther(userop), 0, &[memory]);
                self.boundary(value, state, block, false)?;
                let token = self.value(Op::Memory, 0, &[value]);
                state[memory_index] = Some(token);
                self.reload_all(value, state);
                ops.push(value);
            }
            Op::X87(x87) => {
                let touches_memory = x87.touches_memory();
                let mut args = Vec::new();
                if touches_memory {
                    args.push(self.state_slot(state, memory_index, block)?);
                }
                for input in &cg_op.inputs {
                    args.push(self.read(input, state, temps, block)?);
                }
                let size = cg_op.output.map_or(0, |v| v.size as u8);
                let value = self.value(Op::X87(x87), size, &args);
                if matches!(x87, rk_ir::x87::X87Op::Conditional(..)) {
                    self.boundary(value, state, block, false)?;
                }
                if let Some(output) = cg_op.output {
                    self.write(&output, value, state, temps)?;
                }
                if touches_memory {
                    let token = self.value(Op::Memory, 0, &[value]);
                    state[memory_index] = Some(token);
                }
                ops.push(value);
            }
            Op::Call { target } => {
                let memory = self.state_slot(state, memory_index, block)?;
                let value = self.value(Op::Call { target }, 0, &[memory]);
                self.boundary(value, state, block, false)?;
                let token = self.value(Op::Memory, 0, &[value]);
                state[memory_index] = Some(token);
                let effect = self.call_effect(addr, Some(target));
                self.reload_call(value, effect, state, block)?;
                ops.push(value);
            }
            Op::CallInd => {
                let target = cg_op.inputs.first().ok_or_else(|| {
                    Diagnostic::new(
                        addr,
                        Reason::UnsupportedCall {
                            detail: "indirect call without a target".into(),
                        },
                    )
                })?;
                let target = self.read(target, state, temps, block)?;
                let memory = self.state_slot(state, memory_index, block)?;
                let value = self.value(Op::CallInd, 0, &[memory, target]);
                self.boundary(value, state, block, false)?;
                let token = self.value(Op::Memory, 0, &[value]);
                state[memory_index] = Some(token);
                let effect = self.call_effect(addr, None);
                self.reload_call(value, effect, state, block)?;
                ops.push(value);
            }
            Op::String(string) => {
                let memory = self.state_slot(state, memory_index, block)?;
                let value = self.value(Op::String(string), 0, &[memory]);
                self.boundary(value, state, block, false)?;
                let token = self.value(Op::Memory, 0, &[value]);
                state[memory_index] = Some(token);
                let (_, writes) = rk_ir::string::effect(string.0);
                self.reload_fields(value, writes, state, block)?;
                ops.push(value);
            }
            Op::Div { .. } | Op::Idiv { .. } => {
                let memory = self.state_slot(state, memory_index, block)?;
                let mut args = vec![memory];
                for input in &cg_op.inputs {
                    args.push(self.read(input, state, temps, block)?);
                }
                let size = cg_op.output.map_or(8, |v| v.size as u8);
                let value = self.value(cg_op.op, size, &args);
                self.boundary(value, state, block, false)?;
                let token = self.value(Op::Memory, 0, &[value]);
                state[memory_index] = Some(token);
                if let Some(output) = cg_op.output {
                    self.write(&output, value, state, temps)?;
                }
                self.reload_all(value, state);
                ops.push(value);
            }
            Op::Seh(_) => {
                let memory = self.state_slot(state, memory_index, block)?;
                let value = self.value(cg_op.op, 0, &[memory]);
                self.boundary(value, state, block, false)?;
                let token = self.value(Op::Memory, 0, &[value]);
                state[memory_index] = Some(token);
                ops.push(value);
            }
            Op::P(OpCode::Load) | Op::P(OpCode::Store) => {
                let memory = self.state_slot(state, memory_index, block)?;
                let mut args = vec![memory];
                for input in &cg_op.inputs {
                    args.push(self.read(input, state, temps, block)?);
                }
                let size = cg_op.output.map_or(0, |v| v.size as u8);
                let value = self.value(cg_op.op, size, &args);
                if self.relaxed_memory() {
                    if cg_op.op == Op::P(OpCode::Store) {
                        self.boundary(value, state, block, true)?;
                    }
                } else {
                    self.boundary(value, state, block, false)?;
                }
                let token = self.value(Op::Memory, 0, &[value]);
                state[memory_index] = Some(token);
                if let Some(output) = cg_op.output {
                    self.write(&output, value, state, temps)?;
                }
                ops.push(value);
            }
            Op::Return
            | Op::Tail { .. }
            | Op::ImportTail { .. }
            | Op::BranchInd
            | Op::Breakpoint => {
                let mut args = Vec::new();
                for input in &cg_op.inputs {
                    args.push(self.read(input, state, temps, block)?);
                }
                let value = self.value(cg_op.op, 0, &args);
                self.boundary(value, state, block, false)?;
                ops.push(value);
            }
            Op::Branch { .. } | Op::CBranch { .. } | Op::Trap(_) => {
                let mut args = Vec::new();
                for input in &cg_op.inputs {
                    args.push(self.read(input, state, temps, block)?);
                }
                let value = self.value(cg_op.op, 0, &args);
                if matches!(cg_op.op, Op::Branch { target } | Op::CBranch { target }
                    if !self.block_addrs.contains(&target))
                {
                    self.boundary(value, state, block, false)?;
                }
                ops.push(value);
            }
            Op::P(OpCode::Call) => {
                let value = self.value(cg_op.op, 0, &[]);
                self.boundary(value, state, block, false)?;
                self.reload_all(value, state);
                ops.push(value);
            }
            Op::P(OpCode::CallInd) | Op::P(OpCode::CallOther) | Op::P(OpCode::Unknown) => {
                let memory = self.state_slot(state, memory_index, block)?;
                let value = self.value(cg_op.op, 0, &[memory]);
                self.boundary(value, state, block, false)?;
                let token = self.value(Op::Memory, 0, &[value]);
                state[memory_index] = Some(token);
                self.reload_all(value, state);
                ops.push(value);
            }
            Op::P(_) => {
                let mut args = Vec::new();
                for input in &cg_op.inputs {
                    args.push(self.read(input, state, temps, block)?);
                }
                let size = cg_op.output.map_or(0, |v| v.size as u8);
                let value = self.value(cg_op.op, size, &args);
                if let Some(output) = cg_op.output {
                    self.write(&output, value, state, temps)?;
                }
                if let Op::P(code) = cg_op.op {
                    let expected = match self.pending.shape {
                        Some(FlagShape::Add | FlagShape::Inc | FlagShape::Adc) => {
                            matches!(code, OpCode::IntAdd)
                        }
                        Some(FlagShape::Sub | FlagShape::Cmp | FlagShape::Dec | FlagShape::Sbb) => {
                            matches!(code, OpCode::IntSub)
                        }
                        Some(FlagShape::Logic) => {
                            matches!(code, OpCode::IntAnd | OpCode::IntOr | OpCode::IntXor)
                        }
                        None => false,
                    };
                    let replace = expected
                        && cg_op.output.is_some_and(|output| !self.is_flag(output))
                        && (!matches!(self.pending.shape, Some(FlagShape::Logic))
                            || self.pending.flag.is_none());
                    if replace {
                        self.pending.ab = Some(args.clone());
                        self.pending.flag = Some((value, args, size));
                    }
                }
                ops.push(value);
            }
            Op::Input(_)
            | Op::Const(_)
            | Op::Target(_)
            | Op::Phi
            | Op::Byte(_)
            | Op::Pack
            | Op::Memory
            | Op::StateReload(_)
            | Op::StateKeep(_)
            | Op::CcKind(_) => {
                return Err(Diagnostic::new(
                    addr,
                    Reason::UnsupportedVarnode {
                        detail: format!("synthetic op in lowered instruction {:?}", cg_op.op),
                    },
                ));
            }
        }
        Ok(())
    }

    fn call_effect(&self, site: Addr, target: Option<Addr>) -> CallEffect {
        if self.opts.policy == Policy::Conservative {
            CallEffect::CONSERVATIVE
        } else {
            self.opts.call_facts.call(site, target)
        }
    }

    fn reload_call(
        &mut self,
        site: ValueId,
        effect: CallEffect,
        state: &mut [Option<ValueId>],
        block: BlockId,
    ) -> Result<(), Diagnostic> {
        let target = match self.values[site as usize].op {
            Op::Call { target } => Some(target),
            _ => None,
        };
        let cleanup = (self.opts.policy != Policy::Conservative)
            .then(|| self.opts.call_facts.stack(self.current_addr, target))
            .flatten()
            .map(|effect| effect.cleanup);
        let esp = Registers::get().require("ESP");
        let next_esp = if let Some(cleanup) = cleanup {
            let old = self.read(&esp, state, &mut Lookup::default(), block)?;
            let delta = self.value(Op::Const(4u32.wrapping_add(cleanup as u32) as u64), 4, &[]);
            Some(self.value(Op::P(OpCode::IntAdd), 4, &[old, delta]))
        } else {
            None
        };
        for index in 0..self.keys.len() {
            let key = self.keys[index];
            if key == StateKey::Memory {
                continue;
            }
            let field = self.field_of[index];
            let kept = field.is_some_and(|field| {
                effect.preserves.contains(field) || !effect.writes.contains(field)
            });
            let old = self.state_slot(state, index, block)?;
            state[index] = Some(if kept {
                self.value(Op::StateKeep(key), key_size(key), &[site, old])
            } else {
                self.value(Op::StateReload(key), key_size(key), &[site])
            });
        }
        if let Some(value) = next_esp {
            self.write(&esp, value, state, &mut Lookup::default())?;
        }
        Ok(())
    }

    fn reload_fields(
        &mut self,
        site: ValueId,
        fields: Fields,
        state: &mut [Option<ValueId>],
        block: BlockId,
    ) -> Result<(), Diagnostic> {
        for index in 0..self.keys.len() {
            let key = self.keys[index];
            if key == StateKey::Memory {
                continue;
            }
            if self.field_of[index].is_some_and(|field| fields.contains(field)) {
                let _ = self.state_slot(state, index, block)?;
                state[index] = Some(self.value(Op::StateReload(key), key_size(key), &[site]));
            }
        }
        Ok(())
    }

    fn reload_all(&mut self, site: ValueId, state: &mut [Option<ValueId>]) {
        for (index, slot) in state.iter_mut().enumerate().take(self.keys.len()) {
            let key = self.keys[index];
            if key == StateKey::Memory {
                continue;
            }
            *slot = Some(self.value(Op::StateReload(key), key_size(key), &[site]));
        }
    }

    fn read(
        &mut self,
        varnode: &Varnode,
        state: &mut [Option<ValueId>],
        temps: &mut Lookup<(u64, u32), ValueId>,
        block: BlockId,
    ) -> Result<ValueId, Diagnostic> {
        if varnode.size == 0 || varnode.size > 8 {
            return Err(Diagnostic::new(
                0,
                Reason::UnsupportedVarnode {
                    detail: format!("{} byte varnode", varnode.size),
                },
            ));
        }
        match varnode.space {
            Space::Const => Ok(self.value(Op::Const(varnode.offset), varnode.size as u8, &[])),
            Space::Register => {
                let mut lanes = Vec::with_capacity(varnode.size as usize);
                for n in 0..varnode.size as u64 {
                    let key = StateKey::Register((varnode.offset + n) as u32);
                    let index = self.key_index.get(&key).copied().ok_or_else(|| {
                        Diagnostic::new(
                            0,
                            Reason::UnsupportedVarnode {
                                detail: format!(
                                    "unmapped register byte {:08x}",
                                    varnode.offset + n
                                ),
                            },
                        )
                    })?;
                    lanes.push(self.state_slot(state, index, block)?);
                }
                if lanes.len() == 1 {
                    Ok(lanes[0])
                } else {
                    Ok(self.value(Op::Pack, varnode.size as u8, &lanes))
                }
            }
            Space::Unique => {
                let mut lanes = Vec::with_capacity(varnode.size as usize);
                for n in 0..varnode.size as u64 {
                    lanes.push(*temps.get(&(varnode.offset + n, 1)).ok_or_else(|| {
                        Diagnostic::new(
                            0,
                            Reason::SsaUniqueBeforeDef {
                                detail: format!("unique {:08x} byte {n}", varnode.offset),
                            },
                        )
                    })?);
                }
                if lanes.len() == 1 {
                    Ok(lanes[0])
                } else {
                    Ok(self.value(Op::Pack, varnode.size as u8, &lanes))
                }
            }
            Space::Ram => Err(Diagnostic::new(
                0,
                Reason::UnsupportedDirectRam {
                    detail: "raw direct-ram varnode reached SSA".into(),
                },
            )),
        }
    }

    fn write(
        &mut self,
        varnode: &Varnode,
        value: ValueId,
        state: &mut [Option<ValueId>],
        temps: &mut Lookup<(u64, u32), ValueId>,
    ) -> Result<(), Diagnostic> {
        match varnode.space {
            Space::Register => {
                for n in 0..varnode.size as u64 {
                    let key = StateKey::Register((varnode.offset + n) as u32);
                    let index = self.key_index.get(&key).copied().ok_or_else(|| {
                        Diagnostic::new(
                            0,
                            Reason::UnsupportedVarnode {
                                detail: format!(
                                    "unmapped register byte {:08x}",
                                    varnode.offset + n
                                ),
                            },
                        )
                    })?;
                    state[index] = Some(if varnode.size == 1 {
                        value
                    } else {
                        self.value(Op::Byte(n as u8), 1, &[value])
                    });
                }
                Ok(())
            }
            Space::Unique => {
                for n in 0..varnode.size as u64 {
                    temps.insert(
                        (varnode.offset + n, 1),
                        if varnode.size == 1 {
                            value
                        } else {
                            self.value(Op::Byte(n as u8), 1, &[value])
                        },
                    );
                }
                Ok(())
            }
            _ => Err(Diagnostic::new(
                0,
                Reason::UnsupportedVarnode {
                    detail: format!("unsupported output space {:?}", varnode.space),
                },
            )),
        }
    }
}

fn key_size(key: StateKey) -> u8 {
    if key == StateKey::Memory {
        0
    } else {
        1
    }
}

fn rpo(roots: &[BlockId], succ: &[Vec<BlockId>]) -> Vec<BlockId> {
    let mut seen = vec![false; succ.len()];
    let mut post = Vec::new();
    for &root in roots {
        if seen[root as usize] {
            continue;
        }
        seen[root as usize] = true;
        let mut stack = vec![(root, 0usize)];
        while let Some((block, next)) = stack.last_mut() {
            if *next < succ[*block as usize].len() {
                let child = succ[*block as usize][*next];
                *next += 1;
                if !seen[child as usize] {
                    seen[child as usize] = true;
                    stack.push((child, 0));
                }
            } else {
                post.push(*block);
                stack.pop();
            }
        }
    }
    post.reverse();
    post
}
