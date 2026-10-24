use rk_core::contract::Field;
use rk_core::ir::OpCode;
use rk_core::memory::{Address, Base, Range};
use rk_core::{Lookup, LookupSet};
use rk_ir::Op;

use crate::{Ssa, ValueId};

pub struct Addresses<'a> {
    ssa: &'a Ssa,
    owners: Lookup<ValueId, usize>,
    packs: LookupSet<[ValueId; 4]>,
    pack_memo: Lookup<[ValueId; 4], Address>,
    steps: usize,
    memo: Vec<Option<Address>>,
    active: Vec<bool>,
    depth: usize,
    parameters: bool,
}

impl<'a> Addresses<'a> {
    pub fn new(ssa: &'a Ssa) -> Self {
        let owners = ssa
            .blocks
            .iter()
            .enumerate()
            .flat_map(|(block, item)| item.phis.iter().map(move |&phi| (phi, block)))
            .collect();
        Self {
            ssa,
            owners,
            packs: LookupSet::default(),
            pack_memo: Lookup::default(),
            steps: 0,
            memo: vec![None; ssa.values.len()],
            active: vec![false; ssa.values.len()],
            depth: 0,
            parameters: false,
        }
    }

    pub fn with_parameters(ssa: &'a Ssa) -> Self {
        Self {
            parameters: true,
            ..Self::new(ssa)
        }
    }

    pub fn get(&mut self, id: ValueId) -> Address {
        if let Some(address) = self.memo[id as usize] {
            return address;
        }
        self.steps += 1;
        if self.active[id as usize] || self.depth >= 256 || self.steps > 50_000 {
            return Address::Unknown;
        }
        self.active[id as usize] = true;
        self.depth += 1;
        let computed = self.compute(id);
        let result = if computed == Address::Unknown && self.ssa.value(id).size == 4 {
            Address::exact(Base::Value(id), 0)
        } else {
            computed
        };
        self.depth -= 1;
        self.active[id as usize] = false;
        self.memo[id as usize] = Some(result);
        result
    }

    pub fn state(&mut self, site: ValueId, field: Field) -> Address {
        if field == Field::Esp {
            if let Some(lanes) = self.ssa.stack_states.get(&site) {
                return self.packed(lanes);
            }
        }
        let Some(state) = self.ssa.boundary(site) else {
            return Address::Unknown;
        };
        let lanes: Option<Vec<_>> = rk_ir::lanes(field)
            .iter()
            .map(|key| {
                state
                    .iter()
                    .find(|(candidate, _)| candidate == key)
                    .map(|&(_, id)| id)
            })
            .collect();
        lanes.map_or(Address::Unknown, |lanes| self.packed(&lanes))
    }

    fn lane_source(&self, id: ValueId, lane: usize) -> Option<ValueId> {
        if self.ssa.value(id).op != Op::Byte(lane as u8) {
            return None;
        }
        self.ssa
            .args(id)
            .first()
            .copied()
            .filter(|&root| self.ssa.value(root).size == 4)
    }

    fn packed(&mut self, args: &[ValueId]) -> Address {
        let Ok(key) = <[ValueId; 4]>::try_from(args) else {
            return Address::Unknown;
        };
        if let Some(&address) = self.pack_memo.get(&key) {
            return address;
        }
        self.steps += 1;
        if self.depth >= 256 || self.steps > 50_000 || !self.packs.insert(key) {
            return Address::Unknown;
        }
        self.depth += 1;
        let result = self.packed_inner(args);
        self.depth -= 1;
        self.packs.remove(&key);
        self.pack_memo.insert(key, result);
        result
    }

    fn packed_inner(&mut self, args: &[ValueId]) -> Address {
        if self.parameters
            && args
                .iter()
                .any(|&id| matches!(self.ssa.value(id).op, Op::StateKeep(_)))
        {
            let prior: Vec<_> = args
                .iter()
                .map(|&id| {
                    if matches!(self.ssa.value(id).op, Op::StateKeep(_)) {
                        self.ssa.args(id)[1]
                    } else {
                        id
                    }
                })
                .collect();
            return self.packed(&prior);
        }
        let esp = rk_ir::lanes(Field::Esp);
        if args
            .iter()
            .zip(esp)
            .all(|(&id, &key)| self.ssa.value(id).op == Op::Input(key))
        {
            return Address::exact(Base::Frame, 0);
        }
        if self.parameters {
            for field in Field::ALL.into_iter().take(8) {
                if args
                    .iter()
                    .zip(rk_ir::lanes(field))
                    .all(|(&id, &key)| self.ssa.value(id).op == Op::Input(key))
                {
                    return Address::exact(Base::Parameter(field as u8), 0);
                }
            }
        }
        let roots: Option<Vec<_>> = args
            .iter()
            .enumerate()
            .map(|(lane, &id)| self.lane_source(id, lane))
            .collect();
        if let Some(roots) = roots {
            if roots.iter().all(|&root| root == roots[0]) {
                return self.get(roots[0]);
            }
        }
        if self.parameters {
            let (mut low, mut high) = (0u32, 0u32);
            for (lane, &id) in args.iter().enumerate() {
                if let Op::Const(value) = self.ssa.value(id).op {
                    low |= ((value as u32) & 0xff) << (lane * 8);
                    high |= ((value as u32) & 0xff) << (lane * 8);
                } else {
                    high |= 0xff << (lane * 8);
                }
            }
            if high < i32::MAX as u32 {
                return Address::exact(Base::Absolute, low).bounded(0, high - low);
            }
        }
        let phis: Vec<ValueId> = args
            .iter()
            .copied()
            .filter(|&id| self.ssa.value(id).op == Op::Phi)
            .collect();
        if let Some(&first) = phis.first() {
            let Some(&owner) = self.owners.get(&first) else {
                return Address::Unknown;
            };
            let count = self.ssa.args(first).len();
            if phis.iter().any(|phi| {
                self.owners.get(phi) != Some(&owner) || self.ssa.args(*phi).len() != count
            }) {
                return Address::Unknown;
            }
            let mut result = None;
            for edge in 0..count {
                let inputs: Vec<_> = args
                    .iter()
                    .map(|&id| {
                        if self.ssa.value(id).op == Op::Phi {
                            self.ssa.args(id)[edge]
                        } else {
                            id
                        }
                    })
                    .collect();
                if inputs == args {
                    continue;
                }
                let address = self.packed(&inputs);
                if result.is_some_and(|prior| prior != address) {
                    return Address::Unknown;
                }
                result = Some(address);
            }
            return result.unwrap_or(Address::Unknown);
        }
        Address::Unknown
    }

    fn compute(&mut self, id: ValueId) -> Address {
        let value = self.ssa.value(id);
        let args = self.ssa.args(id);
        match value.op {
            Op::StateKeep(_) if self.parameters || self.ssa.assume_private_stack => {
                self.get(args[1])
            }
            Op::P(OpCode::Load) if self.parameters && value.size == 4 => {
                let address = self.get(args[1]);
                self.memory_value(args[0], address, &mut LookupSet::default(), 0)
            }
            Op::Const(data) if value.size == 4 => Address::exact(Base::Absolute, data as u32),
            Op::Pack if value.size == 4 => self.packed(args),
            Op::Phi if value.size == 4 => {
                let mut result = None;
                for &arg in args {
                    if arg == id {
                        continue;
                    }
                    let next = self.get(arg);
                    if result.is_some_and(|prior| prior != next) {
                        return Address::Unknown;
                    }
                    result = Some(next);
                }
                result.unwrap_or(Address::Unknown)
            }
            Op::P(OpCode::Copy) if value.size == 4 && self.ssa.value(args[0]).size == 4 => {
                self.get(args[0])
            }
            Op::P(OpCode::IntAdd) if value.size == 4 => {
                let a = self.get(args[0]);
                let b = self.get(args[1]);
                a.add(b)
            }
            Op::P(OpCode::IntSub) if value.size == 4 => {
                let a = self.get(args[0]);
                let b = self.get(args[1]);
                a.sub(b)
            }
            Op::P(OpCode::IntAnd) if value.size == 4 => {
                for (input, mask) in [(args[0], args[1]), (args[1], args[0])] {
                    if let Op::Const(mask) = self.ssa.value(mask).op {
                        let cleared = !(mask as u32);
                        if cleared < 256 && cleared & cleared.wrapping_add(1) == 0 {
                            return self.get(input).bounded(cleared, 0);
                        }
                        if mask < i32::MAX as u64 {
                            return Address::exact(Base::Absolute, 0).bounded(0, mask as u32);
                        }
                    }
                }
                Address::Unknown
            }
            Op::P(OpCode::IntMult | OpCode::IntLeft) if value.size == 4 => {
                let a = self.get(args[0]);
                let Op::Const(scale) = self.ssa.value(args[1]).op else {
                    return Address::Unknown;
                };
                let scale = if value.op == Op::P(OpCode::IntLeft) {
                    let Some(scale) = u32::try_from(scale)
                        .ok()
                        .and_then(|scale| 1u32.checked_shl(scale))
                    else {
                        return Address::Unknown;
                    };
                    scale
                } else {
                    scale as u32
                };
                if let Address::Known {
                    base: Base::Absolute,
                    offset,
                    span,
                } = a
                {
                    if let Some(end) = offset
                        .checked_add(span)
                        .and_then(|end| end.checked_mul(scale))
                        .filter(|&end| end < i32::MAX as u32)
                    {
                        return Address::exact(Base::Absolute, offset * scale)
                            .bounded(0, end - offset * scale);
                    }
                }
                Address::Unknown
            }
            _ => Address::Unknown,
        }
    }

    pub fn call_frame(&mut self, site: ValueId) -> Address {
        self.state(site, Field::Esp)
    }

    pub fn argument(&mut self, site: ValueId, offset: i32) -> Address {
        let address = self.call_frame(site).offset(offset as u32);
        self.memory_value(
            self.ssa.args(site)[0],
            address,
            &mut LookupSet::default(),
            0,
        )
    }

    fn memory_value(
        &mut self,
        token: ValueId,
        address: Address,
        active: &mut LookupSet<ValueId>,
        depth: usize,
    ) -> Address {
        self.steps += 1;
        if !address.is_exact() || depth >= 256 || self.steps > 50_000 || !active.insert(token) {
            return Address::Unknown;
        }
        let args = self.ssa.args(token);
        let result = match self.ssa.value(token).op {
            Op::Input(rk_ir::StateKey::Memory) => match address {
                Address::Known {
                    base: Base::Frame,
                    offset,
                    span: 0,
                } if (offset as i32) >= 4 => Address::exact(Base::Argument(offset as i32), 0),
                _ => Address::Unknown,
            },
            Op::Phi => {
                let mut result = None;
                for &arg in args {
                    let next = self.memory_value(arg, address, active, depth + 1);
                    if result.is_some_and(|prior| prior != next) {
                        result = Some(Address::Unknown);
                        break;
                    }
                    result = Some(next);
                }
                result.unwrap_or(Address::Unknown)
            }
            Op::Memory if !args.is_empty() => {
                let effect = args[0];
                let inputs = self.ssa.args(effect);
                match self.ssa.value(effect).op {
                    Op::P(OpCode::Store) => {
                        let at = self.get(inputs[1]);
                        let size = u32::from(self.ssa.value(inputs[2]).size);
                        if size == 4 && at == address {
                            self.get(inputs[2])
                        } else if Range::new(at, size).disjoint(Range::new(address, 4)) {
                            self.memory_value(inputs[0], address, active, depth + 1)
                        } else {
                            Address::Unknown
                        }
                    }
                    Op::P(OpCode::Load) => self.memory_value(inputs[0], address, active, depth + 1),
                    Op::X87(op) if op.memory() == Some(rk_ir::x87::MemoryEffect::Read) => {
                        self.memory_value(inputs[0], address, active, depth + 1)
                    }
                    Op::Memory | Op::Phi | Op::Input(_) => {
                        self.memory_value(effect, address, active, depth + 1)
                    }
                    _ => Address::Unknown,
                }
            }
            _ => Address::Unknown,
        };
        active.remove(&token);
        result
    }
}
