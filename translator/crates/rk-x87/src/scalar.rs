use std::collections::BTreeMap;

use rk_ir::x87::{
    Arithmetic, ArithmeticOp, X87Condition, X87Environment, X87Legacy, X87Op, X87Unary,
};
use rk_ssa::ValueId;

use crate::types::{
    arithmetic, binary, carry, cast, constant_float, empty, env, external, group, helper, literal,
    load, one, resident, st, unary, zero, Assign, Choice, Event, Expr, Type,
};

pub const PARTS: [&str; 4] = ["bits", "exact", "value", "tag"];

pub fn part_bit(part: &str) -> u8 {
    match part {
        "bits" => 1,
        "exact" => 2,
        "value" => 4,
        "tag" => 8,
        _ => 0,
    }
}

pub fn parts(mask: u8) -> impl Iterator<Item = &'static str> {
    PARTS
        .into_iter()
        .filter(move |part| mask & part_bit(part) != 0)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Slot {
    pub value: Option<Expr>,
    pub bits: Option<Expr>,
    pub exact: Option<Expr>,
    pub tag: Option<Expr>,
    pub narrow: bool,
    pub dirty: u8,
    pub quiet: bool,
    pub pending: bool,
}

impl Slot {
    pub fn get(&self, part: &str) -> Option<&Expr> {
        match part {
            "value" => self.value.as_ref(),
            "bits" => self.bits.as_ref(),
            "exact" => self.exact.as_ref(),
            "tag" => self.tag.as_ref(),
            _ => None,
        }
    }

    pub fn set(&mut self, part: &str, value: Expr) {
        match part {
            "value" => self.value = Some(value),
            "bits" => self.bits = Some(value),
            "exact" => self.exact = Some(value),
            "tag" => self.tag = Some(value),
            _ => {}
        }
    }

    pub fn mask(&self) -> u8 {
        let mut mask = 0;
        for part in PARTS {
            if self.get(part).is_some() {
                mask |= part_bit(part);
            }
        }
        mask
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlotShape {
    pub parts: u8,
    pub narrow: bool,
    pub consts: Vec<(&'static str, Expr)>,
    pub dirty: u8,
    pub quiet: bool,
    pub pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarryShape {
    pub active: bool,
    pub slots: Vec<(i32, SlotShape)>,
    pub base: i32,
    pub low: i32,
    pub high: i32,
    pub top_unknown: bool,
    pub status_dirty: bool,
}

pub const INACTIVE: CarryShape = CarryShape {
    active: false,
    slots: Vec::new(),
    base: 0,
    low: 0,
    high: 0,
    top_unknown: false,
    status_dirty: false,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    Known(CarryShape),
    Unknown,
    Unsafe,
}

pub fn merge_shapes(shapes: &[Shape]) -> Shape {
    let mut known = Vec::new();
    for shape in shapes {
        match shape {
            Shape::Known(shape) => known.push(shape),
            Shape::Unsafe => return Shape::Unsafe,
            Shape::Unknown => return Shape::Unknown,
        }
    }
    if known.is_empty() {
        return Shape::Unknown;
    }
    if !known.iter().any(|shape| shape.active) {
        return Shape::Known(INACTIVE);
    }
    let mut slots: BTreeMap<i32, Vec<&SlotShape>> = BTreeMap::new();
    for shape in &known {
        if !shape.active {
            continue;
        }
        for (position, slot) in &shape.slots {
            slots.entry(*position).or_default().push(slot);
        }
    }
    let mut merged = Vec::new();
    for (position, entries) in slots {
        let dirty = entries.iter().fold(0, |mask, slot| mask | slot.dirty);
        let common = if entries.len() == known.len() {
            entries.iter().fold(u8::MAX, |mask, slot| mask & slot.parts)
        } else {
            0
        };
        let mask = dirty | common;
        if mask == 0 {
            continue;
        }
        let narrow = entries.len() == known.len() && entries.iter().all(|slot| slot.narrow);
        let quiet = entries.len() == known.len() && entries.iter().all(|slot| slot.quiet);
        let pending = entries.len() == known.len() && entries.iter().all(|slot| slot.pending);
        let mut consts = Vec::new();
        for part in parts(mask) {
            let mut value: Option<Expr> = None;
            let mut agrees = entries.len() == known.len();
            for slot in &entries {
                if slot.parts & part_bit(part) == 0 {
                    agrees = false;
                    break;
                }
                let found = slot
                    .consts
                    .iter()
                    .find(|(name, _)| *name == part)
                    .map(|(_, value)| value);
                match (&value, found) {
                    (None, Some(found)) => value = Some(found.clone()),
                    (Some(value), Some(found)) if value == found => {}
                    _ => {
                        agrees = false;
                        break;
                    }
                }
            }
            if agrees {
                if let Some(value) = value {
                    consts.push((part, value));
                }
            }
        }
        merged.push((
            position,
            SlotShape {
                parts: mask,
                narrow,
                consts,
                dirty,
                quiet,
                pending,
            },
        ));
    }
    let low = known
        .iter()
        .map(|shape| if shape.active { shape.low } else { 0 })
        .min()
        .unwrap_or(0);
    let high = known
        .iter()
        .map(|shape| if shape.active { shape.high } else { 0 })
        .max()
        .unwrap_or(0);
    let base = known
        .iter()
        .map(|shape| if shape.active { shape.base } else { 0 })
        .min()
        .unwrap_or(0);
    let positions = merged.iter().map(|(position, _)| *position);
    let min_position = positions.clone().min().unwrap_or(0);
    let max_position = positions.max().unwrap_or(0);
    if high - low >= 8 || max_position - min_position >= 8 {
        return Shape::Unsafe;
    }
    let published = known
        .iter()
        .map(|shape| if shape.active { shape.base } else { 0 })
        .collect::<std::collections::BTreeSet<_>>();
    Shape::Known(CarryShape {
        active: true,
        slots: merged,
        base,
        low,
        high,
        top_unknown: published.len() > 1 || known.iter().any(|shape| shape.top_unknown),
        status_dirty: known.iter().any(|shape| shape.status_dirty),
    })
}

pub struct Scalar {
    pub clone: Option<&'static str>,
    pub activations: u32,
    pub choice: Choice,
    pub known_mode: bool,
    pub convention: bool,
    lazy_nan: bool,
    strict: bool,
    pub serial: u32,
    pub temps: Vec<Expr>,
    active: bool,
    top: i32,
    top_dirty: bool,
    top_unknown: bool,
    position: i32,
    base: i32,
    low: i32,
    high: i32,
    status_dirty: bool,
    slots: BTreeMap<u8, Slot>,
}

impl Scalar {
    pub fn new(convention: bool, choice: Choice, known_mode: bool, strict: bool) -> Self {
        Self {
            clone: None,
            activations: 0,
            choice,
            known_mode,
            convention,
            lazy_nan: !strict && convention,
            strict,
            serial: 0,
            temps: Vec::new(),
            active: false,
            top: 0,
            top_dirty: false,
            top_unknown: false,
            position: 0,
            base: 0,
            low: 0,
            high: 0,
            status_dirty: false,
            slots: BTreeMap::new(),
        }
    }

    pub fn reset(&mut self) {
        self.active = false;
        self.top = 0;
        self.top_dirty = false;
        self.top_unknown = false;
        self.position = 0;
        self.base = 0;
        self.low = 0;
        self.high = 0;
        self.status_dirty = false;
        self.slots.clear();
    }

    fn activation(&mut self) -> Vec<Event> {
        let event = Event::Activation(crate::types::Activation {
            index: self.activations,
            clone: self.clone,
            choice: self.choice,
            known_mode: self.known_mode,
        });
        self.activations += 1;
        vec![event]
    }

    fn activate(&mut self, events: &mut Vec<Event>) {
        if !self.active {
            events.extend(self.activation());
            self.active = true;
        }
    }

    pub(crate) fn stage(&mut self, value: Expr, events: &mut Vec<Event>) -> Expr {
        self.temp(value, events)
    }

    fn temp(&mut self, value: Expr, events: &mut Vec<Event>) -> Expr {
        let ty = value.ty();
        let local = Expr::Local(self.serial, ty);
        self.serial += 1;
        self.temps.push(local.clone());
        events.push(Event::Declare(local.clone()));
        events.push(Event::Assign(Assign::new(local.clone(), value)));
        local
    }

    fn phys(&self, offset: u8) -> Expr {
        Expr::Physical(Box::new(env("top")), offset as u32)
    }

    fn slot(&mut self, logical: i32) -> (u8, &mut Slot) {
        self.high = self.high.max(self.position + logical);
        let offset = ((self.top + logical) & 7) as u8;
        (offset, self.slots.entry(offset).or_default())
    }

    pub(crate) fn read(
        &mut self,
        logical: i32,
        part: &'static str,
        events: &mut Vec<Event>,
    ) -> Expr {
        let offset = self.slot(logical).0;
        let slot = self.slots.get_mut(&offset).expect("slot");
        if let Some(value) = slot.get(part) {
            return value.clone();
        }
        let value = self.temp(crate::types::slot(part, self.phys(offset)), events);
        self.slots
            .get_mut(&offset)
            .expect("slot")
            .set(part, value.clone());
        value
    }

    fn assign(
        &mut self,
        logical: i32,
        narrow: Option<bool>,
        quiet: Option<bool>,
        values: &[(&'static str, Expr)],
    ) {
        let (_, slot) = self.slot(logical);
        if let Some(narrow) = narrow {
            slot.narrow = narrow;
        }
        if let Some(quiet) = quiet {
            slot.quiet = quiet;
        }
        for (part, value) in values {
            slot.set(part, value.clone());
            slot.dirty |= part_bit(part);
        }
    }

    fn parts(&mut self, logical: i32, events: &mut Vec<Event>) -> Vec<(&'static str, Expr)> {
        let mut parts = Vec::new();
        for part in PARTS {
            let value = self.read(logical, part, events);
            parts.push((part, value));
        }
        parts
    }

    fn fold(&mut self, logical: i32, events: &mut Vec<Event>, emit_ie: bool) {
        let (offset, slot) = self.slot(logical);
        if !slot.pending {
            return;
        }
        slot.pending = false;
        let Some(value) = slot.value.clone() else {
            return;
        };
        if emit_ie {
            events.push(Event::Assign(Assign {
                target: env("sw"),
                value: Expr::NanTest(Box::new(value.clone())),
                op: "|=",
            }));
            self.status_dirty = true;
        }
        let value = self.temp(Expr::NanFold(Box::new(value)), events);
        let slot = self.slots.get_mut(&offset).expect("slot");
        slot.value = Some(value);
        slot.dirty |= part_bit("value");
        slot.quiet = true;
    }

    fn fold_all(&mut self, events: &mut Vec<Event>, emit_ie: bool) {
        let offsets: Vec<_> = self.slots.keys().copied().collect();
        for offset in offsets {
            self.fold(offset as i32 - self.top, events, emit_ie);
        }
    }

    pub fn fold_pending(&mut self, events: &mut Vec<Event>) {
        if self.active {
            self.fold_all(events, true);
        }
    }

    fn release(&mut self, logical: i32, events: &mut Vec<Event>, subsumed: bool) {
        if subsumed {
            self.slot(logical).1.pending = false;
        } else {
            self.fold(logical, events, true);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn set(
        &mut self,
        logical: i32,
        value: Expr,
        events: &mut Vec<Event>,
        bits: Option<Expr>,
        exact: Option<Expr>,
        tag: Option<Expr>,
        narrow: bool,
    ) {
        self.release(logical, events, false);
        let value = self.temp(value, events);
        let bits = bits.unwrap_or_else(|| literal(0, Type::U64));
        let exact = exact.unwrap_or_else(zero);
        let tag = tag.unwrap_or_else(|| helper("ftag_classify", vec![value.clone()]));
        self.assign(
            logical,
            Some(narrow),
            Some(false),
            &[
                ("value", value),
                ("bits", bits),
                ("exact", exact),
                ("tag", tag),
            ],
        );
    }

    fn move_top(&mut self, delta: i32) {
        self.top = (self.top + delta) & 7;
        self.top_dirty = true;
        self.position += delta;
        self.low = self.low.min(self.position);
    }

    fn drop_slot(&mut self, events: &mut Vec<Event>, subsumed: bool) {
        self.release(0, events, subsumed);
        self.assign(0, None, None, &[("exact", zero()), ("tag", empty())]);
        self.move_top(1);
    }

    fn publish(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        let mut tags = Vec::new();
        for (offset, slot) in &mut self.slots {
            let physical = Expr::Physical(Box::new(env("top")), *offset as u32);
            let position = self.low + ((*offset as i32 - self.low) & 7);
            if self.convention && position < self.position {
                if position >= self.base && slot.dirty & part_bit("tag") != 0 {
                    if let Some(tag) = &slot.tag {
                        tags.push((physical, tag.clone()));
                    }
                }
            } else {
                for part in ["value", "exact", "bits"] {
                    if slot.dirty & part_bit(part) != 0 {
                        if let Some(value) = slot.get(part) {
                            events.push(Event::Assign(Assign::new(
                                crate::types::slot(part, physical.clone()),
                                value.clone(),
                            )));
                        }
                    }
                }
                if slot.dirty & part_bit("tag") != 0 {
                    if let Some(tag) = &slot.tag {
                        tags.push((physical, tag.clone()));
                    }
                }
            }
            slot.dirty = 0;
        }
        if !tags.is_empty() {
            events.push(Event::PublishTags(tags));
        }
        let top_moved = if self.convention {
            self.position != self.base
        } else {
            self.top_dirty
        };
        if top_moved || self.top_unknown {
            events.push(Event::Assign(Assign::new(
                resident("top"),
                group(self.phys(self.top as u8)),
            )));
        }
        self.top_dirty = false;
        self.top_unknown = false;
        self.base = self.position;
        self.low = self.position;
        self.high = self.position;
        if self.status_dirty {
            events.push(Event::Assign(Assign::new(resident("sw"), env("sw"))));
            self.status_dirty = false;
        }
        events
    }

    pub fn flush(&mut self) -> Vec<Event> {
        if !self.active {
            return Vec::new();
        }
        let convention =
            self.convention && self.high < self.low + 8 && self.position < self.low + 8;
        if convention {
            self.convention = true;
        }
        let mut events = Vec::new();
        self.fold_pending(&mut events);
        events.extend(self.publish());
        events
    }

    pub fn snapshot(&self) -> Shape {
        if !self.active {
            return Shape::Known(INACTIVE);
        }
        let mut slots = Vec::new();
        for (offset, slot) in &self.slots {
            let position = self.low + ((*offset as i32 - self.low) & 7) - self.position;
            let mut dirty = slot.dirty;
            let mut parts = slot.mask();
            if position < 0 && self.convention {
                parts &= part_bit("tag");
            }
            dirty &= parts;
            if parts == 0 {
                continue;
            }
            let consts = PARTS
                .into_iter()
                .filter(|part| parts & part_bit(part) != 0)
                .filter_map(|part| match slot.get(part) {
                    Some(value @ Expr::Literal(_, _)) => Some((part, value.clone())),
                    _ => None,
                })
                .collect();
            slots.push((
                position,
                SlotShape {
                    parts,
                    narrow: slot.narrow,
                    consts,
                    dirty,
                    quiet: slot.quiet,
                    pending: slot.pending,
                },
            ));
        }
        slots.sort_by_key(|(position, _)| *position);
        Shape::Known(CarryShape {
            active: true,
            slots,
            base: self.base - self.position,
            low: self.low - self.position,
            high: self.high - self.position,
            top_unknown: self.top_unknown,
            status_dirty: self.status_dirty,
        })
    }

    pub fn seed(&mut self, shape: &CarryShape, values: &BTreeMap<(i32, &'static str), Expr>) {
        self.reset();
        if !shape.active {
            return;
        }
        self.active = true;
        self.base = shape.base;
        self.low = shape.low;
        self.high = shape.high;
        self.top_unknown = shape.top_unknown || shape.base != 0;
        self.status_dirty = shape.status_dirty;
        for (position, slot_shape) in &shape.slots {
            let mut slot = Slot {
                narrow: slot_shape.narrow,
                dirty: slot_shape.dirty,
                quiet: slot_shape.quiet,
                pending: slot_shape.pending,
                ..Slot::default()
            };
            for part in parts(slot_shape.parts) {
                let value = slot_shape
                    .consts
                    .iter()
                    .find(|(name, _)| *name == part)
                    .map(|(_, value)| value.clone())
                    .or_else(|| values.get(&(*position, part)).cloned())
                    .unwrap_or_else(|| carry(*position, part));
                slot.set(part, value);
            }
            self.slots.insert((*position & 7) as u8, slot);
        }
    }

    pub fn normalize(&mut self, events: &mut Vec<Event>) {
        if !self.active {
            events.extend(self.activation());
            self.active = true;
            return;
        }
        if self.top != 0 {
            events.push(Event::Assign(Assign::new(
                env("top"),
                self.phys(self.top as u8),
            )));
            self.slots = self
                .slots
                .iter()
                .map(|(offset, slot)| ((((*offset as i32 - self.top) & 7) as u8), slot.clone()))
                .collect();
        }
        let delta = self.position;
        self.top = 0;
        self.position -= delta;
        self.base -= delta;
        self.low -= delta;
        self.high -= delta;
    }

    fn cw(&self) -> Expr {
        match self.choice {
            Choice::Fast => Expr::Cw(0),
            Choice::Dynamic => env("cw"),
        }
    }

    fn eager(&mut self, events: &mut Vec<Event>, name: &'static str, args: Vec<Expr>) {
        events.extend(self.flush());
        events.push(Event::Evaluate(helper(name, args)));
        events.push(Event::Reset);
        self.reset();
    }

    fn store_integer(&mut self, bits: u16, address: Expr, events: &mut Vec<Event>) {
        self.fold(0, events, true);
        let value = self.read(0, "value", events);
        let exact = self.read(0, "exact", events);
        let raw = self.read(0, "bits", events);
        let conversion = Expr::IntegerConvert {
            bits,
            args: vec![env("sw"), self.cw(), value, exact, raw],
        };
        events.push(Event::Store {
            bits,
            address,
            value: conversion,
            floating: false,
            bcd: false,
        });
        self.status_dirty = true;
    }

    pub fn apply(
        &mut self,
        op: X87Op,
        address: Option<Expr>,
        result: Option<ValueId>,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        self.activate(&mut events);
        let sw = env("sw");
        let cw = self.cw();
        match op {
            X87Op::Constant(name) => {
                self.move_top(-1);
                let narrow = name == "FLD1" || name == "FLDZ";
                self.set(
                    0,
                    constant_float(name),
                    &mut events,
                    None,
                    None,
                    None,
                    narrow,
                );
            }
            X87Op::Push { src, bits } => {
                if let Some(src) = src {
                    let values = self.parts(src as i32, &mut events);
                    let slot = self
                        .slots
                        .get(&(((self.top + src as i32) & 7) as u8))
                        .cloned()
                        .unwrap_or_default();
                    self.move_top(-1);
                    self.release(0, &mut events, false);
                    self.assign(0, Some(slot.narrow), Some(slot.quiet), &values);
                    self.slot(0).1.pending = slot.pending;
                } else {
                    let address = address.expect("x87 load address");
                    let value = load(bits, address, false);
                    self.move_top(-1);
                    self.set(0, value, &mut events, None, None, None, bits == 32);
                }
            }
            X87Op::PushInteger { bits } | X87Op::PushIntegerValue { bits } => {
                let input = address.expect("x87 integer input");
                let value = if matches!(op, X87Op::PushIntegerValue { .. }) {
                    cast(
                        match bits {
                            16 => Type::I16,
                            32 => Type::I32,
                            64 => Type::I64,
                            _ => unreachable!(),
                        },
                        input,
                    )
                } else {
                    load(bits, input, true)
                };
                self.move_top(-1);
                self.set(
                    0,
                    cast(Type::F64, value.clone()),
                    &mut events,
                    Some(cast(Type::U64, value)),
                    Some(one()),
                    None,
                    false,
                );
            }
            X87Op::Store {
                dst,
                bits,
                pop,
                integer,
            } => {
                if let Some(dst) = dst {
                    if dst == 0 {
                        if pop {
                            self.drop_slot(&mut events, false);
                        }
                        return events;
                    }
                    let values = self.parts(0, &mut events);
                    let slot = self
                        .slots
                        .get(&(self.top as u8))
                        .cloned()
                        .unwrap_or_default();
                    self.release(dst as i32, &mut events, false);
                    self.assign(dst as i32, Some(slot.narrow), Some(slot.quiet), &values);
                    self.slot(dst as i32).1.pending = slot.pending;
                } else if integer {
                    self.store_integer(
                        bits,
                        address.expect("x87 integer store address"),
                        &mut events,
                    );
                } else {
                    self.fold(0, &mut events, true);
                    let address = address.expect("x87 store address");
                    let value = self.read(0, "value", &mut events);
                    let slot = self
                        .slots
                        .get(&(self.top as u8))
                        .cloned()
                        .unwrap_or_default();
                    let value = if bits == 32 {
                        if slot.narrow && !self.strict {
                            if slot.quiet && self.known_mode {
                                cast(Type::F32, group(value))
                            } else {
                                Expr::FloatStore(Box::new(cw.clone()), Box::new(value))
                            }
                        } else {
                            helper("fto_float", vec![cw.clone(), value])
                        }
                    } else if bits == 80 {
                        cast(Type::F64, value)
                    } else {
                        value
                    };
                    events.push(Event::Store {
                        bits,
                        address,
                        value,
                        floating: true,
                        bcd: false,
                    });
                }
                if pop {
                    self.drop_slot(&mut events, dst.is_some_and(|dst| dst != 0));
                }
            }
            X87Op::Arithmetic(Arithmetic {
                operator,
                integer,
                reverse,
                pops,
                dst,
                src,
                bits,
            }) => {
                let narrow;
                let (mut lhs, mut rhs);
                if bits != 0 {
                    let address = address.expect("x87 arithmetic address");
                    rhs = load(bits, address, integer);
                    if integer {
                        rhs = cast(Type::F64, rhs);
                    }
                    lhs = self.read(0, "value", &mut events);
                    narrow = self
                        .slots
                        .get(&(self.top as u8))
                        .map(|slot| slot.narrow)
                        .unwrap_or(false)
                        && !integer
                        && bits == 32;
                } else {
                    let lhs_slot = self
                        .slots
                        .get(&(((self.top + dst as i32) & 7) as u8))
                        .cloned()
                        .unwrap_or_default();
                    let rhs_slot = self
                        .slots
                        .get(&(((self.top + src as i32) & 7) as u8))
                        .cloned()
                        .unwrap_or_default();
                    lhs = self.read(dst as i32, "value", &mut events);
                    rhs = self.read(src as i32, "value", &mut events);
                    narrow = lhs_slot.narrow && rhs_slot.narrow;
                }
                if reverse {
                    std::mem::swap(&mut lhs, &mut rhs);
                }
                let raw = arithmetic(operator, lhs.clone(), rhs.clone(), sw.clone());
                let value = if narrow && operator != ArithmeticOp::Div && !self.strict {
                    Expr::FloatArithmetic(
                        match operator {
                            ArithmeticOp::Add => "+",
                            ArithmeticOp::Sub => "-",
                            ArithmeticOp::Mul => "*",
                            ArithmeticOp::Div => "/",
                        },
                        Box::new(cw.clone()),
                        Box::new(lhs),
                        Box::new(rhs),
                        (!self.lazy_nan).then(|| Box::new(sw.clone())),
                    )
                } else if self.lazy_nan {
                    Expr::PcRound(Box::new(cw.clone()), Box::new(raw))
                } else {
                    helper("fx87", vec![sw.clone(), cw.clone(), raw])
                };
                self.release(dst as i32, &mut events, true);
                self.set(dst as i32, value, &mut events, None, None, None, true);
                self.slot(dst as i32).1.pending = self.lazy_nan;
                self.status_dirty = true;
                if pops != 0 {
                    self.drop_slot(&mut events, true);
                }
            }
            X87Op::Compare {
                quiet,
                integer,
                src,
                bits,
                pops,
                zero,
            } => {
                self.fold(0, &mut events, true);
                if bits == 0 && !zero {
                    self.fold(src as i32, &mut events, true);
                }
                let other = if zero {
                    Expr::Literal(Type::F64, 0)
                } else if bits != 0 {
                    let address = address.expect("x87 compare address");
                    let value = load(bits, address, integer);
                    if integer {
                        cast(Type::F64, value)
                    } else {
                        value
                    }
                } else {
                    self.read(src as i32, "value", &mut events)
                };
                let a = self.read(0, "value", &mut events);
                events.push(Event::Evaluate(helper(
                    if quiet { "fucom" } else { "fcom" },
                    vec![sw.clone(), a, other],
                )));
                self.status_dirty = true;
                for _ in 0..pops {
                    self.drop_slot(&mut events, false);
                }
            }
            X87Op::CompareFlags { quiet, src, pop } => {
                self.fold(0, &mut events, true);
                self.fold(src as i32, &mut events, true);
                let a = self.read(0, "value", &mut events);
                let b = self.read(src as i32, "value", &mut events);
                let value = helper(
                    "fcomi_flags",
                    vec![sw.clone(), a, b, literal(quiet as u64, Type::U32)],
                );
                if let Some(result) = result {
                    events.push(Event::Assign(Assign::new(external(result), value)));
                }
                self.status_dirty = true;
                if pop {
                    self.drop_slot(&mut events, false);
                }
            }
            X87Op::Unary(op) => {
                if matches!(op, X87Unary::Abs | X87Unary::Negate) {
                    self.fold(0, &mut events, true);
                } else {
                    self.release(0, &mut events, true);
                }
                let source = self.read(0, "value", &mut events);
                let slot = self
                    .slots
                    .get(&(self.top as u8))
                    .cloned()
                    .unwrap_or_default();
                let narrow = matches!(op, X87Unary::Abs | X87Unary::Negate | X87Unary::Sqrt)
                    && (slot.narrow || matches!(op, X87Unary::Sqrt));
                let value = unary(op, source, sw.clone(), cw.clone());
                self.set(0, value, &mut events, None, None, None, narrow);
                self.status_dirty = true;
            }
            X87Op::Remainder { ieee } => {
                let a = self.read(0, "value", &mut events);
                let b = self.read(1, "value", &mut events);
                let value = helper(
                    "fprem",
                    vec![sw.clone(), a, b, literal(ieee as u64, Type::U32)],
                );
                self.set(0, value, &mut events, None, None, None, false);
                self.status_dirty = true;
            }
            X87Op::Exchange { src } => {
                let a = self.parts(0, &mut events);
                let b = self.parts(src as i32, &mut events);
                let a_slot = self
                    .slots
                    .get(&(self.top as u8))
                    .cloned()
                    .unwrap_or_default();
                let b_slot = self
                    .slots
                    .get(&(((self.top + src as i32) & 7) as u8))
                    .cloned()
                    .unwrap_or_default();
                self.assign(0, Some(b_slot.narrow), Some(b_slot.quiet), &b);
                self.assign(src as i32, Some(a_slot.narrow), Some(a_slot.quiet), &a);
                self.slot(0).1.pending = b_slot.pending;
                self.slot(src as i32).1.pending = a_slot.pending;
            }
            X87Op::Free { src } => {
                self.assign(src as i32, None, None, &[("tag", empty())]);
            }
            X87Op::Status => {
                self.fold_pending(&mut events);
                let value =
                    Expr::StatusWord(Box::new(sw.clone()), Box::new(self.phys(self.top as u8)));
                if let Some(result) = result {
                    events.push(Event::Assign(Assign::new(external(result), value)));
                }
            }
            X87Op::Environment(kind) => {
                let address = address.expect("x87 environment address");
                match kind {
                    X87Environment::LoadCw => {
                        events.extend(self.flush());
                        events.push(Event::Evaluate(helper(
                            "x87_set_cw",
                            vec![Expr::Cpu, Expr::LoadWord(Box::new(address))],
                        )));
                        events.push(Event::Reset);
                        self.reset();
                    }
                    X87Environment::StoreCw => {
                        events.extend(self.flush());
                        events.push(Event::Store {
                            bits: 16,
                            address,
                            value: resident("cw"),
                            floating: false,
                            bcd: false,
                        });
                    }
                    X87Environment::StoreSw => {
                        events.extend(self.flush());
                        events.push(Event::Store {
                            bits: 16,
                            address,
                            value: helper("fstsw", vec![Expr::Cpu]),
                            floating: false,
                            bcd: false,
                        });
                    }
                    X87Environment::StoreEnv => {
                        self.eager(&mut events, "x87_fnstenv", vec![Expr::Cpu, address])
                    }
                    X87Environment::Save => {
                        self.eager(&mut events, "x87_fnsave", vec![Expr::Cpu, address])
                    }
                    X87Environment::LoadEnv => {
                        self.eager(&mut events, "x87_fldenv", vec![Expr::Cpu, address])
                    }
                    X87Environment::Restore => {
                        self.eager(&mut events, "x87_frstor", vec![Expr::Cpu, address])
                    }
                    X87Environment::EmptyTags(reset_top) => {
                        events.extend(self.flush());
                        events.push(Event::EmptyTags(reset_top));
                        events.push(Event::Reset);
                        self.reset();
                    }
                }
            }
            X87Op::Clear => {
                self.fold_all(&mut events, false);
                events.push(Event::Assign(Assign {
                    target: sw.clone(),
                    value: Expr::Binary("&", Box::new(sw), Box::new(Expr::ClearMask(0x80ff))),
                    op: "=",
                }));
                self.status_dirty = true;
            }
            X87Op::Init => self.eager(&mut events, "x87_finit", vec![Expr::Cpu]),
            X87Op::Rotate(delta) => {
                self.move_top(if delta < 0 { -1 } else { 1 });
                events.push(Event::Assign(Assign {
                    target: sw.clone(),
                    value: Expr::Binary("&", Box::new(sw), Box::new(Expr::ClearMask(0x200))),
                    op: "=",
                }));
                self.status_dirty = true;
            }
            X87Op::Conditional(condition, src) => {
                events.extend(self.flush());
                events.push(Event::Conditional(
                    Expr::FlagCondition(condition),
                    vec![Event::Evaluate(helper(
                        "fcopy",
                        vec![
                            Expr::Cpu,
                            literal(0, Type::U32),
                            literal(src as u64, Type::U32),
                        ],
                    ))],
                ));
                events.push(Event::Reset);
                self.reset();
            }
            X87Op::Extract => {
                self.fold_pending(&mut events);
                let captured = self.read(0, "value", &mut events);
                let exponent = helper("fxtract_exponent", vec![captured.clone()]);
                let significand = helper("fxtract_significand", vec![captured]);
                self.move_top(-1);
                self.set(0, significand, &mut events, None, None, None, false);
                self.set(1, exponent, &mut events, None, None, None, false);
            }
            X87Op::LegacyEnvironment(kind) => {
                let address = address.expect("x87 legacy environment address");
                match kind {
                    X87Legacy::Save => {
                        self.eager(&mut events, "x87_fnsave", vec![Expr::Cpu, address])
                    }
                    X87Legacy::Restore => {
                        self.eager(&mut events, "x87_frstor", vec![Expr::Cpu, address])
                    }
                    X87Legacy::StoreEnv => {
                        self.eager(&mut events, "x87_fnstenv", vec![Expr::Cpu, address])
                    }
                    X87Legacy::LoadEnv => {
                        self.eager(&mut events, "x87_fldenv", vec![Expr::Cpu, address])
                    }
                    X87Legacy::BcdStore => {
                        events.extend(self.flush());
                        events.push(Event::Store {
                            bits: 80,
                            address,
                            value: helper("fpop", vec![Expr::Cpu]),
                            floating: false,
                            bcd: true,
                        });
                        events.push(Event::Reset);
                        self.reset();
                    }
                }
            }
            X87Op::EmptyTags(reset_top) => {
                events.extend(self.flush());
                events.push(Event::EmptyTags(reset_top));
                events.push(Event::Reset);
                self.reset();
            }
            X87Op::Nop => {}
            X87Op::Eager(kind) => {
                use rk_ir::x87::X87Eager;
                match kind {
                    X87Eager::Fptan => {
                        events.extend(self.flush());
                        events.push(Event::Evaluate(helper(
                            "fset",
                            vec![
                                Expr::Cpu,
                                literal(0, Type::U32),
                                helper(
                                    "fx87_exact",
                                    vec![resident("sw"), helper("tan", vec![st(0)])],
                                ),
                            ],
                        )));
                        events.push(Event::Evaluate(helper(
                            "fpush",
                            vec![Expr::Cpu, constant_float("FLD1")],
                        )));
                        events.push(Event::Reset);
                        self.reset();
                    }
                    X87Eager::Fsincos => {
                        events.extend(self.flush());
                        let captured = self.temp(st(0), &mut events);
                        events.push(Event::Evaluate(helper(
                            "fset",
                            vec![
                                Expr::Cpu,
                                literal(0, Type::U32),
                                helper(
                                    "fx87_exact",
                                    vec![resident("sw"), helper("sin", vec![captured.clone()])],
                                ),
                            ],
                        )));
                        events.push(Event::Evaluate(helper(
                            "fpush",
                            vec![
                                Expr::Cpu,
                                helper(
                                    "fx87_exact",
                                    vec![resident("sw"), helper("cos", vec![captured])],
                                ),
                            ],
                        )));
                        events.push(Event::Reset);
                        self.reset();
                    }
                    X87Eager::Fscale => {
                        events.extend(self.flush());
                        let value = helper("fscale", vec![st(0), st(1)]);
                        self.set(
                            0,
                            helper("fx87_exact", vec![env("sw"), value]),
                            &mut events,
                            None,
                            None,
                            None,
                            false,
                        );
                        self.status_dirty = true;
                    }
                    X87Eager::Fpatan => {
                        events.extend(self.flush());
                        let value = helper(
                            "fx87_exact",
                            vec![env("sw"), helper("atan2", vec![st(1), st(0)])],
                        );
                        self.set(1, value, &mut events, None, None, None, false);
                        self.drop_slot(&mut events, true);
                        self.status_dirty = true;
                    }
                    X87Eager::Fyl2x | X87Eager::Fyl2xp1 => {
                        events.extend(self.flush());
                        let inner = helper(
                            if matches!(kind, X87Eager::Fyl2x) {
                                "log2"
                            } else {
                                "log1p"
                            },
                            vec![st(0)],
                        );
                        let mut value = binary("*", st(1), inner);
                        if matches!(kind, X87Eager::Fyl2xp1) {
                            value = binary("/", value, Expr::Ln2);
                        }
                        let value = helper("fx87_exact", vec![env("sw"), value]);
                        self.set(1, value, &mut events, None, None, None, false);
                        self.drop_slot(&mut events, true);
                        self.status_dirty = true;
                    }
                    X87Eager::Xtract => {
                        self.fold_pending(&mut events);
                        let captured = self.read(0, "value", &mut events);
                        let exponent = helper("fxtract_exponent", vec![captured.clone()]);
                        let significand = helper("fxtract_significand", vec![captured]);
                        self.move_top(-1);
                        self.set(0, significand, &mut events, None, None, None, false);
                        self.set(1, exponent, &mut events, None, None, None, false);
                    }
                    X87Eager::Fbstp => {
                        events.extend(self.flush());
                        events.push(Event::Store {
                            bits: 80,
                            address: address.expect("x87 bcd address"),
                            value: helper("fpop", vec![Expr::Cpu]),
                            floating: false,
                            bcd: true,
                        });
                        events.push(Event::Reset);
                        self.reset();
                    }
                    X87Eager::ClearExceptions => {
                        self.fold_all(&mut events, false);
                        events.push(Event::Assign(Assign {
                            target: sw.clone(),
                            value: Expr::Binary(
                                "&",
                                Box::new(sw),
                                Box::new(Expr::ClearMask(0x80ff)),
                            ),
                            op: "=",
                        }));
                        self.status_dirty = true;
                    }
                    X87Eager::Fxam => self.eager(&mut events, "fxam", vec![Expr::Cpu]),
                    X87Eager::Init => self.eager(&mut events, "x87_finit", vec![Expr::Cpu]),
                }
            }
        }
        events
    }
}

pub fn carry_values(shape: &CarryShape) -> BTreeMap<(i32, &'static str), Expr> {
    let mut values = BTreeMap::new();
    for (position, slot) in &shape.slots {
        for part in parts(slot.parts) {
            let value = slot
                .consts
                .iter()
                .find(|(name, _)| *name == part)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| carry(*position, part));
            values.insert((*position, part), value);
        }
    }
    values
}

fn _keep(_: &X87Condition, _: &Expr) {}
