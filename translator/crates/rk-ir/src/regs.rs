use std::collections::BTreeMap;
use std::sync::OnceLock;

use rk_core::flow::{Effect, Value};
use rk_core::ir::{Insn, OpCode, Space, Varnode};
use rk_core::isa::ReturnAddress;
use rk_sleigh::Lifter;

#[derive(Clone, Copy, Debug)]
pub struct Arch {
    pub address_bytes: u8,
    pub word: u32,
    pub link: Option<Varnode>,
    pub stack_pointer: Varnode,
    pub frame_pointer: Varnode,
    pub program_counter: Varnode,
    pub thread_base: Varnode,
    pub x86: X86Registers,
}

pub const EFLAGS_MISC: Varnode = Varnode::new(Space::Register, 0xffff0000, 4);

#[derive(Clone, Copy, Debug)]
pub struct X86Registers {
    pub fpu_control: Varnode,
    pub eax: Varnode,
    pub ecx: Varnode,
    pub edx: Varnode,
    pub ebx: Varnode,
    pub esi: Varnode,
    pub edi: Varnode,
    pub cf: Varnode,
    pub pf: Varnode,
    pub af: Varnode,
    pub zf: Varnode,
    pub sf: Varnode,
    pub of: Varnode,
    pub df: Varnode,
    pub segments: [Varnode; 6],
}

impl X86Registers {
    fn resolve(get: impl Fn(&str) -> Result<Varnode, String>) -> Result<Self, String> {
        Ok(Self {
            fpu_control: get("FPUControlWord")?,
            eax: get("EAX")?,
            ecx: get("ECX")?,
            edx: get("EDX")?,
            ebx: get("EBX")?,
            esi: get("ESI")?,
            edi: get("EDI")?,
            cf: get("CF")?,
            pf: get("PF")?,
            af: get("AF")?,
            zf: get("ZF")?,
            sf: get("SF")?,
            of: get("OF")?,
            df: get("DF")?,
            segments: [
                get("CS")?,
                get("DS")?,
                get("ES")?,
                get("SS")?,
                get("FS")?,
                get("GS")?,
            ],
        })
    }

    pub fn arithmetic_flags(&self) -> [Varnode; 6] {
        [self.cf, self.pf, self.af, self.zf, self.sf, self.of]
    }
}

impl Arch {
    fn resolve(map: &BTreeMap<String, Varnode>, address_bytes: u8) -> Result<Self, String> {
        let roles = &rk_core::isa::machine().spec.registers;
        let get = |name: &str| {
            map.get(name)
                .copied()
                .ok_or_else(|| format!("SLEIGH register {name}"))
        };
        let stack_pointer = get(roles.stack_pointer)?;
        Ok(Self {
            address_bytes,
            word: stack_pointer.size,
            link: match roles.return_address {
                ReturnAddress::Stack => None,
                ReturnAddress::Register(name) => Some(get(name)?),
            },
            stack_pointer,
            frame_pointer: get(roles.frame_pointer)?,
            program_counter: get(roles.program_counter)?,
            thread_base: get(roles.thread_base)?,
            x86: X86Registers::resolve(get)?,
        })
    }

    pub fn general(&self) -> [Varnode; 8] {
        [
            self.x86.eax,
            self.x86.ecx,
            self.x86.edx,
            self.x86.ebx,
            self.stack_pointer,
            self.frame_pointer,
            self.x86.esi,
            self.x86.edi,
        ]
    }

    pub fn return_pop(&self, insn: &Insn) -> Option<i32> {
        if insn
            .ops
            .iter()
            .filter(|op| op.opcode == OpCode::Load)
            .count()
            != 1
        {
            return None;
        }
        let effect = Effect::of(&insn.ops, self.stack_pointer, self.frame_pointer);
        match effect.value(self.stack_pointer)? {
            Value::Stack(top) => i32::try_from(top - i64::from(self.word)).ok(),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct Registers {
    map: BTreeMap<String, Varnode>,
    covering: Vec<Option<(String, Varnode)>>,
    vector: Vec<Varnode>,
    userops: BTreeMap<u64, String>,
    arch: Arch,
}

static REGISTERS: OnceLock<Registers> = OnceLock::new();

impl Registers {
    pub fn get() -> &'static Registers {
        REGISTERS.get_or_init(|| Self::scan().expect("SLEIGH register table"))
    }

    fn scan() -> Result<Self, String> {
        let lifter = Lifter::new().map_err(|e| e.to_string())?;
        let mut named: Vec<(&str, u64, u32)> = lifter
            .registers()
            .filter(|&(name, _, size)| name != "contextreg" && size <= 16)
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
        let arch = Arch::resolve(&map, lifter.address_bytes())?;
        let vector = rk_core::isa::machine()
            .spec
            .registers
            .vector
            .iter()
            .map(|name| {
                map.get(*name)
                    .copied()
                    .ok_or_else(|| format!("SLEIGH register {name}"))
            })
            .collect::<Result<_, _>>()?;
        let target = rk_core::isa::machine().target.address_bytes;
        if arch.address_bytes != target {
            return Err(format!(
                "SLEIGH addresses are {} bytes but the target's are {target}",
                arch.address_bytes
            ));
        }
        Ok(Self {
            map,
            covering,
            vector,
            userops: lifter.userop_names().clone(),
            arch,
        })
    }

    pub fn userop(&self, id: Option<u64>) -> Option<&str> {
        self.userops.get(&id?).map(String::as_str)
    }

    pub fn register(&self, name: &str) -> Option<Varnode> {
        self.map.get(name).copied()
    }

    pub fn covering(&self, offset: u64) -> Option<(&str, Varnode)> {
        let (name, v) = self.covering.get(usize::try_from(offset).ok()?)?.as_ref()?;
        Some((name.as_str(), *v))
    }

    pub fn vector(&self, offset: u64) -> bool {
        self.vector
            .iter()
            .any(|v| v.offset <= offset && offset < v.offset + u64::from(v.size))
    }

    pub fn arch(&self) -> &Arch {
        &self.arch
    }

    pub fn require(&self, name: &str) -> Varnode {
        self.register(name)
            .unwrap_or_else(|| panic!("SLEIGH register {name}"))
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
