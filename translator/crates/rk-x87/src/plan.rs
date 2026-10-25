use std::collections::{BTreeMap, BTreeSet};

use rk_core::{Addr, Reason};
use rk_ir::x87::{X87Op, X87Unary};
use rk_ir::{properties, Op};
use rk_ssa::{BlockId, Ssa, ValueId, DISPATCH_BLOCK};

use crate::carry::{self, assert_covers, reset_op};
use crate::scalar::{carry_values, parts, CarryShape, Scalar, Shape};
use crate::types::{carry, env, Assign, Choice, Event, Expr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeForce {
    Auto,
    Dynamic,
    Fast,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub strict: bool,
    pub convention: bool,
    pub cw_clone: bool,
    pub force: ModeForce,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            strict: false,
            convention: true,
            cw_clone: true,
            force: ModeForce::Auto,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModeCondition {
    pub mask: u16,
    pub value: u16,
}

#[derive(Clone, Debug, Default)]
pub struct ModeInput {
    pub known_modes: BTreeSet<Addr>,
}

#[derive(Clone, Debug, Default)]
pub struct CloneProgram {
    pub clone: Option<&'static str>,
    pub blocks: BTreeMap<BlockId, Vec<Event>>,
    pub events: BTreeMap<ValueId, Vec<Event>>,
    pub defaults: BTreeMap<ValueId, Vec<Event>>,
    pub ends: BTreeMap<BlockId, Vec<Event>>,
    pub activations: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub used: bool,
    pub declarations: Vec<Expr>,
    pub programs: Vec<CloneProgram>,
}

impl Plan {
    pub fn clones(&self) -> usize {
        self.programs.len()
    }

    pub fn clone(&self, index: usize) -> &CloneProgram {
        &self.programs[index]
    }
}

fn successors(s: &Ssa) -> BTreeMap<BlockId, Vec<BlockId>> {
    let mut succ: BTreeMap<BlockId, Vec<BlockId>> = BTreeMap::new();
    for (index, block) in s.blocks.iter().enumerate() {
        succ.entry(index as BlockId).or_default();
        for &pred in &block.preds {
            if pred != DISPATCH_BLOCK {
                let row = succ.entry(pred).or_default();
                if !row.contains(&(index as BlockId)) {
                    row.push(index as BlockId);
                }
            }
        }
    }
    succ
}

fn x87_sites(s: &Ssa) -> Vec<(Addr, rk_ir::x87::X87Op)> {
    let mut sites = Vec::new();
    for block in &s.blocks {
        for &value in &block.ops {
            if let Op::X87(op) = s.value(value).op {
                sites.push((s.value_addr[value as usize], op));
            }
        }
    }
    sites
}

fn clone_sensitive(op: X87Op) -> bool {
    match op {
        X87Op::Arithmetic(_) => true,
        X87Op::Unary(X87Unary::Sqrt | X87Unary::Round) => true,
        X87Op::Store {
            dst: None,
            integer: false,
            ..
        } => true,
        X87Op::Store { integer: true, .. } | X87Op::StoreValue { .. } => true,
        _ => false,
    }
}

fn fast_choice(
    s: &Ssa,
    modes: &ModeInput,
    force: ModeForce,
    cw_clone: bool,
) -> (Choice, Vec<Option<&'static str>>, bool) {
    if force == ModeForce::Dynamic {
        return (Choice::Dynamic, vec![None], false);
    }
    if !cw_clone && force == ModeForce::Auto {
        return (Choice::Dynamic, vec![None], false);
    }
    let sites = x87_sites(s);
    let sensitive = sites.iter().filter(|(_, op)| clone_sensitive(*op)).count();
    if force == ModeForce::Fast {
        if !modes.known_modes.contains(&s.addr) {
            return (Choice::Dynamic, vec![Some("fast"), Some("general")], false);
        }
        return (Choice::Fast, vec![Some("fast")], true);
    }
    if modes.known_modes.contains(&s.addr) && sensitive == 0 {
        return (Choice::Fast, vec![Some("fast")], true);
    }
    let instructions = s.blocks.iter().map(|block| block.end).max().unwrap_or(0) as usize;
    if sensitive >= 4 && sensitive * 8 >= instructions {
        (Choice::Dynamic, vec![Some("fast"), Some("general")], false)
    } else {
        (Choice::Dynamic, vec![None], false)
    }
}

fn predecessor_map(s: &Ssa) -> BTreeMap<BlockId, Vec<BlockId>> {
    s.blocks
        .iter()
        .enumerate()
        .map(|(index, block)| {
            (
                index as BlockId,
                block
                    .preds
                    .iter()
                    .copied()
                    .filter(|pred| *pred != DISPATCH_BLOCK)
                    .collect(),
            )
        })
        .collect()
}

fn linear_predecessors(
    s: &Ssa,
    succ: &BTreeMap<BlockId, Vec<BlockId>>,
    preds: &BTreeMap<BlockId, Vec<BlockId>>,
    entry: &BTreeMap<BlockId, Shape>,
) -> BTreeMap<BlockId, BlockId> {
    let mut linear = BTreeMap::new();
    for (index, _) in s.blocks.iter().enumerate() {
        let block = index as BlockId;
        let Some(list) = preds.get(&block) else {
            continue;
        };
        if list.len() != 1 {
            continue;
        }
        let prev = list[0];
        let Some(shape) = entry.get(&block) else {
            continue;
        };
        if matches!(shape, Shape::Unsafe | Shape::Unknown) {
            continue;
        }
        let Some(prev_block) = s.block(prev) else {
            continue;
        };
        let has_terminator = prev_block
            .ops
            .iter()
            .any(|value| properties(s.value(*value).op).terminator);
        if prev_block.end == s.blocks[index].start
            && succ.get(&prev) == Some(&vec![block])
            && !has_terminator
        {
            linear.insert(block, prev);
        }
    }
    linear
}

fn seed(scalar: &mut Scalar, shape: &Shape) {
    match shape {
        Shape::Known(shape) => scalar.seed(shape, &carry_values(shape)),
        _ => scalar.seed(
            &CarryShape {
                active: false,
                slots: Vec::new(),
                base: 0,
                low: 0,
                high: 0,
                top_unknown: false,
                status_dirty: false,
            },
            &BTreeMap::new(),
        ),
    }
}

fn carry_references(value: &Expr, destinations: &BTreeSet<(i32, &'static str)>) -> bool {
    match value {
        Expr::Carry(position, part) => destinations.contains(&(*position, *part)),
        Expr::AxMerge(value)
        | Expr::Slot(_, value)
        | Expr::Physical(value, _)
        | Expr::Rotate(value, _)
        | Expr::Group(value)
        | Expr::Cast(_, value)
        | Expr::Negate(value)
        | Expr::NanFold(value)
        | Expr::NanTest(value)
        | Expr::LoadWord(value) => carry_references(value, destinations),
        Expr::Binary(_, lhs, rhs)
        | Expr::PcRound(lhs, rhs)
        | Expr::FloatStore(lhs, rhs)
        | Expr::StatusWord(lhs, rhs) => {
            carry_references(lhs, destinations) || carry_references(rhs, destinations)
        }
        Expr::FloatArithmetic(_, cw, lhs, rhs, sw) => {
            carry_references(cw, destinations)
                || carry_references(lhs, destinations)
                || carry_references(rhs, destinations)
                || sw
                    .as_ref()
                    .is_some_and(|sw| carry_references(sw, destinations))
        }
        Expr::Load { address, .. } => carry_references(address, destinations),
        Expr::IntegerConvert { args, .. } | Expr::Helper(_, args) => {
            args.iter().any(|arg| carry_references(arg, destinations))
        }
        _ => false,
    }
}

fn transition(
    scalar: &mut Scalar,
    targets: &[BlockId],
    entry: &BTreeMap<BlockId, Shape>,
    events: &mut Vec<Event>,
) -> Result<(), Reason> {
    scalar.fold_pending(events);
    let mut active = Vec::new();
    let mut fallback = false;
    let live = scalar.snapshot();
    for &target in targets {
        match entry.get(&target) {
            Some(Shape::Known(shape)) if shape.active => active.push((target, shape)),
            Some(Shape::Known(shape)) => {
                assert_covers(&Shape::Known(shape.clone()), &live, "x87 edge")?
            }
            _ => fallback = true,
        }
    }
    if !active.is_empty() {
        scalar.normalize(events);
        let live = scalar.snapshot();
        for (target, shape) in &active {
            assert_covers(
                &Shape::Known((*shape).clone()),
                &live,
                &format!("B{target}"),
            )?;
        }
        let mut assigned: Vec<Expr> = Vec::new();
        let mut assignments = Vec::new();
        for (_, shape) in &active {
            for (position, slot) in &shape.slots {
                for part in parts(slot.parts) {
                    if slot.consts.iter().any(|(name, _)| *name == part) {
                        continue;
                    }
                    let name = carry(*position, part);
                    if assigned.contains(&name) {
                        continue;
                    }
                    let value = scalar.read(*position, part, events);
                    assigned.push(name.clone());
                    assignments.push((name, value));
                }
            }
        }
        let destinations = assignments
            .iter()
            .filter_map(|(name, _)| match name {
                Expr::Carry(position, part) => Some((*position, *part)),
                _ => None,
            })
            .collect();
        let assignments: Vec<_> = assignments
            .into_iter()
            .map(|(name, value)| {
                let value = if carry_references(&value, &destinations) {
                    scalar.stage(value, events)
                } else {
                    value
                };
                (name, value)
            })
            .collect();
        if fallback {
            events.extend(scalar.flush());
        }
        for (name, value) in assignments {
            events.push(Event::Assign(Assign::new(name, value)));
        }
    } else if fallback {
        events.extend(scalar.flush());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn lower_clone(
    s: &Ssa,
    clone: Option<&'static str>,
    choice: Choice,
    known_mode: bool,
    strict: bool,
    _convention: bool,
    carry_mode: bool,
    entry: &BTreeMap<BlockId, Shape>,
    linear: &BTreeMap<BlockId, BlockId>,
    succ: &BTreeMap<BlockId, Vec<BlockId>>,
    scalar: &mut Scalar,
) -> Result<CloneProgram, Reason> {
    scalar.reset();
    scalar.clone = clone;
    scalar.choice = choice;
    scalar.known_mode = known_mode;
    scalar.activations = 0;
    let mut program = CloneProgram {
        clone,
        ..CloneProgram::default()
    };
    let mut previous: Option<BlockId> = None;
    for (index, block) in s.blocks.iter().enumerate() {
        let id = index as BlockId;
        let mut start = Vec::new();
        if carry_mode {
            if !linear.contains_key(&id) {
                seed(
                    scalar,
                    entry.get(&id).unwrap_or(&Shape::Known(CarryShape {
                        active: false,
                        slots: Vec::new(),
                        base: 0,
                        low: 0,
                        high: 0,
                        top_unknown: false,
                        status_dirty: false,
                    })),
                );
            }
        } else if previous != linear.get(&id).copied() {
            scalar.reset();
            start.push(Event::Reset);
        }
        program.blocks.insert(id, start);

        for &value in &block.ops {
            let mut events = Vec::new();
            match s.value(value).op {
                Op::X87(op) => {
                    if strict && op.touches_memory() {
                        events.extend(scalar.flush());
                    }
                    let address = if op.touches_memory()
                        || matches!(op, X87Op::PushIntegerValue { .. } | X87Op::PushValue { .. })
                    {
                        s.args(value).last().map(|arg| Expr::External(*arg))
                    } else {
                        None
                    };
                    let result = (op.result_size() > 0).then_some(value);
                    events.extend(scalar.apply(op, address, result));
                }
                op if reset_op(op) => {
                    events.extend(scalar.flush());
                    events.push(Event::Reset);
                    scalar.reset();
                }
                Op::Return | Op::Tail { .. } | Op::Breakpoint => {
                    events.extend(scalar.flush());
                }
                op @ (Op::Branch { .. }
                | Op::CBranch { .. }
                | Op::BranchInd
                | Op::ImportTail { .. }) => {
                    let targets = succ.get(&id).cloned().unwrap_or_default();
                    if carry_mode {
                        if carry::external_exit(s, value) {
                            events.extend(scalar.flush());
                        }
                        transition(scalar, &targets, entry, &mut events)?;
                        if let Op::BranchInd = op {
                            let mut defaults = scalar.flush();
                            program
                                .defaults
                                .insert(value, std::mem::take(&mut defaults));
                        }
                    } else {
                        events.extend(scalar.flush());
                    }
                }
                _ => {}
            }
            if !events.is_empty() {
                program.events.insert(value, events);
            }
        }

        let terminated = block
            .ops
            .iter()
            .any(|value| properties(s.value(*value).op).terminator);
        let mut end = Vec::new();
        if !terminated {
            let targets = succ.get(&id).cloned().unwrap_or_default();
            if targets.is_empty() {
                end.extend(scalar.flush());
            } else if carry_mode {
                if linear.get(targets.first().unwrap_or(&id)).copied() != Some(id) {
                    transition(scalar, &targets, entry, &mut end)?;
                }
            } else if linear.get(targets.first().unwrap_or(&id)).copied() != Some(id) {
                end.extend(scalar.flush());
            }
        }
        program.ends.insert(id, end);
        previous = Some(id);
    }
    program.activations = scalar.activations;
    Ok(program)
}

pub fn prepare(s: &Ssa, modes: &ModeInput, options: &Options) -> Result<Plan, Reason> {
    let used = s.blocks.iter().any(|block| {
        block
            .ops
            .iter()
            .any(|value| matches!(s.value(*value).op, Op::X87(_)))
    });
    if !used {
        return Ok(Plan::default());
    }
    let (choice, clones, known_mode) = fast_choice(s, modes, options.force, options.cw_clone);
    let succ = successors(s);
    let preds = predecessor_map(s);
    let carry_mode = !options.strict && options.convention;
    let entry = if carry_mode {
        let factory = || Scalar::new(options.convention, choice, known_mode, options.strict);
        carry::analyze(s, &factory)?
    } else {
        BTreeMap::new()
    };
    let linear = if carry_mode {
        linear_predecessors(s, &succ, &preds, &entry)
    } else {
        BTreeMap::new()
    };
    let mut scalar = Scalar::new(options.convention, choice, known_mode, options.strict);
    let mut programs = Vec::new();
    for clone in &clones {
        let clone_choice = match *clone {
            Some("fast") => Choice::Fast,
            _ => Choice::Dynamic,
        };
        programs.push(lower_clone(
            s,
            *clone,
            clone_choice,
            known_mode,
            options.strict,
            options.convention,
            carry_mode,
            &entry,
            &linear,
            &succ,
            &mut scalar,
        )?);
    }
    if options.force == ModeForce::Auto
        && !known_mode
        && programs.len() > 1
        && programs[0].activations > 1
    {
        let dynamic = Options {
            strict: options.strict,
            convention: options.convention,
            cw_clone: options.cw_clone,
            force: ModeForce::Dynamic,
        };
        return prepare(s, modes, &dynamic);
    }
    let mut declarations = Vec::new();
    let mut declared = BTreeSet::new();
    for (position, slot) in entry.values().flat_map(|shape| match shape {
        Shape::Known(shape) => shape.slots.clone(),
        _ => Vec::new(),
    }) {
        for part in parts(slot.parts) {
            if slot.consts.iter().any(|(name, _)| *name == part) {
                continue;
            }
            if declared.insert((position, part)) {
                declarations.push(carry(position, part));
            }
        }
    }
    declarations.extend(scalar.temps.iter().cloned());
    declarations.extend([env("cw"), env("sw"), env("top")]);
    Ok(Plan {
        used: true,
        declarations,
        programs,
    })
}
