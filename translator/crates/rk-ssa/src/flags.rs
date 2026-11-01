use std::collections::BTreeMap;

use rk_core::contract::{Field, Fields};
use rk_core::Lookup;
use rk_ir::{Op, StateKey};

use crate::{BlockId, FlagId, FlagKind, Ssa, ValueId, DISPATCH_BLOCK};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlagSource {
    Record(FlagId),
    Input(StateKey),
    Const(u64),
    Phi(Vec<FlagSource>),
    Unknown,
}

#[derive(Clone, Copy, Debug)]
pub struct FlagDescriptor {
    pub kind: FlagKind,
    pub kind_value: Option<ValueId>,
    pub size_value: Option<ValueId>,
    pub size: u8,
    pub mask: Fields,
    pub a: Option<ValueId>,
    pub b: Option<ValueId>,
    pub carry: Option<ValueId>,
    pub result: ValueId,
}

impl FlagDescriptor {
    pub fn values(&self) -> Vec<ValueId> {
        match self.kind {
            FlagKind::Logic => vec![self.result],
            FlagKind::Inc | FlagKind::Dec => self.a.into_iter().chain([self.result]).collect(),
            FlagKind::Adc | FlagKind::Sbb => self
                .a
                .into_iter()
                .chain(self.b)
                .chain(self.carry)
                .chain([self.result])
                .collect(),
            FlagKind::Join => self
                .kind_value
                .into_iter()
                .chain(self.size_value)
                .chain(self.a)
                .chain(self.b)
                .chain(self.carry)
                .chain([self.result])
                .collect(),
            _ => self
                .a
                .into_iter()
                .chain(self.b)
                .chain([self.result])
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FlagPublication {
    pub descriptor: Option<FlagDescriptor>,
    pub forwarded: Fields,
}

pub struct FlagPlan {
    record_of: Lookup<ValueId, FlagId>,
}

fn is_flag(field: Field) -> bool {
    Fields::FLAGS.contains(field)
}

impl Ssa {
    pub fn flag_plan(&self) -> FlagPlan {
        let mut record_of = Lookup::default();
        for (index, record) in self.flags.iter().enumerate() {
            for (field, value) in &record.flags {
                if record.defined.contains(*field) {
                    record_of.insert(*value, index as FlagId);
                }
            }
        }
        FlagPlan { record_of }
    }
}

fn flag_key_indices(s: &Ssa) -> BTreeMap<Field, usize> {
    let mut index = BTreeMap::new();
    for (position, field) in s.field_of.iter().enumerate() {
        if let Some(field) = field.filter(|field| is_flag(*field)) {
            index.entry(field).or_insert(position);
        }
    }
    index
}

fn block_entry_state(s: &Ssa, block: usize) -> Vec<Option<ValueId>> {
    let mut state = vec![None; s.keys.len()];
    for &phi in &s.blocks[block].phis {
        if let Some(&index) = s.phi_key.get(&phi) {
            state[index] = Some(phi);
        }
    }
    if state.iter().all(Option::is_some) {
        return state;
    }
    for index in 0..s.keys.len() {
        if state[index].is_some() {
            continue;
        }
        let mut value = None;
        let mut same = true;
        for pred in &s.blocks[block].preds {
            let candidate = if *pred == DISPATCH_BLOCK {
                s.dispatch.loads.get(index).map(|(_, value)| *value)
            } else {
                s.exit_state[*pred as usize].get(index).copied().flatten()
            };
            match candidate {
                Some(candidate) if value.is_some_and(|value| value != candidate) => same = false,
                Some(candidate) => value = Some(candidate),
                None => same = false,
            }
        }
        if same {
            state[index] = value;
        }
    }
    state
}

fn entry_flag_value(
    s: &Ssa,
    block: usize,
    field: Field,
    flag_keys: &BTreeMap<Field, usize>,
    position: usize,
) -> Option<ValueId> {
    let index = *flag_keys.get(&field)?;
    let item = &s.blocks[block];
    if let Some(&phi) = item
        .phis
        .iter()
        .find(|phi| s.phi_key.get(*phi) == Some(&index))
    {
        return s.args(phi).get(position).copied();
    }
    let pred = *item.preds.get(position)?;
    if pred == DISPATCH_BLOCK {
        return None;
    }
    s.exit_state
        .get(pred as usize)?
        .get(index)
        .copied()
        .flatten()
}

fn record_index(s: &Ssa) -> BTreeMap<(ValueId, Field), FlagId> {
    let mut index = BTreeMap::new();
    for (id, record) in s.flags.iter().enumerate() {
        for (&field, &value) in &record.flags {
            if record.defined.contains(field) && !matches!(s.value(value).op, Op::Const(_)) {
                index.entry((value, field)).or_insert(id as FlagId);
            }
        }
    }
    index
}

#[derive(Clone, Copy)]
enum JoinSource {
    Record(FlagId),
    Settled,
    Zero,
}

fn settled_at(s: &Ssa, field: Field, value: ValueId) -> Option<ValueId> {
    match s.value(value).op {
        Op::StateReload(key) | Op::StateKeep(key) if s.field(key) == Some(field) => {
            Some(s.args(value)[0])
        }
        Op::Input(key) if s.field(key) == Some(field) => Some(ValueId::MAX),
        _ => None,
    }
}

fn const_of(s: &Ssa, value: ValueId) -> Option<u64> {
    match s.value(value).op {
        Op::Const(data) => Some(data),
        _ => None,
    }
}

fn explains(s: &Ssa, source: JoinSource, field: Field, value: ValueId) -> bool {
    match source {
        JoinSource::Settled => settled_at(s, field, value).is_some(),
        JoinSource::Zero => {
            const_of(s, value) == Some(matches!(field, Field::Zf | Field::Pf) as u64)
        }
        JoinSource::Record(id) => s.flag(id).is_some_and(|record| {
            record.defined.contains(field)
                && record.flags.get(&field).is_some_and(|&own| {
                    own == value
                        || const_of(s, own).is_some_and(|data| const_of(s, value) == Some(data))
                })
        }),
    }
}

fn join_source(
    s: &Ssa,
    values: &[(Field, ValueId)],
    index: &BTreeMap<(ValueId, Field), FlagId>,
) -> Option<JoinSource> {
    let sites: Option<Vec<ValueId>> = values
        .iter()
        .map(|&(field, value)| settled_at(s, field, value))
        .collect();
    if sites.is_some_and(|sites| sites.windows(2).all(|pair| pair[0] == pair[1])) {
        return Some(JoinSource::Settled);
    }
    let mut counts: BTreeMap<FlagId, usize> = BTreeMap::new();
    for &(field, value) in values {
        if let Some(&id) = index.get(&(value, field)) {
            *counts.entry(id).or_default() += 1;
        }
    }
    let usable = |id: &FlagId| {
        s.flag(*id)
            .is_some_and(|record| (1..=4).contains(&record.size) && record_has_operands(record))
    };
    match counts
        .into_iter()
        .filter(|(id, _)| usable(id))
        .max_by_key(|(_, count)| *count)
    {
        Some((id, _)) => Some(JoinSource::Record(id)),
        None if values
            .iter()
            .all(|&(_, value)| const_of(s, value).is_some()) =>
        {
            Some(JoinSource::Zero)
        }
        None => None,
    }
}

fn merge_join_sources(
    s: &mut Ssa,
    block: usize,
    preds: &[BlockId],
    flag_keys: &BTreeMap<Field, usize>,
    index: &mut BTreeMap<(ValueId, Field), FlagId>,
) {
    let entry = block_entry_state(s, block);
    let mut incoming: Vec<Vec<(Field, ValueId)>> = vec![Vec::new(); preds.len()];
    let mut merged = Vec::new();
    for field in Fields::FLAGS.iter() {
        let Some(Some(phi)) = flag_keys.get(&field).and_then(|key| entry.get(*key)) else {
            continue;
        };
        let values: Option<Vec<ValueId>> = (0..preds.len())
            .map(|position| entry_flag_value(s, block, field, flag_keys, position))
            .collect();
        let Some(values) = values.filter(|values| values.windows(2).any(|pair| pair[0] != pair[1]))
        else {
            continue;
        };
        merged.push((field, *phi));
        for (position, value) in values.into_iter().enumerate() {
            incoming[position].push((field, value));
        }
    }
    if merged.is_empty() {
        return;
    }
    let Some(sources) = incoming
        .iter()
        .map(|values| join_source(s, values, index))
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    if !sources
        .iter()
        .any(|source| matches!(source, JoinSource::Record(_)))
    {
        return;
    }
    let mut mask = Fields::NONE;
    for (slot, &(field, _)) in merged.iter().enumerate() {
        if (0..preds.len())
            .all(|position| explains(s, sources[position], field, incoming[position][slot].1))
        {
            mask.insert(field);
        }
    }
    if mask.is_empty() {
        return;
    }
    let addr = s.blocks[block]
        .phis
        .first()
        .map_or(0, |value| s.value_addr[*value as usize]);
    let mut kinds = Vec::with_capacity(preds.len());
    let mut sizes = Vec::with_capacity(preds.len());
    let mut parts: [Vec<Option<ValueId>>; 4] = std::array::from_fn(|_| Vec::new());
    let mut constants = BTreeMap::new();
    let mut symbols: BTreeMap<&'static str, ValueId> = BTreeMap::new();
    let mut symbol = |s: &mut Ssa, name: &'static str| {
        *symbols
            .entry(name)
            .or_insert_with(|| s.new_value(Op::Symbol(name), 1, &[], addr))
    };
    for (position, source) in sources.iter().enumerate() {
        let (kind, size, operands) = match *source {
            JoinSource::Record(id) => {
                let record = s.flag(id).cloned().expect("join record");
                let kind = match (record.kind, record.kind_value) {
                    (FlagKind::Join, Some(value)) => value,
                    (kind, _) => symbol(s, cc_kind(kind).expect("record kind")),
                };
                let size = record
                    .size_value
                    .unwrap_or_else(|| s.constant(&mut constants, 1, record.size as u64));
                (
                    kind,
                    Some(size),
                    [record.a, record.b, record.carry, record.result],
                )
            }
            JoinSource::Settled => {
                let keep = incoming[position].first().map(|(_, value)| *value);
                (symbol(s, "X86_CC_NONE"), None, [None, None, keep, None])
            }
            JoinSource::Zero => (symbol(s, "X86_CC_LOGIC"), None, [None, None, None, None]),
        };
        kinds.push(kind);
        sizes.push(size);
        for (part, operand) in parts.iter_mut().zip(operands) {
            part.push(operand);
        }
    }
    let fill = match sizes.iter().flatten().find_map(|size| const_of(s, *size)) {
        Some(size) => s.constant(&mut constants, 1, size),
        None => s.constant(&mut constants, 1, 4),
    };
    let sizes: Vec<ValueId> = sizes.iter().map(|size| size.unwrap_or(fill)).collect();
    let widths: Vec<u8> = parts
        .iter()
        .enumerate()
        .map(|(slot, values)| {
            let widest = values
                .iter()
                .flatten()
                .map(|value| s.value(*value).size)
                .max();
            if slot == 2 {
                1
            } else {
                widest.unwrap_or(4)
            }
        })
        .collect();
    if parts[2]
        .iter()
        .flatten()
        .any(|value| s.value(*value).size != 1)
    {
        return;
    }
    let mut phis = Vec::with_capacity(4);
    for (values, &size) in parts.iter().zip(&widths) {
        let args: Vec<ValueId> = values
            .iter()
            .map(|value| match *value {
                Some(value) if s.value(value).size == size => value,
                Some(value) => match const_of(s, value) {
                    Some(data) => s.constant(&mut constants, size, data),
                    None => {
                        let addr = s.value_addr[value as usize];
                        s.new_value(Op::P(rk_core::ir::OpCode::IntZExt), size, &[value], addr)
                    }
                },
                None => s.constant(&mut constants, size, 0),
            })
            .collect();
        phis.push(s.new_value(Op::Phi, size, &args, addr));
    }
    let kind_value = s.new_value(Op::Phi, 1, &kinds, addr);
    let size_value = s.new_value(Op::Phi, 1, &sizes, addr);
    s.blocks[block].phis.extend([kind_value, size_value]);
    s.blocks[block].phis.extend(phis.iter().copied());
    let flags: BTreeMap<Field, ValueId> = merged
        .iter()
        .filter(|(field, _)| mask.contains(*field))
        .copied()
        .collect();
    let id = s.flags.len() as FlagId;
    for (&field, &value) in &flags {
        index.insert((value, field), id);
    }
    s.flags.push(crate::FlagRecord {
        kind: FlagKind::Join,
        size: 4,
        kind_value: Some(kind_value),
        size_value: Some(size_value),
        a: Some(phis[0]),
        b: Some(phis[1]),
        carry: Some(phis[2]),
        result: Some(phis[3]),
        flags,
        defined: mask,
    });
}

pub fn cc_kind(kind: FlagKind) -> Option<&'static str> {
    match kind {
        FlagKind::Add => Some("X86_CC_ADD"),
        FlagKind::Sub | FlagKind::Cmp => Some("X86_CC_SUB"),
        FlagKind::Logic => Some("X86_CC_LOGIC"),
        FlagKind::Inc => Some("X86_CC_INC"),
        FlagKind::Dec => Some("X86_CC_DEC"),
        FlagKind::Adc => Some("X86_CC_ADC"),
        FlagKind::Sbb => Some("X86_CC_SBB"),
        FlagKind::Incoming | FlagKind::Join => None,
    }
}

pub(crate) fn build_joins(s: &mut Ssa) {
    let mut index = record_index(s);
    let flag_keys = flag_key_indices(s);
    for block in 0..s.blocks.len() {
        let preds = s.blocks[block].preds.clone();
        if preds.len() < 2 || preds.iter().any(|pred| *pred == DISPATCH_BLOCK) {
            continue;
        }
        merge_join_sources(s, block, &preds, &flag_keys, &mut index);
    }
}

impl FlagPlan {
    pub fn source(&self, s: &Ssa, field: Field, value: ValueId) -> FlagSource {
        match s.value(value).op {
            Op::Input(key) => return FlagSource::Input(key),
            Op::Const(data) => return FlagSource::Const(data),
            _ => {}
        }
        if let Some(&record) = self.record_of.get(&value) {
            if s.flag(record)
                .is_some_and(|record| record.flags.get(&field) == Some(&value))
            {
                return FlagSource::Record(record);
            }
        }
        if s.value(value).op == Op::Phi {
            FlagSource::Phi(Vec::new())
        } else {
            FlagSource::Unknown
        }
    }

    pub fn values(&self, s: &Ssa, at: ValueId) -> BTreeMap<Field, ValueId> {
        let mut out = BTreeMap::new();
        let Some(state) = s.boundary(at) else {
            return out;
        };
        for (key, value) in state {
            if let Some(field) = s.field(*key) {
                if is_flag(field) {
                    out.insert(field, *value);
                }
            }
        }
        out
    }

    pub fn boundary(&self, s: &Ssa, at: ValueId) -> BTreeMap<Field, FlagSource> {
        self.values(s, at)
            .into_iter()
            .map(|(field, value)| (field, self.source(s, field, value)))
            .collect()
    }

    pub fn publication(
        &self,
        s: &Ssa,
        at: ValueId,
        keys: &[StateKey],
        guard_only: &[StateKey],
    ) -> FlagPublication {
        let Some(state) = s.boundary(at) else {
            return FlagPublication::default();
        };
        let wanted: Lookup<StateKey, ()> = keys.iter().map(|key| (*key, ())).collect();
        let guard_only: Lookup<StateKey, ()> = guard_only.iter().map(|key| (*key, ())).collect();
        let mut forwarded = Fields::NONE;
        let mut hot = Fields::NONE;
        let mut groups: BTreeMap<FlagId, Fields> = BTreeMap::new();
        let mut constants = Vec::new();
        for (key, value) in state {
            if wanted.get(key).is_none() {
                continue;
            }
            let Some(field) = s.field(*key) else { continue };
            if !is_flag(field) {
                continue;
            }
            let cold = guard_only.get(key).is_some();
            if !cold {
                hot.insert(field);
            }
            if cold
                && matches!(s.value(*value).op, Op::StateKeep(k) | Op::StateReload(k) if k == *key)
            {
                forwarded.insert(field);
                continue;
            }
            let FlagSource::Record(id) = self.source(s, field, *value) else {
                if const_of(s, *value).is_some() {
                    constants.push((field, *value));
                }
                continue;
            };
            let Some(record) = s.flag(id) else { continue };
            if record.size == 0
                || record.size > 4
                || !record.defined.contains(field)
                || !record_has_operands(record)
            {
                continue;
            }
            groups.entry(id).or_default().insert(field);
        }
        let score = |fields: Fields| ((fields & hot).0.count_ones(), fields.0.count_ones());
        let best = groups
            .into_iter()
            .fold(None, |best: Option<(FlagId, Fields)>, (id, fields)| {
                if best.is_none_or(|(_, current)| score(fields) > score(current)) {
                    Some((id, fields))
                } else {
                    best
                }
            });
        let descriptor = best.and_then(|(id, mut mask)| {
            for &(field, value) in &constants {
                if explains(s, JoinSource::Record(id), field, value) {
                    mask.insert(field);
                }
            }
            s.flag(id).map(|record| FlagDescriptor {
                kind: record.kind,
                kind_value: record.kind_value,
                size_value: record.size_value,
                size: record.size,
                mask,
                a: record.a,
                b: record.b,
                carry: record.carry,
                result: record.result.unwrap_or(0),
            })
        });
        FlagPublication {
            descriptor,
            forwarded,
        }
    }
}

fn record_has_operands(record: &crate::FlagRecord) -> bool {
    match record.kind {
        FlagKind::Logic => record.result.is_some(),
        FlagKind::Inc | FlagKind::Dec => record.a.is_some() && record.result.is_some(),
        FlagKind::Adc | FlagKind::Sbb => {
            record.a.is_some()
                && record.b.is_some()
                && record.carry.is_some()
                && record.result.is_some()
        }
        FlagKind::Add | FlagKind::Sub | FlagKind::Cmp => {
            record.a.is_some() && record.b.is_some() && record.result.is_some()
        }
        FlagKind::Join => {
            record.kind_value.is_some()
                && record.size_value.is_some()
                && record.a.is_some()
                && record.b.is_some()
                && record.carry.is_some()
                && record.result.is_some()
        }
        FlagKind::Incoming => false,
    }
}
