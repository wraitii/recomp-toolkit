//! Wraps the pinned Ghidra x86-32 SLEIGH specification embedded by `sleigh-config`: instruction
//! lengths, flow ends, and p-code lifting into `rk_core::ir`. Unknown address spaces fail with a
//! diagnostic and unmapped opcodes become `OpCode::Unknown`. Callers keep one `Lifter` per thread.

use std::collections::{BTreeMap, BTreeSet};

use libsla::{
    Address, AddressSpace, BoolOp, FloatOp, GhidraSleigh, InstructionLoader, IntOp, IntSign,
    OpCode as SleighOp, PcodeInstruction, Sleigh, VarnodeData,
};
use rk_core::ir::{OpCode, PcodeOp, Space, Varnode};
use rk_core::{Addr, Diagnostic, Reason, Result};

const ST_BASE: u64 = 0x1100;
const ST_STRIDE: u64 = 0x10;
const ST_SIZE: u32 = 10;
const DIAGNOSTIC_REGISTERS: [&str; 5] = [
    "FPUInstructionPointer",
    "FPUDataPointer",
    "FPULastInstructionOpcode",
    "FPUPointerSelector",
    "FPUDataSelector",
];

#[derive(Clone, Debug)]
pub struct Lifted {
    pub length: u8,
    pub ops: Vec<PcodeOp>,
    pub x87: bool,
}

#[derive(Clone, Debug)]
pub struct RawLifted {
    pub length: u8,
    pub ops: Vec<PcodeOp>,
    pub userops: Vec<u64>,
    pub mnemonic: String,
}

pub struct Lifter {
    sleigh: GhidraSleigh,
    code_space: AddressSpace,
    register_names: BTreeMap<(u64, u32), String>,
    diagnostic: BTreeSet<(u64, u32)>,
}

struct SliceLoader<'a> {
    base: u64,
    data: &'a [u8],
}

impl InstructionLoader for SliceLoader<'_> {
    fn load_instruction_bytes(&self, source: &VarnodeData) -> std::result::Result<Vec<u8>, String> {
        let Some(start) = source.address.offset.checked_sub(self.base) else {
            return Ok(Vec::new());
        };
        let start = usize::try_from(start).map_err(|e| e.to_string())?;
        if start >= self.data.len() {
            return Ok(Vec::new());
        }
        let end = start.saturating_add(source.size).min(self.data.len());
        Ok(self.data[start..end].to_vec())
    }
}

impl Lifter {
    pub fn new() -> Result<Self> {
        let sleigh = GhidraSleigh::builder()
            .processor_spec(sleigh_config::processor_x86::PSPEC_X86)
            .and_then(|b| b.build(sleigh_config::processor_x86::SLA_X86))
            .map_err(|e| rk_core::Error::input(format!("cannot initialize SLEIGH x86: {e}")))?;
        let code_space = sleigh.default_code_space();
        let mut register_names = BTreeMap::new();
        for (varnode, name) in sleigh.register_name_map() {
            register_names
                .entry((varnode.address.offset, varnode.size as u32))
                .or_insert(name);
        }
        let diagnostic = DIAGNOSTIC_REGISTERS
            .iter()
            .filter_map(|name| {
                let v = sleigh.register_from_name(*name).ok()?;
                Some((v.address.offset, v.size as u32))
            })
            .collect();
        Ok(Self {
            sleigh,
            code_space,
            register_names,
            diagnostic,
        })
    }

    pub fn register_name(&self, offset: u64, size: u32) -> Option<&str> {
        self.register_names.get(&(offset, size)).map(String::as_str)
    }

    pub fn registers(&self) -> impl Iterator<Item = (&str, u64, u32)> {
        self.register_names
            .iter()
            .map(|(&(offset, size), name)| (name.as_str(), offset, size))
    }

    pub fn lift_raw(&self, addr: Addr, data: &[u8]) -> Result<RawLifted> {
        let decoded = self.translate_one(addr, data).map_err(|e| {
            Diagnostic::error(
                addr,
                Reason::LiftFailed {
                    detail: e.to_string(),
                },
            )
        })?;
        let length = u8::try_from(decoded.origin.size).map_err(|_| {
            Diagnostic::error(
                addr,
                Reason::LiftFailed {
                    detail: "instruction longer than 255 bytes".into(),
                },
            )
        })?;
        if length == 0 {
            return Err(Diagnostic::error(
                addr,
                Reason::LiftFailed {
                    detail: "no instruction mark".into(),
                },
            ));
        }
        if length as usize > data.len() {
            return Err(Diagnostic::error(
                addr,
                Reason::LiftFailed {
                    detail: format!(
                        "SLEIGH length {length} overruns supplied bytes {}",
                        data.len()
                    ),
                },
            ));
        }
        if length > 15 {
            return Err(Diagnostic::error(
                addr,
                Reason::LiftFailed {
                    detail: format!("x86 instruction length {length} exceeds 15"),
                },
            ));
        }
        let mnemonic = self
            .translate_native_one(addr, data)
            .map_err(|e| {
                Diagnostic::error(
                    addr,
                    Reason::LiftFailed {
                        detail: e.to_string(),
                    },
                )
            })?
            .instruction
            .mnemonic;
        let mut ops = Vec::new();
        let mut userops = Vec::new();
        for op in decoded.instructions {
            let opcode = map_opcode(op.op_code);
            if opcode == OpCode::Unknown {
                return Err(Diagnostic::error(
                    addr,
                    Reason::Unsupported {
                        detail: format!("raw p-code opcode {:?}", op.op_code),
                    },
                ));
            }
            let output = op.output.as_ref().map(raw_varnode).transpose()?;
            let mut inputs = Vec::with_capacity(op.inputs.len());
            for (index, v) in op.inputs.iter().enumerate() {
                if index == 0 && matches!(op.op_code, SleighOp::Load | SleighOp::Store) {
                    let id = libsla::AddressSpaceId::new(v.address.offset as usize);
                    let space = unsafe { AddressSpace::from_ghidra_id(id) };
                    let tag = raw_space_name(&space.name, &space.id)?;
                    inputs.push(Varnode::new(
                        tag,
                        0,
                        u32::try_from(space.address_size).unwrap_or(u32::MAX),
                    ));
                } else {
                    inputs.push(raw_varnode(v)?);
                }
            }
            let userop = match op.op_code {
                SleighOp::Pseudo(libsla::PseudoOp::CallOther) => {
                    let id = op.inputs.first().map(|v| v.address.offset);
                    if let Some(id) = id {
                        userops.push(id);
                    }
                    id
                }
                _ => None,
            };
            ops.push(PcodeOp {
                opcode,
                output,
                inputs,
                userop,
            });
        }
        Ok(RawLifted {
            length,
            ops,
            userops,
            mnemonic,
        })
    }

    pub fn lengths(&self, addr: Addr, data: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < data.len() {
            let length = self.length(addr + pos as u32, &data[pos..])?;
            out.push(length);
            pos += length as usize;
        }
        if pos != data.len() {
            return Err(Diagnostic::error(
                addr,
                Reason::DecoderCannotTile { span: addr },
            ));
        }
        Ok(out)
    }

    pub fn length(&self, addr: Addr, data: &[u8]) -> Result<u8> {
        let decoded = self.translate_one(addr, data).map_err(|e| {
            Diagnostic::error(
                addr,
                Reason::LiftFailed {
                    detail: e.to_string(),
                },
            )
        })?;
        let length = decoded.origin.size;
        let length = u8::try_from(length).map_err(|_| {
            Diagnostic::error(
                addr,
                Reason::LiftFailed {
                    detail: "instruction longer than 255 bytes".into(),
                },
            )
        })?;
        if length == 0 {
            return Err(Diagnostic::error(
                addr,
                Reason::LiftFailed {
                    detail: "no instruction mark".into(),
                },
            ));
        }
        Ok(length)
    }

    pub fn ends_flow(&self, addr: Addr, data: &[u8]) -> Result<bool> {
        let decoded = self.translate_one(addr, data).map_err(|e| {
            Diagnostic::error(
                addr,
                Reason::LiftFailed {
                    detail: e.to_string(),
                },
            )
        })?;
        Ok(decoded.instructions.iter().any(|op| {
            matches!(
                op.op_code,
                SleighOp::Branch | SleighOp::BranchIndirect | SleighOp::Return
            ) && !(op.op_code == SleighOp::Branch
                && op
                    .inputs
                    .first()
                    .is_some_and(|v| v.address.address_space.is_constant()))
        }))
    }

    pub fn lift(&mut self, addr: Addr, data: &[u8]) -> Result<Lifted> {
        if data == [0x9b] {
            return Ok(Lifted {
                length: 1,
                ops: Vec::new(),
                x87: false,
            });
        }
        let mut ops = Vec::new();
        let mut x87 = false;
        let mut pos = 0usize;
        while pos < data.len() {
            let at = addr + pos as u32;
            let decoded = self.translate_one(at, &data[pos..]).map_err(|e| {
                Diagnostic::error(
                    at,
                    Reason::LiftFailed {
                        detail: e.to_string(),
                    },
                )
            })?;
            let size = decoded.origin.size;
            if size == 0 || pos + size > data.len() {
                return Err(Diagnostic::error(
                    at,
                    Reason::LiftFailed {
                        detail: format!(
                            "SLEIGH length {size} overruns listed length {}",
                            data.len()
                        ),
                    },
                ));
            }
            let sub = self.convert_all(decoded.instructions)?;
            let sub_flow = sub.iter().any(|op| {
                matches!(op.opcode, OpCode::Branch | OpCode::CBranch)
                    && op.inputs.first().is_some_and(|v| {
                        v.space == Space::Const
                            || (v.space == Space::Ram
                                && addr as u64 <= v.offset
                                && v.offset <= (addr + data.len() as u32) as u64)
                    })
            });
            if sub.iter().any(|op| {
                st_index(op.output.as_ref()).is_some()
                    || op.inputs.iter().any(|v| st_index(Some(v)).is_some())
            }) {
                let conditional_move = matches!(&data[pos..pos + size], [0xda | 0xdb, 0xc0..=0xdf]);
                if sub_flow && !conditional_move {
                    return Err(Diagnostic::error(
                        at,
                        Reason::Unsupported {
                            detail: "x87 instruction with internal control flow".into(),
                        },
                    ));
                }
                x87 = true;
            }
            if sub_flow && !ops.is_empty() {
                return Err(Diagnostic::error(
                    addr,
                    Reason::Unsupported {
                        detail: "internal control flow after a folded prefix".into(),
                    },
                ));
            }
            ops.extend(sub);
            pos += size;
        }
        let length = u8::try_from(data.len()).map_err(|_| {
            Diagnostic::error(
                addr,
                Reason::LiftFailed {
                    detail: "instruction longer than 255 bytes".into(),
                },
            )
        })?;
        Ok(Lifted { length, ops, x87 })
    }

    fn translate_one(
        &self,
        addr: Addr,
        data: &[u8],
    ) -> std::result::Result<libsla::PcodeDisassembly, libsla::Error> {
        let loader = SliceLoader {
            base: addr as u64,
            data,
        };
        self.sleigh
            .disassemble_pcode(&loader, Address::new(self.code_space.clone(), addr as u64))
    }

    fn translate_native_one(
        &self,
        addr: Addr,
        data: &[u8],
    ) -> std::result::Result<libsla::NativeDisassembly, libsla::Error> {
        let loader = SliceLoader {
            base: addr as u64,
            data,
        };
        self.sleigh
            .disassemble_native(&loader, Address::new(self.code_space.clone(), addr as u64))
    }

    fn convert_all(&self, raw: Vec<PcodeInstruction>) -> Result<Vec<PcodeOp>> {
        let mut out = Vec::with_capacity(raw.len());
        for op in raw {
            if let Some(converted) = self.convert(op)? {
                out.push(converted);
            }
        }
        Ok(out)
    }

    fn convert(&self, op: PcodeInstruction) -> Result<Option<PcodeOp>> {
        let output = op.output.as_ref().map(|v| self.varnode(v)).transpose()?;
        if let Some(v) = &output {
            if v.space == Space::Register && self.diagnostic.contains(&(v.offset, v.size)) {
                return Ok(None);
            }
        }
        let mut inputs: Vec<Varnode> = op
            .inputs
            .iter()
            .map(|v| self.varnode(v))
            .collect::<Result<_>>()?;
        let mut userop = None;
        match op.op_code {
            SleighOp::Load | SleighOp::Store => {
                if !inputs.is_empty() {
                    inputs.remove(0);
                }
            }
            SleighOp::Pseudo(libsla::PseudoOp::CallOther) => {
                if let Some(id) = inputs.first() {
                    userop = Some(id.offset);
                }
            }
            _ => {}
        }
        let opcode = map_opcode(op.op_code);
        if inputs.len() == 2 && inputs[0] == inputs[1] && inputs[0].space != Space::Const {
            if let Some(output) = output {
                let size = output.size;
                if matches!(
                    opcode,
                    OpCode::IntXor
                        | OpCode::IntSub
                        | OpCode::IntLess
                        | OpCode::IntSLess
                        | OpCode::IntNotEqual
                        | OpCode::IntSBorrow
                        | OpCode::BoolXor
                ) {
                    return Ok(Some(PcodeOp {
                        opcode: OpCode::Copy,
                        output: Some(output),
                        inputs: vec![Varnode::constant(0, size)],
                        userop,
                    }));
                }
                if matches!(
                    opcode,
                    OpCode::IntEqual | OpCode::IntLessEqual | OpCode::IntSLessEqual
                ) {
                    return Ok(Some(PcodeOp {
                        opcode: OpCode::Copy,
                        output: Some(output),
                        inputs: vec![Varnode::constant(1, size)],
                        userop,
                    }));
                }
            }
        }
        Ok(Some(PcodeOp {
            opcode,
            output,
            inputs,
            userop,
        }))
    }

    fn varnode(&self, v: &VarnodeData) -> Result<Varnode> {
        let space = match v.address.address_space.name.as_ref() {
            "register" => Space::Register,
            "unique" => Space::Unique,
            "const" => Space::Const,
            "ram" => Space::Ram,
            other => {
                return Err(Diagnostic::error(
                    v.address.offset as Addr,
                    Reason::Unsupported {
                        detail: format!("unexpected SLEIGH space {other}"),
                    },
                ))
            }
        };
        Ok(Varnode::new(space, v.address.offset, v.size as u32))
    }
}

fn raw_space_name(name: &str, id: &libsla::AddressSpaceId) -> Result<Space> {
    match name {
        "const" => Ok(Space::Const),
        "ram" => Ok(Space::Ram),
        "register" => Ok(Space::Register),
        "unique" => Ok(Space::Unique),
        _ => Err(rk_core::Error::input(format!(
            "unexpected SLEIGH space {name} ({id})"
        ))),
    }
}

fn raw_varnode(v: &VarnodeData) -> Result<Varnode> {
    let space = raw_space_name(&v.address.address_space.name, &v.address.address_space.id)?;
    Ok(Varnode::new(
        space,
        v.address.offset,
        u32::try_from(v.size).unwrap_or(u32::MAX),
    ))
}

fn st_index(v: Option<&Varnode>) -> Option<u32> {
    let v = v?;
    if v.space != Space::Register || v.size != ST_SIZE || v.offset < ST_BASE {
        return None;
    }
    let rel = v.offset - ST_BASE;
    if rel < 8 * ST_STRIDE && rel.is_multiple_of(ST_STRIDE) {
        Some((rel / ST_STRIDE) as u32)
    } else {
        None
    }
}

fn map_opcode(op: SleighOp) -> OpCode {
    use SleighOp as S;
    match op {
        S::Copy => OpCode::Copy,
        S::Load => OpCode::Load,
        S::Store => OpCode::Store,
        S::Branch => OpCode::Branch,
        S::BranchConditional => OpCode::CBranch,
        S::BranchIndirect => OpCode::BranchInd,
        S::Call => OpCode::Call,
        S::CallIndirect => OpCode::CallInd,
        S::Pseudo(libsla::PseudoOp::CallOther) => OpCode::CallOther,
        S::Return => OpCode::Return,
        S::Piece => OpCode::Piece,
        S::Subpiece => OpCode::SubPiece,
        S::Popcount => OpCode::PopCount,
        S::LzCount => OpCode::LzCount,
        S::Bool(BoolOp::Negate) => OpCode::BoolNegate,
        S::Bool(BoolOp::Xor) => OpCode::BoolXor,
        S::Bool(BoolOp::And) => OpCode::BoolAnd,
        S::Bool(BoolOp::Or) => OpCode::BoolOr,
        S::Int(IntOp::Add) => OpCode::IntAdd,
        S::Int(IntOp::Negate) => OpCode::Int2Comp,
        S::Int(IntOp::Subtract) => OpCode::IntSub,
        S::Int(IntOp::Multiply) => OpCode::IntMult,
        S::Int(IntOp::Divide(IntSign::Unsigned)) => OpCode::IntDiv,
        S::Int(IntOp::Divide(IntSign::Signed)) => OpCode::IntSDiv,
        S::Int(IntOp::Remainder(IntSign::Unsigned)) => OpCode::IntRem,
        S::Int(IntOp::Remainder(IntSign::Signed)) => OpCode::IntSRem,
        S::Int(IntOp::Equal) => OpCode::IntEqual,
        S::Int(IntOp::NotEqual) => OpCode::IntNotEqual,
        S::Int(IntOp::LessThan(IntSign::Signed)) => OpCode::IntSLess,
        S::Int(IntOp::LessThan(IntSign::Unsigned)) => OpCode::IntLess,
        S::Int(IntOp::LessThanOrEqual(IntSign::Signed)) => OpCode::IntSLessEqual,
        S::Int(IntOp::LessThanOrEqual(IntSign::Unsigned)) => OpCode::IntLessEqual,
        S::Int(IntOp::Extension(IntSign::Unsigned)) => OpCode::IntZExt,
        S::Int(IntOp::Extension(IntSign::Signed)) => OpCode::IntSExt,
        S::Int(IntOp::Carry(IntSign::Unsigned)) => OpCode::IntCarry,
        S::Int(IntOp::Carry(IntSign::Signed)) => OpCode::IntSCarry,
        S::Int(IntOp::Borrow) => OpCode::IntSBorrow,
        S::Int(IntOp::ShiftLeft) => OpCode::IntLeft,
        S::Int(IntOp::ShiftRight(IntSign::Unsigned)) => OpCode::IntRight,
        S::Int(IntOp::ShiftRight(IntSign::Signed)) => OpCode::IntSRight,
        S::Int(IntOp::Bitwise(BoolOp::Negate)) => OpCode::IntNegate,
        S::Int(IntOp::Bitwise(BoolOp::Xor)) => OpCode::IntXor,
        S::Int(IntOp::Bitwise(BoolOp::And)) => OpCode::IntAnd,
        S::Int(IntOp::Bitwise(BoolOp::Or)) => OpCode::IntOr,
        S::Float(FloatOp::Equal) => OpCode::FloatEqual,
        S::Float(FloatOp::NotEqual) => OpCode::FloatNotEqual,
        S::Float(FloatOp::LessThan) => OpCode::FloatLess,
        S::Float(FloatOp::LessThanOrEqual) => OpCode::FloatLessEqual,
        S::Float(FloatOp::IsNaN) => OpCode::FloatNan,
        S::Float(FloatOp::Add) => OpCode::FloatAdd,
        S::Float(FloatOp::Divide) => OpCode::FloatDiv,
        S::Float(FloatOp::Multiply) => OpCode::FloatMult,
        S::Float(FloatOp::Subtract) => OpCode::FloatSub,
        S::Float(FloatOp::Negate) => OpCode::FloatNeg,
        S::Float(FloatOp::AbsoluteValue) => OpCode::FloatAbs,
        S::Float(FloatOp::SquareRoot) => OpCode::FloatSqrt,
        S::Float(FloatOp::IntToFloat) => OpCode::FloatInt2Float,
        S::Float(FloatOp::FloatToFloat) => OpCode::FloatFloat2Float,
        S::Float(FloatOp::Truncate) => OpCode::FloatTrunc,
        S::Float(FloatOp::Ceiling) => OpCode::FloatCeil,
        S::Float(FloatOp::Floor) => OpCode::FloatFloor,
        S::Float(FloatOp::Round) => OpCode::FloatRound,
        _ => OpCode::Unknown,
    }
}

pub fn format_varnode(v: &Varnode, lifter: &Lifter) -> String {
    match v.space {
        Space::Register => lifter
            .register_name(v.offset, v.size)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("reg[{}:{}]", rk_core::Hex(v.offset as Addr), v.size)),
        Space::Const => format!("0x{:x}", v.offset),
        Space::Unique => format!("unique[0x{:x}:{}]", v.offset, v.size),
        Space::Ram => format!("ram[0x{:x}:{}]", v.offset, v.size),
    }
}
