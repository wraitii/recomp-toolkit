use std::collections::BTreeMap;
use std::sync::OnceLock;

use rk_core::ir::{Space, Varnode};
use rk_sleigh::Lifter;

#[derive(Debug)]
pub struct Registers {
    map: BTreeMap<String, Varnode>,
    covering: Vec<Option<(String, Varnode)>>,
}

static REGISTERS: OnceLock<Registers> = OnceLock::new();

impl Registers {
    pub fn get() -> &'static Registers {
        REGISTERS.get_or_init(|| Self::scan().expect("SLEIGH x86 register table"))
    }

    fn scan() -> Result<Self, String> {
        let lifter = Lifter::new().map_err(|e| e.to_string())?;
        let mut named: Vec<(&str, u64, u32)> = lifter
            .registers()
            .filter(|&(_, offset, size)| offset < 0x2000 && [1, 2, 4, 8, 10, 16].contains(&size))
            .collect();
        named.sort_by_key(|&(_, offset, size)| (size, offset));
        let mut map = BTreeMap::new();
        for (name, offset, size) in named {
            map.entry(name.to_string())
                .or_insert_with(|| Varnode::new(Space::Register, offset, size));
        }
        let end = map
            .values()
            .filter(|v| v.space == Space::Register)
            .map(|v| v.offset + v.size as u64)
            .max()
            .unwrap_or(0);
        let covering = (0..end)
            .map(|offset| {
                map.iter()
                    .filter(|(_, v)| {
                        v.space == Space::Register
                            && v.offset <= offset
                            && offset < v.offset + v.size as u64
                    })
                    .min_by_key(|(_, v)| (v.size, v.offset))
                    .map(|(name, v)| (name.clone(), *v))
            })
            .collect();
        Ok(Self { map, covering })
    }

    pub fn register(&self, name: &str) -> Option<Varnode> {
        self.map.get(name).copied()
    }

    pub fn covering(&self, offset: u64) -> Option<(&str, Varnode)> {
        let (name, v) = self.covering.get(usize::try_from(offset).ok()?)?.as_ref()?;
        Some((name.as_str(), *v))
    }

    pub fn require(&self, name: &str) -> Varnode {
        self.register(name)
            .unwrap_or_else(|| panic!("SLEIGH x86 register {name}"))
    }
}

use rk_core::contract::Field;

use crate::StateKey;

static LANES: OnceLock<[Vec<StateKey>; 14]> = OnceLock::new();

pub fn lanes(field: Field) -> &'static [StateKey] {
    let lanes = LANES.get_or_init(|| {
        std::array::from_fn(|index| {
            let field = Field::ALL[index];
            Registers::get()
                .register(field.name())
                .filter(|register| register.space == Space::Register)
                .map(|register| {
                    (0..register.size)
                        .map(|n| StateKey::Register((register.offset + n as u64) as u32))
                        .collect()
                })
                .unwrap_or_default()
        })
    });
    &lanes[field as usize]
}

pub fn field_lane(key: StateKey) -> Option<(Field, u8)> {
    for field in Field::ALL {
        if let Some(lane) = lanes(field).iter().position(|candidate| *candidate == key) {
            return Some((field, lane as u8));
        }
    }
    None
}

pub fn xmm_offset(key: StateKey) -> Option<(usize, u8)> {
    let StateKey::Register(offset) = key else {
        return None;
    };
    let (name, register) = Registers::get().covering(offset as u64)?;
    let (reg, lane) = name.strip_prefix("XMM")?.split_once('_')?;
    let reg = reg.parse().ok().filter(|reg| *reg < 8)?;
    let mut chars = lane.bytes();
    let kind = chars.next()?;
    let index = (chars.next()? as usize).checked_sub('a' as usize)?;
    let within = (offset as u64 - register.offset) as usize;
    let offset = match kind {
        b'B' if index < 16 => index,
        b'D' if index < 4 => index * 4 + within,
        _ => return None,
    };
    Some((reg, offset as u8))
}
