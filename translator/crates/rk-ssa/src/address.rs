use rk_core::contract::Field;
use rk_core::ir::OpCode;
use rk_core::memory::{Address, Base};
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
        let esp = rk_ir::lanes(Field::Esp);
        if args
            .iter()
            .zip(esp)
            .all(|(&id, &key)| self.ssa.value(id).op == Op::Input(key))
        {
            return Address::exact(Base::Frame, 0);
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
}
