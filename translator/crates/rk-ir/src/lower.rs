use std::collections::{BTreeMap, BTreeSet};

use rk_core::contract::Fields;
use rk_core::ir::{FunctionIr, Insn, OpCode, PcodeOp, SehEffect, Space, Varnode};
use rk_core::{Addr, Diagnostic, Reason};

use crate::op::{Op, SlowFamily, SlowInsn};
use crate::regs::Registers;

#[derive(Clone, Debug)]
pub struct CgOp {
    pub op: Op,
    pub output: Option<Varnode>,
    pub inputs: Vec<Varnode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagShape {
    Add,
    Sub,
    Cmp,
    Inc,
    Dec,
    Adc,
    Sbb,
    Logic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cond {
    Overflow,
    NoOverflow,
    Below,
    AboveEqual,
    Equal,
    NotEqual,
    BelowEqual,
    Above,
    Sign,
    NoSign,
    Parity,
    NoParity,
    Less,
    GreaterEqual,
    LessEqual,
    Greater,
}

#[derive(Clone, Debug)]
pub struct CgInsn {
    pub insn: u32,
    pub addr: Addr,
    pub length: u8,
    pub ops: Vec<CgOp>,
    pub x87: bool,
}

#[derive(Clone, Debug)]
pub struct CgFunction {
    pub addr: Addr,
    pub recipes: &'static dyn FlagRecipes,
    pub insns: Vec<CgInsn>,
    pub succ: Vec<Vec<u32>>,
    pub exits: BTreeMap<u32, Vec<Addr>>,
    pub entries: Vec<Addr>,
    pub tables: BTreeSet<u32>,
    pub dynamic: BTreeSet<u32>,
    pub noreturn: BTreeMap<u32, Addr>,
    pub import_tails: BTreeMap<u32, Addr>,
    pub seh: BTreeMap<u32, Vec<SehEffect>>,
    pub slow: Vec<SlowInsn>,
    pub diagnostics: Vec<Diagnostic>,
}

impl CgFunction {
    pub fn index_of(&self, addr: Addr) -> Option<u32> {
        self.insns
            .binary_search_by_key(&addr, |insn| insn.addr)
            .ok()
            .map(|n| n as u32)
    }
}

pub struct Lowered {
    pub ops: Vec<CgOp>,
    pub x87: bool,
    pub apply_policy: bool,
}

pub trait IsaLowering: Sync {
    fn lower(
        &self,
        insn: &Insn,
        import_tail: Option<Addr>,
        cx: &mut InstructionLowerer,
    ) -> rk_core::Result<Lowered>;

    fn flag_recipes(&self) -> &'static dyn FlagRecipes;
}

pub trait FlagRecipes: Sync {
    fn defines(&self, shape: FlagShape) -> Fields;
    fn expand(
        &self,
        shape: FlagShape,
        inputs: &[Varnode],
        fresh: &mut dyn FnMut(u32) -> Varnode,
    ) -> Vec<PcodeOp>;
    fn condition(
        &self,
        cond: Cond,
        output: Varnode,
        fresh: &mut dyn FnMut(u32) -> Varnode,
    ) -> Vec<PcodeOp>;
    fn direct(
        &self,
        shape: FlagShape,
        cond: Cond,
        inputs: &[Varnode],
        output: Varnode,
    ) -> Option<Vec<PcodeOp>>;
    fn lazy_op(&self, shape: Option<FlagShape>) -> &'static str;
    fn zero_result(&self) -> Fields;

    fn reads(&self, cond: Cond) -> Vec<Varnode> {
        let mut next = 0;
        let output = Varnode::new(Space::Unique, next, 1);
        let ops = self.condition(cond, output, &mut |size| {
            next += 0x10;
            Varnode::new(Space::Unique, next, size)
        });
        let mut reads = Vec::new();
        for input in ops.iter().flat_map(|op| &op.inputs) {
            if input.space == Space::Register && !reads.contains(input) {
                reads.push(*input);
            }
        }
        reads
    }
}

impl std::fmt::Debug for dyn FlagRecipes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FlagRecipes")
    }
}

pub struct InstructionLowerer<'a> {
    isa: &'a dyn IsaLowering,
    regs: &'static Registers,
    unique: u64,
    slow: Vec<SlowInsn>,
    diagnostics: Vec<Diagnostic>,
    pcode_control: bool,
}

pub fn lower(fir: &FunctionIr, isa: &dyn IsaLowering) -> rk_core::Result<CgFunction> {
    lower_with(fir, isa, false)
}

pub fn lower_with(
    fir: &FunctionIr,
    isa: &dyn IsaLowering,
    mark_call_returns: bool,
) -> rk_core::Result<CgFunction> {
    let mut lowerer = InstructionLowerer::new(isa);
    let mut insns = Vec::with_capacity(fir.insns.len());
    for (index, insn) in fir.insns.iter().enumerate() {
        let lowered =
            lowerer.lower_insn_with_import(insn, fir.import_tails.get(&(index as u32)).copied())?;
        let Lowered {
            mut ops,
            x87: is_x87,
            apply_policy,
        } = lowered;
        if apply_policy {
            if let Some(kinds) = fir.seh.get(&(index as u32)) {
                apply_seh(&mut ops, kinds);
            }
            if let Some(target) = fir.noreturn.get(&(index as u32)) {
                ops.push(cg(Op::Trap(*target)));
            }
        }
        if mark_call_returns && insn.flow().is_call() {
            let return_addr = insn.end();
            let arch = Registers::get().arch();
            let (opcode, input) = match arch.link {
                None => (OpCode::Store, 1),
                Some(_) => (OpCode::Copy, 0),
            };
            if let Some(position) = ops.iter().position(|op| {
                op.op == Op::P(opcode)
                    && (arch.link.is_none() || op.output == arch.link)
                    && op.inputs.get(input).is_some_and(|value| {
                        value.space == Space::Const && value.offset == return_addr
                    })
            }) {
                let value = lowerer.fresh(arch.word);
                ops.insert(
                    position,
                    CgOp {
                        op: Op::Target(return_addr),
                        output: Some(value.clone()),
                        inputs: Vec::new(),
                    },
                );
                ops[position + 1].inputs[input] = value;
            }
        }
        if let Some(&target) = fir.intra_calls.get(&(index as u32)) {
            for op in &mut ops {
                if matches!(op.op, Op::Call { .. }) {
                    op.op = Op::Branch { target };
                }
            }
        }
        lowerer.add_tail(fir, index as u32, &mut ops);
        insns.push(CgInsn {
            insn: index as u32,
            addr: insn.addr,
            length: insn.length,
            ops,
            x87: is_x87,
        });
    }
    Ok(CgFunction {
        recipes: isa.flag_recipes(),
        addr: fir.addr,
        insns,
        succ: fir.succ.clone(),
        exits: fir.exits.clone(),
        entries: fir.entries.clone(),
        tables: fir.tables.clone(),
        dynamic: fir.dynamic.clone(),
        noreturn: fir.noreturn.clone(),
        import_tails: fir.import_tails.clone(),
        seh: fir.seh.clone(),
        slow: lowerer.slow,
        diagnostics: lowerer.diagnostics,
    })
}

impl<'a> InstructionLowerer<'a> {
    pub fn new(isa: &'a dyn IsaLowering) -> Self {
        Self {
            isa,
            regs: Registers::get(),
            unique: 0x8000_0000,
            slow: Vec::new(),
            diagnostics: Vec::new(),
            pcode_control: false,
        }
    }

    pub fn with_pcode_control(mut self) -> Self {
        self.pcode_control = true;
        self
    }

    pub fn slow_insns(&self) -> &[SlowInsn] {
        &self.slow
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub fn regs(&self) -> &'static Registers {
        self.regs
    }

    pub fn fresh(&mut self, size: u32) -> Varnode {
        let out = Varnode::new(Space::Unique, self.unique, size);
        self.unique += 0x10;
        out
    }

    pub fn slow(&mut self, insn: &Insn, family: SlowFamily) -> Op {
        let index = self.slow.len() as u32;
        self.slow.push(SlowInsn {
            addr: insn.addr,
            length: insn.length,
            family,
        });
        Op::Slow(index)
    }

    pub fn reject(&mut self, insn: &Insn, reason: Reason, family: SlowFamily) -> Op {
        self.diagnostics.push(Diagnostic::new(insn.addr, reason));
        self.slow(insn, family)
    }

    pub fn lower_insn(&mut self, insn: &Insn) -> rk_core::Result<(Vec<CgOp>, bool)> {
        let lowered = self.lower_insn_with_import(insn, None)?;
        Ok((lowered.ops, lowered.x87))
    }

    fn lower_insn_with_import(
        &mut self,
        insn: &Insn,
        import_tail: Option<Addr>,
    ) -> rk_core::Result<Lowered> {
        let isa = self.isa;
        isa.lower(insn, import_tail, self)
    }

    pub fn convert(&self, op: PcodeOp) -> CgOp {
        let internal = matches!(op.opcode, OpCode::Branch | OpCode::CBranch)
            && op.inputs.first().is_some_and(|v| v.space == Space::Const);
        if !self.pcode_control || !(internal || op.opcode == OpCode::Return) {
            return from_pcode(op);
        }
        CgOp {
            op: if internal {
                Op::P(op.opcode)
            } else {
                Op::Return
            },
            output: op.output,
            inputs: op.inputs,
        }
    }

    fn add_tail(&self, fir: &FunctionIr, index: u32, out: &mut Vec<CgOp>) {
        if out.iter().any(|op| matches!(op.op, Op::CBranch { .. })) {
            let insn = &fir.insns[index as usize];
            let target = insn.end();
            if fir
                .exits
                .get(&index)
                .is_some_and(|targets| targets.contains(&target))
            {
                out.push(cg(Op::Tail { target }));
            }
            return;
        }
        if fir.succ.get(index as usize).is_some_and(|s| !s.is_empty()) {
            return;
        }
        if let Some(targets) = fir.exits.get(&index) {
            for target in targets {
                out.push(cg(Op::Tail { target: *target }));
            }
        }
    }
}

pub fn cg(op: Op) -> CgOp {
    CgOp {
        op,
        output: None,
        inputs: Vec::new(),
    }
}

pub fn from_pcode(op: PcodeOp) -> CgOp {
    match op.opcode {
        OpCode::Branch => {
            let target = op.inputs.first().and_then(target_addr);
            CgOp {
                op: target.map_or(Op::P(OpCode::Branch), |target| Op::Branch { target }),
                output: op.output,
                inputs: Vec::new(),
            }
        }
        OpCode::CBranch => {
            let target = op.inputs.first().and_then(target_addr);
            let inputs = if target.is_some() {
                op.inputs.get(1..).unwrap_or(&[]).to_vec()
            } else {
                op.inputs
            };
            CgOp {
                op: target.map_or(Op::P(OpCode::CBranch), |target| Op::CBranch { target }),
                output: op.output,
                inputs,
            }
        }
        OpCode::Call => {
            let target = op.inputs.first().and_then(target_addr);
            CgOp {
                op: target.map_or(Op::P(OpCode::Call), |target| Op::Call { target }),
                output: op.output,
                inputs: Vec::new(),
            }
        }
        OpCode::CallInd => CgOp {
            op: Op::CallInd,
            output: op.output,
            inputs: op.inputs,
        },
        OpCode::Return => CgOp {
            op: Op::Return,
            output: op.output,
            inputs: Vec::new(),
        },
        OpCode::BranchInd => CgOp {
            op: Op::BranchInd,
            output: op.output,
            inputs: op.inputs,
        },
        OpCode::CallOther => CgOp {
            op: Op::CallOther(op.userop.unwrap_or(0)),
            output: op.output,
            inputs: op.inputs,
        },
        other => CgOp {
            op: Op::P(other),
            output: op.output,
            inputs: op.inputs,
        },
    }
}

fn target_addr(v: &Varnode) -> Option<Addr> {
    match v.space {
        Space::Const | Space::Ram => Some(v.offset as Addr),
        _ => None,
    }
}

pub fn is_control(op: &PcodeOp) -> bool {
    matches!(
        op.opcode,
        OpCode::Branch
            | OpCode::CBranch
            | OpCode::Call
            | OpCode::CallInd
            | OpCode::BranchInd
            | OpCode::CallOther
    )
}

pub fn normalize_direct_ram(
    ops: &[PcodeOp],
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let mut ops = ops.to_vec();
    if let Some(index) = ops.iter().position(|op| {
        op.opcode == OpCode::BranchInd && op.inputs.first().is_some_and(|v| v.space == Space::Ram)
    }) {
        let target = ops[index].inputs[0];
        let unique = fresh(target.size);
        ops[index].inputs[0] = unique;
        ops.insert(
            index,
            PcodeOp {
                opcode: OpCode::Load,
                output: Some(unique),
                inputs: vec![Varnode::constant(target.offset, 4)],
                userop: None,
            },
        );
    }
    let touched: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| {
            !is_control(op)
                && std::iter::once(op.output.as_ref())
                    .chain(op.inputs.iter().map(Some))
                    .flatten()
                    .any(|v| v.space == Space::Ram)
        })
        .map(|(index, _)| index)
        .collect();
    if touched.is_empty() {
        return Ok(ops);
    }
    let mut operands = BTreeSet::new();
    for &index in &touched {
        let op = &ops[index];
        for v in std::iter::once(op.output.as_ref())
            .chain(op.inputs.iter().map(Some))
            .flatten()
        {
            if v.space == Space::Ram {
                operands.insert((v.offset, v.size));
            }
        }
    }
    if operands.len() != 1 {
        return Err(Reason::UnsupportedDirectRam {
            detail: "multiple direct-ram operands".into(),
        });
    }
    let (offset, size) = operands.pop_first().unwrap();
    let address = Varnode::constant(offset, 4);
    let has_write = touched
        .iter()
        .any(|&i| ops[i].output.is_some_and(|v| v.space == Space::Ram));
    let has_read = touched
        .iter()
        .any(|&i| ops[i].inputs.iter().any(|v| v.space == Space::Ram));
    if has_write && has_read {
        let writes = touched
            .iter()
            .filter(|&&i| ops[i].output.is_some_and(|v| v.space == Space::Ram))
            .count();
        if writes != 1 {
            return Err(Reason::UnsupportedDirectRam {
                detail: "unsupported direct-ram read-modify-write".into(),
            });
        }
        let operand = fresh(size);
        let mut out = Vec::with_capacity(ops.len() + 2);
        for mut op in ops {
            if is_control(&op) {
                out.push(op);
                continue;
            }
            let mut changed = false;
            for input in &mut op.inputs {
                if input.space == Space::Ram {
                    *input = operand;
                    changed = true;
                }
            }
            if changed {
                out.push(PcodeOp {
                    opcode: OpCode::Load,
                    output: Some(operand),
                    inputs: vec![address],
                    userop: None,
                });
            }
            if op.output.is_some_and(|v| v.space == Space::Ram) {
                op.output = Some(operand);
                out.push(op);
                out.push(PcodeOp {
                    opcode: OpCode::Store,
                    output: None,
                    inputs: vec![address, operand],
                    userop: None,
                });
            } else {
                out.push(op);
            }
        }
        return Ok(out);
    }
    if has_write {
        for &index in touched.iter().rev() {
            let dst = fresh(size);
            ops[index].output = Some(dst);
            ops.insert(
                index + 1,
                PcodeOp {
                    opcode: OpCode::Store,
                    output: None,
                    inputs: vec![address, dst],
                    userop: None,
                },
            );
        }
        return Ok(ops);
    }
    let src = fresh(size);
    let first = touched[0];
    let mut out = Vec::with_capacity(ops.len() + 1);
    for (index, mut op) in ops.into_iter().enumerate() {
        if index == first {
            out.push(PcodeOp {
                opcode: OpCode::Load,
                output: Some(src),
                inputs: vec![address],
                userop: None,
            });
        }
        if !is_control(&op) {
            for input in &mut op.inputs {
                if input.space == Space::Ram {
                    *input = src;
                }
            }
        }
        out.push(op);
    }
    Ok(out)
}

pub fn copy(output: Varnode, input: Varnode) -> PcodeOp {
    PcodeOp {
        opcode: OpCode::Copy,
        output: Some(output),
        inputs: vec![input],
        userop: None,
    }
}

pub fn op(opcode: OpCode, output: Varnode, inputs: Vec<Varnode>) -> PcodeOp {
    PcodeOp {
        opcode,
        output: Some(output),
        inputs,
        userop: None,
    }
}

pub fn prepare_rmw(
    ops: &[PcodeOp],
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let stores: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| op.opcode == OpCode::Store)
        .map(|(index, _)| index)
        .collect();
    if stores.is_empty() {
        return Ok(ops.to_vec());
    }
    let loads: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| op.opcode == OpCode::Load)
        .map(|(index, _)| index)
        .collect();
    if stores.len() != 1 || loads.is_empty() {
        return Err(Reason::UnsupportedReadModifyWrite {
            detail: "unsupported memory operand shape".into(),
        });
    }
    let load = &ops[loads[0]];
    if loads
        .iter()
        .any(|&index| ops[index].inputs != load.inputs || ops[index].output != load.output)
    {
        return Err(Reason::UnsupportedReadModifyWrite {
            detail: "repeated memory operand loads disagree".into(),
        });
    }
    let store = &ops[stores[0]];
    let loaded = load
        .output
        .ok_or_else(|| Reason::UnsupportedReadModifyWrite {
            detail: "memory load without an output".into(),
        })?;
    if store.inputs.len() != 2
        || store.inputs[0] != load.inputs[0]
        || store.inputs[1].size != loaded.size
    {
        return Err(Reason::UnsupportedReadModifyWrite {
            detail: "read-modify-write addresses disagree".into(),
        });
    }
    let old = fresh(loaded.size);
    let stored = fresh(store.inputs[1].size);
    let mut out = Vec::with_capacity(ops.len() + 2);
    let mut seen_load = false;
    let mut seen_store = false;
    for op in ops {
        match op.opcode {
            OpCode::Load => {
                if !seen_load {
                    out.push(op.clone());
                    out.push(copy(old, op.output.unwrap()));
                    seen_load = true;
                } else {
                    out.push(copy(
                        op.output.unwrap(),
                        if seen_store { stored } else { old },
                    ));
                }
            }
            OpCode::Store => {
                out.push(copy(stored, op.inputs[1]));
                out.push(op.clone());
                seen_store = true;
            }
            _ => out.push(op.clone()),
        }
    }
    Ok(out)
}

fn apply_seh(ops: &mut Vec<CgOp>, kinds: &[SehEffect]) {
    for kind in kinds {
        match kind {
            SehEffect::Landing => ops.insert(0, cg(Op::Seh(*kind))),
            SehEffect::Orphan => {
                if let Some(at) = ops.iter().position(|op| op.op == Op::Return) {
                    ops.insert(at, cg(Op::Seh(*kind)));
                } else {
                    ops.push(cg(Op::Seh(*kind)));
                }
            }
            _ => ops.push(cg(Op::Seh(*kind))),
        }
    }
}
