use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{c_char, CString};
use std::slice;

use rk_core::ir::{OpCode, Space, Varnode};
use rk_sleigh::{Lifter, RawLifted};

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

fn encode(value: &RawLifted) -> Vec<RpOp> {
    value
        .ops
        .iter()
        .map(|op| {
            let mut inputs = [RpVarnode {
                space: 0,
                size: 0,
                offset: 0,
            }; 4];
            let count = op.inputs.len().min(4);
            for (i, input) in op.inputs.iter().take(count).enumerate() {
                inputs[i] = varnode(*input);
            }
            RpOp {
                opcode: op_code(op.opcode),
                output: op.output.map(varnode).unwrap_or(RpVarnode {
                    space: 0,
                    size: 0,
                    offset: 0,
                }),
                has_output: u32::from(op.output.is_some()),
                userop: op.userop.unwrap_or(0),
                has_userop: u32::from(op.userop.is_some()),
                input_count: count as u32,
                inputs,
            }
        })
        .collect()
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
    let result = LIFTER.with(|l| {
        let mut l = l.borrow_mut();
        let lifter = match l.as_mut() {
            Some(lifter) => lifter,
            None => l.insert(Lifter::new().map_err(|e| e.to_string())?),
        };
        lifter.lift_raw(addr, bytes).map_err(|e| e.to_string())
    });
    match result {
        Ok(lifted) => {
            MNEMONIC.with(|m| {
                *m.borrow_mut() = Some(CString::new(lifted.mnemonic.clone()).unwrap_or_default())
            });
            if let Some(op) = lifted.ops.iter().find(|op| op.inputs.len() > 4) {
                set_error(format!(
                    "p-code operation has {} inputs; C ABI supports 4",
                    op.inputs.len()
                ));
                (*out).length = 0;
                (*out).op_count = 0;
                (*out).ops = std::ptr::null_mut();
                (*out).error = std::ptr::null_mut();
                return 0;
            }
            let encoded = encode(&lifted).into_boxed_slice();
            let count = encoded.len();
            (*out).length = lifted.length as u32;
            (*out).op_count = count as u32;
            (*out).ops = Box::into_raw(encoded) as *mut RpOp;
            (*out).error = std::ptr::null_mut();
            1
        }
        Err(error) => {
            set_error(error);
            (*out).length = 0;
            (*out).op_count = 0;
            (*out).ops = std::ptr::null_mut();
            (*out).error = std::ptr::null_mut();
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
