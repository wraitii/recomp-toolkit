//! Prints a `PlannedFunction` as C: the body (blocks, phi moves, publications, flag descriptors,
//! calls, dynamic dispatch, x87 clones) plus its entry, checked-return, alternate-entry and
//! private-contract wrappers. Unrepresentable forms return a diagnostic, and the driver then emits
//! a trap body. Individual unsupported instructions become named runtime traps listed in
//! `EmitReport`. It owns all translated C text: `x87` prints x87 clone events, `helpers` holds the
//! generated C preludes and `model` the emitted-body types that `rk-output` lays out into files.

mod helpers;
mod model;
mod slow;
mod support;
mod x87;

pub use helpers::preludes;
pub use model::{Body, Declaration, Preludes, Requires};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::slow::*;
use crate::support::*;
use crate::x87::{External, PlanLines};
use rk_core::contract::{Field, Fields};
use rk_core::ir::{FunctionIr, Insn, OpCode, SehEffect};
use rk_core::{Addr, Diagnostic, Reason, Result};
use rk_ir::{field_lane, Op, Registers, SlowFamily, StateKey};
use rk_plan::PlannedFunction;
use rk_ssa::{FlagDescriptor, ValueId, DISPATCH_BLOCK};

#[derive(Clone, Copy)]
pub struct PrivateEmit {
    pub mask: Fields,
    pub inputs: Fields,
    pub input_argument: Option<Field>,
}

#[derive(Clone, Copy)]
pub struct RelocEmit {
    pub image_base: Addr,
    pub local_size: u32,
}

pub struct EmitContext<'a> {
    pub ir: &'a FunctionIr,
    pub checked_returns: &'a BTreeSet<Addr>,
    pub effect_returns: &'a BTreeSet<Addr>,
    pub private_masks: &'a BTreeMap<Addr, BTreeSet<Fields>>,
    pub private_input_arguments: &'a BTreeMap<Addr, String>,
    pub known_entries: &'a BTreeSet<Addr>,
    pub region_entries: &'a BTreeMap<Addr, Addr>,
    pub setjmp_targets: &'a BTreeSet<Addr>,
    pub guest_calls: &'a BTreeSet<Addr>,
    pub host_calls: &'a BTreeSet<Addr>,
    pub private: Option<&'a PrivateEmit>,
    pub reloc: Option<RelocEmit>,
}

pub struct EmitReport {
    pub c_field_residency_violations: usize,
    pub fallbacks: BTreeSet<(Addr, String)>,
    pub slow_ops: BTreeSet<(Addr, String)>,
}

pub fn emit_body(planned: &PlannedFunction, ctx: &EmitContext<'_>) -> Result<(Body, EmitReport)> {
    let mut emitter = Emitter::new(planned, ctx);
    emitter.run()?;
    let ir = ctx.ir;
    let report = EmitReport {
        c_field_residency_violations: emitter.residency_violations,
        fallbacks: std::mem::take(&mut emitter.fallbacks),
        slow_ops: std::mem::take(&mut emitter.slow_ops),
    };
    Ok((
        Body {
            addr: planned.ssa.addr,
            insns: ir.insns.len(),
            first: ir.insns.first().map_or(ir.addr, |insn| insn.addr),
            last: ir.insns.last().map_or(ir.addr, |insn| insn.addr),
            text: emitter.out,
            requires: emitter.requires,
            declarations: emitter.declarations,
            private_mask: ctx.private.map(|private| private.mask),
        },
        report,
    ))
}

struct Emitter<'a> {
    plan: &'a PlannedFunction,
    s: &'a rk_ssa::Ssa,
    ctx: &'a EmitContext<'a>,
    out: String,
    requires: Requires,
    declarations: BTreeSet<Declaration>,
    multi: bool,
    checked: bool,
    private: Option<&'a PrivateEmit>,
    materialized: Vec<bool>,
    uses_arena: bool,
    value_block: Vec<u32>,
    successors: Vec<Vec<usize>>,
    transfers: Transfers,
    has_dynamic: bool,
    dispatch: Option<Dispatch>,
    key_location: rk_core::Lookup<StateKey, Option<Location>>,
    clone: usize,
    reload_defs: BTreeMap<ValueId, Vec<ValueId>>,
    available: std::cell::RefCell<Vec<Option<bool>>>,
    keep_lane: rk_core::Lookup<ValueId, (ValueId, u8)>,
    keep_group: BTreeMap<ValueId, KeepGroup>,
    inline_ok: Vec<bool>,
    inlined: Vec<bool>,
    in_block: Vec<bool>,
    residency_violations: usize,
    fallbacks: BTreeSet<(Addr, String)>,
    slow_ops: BTreeSet<(Addr, String)>,
}

#[derive(Clone, Copy)]
struct KeepGroup {
    field: usize,
    olds: [ValueId; 4],
}

#[derive(Clone, Copy)]
struct Dispatch {
    start: Addr,
    span: u32,
}

#[derive(Default)]
struct Transfers {
    sizes: Vec<u8>,
    sources: BTreeMap<u32, Vec<(usize, ValueId)>>,
    targets: BTreeMap<usize, Vec<(ValueId, usize)>>,
    edges: BTreeSet<(u32, usize)>,
}

impl Transfers {
    fn new(plan: &PlannedFunction, successors: &[Vec<usize>], force: bool) -> Self {
        let s = &plan.ssa;
        let sources: BTreeSet<u32> = s
            .blocks
            .iter()
            .enumerate()
            .filter(|(index, block)| {
                successors[*index].len() > 1
                    && block
                        .ops
                        .iter()
                        .any(|value| s.value(*value).op == Op::BranchInd)
            })
            .map(|(index, _)| index as u32)
            .chain([DISPATCH_BLOCK])
            .collect();
        let mut result = Self::default();
        let mut channels: Vec<BTreeMap<u32, ValueId>> = Vec::new();
        let mut copies = 0;
        for (target, block) in s.blocks.iter().enumerate() {
            let preds: Vec<_> = block
                .preds
                .iter()
                .enumerate()
                .filter(|(_, pred)| sources.contains(pred))
                .collect();
            if preds.is_empty() {
                continue;
            }
            for &phi in &block.phis {
                let size = s.value(phi).size;
                if size == 0 || !plan.live.contains(&phi) {
                    continue;
                }
                let args: BTreeMap<_, _> = preds
                    .iter()
                    .map(|(position, pred)| (**pred, s.args(phi)[*position]))
                    .collect();
                copies += args.len();
                let channel = channels
                    .iter()
                    .enumerate()
                    .position(|(index, values)| {
                        result.sizes[index] == size
                            && args.iter().all(|(pred, arg)| {
                                values.get(pred).is_none_or(|value| value == arg)
                            })
                    })
                    .unwrap_or_else(|| {
                        result.sizes.push(size);
                        channels.push(BTreeMap::new());
                        channels.len() - 1
                    });
                channels[channel].extend(args);
                result
                    .targets
                    .entry(target)
                    .or_default()
                    .push((phi, channel));
            }
            if result.targets.contains_key(&target) {
                result
                    .edges
                    .extend(preds.iter().map(|(_, pred)| (**pred, target)));
            }
        }
        let staged: usize = channels.iter().map(BTreeMap::len).sum();
        let received: usize = result.targets.values().map(Vec::len).sum();
        if !force && staged + received + result.targets.len() >= copies * 2 {
            return Self::default();
        }
        for (channel, values) in channels.into_iter().enumerate() {
            for (source, arg) in values {
                result
                    .sources
                    .entry(source)
                    .or_default()
                    .push((channel, arg));
            }
        }
        result
    }
}

impl<'a> Emitter<'a> {
    fn new(plan: &'a PlannedFunction, ctx: &'a EmitContext<'a>) -> Self {
        let s = &plan.ssa;
        let mut materialized = vec![false; s.values.len()];
        let mut value_block = vec![0u32; s.values.len()];
        let mut successors = vec![Vec::new(); s.blocks.len()];
        for (index, block) in s.blocks.iter().enumerate() {
            for value in block.phis.iter().chain(block.ops.iter()) {
                materialized[*value as usize] = true;
                value_block[*value as usize] = index as u32;
            }
            for pred in &block.preds {
                if *pred != DISPATCH_BLOCK {
                    successors[*pred as usize].push(index);
                }
            }
        }
        let key_location = s.keys.iter().map(|key| (*key, location(*key))).collect();
        let entry_targets: BTreeSet<usize> = s
            .entries
            .iter()
            .map(|entry| entry.target as usize)
            .collect();
        let mut indirect_targets: BTreeSet<usize> = BTreeSet::new();
        for (index, block) in s.blocks.iter().enumerate() {
            if block
                .ops
                .iter()
                .any(|value| matches!(s.value(*value).op, Op::BranchInd))
            {
                indirect_targets.extend(successors[index].iter().copied());
            }
        }
        let has_dynamic = s.blocks.iter().any(|block| {
            block.ops.iter().any(|value| {
                let value = *value;
                s.value(value).op == Op::BranchInd
                    && ctx
                        .ir
                        .index_of(s.value_addr[value as usize])
                        .is_some_and(|index| ctx.ir.dynamic.contains(&index))
            })
        });
        let dispatch = has_dynamic
            .then(|| {
                let start = *plan.block_addrs.iter().min()?;
                let end = *plan.block_addrs.iter().max()?;
                let span = end.checked_sub(start)?.checked_add(1)?;
                (span <= (1 << 20)).then_some(Dispatch { start, span })
            })
            .flatten();
        let inline_ok = (0..s.blocks.len())
            .map(|index| {
                let block = &s.blocks[index];
                index > 0
                    && block.preds.len() == 1
                    && block.preds[0] != DISPATCH_BLOCK
                    && (block.preds[0] as usize) < index
                    && !entry_targets.contains(&index)
                    && !indirect_targets.contains(&index)
                    && !has_dynamic
            })
            .collect();
        let uses_arena = s.values.iter().any(|value| {
            matches!(
                value.op,
                Op::P(OpCode::Load) | Op::P(OpCode::Store) | Op::ImportTail { .. }
            ) || matches!(value.op, Op::X87(op) if op.touches_memory())
        });
        let mut in_block = vec![false; s.values.len()];
        for block in &s.blocks {
            for value in &block.ops {
                in_block[*value as usize] = true;
            }
        }
        let mut slots: BTreeMap<(ValueId, usize), [Option<ValueId>; 4]> = BTreeMap::new();
        for (index, value) in s.values.iter().enumerate() {
            let id = index as ValueId;
            if !plan.live.contains(&id) {
                continue;
            }
            let Op::StateKeep(key) = value.op else {
                continue;
            };
            let Some((field, lane)) = field_lane(key) else {
                continue;
            };
            if field as usize >= 8 || lane >= 4 {
                continue;
            }
            let site = s.args(id)[0];
            slots.entry((site, field as usize)).or_insert([None; 4])[lane as usize] = Some(id);
        }
        let mut keep_lane: rk_core::Lookup<ValueId, (ValueId, u8)> = rk_core::Lookup::default();
        let mut keep_group: BTreeMap<ValueId, KeepGroup> = BTreeMap::new();
        for ((_site, field), lanes) in slots {
            let Some(ids) = lanes.iter().copied().collect::<Option<Vec<_>>>() else {
                continue;
            };
            let carrier = ids[0];
            let olds = [
                s.args(ids[0])[1],
                s.args(ids[1])[1],
                s.args(ids[2])[1],
                s.args(ids[3])[1],
            ];
            keep_group.insert(carrier, KeepGroup { field, olds });
            for (lane, id) in ids.into_iter().enumerate() {
                keep_lane.insert(id, (carrier, lane as u8));
            }
        }
        let mut reload_defs: BTreeMap<ValueId, Vec<ValueId>> = BTreeMap::new();
        for (index, value) in s.values.iter().enumerate() {
            let id = index as ValueId;
            if !plan.live.contains(&id) || value.size == 0 || materialized[index] {
                continue;
            }
            match value.op {
                Op::Input(key) => materialized[index] = private_loads(ctx.private, key),
                Op::StateKeep(_) => {
                    materialized[index] = true;
                    let carrier = keep_lane.get(&id).map_or(id, |(carrier, _)| *carrier);
                    if id == carrier {
                        reload_defs.entry(s.args(id)[0]).or_default().push(id);
                    }
                }
                Op::StateReload(_) => {
                    materialized[index] = true;
                    reload_defs.entry(s.args(id)[0]).or_default().push(id);
                }
                _ => {}
            }
        }
        let residency_violations = (0..s.values.len())
            .filter(|index| {
                let id = *index as ValueId;
                plan.live.contains(&id)
                    && !materialized[*index]
                    && reads_c_field(s, id, &materialized, ctx.private)
            })
            .count();
        Self {
            plan,
            s,
            ctx,
            out: String::new(),
            requires: Requires::default(),
            declarations: BTreeSet::new(),
            multi: s.entries.len() > 1,
            checked: ctx.checked_returns.contains(&s.addr) || ctx.private.is_some(),
            private: ctx.private,
            materialized,
            uses_arena,
            value_block,
            transfers: Transfers::new(plan, &successors, has_dynamic),
            successors,
            has_dynamic,
            dispatch,
            key_location,
            clone: 0,
            reload_defs,
            available: std::cell::RefCell::new(vec![None; s.values.len()]),
            keep_lane,
            keep_group,
            inline_ok,
            inlined: vec![false; s.blocks.len()],
            in_block,
            residency_violations,
            fallbacks: BTreeSet::new(),
            slow_ops: BTreeSet::new(),
        }
    }

    fn run(&mut self) -> Result<()> {
        let addr = self.s.addr;
        let body_name = if let Some(private) = self.private {
            format!("fn_contract_{addr:08x}_{:04x}", private.mask.0)
        } else if self.checked {
            format!("body_checked_{addr:08x}")
        } else if self.multi {
            format!("body_{addr:08x}")
        } else {
            format!("fn_{addr:08x}")
        };
        let signature = self.signature(&body_name);
        self.out.push_str(&signature);
        self.out.push_str(" {\n");
        self.write_declarations();
        self.write_x87_declarations()?;
        if self
            .ctx
            .ir
            .seh
            .values()
            .any(|kinds| kinds.contains(&SehEffect::Orphan))
        {
            self.out
                .push_str("uint64_t seh_mark_ = recomp_seh_frame_mark(c);\n");
        }
        if self.uses_arena {
            self.out.push_str("uint8_t *arena_ = g_mem;\n");
            self.requires.arena = true;
        }
        if self.plan.entry_settle {
            self.out.push_str("x86_cc_settle(c);\n");
        }
        self.write_inputs()?;
        let clones = self.plan.x87.as_ref().map_or(1, rk_x87::Plan::clones);
        self.write_dispatch_tables(clones);
        self.entry_dispatch()?;
        if let Some(site) = self
            .reload_defs
            .keys()
            .find(|site| !self.in_block[**site as usize])
        {
            return Err(self.error(
                self.s.value_addr[*site as usize],
                "state reload site is not a block op",
            ));
        }
        for clone in 0..clones {
            self.clone = clone;
            self.blocks()?;
            for (&target, copies) in &self.transfers.targets {
                let _ = writeln!(self.out, "E{}{target}:;", self.block_prefix());
                for (phi, channel) in copies {
                    let _ = writeln!(self.out, "v{phi} = e{channel};");
                }
                let _ = writeln!(self.out, "goto {}{target};", self.block_prefix());
            }
        }
        self.out.push_str("}\n");
        self.wrappers(&body_name);
        self.requires.return_fast = true;
        Ok(())
    }

    fn guest_addr(&self, addr: Addr) -> String {
        if let Some(reloc) = self.ctx.reloc {
            if addr >= reloc.image_base && addr < reloc.image_base + reloc.local_size {
                return format!("(base_ + 0x{:x}u)", addr - reloc.image_base);
            }
        }
        format!("0x{addr:08x}u")
    }

    fn reloc_internal(&self, addr: Addr) -> bool {
        self.ctx.reloc.is_some_and(|reloc| {
            addr >= reloc.image_base && addr < reloc.image_base + reloc.local_size
        })
    }

    fn signature(&self, name: &str) -> String {
        let mut params = vec!["X86 *c".to_string()];
        if self.ctx.reloc.is_some() {
            params.push("uint32_t base_".to_string());
        }
        if let Some(private) = self.private {
            if let Some(argument) = private.input_argument {
                params.push(format!("uint32_t {}_", argument.name().to_lowercase()));
            }
            params.push("uint32_t expected_return_".to_string());
        } else {
            if self.multi {
                params.push("uint32_t entry_".to_string());
            }
            if self.checked {
                params.push("uint32_t expected_return_".to_string());
                params.push("int checked_return_".to_string());
            }
        }
        format!("void {name}({})", params.join(", "))
    }

    fn call_args(&self, entry: Option<Addr>, expected: Option<&str>) -> String {
        let mut args = vec!["c".to_string()];
        if self.ctx.reloc.is_some() {
            args.push("base_".to_string());
        }
        if self.multi {
            let entry = entry.or_else(|| self.ctx.reloc.map(|_| self.s.addr));
            args.push(entry.map_or("0u".to_string(), |addr| self.guest_addr(addr)));
        }
        if self.checked {
            args.push(expected.unwrap_or("0u").to_string());
            args.push(if expected.is_some() { "1" } else { "0" }.to_string());
        }
        args.join(", ")
    }

    fn wrappers(&mut self, body: &str) {
        let addr = self.s.addr;
        if self.private.is_some() {
            return;
        }
        let base_param = if self.ctx.reloc.is_some() {
            ", uint32_t base_"
        } else {
            ""
        };
        if body != format!("fn_{addr:08x}") {
            let args = self.call_args(None, None);
            let _ = writeln!(
                self.out,
                "void fn_{addr:08x}(X86 *c{base_param}) {{ {body}({args}); }}"
            );
        }
        if self.checked {
            let args = self.call_args(None, Some("expected"));
            let _ = writeln!(
                self.out,
                "void fn_checked_{addr:08x}(X86 *c{base_param}, uint32_t expected) {{ {body}({args}); }}"
            );
        }
        if self.multi {
            if self.ctx.region_entries.values().any(|&owner| owner == addr) {
                let primary = self.guest_addr(addr);
                let args = self.call_args(Some(addr), None).replace(&primary, "target");
                let _ = writeln!(
                    self.out,
                    "void region_{addr:08x}(X86 *c{base_param}, uint32_t target) {{ {body}({args}); }}"
                );
            }
            for entry in &self.s.entries {
                let seh_callback = self.ctx.ir.seh.iter().any(|(&index, kinds)| {
                    let insn = &self.ctx.ir.insns[index as usize];
                    insn.addr + Addr::from(insn.length) == entry.addr
                        && kinds
                            .iter()
                            .any(|kind| matches!(kind, SehEffect::Enter | SehEffect::Adopt))
                });
                if entry.addr == addr
                    || (self.ctx.region_entries.contains_key(&entry.addr) && !seh_callback)
                {
                    continue;
                }
                let args = self.call_args(Some(entry.addr), None);
                let _ = writeln!(
                    self.out,
                    "void fn_{:08x}(X86 *c{base_param}) {{ {body}({args}); }}",
                    entry.addr
                );
            }
        }
    }

    fn write_declarations(&mut self) {
        for (channel, size) in self.transfers.sizes.iter().enumerate() {
            let _ = writeln!(self.out, "{} e{channel};", ctype(*size));
        }
        let mut narrow = Vec::new();
        let mut wide = Vec::new();
        for (index, value) in self.s.values.iter().enumerate() {
            let id = index as ValueId;
            if !self.plan.live.contains(&id) || value.size == 0 || !self.materialized[index] {
                continue;
            }
            if self
                .keep_lane
                .get(&id)
                .is_some_and(|(carrier, _)| *carrier != id)
            {
                continue;
            }
            if value.size > 4 {
                wide.push(id);
            } else {
                narrow.push(id);
            }
        }
        for (ctype, ids) in [("uint32_t", narrow), ("uint64_t", wide)] {
            if ids.is_empty() {
                continue;
            }
            let mut names = ids.iter().map(|id| format!("v{id}"));
            let first = names.next().unwrap();
            let _ = write!(self.out, "{ctype} {first}");
            for name in names {
                let _ = write!(self.out, ", {name}");
            }
            self.out.push_str(";\n");
        }
    }

    fn write_x87(&mut self, lines: Vec<String>) {
        for line in lines {
            if identity_assignment(&line) {
                continue;
            }
            if line.contains("recomp_fcom_(") {
                self.requires.fcom = true;
            }
            if x87_reads_flags(&line) {
                self.out.push_str("x86_cc_settle(c);\n");
            }
            let _ = writeln!(self.out, "{line}");
        }
    }

    fn write_x87_declarations(&mut self) -> Result<()> {
        for line in self.x87_lines(|x87, names| x87.declaration_lines(names))? {
            let _ = writeln!(self.out, "{line}");
        }
        Ok(())
    }

    fn write_inputs(&mut self) -> Result<()> {
        for index in 0..self.s.values.len() {
            let id = index as ValueId;
            let Op::Input(key) = self.s.value(id).op else {
                continue;
            };
            if !self.plan.live.contains(&id) || !self.materialized[index] {
                continue;
            }
            if let Some(private) = self.private {
                if let Some((field, _)) = field_lane(key) {
                    if !private.inputs.contains(field) && private.input_argument != Some(field) {
                        continue;
                    }
                }
            }
            let expr = self.field_expr(key, self.s.value(id).size)?;
            let _ = writeln!(self.out, "{}", self.assignment(id, &expr));
        }
        Ok(())
    }

    fn materialize_defs(&mut self, site: ValueId) -> Result<()> {
        let Some(values) = self.reload_defs.get(&site).cloned() else {
            return Ok(());
        };
        for id in values {
            if let Some(group) = self.keep_group.get(&id).copied() {
                let old = self.pack_bytes(&group.olds, false)?;
                let loaded = format!("c->r[{}]", group.field);
                let expr = keep_expr(&loaded, &old);
                let _ = writeln!(self.out, "v{id} = ({expr});");
                continue;
            }
            let expr = self.operation(id)?;
            let _ = writeln!(self.out, "{}", self.assignment(id, &expr));
        }
        Ok(())
    }

    fn block_prefix(&self) -> char {
        self.prefix_for(self.clone)
    }

    fn prefix_for(&self, clone: usize) -> char {
        match self
            .plan
            .x87
            .as_ref()
            .and_then(|plan| plan.programs.get(clone))
            .and_then(|program| program.clone)
        {
            Some("fast") => 'F',
            _ => 'B',
        }
    }

    fn write_dispatch_tables(&mut self, clones: usize) {
        let Some(dispatch) = self.dispatch else {
            return;
        };
        let ctype = if self.s.blocks.len() > u16::MAX as usize {
            "uint32_t"
        } else {
            "uint16_t"
        };
        let mut slots = BTreeMap::new();
        for (block, &addr) in self.plan.block_addrs.iter().enumerate() {
            if let Some(offset) = addr.checked_sub(dispatch.start) {
                if offset < dispatch.span {
                    slots.insert(offset, block);
                }
            }
        }
        let _ = write!(self.out, "static const {ctype} recomp_dispatch_[] = {{");
        for (offset, block) in slots {
            let _ = write!(self.out, " [0x{offset:x}] = {},", block + 1);
        }
        self.out.push_str(" };\n");
        let prefixes: BTreeSet<char> = (0..clones).map(|clone| self.prefix_for(clone)).collect();
        for prefix in prefixes {
            let _ = writeln!(self.out, "static void *const recomp_labels_{prefix}[] = {{");
            for block in 0..self.s.blocks.len() {
                let target = if self.transfers.targets.contains_key(&block) {
                    format!("&&E{prefix}{block}")
                } else {
                    format!("&&{prefix}{block}")
                };
                let _ = writeln!(self.out, "{target},");
            }
            self.out.push_str("};\n");
        }
    }

    fn x87_lines(
        &self,
        lines: impl FnOnce(&rk_x87::Plan, &ValueNames) -> Result<Vec<String>>,
    ) -> Result<Vec<String>> {
        let Some(x87) = self.plan.x87.as_ref() else {
            return Ok(Vec::new());
        };
        let names = ValueNames { emitter: self };
        lines(x87, &names)
    }

    fn entry_dispatch(&mut self) -> Result<()> {
        self.stage_transfer(DISPATCH_BLOCK)?;
        let primary = self
            .s
            .entries
            .iter()
            .find(|entry| entry.addr == self.s.addr)
            .or_else(|| self.s.entries.first())
            .ok_or_else(|| self.error(self.s.addr, "SSA has no entry"))?;
        if self.multi {
            if self.ctx.reloc.is_some() {
                self.out.push_str("switch (entry_ - base_) {\n");
            } else {
                self.out.push_str("switch (entry_) {\n");
            }
            for entry in &self.s.entries {
                if entry.addr == primary.addr {
                    continue;
                }
                let entry_addr = if let Some(reloc) = self.ctx.reloc {
                    format!("0x{:x}u", entry.addr - reloc.image_base)
                } else {
                    self.guest_addr(entry.addr)
                };
                let _ = writeln!(self.out, "case {entry_addr}:");
                self.edge(None, entry.target as usize)?;
            }
            let region = self
                .ctx
                .region_entries
                .values()
                .any(|&owner| owner == self.s.addr);
            if region {
                let primary_addr = if let Some(reloc) = self.ctx.reloc {
                    format!("0x{:x}u", primary.addr - reloc.image_base)
                } else {
                    self.guest_addr(primary.addr)
                };
                let _ = writeln!(self.out, "case 0u: case {primary_addr}:");
            } else {
                self.out.push_str("default:;\n");
            }
            self.edge(None, primary.target as usize)?;
            if region {
                self.out
                    .push_str("default: recomp_region_missing(c, entry_); return;\n");
            }
            self.out.push_str("}\n");
        } else if primary.target == 0 && !self.transfers.edges.contains(&(DISPATCH_BLOCK, 0)) {
            self.edge_copies(DISPATCH_BLOCK, primary.target as usize)?;
        } else {
            self.edge(None, primary.target as usize)?;
        }
        Ok(())
    }

    fn blocks(&mut self) -> Result<()> {
        self.inlined.fill(false);
        for block in 0..self.s.blocks.len() {
            if self.inlined[block] {
                continue;
            }
            self.block(block, true)?;
        }
        Ok(())
    }

    fn can_inline(&self, source: usize, target: usize) -> bool {
        !self.inlined[target] && target > source && self.inline_ok[target]
    }

    fn inline_block(&mut self, source: usize, target: usize) -> Result<()> {
        self.edge_copies(source as u32, target)?;
        self.inlined[target] = true;
        self.block(target, false)
    }

    fn block(&mut self, block: usize, label: bool) -> Result<()> {
        if label {
            let _ = writeln!(self.out, "{}{block}:;", self.block_prefix());
        }
        {
            let item = &self.s.blocks[block];
            let mut commented_addr = None;
            if let Some(insn) = self.ctx.ir.insns.get(item.start as usize) {
                let _ = writeln!(self.out, "// {}", insn.text);
                commented_addr = Some(insn.addr);
            }
            let block_lines =
                self.x87_lines(|x87, names| x87.block_lines(self.clone, block as u32, names))?;
            self.write_x87(block_lines);
            let ops = item.ops.clone();
            let mut continues = true;
            let mut grouped = 0usize;
            for (position, value) in ops.iter().copied().enumerate() {
                if position < grouped {
                    continue;
                }
                let live = self.plan.live.contains(&value);
                let addr = self.s.value_addr[value as usize];
                if (live || self.reload_defs.contains_key(&value)) && commented_addr != Some(addr) {
                    if let Some(index) = self.ctx.ir.index_of(addr) {
                        let insn = &self.ctx.ir.insns[index as usize];
                        let _ = writeln!(self.out, "// {}", insn.text);
                        commented_addr = Some(addr);
                    }
                }
                if live {
                    if let Some(code) = self.packed_float4(&ops, position)? {
                        self.packed_float4_emit(code, &ops[position..position + 4])?;
                        for &lane in &ops[position..position + 4] {
                            self.materialize_defs(lane)?;
                        }
                        grouped = position + 4;
                        continue;
                    }
                    if let (Op::Branch { target: branch }, Some(Op::Tail { target: tail })) = (
                        self.s.value(value).op,
                        ops.get(position + 1).map(|next| self.s.value(*next).op),
                    ) {
                        if branch == tail {
                            let lines = self.x87_lines(|x87, names| {
                                x87.event_lines(self.clone, value, names)
                            })?;
                            self.write_x87(lines);
                            self.publish(value)?;
                            self.materialize_defs(value)?;
                            continue;
                        }
                    }
                    continues = self.value(value)?;
                }
                self.materialize_defs(value)?;
            }
            let ended = ops
                .last()
                .is_some_and(|value| match self.s.value(*value).op {
                    Op::Slow(_) => !continues,
                    op => ends_block(op),
                });
            let end_addr = self.block_end_addr(block);
            let external_branch = ops
                .last()
                .is_some_and(|value| matches!(self.s.value(*value).op, Op::CBranch { .. }))
                && !self
                    .successors(block)
                    .iter()
                    .copied()
                    .any(|next| Some(self.plan.block_addrs[next]) == end_addr);
            if !ended || external_branch {
                let next = self
                    .successors(block)
                    .iter()
                    .copied()
                    .find(|next| Some(self.plan.block_addrs[*next]) == end_addr);
                let end_lines =
                    self.x87_lines(|x87, names| x87.end_lines(self.clone, block as u32, names))?;
                self.write_x87(end_lines);
                match next {
                    Some(next) if self.can_inline(block, next) => self.inline_block(block, next)?,
                    Some(next) => self.edge(Some(block), next)?,
                    None if self.s.blocks[block].preds.is_empty() => {}
                    None if end_addr.is_some_and(|addr| self.ctx.known_entries.contains(&addr)) => {
                        let addr = end_addr.unwrap();
                        if self.reloc_internal(addr) {
                            let target = self.guest_addr(addr);
                            let _ = writeln!(self.out, "recomp_jump(c, {target}); return;");
                        } else {
                            self.declare_entry(addr);
                            let _ = writeln!(self.out, "CALL_FN({addr:08x}); return;");
                        }
                    }
                    None if end_addr.is_some() => {
                        let addr = end_addr.unwrap();
                        let at = self.guest_addr(addr);
                        let _ = writeln!(self.out, "c->eip = {at}; recomp_jump(c, {at}); return;");
                    }
                    None => {
                        return Err(self.error(self.s.addr, "block falls through to no successor"))
                    }
                }
            }
        }
        Ok(())
    }

    fn value(&mut self, value: ValueId) -> Result<bool> {
        let item = *self.s.value(value);
        let addr = self.s.value_addr[value as usize];
        if !matches!(item.op, Op::X87(_)) {
            let lines = self.x87_lines(|x87, names| x87.event_lines(self.clone, value, names))?;
            self.write_x87(lines);
        }
        match item.op {
            Op::Memory => Ok(true),
            Op::X87(op) => {
                if op.touches_memory() || matches!(op, rk_ir::x87::X87Op::Conditional(..)) {
                    self.publish(value)?;
                }
                let lines =
                    self.x87_lines(|x87, names| x87.event_lines(self.clone, value, names))?;
                self.write_x87(lines);
                Ok(true)
            }
            Op::Slow(index) => {
                let slow = self
                    .s
                    .slow(index)
                    .copied()
                    .ok_or_else(|| self.error(addr, "missing slow record"))?;
                self.publish(value)?;
                if matches!(slow.family, SlowFamily::X87) {
                    return Err(Diagnostic::error(
                        addr,
                        Reason::UnsupportedX87 {
                            detail: "unsupported x87 instruction or operand".into(),
                        },
                    ));
                }
                let emitted = self.slow(slow.family, addr)?;
                if emitted {
                    if let Some(insn) = self.insn_at(addr) {
                        self.slow_ops
                            .insert((addr, insn.mnemonic.to_ascii_uppercase()));
                    }
                    if self.plan.settle_sites.contains(&value) {
                        self.out.push_str("x86_cc_settle(c);\n");
                    }
                }
                Ok(emitted)
            }
            Op::CallOther(userop) => {
                self.publish(value)?;
                self.unsupported(addr, &format!("callother userop {userop}"))
            }
            Op::Call { target } => self.call(value, target).map(|_| true),
            Op::CallInd => self.call_ind(value, addr).map(|_| true),
            Op::Tail { target } => {
                self.publish(value)?;
                let at = self.guest_addr(addr);
                let target_expr = self.guest_addr(target);
                if self.ctx.known_entries.contains(&target) && !self.reloc_internal(target) {
                    self.declare_entry(target);
                    let _ = writeln!(self.out, "CALL_FN({target:08x}); return;");
                } else {
                    let _ = writeln!(
                        self.out,
                        "c->eip = {at}; recomp_jump(c, {target_expr}); return;"
                    );
                }
                Ok(false)
            }
            Op::ImportTail { slot } => {
                self.publish(value)?;
                let at = self.guest_addr(addr);
                let slot_expr = self.guest_addr(slot);
                let _ = writeln!(
                    self.out,
                    "c->eip = {at}; recomp_jump(c, recomp_rd32_(arena_, {slot_expr})); return;"
                );
                Ok(false)
            }
            Op::Branch { target } => self.branch(value, target).map(|_| true),
            Op::CBranch { target } => self.cbranch(value, target).map(|_| true),
            Op::BranchInd => self.branch_ind(value, addr).map(|_| true),
            Op::Return => self.return_value(value).map(|_| false),
            Op::Trap(target) => {
                let target = self.guest_addr(target);
                let _ = writeln!(self.out, "recomp_unmodelled(c, {target}); return;");
                Ok(false)
            }
            Op::Breakpoint => {
                self.publish(value)?;
                let at = self.guest_addr(addr);
                let _ = writeln!(self.out, "recomp_breakpoint(c, {at}); return;");
                Ok(false)
            }
            Op::Seh(kind) => {
                self.publish(value)?;
                if matches!(kind, SehEffect::Enter | SehEffect::Adopt) {
                    let insn = self
                        .insn_at(addr)
                        .ok_or_else(|| self.error(addr, "SEH site without instruction"))?;
                    let next = insn.addr + Addr::from(insn.length);
                    let callback = if self.ctx.ir.index_of(next).is_some() {
                        format!("fn_{next:08x}")
                    } else if self.ctx.known_entries.contains(&next) {
                        self.declare_entry(next);
                        format!("entry_{next:08x}")
                    } else {
                        return Err(self.error(addr, "SEH continuation without entry"));
                    };
                    self.requires.seh_call = true;
                    let adopt = u8::from(kind == SehEffect::Adopt);
                    let at = self.guest_addr(addr);
                    let _ = writeln!(self.out, "{{ extern void {callback}(X86 *); c->eip = {at}; recomp_seh_frame_call_(c, {callback}, {adopt}); return; }}");
                    return Ok(false);
                }
                self.seh(kind, addr);
                Ok(true)
            }
            Op::Div { .. } | Op::Idiv { .. } => {
                self.divide(value, addr)?;
                if self.plan.settle_sites.contains(&value) {
                    self.out.push_str("x86_cc_settle(c);\n");
                }
                Ok(true)
            }
            Op::String(name) => {
                self.publish(value)?;
                let _ = writeln!(self.out, "{} (c);", name.0);
                self.out.push_str("x86_cc_settle(c);\n");
                Ok(true)
            }
            Op::P(OpCode::Load) => self.load(value).map(|_| true),
            Op::P(OpCode::Store) => self.store(value).map(|_| true),
            _ => {
                let expr = self.operation(value)?;
                let _ = writeln!(self.out, "{}", self.assignment(value, &expr));
                Ok(true)
            }
        }
    }

    fn insn_at(&self, addr: Addr) -> Option<&Insn> {
        self.ctx
            .ir
            .insns
            .binary_search_by_key(&addr, |insn| insn.addr)
            .ok()
            .map(|index| &self.ctx.ir.insns[index])
    }

    fn slow_trap(&mut self, addr: Addr, reason: &str) {
        let reason = reason.replace(['"', '\\'], "_");
        self.fallbacks.insert((addr, reason.clone()));
        let at = self.guest_addr(addr);
        let _ = writeln!(
            self.out,
            "recomp_unmodelled_named(c, {at}, \"{reason}\"); return;"
        );
    }

    fn unsupported(&mut self, addr: Addr, reason: &str) -> Result<bool> {
        self.slow_trap(addr, reason);
        Ok(false)
    }

    fn slow(&mut self, family: SlowFamily, addr: Addr) -> Result<bool> {
        let insn = self
            .insn_at(addr)
            .cloned()
            .ok_or_else(|| self.error(addr, "slow instruction missing from IR"))?;
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        match family {
            SlowFamily::Rdtsc => {
                self.out.push_str("recomp_rdtsc(c);\n");
                Ok(true)
            }
            SlowFamily::Cpuid => {
                self.out.push_str("recomp_cpuid(c);\n");
                Ok(true)
            }
            SlowFamily::Flags => match mnemonic.as_str() {
                "PUSHFD" => {
                    self.out.push_str("recomp_pushfd(c);\n");
                    Ok(true)
                }
                "POPFD" => {
                    self.out.push_str("recomp_popfd(c);\n");
                    Ok(true)
                }
                "PUSHF" => {
                    self.out.push_str("recomp_pushf(c);\n");
                    Ok(true)
                }
                "POPF" => {
                    self.out.push_str("recomp_popf(c);\n");
                    Ok(true)
                }
                _ => self.unsupported(addr, &mnemonic),
            },
            SlowFamily::Int => {
                let vector = match mnemonic.as_str() {
                    "INT1" => Some(1),
                    "INTO" => Some(4),
                    _ => slow_operands(&insn).and_then(|ops| match ops.first() {
                        Some(SlowOperand::Imm(value)) => Some(*value as u32),
                        _ => None,
                    }),
                };
                let Some(vector) = vector else {
                    return self.unsupported(addr, &mnemonic);
                };
                let _ = writeln!(self.out, "recomp_int(c, 0x{vector:x}u);");
                Ok(true)
            }
            SlowFamily::In => self.slow_port(&insn, true, addr),
            SlowFamily::Out => self.slow_port(&insn, false, addr),
            SlowFamily::Mmx => self.slow_mmx(&insn, addr),
            SlowFamily::Sse => self.slow_sse(&insn, addr),
            SlowFamily::Segment => self.slow_segment(&insn, addr),
            _ => self.unsupported(addr, &mnemonic),
        }
    }

    fn slow_port(&mut self, insn: &Insn, input: bool, addr: Addr) -> Result<bool> {
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        let repeat = insn.repeat.is_some();
        let prefix = if repeat { "rep_" } else { "" };
        if mnemonic.starts_with("INS") || mnemonic.starts_with("OUTS") {
            let suffix = mnemonic.chars().last().unwrap_or('B').to_ascii_lowercase();
            if !matches!(suffix, 'b' | 'w' | 'd') {
                return self.unsupported(addr, &mnemonic);
            }
            let root = if mnemonic.starts_with("INS") {
                "ins"
            } else {
                "outs"
            };
            let _ = writeln!(self.out, "{prefix}{root}{suffix}(c);");
            return Ok(true);
        }
        let Some(operands) = slow_operands(insn) else {
            return self.unsupported(addr, &mnemonic);
        };
        if input && mnemonic == "IN" {
            let Some(SlowOperand::Gpr { index, shift, mask }) = operands.first() else {
                return self.unsupported(addr, "IN destination");
            };
            let Some(port) = operands.get(1).and_then(port_expr) else {
                return self.unsupported(addr, "IN port");
            };
            let size = gpr_size(*mask);
            let _ = writeln!(
                self.out,
                "recomp_insn_in(c, {port}, {size}, &c->r[{index}], {shift}, 0x{mask:x}u);"
            );
            return Ok(true);
        }
        if !input && mnemonic == "OUT" {
            let Some(port) = operands.first().and_then(port_expr) else {
                return self.unsupported(addr, "OUT port");
            };
            let Some(SlowOperand::Gpr { index, shift, mask }) = operands.get(1) else {
                return self.unsupported(addr, "OUT source");
            };
            let size = gpr_size(*mask);
            let _ = writeln!(
                self.out,
                "recomp_insn_out(c, {port}, &c->r[{index}], {shift}, 0x{mask:x}u, {size});"
            );
            return Ok(true);
        }
        self.unsupported(addr, &mnemonic)
    }

    fn slow_segment(&mut self, insn: &Insn, addr: Addr) -> Result<bool> {
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        if matches!(mnemonic.as_str(), "LDS" | "LES") {
            let Some(operands) = slow_operands(insn) else {
                return self.unsupported(addr, &mnemonic);
            };
            let (Some(SlowOperand::Gpr { index, shift, mask }), Some(SlowOperand::Mem(address))) =
                (operands.first(), operands.get(1))
            else {
                return self.unsupported(addr, &mnemonic);
            };
            let temp = format!("a_{addr:08x}_");
            let _ = writeln!(self.out, "uint32_t {temp} = (uint32_t)({address});");
            let line = write_gpr(
                *index,
                *shift,
                *mask,
                &format!("recomp_rd32_(g_mem, {temp})"),
            );
            let _ = writeln!(self.out, "{line}");
            let _ = writeln!(self.out, "(void)recomp_rd16_(g_mem, {temp} + 4u);");
            return Ok(true);
        }
        self.unsupported(addr, &mnemonic)
    }

    fn slow_sse(&mut self, insn: &Insn, addr: Addr) -> Result<bool> {
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        if mnemonic == "SFENCE" {
            self.out
                .push_str("__atomic_thread_fence(__ATOMIC_RELEASE);\n");
            return Ok(true);
        }
        let Some(SlowOperand::Mem(address)) =
            slow_operands(insn).and_then(|ops| ops.into_iter().next())
        else {
            return self.unsupported(addr, &mnemonic);
        };
        let call = match mnemonic.as_str() {
            "FXSAVE" => format!("recomp_fxsave(c, (uint32_t)({address}));"),
            "STMXCSR" => format!("recomp_stmxcsr(c, (uint32_t)({address}));"),
            _ => {
                let at = self.guest_addr(addr);
                format!("recomp_ldmxcsr(c, {at}, (uint32_t)({address}));")
            }
        };
        let _ = writeln!(self.out, "{call}");
        Ok(true)
    }

    fn slow_mmx(&mut self, insn: &Insn, addr: Addr) -> Result<bool> {
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        if matches!(mnemonic.as_str(), "EMMS" | "FEMMS") {
            self.out.push_str("c->fpu_tag = 0xffffu;\n");
            return Ok(true);
        }
        let Some(operands) = slow_operands(insn) else {
            return self.unsupported(addr, &mnemonic);
        };
        let Some(dst) = operands.first() else {
            return self.unsupported(addr, &mnemonic);
        };
        if let Some(operator) = sse_logic(&mnemonic) {
            let (SlowOperand::Xmm(destination), Some(source)) = (dst, operands.get(1)) else {
                return self.unsupported(addr, &mnemonic);
            };
            for lane in 0..4u32 {
                let source = match source {
                    SlowOperand::Xmm(index) => format!("c->xmm[{index}][{lane}]"),
                    SlowOperand::Mem(address) => {
                        format!("recomp_rd32_(g_mem, (uint32_t)({address}) + {}u)", lane * 4)
                    }
                    _ => {
                        return self.unsupported(addr, &mnemonic);
                    }
                };
                let invert = if matches!(mnemonic.as_str(), "ANDNPS" | "ANDNPD") {
                    "~"
                } else {
                    ""
                };
                let _ = writeln!(
                    self.out,
                    "c->xmm[{destination}][{lane}] = {invert}c->xmm[{destination}][{lane}] {operator} {source};"
                );
            }
            return Ok(true);
        }
        match mnemonic.as_str() {
            "MOVD" => return self.slow_movd(dst, operands.get(1), addr),
            "MOVQ" => return self.slow_movq(dst, operands.get(1), addr),
            "CVTPI2PS" => return self.slow_cvtpi2ps(dst, operands.get(1), addr),
            "CVTPS2PI" => return self.slow_cvtps2pi(dst, operands.get(1), addr),
            "CVTTPS2DQ" => return self.slow_cvttps2dq(dst, operands.get(1), addr),
            "CVTTSD2SI" => return self.slow_cvttsd2si(dst, operands.get(1), addr),
            "PEXTRW" => return self.slow_pextrw(dst, operands.get(1), operands.get(2), addr),
            "MOVDQ2Q" => return self.slow_movdq2q(dst, operands.get(1), addr),
            "MOVNTQ" => {
                if let (SlowOperand::Mem(address), Some(SlowOperand::Mm(source))) =
                    (dst, operands.get(1))
                {
                    let _ = writeln!(
                        self.out,
                        "recomp_wr64_(g_mem, (uint32_t)({address}), c->mm[{source}]);"
                    );
                    return Ok(true);
                }
                return self.unsupported(addr, &mnemonic);
            }
            "PSHUFW" => {
                if let (SlowOperand::Mm(destination), Some(source), Some(SlowOperand::Imm(order))) =
                    (dst, operands.get(1), operands.get(2))
                {
                    let Some(source) = mmx_source(source, 64) else {
                        return self.unsupported(addr, &mnemonic);
                    };
                    let _ = writeln!(
                        self.out,
                        "c->mm[{destination}] = mmx_pshufw({source}, 0x{:x}u);",
                        order & 0xff
                    );
                    return Ok(true);
                }
                return self.unsupported(addr, &mnemonic);
            }
            _ => {}
        }
        if let SlowOperand::Xmm(destination) = dst {
            if let Some((function, bits)) = xmm_binary(&mnemonic) {
                let Some(source) = operands.get(1) else {
                    return self.unsupported(addr, &mnemonic);
                };
                let Some((source, close)) = self.xmm_pair(source) else {
                    return self.unsupported(addr, &mnemonic);
                };
                if let Some(bits) = bits {
                    let _ = writeln!(
                        self.out,
                        "{function}(c->xmm[{destination}], {source}, {bits}u);"
                    );
                } else {
                    let _ = writeln!(self.out, "{function}(c->xmm[{destination}], {source});");
                }
                if close {
                    self.out.push_str("}\n");
                }
                return Ok(true);
            }
            if let Some((function, bits)) = xmm_shift(&mnemonic) {
                let Some(count) = operands.get(1).and_then(|source| self.xmm_count(source)) else {
                    return self.unsupported(addr, &mnemonic);
                };
                self.out.push_str("{\n");
                let _ = writeln!(self.out, "uint64_t n_ = {count};");
                let _ = writeln!(self.out, "{function}(c->xmm[{destination}], n_, {bits}u);");
                self.out.push_str("}\n");
                return Ok(true);
            }
            if let Some((from, is_signed)) = xmm_pack(&mnemonic) {
                let Some(source) = operands.get(1) else {
                    return self.unsupported(addr, &mnemonic);
                };
                let Some((source, close)) = self.xmm_pair(source) else {
                    return self.unsupported(addr, &mnemonic);
                };
                let _ = writeln!(
                    self.out,
                    "xmm_pack(c->xmm[{destination}], {source}, {from}u, {is_signed}u);"
                );
                if close {
                    self.out.push_str("}\n");
                }
                return Ok(true);
            }
            if let Some((bits, high)) = xmm_unpack(&mnemonic) {
                let Some(source) = operands.get(1) else {
                    return self.unsupported(addr, &mnemonic);
                };
                let Some((source, close)) = self.xmm_pair(source) else {
                    return self.unsupported(addr, &mnemonic);
                };
                let _ = writeln!(
                    self.out,
                    "xmm_punpck(c->xmm[{destination}], {source}, {bits}u, {high});"
                );
                if close {
                    self.out.push_str("}\n");
                }
                return Ok(true);
            }
        }
        let SlowOperand::Mm(destination) = dst else {
            return self.unsupported(addr, &mnemonic);
        };
        let Some(source) = operands.get(1).and_then(|operand| mmx_source(operand, 64)) else {
            return self.unsupported(addr, &mnemonic);
        };
        if let Some((function, bits)) = mmx_shift(&mnemonic) {
            let _ = writeln!(
                self.out,
                "c->mm[{destination}] = {function}(c->mm[{destination}], {source}, {bits}u);"
            );
            return Ok(true);
        }
        if let Some((function, bits)) = mmx_binary(&mnemonic) {
            if bits == 0 {
                let _ = writeln!(
                    self.out,
                    "c->mm[{destination}] = {function}(c->mm[{destination}], {source});"
                );
            } else {
                let _ = writeln!(
                    self.out,
                    "c->mm[{destination}] = {function}(c->mm[{destination}], {source}, {bits}u);"
                );
            }
            return Ok(true);
        }
        self.unsupported(addr, &mnemonic)
    }

    fn xmm_pair(&mut self, source: &SlowOperand) -> Option<(String, bool)> {
        match source {
            SlowOperand::Xmm(index) => Some((format!("c->xmm[{index}]"), false)),
            SlowOperand::Mem(address) => {
                self.out.push_str("{\nuint32_t s_[4];\n");
                for lane in 0..4u32 {
                    let _ = writeln!(
                        self.out,
                        "s_[{lane}] = recomp_rd32_(g_mem, (uint32_t)({address}) + {}u);",
                        lane * 4
                    );
                }
                Some(("s_".to_string(), true))
            }
            _ => None,
        }
    }

    fn xmm_count(&self, source: &SlowOperand) -> Option<String> {
        match source {
            SlowOperand::Xmm(index) => Some(format!("xmm_low64(c->xmm[{index}])")),
            SlowOperand::Mem(address) => {
                Some(format!("recomp_rd64_(g_mem, (uint32_t)({address}))"))
            }
            SlowOperand::Mm(index) => Some(format!("c->mm[{index}]")),
            SlowOperand::Imm(value) => Some(format!("(uint64_t)0x{value:x}u")),
            SlowOperand::Gpr { .. } => mmx_source(source, 64),
        }
    }

    fn xmm_low_source(&self, source: &SlowOperand) -> Option<String> {
        match source {
            SlowOperand::Xmm(index) => Some(format!("xmm_low64(c->xmm[{index}])")),
            SlowOperand::Mem(address) => {
                Some(format!("recomp_rd64_(g_mem, (uint32_t)({address}))"))
            }
            SlowOperand::Mm(index) => Some(format!("c->mm[{index}]")),
            SlowOperand::Gpr { .. } | SlowOperand::Imm(_) => mmx_source(source, 64),
        }
    }

    fn slow_cvtpi2ps(
        &mut self,
        dst: &SlowOperand,
        src: Option<&SlowOperand>,
        addr: Addr,
    ) -> Result<bool> {
        let (SlowOperand::Xmm(destination), Some(source)) = (dst, src) else {
            return self.unsupported(addr, "CVTPI2PS form");
        };
        let Some(source) = mmx_source(source, 64) else {
            return self.unsupported(addr, "CVTPI2PS source");
        };
        self.out.push_str("{\n");
        let _ = writeln!(self.out, "uint64_t v_ = {source};");
        let _ = writeln!(
            self.out,
            "c->xmm[{destination}][0] = recomp_sse_f32_bits((float)(int32_t)(uint32_t)v_);"
        );
        let _ = writeln!(
            self.out,
            "c->xmm[{destination}][1] = recomp_sse_f32_bits((float)(int32_t)(uint32_t)(v_ >> 32));"
        );
        self.out.push_str("}\n");
        Ok(true)
    }

    fn slow_cvtps2pi(
        &mut self,
        dst: &SlowOperand,
        src: Option<&SlowOperand>,
        addr: Addr,
    ) -> Result<bool> {
        let (SlowOperand::Mm(destination), Some(source)) = (dst, src) else {
            return self.unsupported(addr, "CVTPS2PI form");
        };
        let Some(source) = self.xmm_low_source(source) else {
            return self.unsupported(addr, "CVTPS2PI source");
        };
        self.out.push_str("{\n");
        let _ = writeln!(self.out, "uint64_t v_ = {source};");
        let _ = writeln!(
            self.out,
            "uint32_t a_ = (uint32_t)recomp_sse_cvtss2si(recomp_sse_f32((uint32_t)v_));"
        );
        let _ = writeln!(
            self.out,
            "uint32_t b_ = (uint32_t)recomp_sse_cvtss2si(recomp_sse_f32((uint32_t)(v_ >> 32)));"
        );
        let _ = writeln!(
            self.out,
            "c->mm[{destination}] = (uint64_t)a_ | ((uint64_t)b_ << 32);"
        );
        self.out.push_str("}\n");
        Ok(true)
    }

    fn slow_cvttps2dq(
        &mut self,
        dst: &SlowOperand,
        src: Option<&SlowOperand>,
        addr: Addr,
    ) -> Result<bool> {
        let (SlowOperand::Xmm(destination), Some(source)) = (dst, src) else {
            return self.unsupported(addr, "CVTTPS2DQ form");
        };
        let Some((source, close)) = self.xmm_pair(source) else {
            return self.unsupported(addr, "CVTTPS2DQ source");
        };
        for lane in 0..4u32 {
            let _ = writeln!(
                self.out,
                "c->xmm[{destination}][{lane}] = (uint32_t)recomp_sse_cvttss2si(recomp_sse_f32({source}[{lane}]));"
            );
        }
        if close {
            self.out.push_str("}\n");
        }
        Ok(true)
    }

    fn slow_cvttsd2si(
        &mut self,
        dst: &SlowOperand,
        src: Option<&SlowOperand>,
        addr: Addr,
    ) -> Result<bool> {
        let (SlowOperand::Gpr { index, shift, mask }, Some(source)) = (dst, src) else {
            return self.unsupported(addr, "CVTTSD2SI form");
        };
        let source = match source {
            SlowOperand::Xmm(register) => format!("xmm_f64(c, {register})"),
            SlowOperand::Mem(address) => format!("rdf64((uint32_t)({address}))"),
            _ => return self.unsupported(addr, "CVTTSD2SI source"),
        };
        let line = write_gpr(
            *index,
            *shift,
            *mask,
            &format!("(uint32_t)recomp_sse_cvttsd2si({source})"),
        );
        let _ = writeln!(self.out, "{line}");
        Ok(true)
    }

    fn slow_pextrw(
        &mut self,
        dst: &SlowOperand,
        src: Option<&SlowOperand>,
        imm: Option<&SlowOperand>,
        addr: Addr,
    ) -> Result<bool> {
        let (
            SlowOperand::Gpr { index, shift, mask },
            Some(SlowOperand::Xmm(register)),
            Some(SlowOperand::Imm(order)),
        ) = (dst, src, imm)
        else {
            return self.unsupported(addr, "PEXTRW form");
        };
        let word = order & 7;
        let lane = word / 2;
        let bits = (word % 2) * 16;
        let line = write_gpr(
            *index,
            *shift,
            *mask,
            &format!("(c->xmm[{register}][{lane}] >> {bits}) & 0xffffu"),
        );
        let _ = writeln!(self.out, "{line}");
        Ok(true)
    }

    fn slow_movdq2q(
        &mut self,
        dst: &SlowOperand,
        src: Option<&SlowOperand>,
        addr: Addr,
    ) -> Result<bool> {
        let (SlowOperand::Mm(destination), Some(SlowOperand::Xmm(source))) = (dst, src) else {
            return self.unsupported(addr, "MOVDQ2Q form");
        };
        let _ = writeln!(
            self.out,
            "c->mm[{destination}] = xmm_low64(c->xmm[{source}]);"
        );
        Ok(true)
    }

    fn slow_movd(
        &mut self,
        dst: &SlowOperand,
        src: Option<&SlowOperand>,
        addr: Addr,
    ) -> Result<bool> {
        match (dst, src) {
            (SlowOperand::Mm(destination), Some(source)) => {
                let Some(source) = mmx_source(source, 32) else {
                    return self.unsupported(addr, "MOVD source");
                };
                let _ = writeln!(self.out, "c->mm[{destination}] = {source};");
            }
            (SlowOperand::Mem(address), Some(SlowOperand::Mm(source))) => {
                let _ = writeln!(
                    self.out,
                    "recomp_wr32_(g_mem, (uint32_t)({address}), (uint32_t)c->mm[{source}]);"
                );
            }
            (SlowOperand::Gpr { index, shift, mask }, Some(SlowOperand::Mm(source))) => {
                let line = write_gpr(*index, *shift, *mask, &format!("(uint32_t)c->mm[{source}]"));
                let _ = writeln!(self.out, "{line}");
            }
            (SlowOperand::Xmm(destination), Some(source)) => {
                let Some(source) = mmx_source(source, 32) else {
                    return self.unsupported(addr, "MOVD source");
                };
                self.out.push_str("{\n");
                let _ = writeln!(self.out, "uint32_t v_ = (uint32_t)({source});");
                let _ = writeln!(
                    self.out,
                    "c->xmm[{destination}][0] = v_; c->xmm[{destination}][1] = 0u; c->xmm[{destination}][2] = 0u; c->xmm[{destination}][3] = 0u;"
                );
                self.out.push_str("}\n");
            }
            (SlowOperand::Mem(address), Some(SlowOperand::Xmm(source))) => {
                let _ = writeln!(
                    self.out,
                    "recomp_wr32_(g_mem, (uint32_t)({address}), c->xmm[{source}][0]);"
                );
            }
            (SlowOperand::Gpr { index, shift, mask }, Some(SlowOperand::Xmm(source))) => {
                let line = write_gpr(*index, *shift, *mask, &format!("c->xmm[{source}][0]"));
                let _ = writeln!(self.out, "{line}");
            }
            _ => {
                return self.unsupported(addr, "MOVD form");
            }
        }
        Ok(true)
    }

    fn slow_movq(
        &mut self,
        dst: &SlowOperand,
        src: Option<&SlowOperand>,
        addr: Addr,
    ) -> Result<bool> {
        match (dst, src) {
            (SlowOperand::Mm(destination), Some(source)) => {
                let Some(source) = mmx_source(source, 64) else {
                    return self.unsupported(addr, "MOVQ source");
                };
                let _ = writeln!(self.out, "c->mm[{destination}] = {source};");
            }
            (SlowOperand::Mem(address), Some(SlowOperand::Mm(source))) => {
                let _ = writeln!(
                    self.out,
                    "recomp_wr64_(g_mem, (uint32_t)({address}), c->mm[{source}]);"
                );
            }
            (SlowOperand::Xmm(destination), Some(source)) => {
                let Some(source) = self.xmm_low_source(source) else {
                    return self.unsupported(addr, "MOVQ source");
                };
                self.out.push_str("{\n");
                let _ = writeln!(self.out, "uint64_t v_ = {source};");
                let _ = writeln!(
                    self.out,
                    "c->xmm[{destination}][0] = (uint32_t)v_; c->xmm[{destination}][1] = (uint32_t)(v_ >> 32); c->xmm[{destination}][2] = 0u; c->xmm[{destination}][3] = 0u;"
                );
                self.out.push_str("}\n");
            }
            (SlowOperand::Mem(address), Some(SlowOperand::Xmm(source))) => {
                let _ = writeln!(
                    self.out,
                    "recomp_wr64_(g_mem, (uint32_t)({address}), xmm_low64(c->xmm[{source}]));"
                );
            }
            _ => {
                return self.unsupported(addr, "MOVQ form");
            }
        }
        Ok(true)
    }

    fn call(&mut self, value: ValueId, target: Addr) -> Result<()> {
        self.publish(value)?;
        self.publish_guard(value)?;
        if self.ctx.setjmp_targets.contains(&target) {
            self.out.push_str("RECOMP_SETJMP3(c);\n");
            return Ok(());
        }
        if !self.ctx.known_entries.contains(&target) {
            self.emit_call_target(value, target)?;
            if self.plan.settle_sites.contains(&value) {
                self.out.push_str("x86_cc_settle(c);\n");
            }
            return Ok(());
        }
        if self.reloc_internal(target) {
            self.emit_call_target(value, target)?;
            if self.plan.settle_sites.contains(&value) {
                self.out.push_str("x86_cc_settle(c);\n");
            }
            return Ok(());
        }
        if let Some((private_target, wanted)) = self.plan.call_masks.get(&value).copied() {
            if private_target == target {
                if let Some(mask) = self
                    .ctx
                    .private_masks
                    .get(&target)
                    .and_then(|masks| select_mask(masks, wanted))
                {
                    let expected = self.plan.return_addr.get(&value).copied().ok_or_else(|| {
                        self.error(
                            self.s.value_addr[value as usize],
                            "call without return address",
                        )
                    })?;
                    let argument = self
                        .ctx
                        .private_input_arguments
                        .get(&target)
                        .and_then(|name| Field::from_name(name));
                    self.declarations
                        .insert(Declaration::PrivatePrepare(target));
                    self.declarations.insert(Declaration::Checked(target));
                    self.declarations.insert(Declaration::PrivateContract {
                        addr: target,
                        mask,
                        input_argument: argument.map(|field| field.name().to_string()),
                    });
                    let expected_addr = expected;
                    let expected = self.guest_addr(expected_addr);
                    let args = argument.map_or_else(
                        || format!("c, {expected}"),
                        |field| format!("c, c->r[{}], {expected}", field as usize),
                    );
                    let _ = writeln!(
                        self.out,
                        "if (entry_contract_prepare_{target:08x}(c, 0)) {{ entry_contract_{target:08x}_{:04x}({args}); }} else {{ CALL_FN_CHECKED({target:08x}, {expected}); }}", mask.0
                    );
                    self.resume_check(value, target, expected_addr);
                    if self.plan.settle_sites.contains(&value) {
                        self.out.push_str("x86_cc_settle(c);\n");
                    }
                    return Ok(());
                }
            }
        }
        let checked = self.ctx.checked_returns.contains(&target);
        if checked {
            let expected = self.plan.return_addr.get(&value).copied().ok_or_else(|| {
                self.error(
                    self.s.value_addr[value as usize],
                    "call without return address",
                )
            })?;
            self.declarations.insert(Declaration::Checked(target));
            let expected = self.guest_addr(expected);
            let _ = writeln!(self.out, "CALL_FN_CHECKED({target:08x}, {expected});");
            self.resume_check(value, target, self.plan.return_addr[&value]);
        } else {
            self.declare_entry(target);
            let _ = writeln!(self.out, "CALL_FN({target:08x});");
        }
        if self.plan.settle_sites.contains(&value) {
            self.out.push_str("x86_cc_settle(c);\n");
        }
        Ok(())
    }

    fn emit_call_target(&mut self, value: ValueId, target: Addr) -> Result<()> {
        let target = self.guest_addr(target);
        if self.ctx.reloc.is_none() {
            let _ = writeln!(self.out, "recomp_call(c, {target});");
            return Ok(());
        }
        let returned = self.plan.return_addr.get(&value).copied().ok_or_else(|| {
            self.error(
                self.s.value_addr[value as usize],
                "call without return address",
            )
        })?;
        let returned = self.guest_addr(returned);
        let _ = writeln!(
            self.out,
            "{{ unsigned reloc_depth_ = recomp_dynamic_return_push({returned}); recomp_call(c, {target}); recomp_dynamic_return_pop(reloc_depth_); }}"
        );
        Ok(())
    }

    fn call_ind(&mut self, value: ValueId, addr: Addr) -> Result<()> {
        self.publish(value)?;
        self.publish_guard(value)?;
        let args = self.s.args(value);
        let target = args
            .get(1)
            .or_else(|| args.first())
            .copied()
            .ok_or_else(|| self.error(addr, "indirect call without target"))?;
        let target = self.expr(target)?;
        let helper = if self.ctx.guest_calls.contains(&addr) {
            "recomp_icall_guest"
        } else if self.ctx.host_calls.contains(&addr) {
            "recomp_icall_host"
        } else {
            "recomp_icall"
        };
        let at = self.guest_addr(addr);
        let _ = writeln!(self.out, "{helper}(c, (uint32_t)({target}), {at});");
        if self.plan.settle_sites.contains(&value) {
            self.out.push_str("x86_cc_settle(c);\n");
        }
        Ok(())
    }

    fn divide(&mut self, value: ValueId, addr: Addr) -> Result<()> {
        self.publish(value)?;
        let args = self.s.args(value);
        let divisor = args
            .get(2)
            .copied()
            .ok_or_else(|| self.error(addr, "division without divisor"))?;
        let divisor = self.expr(divisor)?;
        let helper = match self.s.value(value).op {
            Op::Div { size: 1, .. } => "div8",
            Op::Div { size: 2, .. } => "div16",
            Op::Div { .. } => "div32",
            Op::Idiv { size: 1, .. } => "idiv8",
            Op::Idiv { size: 2, .. } => "idiv16",
            _ => "idiv32",
        };
        let at = self.guest_addr(addr);
        let _ = writeln!(self.out, "{helper}(c, (uint32_t)({divisor}), {at});");
        let _ = writeln!(
            self.out,
            "{}",
            self.assignment(value, "c->r[0] | ((uint64_t)c->r[2] << 32)")
        );
        Ok(())
    }

    fn load(&mut self, value: ValueId) -> Result<()> {
        self.publish(value)?;
        let size = self.s.value(value).size;
        let args = self.s.args(value);
        let address = self.expr(
            args.get(1)
                .copied()
                .ok_or_else(|| self.error(0, "load without address"))?,
        )?;
        let read = match size {
            1 => "rd8",
            2 => "rd16",
            4 => "rd32",
            8 => "rd64",
            _ => return Err(self.error(self.s.value_addr[value as usize], "unsupported load size")),
        };
        let helper = format!("recomp_{read}_");
        let _ = writeln!(
            self.out,
            "{}",
            self.assignment(value, &format!("{helper}(arena_, (uint32_t)({address}))"))
        );
        Ok(())
    }

    fn store(&mut self, value: ValueId) -> Result<()> {
        self.publish(value)?;
        let args = self.s.args(value);
        let address = self.expr(
            args.get(1)
                .copied()
                .ok_or_else(|| self.error(0, "store without address"))?,
        )?;
        let stored = args
            .get(2)
            .copied()
            .ok_or_else(|| self.error(0, "store without value"))?;
        let stored = self.expr(stored)?;
        let size = self.s.value(args[2]).size;
        let write = match size {
            1 => "wr8",
            2 => "wr16",
            4 => "wr32",
            8 => "wr64",
            _ => {
                return Err(self.error(self.s.value_addr[value as usize], "unsupported store size"))
            }
        };
        let helper = format!("recomp_{write}_");
        let _ = writeln!(
            self.out,
            "{helper}(arena_, (uint32_t)({address}), ({}){stored});",
            ctype(size)
        );
        Ok(())
    }

    fn branch(&mut self, value: ValueId, target: Addr) -> Result<()> {
        let current = self.block_of(value);
        if let Some(target_block) = self.successor(current, Some(target))? {
            if self.can_inline(current, target_block) {
                return self.inline_block(current, target_block);
            }
            return self.edge(Some(current), target_block);
        }
        self.publish(value)?;
        let at = self.guest_addr(self.s.value_addr[value as usize]);
        let target_expr = self.guest_addr(target);
        if self.ctx.known_entries.contains(&target) && !self.reloc_internal(target) {
            if self.checked && self.ctx.checked_returns.contains(&target) {
                self.declarations.insert(Declaration::Checked(target));
                if self.private.is_some() {
                    let _ = writeln!(
                        self.out,
                        "CALL_FN_CHECKED({target:08x}, rd32(c->r[R_ESP])); return;"
                    );
                } else {
                    self.declare_entry(target);
                    let _ = writeln!(self.out, "if (checked_return_) {{ CALL_FN_CHECKED({target:08x}, rd32(c->r[R_ESP])); }} else {{ CALL_FN({target:08x}); }} return;");
                }
            } else {
                self.declare_entry(target);
                let _ = writeln!(self.out, "CALL_FN({target:08x}); return;");
            }
        } else {
            let _ = writeln!(
                self.out,
                "c->eip = {at}; recomp_jump(c, {target_expr}); return;"
            );
        }
        Ok(())
    }

    fn cbranch(&mut self, value: ValueId, target: Addr) -> Result<()> {
        self.publish(value)?;
        let current = self.block_of(value);
        let args = self.s.args(value);
        let condition = self.expr(
            *args
                .first()
                .ok_or_else(|| self.error(0, "branch without condition"))?,
        )?;
        let taken = self.successor(current, Some(target))?;
        if let Some(taken) = taken {
            let end_addr = self.block_end_addr(current);
            let fallthrough = self
                .successors(current)
                .iter()
                .copied()
                .find(|block| Some(self.plan.block_addrs[*block]) == end_addr);
            let _ = writeln!(self.out, "if ((uint32_t)({condition}))");
            self.edge(Some(current), taken)?;
            if let Some(fallthrough) = fallthrough {
                if fallthrough != taken && self.can_inline(current, fallthrough) {
                    return self.inline_block(current, fallthrough);
                }
                self.out.push_str("else\n");
                self.edge(Some(current), fallthrough)?;
            }
            return Ok(());
        }
        let at = self.guest_addr(self.s.value_addr[value as usize]);
        let target_expr = self.guest_addr(target);
        if self.ctx.known_entries.contains(&target) && !self.reloc_internal(target) {
            self.declare_entry(target);
            let _ = writeln!(
                self.out,
                "if ((uint32_t)({condition})) {{ CALL_FN({target:08x}); return; }}"
            );
        } else {
            let _ = writeln!(
                self.out,
                "if ((uint32_t)({condition})) {{ c->eip = {at}; recomp_jump(c, {target_expr}); return; }}"
            );
        }
        let end_addr = self.block_end_addr(current);
        let fallthrough = self
            .successors(current)
            .iter()
            .copied()
            .find(|block| Some(self.plan.block_addrs[*block]) == end_addr);
        match fallthrough {
            Some(fallthrough) => self.edge(Some(current), fallthrough),
            None => Ok(()),
        }
    }

    fn branch_ind(&mut self, value: ValueId, addr: Addr) -> Result<()> {
        let args = self.s.args(value);
        let target = self.expr(
            *args
                .first()
                .ok_or_else(|| self.error(addr, "indirect jump without target"))?,
        )?;
        let current = self.block_of(value);
        let dynamic = self.has_dynamic
            && self
                .ctx
                .ir
                .index_of(addr)
                .is_some_and(|index| self.ctx.ir.dynamic.contains(&index));
        let successors = self.successors(current).to_vec();
        self.stage_transfer(current as u32)?;
        if dynamic && self.dispatch.is_some() {
            let dispatch = self.dispatch.unwrap();
            self.out.push_str("{\n");
            let start = self.guest_addr(dispatch.start);
            let _ = writeln!(self.out, "uint32_t t_ = (uint32_t)({target});");
            let _ = writeln!(self.out, "uint32_t o_ = t_ - {start};");
            let _ = writeln!(
                self.out,
                "if (o_ < 0x{:x}u) {{ uint32_t d_ = recomp_dispatch_[o_]; if (d_) goto *recomp_labels_{}[d_ - 1]; }}",
                dispatch.span,
                self.block_prefix()
            );
            let default_lines =
                self.x87_lines(|x87, names| x87.default_lines(self.clone, value, names))?;
            self.write_x87(default_lines);
            self.publish(value)?;
            let at = self.guest_addr(addr);
            let _ = writeln!(self.out, "c->eip = {at}; recomp_jump(c, t_); return;");
            self.out.push_str("}\n");
            return Ok(());
        }
        let _ = writeln!(self.out, "switch ((uint32_t)({target})) {{");
        for target_block in &successors {
            let target = self.guest_addr(self.plan.block_addrs[*target_block]);
            let _ = writeln!(self.out, "case {target}:");
            self.edge(Some(current), *target_block)?;
        }
        self.out.push_str("default:\n");
        let default_lines =
            self.x87_lines(|x87, names| x87.default_lines(self.clone, value, names))?;
        self.write_x87(default_lines);
        self.publish(value)?;
        let at = self.guest_addr(addr);
        let _ = writeln!(
            self.out,
            "c->eip = {at}; recomp_jump(c, (uint32_t)({target})); return;"
        );
        self.out.push_str("}\n");
        Ok(())
    }

    fn resume_check(&mut self, value: ValueId, target: Addr, expected: Addr) {
        if self.ctx.effect_returns.contains(&target) {
            let site = self.guest_addr(self.s.value_addr[value as usize]);
            let expected = self.guest_addr(expected);
            let _ = writeln!(
                self.out,
                "if (RECOMP_UNLIKELY(c->eip != {expected})) {{ recomp_return_resume(c, {site}, {expected}, c->eip); return; }}"
            );
        }
    }

    fn return_value(&mut self, value: ValueId) -> Result<()> {
        if let Some(private) = self.private {
            self.publish_private_return(value, private)?;
            self.out
                .push_str("if (RECOMP_UNLIKELY(c->eip != expected_return_)) {\n");
            self.publish_private_fallback(value)?;
            if !self.ctx.effect_returns.contains(&self.s.addr) {
                self.out.push_str("recomp_return_fast_(c);\n");
            }
            self.out.push_str("return;\n}\nreturn;\n");
            return Ok(());
        }
        self.publish(value)?;
        if self.ctx.effect_returns.contains(&self.s.addr) {
            self.out.push_str(
                "if (!checked_return_) { recomp_return_fast_(c); return; }\nif (RECOMP_UNLIKELY(c->eip != expected_return_)) return;\nrecomp_return_fast_(c);\nreturn;\n",
            );
            return Ok(());
        }
        if !self.ctx.ir.intra_call_returns.is_empty() {
            let current = self.block_of(value);
            let targets: Vec<usize> = self
                .successors(current)
                .iter()
                .copied()
                .filter(|target| {
                    self.ctx
                        .ir
                        .intra_call_returns
                        .contains(&self.plan.block_addrs[*target])
                })
                .collect();
            if !targets.is_empty() {
                self.out.push_str("switch (c->eip) {\n");
                for target in targets {
                    let target_addr = self.guest_addr(self.plan.block_addrs[target]);
                    let _ = writeln!(self.out, "case {target_addr}:");
                    self.edge(Some(current), target)?;
                }
                self.out.push_str("default:\n");
                if self.checked {
                    self.out.push_str(
                        "if (!checked_return_ || RECOMP_UNLIKELY(c->eip != expected_return_)) recomp_return_fast_(c);\nreturn;\n",
                    );
                } else {
                    self.out.push_str("recomp_return_fast_(c);\nreturn;\n");
                }
                self.out.push_str("}\n");
                return Ok(());
            }
        }
        if self.checked {
            self.out.push_str(
                "if (!checked_return_ || RECOMP_UNLIKELY(c->eip != expected_return_)) recomp_return_fast_(c);\nreturn;\n",
            );
        } else {
            self.out.push_str("recomp_return_fast_(c);\nreturn;\n");
        }
        Ok(())
    }

    fn publish_private_return(&mut self, value: ValueId, private: &PrivateEmit) -> Result<()> {
        let Some(keys) = self.plan.publish.get(&value).cloned() else {
            return Ok(());
        };
        let keys: Vec<StateKey> = keys
            .into_iter()
            .filter(|key| {
                self.s
                    .field(*key)
                    .is_none_or(|field| private.mask.contains(field))
            })
            .collect();
        self.publish_keys(value, &keys, true)
    }

    fn publish_private_fallback(&mut self, value: ValueId) -> Result<()> {
        let Some(keys) = self.plan.publish.get(&value).cloned() else {
            return Ok(());
        };
        let Some(state) = self.s.boundary(value) else {
            return Ok(());
        };
        let keys: Vec<StateKey> = keys
            .into_iter()
            .filter(|key| state.iter().any(|(candidate, _)| candidate == key))
            .collect();
        self.publish_keys(value, &keys, true)
    }

    fn seh(&mut self, kind: SehEffect, addr: Addr) {
        if kind == SehEffect::Leave {
            let at = self.guest_addr(addr);
            let _ = writeln!(self.out, "c->eip = {at};");
        }
        match kind {
            SehEffect::Enter | SehEffect::Adopt => unreachable!(),
            SehEffect::Leave => self.out.push_str("recomp_seh_frame_leave(c);\n"),
            SehEffect::Landing => {
                let at = self.guest_addr(addr);
                let _ = writeln!(
                    self.out,
                    "if (recomp_seh_pending_target()) recomp_seh_intercept(c, {at});"
                );
            }
            SehEffect::Orphan => {
                self.out
                    .push_str("recomp_seh_frame_orphan(c, seh_mark_);\n");
            }
        }
    }

    fn publish(&mut self, value: ValueId) -> Result<()> {
        let Some(keys) = self.plan.publish.get(&value).cloned() else {
            return Ok(());
        };
        self.publish_keys(value, &keys, true)
    }

    fn publish_guard(&mut self, value: ValueId) -> Result<()> {
        let Some(keys) = self.plan.guard.get(&value).cloned() else {
            return Ok(());
        };
        self.out
            .push_str("if (RECOMP_UNLIKELY(recomp_hooks_ever)) {\n");
        self.publish_keys(value, &keys, false)?;
        self.out.push_str("}\n");
        Ok(())
    }

    fn publish_keys(
        &mut self,
        value: ValueId,
        keys: &[StateKey],
        flags_action: bool,
    ) -> Result<()> {
        let Some(state) = self.s.boundary(value) else {
            return Ok(());
        };
        let state: BTreeMap<StateKey, ValueId> = state
            .iter()
            .copied()
            .filter(|(_, value)| flags_action || self.available(*value))
            .collect();
        let mut locations = BTreeSet::new();
        for key in keys {
            if let Some(Some(location)) = self.key_location.get(key) {
                locations.insert(*location);
            }
        }
        let flag_fields: Fields = keys
            .iter()
            .filter_map(|key| self.s.field(*key))
            .filter(|field| Fields::FLAGS.contains(*field))
            .collect();
        let publication = self.plan.flag_publish.get(&value);
        let descriptor = publication
            .and_then(|publication| publication.descriptor)
            .map(|descriptor| (descriptor, descriptor.mask & flag_fields))
            .filter(|(_, mask)| !mask.is_empty());
        let forwarded =
            publication.map_or(Fields::NONE, |publication| publication.forwarded) & flag_fields;
        let mask = descriptor.map_or(Fields::NONE, |(_, mask)| mask);
        let written = mask | (flag_fields & !mask & !forwarded);
        if !written.is_empty() {
            let need = cc_mask(Fields::FLAGS.difference(written));
            if need == 0 {
                self.out.push_str("x86_cc_drop(c);\n");
            } else {
                let _ = writeln!(self.out, "x86_cc_settle_or_drop(c, 0x{need:02x}u);");
            }
        }
        if let Some((_, mask)) = descriptor {
            locations.retain(
                |location| !matches!(location, Location::Flag(field) if mask.contains(*field)),
            );
        }
        if !forwarded.is_empty() {
            locations.retain(
                |location| !matches!(location, Location::Flag(field) if forwarded.contains(*field)),
            );
        }
        for location in locations {
            self.publish_location(location, &state, keys)?;
        }
        if let Some((descriptor, mask)) = descriptor {
            self.publish_descriptor(&descriptor, mask)?;
        }
        Ok(())
    }

    fn publish_descriptor(&mut self, descriptor: &FlagDescriptor, mask: Fields) -> Result<()> {
        let op = match (descriptor.kind, descriptor.kind_value) {
            (rk_ssa::FlagKind::Join, Some(value)) => self.expr(value)?,
            (kind, _) => match rk_ssa::cc_kind(kind) {
                Some(kind) => support::cc_kind_name(kind).to_string(),
                None => return Ok(()),
            },
        };
        let (a, b, carry) = match descriptor.kind {
            rk_ssa::FlagKind::Logic => ("0u".to_string(), "0u".to_string(), "0u".to_string()),
            rk_ssa::FlagKind::Inc | rk_ssa::FlagKind::Dec => (
                self.expr(descriptor.a.unwrap_or(0))?,
                "0u".to_string(),
                "0u".to_string(),
            ),
            rk_ssa::FlagKind::Adc | rk_ssa::FlagKind::Sbb | rk_ssa::FlagKind::Join => (
                self.expr(descriptor.a.unwrap_or(0))?,
                self.expr(descriptor.b.unwrap_or(0))?,
                self.expr(descriptor.carry.unwrap_or(0))?,
            ),
            _ => (
                self.expr(descriptor.a.unwrap_or(0))?,
                self.expr(descriptor.b.unwrap_or(0))?,
                "0u".to_string(),
            ),
        };
        let result = self.expr(descriptor.result)?;
        let _ = writeln!(self.out, "c->cc_op = {op};");
        let _ = writeln!(self.out, "c->cc_size = {}u;", descriptor.size);
        let _ = writeln!(self.out, "c->cc_mask = 0x{:02x}u;", cc_mask(mask));
        let _ = writeln!(self.out, "c->cc_carry = (uint8_t)(({carry}) & 1u);");
        let _ = writeln!(self.out, "c->cc_a = (uint32_t)({a});");
        let _ = writeln!(self.out, "c->cc_b = (uint32_t)({b});");
        let _ = writeln!(self.out, "c->cc_res = (uint32_t)({result});");
        Ok(())
    }

    fn available(&self, value: ValueId) -> bool {
        if let Some(known) = self.available.borrow()[value as usize] {
            return known;
        }
        let result = if self.materialized[value as usize] {
            self.plan.live.contains(&value)
        } else {
            match self.s.value(value).op {
                Op::Const(_) | Op::Target(_) | Op::Memory | Op::Input(_) | Op::StateReload(_) => {
                    true
                }
                Op::Byte(_)
                | Op::Pack
                | Op::StateKeep(_)
                | Op::P(OpCode::Copy | OpCode::IntZExt) => {
                    self.s.args(value).iter().all(|arg| self.available(*arg))
                }
                _ => false,
            }
        };
        self.available.borrow_mut()[value as usize] = Some(result);
        result
    }

    fn publish_location(
        &mut self,
        location: Location,
        state: &BTreeMap<StateKey, ValueId>,
        keys: &[StateKey],
    ) -> Result<()> {
        match location {
            Location::Gpr(index) => {
                let field = Field::ALL[index];
                let lanes = rk_ir::lanes(field);
                let mut values = Vec::with_capacity(4);
                for lane in 0..4 {
                    let Some(&key) = lanes.get(lane) else {
                        values.push(None);
                        continue;
                    };
                    values.push(self.listed_value(state, keys, key)?);
                }
                self.publish_lanes(&format!("r[{index}]"), values)?;
            }
            Location::Flag(field) => {
                let key = rk_ir::lanes(field).first().copied();
                let value = match key {
                    Some(key) => self.listed_value(state, keys, key)?,
                    None => None,
                };
                let expr = match value {
                    Some(value) => self.expr(value)?,
                    None => format!("c->eflags_{}", field.name().to_ascii_lowercase()),
                };
                let _ = writeln!(
                    self.out,
                    "c->eflags_{} = (uint32_t)(({expr}) & 1u);",
                    field.name().to_ascii_lowercase()
                );
            }
            Location::Xmm(reg, lane) => {
                let register = Registers::get().require(&format!("XMM{reg}"));
                let mut values = Vec::with_capacity(4);
                for byte in 0..4 {
                    let key = StateKey::Register(
                        (register.offset + lane as u64 * 4 + byte as u64) as u32,
                    );
                    values.push(self.listed_value(state, keys, key)?);
                }
                let text = match values.iter().copied().collect::<Option<Vec<_>>>() {
                    Some(all) => self.pack_lanes(&all),
                    None => None,
                };
                let expr = match text {
                    Some(text) => text,
                    None => values
                        .iter()
                        .enumerate()
                        .map(|(byte, value)| {
                            let expr = match value {
                                Some(value) => self.expr(*value),
                                None => {
                                    Ok(format!("((c->xmm[{reg}][{lane}] >> {}) & 0xffu)", byte * 8))
                                }
                            }?;
                            Ok(format!("((({expr}) & 0xffu) << {})", byte * 8))
                        })
                        .collect::<Result<Vec<_>>>()?
                        .join(" | "),
                };
                let _ = writeln!(self.out, "c->xmm[{reg}][{lane}] = (uint32_t)({expr});");
            }
            Location::Eip => self.publish_register("eip", "EIP", state, keys)?,
            Location::Fs => self.publish_register("fs_base", "FS_OFFSET", state, keys)?,
            Location::Df => {
                let key = StateKey::Register(Registers::get().require("DF").offset as u32);
                let value = self.listed_value(state, keys, key)?;
                let expr = match value {
                    Some(value) => self.expr(value)?,
                    None => "c->eflags_df".to_string(),
                };
                let _ = writeln!(self.out, "c->eflags_df = (uint32_t)(({expr}) & 1u);");
            }
        }
        Ok(())
    }

    fn publish_register(
        &mut self,
        field: &str,
        register: &str,
        state: &BTreeMap<StateKey, ValueId>,
        keys: &[StateKey],
    ) -> Result<()> {
        let varnode = Registers::get().require(register);
        let mut values = Vec::with_capacity(varnode.size as usize);
        for lane in 0..varnode.size {
            let key = StateKey::Register((varnode.offset + lane as u64) as u32);
            values.push(self.listed_value(state, keys, key)?);
        }
        self.publish_lanes(field, values)
    }

    fn listed_value(
        &self,
        state: &BTreeMap<StateKey, ValueId>,
        keys: &[StateKey],
        key: StateKey,
    ) -> Result<Option<ValueId>> {
        if !keys.contains(&key) {
            return Ok(None);
        }
        let value = state
            .get(&key)
            .copied()
            .ok_or_else(|| self.error(0, "missing publish value"))?;
        if !self.available(value) {
            return Err(self.error(0, "unavailable publish value"));
        }
        Ok(Some(value))
    }

    fn publish_lanes(&mut self, field: &str, values: Vec<Option<ValueId>>) -> Result<()> {
        if let Some(all) = values.iter().copied().collect::<Option<Vec<_>>>() {
            if let Some(text) = self.pack_lanes(&all) {
                let _ = writeln!(self.out, "c->{field} = {text};");
                return Ok(());
            }
        }
        let mut parts = Vec::new();
        for (lane, value) in values.into_iter().enumerate() {
            let expr = match value {
                Some(value) => self.expr(value)?,
                None => format!("((c->{field} >> {}) & 0xffu)", lane * 8),
            };
            parts.push(format!("((({expr}) & 0xffu) << {})", lane * 8));
        }
        let _ = writeln!(self.out, "c->{field} = {};", parts.join(" | "));
        Ok(())
    }

    fn successor(&self, current: usize, target: Option<Addr>) -> Result<Option<usize>> {
        let successors = self.successors(current);
        match target {
            Some(addr) => Ok(successors
                .iter()
                .copied()
                .find(|block| self.plan.block_addrs[*block] == addr)),
            None => successors
                .first()
                .copied()
                .map(Some)
                .ok_or_else(|| self.error(self.s.addr, "block has no fallthrough")),
        }
    }

    fn successors(&self, current: usize) -> &[usize] {
        &self.successors[current]
    }

    fn block_of(&self, value: ValueId) -> usize {
        self.value_block[value as usize] as usize
    }

    fn block_end_addr(&self, block: usize) -> Option<Addr> {
        let insns = &self.ctx.ir.insns;
        let end = self.s.blocks[block].end;
        (end > 0)
            .then(|| insns.get(end as usize - 1))
            .flatten()
            .map(|insn| insn.addr + insn.length as Addr)
    }

    fn edge(&mut self, source: Option<usize>, target: usize) -> Result<()> {
        let pred = source.map_or(DISPATCH_BLOCK, |block| block as u32);
        if self.transfers.edges.contains(&(pred, target)) {
            let _ = writeln!(self.out, "goto E{}{target};", self.block_prefix());
            return Ok(());
        }
        self.out.push_str("{\n");
        self.edge_copies(pred, target)?;
        let _ = writeln!(self.out, "goto {}{target};", self.block_prefix());
        self.out.push_str("}\n");
        Ok(())
    }

    fn stage_transfer(&mut self, source: u32) -> Result<()> {
        if let Some(copies) = self.transfers.sources.get(&source) {
            for &(channel, arg) in copies {
                let expr = self.expr(arg)?;
                let _ = writeln!(
                    self.out,
                    "e{channel} = ({})({expr});",
                    ctype(self.transfers.sizes[channel])
                );
            }
        }
        Ok(())
    }

    fn edge_copies(&mut self, pred: u32, target: usize) -> Result<()> {
        let block = &self.s.blocks[target];
        let position = block.preds.iter().position(|candidate| *candidate == pred);
        if let Some(position) = position {
            let phis: Vec<_> = block
                .phis
                .iter()
                .copied()
                .filter(|phi| self.plan.live.contains(phi) && self.s.value(*phi).size != 0)
                .collect();
            let destinations = phis.iter().copied().collect();
            let parallel = phis
                .iter()
                .any(|phi| self.reads_phi(self.s.args(*phi)[position], &destinations));
            for &phi in &phis {
                let arg = *self
                    .s
                    .args(phi)
                    .get(position)
                    .ok_or_else(|| self.error(0, "phi arity"))?;
                let ctype = ctype(self.s.value(phi).size);
                let expr = self.expr(arg)?;
                if parallel {
                    let _ = writeln!(self.out, "{ctype} p{phi} = ({ctype})({expr});");
                } else {
                    let _ = writeln!(self.out, "v{phi} = ({ctype})({expr});");
                }
            }
            if parallel {
                for &phi in &phis {
                    let _ = writeln!(self.out, "v{phi} = p{phi};");
                }
            }
        }
        Ok(())
    }

    fn reads_phi(&self, value: ValueId, phis: &BTreeSet<ValueId>) -> bool {
        if matches!(self.s.value(value).op, Op::Byte(_)) {
            return phis.contains(&value)
                || self
                    .s
                    .args(value)
                    .iter()
                    .any(|arg| self.reads_phi(*arg, phis));
        }
        if let Some((carrier, _)) = self.keep_lane.get(&value) {
            return phis.contains(carrier);
        }
        if self.materialized[value as usize] {
            return phis.contains(&value);
        }
        match self.s.value(value).op {
            Op::Pack | Op::StateKeep(_) | Op::P(OpCode::Copy | OpCode::IntZExt) => self
                .s
                .args(value)
                .iter()
                .any(|arg| self.reads_phi(*arg, phis)),
            _ => phis.contains(&value),
        }
    }

    fn declare_entry(&mut self, target: Addr) {
        self.declarations.insert(Declaration::Entry(target));
    }

    fn assignment(&self, value: ValueId, expr: &str) -> String {
        let size = self.s.value(value).size;
        let ctype = ctype(size);
        if matches!(size, 4 | 8) {
            format!("v{value} = ({ctype})({expr});")
        } else {
            format!("v{value} = ({ctype})(({expr}) & {});", c_mask(size))
        }
    }

    fn expr(&self, value: ValueId) -> Result<String> {
        if let Some((carrier, lane)) = self.keep_lane.get(&value).copied() {
            return Ok(if lane == 0 {
                format!("(v{carrier} & 0xffu)")
            } else {
                format!("((v{carrier} >> {}) & 0xffu)", lane as u32 * 8)
            });
        }
        if self.materialized[value as usize] {
            return Ok(format!("v{value}"));
        }
        let item = *self.s.value(value);
        match item.op {
            Op::Const(data) => Ok(const_expr(data, item.size)),
            Op::Input(key) | Op::StateReload(key) => self.field_expr(key, item.size),
            Op::CcKind(kind) => Ok(cc_kind_name(kind).to_string()),
            Op::Target(addr) => Ok(self.guest_addr(addr)),
            Op::Memory => Ok("0u".to_string()),
            Op::Byte(index) => Ok(format!(
                "((({}) >> {}) & 0xffu)",
                self.expr(
                    *self
                        .s
                        .args(value)
                        .first()
                        .ok_or_else(|| self.error(0, "missing argument"))?
                )?,
                index as u32 * 8
            )),
            Op::Pack => match self.pack_lanes(self.s.args(value)) {
                Some(text) => Ok(text),
                None => self.pack_bytes(self.s.args(value), item.size > 4),
            },
            Op::StateKeep(key) => {
                let old = self.expr(
                    *self
                        .s
                        .args(value)
                        .get(1)
                        .ok_or_else(|| self.error(0, "missing argument"))?,
                )?;
                let loaded = self.field_expr(key, item.size)?;
                let loaded =
                    if field_lane(key).is_some_and(|(field, _)| Fields::FLAGS.contains(field)) {
                        format!("(x86_cc_settle(c), {loaded})")
                    } else {
                        loaded
                    };
                Ok(keep_expr(&loaded, &old))
            }
            Op::P(OpCode::Copy) | Op::P(OpCode::IntZExt) => self.expr(
                *self
                    .s
                    .args(value)
                    .first()
                    .ok_or_else(|| self.error(0, "missing argument"))?,
            ),
            _ => Ok(format!("v{value}")),
        }
    }

    fn packed_float_arg_ok(&self, value: ValueId) -> bool {
        if self.materialized[value as usize] {
            return self.plan.live.contains(&value);
        }
        match self.s.value(value).op {
            Op::Const(_) | Op::Target(_) | Op::Memory => true,
            Op::Input(StateKey::Register(_)) | Op::StateReload(StateKey::Register(_)) => true,
            Op::Byte(_) | Op::Pack | Op::P(OpCode::Copy | OpCode::IntZExt) => self
                .s
                .args(value)
                .iter()
                .all(|arg| self.packed_float_arg_ok(*arg)),
            _ => false,
        }
    }

    fn packed_float4(&self, ops: &[ValueId], position: usize) -> Result<Option<OpCode>> {
        let code = match self.s.value(ops[position]).op {
            Op::P(
                code @ (OpCode::FloatAdd | OpCode::FloatSub | OpCode::FloatMult | OpCode::FloatDiv),
            ) => code,
            _ => return Ok(None),
        };
        let Some(group) = ops.get(position..position + 4) else {
            return Ok(None);
        };
        let addr = self.s.value_addr[ops[position] as usize];
        for &value in group {
            let item = self.s.value(value);
            if !self.plan.live.contains(&value)
                || !self.materialized[value as usize]
                || item.op != Op::P(code)
                || item.size != 4
                || self.s.value_addr[value as usize] != addr
                || self.reload_defs.contains_key(&value)
                || self.keep_lane.get(&value).is_some()
            {
                return Ok(None);
            }
            let args = self.s.args(value);
            if args.len() != 2
                || args.iter().any(|arg| self.s.value(*arg).size != 4)
                || !args.iter().all(|arg| self.packed_float_arg_ok(*arg))
            {
                return Ok(None);
            }
            let lines = self.x87_lines(|x87, names| x87.event_lines(self.clone, value, names))?;
            if !lines.is_empty() {
                return Ok(None);
            }
        }
        Ok(Some(code))
    }

    fn packed_float4_emit(&mut self, code: OpCode, values: &[ValueId]) -> Result<()> {
        let helper = match code {
            OpCode::FloatAdd => "add",
            OpCode::FloatSub => "sub",
            OpCode::FloatMult => "mul",
            _ => "div",
        };
        let mut args = Vec::with_capacity(8);
        for &value in values {
            for &arg in self.s.args(value) {
                args.push(format!("(uint32_t)({})", self.expr(arg)?));
            }
        }
        let outputs: Vec<_> = values.iter().map(|value| format!("&v{value}")).collect();
        let _ = writeln!(
            self.out,
            "recomp_sse_{helper}ps4(c, {}, {});",
            args.join(", "),
            outputs.join(", ")
        );
        Ok(())
    }

    fn operation(&self, value: ValueId) -> Result<String> {
        let item = *self.s.value(value);
        let args = self.s.args(value);
        let a = |index: usize| -> Result<String> {
            self.expr(
                *args
                    .get(index)
                    .ok_or_else(|| self.error(0, "missing argument"))?,
            )
        };
        let wide = item.size > 4 || item.op == Op::P(OpCode::SubPiece);
        let cast = |text: String| {
            if wide {
                format!("(uint64_t)({text})")
            } else {
                text
            }
        };
        let zero = if wide { "0ull" } else { "0u" };
        let one = if wide { "1ull" } else { "1u" };
        if let Op::P(
            OpCode::FloatNan
            | OpCode::FloatEqual
            | OpCode::FloatLess
            | OpCode::FloatAdd
            | OpCode::FloatSub
            | OpCode::FloatMult
            | OpCode::FloatDiv
            | OpCode::FloatMin
            | OpCode::FloatMax
            | OpCode::Cvtss2si
            | OpCode::Cvttss2si
            | OpCode::Cvtsi2ss,
        ) = item.op
        {
            if args.iter().any(|arg| self.s.value(*arg).size != 4) {
                return Err(self.error(0, "non-binary32 scalar float operand"));
            }
        }
        let result = match item.op {
            Op::Const(data) => const_expr(data, item.size),
            Op::Input(key) => self.field_expr(key, item.size)?,
            Op::CcKind(kind) => cc_kind_name(kind).to_string(),
            Op::Target(addr) => self.guest_addr(addr),
            Op::Byte(index) => format!("(({}) >> {})", a(0)?, index as u32 * 8),
            Op::Pack => match self.pack_lanes(args) {
                Some(text) => text,
                None => self.pack_bytes(args, item.size > 4)?,
            },
            Op::StateReload(key) => self.field_expr(key, item.size)?,
            Op::StateKeep(key) => {
                let old = a(1)?;
                let loaded = self.field_expr(key, item.size)?;
                let loaded =
                    if field_lane(key).is_some_and(|(field, _)| Fields::FLAGS.contains(field)) {
                        format!("(x86_cc_settle(c), {loaded})")
                    } else {
                        loaded
                    };
                keep_expr(&loaded, &old)
            }
            Op::Memory => "0u".to_string(),
            Op::Phi => format!("v{value}"),
            Op::P(code) => match code {
                OpCode::FloatNan => format!("!!isnan(recomp_sse_f32((uint32_t)({})))", a(0)?),
                OpCode::FloatEqual => format!(
                    "(recomp_sse_f32((uint32_t)({})) == recomp_sse_f32((uint32_t)({})))",
                    a(0)?,
                    a(1)?
                ),
                OpCode::FloatNotEqual => format!(
                    "(recomp_sse_f32((uint32_t)({})) != recomp_sse_f32((uint32_t)({})))",
                    a(0)?,
                    a(1)?
                ),
                OpCode::FloatLess => format!(
                    "(recomp_sse_f32((uint32_t)({})) < recomp_sse_f32((uint32_t)({})))",
                    a(0)?,
                    a(1)?
                ),
                OpCode::FloatLessEqual => format!(
                    "(recomp_sse_f32((uint32_t)({})) <= recomp_sse_f32((uint32_t)({})))",
                    a(0)?,
                    a(1)?
                ),
                OpCode::FloatSqrt => format!("recomp_sse_sqrtps((uint32_t)({}))", a(0)?),
                OpCode::FloatRcp => format!("recomp_sse_rcpps((uint32_t)({}))", a(0)?),
                OpCode::FloatRsqrt => format!("recomp_sse_rsqrtps((uint32_t)({}))", a(0)?),
                OpCode::FloatAdd
                | OpCode::FloatSub
                | OpCode::FloatMult
                | OpCode::FloatDiv
                | OpCode::FloatMin
                | OpCode::FloatMax => {
                    let helper = match code {
                        OpCode::FloatAdd => "add",
                        OpCode::FloatSub => "sub",
                        OpCode::FloatMult => "mul",
                        OpCode::FloatDiv => "div",
                        OpCode::FloatMin => "min",
                        _ => "max",
                    };
                    let context = if matches!(code, OpCode::FloatMin | OpCode::FloatMax) {
                        ""
                    } else {
                        "c, "
                    };
                    format!(
                        "recomp_sse_{helper}ps({context}(uint32_t)({}), (uint32_t)({}))",
                        a(0)?,
                        a(1)?
                    )
                }
                OpCode::Cvtss2si | OpCode::Cvttss2si => {
                    let helper = if code == OpCode::Cvtss2si {
                        "cvtss2si"
                    } else {
                        "cvttss2si"
                    };
                    format!(
                        "(uint32_t)recomp_sse_{helper}(recomp_sse_f32((uint32_t)({})))",
                        a(0)?
                    )
                }
                OpCode::Cvtsi2ss => {
                    format!("recomp_sse_f32_bits((float)(int32_t)(uint32_t)({}))", a(0)?)
                }
                OpCode::Copy | OpCode::IntZExt => a(0)?,
                OpCode::IntSExt => {
                    let from = args.first().map_or(1, |arg| self.s.value(*arg).size) * 8;
                    let to = item.size * 8;
                    format!("(uint{to}_t)(int{from}_t)({})", a(0)?)
                }
                OpCode::IntAdd => format!("{} + {}", cast(a(0)?), cast(a(1)?)),
                OpCode::IntSub => format!("{} - {}", cast(a(0)?), cast(a(1)?)),
                OpCode::IntMult => format!("{} * {}", cast(a(0)?), cast(a(1)?)),
                OpCode::IntAnd => format!("{} & {}", cast(a(0)?), cast(a(1)?)),
                OpCode::IntOr => format!("{} | {}", cast(a(0)?), cast(a(1)?)),
                OpCode::IntXor => format!("{} ^ {}", cast(a(0)?), cast(a(1)?)),
                OpCode::IntEqual => format!("(({}) == ({}))", a(0)?, a(1)?),
                OpCode::IntNotEqual => format!("(({}) != ({}))", a(0)?, a(1)?),
                OpCode::IntLess => format!("(({}) < ({}))", a(0)?, a(1)?),
                OpCode::IntLessEqual => format!("(({}) <= ({}))", a(0)?, a(1)?),
                OpCode::IntSLess | OpCode::IntSLessEqual => {
                    let size = args.first().map_or(4, |arg| self.s.value(*arg).size);
                    let op = if code == OpCode::IntSLess { "<" } else { "<=" };
                    if matches!(size, 1 | 2 | 4 | 8) {
                        let ty = signed_type(size);
                        format!("(({ty})({}) {op} ({ty})({}))", a(0)?, a(1)?)
                    } else {
                        let sign = const_expr(1u64 << (size * 8 - 1), size);
                        format!("((({}) ^ {sign}) {op} (({}) ^ {sign}))", a(0)?, a(1)?)
                    }
                }
                OpCode::IntLeft | OpCode::IntRight => {
                    let bits = args.first().map_or(32, |arg| self.s.value(*arg).size) * 8;
                    let op = if code == OpCode::IntLeft { "<<" } else { ">>" };
                    format!(
                        "(({}) >= {bits} ? {zero} : (({}) {op} ({})))",
                        a(1)?,
                        a(0)?,
                        a(1)?
                    )
                }
                OpCode::IntSRight => {
                    let bits = args.first().map_or(32, |arg| self.s.value(*arg).size) * 8;
                    let sign = if bits >= 64 {
                        format!("0x{:x}ull", 1u64 << 63)
                    } else {
                        format!("0x{:x}u", 1u32 << (bits - 1))
                    };
                    format!("(({}) >= {bits} ? (({}) - (({}) >> {})) : (((({}) ^ {sign}) >> ({})) - ({sign} >> ({}))))",
                        a(1)?, zero, a(0)?, bits - 1, a(0)?, a(1)?, a(1)?)
                }
                OpCode::IntCarry => {
                    let operand = c_mask(args.first().map_or(4, |arg| self.s.value(*arg).size));
                    format!("(({}) > ({operand} - ({})))", a(0)?, a(1)?)
                }
                OpCode::IntSCarry | OpCode::IntSBorrow => {
                    let bits = args.first().map_or(32, |arg| self.s.value(*arg).size) * 8;
                    let op = if code == OpCode::IntSCarry { "+" } else { "-" };
                    let not = if code == OpCode::IntSCarry { "~" } else { "" };
                    let x = a(0)?;
                    let y = a(1)?;
                    let diff = format!("(({x}) ^ ({y}))");
                    let sum = format!("(({x}) {op} ({y}))");
                    let merged = format!("({not}{diff} & (({x}) ^ {sum}))");
                    format!("(({merged} >> {}) & {one})", bits - 1)
                }
                OpCode::Int2Comp => format!("{zero} - {}", cast(a(0)?)),
                OpCode::IntNegate => format!("~({})", cast(a(0)?)),
                OpCode::BoolNegate => format!("!({})", a(0)?),
                OpCode::BoolXor => format!("(({}) != ({}))", a(0)?, a(1)?),
                OpCode::BoolAnd => format!("(({}) && ({}))", a(0)?, a(1)?),
                OpCode::BoolOr => format!("(({}) || ({}))", a(0)?, a(1)?),
                OpCode::IntDiv => format!("(({}) / ({}))", cast(a(0)?), cast(a(1)?)),
                OpCode::IntRem => format!("(({}) % ({}))", cast(a(0)?), cast(a(1)?)),
                OpCode::IntSDiv => format!(
                    "(({})({}) / ({})({}))",
                    signed_type(item.size),
                    a(0)?,
                    signed_type(item.size),
                    a(1)?
                ),
                OpCode::IntSRem => format!(
                    "(({})({}) % ({})({}))",
                    signed_type(item.size),
                    a(0)?,
                    signed_type(item.size),
                    a(1)?
                ),
                OpCode::PopCount => {
                    let helper = if args.first().map_or(4, |arg| self.s.value(*arg).size) > 4 {
                        "__builtin_popcountll"
                    } else {
                        "__builtin_popcount"
                    };
                    format!("{helper}(({}))", a(0)?)
                }
                OpCode::LzCount => {
                    let bits = args.first().map_or(4, |arg| self.s.value(*arg).size) * 8;
                    let helper = if bits > 32 {
                        "__builtin_clzll"
                    } else {
                        "__builtin_clz"
                    };
                    format!(
                        "(({}) ? ({helper}(({})) - {}) : {bits})",
                        a(0)?,
                        a(0)?,
                        32u32.saturating_sub(bits as u32)
                    )
                }
                OpCode::Piece => {
                    let shift = args.get(1).map_or(0, |arg| self.s.value(*arg).size) * 8;
                    if shift >= 32 {
                        format!(
                            "((((uint64_t)({})) << {shift}) | ((uint64_t)({})))",
                            a(0)?,
                            a(1)?
                        )
                    } else {
                        format!("((({}) << {shift}) | ({}))", a(0)?, a(1)?)
                    }
                }
                OpCode::SubPiece => format!(
                    "(({}) >= 8 ? 0ull : (({}) >> (({}) * 8)))",
                    a(1)?,
                    a(0)?,
                    a(1)?
                ),
                _ => {
                    return Err(self.error(
                        self.s.value_addr[value as usize],
                        &format!("unsupported p-code {code:?} at v{value}"),
                    ))
                }
            },
            _ => {
                return Err(self.error(
                    self.s.value_addr[value as usize],
                    &format!("unsupported SSA op {:?} at v{value}", item.op),
                ))
            }
        };
        Ok(result)
    }

    fn field_expr(&self, key: StateKey, size: u8) -> Result<String> {
        if size <= 1 {
            return self.lane_expr(key);
        }
        let (base, lane, width) = self
            .lane_origin(key)
            .ok_or_else(|| self.error(0, &format!("unsupported register lane {key:?}")))?;
        let size = size as u32;
        if lane + size > width {
            return Err(self.error(
                0,
                &format!("register field {key:?} has no {size}-byte slice"),
            ));
        }
        if lane == 0 && size == width {
            Ok(base)
        } else if lane == 0 {
            Ok(format!("({base} & 0x{:x}u)", (1u64 << (size * 8)) - 1))
        } else {
            Ok(format!(
                "(({base} >> {}) & 0x{:x}u)",
                lane * 8,
                (1u64 << (size * 8)) - 1
            ))
        }
    }

    fn lane_expr(&self, key: StateKey) -> Result<String> {
        let (base, lane, width) = self
            .lane_origin(key)
            .ok_or_else(|| self.error(0, &format!("unsupported register lane {key:?}")))?;
        if width == 1 {
            Ok(format!("({base} & 0xffu)"))
        } else {
            Ok(format!("(({base} >> {}) & 0xffu)", lane * 8))
        }
    }

    fn lane_origin(&self, key: StateKey) -> Option<(String, u32, u32)> {
        if let Some((field, lane)) = field_lane(key) {
            if (field as usize) < 8 {
                if let Some(private) = self.private {
                    if private.input_argument == Some(field) {
                        return Some((format!("{}_", field.name().to_lowercase()), lane as u32, 4));
                    }
                }
                return Some((format!("c->r[{}]", field as usize), lane as u32, 4));
            }
            return Some((
                format!("c->eflags_{}", field.name().to_ascii_lowercase()),
                0,
                1,
            ));
        }
        if let Some((reg, offset)) = rk_ir::xmm_offset(key) {
            return Some((
                format!("c->xmm[{reg}][{}]", offset / 4),
                (offset % 4) as u32,
                4,
            ));
        }
        let StateKey::Register(offset) = key else {
            return None;
        };
        let (name, register) = Registers::get().covering(offset as u64)?;
        let lane = (offset as u64 - register.offset) as u32;
        let base = match name {
            "EIP" | "IP" => "c->eip",
            "FS_OFFSET" => "c->fs_base",
            "DF" => "c->eflags_df",
            "ES" | "DS" | "SS" => "0x23u",
            "CS" => "0x1bu",
            "FS" => "0x3bu",
            "GS" | "GS_OFFSET" => "0u",
            _ => return None,
        };
        Some((base.to_string(), lane, register.size))
    }

    fn pack_bytes(&self, args: &[ValueId], wide: bool) -> Result<String> {
        let cast = if wide { "uint64_t" } else { "uint32_t" };
        let mut parts = Vec::new();
        for (index, arg) in args.iter().enumerate() {
            parts.push(format!(
                "((({cast})({}) & 0xffu) << {})",
                self.expr(*arg)?,
                index * 8
            ));
        }
        Ok(format!("({})", parts.join(" | ")))
    }

    fn pack_lanes(&self, args: &[ValueId]) -> Option<String> {
        let constants: Option<Vec<u64>> = args
            .iter()
            .map(|arg| match self.s.value(*arg).op {
                Op::Const(data) => Some(data),
                _ => None,
            })
            .collect();
        if let Some(constants) = constants {
            let mut value = 0u64;
            for (index, byte) in constants.iter().enumerate() {
                value |= (byte & 0xff) << (index * 8);
            }
            return Some(if args.len() > 4 {
                format!("0x{value:x}ull")
            } else {
                format!("0x{value:x}u")
            });
        }
        let mut inner: Option<ValueId> = None;
        let mut bytes = true;
        for (index, arg) in args.iter().enumerate() {
            let value = *arg;
            match self.s.value(value).op {
                Op::Byte(byte) if byte as usize == index => {
                    let source = *self.s.args(value).first()?;
                    if inner.is_some_and(|inner| inner != source) {
                        bytes = false;
                        break;
                    }
                    inner = Some(source);
                }
                _ => {
                    bytes = false;
                    break;
                }
            }
        }
        if bytes {
            let inner = inner?;
            let bits = args.len() * 8;
            let text = self.expr(inner).ok()?;
            if self.s.value(inner).size as usize * 8 == bits {
                return Some(format!("({text})"));
            }
            if bits < 64 {
                return Some(format!("(({text}) & 0x{:x}u)", (1u64 << bits) - 1));
            }
        }
        if args.iter().any(|arg| self.materialized[*arg as usize]) {
            return None;
        }
        let mut origin: Option<(String, u32, u32)> = None;
        let mut lanes = Vec::new();
        for (index, arg) in args.iter().enumerate() {
            let value = *arg;
            let item = *self.s.value(value);
            let (key, offset) = match item.op {
                Op::Input(key) | Op::StateReload(key) => (key, 0),
                Op::Byte(byte) => {
                    if byte as usize != index {
                        return None;
                    }
                    let inner = *self.s.args(value).first()?;
                    match self.s.value(inner).op {
                        Op::Input(key) | Op::StateReload(key) => (key, byte as u32),
                        _ => return None,
                    }
                }
                _ => return None,
            };
            let (base, lane, width) = self.lane_origin(key)?;
            let lane = lane + offset;
            if lane >= width {
                return None;
            }
            match &origin {
                None => origin = Some((base, lane, width)),
                Some((first_base, _, first_width)) => {
                    if *first_base != base || *first_width != width {
                        return None;
                    }
                }
            }
            lanes.push(lane);
        }
        let (base, first, width) = origin?;
        if lanes
            .iter()
            .enumerate()
            .any(|(index, lane)| *lane != first + index as u32)
        {
            return None;
        }
        let bits = args.len() * 8;
        if first == 0 && bits >= width as usize * 8 {
            Some(format!("({base})"))
        } else if first == 0 {
            Some(format!("({base} & 0x{:x}u)", (1u64 << bits) - 1))
        } else {
            Some(format!(
                "(({base} >> {}) & 0x{:x}u)",
                first * 8,
                (1u64 << bits) - 1
            ))
        }
    }

    fn error(&self, addr: Addr, detail: &str) -> rk_core::Error {
        Diagnostic::error(
            addr,
            Reason::Unsupported {
                detail: detail.into(),
            },
        )
    }
}

struct ValueNames<'b, 'a> {
    emitter: &'b Emitter<'a>,
}

impl External for ValueNames<'_, '_> {
    fn value(&self, id: ValueId) -> Result<String> {
        self.emitter.expr(id)
    }
}
