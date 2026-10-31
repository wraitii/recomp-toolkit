use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{c_char, CString};
use std::slice;

use rk_core::ir::{Insn, OpCode, PcodeOp, Space, Varnode};
use rk_core::Addr;
use rk_ir::x87::X87Environment;
use rk_ir::{CgOp, InstructionLowerer, Op, SlowInsn, X87Op};
use rk_sleigh::Lifter;

mod jit;

const RP_OP_DIVMOD: u32 = 70;
const RP_OP_SDIVMOD: u32 = 71;
const RP_STRING_MOVS: u64 = 0;
const RP_STRING_STOS: u64 = 1;
const RP_STRING_LODS: u64 = 2;
const RP_STRING_CMPS: u64 = 3;
const RP_STRING_SCAS: u64 = 4;
const RP_STRING_ONCE: u64 = 0;
const RP_STRING_REPE: u64 = 1;
const RP_STRING_REPNE: u64 = 2;
const RP_STRING_MODES: u64 = 3;
const RP_STRING_WIDTHS: u64 = 3;
const RP_USEROP_STRING_BASE: u64 = 0x1000;
const RP_USEROP_RDTSC: u64 = u64::MAX;
const RP_USEROP_CPUID: u64 = u64::MAX - 1;
const RP_USEROP_X87_CLEAR: u64 = u64::MAX - 2;
const RP_USEROP_X87_EMPTY: u64 = u64::MAX - 3;
const RP_USEROP_X87_EMPTY_RESET: u64 = u64::MAX - 4;
const X87_CONTROL: Varnode = Varnode::new(Space::Register, 0x300, 2);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RpVarnode {
    pub space: u32,
    pub size: u32,
    pub offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RpOp {
    pub opcode: u32,
    pub output: RpVarnode,
    pub has_output: u32,
    pub userop: u64,
    pub has_userop: u32,
    pub input_count: u32,
    pub inputs: [RpVarnode; 4],
}

#[repr(C)]
pub struct RpLift {
    pub length: u32,
    pub op_count: u32,
    pub ops: *mut RpOp,
    pub error: *mut c_char,
}

thread_local! {
    static LIFTER: RefCell<Option<Lifter>> = const { RefCell::new(None) };
    static LAST: RefCell<BTreeMap<u32, Vec<u8>>> = const { RefCell::new(BTreeMap::new()) };
    static ERROR: RefCell<Option<std::ffi::CString>> = const { RefCell::new(None) };
    static MNEMONIC: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn space_id(space: Space) -> u32 {
    match space {
        Space::Const => 0,
        Space::Ram => 1,
        Space::Register => 2,
        Space::Unique => 3,
    }
}

fn varnode(v: Varnode) -> RpVarnode {
    RpVarnode {
        space: space_id(v.space),
        size: v.size,
        offset: v.offset,
    }
}

fn set_error(message: String) {
    ERROR.with(|e| *e.borrow_mut() = Some(std::ffi::CString::new(message).unwrap_or_default()));
}

fn op_code(code: OpCode) -> u32 {
    use OpCode::*;
    match code {
        Copy => 1,
        Load => 2,
        Store => 3,
        Branch => 4,
        CBranch => 5,
        BranchInd => 6,
        Call => 7,
        CallInd => 8,
        CallOther => 9,
        Return => 10,
        IntEqual => 11,
        IntNotEqual => 12,
        IntSLess => 13,
        IntSLessEqual => 14,
        IntLess => 15,
        IntLessEqual => 16,
        IntZExt => 17,
        IntSExt => 18,
        IntAdd => 19,
        IntSub => 20,
        IntCarry => 21,
        IntSCarry => 22,
        IntSBorrow => 23,
        Int2Comp => 24,
        IntNegate => 25,
        IntXor => 26,
        IntAnd => 27,
        IntOr => 28,
        IntLeft => 29,
        IntRight => 30,
        IntSRight => 31,
        IntMult => 32,
        IntDiv => 33,
        IntSDiv => 34,
        IntRem => 35,
        IntSRem => 36,
        BoolNegate => 37,
        BoolXor => 38,
        BoolAnd => 39,
        BoolOr => 40,
        FloatEqual => 41,
        FloatNotEqual => 42,
        FloatLess => 43,
        FloatLessEqual => 44,
        FloatNan => 45,
        FloatAdd => 46,
        FloatDiv => 47,
        FloatMult => 48,
        FloatSub => 49,
        FloatNeg => 50,
        FloatAbs => 51,
        FloatSqrt => 52,
        FloatInt2Float => 53,
        FloatFloat2Float => 54,
        FloatTrunc => 55,
        FloatCeil => 56,
        FloatFloor => 57,
        FloatRound => 58,
        Piece => 59,
        SubPiece => 60,
        PopCount => 61,
        LzCount => 62,
        FloatMin => 63,
        FloatMax => 64,
        Cvtss2si => 65,
        Cvttss2si => 66,
        Cvtsi2ss => 67,
        FloatRcp => 68,
        FloatRsqrt => 69,
        Unknown => 0,
    }
}

fn rp_op(
    opcode: u32,
    output: Option<Varnode>,
    inputs: &[Varnode],
    userop: Option<u64>,
) -> Result<RpOp, String> {
    if inputs.len() > 4 {
        return Err(format!(
            "p-code operation has {} inputs; C ABI supports 4",
            inputs.len()
        ));
    }
    let empty = RpVarnode {
        space: 0,
        size: 0,
        offset: 0,
    };
    let mut encoded = [empty; 4];
    for (slot, input) in encoded.iter_mut().zip(inputs) {
        *slot = varnode(*input);
    }
    Ok(RpOp {
        opcode,
        output: output.map_or(empty, varnode),
        has_output: u32::from(output.is_some()),
        userop: userop.unwrap_or(0),
        has_userop: u32::from(userop.is_some()),
        input_count: inputs.len() as u32,
        inputs: encoded,
    })
}

fn pcode(op: &PcodeOp) -> Result<RpOp, String> {
    rp_op(op_code(op.opcode), op.output, &op.inputs, op.userop)
}

fn native_userop(id: u64) -> Result<RpOp, String> {
    rp_op(
        op_code(OpCode::CallOther),
        None,
        &[Varnode::constant(id, 8)],
        Some(id),
    )
}

fn memory(opcode: OpCode, output: Option<Varnode>, inputs: &[Varnode]) -> Result<RpOp, String> {
    let mut with_space = vec![Varnode::new(Space::Ram, 0, 4)];
    with_space.extend_from_slice(inputs);
    rp_op(op_code(opcode), output, &with_space, None)
}

fn string_userop(name: &str) -> Option<u64> {
    let (mode, base) = if let Some(base) = name.strip_prefix("repne_") {
        (RP_STRING_REPNE, base)
    } else if let Some(base) = name
        .strip_prefix("repe_")
        .or_else(|| name.strip_prefix("rep_"))
    {
        (RP_STRING_REPE, base)
    } else {
        (RP_STRING_ONCE, name)
    };
    let (root, suffix) = base.split_at_checked(4)?;
    let family = match root {
        "movs" => RP_STRING_MOVS,
        "stos" => RP_STRING_STOS,
        "lods" => RP_STRING_LODS,
        "cmps" => RP_STRING_CMPS,
        "scas" => RP_STRING_SCAS,
        _ => return None,
    };
    let width = match suffix {
        "b" => 0,
        "w" => 1,
        "d" => 2,
        _ => return None,
    };
    Some(RP_USEROP_STRING_BASE + (family * RP_STRING_WIDTHS + width) * RP_STRING_MODES + mode)
}

fn interpreter_op(op: CgOp, insn: &Insn, slow: &[SlowInsn]) -> Result<RpOp, String> {
    let control = |target: Addr| {
        u32::try_from(target)
            .map(|target| Varnode::new(Space::Ram, u64::from(target), 4))
            .map_err(|_| "control target exceeds 32-bit guest address space".to_owned())
    };
    let code = op_code;
    match op.op {
        Op::P(OpCode::Load | OpCode::Store) => {
            let opcode = if op.op == Op::P(OpCode::Load) {
                OpCode::Load
            } else {
                OpCode::Store
            };
            memory(opcode, op.output, &op.inputs)
        }
        Op::P(opcode) => rp_op(code(opcode), op.output, &op.inputs, None),
        Op::Branch { target } => rp_op(code(OpCode::Branch), None, &[control(target)?], None),
        Op::Call { target } => rp_op(code(OpCode::Call), None, &[control(target)?], None),
        Op::CBranch { target } => {
            let mut inputs = vec![control(target)?];
            inputs.extend(op.inputs);
            rp_op(code(OpCode::CBranch), None, &inputs, None)
        }
        Op::CallInd => rp_op(code(OpCode::CallInd), op.output, &op.inputs, None),
        Op::BranchInd => rp_op(code(OpCode::BranchInd), op.output, &op.inputs, None),
        Op::Return => rp_op(code(OpCode::Return), op.output, &op.inputs, None),
        Op::CallOther(id) => rp_op(code(OpCode::CallOther), op.output, &op.inputs, Some(id)),
        Op::String(name) => native_userop(
            string_userop(name.0).ok_or_else(|| format!("unknown string helper {}", name.0))?,
        ),
        Op::Div { size, .. } | Op::Idiv { size, .. } => {
            let opcode = if matches!(op.op, Op::Div { .. }) {
                RP_OP_DIVMOD
            } else {
                RP_OP_SDIVMOD
            };
            let mut inputs = op.inputs;
            inputs.truncate(2);
            inputs.push(Varnode::constant(u64::from(size), 1));
            rp_op(opcode, op.output, &inputs, None)
        }
        Op::X87(X87Op::Clear) => native_userop(RP_USEROP_X87_CLEAR),
        Op::X87(X87Op::EmptyTags(reset_top)) => native_userop(if reset_top {
            RP_USEROP_X87_EMPTY_RESET
        } else {
            RP_USEROP_X87_EMPTY
        }),
        Op::X87(X87Op::Environment(X87Environment::LoadCw)) => {
            memory(OpCode::Load, Some(X87_CONTROL), &op.inputs[..1])
        }
        Op::X87(X87Op::Environment(X87Environment::StoreCw)) => {
            let mut inputs = op.inputs;
            inputs.push(X87_CONTROL);
            memory(OpCode::Store, None, &inputs)
        }
        Op::X87(_) => Err(format!("{}: x87 operation is not modelled", insn.mnemonic)),
        Op::Slow(_) if insn.mnemonic == "RDTSC" => native_userop(RP_USEROP_RDTSC),
        Op::Slow(_) if insn.mnemonic == "CPUID" => native_userop(RP_USEROP_CPUID),
        Op::Slow(i) => Err(format!(
            "{}: unsupported {:?}",
            insn.mnemonic, slow[i as usize].family
        )),
        other => Err(format!(
            "{}: unsupported corrected operation {:?}",
            insn.mnemonic, other
        )),
    }
}

fn with_lifter<T>(f: impl FnOnce(&mut Lifter) -> rk_core::Result<T>) -> Result<T, String> {
    LIFTER.with(|l| {
        let mut l = l.borrow_mut();
        let lifter = match l.as_mut() {
            Some(lifter) => lifter,
            None => l.insert(Lifter::new().map_err(|e| e.to_string())?),
        };
        f(lifter).map_err(|e| e.to_string())
    })
}

fn is_internal(opcode: OpCode, inputs: &[Varnode]) -> bool {
    matches!(opcode, OpCode::Branch | OpCode::CBranch)
        && inputs.first().is_some_and(|v| v.space == Space::Const)
}

fn lift_one(addr: u32, bytes: &[u8]) -> Result<(u8, String, Vec<RpOp>), String> {
    rk_x86::install().map_err(|e| e.to_string())?;
    let at = Addr::from(addr);
    let length = with_lifter(|l| l.length(at, bytes))?;
    let machine = rk_core::isa::machine();
    if length > machine.max_length || length as usize > bytes.len() {
        return Err(format!("instruction length {length} is not modelled"));
    }
    let bytes = &bytes[..length as usize];
    let lifted = with_lifter(|l| l.lift(at, bytes))?;
    let decoded = machine
        .decoder
        .decode(at, bytes)
        .ok_or_else(|| "instruction decoder rejected lifted bytes".to_owned())?;
    let insn = Insn::new(at, bytes, decoded, lifted.ops, lifted.x87);
    if insn.mnemonic == "SHRD" {
        return Err("x86 SHRD overflow-flag semantics are not modelled".into());
    }
    let mut lowerer = InstructionLowerer::new(&rk_x87::Recipes).with_pcode_control();
    let (ops, _, _) = lowerer.lower_insn(&insn).map_err(|e| e.to_string())?;
    let ops = if rk_ir::wide::required(&ops) {
        rk_ir::wide::legalize(&ops, insn.addr).map_err(|e| e.to_string())?
    } else {
        ops
    };
    let lowered_internal = ops.iter().any(|op| match op.op {
        Op::P(opcode) => is_internal(opcode, &op.inputs),
        _ => false,
    });
    if lowered_internal && insn.ops.iter().any(|op| is_internal(op.opcode, &op.inputs)) {
        let unchanged = ops.len() == insn.ops.len()
            && ops.iter().zip(&insn.ops).all(|(a, b)| {
                a.op == Op::P(b.opcode) && a.output == b.output && a.inputs == b.inputs
            });
        if !unchanged {
            return Err(format!(
                "{}: corrected internal p-code control flow is not modelled",
                insn.mnemonic
            ));
        }
        let raw = with_lifter(|l| l.lift_raw(at, bytes))?;
        let encoded = raw.ops.iter().map(pcode).collect::<Result<_, _>>()?;
        return Ok((length, insn.mnemonic, encoded));
    }
    let encoded = ops
        .into_iter()
        .map(|op| interpreter_op(op, &insn, lowerer.slow_insns()))
        .collect::<Result<_, _>>()?;
    Ok((length, insn.mnemonic, encoded))
}

#[no_mangle]
pub unsafe extern "C" fn rp_lift(addr: u32, data: *const u8, len: usize, out: *mut RpLift) -> i32 {
    if out.is_null() || (data.is_null() && len != 0) {
        return 0;
    }
    let bytes = if len == 0 {
        &[][..]
    } else {
        slice::from_raw_parts(data, len)
    };
    // SLEIGH keeps a disassembly cache keyed by address, so rewriting live bytes at a
    // previously lifted PC would otherwise return the old instruction.
    let changed = LAST.with(|last| {
        let mut last = last.borrow_mut();
        let changed = last
            .get(&addr)
            .is_some_and(|previous| previous.as_slice() != bytes);
        if !last.contains_key(&addr) && last.len() >= 65536 {
            last.clear();
            last.insert(addr, bytes.to_vec());
            return true;
        }
        last.insert(addr, bytes.to_vec());
        changed
    });
    if changed {
        LIFTER.with(|l| *l.borrow_mut() = None);
    }
    (*out).length = 0;
    (*out).op_count = 0;
    (*out).ops = std::ptr::null_mut();
    (*out).error = std::ptr::null_mut();
    match lift_one(addr, bytes) {
        Ok((length, mnemonic, ops)) => {
            MNEMONIC.with(|m| *m.borrow_mut() = Some(CString::new(mnemonic).unwrap_or_default()));
            let encoded = ops.into_boxed_slice();
            (*out).length = u32::from(length);
            (*out).op_count = encoded.len() as u32;
            (*out).ops = Box::into_raw(encoded) as *mut RpOp;
            1
        }
        Err(error) => {
            set_error(error);
            0
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn rp_free(ops: *mut RpOp, count: u32) {
    if !ops.is_null() {
        let slice = std::ptr::slice_from_raw_parts_mut(ops, count as usize);
        drop(Box::from_raw(slice));
    }
}

#[no_mangle]
pub extern "C" fn rp_error() -> *const c_char {
    ERROR.with(|e| e.borrow().as_ref().map_or(std::ptr::null(), |s| s.as_ptr()))
}

#[no_mangle]
pub extern "C" fn rp_mnemonic() -> *const c_char {
    MNEMONIC.with(|m| m.borrow().as_ref().map_or(std::ptr::null(), |s| s.as_ptr()))
}
