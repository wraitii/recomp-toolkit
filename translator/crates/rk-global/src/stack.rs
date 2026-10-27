use std::collections::{BTreeMap, BTreeSet, VecDeque};

use rk_core::contract::{Field, Fields};
use rk_core::ir::{FunctionIr, Insn, OpCode, PcodeOp, Space, Varnode};
use rk_core::memory::{Address, Base, Effects, Footprint, Range};
use rk_core::program::{Import, Program};
use rk_core::settings::Settings;
use rk_core::Addr;
use rk_ir::Registers;
use std::borrow::Cow;

type Checked<T> = std::result::Result<T, &'static str>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Memory,
    Returns,
    Cleanup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReturnEffect {
    Exact,
    Adjust(i32),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Value {
    Stack(u32),
    Constant(u64),
    Input(Varnode),
    ReturnAddress,
    ReturnOffset(i32),
    Thread,
    Import(Addr),
    Derived(u32),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct State {
    registers: BTreeMap<Varnode, Value>,
    slots: BTreeMap<(u32, u32), Value>,
}

#[derive(Clone, Debug)]
pub struct StackEffect {
    pub cleanup: i32,
    pub writes: Vec<(i32, u32)>,
}

#[derive(Clone, Debug)]
pub struct Summary {
    pub effect: StackEffect,
    pub reads: BTreeSet<(i32, u32)>,
    pub reads_other: bool,
    pub writes_other: bool,
    pub preserves: Fields,
    pub mapped_noreturn: bool,
    pub returns: ReturnEffect,
    pub escapes: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Analysis {
    pub functions: BTreeMap<Addr, Summary>,
    pub sites: BTreeMap<Addr, Summary>,
    pub rejected: BTreeMap<String, usize>,
    pub rejected_sites: BTreeMap<String, usize>,
    pub unbalanced_returns: BTreeSet<Addr>,
    pub rejected_functions: BTreeMap<Addr, String>,
    pub cleanups: BTreeMap<Addr, i32>,
    pub site_cleanups: BTreeMap<Addr, i32>,
    pub rejected_cleanups: BTreeMap<Addr, String>,
    pub argument_writes: BTreeMap<Addr, Vec<(i32, u32)>>,
    pub stack_helpers: BTreeMap<Addr, &'static str>,
    pub stack_probes: BTreeMap<Addr, &'static str>,
}

pub struct CallFacts<'a> {
    pub registers: crate::ContractCallFacts<'a>,
    pub stack: &'a Analysis,
    pub memory: Option<&'a crate::memory::Analysis>,
}

impl rk_ssa::CallFacts for CallFacts<'_> {
    fn summary(&self, site: Addr, target: Option<Addr>) -> rk_ssa::CallSummary<'_> {
        let mut result = rk_ssa::CallSummary {
            borrowed: None,
            checkpoints: None,
            ..self.registers.summary(site, target)
        };
        let summary = self.summary(site, target);
        if let Some(summary) = summary {
            result.cleanup = Some(summary.effect.cleanup);
        }
        let memory = self.memory.and_then(|effects| match target {
            Some(target) => effects.functions.get(&target),
            None => effects.sites.get(&site),
        });
        result.effects = memory.map(Cow::Borrowed).or_else(|| {
            summary.map(|summary| {
                let mut writes = Footprint::default();
                for &(offset, size) in &summary.effect.writes {
                    writes.insert(Range::new(Address::exact(Base::Frame, offset as u32), size));
                }
                Cow::Owned(Effects {
                    reads: Footprint::unknown(),
                    writes,
                })
            })
        });
        result
    }

    fn call(&self, site: Addr, target: Option<Addr>) -> rk_ssa::CallEffect {
        let mut effect = self.registers.call(site, target);
        if let Some(summary) = self.summary(site, target) {
            effect.preserves |= summary.preserves;
            effect.writes = effect.writes & !summary.preserves;
            if summary.mapped_noreturn {
                effect.reads = Fields::ALL;
                effect.kills = Fields::NONE;
            }
        }
        effect
    }
}

impl CallFacts<'_> {
    fn summary(&self, site: Addr, target: Option<Addr>) -> Option<&Summary> {
        match target {
            Some(target) => self.stack.functions.get(&target),
            None => self.stack.sites.get(&site),
        }
    }
}

fn overlap(a: u32, width_a: u32, b: u32, width_b: u32) -> bool {
    use rk_core::memory::{Address, Base, Range};
    !Range::new(Address::exact(Base::Frame, a), width_a)
        .disjoint(Range::new(Address::exact(Base::Frame, b), width_b))
}

fn register_overlap(a: Varnode, b: Varnode) -> bool {
    a.space == b.space && a.offset < b.offset + b.size as u64 && b.offset < a.offset + a.size as u64
}

fn fields() -> impl Iterator<Item = (Field, Varnode)> {
    Field::ALL.into_iter().filter_map(|field| {
        Registers::get()
            .register(field.name())
            .map(|register| (field, register))
    })
}

impl State {
    fn entry() -> Self {
        let mut state = Self::default();
        for (field, register) in fields() {
            state.registers.insert(
                register,
                if field == Field::Esp {
                    Value::Stack(0)
                } else {
                    Value::Input(register)
                },
            );
        }
        if let Some(thread) = Registers::get().register("FS_OFFSET") {
            state.registers.insert(thread, Value::Thread);
        }
        state.slots.insert((0, 4), Value::ReturnAddress);
        state
    }

    fn read(&self, input: Varnode, locals: &BTreeMap<Varnode, Value>) -> Option<Value> {
        match input.space {
            Space::Const => Some(Value::Constant(input.offset & mask(input.size))),
            Space::Register => self.registers.get(&input).copied(),
            Space::Unique => locals.get(&input).copied(),
            _ => None,
        }
    }

    fn write(&mut self, output: Varnode, value: Option<Value>) -> Checked<()> {
        if output.space != Space::Register {
            return Err("non-register output");
        }
        if ["CS", "DS", "ES", "SS", "FS", "GS", "FS_OFFSET"]
            .iter()
            .filter_map(|name| Registers::get().register(name))
            .any(|segment| register_overlap(output, segment))
        {
            return Err("segment base write");
        }
        self.registers
            .retain(|&register, _| !register_overlap(output, register));
        if let Some(value) = value {
            self.registers.insert(output, value);
        }
        Ok(())
    }

    fn merge(&mut self, other: &Self) -> bool {
        let old = (self.registers.len(), self.slots.len());
        self.registers
            .retain(|key, value| other.registers.get(key) == Some(value));
        self.slots
            .retain(|key, value| other.slots.get(key) == Some(value));
        old != (self.registers.len(), self.slots.len())
    }

    fn clobber(&mut self, private: bool) {
        if private {
            self.slots.retain(|&(at, _), _| (at as i32) < 0);
        } else {
            self.slots.clear();
        }
    }

    fn invalidate(&mut self, offset: u32, size: u32) -> Checked<()> {
        if size == 0 || size > 32 {
            return Err("unsupported memory width");
        }
        self.slots
            .retain(|&(at, width), _| !overlap(offset, size, at, width));
        Ok(())
    }

    fn returning(&self) -> Checked<(Fields, ReturnEffect)> {
        let esp = Registers::get().require("ESP");
        if self.registers.get(&esp) != Some(&Value::Stack(0)) {
            return Err("unproved ESP balance");
        }
        let returns = match self.slots.get(&(0, 4)) {
            Some(Value::ReturnAddress) => ReturnEffect::Exact,
            Some(Value::ReturnOffset(delta)) => ReturnEffect::Adjust(*delta),
            _ => ReturnEffect::Unknown,
        };
        Ok((
            fields()
                .filter_map(|(field, register)| {
                    (self.registers.get(&register) == Some(&Value::Input(register)))
                        .then_some(field)
                })
                .collect(),
            returns,
        ))
    }
}

fn mask(size: u32) -> u64 {
    if size >= 8 {
        u64::MAX
    } else {
        (1u64 << (size * 8)) - 1
    }
}

fn evaluate(op: &PcodeOp, state: &State, locals: &BTreeMap<Varnode, Value>) -> Option<Value> {
    exact(op, state, locals).or_else(|| derived(op, state, locals))
}

fn derived(op: &PcodeOp, state: &State, locals: &BTreeMap<Varnode, Value>) -> Option<Value> {
    if op.output?.size != 4 {
        return None;
    }
    let read = |index: usize| {
        op.inputs
            .get(index)
            .and_then(|input| state.read(*input, locals))
    };
    let (a, b) = (read(0), read(1));
    let base = |value| match value {
        Some(Value::Stack(offset) | Value::Derived(offset)) => Some(offset),
        _ => None,
    };
    let constant = |value| match value {
        Some(Value::Constant(data)) => data as u32,
        _ => 0,
    };
    let base = match op.opcode {
        OpCode::IntAnd => base(a).or(base(b)),
        OpCode::IntAdd => match (base(a), base(b)) {
            (Some(offset), None) => Some(offset.wrapping_add(constant(b))),
            (None, Some(offset)) => Some(offset.wrapping_add(constant(a))),
            _ => None,
        },
        OpCode::IntSub if base(b).is_none() => {
            base(a).map(|offset| offset.wrapping_sub(constant(b)))
        }
        _ => None,
    }?;
    Some(Value::Derived(base))
}

fn exact(op: &PcodeOp, state: &State, locals: &BTreeMap<Varnode, Value>) -> Option<Value> {
    let output = op.output?;
    let a = state.read(*op.inputs.first()?, locals)?;
    if op.opcode == OpCode::Copy && op.inputs[0].size == output.size {
        return Some(a);
    }
    match op.opcode {
        OpCode::IntZExt | OpCode::IntSExt => {
            let Value::Constant(mut data) = a else {
                return None;
            };
            if op.opcode == OpCode::IntSExt {
                let bits = op.inputs[0].size * 8;
                if bits == 0 || bits > 64 {
                    return None;
                }
                let sign = 1u64 << (bits - 1);
                data = (data ^ sign).wrapping_sub(sign);
            }
            Some(Value::Constant(data & mask(output.size)))
        }
        OpCode::IntAdd | OpCode::IntSub if output.size == 4 => {
            let b = state.read(*op.inputs.get(1)?, locals)?;
            let subtract = op.opcode == OpCode::IntSub;
            let offset = |value, delta: i64| match value {
                Value::ReturnAddress => Some(Value::ReturnOffset(delta as i32)),
                Value::ReturnOffset(at) => Some(Value::ReturnOffset(at.wrapping_add(delta as i32))),
                _ => None,
            };
            if let Value::Constant(data) = b {
                if let Some(value) = offset(
                    a,
                    if subtract {
                        -(data as u32 as i32 as i64)
                    } else {
                        data as u32 as i32 as i64
                    },
                ) {
                    return Some(value);
                }
            }
            if !subtract {
                if let Value::Constant(data) = a {
                    if let Some(value) = offset(b, data as u32 as i32 as i64) {
                        return Some(value);
                    }
                }
            }
            match (a, b) {
                (Value::Thread, Value::Constant(_)) | (Value::Constant(_), Value::Thread) => {
                    return Some(Value::Thread)
                }
                _ => {}
            }
            let address = |value| match value {
                Value::Stack(offset) => Address::exact(Base::Frame, offset),
                Value::Constant(data) => Address::exact(Base::Absolute, data as u32),
                _ => Address::Unknown,
            };
            let result = if subtract {
                address(a).sub(address(b))
            } else {
                address(a).add(address(b))
            };
            match result {
                Address::Known {
                    base: Base::Frame,
                    offset,
                    span: 0,
                } => Some(Value::Stack(offset)),
                Address::Known {
                    base: Base::Absolute,
                    offset,
                    span: 0,
                } => Some(Value::Constant(offset as u64)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn target(insn: &Insn) -> Option<Addr> {
    insn.ops.iter().find_map(|op| {
        if !matches!(op.opcode, OpCode::Call | OpCode::Branch) {
            return None;
        }
        let input = op.inputs.first()?;
        matches!(input.space, Space::Ram | Space::Const).then_some(input.offset as Addr)
    })
}

fn rejection(reason: &str, fir: &FunctionIr, known: &BTreeMap<Addr, Summary>) -> String {
    if !matches!(reason, "unresolved direct call" | "unresolved tail") {
        return reason.into();
    }
    let missing: Vec<_> = fir
        .insns
        .iter()
        .enumerate()
        .filter_map(|(index, insn)| {
            if reason == "unresolved direct call" {
                if !insn.ops.iter().any(|op| op.opcode == OpCode::Call) {
                    return None;
                }
            } else if !fir
                .exits
                .get(&(index as u32))
                .is_some_and(|targets| !targets.is_empty())
            {
                return None;
            }
            let callee = target(insn)?;
            (!known.contains_key(&callee)).then(|| format!("{:08x}->{callee:08x}", insn.addr))
        })
        .collect();
    format!("{reason}: {}", missing.join(","))
}

fn compose(
    state: &mut State,
    summary: &Summary,
    offset: u32,
    reads: &mut BTreeSet<(i32, u32)>,
    writes: &mut BTreeSet<(i32, u32)>,
    private: bool,
) -> Checked<()> {
    if summary.writes_other {
        state.clobber(private);
    }
    for &(at, width) in &summary.effect.writes {
        let at = offset.wrapping_add(at as u32);
        state.invalidate(at, width)?;
        writes.insert((at as i32, width));
    }
    reads.extend(
        summary
            .reads
            .iter()
            .map(|&(at, width)| (offset.wrapping_add(at as u32) as i32, width)),
    );
    for (field, register) in fields() {
        if !summary.preserves.contains(field) {
            state.write(register, None)?;
        }
    }
    Ok(())
}

fn import_pop(pops: &BTreeMap<String, i32>, import: &Import) -> Option<i32> {
    pops.get(&format!(
        "{}!{}",
        import.dll.to_ascii_lowercase(),
        import.name
    ))
    .copied()
}

fn import_call(state: &mut State, pop: i32, private: bool) -> Checked<()> {
    let esp = Registers::get().require("ESP");
    let Some(Value::Stack(offset)) = state.registers.get(&esp).copied() else {
        return Err("unknown call frame");
    };
    state.clobber(private);
    for (field, register) in fields() {
        if !matches!(field, Field::Ebx | Field::Esi | Field::Edi | Field::Ebp) {
            state.write(register, None)?;
        }
    }
    state.write(esp, Some(Value::Stack(offset.wrapping_add(4 + pop as u32))))
}

struct Context<'a> {
    program: &'a Program,
    analysis: &'a rk_frontend::Analysis,
    excluded: &'a BTreeSet<Addr>,
    settings: &'a Settings,
    helpers: BTreeSet<Addr>,
    probes: BTreeSet<Addr>,
}

struct Walk<'a> {
    context: &'a Context<'a>,
    known: &'a BTreeMap<Addr, Summary>,
    mode: Mode,
    reads: BTreeSet<(i32, u32)>,
    writes: BTreeSet<(i32, u32)>,
    reads_other: bool,
    writes_other: bool,
    mapped_noreturn: bool,
    escapes: bool,
}

fn unmodeled(insn: &Insn) -> bool {
    insn.ops.iter().any(|op| {
        op.opcode == OpCode::CallOther
            || (matches!(op.opcode, OpCode::Branch | OpCode::CBranch)
                && op
                    .inputs
                    .first()
                    .is_some_and(|input| input.space == Space::Const))
    })
}

impl Walk<'_> {
    fn private(&self) -> bool {
        self.mode == Mode::Cleanup && self.context.settings.assume_private_stack
    }

    fn caller_pointer(&self, value: Option<Value>) -> bool {
        match value {
            Some(Value::Stack(offset)) => offset as i32 >= 0,
            Some(Value::Derived(offset)) => {
                offset as i32 >= 0 || !self.context.settings.assume_stack_pointer_bases
            }
            _ => false,
        }
    }

    fn pass_registers(&mut self, state: &State) {
        let esp = Registers::get().require("ESP");
        if fields().any(|(_, register)| {
            register != esp && self.caller_pointer(state.registers.get(&register).copied())
        }) {
            self.escapes = true;
        }
    }

    fn ops(&mut self, insn: &Insn, state: &mut State, depth: usize, table: bool) -> Checked<()> {
        let (program, mode) = (self.context.program, self.mode);
        let mut locals = BTreeMap::new();
        for op in &insn.ops {
            let mut value = evaluate(op, state, &locals);
            if let [input] = op.inputs.as_slice() {
                if mode == Mode::Cleanup
                    && op.opcode == OpCode::Copy
                    && input.space == Space::Ram
                    && input.size == 4
                    && program.image.imports.contains_key(&(input.offset as Addr))
                {
                    value = Some(Value::Import(input.offset as Addr));
                }
            }
            if op.inputs.iter().any(|input| input.space == Space::Ram) && !op.opcode.is_branch() {
                self.reads_other = true;
            }
            match op.opcode {
                OpCode::Store => {
                    if op.inputs.len() != 2 {
                        return Err("unsupported store");
                    }
                    let address = state.read(op.inputs[0], &locals);
                    if self.caller_pointer(state.read(op.inputs[1], &locals))
                        || matches!(address, Some(Value::Derived(_)))
                            && self.caller_pointer(address)
                    {
                        self.escapes = true;
                    }
                    if address == Some(Value::Thread) && mode == Mode::Cleanup {
                        self.writes_other = true;
                        continue;
                    }
                    let Some(Value::Stack(offset)) = address else {
                        if mode == Mode::Memory {
                            return Err("potentially aliased non-stack write");
                        }
                        state.clobber(self.private());
                        self.writes_other = true;
                        continue;
                    };
                    let size = op.inputs[1].size;
                    let stored = state.read(op.inputs[1], &locals);
                    state.invalidate(offset, size)?;
                    self.writes.insert((offset as i32, size));
                    if let Some(stored) = stored {
                        state.slots.insert((offset, size), stored);
                    }
                    if state.slots.len() > 128 {
                        return Err("stack slot budget");
                    }
                }
                OpCode::Load => {
                    let output = op.output.ok_or("load has no output")?;
                    let address =
                        state.read(*op.inputs.first().ok_or("load has no address")?, &locals);
                    if let Some(Value::Stack(offset)) = address {
                        self.reads.insert((offset as i32, output.size));
                        value = state.slots.get(&(offset, output.size)).copied();
                    } else {
                        self.reads_other = true;
                        value = None;
                    }
                }
                OpCode::Call => {
                    let target = target(insn).ok_or("unknown direct call")?;
                    if mode == Mode::Cleanup && self.context.helpers.contains(&target) {
                        self.inline(target, state, depth)?;
                        continue;
                    }
                    if mode == Mode::Cleanup && self.context.probes.contains(&target) {
                        let (esp, eax) = (
                            Registers::get().require("ESP"),
                            Registers::get().require("EAX"),
                        );
                        let (Some(Value::Stack(offset)), Some(Value::Constant(size))) = (
                            state.registers.get(&esp).copied(),
                            state.registers.get(&eax).copied(),
                        ) else {
                            return Err("unknown stack probe size");
                        };
                        state.write(eax, None)?;
                        state.write(
                            esp,
                            Some(Value::Stack(
                                offset.wrapping_add(4).wrapping_sub(size as u32),
                            )),
                        )?;
                        continue;
                    }
                    self.pass_registers(state);
                    if mode == Mode::Cleanup && !self.known.contains_key(&target) {
                        if let Some(pop) = program.import_thunk(target).and_then(|import| {
                            import_pop(&self.context.settings.import_pops, import)
                        }) {
                            import_call(state, pop, self.private())?;
                            self.reads_other = true;
                            self.writes_other = true;
                            continue;
                        }
                    }
                    let callee = self.known.get(&target).ok_or("unresolved direct call")?;
                    let esp = Registers::get().require("ESP");
                    let Some(Value::Stack(offset)) = state.registers.get(&esp).copied() else {
                        return Err("unknown call frame");
                    };
                    let private = self.private();
                    compose(
                        state,
                        callee,
                        offset,
                        &mut self.reads,
                        &mut self.writes,
                        private,
                    )?;
                    state.write(
                        esp,
                        Some(Value::Stack(
                            offset.wrapping_add(4 + callee.effect.cleanup as u32),
                        )),
                    )?;
                    self.reads_other |= callee.reads_other;
                    self.writes_other |= callee.writes_other;
                    self.mapped_noreturn |= callee.mapped_noreturn;
                }
                OpCode::CallInd if mode == Mode::Cleanup => {
                    self.pass_registers(state);
                    let pop = match op
                        .inputs
                        .first()
                        .and_then(|target| state.read(*target, &locals))
                    {
                        Some(Value::Import(slot)) => {
                            program.image.imports.get(&slot).and_then(|import| {
                                import_pop(&self.context.settings.import_pops, import)
                            })
                        }
                        _ => self.context.settings.call_pops.get(&insn.addr).copied(),
                    }
                    .ok_or("unmodeled control or user operation")?;
                    import_call(state, pop, self.private())?;
                    self.reads_other = true;
                    self.writes_other = true;
                }
                OpCode::BranchInd if table => {}
                OpCode::CallInd
                | OpCode::CallOther
                | OpCode::Unknown
                | OpCode::BranchInd
                | OpCode::Return => {
                    return Err("unmodeled control or user operation");
                }
                _ => {}
            }
            if let Some(output) = op.output {
                match output.space {
                    Space::Register => state.write(output, value)?,
                    Space::Unique => {
                        locals.retain(|&key, _| !register_overlap(key, output));
                        if let Some(value) = value {
                            locals.insert(output, value);
                        }
                    }
                    Space::Ram if mode == Mode::Cleanup => {
                        state.clobber(self.private());
                        self.writes_other = true;
                    }
                    _ => return Err("unmodeled output space"),
                }
            }
        }
        Ok(())
    }

    fn inline(&mut self, addr: Addr, state: &mut State, depth: usize) -> Checked<()> {
        let fir = self
            .context
            .analysis
            .ir
            .get(&addr)
            .and_then(|fir| fir.as_ref().ok())
            .filter(|_| depth < 2)
            .ok_or("unmodeled helper")?;
        let mut index = fir.index_of(fir.addr).ok_or("missing entry")?;
        for _ in 0..64 {
            let insn = fir.insns.get(index as usize).ok_or("invalid CFG index")?;
            if unmodeled(insn) || fir.noreturn.contains_key(&index) {
                return Err("unmodeled helper");
            }
            if insn.mnemonic.eq_ignore_ascii_case("RET") {
                let pop =
                    return_pop(self.context.program, insn).ok_or("unsupported return encoding")?;
                let esp = Registers::get().require("ESP");
                let Some(Value::Stack(offset)) = state.registers.get(&esp).copied() else {
                    return Err("unknown call frame");
                };
                return state.write(esp, Some(Value::Stack(offset.wrapping_add(4 + pop as u32))));
            }
            self.ops(insn, state, depth + 1, false)?;
            match fir.succ.get(index as usize).map(Vec::as_slice) {
                Some(&[next]) if !fir.exits.contains_key(&index) => index = next,
                _ => return Err("unmodeled helper"),
            }
        }
        Err("unmodeled helper")
    }
}

fn summarize(
    context: &Context<'_>,
    fir: &FunctionIr,
    known: &BTreeMap<Addr, Summary>,
    mode: Mode,
) -> Checked<Summary> {
    let (program, excluded) = (context.program, context.excluded);
    let reentered = !fir.entries.is_empty()
        || !fir.seh.is_empty()
        || !fir.seh_handlers.is_empty()
        || !fir.seh_unwind_only.is_empty();
    if reentered && mode != Mode::Cleanup || !fir.dynamic.is_empty() || !fir.import_tails.is_empty()
    {
        return Err("unmodeled entry or exception flow");
    }
    let entry = fir.index_of(fir.addr).ok_or("missing entry")?;
    let mut before = BTreeMap::from([(entry, State::entry())]);
    let mut pending = VecDeque::from([entry]);
    let mut walk = Walk {
        context,
        known,
        mode,
        reads: BTreeSet::new(),
        writes: BTreeSet::new(),
        reads_other: false,
        writes_other: false,
        mapped_noreturn: false,
        escapes: reentered,
    };
    let mut cleanups = BTreeSet::new();
    let mut preserves = Fields::ALL;
    let mut returns = None;
    let mut steps = 0;
    while let Some(index) = pending.pop_front() {
        steps += 1;
        if steps > 10000 || walk.reads.len() + walk.writes.len() > 512 {
            return Err("summary budget");
        }
        let insn = fir.insns.get(index as usize).ok_or("invalid CFG index")?;
        let successors = fir.succ.get(index as usize).ok_or("missing successors")?;
        if insn.x87 && mode != Mode::Cleanup || unmodeled(insn) {
            return Err("unmodeled instruction effects");
        }
        if fir.noreturn.contains_key(&index) {
            if target(insn).is_some_and(|target| excluded.contains(&target)) {
                return Err("opaque noreturn callee");
            }
            if !successors.is_empty() {
                return Err("noreturn call has successors");
            }
            walk.mapped_noreturn = true;
            continue;
        }
        let mut state = before[&index].clone();
        if insn.mnemonic.eq_ignore_ascii_case("RET") {
            if !successors.is_empty()
                || fir
                    .exits
                    .get(&index)
                    .is_some_and(|targets| !targets.is_empty())
            {
                return Err("return has successors");
            }
            let (preserved, effect) = state.returning()?;
            if walk.caller_pointer(
                state
                    .registers
                    .get(&Registers::get().require("EAX"))
                    .copied(),
            ) {
                walk.escapes = true;
            }
            preserves = preserves & preserved;
            returns = Some(match (returns, effect) {
                (None, effect) => effect,
                (Some(a), b) if a == b => a,
                _ => ReturnEffect::Unknown,
            });
            cleanups.insert(return_pop(program, insn).ok_or("unsupported return encoding")?);
            walk.reads.insert((0, 4));
            continue;
        }
        let tail = fir.exits.get(&index).filter(|targets| !targets.is_empty());
        walk.ops(
            insn,
            &mut state,
            0,
            mode == Mode::Cleanup && fir.tables.contains(&index),
        )?;
        if let Some(targets) = tail {
            if !insn.mnemonic.eq_ignore_ascii_case("JMP")
                || !successors.is_empty()
                || targets.len() != 1
                || target(insn) != Some(targets[0])
            {
                return Err("unmodeled tail exit");
            }
            let (preserved, effect) = state.returning()?;
            preserves = preserves & preserved;
            if program.is_noreturn(targets[0]) {
                if excluded.contains(&targets[0]) {
                    return Err("opaque noreturn callee");
                }
                walk.mapped_noreturn = true;
                continue;
            }
            let callee = known.get(&targets[0]).ok_or("unresolved tail")?;
            walk.pass_registers(&state);
            walk.escapes |= callee.escapes;
            let private = walk.private();
            compose(
                &mut state,
                callee,
                0,
                &mut walk.reads,
                &mut walk.writes,
                private,
            )?;
            state.write(Registers::get().require("ESP"), Some(Value::Stack(0)))?;
            let (preserved, _) = state.returning()?;
            preserves = preserves & preserved;
            let effect = match (effect, callee.returns) {
                _ if callee.writes_other => ReturnEffect::Unknown,
                (ReturnEffect::Exact, next) | (next, ReturnEffect::Exact) => next,
                (ReturnEffect::Adjust(a), ReturnEffect::Adjust(b)) => match a.wrapping_add(b) {
                    0 => ReturnEffect::Exact,
                    delta => ReturnEffect::Adjust(delta),
                },
                _ => ReturnEffect::Unknown,
            };
            returns = Some(match (returns, effect) {
                (None, effect) => effect,
                (Some(a), b) if a == b => a,
                _ => ReturnEffect::Unknown,
            });
            cleanups.insert(callee.effect.cleanup);
            walk.reads_other |= callee.reads_other;
            walk.writes_other |= callee.writes_other;
            walk.mapped_noreturn |= callee.mapped_noreturn;
            continue;
        }
        if successors.is_empty() {
            return Err("unmodeled terminal instruction");
        }
        for &next in successors {
            if next as usize >= fir.insns.len() {
                return Err("external CFG edge");
            }
            if let Some(previous) = before.get_mut(&next) {
                if previous.merge(&state) {
                    pending.push_back(next);
                }
            } else {
                before.insert(next, state.clone());
                pending.push_back(next);
            }
        }
    }
    if cleanups.len() != 1 {
        return Err("missing or inconsistent normal returns");
    }
    Ok(Summary {
        effect: StackEffect {
            cleanup: *cleanups.first().unwrap(),
            writes: walk.writes.into_iter().collect(),
        },
        reads: walk.reads,
        reads_other: walk.reads_other,
        writes_other: walk.writes_other,
        preserves,
        mapped_noreturn: walk.mapped_noreturn,
        returns: returns.unwrap_or(ReturnEffect::Unknown),
        escapes: walk.escapes,
    })
}

pub fn return_pop(program: &Program, insn: &Insn) -> Option<i32> {
    let raw = program.image.bytes(insn.addr, insn.length as usize)?;
    let raw = raw.strip_prefix(&[0xf3]).unwrap_or(raw);
    match raw {
        [0xc3] => Some(0),
        [0xc2, low, high] => Some(i32::from(u16::from_le_bytes([*low, *high]))),
        _ => None,
    }
}

pub fn analyze(
    program: &Program,
    analysis: &rk_frontend::Analysis,
    excluded: &BTreeSet<Addr>,
    indirect: &BTreeMap<Addr, Vec<Addr>>,
    returns_only: bool,
    settings: &Settings,
) -> Analysis {
    let mut output = Analysis::default();
    let mut callers: BTreeMap<Addr, BTreeSet<Addr>> = BTreeMap::new();
    for (&addr, result) in &analysis.ir {
        if let Ok(fir) = result {
            for insn in &fir.insns {
                if let Some(target) = target(insn) {
                    callers.entry(target).or_default().insert(addr);
                }
            }
        }
    }
    let image = &program.image;
    for &addr in program.functions.keys() {
        let helper = rk_frontend::msvc_seh_prolog(image, addr).is_some()
            || rk_frontend::msvc_seh_epilog(image, addr);
        if helper || settings.stack_helpers.contains(&addr) {
            let source = if helper { "detected" } else { "configured" };
            output.stack_helpers.insert(addr, source);
        }
        let probe = rk_frontend::msvc_chkstk(image, addr);
        if probe || settings.stack_probes.contains(&addr) {
            let source = if probe { "detected" } else { "configured" };
            output.stack_probes.insert(addr, source);
        }
    }
    let context = Context {
        program,
        analysis,
        excluded,
        settings,
        helpers: output.stack_helpers.keys().copied().collect(),
        probes: output.stack_probes.keys().copied().collect(),
    };
    let mut pending: BTreeSet<Addr> = analysis.ir.keys().copied().collect();
    let mut failures = BTreeMap::new();
    while let Some(addr) = pending.pop_first() {
        if output.functions.contains_key(&addr) {
            continue;
        }
        let result = if excluded.contains(&addr) {
            Err("opaque or native function")
        } else {
            analysis.ir[&addr]
                .as_ref()
                .map_err(|_| "unlifted function")
                .and_then(|fir| {
                    summarize(
                        &context,
                        fir,
                        &output.functions,
                        if returns_only {
                            Mode::Returns
                        } else {
                            Mode::Memory
                        },
                    )
                })
        };
        match result {
            Ok(summary) => {
                output.functions.insert(addr, summary);
                failures.remove(&addr);
                if let Some(callers) = callers.get(&addr) {
                    pending.extend(callers);
                }
            }
            Err(reason) => {
                if reason == "unproved ESP balance" {
                    output.unbalanced_returns.insert(addr);
                }
                failures.insert(addr, reason);
            }
        }
    }
    for reason in failures.values() {
        *output.rejected.entry((*reason).into()).or_default() += 1;
    }
    output.rejected_functions = failures
        .into_iter()
        .map(|(addr, reason)| {
            let detail = analysis.ir[&addr].as_ref().map_or_else(
                |_| reason.into(),
                |fir| rejection(reason, fir, &output.functions),
            );
            (addr, detail)
        })
        .collect();
    if returns_only {
        infer_cleanups(&context, &mut output);
    }
    for (&site, targets) in indirect {
        let result = join(program, excluded, targets, &output.functions);
        match result {
            Ok(summary) => {
                output.sites.insert(site, summary);
            }
            Err(reason) => {
                *output.rejected_sites.entry(reason.into()).or_default() += 1;
            }
        }
    }
    if returns_only {
        for insn in analysis
            .ir
            .values()
            .filter_map(|result| result.as_ref().ok())
            .flat_map(|fir| &fir.insns)
        {
            let import = program
                .iat_call(insn)
                .or_else(|| target(insn).and_then(|target| program.import_thunk(target)));
            let cleanup = if let Some(import) = import {
                import_pop(&settings.import_pops, import)
            } else if insn.ops.iter().any(|op| op.opcode == OpCode::CallInd) {
                settings.call_pops.get(&insn.addr).copied()
            } else {
                None
            };
            if let Some(cleanup) = cleanup {
                output.site_cleanups.insert(insn.addr, cleanup);
            }
        }
    }
    output
}

fn terminal(fir: &FunctionIr) -> bool {
    !fir.insns
        .iter()
        .any(|insn| insn.mnemonic.eq_ignore_ascii_case("RET"))
        && fir.exits.values().all(Vec::is_empty)
        && fir.import_tails.is_empty()
        && fir.dynamic.is_empty()
}

fn assumed(cleanup: i32) -> Summary {
    Summary {
        effect: StackEffect {
            cleanup,
            writes: Vec::new(),
        },
        reads: BTreeSet::new(),
        reads_other: true,
        writes_other: true,
        preserves: Fields::from_iter([Field::Ebx, Field::Esi, Field::Edi, Field::Ebp]),
        mapped_noreturn: false,
        returns: ReturnEffect::Unknown,
        escapes: true,
    }
}

fn infer_cleanups(context: &Context<'_>, output: &mut Analysis) {
    let (program, analysis, excluded) = (context.program, context.analysis, context.excluded);
    let mut known = output.functions.clone();
    for (&addr, &cleanup) in &context.settings.function_pops {
        known.entry(addr).or_insert_with(|| assumed(cleanup));
    }
    let mut candidates = BTreeSet::new();
    loop {
        let previous = candidates.len();
        for (&addr, result) in &analysis.ir {
            if excluded.contains(&addr)
                || known.contains_key(&addr)
                || context.helpers.contains(&addr)
                || context.probes.contains(&addr)
            {
                continue;
            }
            let Ok(fir) = result else { continue };
            let mut pops: BTreeSet<_> = fir
                .insns
                .iter()
                .filter(|insn| insn.mnemonic.eq_ignore_ascii_case("RET"))
                .map(|insn| return_pop(program, insn))
                .collect();
            pops.extend(
                fir.exits
                    .values()
                    .flatten()
                    .filter(|&&target| !program.is_noreturn(target))
                    .map(|target| known.get(target).map(|summary| summary.effect.cleanup)),
            );
            if pops.is_empty() && terminal(fir) {
                pops.insert(Some(0));
            }
            let Some(Some(cleanup)) = pops.first().copied().filter(|_| pops.len() == 1) else {
                continue;
            };
            known.insert(addr, assumed(cleanup));
            candidates.insert(addr);
        }
        if candidates.len() == previous {
            break;
        }
    }
    loop {
        let mut removed = Vec::new();
        let mut narrowed = false;
        for &addr in &candidates {
            let fir = analysis.ir[&addr].as_ref().unwrap();
            if terminal(fir) {
                continue;
            }
            let result = summarize(context, fir, &known, Mode::Cleanup);
            let reason = match result {
                Ok(summary) if summary.effect.cleanup == known[&addr].effect.cleanup => {
                    let preserves = known[&addr].preserves;
                    if summary.preserves & preserves != preserves {
                        known.get_mut(&addr).unwrap().preserves = summary.preserves & preserves;
                        narrowed = true;
                    }
                    continue;
                }
                Ok(_) => "inconsistent cleanup candidate",
                Err(reason) => reason,
            };
            output
                .rejected_cleanups
                .insert(addr, rejection(reason, fir, &known));
            removed.push(addr);
        }
        if removed.is_empty() && !narrowed {
            break;
        }
        for addr in removed {
            candidates.remove(&addr);
            known.remove(&addr);
        }
    }
    let mut exact = known.clone();
    for _ in 0..2 {
        let mut next = known.clone();
        for &addr in &candidates {
            let fir = analysis.ir[&addr].as_ref().unwrap();
            if terminal(fir) {
                continue;
            }
            if let Some(summary) = summarize(context, fir, &exact, Mode::Cleanup)
                .ok()
                .filter(|summary| summary.effect.cleanup == known[&addr].effect.cleanup)
            {
                next.insert(addr, summary);
            }
        }
        exact = next;
    }
    output.argument_writes = exact
        .into_iter()
        .filter(|(_, summary)| !summary.escapes)
        .map(|(addr, summary)| {
            (
                addr,
                summary
                    .effect
                    .writes
                    .into_iter()
                    .filter(|&(at, _)| at >= 0)
                    .collect(),
            )
        })
        .collect();
    output.cleanups = candidates
        .into_iter()
        .chain(context.settings.function_pops.keys().copied())
        .filter(|addr| !output.functions.contains_key(addr))
        .map(|addr| (addr, known[&addr].effect.cleanup))
        .collect();
}

fn join(
    program: &Program,
    excluded: &BTreeSet<Addr>,
    targets: &[Addr],
    known: &BTreeMap<Addr, Summary>,
) -> Checked<Summary> {
    let mut result: Option<Summary> = None;
    let mut mapped_noreturn = false;
    for target in targets {
        if excluded.contains(target) {
            return Err("opaque or native target");
        }
        if program.is_noreturn(*target) {
            mapped_noreturn = true;
            continue;
        }
        let next = known.get(target).ok_or("unknown returning target")?;
        if let Some(result) = &mut result {
            if result.effect.cleanup != next.effect.cleanup {
                return Err("inconsistent target cleanup");
            }
            result.effect.writes.extend_from_slice(&next.effect.writes);
            result.reads.extend(&next.reads);
            result.reads_other |= next.reads_other;
            result.writes_other |= next.writes_other;
            result.preserves = result.preserves & next.preserves;
            result.mapped_noreturn |= next.mapped_noreturn;
            result.escapes |= next.escapes;
            if result.returns != next.returns {
                result.returns = ReturnEffect::Unknown;
            }
        } else {
            result = Some(next.clone());
        }
    }
    let mut result = result.ok_or("no returning target")?;
    result.mapped_noreturn |= mapped_noreturn;
    result.effect.writes.sort_unstable();
    result.effect.writes.dedup();
    Ok(result)
}
